//! threshold(2 値化。DESIGN.md §9.12)。
//!
//! 作業空間は **sRGB 符号値**。輝度は u8 格子上の整数演算
//! `luma = (2126*R + 7152*G + 722*B + 5000) / 10000` で求め、`luma > T` を白とする。
//! 出力は RGB を 0 か 255(f32 では 0.0 / 1.0)に置き換え、**アルファは保持**する。
//!
//! # 決定論
//!
//! - 輝度と Otsu は **整数演算のみ**(libm 不使用、丸めの解釈が入る余地がない)
//! - Sauvola だけは平方根が要るので f64 を使うが、使うのは IEEE 754 が
//!   厳密丸めを規定している `+` `-` `*` `/` `sqrt` の 5 演算だけで、
//!   `mul_add` は使わない。閾値 `T` は比較の直前に 1e-6 グリッドへ量子化するので、
//!   最下位ビットの揺れが白黒の判定に漏れることもない
//!
//! # 用途の注意(vocab にも同じことを書いている)
//!
//! 2 値化はストロークを欠けさせるので、**読み手が VLM(vision model)なら
//! 二値化しない方が判読率が高い**。この op は Tesseract 等の外部 OCR エンジンへ
//! 渡すためのもの。

use crate::linear::{quantize_1e6, unit_to_u8, LinearImage};
use crate::parallel;
use crate::recipe::ThresholdMethod;
use crate::{AtxError::InvalidRecipe, Result};

/// `sauvola` の窓幅の既定値と値域(奇数のみ)。
const WINDOW_DEFAULT: u32 = 31;
const WINDOW_MIN: u32 = 3;
const WINDOW_MAX: u32 = 255;

/// `sauvola` の感度の既定値と値域。
const K_DEFAULT: f64 = 0.2;
const K_MIN: f64 = 0.0;
const K_MAX: f64 = 1.0;

/// Sauvola の式に現れる標準偏差の正規化定数 R(u8 レンジの半分)。
const SAUVOLA_R: f64 = 128.0;

pub fn validate(
    index: usize,
    method: ThresholdMethod,
    value: Option<u8>,
    window: Option<u32>,
    k: Option<f64>,
) -> Result<()> {
    let name = method.as_str();

    // value は fixed 専用・かつ fixed では必須。
    match (method, value) {
        (ThresholdMethod::Fixed, None) => {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): method \"fixed\" requires value (0..=255)"
            )));
        }
        (ThresholdMethod::Otsu | ThresholdMethod::Sauvola, Some(v)) => {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): value is only valid with method \"fixed\", \
                 but method is {name:?} (got value {v})"
            )));
        }
        _ => {}
    }

    // window / k は sauvola 専用。
    if method != ThresholdMethod::Sauvola {
        if let Some(w) = window {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): window is only valid with method \"sauvola\", \
                 but method is {name:?} (got window {w})"
            )));
        }
        if let Some(kv) = k {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): k is only valid with method \"sauvola\", \
                 but method is {name:?} (got k {kv})"
            )));
        }
    }

    if let Some(w) = window {
        if !(WINDOW_MIN..=WINDOW_MAX).contains(&w) {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): window must be within \
                 {WINDOW_MIN}..={WINDOW_MAX}, got {w}"
            )));
        }
        if w % 2 == 0 {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): window must be odd so the local window is \
                 centred on the pixel, got {w}"
            )));
        }
    }
    if let Some(kv) = k {
        if !kv.is_finite() || !(K_MIN..=K_MAX).contains(&kv) {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (threshold): k must be finite and within \
                 {K_MIN}..={K_MAX}, got {kv}"
            )));
        }
    }
    Ok(())
}

/// BT.709 輝度(u8 格子上の整数演算)。
#[inline]
fn luma_u8(px: [f32; 4]) -> u8 {
    let r = unit_to_u8(px[0]) as u32;
    let g = unit_to_u8(px[1]) as u32;
    let b = unit_to_u8(px[2]) as u32;
    ((2126 * r + 7152 * g + 722 * b + 5000) / 10000) as u8
}

/// 輝度プレーン(行優先 w*h)。3 つの method が共有する。
fn luma_plane(img: &LinearImage) -> Vec<u8> {
    img.data.iter().map(|px| luma_u8(*px)).collect()
}

/// 256 ビンの輝度ヒストグラム。
fn histogram(luma: &[u8]) -> [u64; 256] {
    let mut hist = [0u64; 256];
    for &v in luma {
        hist[v as usize] += 1;
    }
    hist
}

/// Otsu の閾値(級間分散最大)。**整数演算のみ**で求め、最大が複数なら最小の閾値を採る。
///
/// 級間分散は `σ² = (S0·N − S·W0)² / (W0·W1)` に比例する(S = 全輝度和、
/// S0 / W0 = 閾値以下の和と個数)。除算を伴う分数どうしの比較は、
/// **商と剰余に分けた厳密比較**で行うので f64 は 1 回も現れない。
fn otsu_threshold(hist: &[u64; 256]) -> u8 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0;
    }
    let sum: u128 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as u128 * c as u128)
        .sum();

    let mut best_t = 0u8;
    // 最良スコアを (商, 剰余, 分母) の形で持つ。初期値 0/1 = 0。
    let mut best = (0u128, 0u128, 1u128);
    let mut w0 = 0u128;
    let mut s0 = 0u128;
    for (t, &count) in hist.iter().enumerate() {
        w0 += count as u128;
        s0 += t as u128 * count as u128;
        let w1 = total as u128 - w0;
        if w0 == 0 || w1 == 0 {
            continue;
        }
        let a = s0 * total as u128;
        let b = sum * w0;
        let diff = a.abs_diff(b);
        let numerator = diff * diff;
        let denominator = w0 * w1;
        let candidate = (
            numerator / denominator,
            numerator % denominator,
            denominator,
        );
        if greater(candidate, best) {
            best = candidate;
            best_t = t as u8;
        }
    }
    best_t
}

/// `(商, 剰余, 分母)` で表した非負有理数の厳密な `>` 比較。
///
/// `q1 + r1/d1 > q2 + r2/d2` ⇔ `q1 > q2`、または `q1 == q2` かつ `r1*d2 > r2*d1`。
/// `r < d` なので交差積は `d1*d2` を超えず、u128 に収まる。
#[inline]
fn greater(a: (u128, u128, u128), b: (u128, u128, u128)) -> bool {
    if a.0 != b.0 {
        return a.0 > b.0;
    }
    a.1 * b.2 > b.1 * a.2
}

/// 積分画像(輝度の総和 / 平方和)。どちらも `(w+1) * (h+1)`。
struct Integral {
    w: usize,
    sum: Vec<u64>,
    sum_sq: Vec<u64>,
}

impl Integral {
    fn build(luma: &[u8], w: usize, h: usize) -> Self {
        let stride = w + 1;
        let mut sum = vec![0u64; stride * (h + 1)];
        let mut sum_sq = vec![0u64; stride * (h + 1)];
        for y in 0..h {
            for x in 0..w {
                let v = luma[y * w + x] as u64;
                // 包除。left + up は必ず diag 以上なので、u64 のまま引いて安全。
                let up = y * stride + (x + 1);
                let left = (y + 1) * stride + x;
                let diag = y * stride + x;
                let here = (y + 1) * stride + (x + 1);
                sum[here] = v + sum[up] + sum[left] - sum[diag];
                sum_sq[here] = v * v + sum_sq[up] + sum_sq[left] - sum_sq[diag];
            }
        }
        Self {
            w: stride,
            sum,
            sum_sq,
        }
    }

    /// 閉区間 `[x0, x1] x [y0, y1]` の (総和, 平方和, 画素数)。
    #[inline]
    fn window(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> (u64, u64, u64) {
        let a = (y1 + 1) * self.w + (x1 + 1);
        let b = y0 * self.w + (x1 + 1);
        let c = (y1 + 1) * self.w + x0;
        let d = y0 * self.w + x0;
        let s = (self.sum[a] + self.sum[d]) - (self.sum[b] + self.sum[c]);
        let sq = (self.sum_sq[a] + self.sum_sq[d]) - (self.sum_sq[b] + self.sum_sq[c]);
        let count = (x1 - x0 + 1) as u64 * (y1 - y0 + 1) as u64;
        (s, sq, count)
    }
}

/// Sauvola の局所閾値(1e-6 グリッドへ量子化済み)。
#[inline]
fn sauvola_threshold(sum: u64, sum_sq: u64, count: u64, k: f64) -> f64 {
    let n = count as f64;
    let mean = sum as f64 / n;
    let mean_sq = sum_sq as f64 / n;
    let square = mean * mean;
    // 標本分散は理論上非負だが、丸めで -0 側へ落ちることがあるのでクランプする。
    let variance = if mean_sq > square {
        mean_sq - square
    } else {
        0.0
    };
    let sigma = variance.sqrt();
    // mul_add(FMA)を使わず、乗算と加算を別の式に分ける(ops/mod.rs の決定論規約)。
    let ratio = sigma / SAUVOLA_R;
    let inner = ratio - 1.0;
    let scaled = k * inner;
    let factor = 1.0 + scaled;
    quantize_1e6(mean * factor)
}

/// 2 値化を適用する(**sRGB 符号値空間**、エンジン側で空間変換済みの前提)。
///
/// `white` = `luma > T`(`invert` で反転)。アルファは触らない。
pub fn apply(
    img: &LinearImage,
    method: ThresholdMethod,
    value: Option<u8>,
    window: Option<u32>,
    k: Option<f64>,
    invert: bool,
) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = img.clone();
    if w == 0 || h == 0 {
        return out;
    }
    let luma = luma_plane(img);
    let wu = w as usize;
    let hu = h as usize;

    match method {
        ThresholdMethod::Fixed | ThresholdMethod::Otsu => {
            let t = match method {
                ThresholdMethod::Fixed => value.unwrap_or(0),
                _ => otsu_threshold(&histogram(&luma)),
            };
            let luma = &luma;
            parallel::fill_rows(&mut out.data, wu, hu, |y, row| {
                for (x, px) in row.iter_mut().enumerate() {
                    let white = luma[y * wu + x] > t;
                    paint(px, white != invert);
                }
            });
        }
        ThresholdMethod::Sauvola => {
            let win = window.unwrap_or(WINDOW_DEFAULT) as usize;
            let k = k.unwrap_or(K_DEFAULT);
            let half = win / 2;
            let integral = Integral::build(&luma, wu, hu);
            let luma = &luma;
            let integral = &integral;
            parallel::fill_rows(&mut out.data, wu, hu, |y, row| {
                // 窓は画像端でクランプする(縮小窓)ので、端でも添字が外れない。
                let y0 = y.saturating_sub(half);
                let y1 = (y + half).min(hu - 1);
                for (x, px) in row.iter_mut().enumerate() {
                    let x0 = x.saturating_sub(half);
                    let x1 = (x + half).min(wu - 1);
                    let (s, sq, count) = integral.window(x0, y0, x1, y1);
                    let t = sauvola_threshold(s, sq, count, k);
                    let white = luma[y * wu + x] as f64 > t;
                    paint(px, white != invert);
                }
            });
        }
    }
    out
}

/// RGB を白(1.0)か黒(0.0)に置き換える。アルファは不変。
#[inline]
fn paint(px: &mut [f32; 4], white: bool) {
    let v = if white { 1.0 } else { 0.0 };
    px[0] = v;
    px[1] = v;
    px[2] = v;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_defaults_are_accepted() {
        assert!(validate(0, ThresholdMethod::Otsu, None, None, None).is_ok());
        assert!(validate(0, ThresholdMethod::Sauvola, None, Some(31), Some(0.2)).is_ok());
        assert!(validate(0, ThresholdMethod::Fixed, Some(128), None, None).is_ok());
    }

    #[test]
    fn validate_rejects_even_window() {
        let err = validate(1, ThresholdMethod::Sauvola, None, Some(30), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("operations[1] (threshold)"), "{err}");
        assert!(err.contains("odd"), "{err}");
    }

    #[test]
    fn validate_rejects_misplaced_parameters() {
        assert!(validate(0, ThresholdMethod::Fixed, None, None, None).is_err());
        assert!(validate(0, ThresholdMethod::Otsu, Some(1), None, None).is_err());
        assert!(validate(0, ThresholdMethod::Otsu, None, None, Some(0.2)).is_err());
        assert!(validate(0, ThresholdMethod::Fixed, Some(1), Some(31), None).is_err());
    }

    #[test]
    fn luma_of_pure_channels_matches_the_integer_formula() {
        assert_eq!(luma_u8([1.0, 0.0, 0.0, 1.0]), 54); // (2126*255+5000)/10000
        assert_eq!(luma_u8([0.0, 1.0, 0.0, 1.0]), 182);
        assert_eq!(luma_u8([0.0, 0.0, 1.0, 1.0]), 18);
        assert_eq!(luma_u8([1.0, 1.0, 1.0, 1.0]), 255);
    }

    #[test]
    fn otsu_picks_the_valley_of_a_two_peak_histogram() {
        let mut hist = [0u64; 256];
        hist[40] = 100;
        hist[200] = 100;
        let t = otsu_threshold(&hist);
        assert!((40..200).contains(&(t as u32)), "got {t}");
    }

    #[test]
    fn integral_window_matches_the_direct_sum() {
        let luma: Vec<u8> = (0..12u8).collect();
        let integral = Integral::build(&luma, 4, 3);
        let (s, sq, count) = integral.window(1, 1, 3, 2);
        let mut es = 0u64;
        let mut esq = 0u64;
        for y in 1..=2usize {
            for x in 1..=3usize {
                let v = luma[y * 4 + x] as u64;
                es += v;
                esq += v * v;
            }
        }
        assert_eq!((s, sq, count), (es, esq, 6));
    }
}

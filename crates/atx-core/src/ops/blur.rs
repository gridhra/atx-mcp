//! blur / median / unsharp_mask(カーネル系)。
//!
//! **作業空間: 線形光**(`ops/mod.rs` の表を参照)。画素の加重平均は光量に対して
//! 行ってはじめて物理的に正しい(v1 は sRGB 符号値の u8 上で平均していたため、
//! 明暗の境界がぼかすたびに暗く沈んでいた)。
//!
//! 実装規約:
//! - ガウスカーネルは f64 で生成後、重みを 1e-6 グリッドに量子化し、
//!   **量子化後の合計**で正規化してから f32 化する(exp() の libm 差を遮断)
//! - 画素の累算は f32・走査順そのままの左結合(再結合禁止、`mul_add` 不使用)
//! - `blur` / `unsharp_mask` は **アルファをプリマルチプライしてから**畳み込み、
//!   後で解く(半透明の縁で背景色が滲まない)。全画素 α=1.0 の画像では
//!   プリマルチプライは厳密な恒等なので専用の高速パスは不要
//! - `median` は加重平均を取らない順序統計フィルタなのでプリマルチプライしない。
//!   f32 値の中央値は `f32::total_cmp`(全順序)でソートして取る

use crate::linear::{quantize_1e6, LinearImage};
use crate::parallel;
use crate::{AtxError, Result};

/// `unsharp_mask` の threshold を 0..1 スケールへ写す係数。
///
/// レシピ上の `threshold` は v1 から変わらず「u8 符号値の差」を意図した 0..=255 だが、
/// v2 の比較は**線形光の 0..1** 上で行われる。`threshold / 255` を線形光の差の
/// 閾値として解釈する(暗部では v1 より保護が効きやすく、明部では効きにくい)。
/// 完全な等価性は空間が変わった以上あり得ないため、単純で説明可能な規則を採る。
const THRESHOLD_SCALE: f32 = 1.0 / 255.0;

pub fn validate_blur(index: usize, sigma: f64) -> Result<()> {
    if !sigma.is_finite() || !(0.1..=100.0).contains(&sigma) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (blur): sigma must be within 0.1..=100.0, got {sigma}"
        )));
    }
    Ok(())
}

pub fn validate_median(index: usize, radius: u32) -> Result<()> {
    if !(1..=16).contains(&radius) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (median): radius must be within 1..=16, got {radius}"
        )));
    }
    Ok(())
}

pub fn validate_unsharp(index: usize, amount: f64, radius: f64, threshold: u8) -> Result<()> {
    let _ = threshold; // u8 は常に有効域
    if !amount.is_finite() || !(0.0..=4.0).contains(&amount) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (unsharp_mask): amount must be within 0.0..=4.0, got {amount}"
        )));
    }
    if !radius.is_finite() || !(0.1..=50.0).contains(&radius) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (unsharp_mask): radius must be within 0.1..=50.0, got {radius}"
        )));
    }
    Ok(())
}

/// ガウスカーネルの重み(量子化・正規化済み)と半径を計算する。
///
/// 決定論規約(mod.rs 参照):
/// 1. f64 で `exp(-(i^2) / (2*sigma^2))` を計算する
/// 2. 各重みを 1e-6 グリッドへ量子化する((w*1e6).round()/1e6)
/// 3. 量子化後の重みの合計で正規化する(除算の順序を固定: 量子化 → 合計 → 正規化)
/// 4. 最後に f32 へ落とす(画素ループは f32 で回る)
///
/// 半径(半値幅)は `ceil(3*sigma)` を 255 に上限キャップする。
fn gaussian_kernel(sigma: f64) -> (i32, Vec<f32>) {
    let radius = ((3.0 * sigma).ceil() as i64).clamp(1, 255) as i32;
    let two_sigma_sq = 2.0 * sigma * sigma;

    let quantized: Vec<f64> = (-radius..=radius)
        .map(|i| quantize_1e6((-((i * i) as f64) / two_sigma_sq).exp()))
        .collect();

    // 量子化後の重みの合計で正規化する(この順序が決定論の要)。
    let sum: f64 = quantized.iter().fold(0.0f64, |acc, w| acc + w);
    let weights: Vec<f32> = quantized.iter().map(|w| (w / sum) as f32).collect();
    (radius, weights)
}

/// ガウスぼかし。分離可能な1次元カーネルを横→縦の順で適用する(2パス)。
/// 端はクランプ(複製)。**アルファをプリマルチプライした 4 チャンネル**に掛ける。
///
/// 行単位でスレッド分割して計算する。画素ごとの f32 累算順序(決定論の要)は
/// 分割の有無に関わらず `weights` の添字昇順で固定される。
pub fn gaussian_blur(img: &LinearImage, sigma: f64) -> LinearImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let (radius, weights) = gaussian_kernel(sigma);
    let premul = img.premultiplied();
    let (wu, hu) = (w as usize, h as usize);

    // --- パス1: 水平方向 ---
    let mut horiz = vec![[0f32; 4]; wu * hu];
    parallel::fill_rows(&mut horiz, wu, hu, |y, row| {
        for (x, slot) in row.iter_mut().enumerate() {
            let mut acc = [0f32; 4];
            for (k, &weight) in weights.iter().enumerate() {
                let dx = k as i64 - radius as i64;
                let sx = (x as i64 + dx).clamp(0, w as i64 - 1) as usize;
                let px = premul.data[y * wu + sx];
                for c in 0..4 {
                    let term = px[c] * weight;
                    acc[c] += term;
                }
            }
            *slot = acc;
        }
    });

    // --- パス2: 垂直方向 ---
    let mut out_data = vec![[0f32; 4]; wu * hu];
    parallel::fill_rows(&mut out_data, wu, hu, |y, row| {
        for (x, slot) in row.iter_mut().enumerate() {
            let mut acc = [0f32; 4];
            for (k, &weight) in weights.iter().enumerate() {
                let dy = k as i64 - radius as i64;
                let sy = (y as i64 + dy).clamp(0, h as i64 - 1) as usize;
                let px = horiz[sy * wu + x];
                for c in 0..4 {
                    let term = px[c] * weight;
                    acc[c] += term;
                }
            }
            *slot = acc;
        }
    });

    let mut out = LinearImage {
        width: w,
        height: h,
        data: out_data,
    };
    out.unpremultiply();
    out
}

/// メディアンフィルタ。(2r+1)^2 の正方形ウィンドウ、チャンネル毎に中央値を取る。
/// 端はクランプ。
///
/// v1 は u8 の 256 バケツヒストグラムで求めていたが、v2 の画素は f32 なので
/// **`f32::total_cmp` による全順序ソート**へ置き換えた(NaN が来ても順序が定まる。
/// 実際には NaN は生成されないが、比較の全順序性は決定論の前提)。
/// 順序統計量なので加重平均は発生せず、プリマルチプライは行わない。
pub fn median(img: &LinearImage, radius: u32) -> LinearImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let r = radius as i64;
    let window_len = (2 * radius + 1) as usize * (2 * radius + 1) as usize;
    let median_rank = window_len / 2; // 0-indexed、奇数個なので中央要素
    let (wu, hu) = (w as usize, h as usize);

    let mut out_data = vec![[0f32; 4]; wu * hu];
    parallel::fill_rows(&mut out_data, wu, hu, |y, row| {
        let mut window: Vec<[f32; 4]> = Vec::with_capacity(window_len);
        let mut channel: Vec<f32> = Vec::with_capacity(window_len);
        for (x, slot) in row.iter_mut().enumerate() {
            window.clear();
            for dy in -r..=r {
                for dx in -r..=r {
                    window.push(img.get_clamped(x as i64 + dx, y as i64 + dy));
                }
            }
            let mut px = [0f32; 4];
            for (c, out_c) in px.iter_mut().enumerate() {
                channel.clear();
                channel.extend(window.iter().map(|p| p[c]));
                channel.sort_by(|a, b| a.total_cmp(b));
                *out_c = channel[median_rank];
            }
            *slot = px;
        }
    });

    LinearImage {
        width: w,
        height: h,
        data: out_data,
    }
}

/// アンシャープマスク。`blurred = gaussian_blur(img, radius)` を基準に、
/// チャンネル毎の絶対差が `threshold/255`(線形光スケール)以下ならそのまま(保護)、
/// 超えていれば `orig + amount * (orig - blurred)` を f32 で計算し 0..1 へクランプする。
///
/// `threshold` はチャンネル毎の絶対差で判定する(輝度ベースではない)。
/// より単純で決定論を保ちやすいための意図的な簡略化。
pub fn unsharp_mask(img: &LinearImage, amount: f64, radius: f64, threshold: u8) -> LinearImage {
    let blurred = gaussian_blur(img, radius);
    let amount = amount as f32;
    let threshold = threshold as f32 * THRESHOLD_SCALE;
    let mut out = img.clone();

    for (px, blur) in out.data.iter_mut().zip(blurred.data.iter()) {
        // v1 と同じく 4 チャンネルすべてに掛ける(アルファのエッジも同時に立てる)。
        for c in 0..4 {
            let diff = px[c] - blur[c];
            if diff.abs() > threshold {
                let boost = amount * diff;
                px[c] = (px[c] + boost).clamp(0.0, 1.0);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_weights_sum_to_one() {
        for sigma in [0.5f64, 1.0, 3.7, 12.0] {
            let (_, weights) = gaussian_kernel(sigma);
            let sum: f32 = weights.iter().fold(0.0f32, |a, w| a + w);
            assert!((sum - 1.0).abs() < 1e-5, "sigma {sigma}: sum {sum}");
        }
    }

    #[test]
    fn blur_of_uniform_image_is_uniform() {
        let img = LinearImage::from_pixel(16, 16, [0.2, 0.4, 0.6, 1.0]);
        let out = gaussian_blur(&img, 2.0);
        for px in &out.data {
            for (a, b) in px.iter().zip([0.2f32, 0.4, 0.6, 1.0]) {
                assert!((a - b).abs() < 1e-5, "{px:?}");
            }
        }
    }

    #[test]
    fn median_picks_the_middle_value() {
        // 3x3 に 0,1,...,8 を敷き詰め、radius=1 の中央画素は中央値 4/8 になる。
        let mut img = LinearImage::new(3, 3);
        for i in 0..9u32 {
            img.set(i % 3, i / 3, [i as f32 / 8.0, 0.0, 0.0, 1.0]);
        }
        let out = median(&img, 1);
        assert!((out.get(1, 1)[0] - 4.0 / 8.0).abs() < 1e-6);
    }
}

//! auto_levels(自動レベル補正)。
//!
//! sRGB f32 空間。256 ビンのヒストグラムから上下 clip_percent% の分位点を取り、
//! その範囲を 0..1 へ線形伸長する(levels の in_black/in_white を自動決定するのと等価)。
//! per_channel=false は BT.709 輝度のヒストグラムで単一の伸長を全チャンネルへ、
//! true はチャンネル別(色被り補正、ただし色相が動く旨を vocab で警告)。
//! 入力画像に対して決定論的(ヒストグラムは固定ビン・整数カウント)。

use crate::linear::LinearImage;
use crate::parallel;
use crate::{AtxError::InvalidRecipe, Result};

/// ヒストグラムのビン数。
const BINS: usize = 256;

/// clip_percent の許容範囲。
const CLIP_MIN: f64 = 0.0;
const CLIP_MAX: f64 = 10.0;

/// BT.709 輝度係数(sRGB 符号値ベース。gradient_map と同じ係数)。
const LUMA_R: f64 = 0.2126;
const LUMA_G: f64 = 0.7152;
const LUMA_B: f64 = 0.0722;

pub fn validate(index: usize, clip_percent: f64) -> Result<()> {
    if !clip_percent.is_finite() || !(CLIP_MIN..=CLIP_MAX).contains(&clip_percent) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (auto_levels): clip_percent must be finite and within \
             {CLIP_MIN}..={CLIP_MAX}, got {clip_percent}"
        )));
    }
    Ok(())
}

/// sRGB f32 値(0..1)をヒストグラムのビン添字(0..=255)へ量子化する。
/// `bin = floor(v*255 + 0.5)`、クランプ済み。
#[inline]
fn bin_of(v: f32) -> usize {
    let scaled = v.clamp(0.0, 1.0) * 255.0 + 0.5;
    (scaled.floor() as i32).clamp(0, 255) as usize
}

/// 256 ビンの整数ヒストグラムを固定順序(走査順)で構築する。
fn histogram(values: impl Iterator<Item = f32>) -> [u64; BINS] {
    let mut hist = [0u64; BINS];
    for v in values {
        hist[bin_of(v)] += 1;
    }
    hist
}

/// ヒストグラムから (lo, hi) の分位点(0..1 の符号値スケール)を求める。
///
/// - `lo`: 累積カウントが `total * clip_percent / 100` を**初めて超える**ビンの値
/// - `hi`: 上から対称に同じ規則で求める
/// - `hi <= lo` なら呼び出し側で no-op ガードとして扱う(フラット画像対策)
fn clip_bounds(hist: &[u64; BINS], clip_percent: f64) -> (f32, f32) {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return (0.0, 1.0);
    }
    let threshold = (total as f64) * clip_percent / 100.0;

    let mut lo_bin = 0usize;
    let mut cumulative = 0u64;
    for (i, &c) in hist.iter().enumerate() {
        cumulative += c;
        if (cumulative as f64) > threshold {
            lo_bin = i;
            break;
        }
        lo_bin = i;
    }

    let mut hi_bin = BINS - 1;
    let mut cumulative = 0u64;
    for (i, &c) in hist.iter().enumerate().rev() {
        cumulative += c;
        if (cumulative as f64) > threshold {
            hi_bin = i;
            break;
        }
        hi_bin = i;
    }

    (lo_bin as f32 / 255.0, hi_bin as f32 / 255.0)
}

/// 1チャンネル分の値を (v - lo) / (hi - lo) で 0..1 へ伸長する(クランプ)。
/// `hi <= lo` は no-op(元の値をそのまま返す)。
#[inline]
fn stretch(v: f32, lo: f32, hi: f32) -> f32 {
    if hi <= lo {
        return v;
    }
    let lo64 = lo as f64;
    let hi64 = hi as f64;
    let v64 = v as f64;
    let span = hi64 - lo64;
    let out = (v64 - lo64) / span;
    out.clamp(0.0, 1.0) as f32
}

/// auto_levels を適用する(**sRGB 符号値空間**、エンジン側で空間変換済みの前提)。
///
/// アルファは不変。
pub fn apply(img: &LinearImage, clip_percent: f64, per_channel: bool) -> LinearImage {
    let mut out = img.clone();

    if per_channel {
        let hist_r = histogram(img.data.iter().map(|px| px[0]));
        let hist_g = histogram(img.data.iter().map(|px| px[1]));
        let hist_b = histogram(img.data.iter().map(|px| px[2]));
        let (lo_r, hi_r) = clip_bounds(&hist_r, clip_percent);
        let (lo_g, hi_g) = clip_bounds(&hist_g, clip_percent);
        let (lo_b, hi_b) = clip_bounds(&hist_b, clip_percent);

        parallel::for_each_chunk(&mut out.data, |chunk| {
            for px in chunk.iter_mut() {
                px[0] = stretch(px[0], lo_r, hi_r);
                px[1] = stretch(px[1], lo_g, hi_g);
                px[2] = stretch(px[2], lo_b, hi_b);
            }
        });
    } else {
        let hist_luma = histogram(img.data.iter().map(|px| {
            let luma = LUMA_R * px[0] as f64 + LUMA_G * px[1] as f64 + LUMA_B * px[2] as f64;
            luma as f32
        }));
        let (lo, hi) = clip_bounds(&hist_luma, clip_percent);

        parallel::for_each_chunk(&mut out.data, |chunk| {
            for px in chunk.iter_mut() {
                px[0] = stretch(px[0], lo, hi);
                px[1] = stretch(px[1], lo, hi);
                px[2] = stretch(px[2], lo, hi);
            }
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_default() {
        assert!(validate(0, 0.5).is_ok());
    }

    #[test]
    fn validate_rejects_negative() {
        assert!(validate(0, -0.1).is_err());
    }

    #[test]
    fn validate_rejects_above_max() {
        assert!(validate(0, 10.1).is_err());
    }

    #[test]
    fn validate_rejects_non_finite() {
        assert!(validate(0, f64::NAN).is_err());
    }

    #[test]
    fn flat_image_is_noop() {
        let img = LinearImage::from_pixel(4, 4, [0.4, 0.4, 0.4, 1.0]);
        let out = apply(&img, 0.5, false);
        assert_eq!(out.data, img.data);
    }
}

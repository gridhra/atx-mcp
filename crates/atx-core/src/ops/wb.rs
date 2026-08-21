//! ホワイトバランス(temperature / tint)。
//!
//! RAW を持たない sRGB 画像への簡易モデル: temperature / tint をチャンネルゲインに
//! 写像して適用する(正確な色順応変換は Phase B の f32 + lcms2 で扱う。
//! ここでは「見た目が Lightroom の WB スライダに近い、決定論的で単調な補正」を目標とする)。
//! ゲイン算出に超越関数を使う場合は f64 計算 → 1e-6 量子化(ops/mod.rs 規約)。
//!
//! # ゲインモデル(v0.3 で確定。変更は ENGINE_VERSION 更新を伴う)
//!
//! スライダ `temperature = t ∈ [-100, 100]`(正 = 暖色へ)、
//! `tint = m ∈ [-100, 100]`(正 = マゼンタへ)から生ゲインを作る:
//!
//! ```text
//! g_r = 1 + 0.35 * (t / 100)     // t=+100 で R は 1.35 倍
//! g_b = 1 - 0.35 * (t / 100)     // t=+100 で B は 0.65 倍(暖色 = 青を引く)
//! g_g = 1 - 0.25 * (m / 100)     // m=+100(マゼンタ)で G は 0.75 倍
//! ```
//!
//! temperature は R と B を対称に動かし、tint は G だけを動かす。
//! 「マゼンタ = 緑の補色」なので **正の tint は G を下げる** 向きに定義する。
//!
//! そのままだと全体の明るさが動いてしまうため、BT.709 の輝度係数で重み付けした
//! 平均でゲインを正規化する:
//!
//! ```text
//! mean = 0.2126 * g_r + 0.7152 * g_g + 0.0722 * g_b
//! G_c  = quantize_1e6(g_c / mean)          (c ∈ {r, g, b})
//! ```
//!
//! これで「ゲインの輝度加重平均 ≈ 1」となり、平均輝度はおおよそ保たれる
//! (画素ごとのクランプがあるため厳密な保存ではない。テストでは 2% 以内を確認)。
//!
//! 画素適用は `out_c = clamp(round_half_away(px_c * G_c), 0, 255)`。アルファは不変。
//!
//! # 決定論
//! - 使う演算は四則のみで libm を踏まない。それでも `mean` による除算結果を
//!   **画素ループに入る前に 1e-6 グリッドへ量子化**する(ops/mod.rs 規約の徹底。
//!   将来モデルを黒体放射近似などへ差し替えても経路が変わらないようにする保険)
//! - 量子化済みゲインから 256 エントリ LUT を組み、画素ループは配列引きに閉じる
//! - `t == 0 && m == 0` は短絡して入力をそのまま返す(浮動小数の丸めで
//!   `mean` が厳密に 1.0 にならない可能性があるため、恒等はバイト一致で保証する)

use image::RgbaImage;

use crate::{AtxError::InvalidRecipe, Result};

/// スライダの許容範囲(両端含む)。
const SLIDER_MIN: f64 = -100.0;
const SLIDER_MAX: f64 = 100.0;

/// temperature = ±100 での R / B ゲイン振幅。
const TEMPERATURE_AMPLITUDE: f64 = 0.35;
/// tint = ±100 での G ゲイン振幅。
const TINT_AMPLITUDE: f64 = 0.25;

/// BT.709 輝度係数(正規化の重み)。
const LUMA_R: f64 = 0.2126;
const LUMA_G: f64 = 0.7152;
const LUMA_B: f64 = 0.0722;

/// 静的検証: temperature / tint がともに有限かつ -100..=100。
pub fn validate(index: usize, temperature: f64, tint: f64) -> Result<()> {
    for (name, value) in [("temperature", temperature), ("tint", tint)] {
        if !value.is_finite() || !(SLIDER_MIN..=SLIDER_MAX).contains(&value) {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (white_balance): {name} must be a finite value within \
                 {SLIDER_MIN}..={SLIDER_MAX}, got {value}"
            )));
        }
    }
    Ok(())
}

/// 正規化済み R/G/B ゲインを返す(1e-6 グリッドへ量子化済み)。
///
/// モジュールドキュメントのモデルをそのまま実装したもの。
/// `mean` が 0 以下になることはモデル上ありえない(振幅 0.35/0.25 では
/// どのゲインも 0.65 以上)が、念のため 0 以下なら正規化を諦めて生ゲインを返す。
fn gains(temperature: f64, tint: f64) -> [f64; 3] {
    let t = temperature / 100.0;
    let m = tint / 100.0;

    let g_r = 1.0 + TEMPERATURE_AMPLITUDE * t;
    let g_b = 1.0 - TEMPERATURE_AMPLITUDE * t;
    let g_g = 1.0 - TINT_AMPLITUDE * m;

    let mean = LUMA_R * g_r + LUMA_G * g_g + LUMA_B * g_b;
    let raw = [g_r, g_g, g_b];
    if !mean.is_finite() || mean <= 0.0 {
        return raw;
    }
    let mut out = [0.0f64; 3];
    for (slot, g) in out.iter_mut().zip(raw) {
        // 1e-6 グリッドへ量子化してから画素へ持ち込む。
        *slot = (g / mean * 1e6).round() / 1e6;
    }
    out
}

/// 1 チャンネル分の 256 エントリ LUT(`round_half_away(i * gain)` をクランプ)。
fn gain_lut(gain: f64) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        *slot = (i as f64 * gain).clamp(0.0, 255.0).round() as u8;
    }
    lut
}

/// ホワイトバランスを適用する。アルファは不変。
///
/// `temperature == 0.0 && tint == 0.0` は恒等として短絡する(バイト一致保証)。
/// モデルの詳細はモジュールドキュメント参照。
pub fn apply(img: &RgbaImage, temperature: f64, tint: f64) -> RgbaImage {
    if temperature == 0.0 && tint == 0.0 {
        return img.clone();
    }
    let g = gains(temperature, tint);
    let luts = [gain_lut(g[0]), gain_lut(g[1]), gain_lut(g[2])];

    let mut out = img.clone();
    for px in out.pixels_mut() {
        for (i, lut) in luts.iter().enumerate() {
            px.0[i] = lut[px.0[i] as usize];
        }
        // px.0[3](アルファ)は意図的に触れない。
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_gains_are_exactly_one() {
        let g = gains(0.0, 0.0);
        assert_eq!(g, [1.0, 1.0, 1.0], "正規化後も中立ゲインは 1.0 ちょうど");
    }

    #[test]
    fn warm_raises_red_and_lowers_blue() {
        let g = gains(100.0, 0.0);
        assert!(g[0] > 1.0, "R gain {} should exceed 1", g[0]);
        assert!(g[2] < 1.0, "B gain {} should be below 1", g[2]);
        assert!(g[0] > g[1] && g[1] > g[2]);
    }

    #[test]
    fn magenta_tint_lowers_green() {
        let g = gains(0.0, 100.0);
        assert!(g[1] < 1.0, "G gain {} should be below 1", g[1]);
        assert!(g[0] > 1.0 && g[2] > 1.0, "正規化で R/B は持ち上がる");
    }

    #[test]
    fn gains_are_quantized_to_1e6_grid() {
        for (t, m) in [(37.5, -12.25), (-100.0, 100.0), (1.0, 1.0)] {
            for g in gains(t, m) {
                let scaled = g * 1e6;
                assert!(
                    (scaled - scaled.round()).abs() < 1e-6,
                    "gain {g} is not on the 1e-6 grid"
                );
            }
        }
    }

    #[test]
    fn luma_weighted_mean_of_gains_is_about_one() {
        for (t, m) in [(100.0, 0.0), (-100.0, 0.0), (0.0, 100.0), (60.0, -40.0)] {
            let g = gains(t, m);
            let mean = LUMA_R * g[0] + LUMA_G * g[1] + LUMA_B * g[2];
            assert!(
                (mean - 1.0).abs() < 1e-5,
                "weighted mean {mean} for ({t}, {m}) should be ~1"
            );
        }
    }
}

//! ホワイトバランス(temperature / tint)。
//!
//! **作業空間: 線形光**(`ops/mod.rs` の表を参照)。
//!
//! # v2 でホワイトバランスは物理的に正しくなった
//!
//! ホワイトバランスの本質は「センサ/照明のチャンネルごとの露出違いを打ち消す」ことで、
//! これは **光量に対する乗算**である。v1 は sRGB 符号値(u8)にゲインを直接掛けていたため、
//! 数学的には `(k·v)` ではなく `OETF(k·EOTF(v))` ではなく `k·OETF(EOTF(v))` — つまり
//! 符号値の線形スケール = 光量に対しては非線形な操作になっていた。
//! v2 では同じゲインが**線形光の倍率**として掛かるため、実際のカラー写真の
//! 色順応に一致する挙動になる。
//!
//! **見た目の変化**: スライダの写像(下記のモデル)は 1 も変えていないが、
//! 同じ `temperature` でも v1 より**中間トーンの色転びが穏やかで、
//! ハイライトとシャドウの色被りが自然**になる。強い暖色指定でハイライトが
//! 飽和して色相が転ぶ現象も減る。数値としては v1 と一致しない(ENGINE_VERSION 2)。
//!
//! # ゲインモデル(v0.3 で確定した写像。v0.4 でも変更なし)
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
//! BT.709 係数は**線形光の輝度**の定義そのものなので、v2 ではこの正規化も
//! 本来の意味(輝度保存)どおりに働く。
//!
//! 画素適用は `out_c = clamp(px_c * G_c, 0, 1)`。アルファは不変。
//!
//! # 決定論
//! - 使う演算は四則のみで libm を踏まない。それでも `mean` による除算結果を
//!   **画素ループに入る前に 1e-6 グリッドへ量子化**する(ops/mod.rs 規約の徹底)
//! - v1 は量子化ゲインから 256 エントリ LUT を組んでいたが、v2 の画素は連続値なので
//!   LUT に落とさず f32 の乗算で直接適用する(丸めは出口の 1 回だけ)
//! - `t == 0 && m == 0` は短絡して入力をそのまま返す(浮動小数の丸めで
//!   `mean` が厳密に 1.0 にならない可能性があるため、恒等はバイト一致で保証する)

use crate::linear::{quantize_1e6, LinearImage};
use crate::parallel;
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
        *slot = quantize_1e6(g / mean);
    }
    out
}

/// ホワイトバランスを適用する(**線形光の乗算**)。アルファは不変。
///
/// `temperature == 0.0 && tint == 0.0` は恒等として短絡する(バイト一致保証)。
/// モデルの詳細はモジュールドキュメント参照。
pub fn apply(img: &LinearImage, temperature: f64, tint: f64) -> LinearImage {
    if temperature == 0.0 && tint == 0.0 {
        return img.clone();
    }
    let g = gains(temperature, tint);
    let gains_f32 = [g[0] as f32, g[1] as f32, g[2] as f32];

    let mut out = img.clone();
    parallel::for_each_chunk(&mut out.data, |chunk| {
        for px in chunk.iter_mut() {
            for (i, gain) in gains_f32.iter().enumerate() {
                px[i] = (px[i] * gain).clamp(0.0, 1.0);
            }
            // px[3](アルファ)は意図的に触れない。
        }
    });
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

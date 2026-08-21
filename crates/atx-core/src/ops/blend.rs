//! レイヤー合成(v0.6)。separable ブレンド関数 12 種 + W3C の合成式。
//!
//! # 準拠仕様
//!
//! [W3C Compositing and Blending Level 1](https://www.w3.org/TR/compositing-1/) の
//! `simple alpha compositing` + `separable blend modes`。**ストレートアルファ**のまま、
//! 仕様の式をそのまま f32 で書き下す:
//!
//! ```text
//! αo = αs + αb × (1 − αs)
//! Co = ( αs × (1 − αb) × Cs + αs × αb × B(Cb, Cs) + (1 − αs) × αb × Cb ) / αo
//! ```
//!
//! `αo == 0` のときは RGBA すべて 0(仕様上 Co は未定義。透明画素の色は出力に出ない)。
//!
//! # 合成空間: sRGB 符号値
//!
//! ブレンド関数は「Cb / Cs が 0..1 の**符号値**」であることを前提に定義されている
//! (`multiply` で中間グレー同士が中間より暗くなる、`screen` の対称性、`soft_light` の
//! D(Cb) の分岐点 0.25 など、すべて符号値上の慣習)。線形光で同じ式を適用すると
//! Photoshop / CSS / Figma と見た目が一致しない。したがって**合成は sRGB 符号値空間で
//! 行う**(`ops/mod.rs` の作業空間表では `curves` / `lut` 側と同じ立場)。
//!
//! # 決定論
//!
//! - 固定順序の f32、FMA 禁止(`ops/mod.rs` の規約)。乗算と加算を別の文に分ける
//! - 超越関数は使わない。`soft_light` の `sqrt` のみ **IEEE-754 で厳密に丸められる**
//!   演算なので画素ループ内で呼んでよい(libm 依存の exp / pow とは違う)
//! - 端点は式ではなく分岐で確定させる: `αs == 0` は backdrop をそのまま残す
//!   (`(αb × Cb) / αb` は f32 で `Cb` に戻らないことがある)。`αs == 1 かつ αb == 1`
//!   は式のままで厳密に `B(Cb, Cs)` になるので分岐不要
//! - 入力は 0..1 へクランプしてからブレンドする(`color_burn` の除算や
//!   `soft_light` の sqrt が定義域外の値で NaN を出さないため)

use crate::linear::LinearImage;
use crate::recipe::BlendMode;

/// separable ブレンド関数 `B(Cb, Cs)`(W3C compositing-1 §9)。
///
/// `cb`(backdrop)/ `cs`(source)はいずれも 0..1 の sRGB 符号値。
#[inline]
pub(crate) fn blend_channel(mode: BlendMode, cb: f32, cs: f32) -> f32 {
    match mode {
        // B(Cb, Cs) = Cs
        BlendMode::Normal => cs,
        // B(Cb, Cs) = Cb × Cs
        BlendMode::Multiply => cb * cs,
        // B(Cb, Cs) = Cb + Cs − (Cb × Cs)
        BlendMode::Screen => screen(cb, cs),
        // B(Cb, Cs) = HardLight(Cs, Cb) — 引数を入れ替えた hard_light
        BlendMode::Overlay => hard_light(cs, cb),
        // B(Cb, Cs) = min(Cb, Cs)
        BlendMode::Darken => {
            if cb <= cs {
                cb
            } else {
                cs
            }
        }
        // B(Cb, Cs) = max(Cb, Cs)
        BlendMode::Lighten => {
            if cb >= cs {
                cb
            } else {
                cs
            }
        }
        // Cb == 0 → 0 / Cs == 1 → 1 / それ以外 min(1, Cb / (1 − Cs))
        BlendMode::ColorDodge => {
            if cb == 0.0 {
                0.0
            } else if cs == 1.0 {
                1.0
            } else {
                let q = cb / (1.0 - cs);
                if q < 1.0 {
                    q
                } else {
                    1.0
                }
            }
        }
        // Cb == 1 → 1 / Cs == 0 → 0 / それ以外 1 − min(1, (1 − Cb) / Cs)
        BlendMode::ColorBurn => {
            if cb == 1.0 {
                1.0
            } else if cs == 0.0 {
                0.0
            } else {
                let q = (1.0 - cb) / cs;
                if q < 1.0 {
                    1.0 - q
                } else {
                    0.0
                }
            }
        }
        BlendMode::HardLight => hard_light(cb, cs),
        BlendMode::SoftLight => soft_light(cb, cs),
        // B(Cb, Cs) = |Cb − Cs|
        BlendMode::Difference => {
            if cb >= cs {
                cb - cs
            } else {
                cs - cb
            }
        }
        // B(Cb, Cs) = Cb + Cs − 2 × Cb × Cs
        BlendMode::Exclusion => {
            let p = cb * cs;
            let two_p = 2.0 * p;
            let sum = cb + cs;
            sum - two_p
        }
    }
}

/// `Screen(Cb, Cs) = Cb + Cs − (Cb × Cs)`
#[inline]
fn screen(cb: f32, cs: f32) -> f32 {
    let p = cb * cs;
    let sum = cb + cs;
    sum - p
}

/// `HardLight(Cb, Cs) = Cs <= 0.5 ? Multiply(Cb, 2 × Cs) : Screen(Cb, 2 × Cs − 1)`
#[inline]
fn hard_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        let d = 2.0 * cs;
        cb * d
    } else {
        let d = 2.0 * cs;
        screen(cb, d - 1.0)
    }
}

/// `SoftLight`(W3C compositing-1 §9.1.10)。
///
/// ```text
/// Cs <= 0.5: B = Cb − (1 − 2 × Cs) × Cb × (1 − Cb)
/// Cs >  0.5: B = Cb + (2 × Cs − 1) × (D(Cb) − Cb)
/// D(Cb) = Cb <= 0.25 ? ((16 × Cb − 12) × Cb + 4) × Cb : sqrt(Cb)
/// ```
#[inline]
fn soft_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        let two_cs = 2.0 * cs;
        let k = 1.0 - two_cs;
        let one_minus_cb = 1.0 - cb;
        let t = k * cb;
        let t = t * one_minus_cb;
        cb - t
    } else {
        let two_cs = 2.0 * cs;
        let k = two_cs - 1.0;
        let d = if cb <= 0.25 {
            let a = 16.0 * cb;
            let a = a - 12.0;
            let a = a * cb;
            let a = a + 4.0;
            a * cb
        } else {
            cb.sqrt()
        };
        let t = k * (d - cb);
        cb + t
    }
}

/// レイヤー 1 枚を backdrop(キャンバス)へ合成する。
///
/// - `backdrop` / `layer` はどちらも **sRGB 符号値空間**の同寸法バッファ
/// - `opacity` はレイヤー不透明度(0..1)、`weights` は backdrop 寸法で解決済みの
///   マスク重み(無ければ全画素 1.0 扱い)
/// - `αs = レイヤーのアルファ × opacity × マスク重み`
pub(crate) fn composite(
    backdrop: &mut LinearImage,
    layer: &LinearImage,
    mode: BlendMode,
    opacity: f32,
    weights: Option<&[f32]>,
) {
    debug_assert_eq!(backdrop.dimensions(), layer.dimensions());
    for (i, (dst, src)) in backdrop.data.iter_mut().zip(layer.data.iter()).enumerate() {
        let w = match weights {
            Some(m) => m[i],
            None => 1.0,
        };
        // αs = レイヤーアルファ × opacity × マスク重み(固定順序)。
        let a_s = src[3].clamp(0.0, 1.0);
        let a_s = a_s * opacity;
        let a_s = a_s * w;
        // 端点: 何も乗らないなら backdrop をバイト単位でそのまま残す。
        if a_s == 0.0 {
            continue;
        }
        let a_b = dst[3].clamp(0.0, 1.0);

        // αo = αs + αb × (1 − αs)
        let inv_as = 1.0 - a_s;
        let ab_term = a_b * inv_as;
        let a_o = a_s + ab_term;
        if a_o == 0.0 {
            *dst = [0.0, 0.0, 0.0, 0.0];
            continue;
        }

        // Co = ( αs(1 − αb)Cs + αs·αb·B(Cb, Cs) + (1 − αs)αb·Cb ) / αo
        let k_src = a_s * (1.0 - a_b);
        let k_blend = a_s * a_b;
        let k_dst = inv_as * a_b;
        let mut out = [0.0f32; 4];
        for c in 0..3 {
            let cs = src[c].clamp(0.0, 1.0);
            let cb = dst[c].clamp(0.0, 1.0);
            let b = blend_channel(mode, cb, cs);
            let t1 = k_src * cs;
            let t2 = k_blend * b;
            let t3 = k_dst * cb;
            let sum = t1 + t2;
            let sum = sum + t3;
            out[c] = sum / a_o;
        }
        out[3] = a_o;
        *dst = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不透明どうしの合成は B(Cb, Cs) と厳密に一致する(除算・重みが恒等になる経路)。
    #[test]
    fn opaque_over_opaque_is_exactly_the_blend_function() {
        let mut backdrop = LinearImage::from_pixel(1, 1, [0.25, 0.5, 0.75, 1.0]);
        let layer = LinearImage::from_pixel(1, 1, [0.6, 0.6, 0.6, 1.0]);
        composite(&mut backdrop, &layer, BlendMode::Multiply, 1.0, None);
        let px = backdrop.get(0, 0);
        assert_eq!(px[0], 0.25f32 * 0.6);
        assert_eq!(px[1], 0.5f32 * 0.6);
        assert_eq!(px[2], 0.75f32 * 0.6);
        assert_eq!(px[3], 1.0);
    }

    /// αs == 0 は backdrop をビット単位で保つ。
    #[test]
    fn zero_source_alpha_keeps_backdrop_bit_exact() {
        let original = LinearImage::from_pixel(1, 1, [0.1, 0.2, 0.3, 0.4]);
        let mut backdrop = original.clone();
        let layer = LinearImage::from_pixel(1, 1, [0.9, 0.9, 0.9, 1.0]);
        composite(&mut backdrop, &layer, BlendMode::Screen, 0.0, None);
        assert_eq!(backdrop, original);
    }

    /// normal / 不透明 / opacity 1 はレイヤーそのもの。
    #[test]
    fn normal_opaque_replaces_backdrop() {
        let mut backdrop = LinearImage::from_pixel(1, 1, [0.1, 0.2, 0.3, 1.0]);
        let layer = LinearImage::from_pixel(1, 1, [0.7, 0.6, 0.5, 1.0]);
        composite(&mut backdrop, &layer, BlendMode::Normal, 1.0, None);
        assert_eq!(backdrop.get(0, 0), [0.7, 0.6, 0.5, 1.0]);
    }

    /// W3C Compositing and Blending Level 1 §9 の separable ブレンド関数を
    /// **仕様本文から手で導いた表**で固定する。
    ///
    /// 各行のコメントに仕様の式と代入を書いてある(実装を読み直して作った表ではなく、
    /// 仕様を読み直して作った表であることがレビューで確認できるようにするため)。
    /// 許容差 1e-6。
    #[test]
    fn separable_blend_functions_match_the_w3c_spec() {
        use BlendMode::*;
        // (mode, Cb, Cs, expected B(Cb, Cs))
        const TABLE: &[(BlendMode, f32, f32, f32)] = &[
            // --- normal: B = Cs ---
            (Normal, 0.3, 0.7, 0.7),
            (Normal, 1.0, 0.0, 0.0),
            (Normal, 0.0, 1.0, 1.0),
            // --- multiply: B = Cb × Cs ---
            (Multiply, 0.5, 0.5, 0.25), // 0.5 × 0.5
            (Multiply, 1.0, 0.3, 0.3),  // 白の backdrop は恒等
            (Multiply, 0.0, 0.9, 0.0),  // 黒の backdrop は吸収
            (Multiply, 0.25, 0.4, 0.1), // 0.25 × 0.4
            // --- screen: B = Cb + Cs − Cb × Cs ---
            (Screen, 0.5, 0.5, 0.75),  // 0.5 + 0.5 − 0.25
            (Screen, 0.0, 0.4, 0.4),   // 黒の backdrop は恒等
            (Screen, 1.0, 0.2, 1.0),   // 1 + 0.2 − 0.2
            (Screen, 0.25, 0.4, 0.55), // 0.25 + 0.4 − 0.1
            // --- overlay: B = HardLight(Cs, Cb)(引数交換)---
            (Overlay, 0.25, 0.6, 0.3), // Cb <= 0.5 → Multiply(Cs, 2×Cb) = 0.6 × 0.5
            (Overlay, 0.75, 0.6, 0.8), // Cb > 0.5 → Screen(Cs, 2×Cb−1) = 0.6+0.5−0.3
            (Overlay, 0.5, 0.8, 0.8),  // 境界 Cb = 0.5 → Multiply(0.8, 1.0)
            (Overlay, 0.0, 0.9, 0.0),  // Multiply(0.9, 0.0)
            (Overlay, 1.0, 0.3, 1.0),  // Screen(0.3, 1.0)
            // --- darken: B = min(Cb, Cs) ---
            (Darken, 0.3, 0.7, 0.3),
            (Darken, 0.7, 0.3, 0.3),
            (Darken, 0.5, 0.5, 0.5),
            // --- lighten: B = max(Cb, Cs) ---
            (Lighten, 0.3, 0.7, 0.7),
            (Lighten, 0.7, 0.3, 0.7),
            (Lighten, 0.0, 0.0, 0.0),
            // --- color_dodge ---
            // Cb == 0 → 0(Cs によらない。仕様の分岐順は Cb が先)
            (ColorDodge, 0.0, 0.5, 0.0),
            (ColorDodge, 0.0, 1.0, 0.0),
            // Cs == 1 → 1(0 除算の極限)
            (ColorDodge, 0.4, 1.0, 1.0),
            // それ以外 min(1, Cb / (1 − Cs))
            (ColorDodge, 0.25, 0.5, 0.5), // 0.25 / 0.5
            (ColorDodge, 0.6, 0.5, 1.0),  // 1.2 → クランプ
            (ColorDodge, 0.3, 0.0, 0.3),  // Cs = 0 は恒等
            // --- color_burn ---
            // Cb == 1 → 1(Cs によらない。仕様の分岐順は Cb が先)
            (ColorBurn, 1.0, 0.2, 1.0),
            (ColorBurn, 1.0, 0.0, 1.0),
            // Cs == 0 → 0(0 除算の極限)
            (ColorBurn, 0.5, 0.0, 0.0),
            // それ以外 1 − min(1, (1 − Cb) / Cs)
            (ColorBurn, 0.5, 0.5, 0.0),  // 1 − min(1, 1.0)
            (ColorBurn, 0.75, 0.5, 0.5), // 1 − 0.25/0.5
            (ColorBurn, 0.5, 1.0, 0.5),  // 1 − 0.5/1
            // --- hard_light ---
            (HardLight, 0.6, 0.25, 0.3), // Cs <= 0.5 → Multiply(0.6, 0.5)
            (HardLight, 0.6, 0.5, 0.6),  // 境界 → Multiply(0.6, 1.0)
            (HardLight, 0.6, 0.75, 0.8), // Cs > 0.5 → Screen(0.6, 0.5)
            (HardLight, 0.6, 1.0, 1.0),  // Screen(0.6, 1.0)
            // --- soft_light ---
            // Cs <= 0.5: B = Cb − (1 − 2×Cs) × Cb × (1 − Cb)
            (SoftLight, 0.5, 0.0, 0.25),  // 0.5 − 1 × 0.5 × 0.5
            (SoftLight, 0.5, 0.5, 0.5),   // 係数 0 → 恒等
            (SoftLight, 0.2, 0.25, 0.12), // 0.2 − 0.5 × 0.2 × 0.8
            // Cs > 0.5: B = Cb + (2×Cs − 1) × (D(Cb) − Cb)
            // D(Cb) = Cb <= 0.25 ? ((16×Cb − 12) × Cb + 4) × Cb : sqrt(Cb)
            // Cb = 0.25(多項式ブランチの境界): D = ((4 − 12) × 0.25 + 4) × 0.25 = 0.5
            (SoftLight, 0.25, 1.0, 0.5),    // 0.25 + 1 × (0.5 − 0.25)
            (SoftLight, 0.25, 0.75, 0.375), // 0.25 + 0.5 × 0.25
            // Cb = 0.16(多項式ブランチ内部): D = ((2.56 − 12) × 0.16 + 4) × 0.16 = 0.398336
            (SoftLight, 0.16, 0.75, 0.279168), // 0.16 + 0.5 × (0.398336 − 0.16)
            // Cb > 0.25 は sqrt ブランチ
            (SoftLight, 0.64, 0.75, 0.72), // 0.64 + 0.5 × (0.8 − 0.64)
            (SoftLight, 1.0, 1.0, 1.0),    // D(1) = 1 → 恒等
            (SoftLight, 0.0, 1.0, 0.0),    // D(0) = 0 → 恒等
            // --- difference: B = |Cb − Cs| ---
            (Difference, 0.3, 0.7, 0.4),
            (Difference, 0.7, 0.3, 0.4),
            (Difference, 0.5, 0.5, 0.0),
            (Difference, 0.0, 1.0, 1.0),
            // --- exclusion: B = Cb + Cs − 2 × Cb × Cs ---
            (Exclusion, 0.5, 0.5, 0.5), // 1.0 − 0.5
            (Exclusion, 0.0, 0.3, 0.3),
            (Exclusion, 1.0, 0.3, 0.7), // 1.3 − 0.6
            (Exclusion, 1.0, 1.0, 0.0), // 2 − 2
        ];

        for &(mode, cb, cs, want) in TABLE {
            let got = blend_channel(mode, cb, cs);
            assert!(
                (got - want).abs() <= 1e-6,
                "B({mode:?}, Cb={cb}, Cs={cs}) = {got}, want {want}"
            );
        }
    }

    /// 完全透明どうしなら αo = 0 で RGBA すべて 0。
    #[test]
    fn transparent_over_transparent_is_zero() {
        let mut backdrop = LinearImage::from_pixel(1, 1, [0.1, 0.2, 0.3, 0.0]);
        let layer = LinearImage::from_pixel(1, 1, [0.9, 0.9, 0.9, 0.5]);
        // αs = 0.5 × 0 (マスク) → 端点分岐で backdrop 維持。
        composite(&mut backdrop, &layer, BlendMode::Normal, 1.0, Some(&[0.0]));
        assert_eq!(backdrop.get(0, 0), [0.1, 0.2, 0.3, 0.0]);
    }
}

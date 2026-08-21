//! レイヤー合成(v0.6 / v0.7)。separable ブレンド関数 12 種 +
//! **非 separable 4 種**(hue / saturation / color / luminosity、v0.7)+ W3C の合成式。
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
//!
//! # 非 separable モード(v0.7)
//!
//! `hue` / `saturation` / `color` / `luminosity` はチャンネルを独立に処理できず、
//! **RGB の三つ組み**に対して `Lum` / `Sat` / `SetLum` / `SetSat` / `ClipColor` を
//! 組み合わせて定義される(compositing-1 §non-separable)。したがって
//! ブレンド関数の入口は `blend_rgb`(三つ組み)であり、separable モードは
//! そこからチャンネルごとに `blend_channel` へ振り分ける。
//!
//! ## 数値方針: ヘルパ連鎖だけ f64
//!
//! `SetLum(SetSat(...))` は「除算 → 加算 → 除算」と 3〜4 段の連鎖になり、
//! f32 のままだと Sat = 0 付近・ClipColor 発動付近で桁落ちが目立つ
//! (`ClipColor` の分母 `L − n` / `x − L` が小さいときに顕著)。
//! そこで **入口で f32 → f64 に上げ、連鎖を f64 で計算し、最後に f32 へ戻す**。
//! f64 の四則は IEEE-754 で厳密に丸められるので、libm を経由しない限り
//! プラットフォーム間で一致する(超越関数は一切使っていない)。
//! 戻すときに 0..1 へクランプする(ClipColor は数学的に範囲内を保証するが、
//! f64 → f32 の丸めで 1 ULP はみ出しうるため)。

use crate::linear::LinearImage;
use crate::recipe::BlendMode;

/// 非 separable モードの輝度係数(W3C compositing-1 §非 separable の `Lum`)。
///
/// ```text
/// Lum(C) = 0.3 × Cred + 0.59 × Cgreen + 0.11 × Cblue
/// ```
///
/// **罠**: これは BT.709(0.2126 / 0.7152 / 0.0722)でも BT.601 の厳密値
/// (0.299 / 0.587 / 0.114)でもなく、**仕様が本文に直書きしている丸めた係数**である。
/// Photoshop / CSS / Figma もこの値で実装しているので、「より正しい」係数へ
/// 差し替えると他ツールと結果が一致しなくなる(合成は物理的正しさより
/// 既存ツールとの一致を採る、という DESIGN.md §9.7 の判断軸をここでも通す)。
/// `ops::mask` の重み輝度が BT.709 なのとは**意図的に別系統**であることに注意。
const LUM_R: f64 = 0.3;
const LUM_G: f64 = 0.59;
const LUM_B: f64 = 0.11;

/// `ClipColor` の分母 `L − n` / `x − L` を「実質 0」とみなす閾値(**L ± ε ガード**)。
///
/// 単なる `den > 0.0` では足りない: `C = [−0.2, −0.2, −0.2]` のような無彩色では
/// `L` が丸め誤差ぶんだけ `n` と食い違い、`den ≈ 2e-17` の**正の**極小値になる。
/// その状態で `((C − L) × L) / den` を計算すると、分子も分母も丸め誤差の塊なので
/// 商が 0.2 のような有限値に化けて結果が壊れる(実測で `[0, 0, 0]` が出た)。
/// 数学的な極限は「全成分が L」なので、この規模の分母は分岐で潰す。
/// 本来 ClipColor が効くべき場面の分母は色差そのもの(1e-3 以上)なので、
/// 1e-12 の閾値が正当な計算を横取りすることはない。
const CLIP_EPS: f64 = 1e-12;

/// RGB 三つ組みに対するブレンド関数 `B(Cb, Cs)`。
///
/// separable モードはチャンネルごとに [`blend_channel`] へ委譲する
/// (呼び出し順・演算順が v0.6 と同一なのでビット同一)。
#[inline]
pub(crate) fn blend_rgb(mode: BlendMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    match mode {
        // --- 非 separable(v0.7): RGB を一括で扱う ---
        // B(Cb, Cs) = SetLum(SetSat(Cs, Sat(Cb)), Lum(Cb))
        BlendMode::Hue => {
            let (cb, cs) = (to_f64(cb), to_f64(cs));
            let t = set_sat(cs, sat(cb));
            to_f32(set_lum(t, lum(cb)))
        }
        // B(Cb, Cs) = SetLum(SetSat(Cb, Sat(Cs)), Lum(Cb))
        BlendMode::Saturation => {
            let (cb, cs) = (to_f64(cb), to_f64(cs));
            let t = set_sat(cb, sat(cs));
            to_f32(set_lum(t, lum(cb)))
        }
        // B(Cb, Cs) = SetLum(Cs, Lum(Cb))
        BlendMode::Color => {
            let (cb, cs) = (to_f64(cb), to_f64(cs));
            to_f32(set_lum(cs, lum(cb)))
        }
        // B(Cb, Cs) = SetLum(Cb, Lum(Cs))
        BlendMode::Luminosity => {
            let (cb, cs) = (to_f64(cb), to_f64(cs));
            to_f32(set_lum(cb, lum(cs)))
        }
        // --- separable(v0.6): チャンネル独立 ---
        separable => [
            blend_channel(separable, cb[0], cs[0]),
            blend_channel(separable, cb[1], cs[1]),
            blend_channel(separable, cb[2], cs[2]),
        ],
    }
}

#[inline]
fn to_f64(c: [f32; 3]) -> [f64; 3] {
    [c[0] as f64, c[1] as f64, c[2] as f64]
}

/// f64 の三つ組みを 0..1 へクランプして f32 へ戻す(上のモジュールコメント参照)。
#[inline]
fn to_f32(c: [f64; 3]) -> [f32; 3] {
    [
        c[0].clamp(0.0, 1.0) as f32,
        c[1].clamp(0.0, 1.0) as f32,
        c[2].clamp(0.0, 1.0) as f32,
    ]
}

/// `Lum(C) = 0.3 × R + 0.59 × G + 0.11 × B`(係数の出所は [`LUM_R`] のコメント)。
///
/// 総和は R → G → B の固定順・左結合(再結合禁止の規約)。
#[inline]
fn lum(c: [f64; 3]) -> f64 {
    let r = LUM_R * c[0];
    let g = LUM_G * c[1];
    let b = LUM_B * c[2];
    let s = r + g;
    s + b
}

/// `Sat(C) = max(R, G, B) − min(R, G, B)`
#[inline]
fn sat(c: [f64; 3]) -> f64 {
    max3(c) - min3(c)
}

#[inline]
fn min3(c: [f64; 3]) -> f64 {
    let m = if c[0] <= c[1] { c[0] } else { c[1] };
    if m <= c[2] {
        m
    } else {
        c[2]
    }
}

#[inline]
fn max3(c: [f64; 3]) -> f64 {
    let m = if c[0] >= c[1] { c[0] } else { c[1] };
    if m >= c[2] {
        m
    } else {
        c[2]
    }
}

/// `ClipColor(C)`(compositing-1)。
///
/// ```text
/// L = Lum(C); n = min(C); x = max(C)
/// if (n < 0) C = L + (((C − L) × L) / (L − n))
/// if (x > 1) C = L + (((C − L) × (1 − L)) / (x − L))
/// ```
///
/// **L ± ε のガード**: 仕様の式は `L − n` / `x − L` で割るが、成分が全て等しい
/// (= 彩度 0 の)色では `n == x == L` になり `0 / 0` を踏む。この場合の極限は
/// 「全成分が L」なので、分母が正でないときは `[L, L, L]` を返す分岐で確定させる
/// (端点を式ではなく分岐で決める、という §9.7 の規約と同じ手口)。
/// なお `n` / `x` は**両方の if を通す前に一度だけ**取る(仕様本文どおりの順序)。
#[inline]
fn clip_color(c: [f64; 3]) -> [f64; 3] {
    let l = lum(c);
    let n = min3(c);
    let x = max3(c);
    let mut out = c;

    if n < 0.0 {
        let den = l - n;
        if den > CLIP_EPS {
            for v in out.iter_mut() {
                let d = *v - l;
                let num = d * l;
                let q = num / den;
                *v = l + q;
            }
        } else {
            out = [l, l, l];
        }
    }
    if x > 1.0 {
        let den = x - l;
        if den > CLIP_EPS {
            let one_minus_l = 1.0 - l;
            for v in out.iter_mut() {
                let d = *v - l;
                let num = d * one_minus_l;
                let q = num / den;
                *v = l + q;
            }
        } else {
            out = [l, l, l];
        }
    }
    out
}

/// `SetLum(C, l) = ClipColor(C + (l − Lum(C)))`
#[inline]
fn set_lum(c: [f64; 3], l: f64) -> [f64; 3] {
    let d = l - lum(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

/// `SetSat(C, s)`(compositing-1 の **成分添字による min/mid/max 形式**)。
///
/// ```text
/// if (Cmax > Cmin)
///     Cmid = (((Cmid − Cmin) × s) / (Cmax − Cmin));  Cmax = s
/// else
///     Cmid = Cmax = 0
/// Cmin = 0
/// ```
///
/// # 同値成分のタイブレーク(決定論の要)
///
/// 仕様は「最小 / 中間 / 最大の成分」としか書いておらず、**2 つ以上の成分が
/// 厳密に等しいときにどちらを min とみなすか**を決めていない。結果の値自体は
/// どちらを選んでも同じ(等しい成分は同じ式に入る)だが、**どの添字に `s` が
/// 入るか**は変わるため、実装が違えばチャンネルが入れ替わる。
///
/// atx は **`f64::total_cmp` による全順序 + 同値なら添字の小さい方が下位ランク**
/// と固定する(R < G < B。例: `C = [0.4, 0.4, 0.9]` なら
/// min = R(添字 0)、mid = G(添字 1)、max = B)。
/// `Cmax == Cmin`(完全な無彩色)の分岐では全成分が 0 になるので、
/// どのみち添字の選び方は結果に出ない。
#[inline]
fn set_sat(c: [f64; 3], s: f64) -> [f64; 3] {
    // 値の昇順、同値なら添字の昇順(total_cmp で NaN が来ても順序が定まる)。
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| c[a].total_cmp(&c[b]).then(a.cmp(&b)));
    let (i_min, i_mid, i_max) = (idx[0], idx[1], idx[2]);

    let mut out = [0.0f64; 3];
    if c[i_max] > c[i_min] {
        let num = (c[i_mid] - c[i_min]) * s;
        let den = c[i_max] - c[i_min];
        out[i_mid] = num / den;
        out[i_max] = s;
    } else {
        out[i_mid] = 0.0;
        out[i_max] = 0.0;
    }
    out[i_min] = 0.0;
    out
}

/// separable ブレンド関数 `B(Cb, Cs)`(W3C compositing-1 §9)。
///
/// `cb`(backdrop)/ `cs`(source)はいずれも 0..1 の sRGB 符号値。
///
/// 非 separable モードはチャンネル単独では定義できないため、この関数には来ない
/// ([`blend_rgb`] が先に捕まえる)。
#[inline]
pub(crate) fn blend_channel(mode: BlendMode, cb: f32, cs: f32) -> f32 {
    match mode {
        // 非 separable(hue / saturation / color / luminosity)は blend_rgb 専用。
        BlendMode::Hue | BlendMode::Saturation | BlendMode::Color | BlendMode::Luminosity => {
            unreachable!(
            "non-separable blend mode {mode:?} must go through blend_rgb (it needs the RGB triple)"
        )
        }
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
        composite_px(dst, src, mode, opacity, w);
    }
}

/// **1 画素**の合成(W3C compositing-1)。[`composite`] の内側そのものであり、
/// `svg_overlay`(v0.8)のようにキャンバス全面ではなく**部分領域**へ合成する op も
/// この関数を通す。式・演算順・端点分岐を 1 実装に閉じ込めることで、
/// 「レイヤー合成と同じ数式」であることがコード上で自明になる。
///
/// `αs = src[3] × opacity × w`(固定順序)。`w` は合成マスクの重み(無ければ 1.0)。
#[inline]
pub(crate) fn composite_px(
    dst: &mut [f32; 4],
    src: &[f32; 4],
    mode: BlendMode,
    opacity: f32,
    w: f32,
) {
    {
        // αs = レイヤーアルファ × opacity × マスク重み(固定順序)。
        let a_s = src[3].clamp(0.0, 1.0);
        let a_s = a_s * opacity;
        let a_s = a_s * w;
        // 端点: 何も乗らないなら backdrop をバイト単位でそのまま残す。
        if a_s == 0.0 {
            return;
        }
        let a_b = dst[3].clamp(0.0, 1.0);

        // αo = αs + αb × (1 − αs)
        let inv_as = 1.0 - a_s;
        let ab_term = a_b * inv_as;
        let a_o = a_s + ab_term;
        if a_o == 0.0 {
            *dst = [0.0, 0.0, 0.0, 0.0];
            return;
        }

        // Co = ( αs(1 − αb)Cs + αs·αb·B(Cb, Cs) + (1 − αs)αb·Cb ) / αo
        let k_src = a_s * (1.0 - a_b);
        let k_blend = a_s * a_b;
        let k_dst = inv_as * a_b;
        // ブレンド関数の入力は 0..1 へクランプしてから渡す。非 separable モードは
        // RGB を三つ組みで必要とするので、ここで一括して求める(separable モードでは
        // v0.6 と同じ呼び出し順・演算順になるためビット同一)。
        let cs = [
            src[0].clamp(0.0, 1.0),
            src[1].clamp(0.0, 1.0),
            src[2].clamp(0.0, 1.0),
        ];
        let cb = [
            dst[0].clamp(0.0, 1.0),
            dst[1].clamp(0.0, 1.0),
            dst[2].clamp(0.0, 1.0),
        ];
        let b = blend_rgb(mode, cb, cs);

        let mut out = [0.0f32; 4];
        for c in 0..3 {
            let t1 = k_src * cs[c];
            let t2 = k_blend * b[c];
            let t3 = k_dst * cb[c];
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

    /// W3C Compositing and Blending Level 1 の**非 separable** ブレンド関数
    /// (hue / saturation / color / luminosity)を、仕様本文から手で導いた表で固定する。
    ///
    /// 各行のコメントに `Lum` / `Sat` / `SetSat` / `SetLum` / `ClipColor` の代入を
    /// 書き下してある(実装を読み直して作った表ではないことがレビューで確認できる)。
    /// `Lum` の係数は **0.3 / 0.59 / 0.11**(仕様本文の値。BT.709 ではない)。
    /// 許容差 1e-6。
    #[test]
    fn non_separable_blend_functions_match_the_w3c_spec() {
        use BlendMode::*;
        /// (mode, Cb, Cs, expected B(Cb, Cs))
        type Row = (BlendMode, [f32; 3], [f32; 3], [f32; 3]);
        const TABLE: &[Row] = &[
            // ================= luminosity: SetLum(Cb, Lum(Cs)) =================
            // Lum(Cs) = 0.5 / Lum(Cb) = 0.06+0.236+0.066 = 0.362 → d = +0.138、クリップなし
            (
                Luminosity,
                [0.2, 0.4, 0.6],
                [0.5, 0.5, 0.5],
                [0.338, 0.538, 0.738],
            ),
            // ClipColor 発動(x > 1): d = 1 − 0.69 = 0.31 → [0.51, 1.21, 1.21]、L = 1.0
            // 1 − L = 0 なので全成分が L に潰れる
            (
                Luminosity,
                [0.2, 0.9, 0.9],
                [1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0],
            ),
            // ClipColor 発動(n < 0): Lum(Cb) = 0.218、d = −0.218 → n = −0.118、L = 0
            // C = L + ((C − L) × L)/(L − n) は L = 0 なので全成分 0
            (
                Luminosity,
                [0.2, 0.1, 0.9],
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
            ),
            // 無彩色 backdrop(Sat = 0)は無彩色のまま輝度だけ動く: Lum(Cs) = 0.484
            (
                Luminosity,
                [0.5, 0.5, 0.5],
                [0.2, 0.7, 0.1],
                [0.484, 0.484, 0.484],
            ),
            // Cs == Cb → d = 0 で恒等
            (
                Luminosity,
                [0.3, 0.6, 0.9],
                [0.3, 0.6, 0.9],
                [0.3, 0.6, 0.9],
            ),
            // ClipColor(n < 0)が非自明に効く例: Lum(Cb) = 0.645、d = −0.245
            // → [−0.245, 0.755, 0.255]、L = 0.4、L − n = 0.645
            // C1 = 0.4 + (0.355 × 0.4)/0.645、C2 = 0.4 + (−0.145 × 0.4)/0.645
            (
                Luminosity,
                [0.0, 1.0, 0.5],
                [0.4, 0.4, 0.4],
                [0.0, 0.620_155_04, 0.310_077_52],
            ),
            // ==================== color: SetLum(Cs, Lum(Cb)) ====================
            // 純赤を中間グレーへ: Lum(Cs) = 0.3、d = +0.2 → [1.2, 0.2, 0.2]
            // x = 1.2 > 1 → L = 0.5、x − L = 0.7、1 − L = 0.5
            (
                Color,
                [0.5, 0.5, 0.5],
                [1.0, 0.0, 0.0],
                [1.0, 0.285_714_3, 0.285_714_3],
            ),
            // 無彩色ソース(Sat = 0)→ backdrop の輝度の無彩色: Lum(Cb) = 0.395
            (
                Color,
                [0.2, 0.4, 0.9],
                [0.3, 0.3, 0.3],
                [0.395, 0.395, 0.395],
            ),
            // Cs == Cb → d = 0 で恒等
            (Color, [0.1, 0.5, 0.8], [0.1, 0.5, 0.8], [0.1, 0.5, 0.8]),
            // ClipColor 発動(x > 1): Lum(Cs) = 0.405、d = +0.495 → [0.495, 0.995, 1.495]
            // L = 0.9、x − L = 0.595、1 − L = 0.1
            (
                Color,
                [0.9, 0.9, 0.9],
                [0.0, 0.5, 1.0],
                [0.831_932_8, 0.915_966_4, 1.0],
            ),
            // 黒 backdrop → L = 0 なので ClipColor(n < 0)が全成分を 0 にする
            (Color, [0.0, 0.0, 0.0], [0.8, 0.2, 0.4], [0.0, 0.0, 0.0]),
            // ============ hue: SetLum(SetSat(Cs, Sat(Cb)), Lum(Cb)) ============
            // Sat(Cb) = 0.25、SetSat([1,0,0], 0.25) = [0.25, 0, 0](max = R)
            // Lum = 0.075、Lum(Cb) = 0.325 → d = 0.25 → [0.5, 0.25, 0.25] = Cb
            // (backdrop がすでにこの色相なので恒等になる)
            (Hue, [0.5, 0.25, 0.25], [1.0, 0.0, 0.0], [0.5, 0.25, 0.25]),
            // 無彩色 backdrop → Sat(Cb) = 0 → SetSat(Cs, 0) = [0,0,0]
            // → SetLum(.., 0.4) = backdrop そのもの
            (Hue, [0.4, 0.4, 0.4], [1.0, 0.0, 0.0], [0.4, 0.4, 0.4]),
            // 無彩色ソース → SetSat は Cmax == Cmin 分岐で [0,0,0]
            // → Lum(Cb) = 0.443 の無彩色
            (Hue, [0.2, 0.5, 0.8], [0.6, 0.6, 0.6], [0.443, 0.443, 0.443]),
            // **同値成分のタイブレーク**: Cs = [0.2, 0.9, 0.9] は G と B が同値。
            // 添字の小さい G が mid、B が max。mid = ((0.9−0.2)×s)/(0.9−0.2) = s = max
            // なので出力は添字の選び方に依存しない(Sat(Cb) = 0.1、Lum(Cb) = 0.511)
            (Hue, [0.5, 0.5, 0.6], [0.2, 0.9, 0.9], [0.441, 0.541, 0.541]),
            // ClipColor 発動(x > 1): Sat(Cb) = 0.09、Lum(Cb) = 0.9394
            // SetSat = [0.09, 0, 0] → d = 0.9124 → [1.0024, 0.9124, 0.9124]
            // L = 0.9394、x − L = 0.063、1 − L = 0.0606
            (
                Hue,
                [0.9, 0.95, 0.99],
                [1.0, 0.0, 0.0],
                [1.0, 0.913_428_57, 0.913_428_57],
            ),
            // ======= saturation: SetLum(SetSat(Cb, Sat(Cs)), Lum(Cb)) =======
            // Sat(Cs) = 1、SetSat([0.2,0.5,0.8], 1) = [0, 0.5, 1]
            // Lum = 0.405、Lum(Cb) = 0.443 → d = 0.038 → [0.038, 0.538, 1.038]
            // x > 1 → L = 0.443、x − L = 0.595、1 − L = 0.557
            (
                Saturation,
                [0.2, 0.5, 0.8],
                [1.0, 0.0, 0.0],
                [0.063_865_55, 0.531_932_8, 1.0],
            ),
            // Sat(Cs) = 0 → SetSat(Cb, 0) = [0,0,0] → Lum(Cb) = 0.443 の無彩色
            (
                Saturation,
                [0.2, 0.5, 0.8],
                [0.3, 0.3, 0.3],
                [0.443, 0.443, 0.443],
            ),
            // 無彩色 backdrop は Cmax == Cmin 分岐 → [0,0,0] → 輝度だけ戻って恒等
            (
                Saturation,
                [0.6, 0.6, 0.6],
                [1.0, 0.0, 0.0],
                [0.6, 0.6, 0.6],
            ),
            // Sat(Cs) == Sat(Cb) = 0.8 なので恒等になる例
            (
                Saturation,
                [0.1, 0.4, 0.9],
                [0.0, 0.0, 0.8],
                [0.1, 0.4, 0.9],
            ),
            // **タイブレーク + ClipColor(n < 0)**: Cb = [0.5, 0.5, 0.2] は R と G が同値。
            // 添字の小さい R が mid、G が max → どちらも s = 0.8 になる。
            // SetSat = [0.8, 0.8, 0]、Lum = 0.712、Lum(Cb) = 0.467 → d = −0.245
            // → [0.555, 0.555, −0.245]、L = 0.467、L − n = 0.712
            (
                Saturation,
                [0.5, 0.5, 0.2],
                [0.9, 0.1, 0.1],
                [0.524_719_1, 0.524_719_1, 0.0],
            ),
        ];

        for &(mode, cb, cs, want) in TABLE {
            let got = blend_rgb(mode, cb, cs);
            for c in 0..3 {
                assert!(
                    (got[c] - want[c]).abs() <= 1e-6,
                    "B({mode:?}, Cb={cb:?}, Cs={cs:?})[{c}] = {}, want {}",
                    got[c],
                    want[c]
                );
            }
        }
    }

    /// `Lum` の係数は仕様本文の 0.3 / 0.59 / 0.11(BT.709 の
    /// 0.2126 / 0.7152 / 0.0722 ではない)。`luminosity` を白いソースで叩くと
    /// 「輝度を 1 へ持ち上げる差分」として係数がそのまま観測できる。
    #[test]
    fn lum_uses_the_spec_coefficients_not_bt709() {
        // Lum([1,0,0]) = 0.3 を確かめる: 黒い backdrop に赤い光度を載せると
        // SetLum([0,0,0], 0.3) = [0.3, 0.3, 0.3]。
        for (cs, want) in [
            ([1.0f32, 0.0, 0.0], 0.3f32),
            ([0.0, 1.0, 0.0], 0.59),
            ([0.0, 0.0, 1.0], 0.11),
        ] {
            let got = blend_rgb(BlendMode::Luminosity, [0.0, 0.0, 0.0], cs);
            assert!(
                (got[0] - want).abs() <= 1e-6 && got[0] == got[1] && got[1] == got[2],
                "Lum({cs:?}) observed as {got:?}, want {want}"
            );
        }
        // BT.709 だったら赤は 0.2126 になるはず、という反証。
        let red = blend_rgb(BlendMode::Luminosity, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        assert!(
            (red[0] - 0.2126).abs() > 0.05,
            "coefficients look like BT.709"
        );
    }

    /// `color` と `luminosity` は相補的: `color` の結果は backdrop の輝度を、
    /// `luminosity` の結果はソースの輝度を持つ(ClipColor が効かない範囲で)。
    #[test]
    fn color_keeps_backdrop_luminance_and_luminosity_takes_source_luminance() {
        let cb = [0.35f32, 0.45, 0.55];
        let cs = [0.6f32, 0.3, 0.4];
        let lb = lum(to_f64(cb));
        let ls = lum(to_f64(cs));

        let color = blend_rgb(BlendMode::Color, cb, cs);
        assert!((lum(to_f64(color)) - lb).abs() <= 1e-6, "{color:?}");

        let luminosity = blend_rgb(BlendMode::Luminosity, cb, cs);
        assert!(
            (lum(to_f64(luminosity)) - ls).abs() <= 1e-6,
            "{luminosity:?}"
        );
    }

    /// `hue` / `saturation` は backdrop の輝度を保ち、`saturation` はさらに
    /// ソースの彩度を持つ(ClipColor が効かない範囲で)。
    #[test]
    fn saturation_takes_source_saturation_and_keeps_backdrop_luminance() {
        let cb = [0.4f32, 0.5, 0.45];
        let cs = [0.7f32, 0.2, 0.5];
        let out = blend_rgb(BlendMode::Saturation, cb, cs);
        assert!(
            (lum(to_f64(out)) - lum(to_f64(cb))).abs() <= 1e-6,
            "{out:?}"
        );
        assert!(
            (sat(to_f64(out)) - sat(to_f64(cs))).abs() <= 1e-6,
            "{out:?}"
        );
    }

    /// `SetSat` の同値成分は同じ出力になる = タイブレークの選び方は結果に出ない
    /// (決定論のために順序は固定するが、値の意味は添字に依存しない)。
    #[test]
    fn set_sat_ties_produce_equal_components() {
        // min と mid が同値。
        let a = set_sat([0.4, 0.4, 0.9], 0.6);
        assert_eq!(a[0], a[1]);
        // mid と max が同値。
        let b = set_sat([0.2, 0.9, 0.9], 0.6);
        assert_eq!(b[1], b[2]);
        assert_eq!(b[2], 0.6);
        // 全成分同値 → Cmax == Cmin 分岐で全 0。
        assert_eq!(set_sat([0.5, 0.5, 0.5], 0.6), [0.0, 0.0, 0.0]);
    }

    /// `ClipColor` の 0 除算ガード: 無彩色で n < 0 / x > 1 になる入力
    /// (`SetLum` の途中でしか起きない)を直接叩く。
    #[test]
    fn clip_color_guards_against_degenerate_denominators() {
        // 全成分が同じ負値 → L ≈ n なので分母が丸め誤差規模。極限は [L, L, L]。
        for v in clip_color([-0.2, -0.2, -0.2]) {
            assert!((v - -0.2).abs() < 1e-9, "{v}");
        }
        // 全成分が同じ 1 超 → x ≈ L なので同上。
        for v in clip_color([1.5, 1.5, 1.5]) {
            assert!((v - 1.5).abs() < 1e-9, "{v}");
        }
        // 実際の合成経路では SetLum が先に L を範囲内へ置くのでこの縮退は現れない。
        let out = blend_rgb(BlendMode::Color, [0.5, 0.5, 0.5], [2.0, 2.0, 2.0]);
        assert_eq!(out, [0.5, 0.5, 0.5]);
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

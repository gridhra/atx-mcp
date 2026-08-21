//! 仕上げ系 op: vignette(周辺減光)/ grain(フィルム粒状)。
//!
//! - vignette: リニア光の露光減衰として実装(strength 正=減光、負=増光)。
//!   中心からの正規化距離 d(半対角比)に対し radius..radius+feather で滑らかに減衰。
//!   係数計算は f64 → 1e-6 量子化 → 固定順序 f32
//! - grain: 座標 + seed のハッシュ由来の決定論ノイズ(sRGB 空間、知覚的)。
//!   乱数クレート不使用。size はブロック拡大(1=画素粒)。monochrome は輝度のみ変調
//!
//! # flip の空間非依存性
//!
//! `flip` はバッファ要素の**純粋な置換**で、画素値そのものには一切触れない。
//! したがって現在の作業空間(線形光 / sRGB 符号値)がどちらでも結果は同じであり、
//! `engine::op_space` は `flip` に対して `None` を返す(空間変換を挟まない)。
//! 「水平反転 2 回 = 恒等」がバイト同一で成り立つのはこの性質による。
//!
//! # vignette の減衰曲線(設計の固定)
//!
//! 画素中心を連続座標 `(x + 0.5, y + 0.5)`、画像中心を `(w/2, h/2)` として
//!
//! ```text
//! dx = (x + 0.5) - w/2            dy = (y + 0.5) - h/2
//! half_diag = sqrt(w^2 + h^2) / 2
//! d = sqrt(dx^2 + dy^2) / half_diag        (角で d ≈ 1)
//! ```
//!
//! `dx^2` は列ごと、`dy^2` は行ごとに f64 で**先に**計算して使い回す
//! (画素ごとの再計算を避けつつ、値は完全に同一。`sqrt` は IEEE754 で
//! 正しく丸められる演算なのでプラットフォーム差を持たない)。
//!
//! ゲインは **smoothstep**(3次エルミート `s = t^2 (3 - 2t)`)で決める。
//! cosine 版(`(1 - cos(pi t)) / 2`)ではなく多項式を選んだのは、
//! 画素ループに libm(`cos`)を持ち込まないという `ops/mod.rs` の決定論規約に
//! 素直に従えるため。両者の形状差は微小(最大 ~2.8%)で、実用上の見た目は変わらない。
//!
//! ```text
//! d <= radius                 -> gain = 1.0
//! radius < d < radius+feather -> t = (d - radius) / feather
//!                                s = t^2 * (3 - 2t)
//!                                gain = 1.0 - strength * s
//! d >= radius+feather         -> gain = 1.0 - strength
//! ```
//!
//! `feather == 0` は段差(radius で階段状に切り替わる)。
//! `strength` が負なら `gain > 1` になり周辺が**明るく**なる。
//! `strength ∈ [-1, 1]`, `s ∈ [0, 1]` なので **gain は常に 0..=2** に収まり、
//! 負にはならない(下限クランプは不要。上限も**あえてクランプしない** —
//! 線形光の 1.0 超えは出口の `unit_to_u8` / `unit_to_u16` が最終的に
//! 飽和させるので、途中の op へは overshoot をそのまま渡す方が情報が落ちない)。
//!
//! ゲインは f64 で求めてから **1e-6 グリッドへ量子化**して f32 化し、
//! RGB へ固定順序で掛ける(アルファには触れない)。
//!
//! # grain のノイズ(設計の固定)
//!
//! ブロック座標 `(x / size, y / size)`(整数除算 = 最近傍のブロック拡大)と
//! `seed`・チャンネル添字から **splitmix64 の整数ミックス**でハッシュを引く。
//! 浮動小数を一切経由しない整数演算だけなので、値はプラットフォームを跨いで厳密に同じ。
//!
//! ```text
//! h      = splitmix64 の多段ミックス(seed, block_x, block_y, channel)
//! u      = (h >> 32) as u32 / u32::MAX      (0..=1)
//! noise  = u * 2 - 1                        (-1..=1)
//! delta  = quantize_1e6(noise * amount * GRAIN_SCALE)
//! ```
//!
//! `monochrome = true` は channel = 0 のハッシュを RGB 共通に使う
//! (= 輝度だけが揺れる、銀塩フィルムに近い粒)。`false` はチャンネルごとに
//! 独立したハッシュを引く(カラーノイズ)。どちらもアルファは不変。
//!
//! `delta` は **sRGB 符号値**へ直接加算して 0..1 でクランプする
//! (粒状感は知覚量なので線形光ではなく符号値で乗せる)。

use crate::linear::{quantize_1e6, LinearImage};
use crate::parallel;
use crate::recipe::FlipDirection;
use crate::{AtxError::InvalidRecipe, Result};

/// vignette の strength 許容範囲(負=増光)。
const VIGNETTE_STRENGTH_MAX: f64 = 1.0;
/// vignette の radius 許容範囲上限(半対角比。1.0 超は「角まで完全に平坦」を許すため)。
const VIGNETTE_RADIUS_MAX: f64 = 1.5;
/// vignette の feather 許容範囲上限。
const VIGNETTE_FEATHER_MAX: f64 = 1.0;

/// grain の amount 許容範囲上限。
const GRAIN_AMOUNT_MAX: f64 = 1.0;
/// grain の粒サイズ許容範囲。
const GRAIN_SIZE_MIN: u32 = 1;
const GRAIN_SIZE_MAX: u32 = 4;

/// `amount = 1.0` のときの sRGB 符号値上の最大振幅。
///
/// `0.12` は 8bit 換算で **±30.6/255**。銀塩の粒状やデジタルの高感度ノイズは
/// 実写でおよそ ±5〜30 段の範囲に収まるので、「amount = 1 で目一杯効かせた状態」が
/// その上端に一致するように選んだ。これより大きいと amount の下側 1/3 しか
/// 実用域が無くなり、小さいと「最大にしても効かない」スライダになる。
const GRAIN_SCALE: f64 = 0.12;

/// u32 の全域を 0..=1 へ写すための分母。
const U32_SPAN: f64 = u32::MAX as f64;

// ----------------------------------------------------------------- validate

/// vignette の静的検証: strength は有限かつ -1..=1、radius は有限かつ 0..=1.5、
/// feather は有限かつ 0..=1。
///
/// `strength == 0`(恒等)は無意味だがエラーにはしない。
/// レシピを機械生成する側が「効果なし」を素直に表現できる方が扱いやすいため
/// (`levels` の恒等指定を許しているのと同じ方針)。
pub fn validate_vignette(index: usize, strength: f64, radius: f64, feather: f64) -> Result<()> {
    if !strength.is_finite()
        || !(-VIGNETTE_STRENGTH_MAX..=VIGNETTE_STRENGTH_MAX).contains(&strength)
    {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (vignette): strength must be a finite value within \
             -{VIGNETTE_STRENGTH_MAX}..={VIGNETTE_STRENGTH_MAX}, got {strength}"
        )));
    }
    if !radius.is_finite() || !(0.0..=VIGNETTE_RADIUS_MAX).contains(&radius) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (vignette): radius must be a finite value within \
             0.0..={VIGNETTE_RADIUS_MAX}, got {radius}"
        )));
    }
    if !feather.is_finite() || !(0.0..=VIGNETTE_FEATHER_MAX).contains(&feather) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (vignette): feather must be a finite value within \
             0.0..={VIGNETTE_FEATHER_MAX}, got {feather}"
        )));
    }
    Ok(())
}

/// grain の静的検証: amount は有限かつ 0..=1、size は 1..=4。
///
/// `amount == 0`(恒等)は vignette の `strength == 0` と同じ理由で許可する。
pub fn validate_grain(index: usize, amount: f64, size: u32) -> Result<()> {
    if !amount.is_finite() || !(0.0..=GRAIN_AMOUNT_MAX).contains(&amount) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (grain): amount must be a finite value within \
             0.0..={GRAIN_AMOUNT_MAX}, got {amount}"
        )));
    }
    if !(GRAIN_SIZE_MIN..=GRAIN_SIZE_MAX).contains(&size) {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (grain): size must be within \
             {GRAIN_SIZE_MIN}..={GRAIN_SIZE_MAX}, got {size}"
        )));
    }
    Ok(())
}

// --------------------------------------------------------------------- flip

/// 反転は純粋な画素置換なので空間非依存。
///
/// - `Horizontal`: `out(x, y) = in(w - 1 - x, y)`
/// - `Vertical`: `out(x, y) = in(x, h - 1 - y)`
///
/// 画素値は 1 ビットも変換しないので、同じ方向に 2 回適用すると厳密な恒等になる。
pub fn flip(img: &LinearImage, direction: FlipDirection) -> LinearImage {
    let (width, height) = img.dimensions();
    let w = width as usize;
    let h = height as usize;
    let mut out = img.clone();
    if w == 0 || h == 0 {
        return out;
    }
    let src = &img.data;
    parallel::fill_rows(&mut out.data, w, h, |y, row| {
        let src_y = match direction {
            FlipDirection::Horizontal => y,
            FlipDirection::Vertical => h - 1 - y,
        };
        let base = src_y * w;
        for (x, px) in row.iter_mut().enumerate() {
            let src_x = match direction {
                FlipDirection::Horizontal => w - 1 - x,
                FlipDirection::Vertical => x,
            };
            *px = src[base + src_x];
        }
    });
    out
}

// ----------------------------------------------------------------- vignette

/// 減衰帯のゲイン(モジュールドキュメントの式。f64 → 1e-6 量子化)。
fn vignette_gain(d: f64, strength: f64, radius: f64, feather: f64) -> f32 {
    let gain = if d <= radius {
        1.0
    } else if feather <= 0.0 || d >= radius + feather {
        1.0 - strength
    } else {
        let t = (d - radius) / feather;
        // smoothstep(3次エルミート)。演算順序は固定(再結合しない)。
        let s = t * t * (3.0 - 2.0 * t);
        1.0 - strength * s
    };
    quantize_1e6(gain) as f32
}

/// リニア光で適用。
///
/// 各画素の RGB に中心距離由来のゲインを掛ける(アルファは不変)。
/// `strength == 0.0` は厳密な恒等なので、入力をそのまま複製して返す
/// (量子化 → f32 化の経路すら通らないため、バイト同一が保証される)。
///
/// 曲線・座標系・クランプ方針の詳細はモジュールドキュメントを参照。
pub fn vignette(img: &LinearImage, strength: f64, radius: f64, feather: f64) -> LinearImage {
    let (width, height) = img.dimensions();
    let w = width as usize;
    let h = height as usize;
    let mut out = img.clone();
    if strength == 0.0 || w == 0 || h == 0 {
        return out;
    }

    // 中心からのオフセットの平方を列/行ごとに f64 で先に持つ(画素ごとの再計算回避)。
    let cx = width as f64 / 2.0;
    let cy = height as f64 / 2.0;
    let dx2: Vec<f64> = (0..w)
        .map(|x| {
            let dx = (x as f64 + 0.5) - cx;
            dx * dx
        })
        .collect();
    let dy2: Vec<f64> = (0..h)
        .map(|y| {
            let dy = (y as f64 + 0.5) - cy;
            dy * dy
        })
        .collect();
    // 半対角(角で d ≈ 1 になる正規化)。
    let half_diag = {
        let ww = width as f64;
        let hh = height as f64;
        (ww * ww + hh * hh).sqrt() / 2.0
    };

    let dx2 = &dx2;
    let dy2 = &dy2;
    parallel::fill_rows(&mut out.data, w, h, move |y, row| {
        let ry = dy2[y];
        for (x, px) in row.iter_mut().enumerate() {
            let d = (dx2[x] + ry).sqrt() / half_diag;
            let gain = vignette_gain(d, strength, radius, feather);
            // 固定順序の f32 乗算。アルファ(px[3])には触れない。
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
    out
}

// -------------------------------------------------------------------- grain

/// splitmix64 の最終ミックス(整数演算のみ)。
#[inline]
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// ブロック座標 + seed + チャンネルの位置ハッシュ。
///
/// 各軸を奇数の大きな定数で撹拌してから splitmix64 に通すのを段重ねする。
/// 隣接ブロック(bx が 1 違い)でも出力が完全に無相関になり、
/// 「縞」や「格子」といった構造的アーティファクトが出ない。
#[inline]
fn grain_hash(block_x: u64, block_y: u64, seed: u64, channel: u64) -> u64 {
    let mut h = splitmix64(seed ^ 0xA076_1D64_78BD_642F);
    h = splitmix64(h ^ block_x.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    h = splitmix64(h ^ block_y.wrapping_mul(0xE703_7ED1_A0B4_28DB));
    splitmix64(h ^ channel.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// 位置ハッシュから sRGB 符号値へ足す差分を作る(-amount*SCALE ..= +amount*SCALE)。
#[inline]
fn grain_delta(block_x: u64, block_y: u64, seed: u64, channel: u64, amount: f64) -> f32 {
    let h = grain_hash(block_x, block_y, seed, channel);
    // 上位 32bit を使う(splitmix64 の下位ビットも良質だが、上位の方が保守的)。
    let u = ((h >> 32) as u32) as f64 / U32_SPAN;
    let noise = u * 2.0 - 1.0;
    quantize_1e6(noise * amount * GRAIN_SCALE) as f32
}

/// sRGB f32 空間で適用(エンジン側で空間変換済みの前提)。
///
/// `amount == 0.0` は厳密な恒等なので入力をそのまま複製して返す(バイト同一)。
/// それ以外は各画素の RGB に位置ハッシュ由来の差分を足して 0..1 へクランプする
/// (アルファは不変)。ノイズの定義はモジュールドキュメントを参照。
pub fn grain(
    img: &LinearImage,
    amount: f64,
    size: u32,
    monochrome: bool,
    seed: u64,
) -> LinearImage {
    let (width, height) = img.dimensions();
    let w = width as usize;
    let h = height as usize;
    let mut out = img.clone();
    if amount == 0.0 || w == 0 || h == 0 {
        return out;
    }
    // validate 済み(1..=4)だが、0 除算を避けるため下限を固定する。
    let size = u64::from(size.max(GRAIN_SIZE_MIN));

    parallel::fill_rows(&mut out.data, w, h, move |y, row| {
        let block_y = y as u64 / size;
        for (x, px) in row.iter_mut().enumerate() {
            let block_x = x as u64 / size;
            // アルファ(px[3])は対象外なので RGB の 3 要素だけを見る。
            let mono_delta = if monochrome {
                Some(grain_delta(block_x, block_y, seed, 0, amount))
            } else {
                None
            };
            for (c, v) in px[..3].iter_mut().enumerate() {
                let delta = match mono_delta {
                    Some(d) => d,
                    None => grain_delta(block_x, block_y, seed, c as u64, amount),
                };
                *v = (*v + delta).clamp(0.0, 1.0);
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_is_flat_inside_radius_and_saturates_outside() {
        assert_eq!(vignette_gain(0.0, 0.5, 0.6, 0.3), 1.0);
        assert_eq!(vignette_gain(0.6, 0.5, 0.6, 0.3), 1.0);
        assert_eq!(vignette_gain(0.9, 0.5, 0.6, 0.3), 0.5);
        assert_eq!(vignette_gain(1.5, 0.5, 0.6, 0.3), 0.5);
    }

    /// 減衰帯の中点は smoothstep なので `s = 0.5` = ゲインの中点。
    #[test]
    fn gain_midpoint_of_falloff_is_the_smoothstep_midpoint() {
        let g = vignette_gain(0.75, 0.5, 0.6, 0.3);
        assert!((g - 0.75).abs() < 1e-6, "midpoint gain {g}");
    }

    /// feather = 0 は radius で階段状に切り替わる。
    #[test]
    fn zero_feather_is_a_step() {
        assert_eq!(vignette_gain(0.5, 0.4, 0.5, 0.0), 1.0);
        assert_eq!(vignette_gain(0.5001, 0.4, 0.5, 0.0), 0.6);
    }

    /// 負の strength は gain > 1(増光)。
    #[test]
    fn negative_strength_brightens() {
        assert_eq!(vignette_gain(1.0, -0.5, 0.5, 0.2), 1.5);
    }

    /// ノイズは -amount*SCALE ..= +amount*SCALE に収まる。
    #[test]
    fn grain_delta_stays_within_amplitude() {
        let limit = GRAIN_SCALE as f32 + 1e-6;
        for y in 0..64u64 {
            for x in 0..64u64 {
                let d = grain_delta(x, y, 7, 0, 1.0);
                assert!(d.abs() <= limit, "delta {d} out of range at ({x},{y})");
            }
        }
    }

    /// 位置ハッシュは座標・seed・チャンネルのいずれが変わっても値が変わる。
    #[test]
    fn grain_hash_separates_inputs() {
        let base = grain_hash(3, 4, 5, 0);
        assert_ne!(base, grain_hash(4, 4, 5, 0));
        assert_ne!(base, grain_hash(3, 5, 5, 0));
        assert_ne!(base, grain_hash(3, 4, 6, 0));
        assert_ne!(base, grain_hash(3, 4, 5, 1));
    }
}

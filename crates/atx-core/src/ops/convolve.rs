//! 任意カーネル畳み込み(size×size、size ∈ {3,5,7,9})。
//!
//! **作業空間: 線形光**(`ops/mod.rs` の表を参照)。畳み込みは加重平均なので
//! 光量に対して行う。アルファはプリマルチプライしてから畳み込み、後で解く
//! (RGB のみ対象なのでアルファ自体は不変)。
//!
//! - 出力 = clamp((Σ kernel_i * px_i) / divisor + offset/255)
//! - RGB のみ対象、A は不変(ぼかし系と違い、輪郭・エンボス等でアルファを歪めない)
//! - 端はクランプ(複製)。f32 固定順序累算 → 0..1 クランプ
//! - カーネル値はレシピ由来(canonical 量子化済み)なので追加量子化不要
//!
//! # `offset` のスケール(v2 の解釈)
//!
//! レシピ上の `offset` は v1 から変わらず -255..=255(u8 符号値のバイアス)。
//! v2 の画素は 0..1 の線形光なので **`offset / 255` を線形光に足す**。
//! エンボス系カーネルで使う「中間グレーへ寄せる」用途では、v1 の `offset: 128` は
//! 線形 0.502(= sRGB 符号値では約 188)になる点に注意
//! (符号値 128 相当に寄せたいなら `offset: 55` 前後を指定する)。

use crate::linear::LinearImage;
use crate::parallel;
use crate::{AtxError, Result};

/// カーネルの値そのものの絶対値上限。極端な値による overflow/オーバーフロー的な
/// 破綻を防ぐための実務的な上限。
const MAX_KERNEL_ABS: f64 = 256.0;

/// `offset`(u8 符号値スケール)を 0..1 スケールへ写す係数。
const OFFSET_SCALE: f32 = 1.0 / 255.0;

pub fn validate(index: usize, kernel: &[f64], size: u32, divisor: f64, offset: f64) -> Result<()> {
    let fail = |message: String| {
        AtxError::InvalidRecipe(format!("operations[{index}] (convolve): {message}"))
    };

    if !matches!(size, 3 | 5 | 7 | 9) {
        return Err(fail(format!("size must be one of 3, 5, 7, 9, got {size}")));
    }

    let expected_len = (size as usize) * (size as usize);
    if kernel.len() != expected_len {
        return Err(fail(format!(
            "kernel.len() must equal size*size = {expected_len}, got {}",
            kernel.len()
        )));
    }

    for (i, &v) in kernel.iter().enumerate() {
        if !v.is_finite() {
            return Err(fail(format!("kernel[{i}] must be finite, got {v}")));
        }
        if v.abs() > MAX_KERNEL_ABS {
            return Err(fail(format!(
                "kernel[{i}] must satisfy |v| <= {MAX_KERNEL_ABS}, got {v}"
            )));
        }
    }

    if !divisor.is_finite() {
        return Err(fail(format!("divisor must be finite, got {divisor}")));
    }
    if divisor.abs() < 1e-6 {
        return Err(fail(format!(
            "divisor must satisfy |divisor| >= 1e-6, got {divisor}"
        )));
    }

    if !offset.is_finite() || !(-255.0..=255.0).contains(&offset) {
        return Err(fail(format!(
            "offset must be finite and within -255.0..=255.0, got {offset}"
        )));
    }

    Ok(())
}

/// 任意カーネルの畳み込みを適用する。RGB のみ対象、A は不変。
///
/// 1画素あたりの累算順序は kernel の添字昇順(行優先: 上から下、各行は左から右)で
/// 固定する。行単位でスレッド分割して並列化するが、画素間の実行順序は
/// 決定論の対象外(mod.rs 参照)。
pub fn apply(
    img: &LinearImage,
    kernel: &[f64],
    size: u32,
    divisor: f64,
    offset: f64,
) -> LinearImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let (wu, hu) = (w as usize, h as usize);
    let r = (size / 2) as i64;
    let kernel_f32: Vec<f32> = kernel.iter().map(|v| *v as f32).collect();
    let divisor = divisor as f32;
    let offset = offset as f32 * OFFSET_SCALE;
    let premul = img.premultiplied();

    let mut out_data = vec![[0f32; 4]; wu * hu];
    parallel::fill_rows(&mut out_data, wu, hu, |y, row| {
        for (x, slot) in row.iter_mut().enumerate() {
            let mut acc = [0f32; 3];
            let mut k = 0usize;
            for ky in -r..=r {
                for kx in -r..=r {
                    let px = premul.get_clamped(x as i64 + kx, y as i64 + ky);
                    let weight = kernel_f32[k];
                    // 乗算 → 加算 を分離(mul_add 禁止)。
                    for (c, a) in acc.iter_mut().enumerate() {
                        let term = weight * px[c];
                        *a += term;
                    }
                    k += 1;
                }
            }
            let src = img.get(x as u32, y as u32);
            let alpha = src[3];
            let mut px = [0f32; 4];
            for c in 0..3 {
                // プリマルチプライ空間の結果をストレートへ戻す。
                let value = acc[c] / divisor;
                let straight = if alpha < crate::linear::ALPHA_EPSILON {
                    0.0
                } else {
                    value / alpha
                };
                px[c] = (straight + offset).clamp(0.0, 1.0);
            }
            px[3] = alpha;
            *slot = px;
        }
    });

    LinearImage {
        width: w,
        height: h,
        data: out_data,
    }
}

//! 任意カーネル畳み込み(size×size、size ∈ {3,5,7,9})。
//!
//! - 出力 = clamp((Σ kernel_i * px_i) / divisor + offset)
//! - RGB のみ対象、A は不変(ぼかし系と違い、輪郭・エンボス等でアルファを歪めない)
//! - 端はクランプ(複製)。f64 固定順序累算 → half-away-from-zero 丸め
//! - カーネル値はレシピ由来(canonical 量子化済み)なので追加量子化不要

use image::RgbaImage;

use crate::{AtxError, Result};

/// カーネルの値そのものの絶対値上限。極端な値による overflow/オーバーフロー的な
/// 破綻を防ぐための実務的な上限(v0.3 で新規追加する制約)。
const MAX_KERNEL_ABS: f64 = 256.0;

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

/// half-away-from-zero で f64 を u8(0..=255)へ丸める。
fn round_to_u8(v: f64) -> u8 {
    let r = if v >= 0.0 {
        (v + 0.5).floor()
    } else {
        (v - 0.5).ceil()
    };
    r.clamp(0.0, 255.0) as u8
}

/// 座標を [0, len) へクランプ(端の複製、= replicate border)する。
fn clamp_coord(v: i64, len: u32) -> u32 {
    v.clamp(0, len as i64 - 1) as u32
}

/// 行の範囲を CPU コア数に応じたチャンクへ分割する(blur.rs と同じ idiom)。
///
/// **決定論への影響なし**: 行は互いに独立(1画素の f64 累算順序はカーネルの
/// 添字昇順のまま、他画素の計算順序と無関係)なので、スレッド分割・実行順は
/// 出力バイト列に一切影響しない。性能のためだけの並列化。
fn parallel_row_chunks(total_rows: u32) -> Vec<std::ops::Range<u32>> {
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(total_rows.max(1) as usize)
        .max(1);
    let chunk = total_rows.div_ceil(n_threads as u32).max(1);
    let mut ranges = Vec::new();
    let mut start = 0u32;
    while start < total_rows {
        let end = (start + chunk).min(total_rows);
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// 任意カーネルの畳み込みを適用する。RGB のみ対象、A は不変。
///
/// 1画素あたりの累算順序は kernel の添字昇順(行優先: 上から下、各行は左から右)で
/// 固定する。行単位でスレッド分割して並列化するが、画素間の実行順序は
/// 決定論の対象外(mod.rs 参照)。
pub fn apply(img: &RgbaImage, kernel: &[f64], size: u32, divisor: f64, offset: f64) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let (wu, hu) = (w as usize, h as usize);
    let src: &[u8] = img.as_raw();
    let r = (size / 2) as i32;

    let mut out_raw = vec![0u8; wu * hu * 4];
    std::thread::scope(|scope| {
        let mut remaining = out_raw.as_mut_slice();
        for range in parallel_row_chunks(h) {
            let rows_in_chunk = (range.end - range.start) as usize;
            let (chunk, rest) = remaining.split_at_mut(rows_in_chunk * wu * 4);
            remaining = rest;
            let kernel = &kernel;
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(wu * 4).enumerate() {
                    let y = range.start + i as u32;
                    for x in 0..w {
                        let mut acc = [0f64; 3];
                        let mut k = 0usize;
                        for ky in -r..=r {
                            let sy = clamp_coord(y as i64 + ky as i64, h);
                            for kx in -r..=r {
                                let sx = clamp_coord(x as i64 + kx as i64, w);
                                let src_idx = (sy as usize * wu + sx as usize) * 4;
                                let weight = kernel[k];
                                acc[0] += weight * src[src_idx] as f64;
                                acc[1] += weight * src[src_idx + 1] as f64;
                                acc[2] += weight * src[src_idx + 2] as f64;
                                k += 1;
                            }
                        }
                        let src_idx = (y as usize * wu + x as usize) * 4;
                        let out_idx = (x as usize) * 4;
                        row[out_idx] = round_to_u8(acc[0] / divisor + offset);
                        row[out_idx + 1] = round_to_u8(acc[1] / divisor + offset);
                        row[out_idx + 2] = round_to_u8(acc[2] / divisor + offset);
                        row[out_idx + 3] = src[src_idx + 3];
                    }
                }
            });
        }
    });

    RgbaImage::from_raw(w, h, out_raw).expect("buffer sized w*h*4 matches RgbaImage layout")
}

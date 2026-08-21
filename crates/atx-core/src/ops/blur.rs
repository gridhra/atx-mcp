//! blur / median / unsharp_mask(カーネル系)。
//!
//! 実装規約:
//! - ガウスカーネルは f64 で生成後、重みを 1e-6 グリッドに量子化し正規化してから適用
//!   (exp() の libm 差を遮断。mod.rs の決定論規約参照)
//! - median は整数演算のみ(決定論は自明)
//! - unsharp_mask は blur と同じ量子化カーネルを用い、ブレンドは f64 → 明示丸め

use image::RgbaImage;

use crate::{AtxError, Result};

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
///
/// 半径(半値幅)は `ceil(3*sigma)` を 255 に上限キャップする。
fn gaussian_kernel(sigma: f64) -> (i32, Vec<f64>) {
    let radius = ((3.0 * sigma).ceil() as i64).clamp(1, 255) as i32;
    let two_sigma_sq = 2.0 * sigma * sigma;

    let mut weights: Vec<f64> = (-radius..=radius)
        .map(|i| {
            let raw = (-((i * i) as f64) / two_sigma_sq).exp();
            quantize_weight(raw)
        })
        .collect();

    // 量子化後の重みの合計で正規化する(この順序が決定論の要)。
    let sum: f64 = weights.iter().fold(0.0f64, |acc, w| acc + w);
    for w in weights.iter_mut() {
        *w /= sum;
    }
    (radius, weights)
}

/// f64 の重みを 1e-6 グリッドへ量子化する。
fn quantize_weight(w: f64) -> f64 {
    (w * 1e6).round() / 1e6
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

/// x 座標を [0, len) へクランプ(端の複製、= replicate border)する。
fn clamp_coord(v: i64, len: u32) -> u32 {
    v.clamp(0, len as i64 - 1) as u32
}

/// 行の範囲を CPU コア数に応じたチャンクへ分割し、各チャンクを別スレッドで実行する。
///
/// **決定論への影響なし**: 行(または列。パス2は行方向にチャンク分割する点は同じ)は
/// 互いに独立(1画素の f64 累算順序は `weights` の添字昇順のまま、他画素の計算順序と
/// 無関係)なので、スレッド分割・実行順は出力バイト列に一切影響しない。
/// 性能のためだけの並列化(mod.rs の決定論規約における「reassociation」とは
/// 1画素内の累算順序の話であり、画素間の実行順序はそもそも規約の対象外)。
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

/// ガウスぼかし。分離可能な1次元カーネルを横→縦の順で適用する(2パス)。
/// 端はクランプ(複製)。アルファチャンネルも含め全4チャンネルにぼかしをかける。
///
/// 行(パス1)・列(パス2、実装上は転置せず行方向チャンクのまま)単位で
/// スレッド分割して計算する。画素ごとの f64 累算順序(決定論の要)は
/// 分割の有無に関わらず `weights` の添字昇順で固定される。
pub fn gaussian_blur(img: &RgbaImage, sigma: f64) -> RgbaImage {
    let (radius, weights) = gaussian_kernel(sigma);
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let (wu, hu) = (w as usize, h as usize);
    let src: &[u8] = img.as_raw();

    // --- パス1: 水平方向 ---
    let mut horiz = vec![0f64; wu * hu * 4];
    std::thread::scope(|scope| {
        let mut remaining = horiz.as_mut_slice();
        for range in parallel_row_chunks(h) {
            let rows_in_chunk = (range.end - range.start) as usize;
            let (chunk, rest) = remaining.split_at_mut(rows_in_chunk * wu * 4);
            remaining = rest;
            let weights = &weights;
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(wu * 4).enumerate() {
                    let y = range.start + i as u32;
                    for x in 0..w {
                        let mut acc = [0f64; 4];
                        for (k, &weight) in weights.iter().enumerate() {
                            let dx = k as i32 - radius;
                            let sx = clamp_coord(x as i64 + dx as i64, w);
                            let src_idx = ((y as usize) * wu + sx as usize) * 4;
                            for c in 0..4 {
                                acc[c] += (src[src_idx + c] as f64) * weight;
                            }
                        }
                        let idx = (x as usize) * 4;
                        row[idx..idx + 4].copy_from_slice(&acc);
                    }
                }
            });
        }
    });

    // --- パス2: 垂直方向 ---
    let mut out_raw = vec![0u8; wu * hu * 4];
    let horiz_ref: &[f64] = &horiz;
    std::thread::scope(|scope| {
        let mut remaining = out_raw.as_mut_slice();
        for range in parallel_row_chunks(h) {
            let rows_in_chunk = (range.end - range.start) as usize;
            let (chunk, rest) = remaining.split_at_mut(rows_in_chunk * wu * 4);
            remaining = rest;
            let weights = &weights;
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(wu * 4).enumerate() {
                    let y = range.start + i as u32;
                    for x in 0..w {
                        let mut acc = [0f64; 4];
                        for (k, &weight) in weights.iter().enumerate() {
                            let dy = k as i32 - radius;
                            let sy = clamp_coord(y as i64 + dy as i64, h);
                            let idx = (sy as usize * wu + x as usize) * 4;
                            for c in 0..4 {
                                acc[c] += horiz_ref[idx + c] * weight;
                            }
                        }
                        let out_idx = (x as usize) * 4;
                        row[out_idx] = round_to_u8(acc[0]);
                        row[out_idx + 1] = round_to_u8(acc[1]);
                        row[out_idx + 2] = round_to_u8(acc[2]);
                        row[out_idx + 3] = round_to_u8(acc[3]);
                    }
                }
            });
        }
    });

    RgbaImage::from_raw(w, h, out_raw).expect("buffer sized w*h*4 matches RgbaImage layout")
}

/// メディアンフィルタ。(2r+1)^2 の正方形ウィンドウ、チャンネル毎にカウントソートで
/// 中央値を取る(u8 のヒストグラム、256 バケツの整数演算のみ)。端はクランプ。
pub fn median(img: &RgbaImage, radius: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let r = radius as i64;
    let window_len = (2 * radius + 1) as usize * (2 * radius + 1) as usize;
    let median_rank = window_len / 2; // 0-indexed、奇数個なので中央要素

    let mut out = RgbaImage::new(w, h);
    let mut hist = [[0u32; 256]; 4];

    for y in 0..h {
        for x in 0..w {
            for channel_hist in hist.iter_mut() {
                *channel_hist = [0u32; 256];
            }
            for dy in -r..=r {
                let sy = clamp_coord(y as i64 + dy, h);
                for dx in -r..=r {
                    let sx = clamp_coord(x as i64 + dx, w);
                    let px = img.get_pixel(sx, sy).0;
                    for c in 0..4 {
                        hist[c][px[c] as usize] += 1;
                    }
                }
            }
            let mut px = [0u8; 4];
            for c in 0..4 {
                let mut remaining = median_rank as i64;
                let mut value = 0u8;
                for (bucket, &count) in hist[c].iter().enumerate() {
                    remaining -= count as i64;
                    if remaining < 0 {
                        value = bucket as u8;
                        break;
                    }
                }
                px[c] = value;
            }
            out.put_pixel(x, y, image::Rgba(px));
        }
    }

    out
}

/// アンシャープマスク。`blurred = gaussian_blur(img, radius)` を基準に、
/// チャンネル毎の絶対差が `threshold` 以下ならそのまま(保護)、超えていれば
/// `orig + amount * (orig - blurred)` を f64 で計算し、クランプ・half-away-from-zero
/// 丸めで u8 化する。
///
/// `threshold` はチャンネル毎の絶対差で判定する(輝度ベースではない)。
/// より単純で決定論を保ちやすいための意図的な簡略化。
pub fn unsharp_mask(img: &RgbaImage, amount: f64, radius: f64, threshold: u8) -> RgbaImage {
    let blurred = gaussian_blur(img, radius);
    let (w, h) = img.dimensions();
    let mut out = RgbaImage::new(w, h);
    let threshold = threshold as i32;

    for y in 0..h {
        for x in 0..w {
            let orig = img.get_pixel(x, y).0;
            let blur = blurred.get_pixel(x, y).0;
            let mut px = [0u8; 4];
            for c in 0..4 {
                let diff = orig[c] as i32 - blur[c] as i32;
                px[c] = if diff.abs() <= threshold {
                    orig[c]
                } else {
                    let value = orig[c] as f64 + amount * (diff as f64);
                    round_to_u8(value)
                };
            }
            out.put_pixel(x, y, image::Rgba(px));
        }
    }

    out
}

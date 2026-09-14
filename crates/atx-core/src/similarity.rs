//! 画像の**類似度**を数値化する: 知覚ハッシュ(dHash)と SSIM。
//!
//! 用途は「同じ絵か」「どれだけ壊れたか」をエージェントが判断できるようにすること。
//! `inspect_image` が dHash を、`compare_revisions` がハッシュ距離と SSIM を返す。
//!
//! # 決定論の規約(`ops::mod` 冒頭と同じ)
//!
//! - 輝度化・面積平均・積分画像はすべて整数演算(`crate::stats::luma_u8` を共有)。
//!   dHash は f64 を 1 度も通らないので、どのプラットフォームでもビット同一。
//! - SSIM は最後の窓ごとの割り算だけ f64。`mul_add` を使わず、総和は走査順の左結合。
//!   窓の分散・共分散は整数(i128)で厳密に作ってから f64 にするので、
//!   「平方和 − 平均の平方」の桁落ちも起こらない。
//! - 縮小は**整数ボックス平均**(`crate::stats::downscale_gray` と同一手順)。
//!   k×k ブロックの平均を整数で取るだけなので、補間フィルタの実装差は入らない。

use image::GrayImage;

use crate::stats::{downscale_gray, gray_from_interleaved, luma_u8};

/// dHash のグリッド: 9 列 × 8 行の面積平均を取り、行ごとに隣接列を比べて 64 bit にする。
const DHASH_COLS: u32 = 9;
const DHASH_ROWS: u32 = 8;

/// SSIM の一様窓の辺(px)。画像がこれより小さい辺を持つ場合はその辺に合わせて縮める。
const SSIM_WINDOW: u32 = 8;

/// SSIM 計算前に縮小する長辺のしきい値(px)。
const SSIM_MAX_LONG_EDGE: u32 = 2048;

/// 知覚ハッシュ(dHash)を RGBA8 バッファから計算する。
///
/// 手順: BT.709 輝度 → 9×8 セルへ**面積平均**(各セル内の画素を u64 で総和して
/// 画素数で割る。整数の切り捨て)→ 各行で左右のセルを比べ、`left < right` なら 1。
///
/// ビットの並びは「行 0 の左端が最上位ビット」。`ビット添字 = 行*8 + 列`、
/// 実際のビット位置は `63 - ビット添字` なので、16 進表記を左から読むと
/// 上の行から順に並ぶ。
///
/// `width * height * 4 == rgba.len()` を満たさない、または寸法 0 の場合は 0 を返す。
pub fn dhash_rgba8(rgba: &[u8], width: u32, height: u32) -> u64 {
    dhash_interleaved(rgba, 4, width, height)
}

/// 知覚ハッシュ(dHash)を RGB8 バッファから計算する。同じ画素なら
/// [`dhash_rgba8`] と完全に同じ値を返す(アルファは輝度に使わないため)。
pub fn dhash_rgb8(rgb: &[u8], width: u32, height: u32) -> u64 {
    dhash_interleaved(rgb, 3, width, height)
}

fn dhash_interleaved(buf: &[u8], channels: usize, width: u32, height: u32) -> u64 {
    let expected = u64::from(width) * u64::from(height) * channels as u64;
    if width == 0 || height == 0 || buf.len() as u64 != expected {
        return 0;
    }

    // セル平均(9 列 × 8 行)。セル境界は整数分割 `w*i/9` なので、
    // 画素は重複なく・漏れなくどこか 1 セルに属する(w >= 9 のとき)。
    // w < 9(または h < 8)の場合は同じ画素列を複数セルが共有する。
    let mut cell = [[0u64; DHASH_COLS as usize]; DHASH_ROWS as usize];
    for row in 0..DHASH_ROWS {
        let (y0, y1) = cell_bounds(height, row, DHASH_ROWS);
        for col in 0..DHASH_COLS {
            let (x0, x1) = cell_bounds(width, col, DHASH_COLS);
            let mut sum = 0u64;
            let mut count = 0u64;
            for y in y0..y1 {
                let row_start = (u64::from(y) * u64::from(width)) as usize * channels;
                for x in x0..x1 {
                    let i = row_start + x as usize * channels;
                    sum += u64::from(luma_u8(buf[i], buf[i + 1], buf[i + 2]));
                    count += 1;
                }
            }
            cell[row as usize][col as usize] = sum.checked_div(count).unwrap_or(0);
        }
    }

    let mut hash = 0u64;
    for (row, cells) in cell.iter().enumerate() {
        for col in 0..(DHASH_COLS as usize - 1) {
            if cells[col] < cells[col + 1] {
                let index = row * (DHASH_COLS as usize - 1) + col;
                hash |= 1u64 << (63 - index);
            }
        }
    }
    hash
}

/// `extent` を `divisions` 等分したときの `index` 番目の半開区間 `[start, end)`。
///
/// 整数分割 `extent*index/divisions` を使うので、区間は重複せず全体を覆う。
/// `extent < divisions` のときは空区間になりうるので、最低 1 画素になるよう補正する。
fn cell_bounds(extent: u32, index: u32, divisions: u32) -> (u32, u32) {
    let start = (u64::from(extent) * u64::from(index) / u64::from(divisions)) as u32;
    let end = (u64::from(extent) * u64::from(index + 1) / u64::from(divisions)) as u32;
    let start = start.min(extent - 1);
    (start, end.max(start + 1).min(extent))
}

/// dHash を 16 桁小文字の 16 進文字列にする(先頭が最上位ビット)。
pub fn dhash_hex(hash: u64) -> String {
    format!("{hash:016x}")
}

/// 2 つの dHash のハミング距離(0..=64)。
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 2 枚のグレースケール画像の SSIM(0 に近いほど非類似、1 が完全一致)。
///
/// - **寸法が一致しない場合は `None`**(SSIM は画素対応を前提とする指標なので、
///   寸法違いは「低い類似度」ではなく「測れない」として返す)。
/// - 長辺が 2048 を超える場合は、両方を同じ比率で整数ボックス平均で縮小してから測る
///   (計算量の上限を決めるため。両方に同じ縮小を掛けるので対称性は保たれる)。
/// - 窓は 8×8 の一様窓(ガウシアン窓ではない)。stride 1 で全位置を走らせ、
///   全窓の SSIM の**算術平均**を 1e-4 グリッドへ量子化して返す。
///   画像の辺が 8 未満のときは窓をその辺に合わせて縮める。
/// - 定数は標準の K1=0.01、K2=0.03、L=255。
pub fn ssim_gray(a: &GrayImage, b: &GrayImage) -> Option<f64> {
    if a.dimensions() != b.dimensions() {
        return None;
    }
    let (w0, h0) = a.dimensions();
    if w0 == 0 || h0 == 0 {
        return None;
    }

    let a = downscale_gray(a, SSIM_MAX_LONG_EDGE);
    let b = downscale_gray(b, SSIM_MAX_LONG_EDGE);
    let (w, h) = a.dimensions();
    debug_assert_eq!(a.dimensions(), b.dimensions());

    let win_w = SSIM_WINDOW.min(w);
    let win_h = SSIM_WINDOW.min(h);
    let n = u64::from(win_w) * u64::from(win_h);

    let ia = Integrals::new(&a);
    let ib = Integrals::new(&b);
    let cross = cross_integral(&a, &b);
    let cw = w as usize + 1;

    // C1 = (K1*L)^2 = (0.01*255)^2、C2 = (K2*L)^2 = (0.03*255)^2。
    // どちらも有限桁の 10 進なので f64 で厳密に表せる範囲の定数リテラルで書く。
    const C1: f64 = 6.5025;
    const C2: f64 = 58.5225;

    let n_f = n as f64;
    let n_sq = n_f * n_f;
    let mut total = 0.0f64;
    let mut windows = 0u64;

    for y in 0..=(h - win_h) {
        for x in 0..=(w - win_w) {
            let sum_a = ia.sum.rect(cw, x, y, win_w, win_h);
            let sum_b = ib.sum.rect(cw, x, y, win_w, win_h);
            let sq_a = ia.sq.rect(cw, x, y, win_w, win_h);
            let sq_b = ib.sq.rect(cw, x, y, win_w, win_h);
            let sum_ab = cross.rect(cw, x, y, win_w, win_h);

            let mu_a = sum_a as f64 / n_f;
            let mu_b = sum_b as f64 / n_f;

            // 分散 * n^2 と共分散 * n^2 を整数で厳密に作る(桁落ちなし)。
            let var_a_num = i128::from(n) * sq_a as i128 - (sum_a as i128) * (sum_a as i128);
            let var_b_num = i128::from(n) * sq_b as i128 - (sum_b as i128) * (sum_b as i128);
            let cov_num = i128::from(n) * sum_ab as i128 - (sum_a as i128) * (sum_b as i128);
            let var_a = var_a_num as f64 / n_sq;
            let var_b = var_b_num as f64 / n_sq;
            let cov = cov_num as f64 / n_sq;

            // mul_add は使わない(FMA の丸めがアーキテクチャ差を生むため)。
            let luminance_num = 2.0 * (mu_a * mu_b) + C1;
            let contrast_num = 2.0 * cov + C2;
            let luminance_den = mu_a * mu_a + mu_b * mu_b + C1;
            let contrast_den = var_a + var_b + C2;
            total += (luminance_num * contrast_num) / (luminance_den * contrast_den);
            windows += 1;
        }
    }

    if windows == 0 {
        return None;
    }
    Some(quantize_1e4(total / windows as f64))
}

/// 1e-4 グリッドへ half-away-from-zero 量子化する。
fn quantize_1e4(v: f64) -> f64 {
    let scaled = v * 10_000.0;
    let r = if scaled >= 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    r / 10_000.0
}

/// 積分画像(サイズ `(w+1)*(h+1)`、先頭行・先頭列は 0)。
struct Integral<T>(Vec<T>);

impl Integral<u64> {
    /// 矩形 `[x, x+rw) × [y, y+rh)` の総和。
    #[inline]
    fn rect(&self, cw: usize, x: u32, y: u32, rw: u32, rh: u32) -> u64 {
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = (x0 + rw as usize, y0 + rh as usize);
        let t = &self.0;
        t[y1 * cw + x1] + t[y0 * cw + x0] - t[y0 * cw + x1] - t[y1 * cw + x0]
    }
}

impl Integral<u128> {
    #[inline]
    fn rect(&self, cw: usize, x: u32, y: u32, rw: u32, rh: u32) -> u128 {
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = (x0 + rw as usize, y0 + rh as usize);
        let t = &self.0;
        t[y1 * cw + x1] + t[y0 * cw + x0] - t[y0 * cw + x1] - t[y1 * cw + x0]
    }
}

/// 1 枚の画像の「値の総和」と「平方和」の積分画像。
struct Integrals {
    sum: Integral<u64>,
    sq: Integral<u128>,
}

impl Integrals {
    fn new(img: &GrayImage) -> Self {
        let (w, h) = img.dimensions();
        let cw = w as usize + 1;
        let ch = h as usize + 1;
        let mut sum = vec![0u64; cw * ch];
        let mut sq = vec![0u128; cw * ch];
        let buf = img.as_raw();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let v = u64::from(buf[y * w as usize + x]);
                let i = (y + 1) * cw + (x + 1);
                sum[i] = v + sum[i - 1] + sum[i - cw] - sum[i - cw - 1];
                let v2 = u128::from(v) * u128::from(v);
                sq[i] = v2 + sq[i - 1] + sq[i - cw] - sq[i - cw - 1];
            }
        }
        Self {
            sum: Integral(sum),
            sq: Integral(sq),
        }
    }
}

/// 2 枚の画素積 `a*b` の積分画像。
fn cross_integral(a: &GrayImage, b: &GrayImage) -> Integral<u128> {
    let (w, h) = a.dimensions();
    let cw = w as usize + 1;
    let ch = h as usize + 1;
    let mut t = vec![0u128; cw * ch];
    let (pa, pb) = (a.as_raw(), b.as_raw());
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = u128::from(pa[y * w as usize + x]) * u128::from(pb[y * w as usize + x]);
            let i = (y + 1) * cw + (x + 1);
            t[i] = v + t[i - 1] + t[i - cw] - t[i - cw - 1];
        }
    }
    Integral(t)
}

/// RGBA8 / RGB8 バッファから SSIM 用のグレースケール画像を作る。
///
/// 輝度定義は dHash や `stats::sharpness` と共有(`stats::luma_u8`)。
pub fn gray_from_rgba8(rgba: &[u8], width: u32, height: u32) -> Option<GrayImage> {
    gray_from_interleaved(rgba, 4, width, height)
}

/// RGB8 版。[`gray_from_rgba8`] と同じ画素なら同じ結果になる。
pub fn gray_from_rgb8(rgb: &[u8], width: u32, height: u32) -> Option<GrayImage> {
    gray_from_interleaved(rgb, 3, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_16_lowercase_digits() {
        assert_eq!(dhash_hex(0), "0000000000000000");
        assert_eq!(dhash_hex(u64::MAX), "ffffffffffffffff");
        assert_eq!(dhash_hex(0xABCD), "000000000000abcd");
    }

    #[test]
    fn hamming_counts_differing_bits() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1011, 0b1101), 2);
    }

    /// 一様画像は全セル平均が等しい → `left < right` がどこでも偽 → ハッシュは 0。
    #[test]
    fn uniform_image_hashes_to_zero() {
        let rgba = [77u8, 77, 77, 255].repeat(32 * 32);
        assert_eq!(dhash_rgba8(&rgba, 32, 32), 0);
    }

    /// 左→右で明るくなるグラデーションは、全行で全ビットが 1 になる
    /// (各行 8 ビット × 8 行 = 上位 64 ビットのうち 64 本すべて)。
    #[test]
    fn horizontal_ramp_sets_every_bit() {
        let mut rgba = Vec::new();
        for _ in 0..64 {
            for x in 0..64u32 {
                let v = (x * 4) as u8;
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        assert_eq!(dhash_rgba8(&rgba, 64, 64), u64::MAX);
    }

    #[test]
    fn rgb_and_rgba_agree() {
        let mut rgb = Vec::new();
        let mut rgba = Vec::new();
        for y in 0..40u32 {
            for x in 0..40u32 {
                let (r, g, b) = ((x * 6) as u8, (y * 6) as u8, ((x + y) * 3) as u8);
                rgb.extend_from_slice(&[r, g, b]);
                rgba.extend_from_slice(&[r, g, b, 200]);
            }
        }
        assert_eq!(dhash_rgb8(&rgb, 40, 40), dhash_rgba8(&rgba, 40, 40));
    }

    #[test]
    fn malformed_buffer_hashes_to_zero() {
        assert_eq!(dhash_rgba8(&[0, 0, 0, 255], 2, 2), 0);
        assert_eq!(dhash_rgba8(&[], 0, 0), 0);
    }

    /// 9 列 8 行より小さい画像でも panic せず、セルは最低 1 画素を持つ。
    #[test]
    fn tiny_image_does_not_panic() {
        let rgba = [10u8, 20, 30, 255].repeat(3 * 2);
        let _ = dhash_rgba8(&rgba, 3, 2);
    }

    #[test]
    fn cell_bounds_partition_without_gaps() {
        for extent in [1u32, 2, 8, 9, 13, 64, 1477] {
            let mut prev_end = 0u32;
            for i in 0..9 {
                let (s, e) = cell_bounds(extent, i, 9);
                assert!(s < e, "extent={extent} i={i}");
                assert!(e <= extent);
                if extent >= 9 {
                    assert_eq!(s, prev_end, "extent={extent} i={i}");
                }
                prev_end = e;
            }
            if extent >= 9 {
                assert_eq!(prev_end, extent);
            }
        }
    }

    #[test]
    fn ssim_of_identical_is_exactly_one() {
        let img = GrayImage::from_fn(32, 24, |x, y| image::Luma([((x * 7 + y * 3) % 256) as u8]));
        assert_eq!(ssim_gray(&img, &img), Some(1.0));
    }

    #[test]
    fn ssim_rejects_mismatched_dimensions() {
        let a = GrayImage::from_pixel(16, 16, image::Luma([100]));
        let b = GrayImage::from_pixel(16, 17, image::Luma([100]));
        assert_eq!(ssim_gray(&a, &b), None);
    }

    #[test]
    fn ssim_is_symmetric() {
        let a = GrayImage::from_fn(24, 24, |x, y| image::Luma([((x * 11 + y) % 256) as u8]));
        let b = GrayImage::from_fn(24, 24, |x, y| image::Luma([((y * 5 + x * 2) % 256) as u8]));
        assert_eq!(ssim_gray(&a, &b), ssim_gray(&b, &a));
    }

    #[test]
    fn ssim_handles_images_smaller_than_the_window() {
        let a = GrayImage::from_fn(4, 3, |x, _| image::Luma([(x * 60) as u8]));
        assert_eq!(ssim_gray(&a, &a), Some(1.0));
    }

    #[test]
    fn quantize_is_half_away_from_zero() {
        assert_eq!(quantize_1e4(0.123_45), 0.1235);
        assert_eq!(quantize_1e4(-0.123_45), -0.1235);
        assert_eq!(quantize_1e4(1.0), 1.0);
    }
}

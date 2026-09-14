//! inspect 用の輝度ヒストグラム統計。
//!
//! 「どの画像に補正が必要か」を編集者が判断するための、黒点 / 白点 / 中央値を返す。
//! 決定論的: 同一入力バイト列 → 完全に同一の `ImageStats`。
//!
//! # 設計
//!
//! - **色空間**: sRGB 符号値(0-255)そのまま。輝度は BT.709 係数
//!   (`ops::auto_levels` と同じ 0.2126 / 0.7152 / 0.0722)を符号値へ直接掛ける。
//!   線形化はしない(編集者が Photoshop 等のヒストグラムで見る値と揃えるため)。
//! - **ビン**: 256 ビン整数ヒストグラム。ビン添字は `floor(luma + 0.5)` を 0..=255 へ
//!   クランプしたもの(`auto_levels::bin_of` と同じ量子化規則)。
//! - **分位点**: nearest-rank。`rank = ceil(p/100 * N)`(最低 1)を満たす最初のビン添字を
//!   その分位点の値とする。ビン内補間はしない = 常に整数値(0..255)を f64 で返す。
//! - **アルファ**: 無視する(格納されている RGB 符号値をそのまま使う)。プリマルチプライド
//!   でない透明画素の RGB も統計に入る。
//! - **サブサンプル**: 下記参照。
//! - **鮮鋭度(sharpness)**: 上記のヒストグラム系とは別経路。全画素の BT.709 輝度 u8 を
//!   長辺 1024 以下へ整数ボックス平均で縮小し、3×3 ラプラシアン(整数カーネル)の応答の**分散**を
//!   整数演算で求める。間引きグリッドではなく縮小画像を使うのは、間引きだと隣接画素が
//!   飛んでしまいラプラシアンが意味を失うため。
use image::GrayImage;

pub const BINS: usize = 256;

/// BT.709 輝度係数(sRGB 符号値ベース。`ops::auto_levels` と同一)。
const LUMA_R: f64 = 0.2126;
const LUMA_G: f64 = 0.7152;
const LUMA_B: f64 = 0.0722;

/// 統計に使うサンプル数の目安(上限)。512x512 = 262144。
///
/// これ以下の画素数の画像は全画素を使う(= 厳密値)。それより大きい画像は
/// グリッド間引きで概ねこの数まで落とす。
pub const TARGET_SAMPLES: u64 = 262_144;

/// 輝度ヒストグラム統計(inspect の追加情報)。
///
/// 値は **決定論的なサブサンプル**上で計算される(`grid_stride` 参照)。
/// 512x512(262144 画素)以下の画像では全画素が使われるため厳密値。
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct ImageStats {
    /// BT.709 輝度(sRGB 符号値)の分位点(0-255 スケール、小数1桁丸め)
    pub luma_p0_2: f64,
    pub luma_p1: f64,
    pub luma_p50: f64,
    pub luma_p99: f64,
    pub luma_p99_8: f64,
    /// チャンネル平均(0-255、小数1桁)
    pub mean_r: f64,
    pub mean_g: f64,
    pub mean_b: f64,
    /// Variance of the 3x3 Laplacian response over the BT.709 luma, downscaled to a
    /// long edge of at most 1024 px (higher = crisper edges; rounded to 1 decimal).
    /// Scene-dependent, so treat it as a relative measure: for documents, below ~30
    /// usually means motion blur or defocus; compare against a known-good capture
    /// rather than an absolute.
    pub sharpness: f64,
}

/// 鮮鋭度計算で使う縮小後の長辺(px)。
pub const SHARPNESS_LONG_EDGE: u32 = 1024;

/// BT.709 輝度(sRGB 符号値 → u8)。係数を 1/10000 の固定小数で持ち、
/// 丸めは half-up(+5000 して切り捨て)。浮動小数を一切使わないので
/// プラットフォーム差が入る余地がない。
///
/// `crate::similarity` の知覚ハッシュ / SSIM も同じ輝度定義を共有する。
#[inline]
pub fn luma_u8(r: u8, g: u8, b: u8) -> u8 {
    let v = 2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b) + 5000;
    (v / 10000) as u8
}

/// インターリーブ配置の 8bit バッファから BT.709 輝度のグレースケール画像を作る。
///
/// `sharpness`(このモジュール)と SSIM 用のグレー化(`crate::similarity` の
/// `gray_from_rgb8` / `gray_from_rgba8`)が共有する唯一の実装。
pub(crate) fn gray_from_interleaved(
    buf: &[u8],
    channels: usize,
    width: u32,
    height: u32,
) -> Option<GrayImage> {
    let expected = u64::from(width) * u64::from(height) * channels as u64;
    if width == 0 || height == 0 || buf.len() as u64 != expected {
        return None;
    }
    let out: Vec<u8> = buf
        .chunks_exact(channels)
        .map(|px| luma_u8(px[0], px[1], px[2]))
        .collect();
    GrayImage::from_raw(width, height, out)
}

/// 長辺が `long_edge` 以下になるよう**整数ボックス平均**で縮小する。
///
/// 縮小率は整数 `k = ceil(長辺 / long_edge)` に固定し、k×k ブロック(端は実在画素数)の
/// 平均を取る。整数演算のみで f32 の補間を通らないため、決定論が自明で、
/// 最適化なしの debug ビルドでも速い(Triangle 補間は 1477×1108 → 1024 で 3 秒かかっていた)。
pub(crate) fn downscale_gray(gray: &GrayImage, long_edge: u32) -> std::borrow::Cow<'_, GrayImage> {
    let (w, h) = (gray.width(), gray.height());
    let long = w.max(h);
    if long <= long_edge || long == 0 || long_edge == 0 {
        return std::borrow::Cow::Borrowed(gray);
    }
    let k = long.div_ceil(long_edge);
    let nw = w.div_ceil(k).max(1);
    let nh = h.div_ceil(k).max(1);
    let src = gray.as_raw();
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for by in 0..nh {
        let y0 = by * k;
        let y1 = (y0 + k).min(h);
        for bx in 0..nw {
            let x0 = bx * k;
            let x1 = (x0 + k).min(w);
            let mut sum: u64 = 0;
            for y in y0..y1 {
                let row = (y * w) as usize;
                for x in x0..x1 {
                    sum += u64::from(src[row + x as usize]);
                }
            }
            let count = u64::from(y1 - y0) * u64::from(x1 - x0);
            // 四捨五入(整数)。count ≥ 1 は保証される。
            out.push(((sum + count / 2) / count) as u8);
        }
    }
    std::borrow::Cow::Owned(GrayImage::from_raw(nw, nh, out).expect("box downscale dimensions"))
}

/// グレースケール画像の鮮鋭度(3×3 ラプラシアン応答の分散、小数1桁丸め)。
///
/// 長辺 1024 以下へ整数ボックス平均で縮小してから計算するので、同じ被写体を別解像度で
/// 撮った画像どうしを比べやすい。3×3 未満の画像は 0.0。
pub fn sharpness_gray(gray: &GrayImage) -> f64 {
    laplacian_variance(&downscale_gray(gray, SHARPNESS_LONG_EDGE))
}

/// 3×3 ラプラシアン(カーネル `[0,1,0, 1,-4,1, 0,1,0]`)応答の分散。
///
/// 応答は i32(範囲 ±1020)、総和は i64、平方和は i128 で厳密に積む。
/// 分散は `(n*Σl² - (Σl)²) / n²` を整数で作ってから 1 回だけ f64 へ落とすので、
/// 走査順に依存する浮動小数の再結合誤差が発生しない。境界 1 画素は除外する。
fn laplacian_variance(gray: &GrayImage) -> f64 {
    let (w, h) = gray.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let buf = gray.as_raw();
    let stride = w as usize;
    let mut sum: i64 = 0;
    let mut sum_sq: i128 = 0;
    let mut n: u64 = 0;
    for y in 1..(h as usize - 1) {
        let row = y * stride;
        for x in 1..(w as usize - 1) {
            let c = i32::from(buf[row + x]);
            let up = i32::from(buf[row - stride + x]);
            let down = i32::from(buf[row + stride + x]);
            let left = i32::from(buf[row + x - 1]);
            let right = i32::from(buf[row + x + 1]);
            let l = up + left - 4 * c + right + down;
            sum += i64::from(l);
            sum_sq += i128::from(l) * i128::from(l);
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let numerator = i128::from(n) * sum_sq - i128::from(sum) * i128::from(sum);
    round1(numerator as f64 / (n as f64 * n as f64))
}

/// グリッド間引きの刻み幅。`k = ceil(sqrt(w*h / TARGET_SAMPLES))`(最低 1)。
///
/// x, y の**両方**を k 刻みで走査するので、サンプル数は概ね `w*h / k^2`
/// (= TARGET_SAMPLES 以下)になる。画像サイズだけで決まるので決定論的。
pub fn grid_stride(width: u32, height: u32) -> u32 {
    let total = u64::from(width) * u64::from(height);
    if total <= TARGET_SAMPLES {
        return 1;
    }
    // f64 の sqrt は 2^53 未満の整数比では丸め誤差が問題にならないが、
    // 境界で 1 小さい k を選ばないよう ceil 後に検算して補正する。
    let mut k = ((total as f64) / (TARGET_SAMPLES as f64)).sqrt().ceil() as u64;
    k = k.max(1);
    while k > 1 && samples_for(width, height, (k - 1) as u32) <= TARGET_SAMPLES {
        k -= 1;
    }
    while samples_for(width, height, k as u32) > TARGET_SAMPLES {
        k += 1;
    }
    k.min(u64::from(u32::MAX)) as u32
}

/// 刻み幅 k で実際に取られるサンプル数。
fn samples_for(width: u32, height: u32, k: u32) -> u64 {
    let k = u64::from(k.max(1));
    let nx = u64::from(width).div_ceil(k);
    let ny = u64::from(height).div_ceil(k);
    nx * ny
}

/// 輝度(0..255)をビン添字へ量子化する。
#[inline]
fn bin_of(luma: f64) -> usize {
    let b = (luma + 0.5).floor();
    if b <= 0.0 {
        0
    } else if b >= (BINS - 1) as f64 {
        BINS - 1
    } else {
        b as usize
    }
}

/// nearest-rank 分位点。`hist` は 256 ビン、`total` は総サンプル数(> 0)。
fn percentile(hist: &[u64; BINS], total: u64, p: f64) -> f64 {
    let rank = ((p / 100.0) * total as f64).ceil().max(1.0);
    let rank = if rank > total as f64 {
        total
    } else {
        rank as u64
    };
    let mut cumulative = 0u64;
    for (i, &c) in hist.iter().enumerate() {
        cumulative += c;
        if cumulative >= rank {
            return i as f64;
        }
    }
    (BINS - 1) as f64
}

/// 小数1桁へ half-away-from-zero 丸め。
fn round1(v: f64) -> f64 {
    let scaled = v * 10.0;
    let r = if scaled >= 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    r / 10.0
}

/// RGBA8 画素バッファ(row-major, 4byte/px)から統計を計算する。
///
/// `width * height * 4 == rgba.len()` を前提とする。サンプルが 0 件なら `None`。
pub fn from_rgba8(rgba: &[u8], width: u32, height: u32) -> Option<ImageStats> {
    from_interleaved(rgba, 4, width, height)
}

/// RGB8 画素バッファ(row-major, 3byte/px)から統計を計算する。
///
/// JPEG のように RGBA へ広げる必要がない入力で、無駄な 1 パスを省くための入口。
/// 同じ画素なら `from_rgba8` と完全に同じ値を返す(アルファは元々無視するため)。
pub fn from_rgb8(rgb: &[u8], width: u32, height: u32) -> Option<ImageStats> {
    from_interleaved(rgb, 3, width, height)
}

/// インターリーブ配置の 8bit バッファ(先頭 3 バイトが R, G, B)から統計を計算する。
fn from_interleaved(buf: &[u8], channels: usize, width: u32, height: u32) -> Option<ImageStats> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = u64::from(width) * u64::from(height) * channels as u64;
    if buf.len() as u64 != expected {
        return None;
    }

    let k = grid_stride(width, height);
    let mut hist = [0u64; BINS];
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;
    let mut count = 0u64;

    let row_bytes = u64::from(width) * channels as u64;
    let mut y = 0u32;
    while y < height {
        let row_start = (u64::from(y) * row_bytes) as usize;
        let mut x = 0u32;
        while x < width {
            let i = row_start + (x as usize) * channels;
            let r = buf[i];
            let g = buf[i + 1];
            let b = buf[i + 2];
            sum_r += u64::from(r);
            sum_g += u64::from(g);
            sum_b += u64::from(b);
            let luma = LUMA_R * f64::from(r) + LUMA_G * f64::from(g) + LUMA_B * f64::from(b);
            hist[bin_of(luma)] += 1;
            count += 1;
            x = x.saturating_add(k);
        }
        y = y.saturating_add(k);
    }

    if count == 0 {
        return None;
    }
    let n = count as f64;

    // 鮮鋭度は間引きではなく縮小画像で測る(隣接画素が要るため)。
    let sharpness = gray_from_interleaved(buf, channels, width, height)
        .map(|gray| sharpness_gray(&gray))
        .unwrap_or(0.0);

    Some(ImageStats {
        luma_p0_2: round1(percentile(&hist, count, 0.2)),
        luma_p1: round1(percentile(&hist, count, 1.0)),
        luma_p50: round1(percentile(&hist, count, 50.0)),
        luma_p99: round1(percentile(&hist, count, 99.0)),
        luma_p99_8: round1(percentile(&hist, count, 99.8)),
        mean_r: round1(sum_r as f64 / n),
        mean_g: round1(sum_g as f64 / n),
        mean_b: round1(sum_b as f64 / n),
        sharpness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stride_is_one_for_small_images() {
        assert_eq!(grid_stride(512, 512), 1);
        assert_eq!(grid_stride(1, 1), 1);
    }

    #[test]
    fn stride_keeps_samples_under_target() {
        for (w, h) in [(513, 512), (1477, 1108), (4000, 3000), (10000, 10000)] {
            let k = grid_stride(w, h);
            assert!(k >= 1);
            let n = samples_for(w, h, k);
            assert!(n <= TARGET_SAMPLES, "{w}x{h}: k={k} n={n}");
            if k > 1 {
                assert!(
                    samples_for(w, h, k - 1) > TARGET_SAMPLES,
                    "{w}x{h}: k={k} is not minimal"
                );
            }
        }
    }

    #[test]
    fn round1_is_half_away_from_zero() {
        assert_eq!(round1(0.25), 0.3);
        assert_eq!(round1(1.05), 1.1);
        assert_eq!(round1(128.0), 128.0);
    }

    #[test]
    fn flat_gray_is_exact() {
        let rgba = [128u8, 128, 128, 255].repeat(64 * 64);
        let s = from_rgba8(&rgba, 64, 64).unwrap();
        assert_eq!(s.luma_p0_2, 128.0);
        assert_eq!(s.luma_p50, 128.0);
        assert_eq!(s.luma_p99_8, 128.0);
        assert_eq!(s.mean_r, 128.0);
    }

    #[test]
    fn flat_gray_has_zero_sharpness() {
        let rgba = [128u8, 128, 128, 255].repeat(64 * 64);
        assert_eq!(from_rgba8(&rgba, 64, 64).unwrap().sharpness, 0.0);
    }

    /// 1 画素おきの白黒縦縞は最大級のラプラシアン応答を出す(縮小が要らない 64x64)。
    #[test]
    fn stripes_have_large_sharpness() {
        let mut rgba = Vec::new();
        for _ in 0..64 {
            for x in 0..64 {
                let v = if x % 2 == 0 { 0u8 } else { 255 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let s = from_rgba8(&rgba, 64, 64).unwrap();
        assert!(s.sharpness > 10_000.0, "{}", s.sharpness);
    }

    #[test]
    fn luma_u8_matches_bt709_endpoints() {
        assert_eq!(luma_u8(0, 0, 0), 0);
        assert_eq!(luma_u8(255, 255, 255), 255);
        assert_eq!(luma_u8(255, 0, 0), 54); // 0.2126*255 = 54.213
        assert_eq!(luma_u8(0, 255, 0), 182); // 0.7152*255 = 182.376
        assert_eq!(luma_u8(0, 0, 255), 18); // 0.0722*255 = 18.411
    }

    #[test]
    fn sharpness_is_zero_for_tiny_images() {
        let rgba = [10u8, 20, 30, 255].repeat(4);
        assert_eq!(from_rgba8(&rgba, 2, 2).unwrap().sharpness, 0.0);
    }

    #[test]
    fn rejects_mismatched_buffer() {
        assert!(from_rgba8(&[0, 0, 0, 255], 2, 2).is_none());
    }
}

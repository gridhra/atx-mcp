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

    Some(ImageStats {
        luma_p0_2: round1(percentile(&hist, count, 0.2)),
        luma_p1: round1(percentile(&hist, count, 1.0)),
        luma_p50: round1(percentile(&hist, count, 50.0)),
        luma_p99: round1(percentile(&hist, count, 99.0)),
        luma_p99_8: round1(percentile(&hist, count, 99.8)),
        mean_r: round1(sum_r as f64 / n),
        mean_g: round1(sum_g as f64 / n),
        mean_b: round1(sum_b as f64 / n),
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
    fn rejects_mismatched_buffer() {
        assert!(from_rgba8(&[0, 0, 0, 255], 2, 2).is_none());
    }
}

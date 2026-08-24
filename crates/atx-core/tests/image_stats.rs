//! `ImageInfo::stats`(輝度ヒストグラム統計)のテスト。
//!
//! 期待値はすべて `stats` モジュールの規則(256 ビン / nearest-rank / half-away 丸め)から
//! 手計算したもの。実装の出力をそのまま貼り付けたゴールデンではない。

use atx_core::stats::{grid_stride, ImageStats, TARGET_SAMPLES};
use atx_core::{inspect_bytes, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn stats_of(img: &RgbaImage) -> ImageStats {
    inspect_bytes(&encode_png(img), &Limits::default())
        .unwrap()
        .stats
        .expect("raster input must produce stats")
}

/// フラットな中間グレー: 全分位点が 128、平均も厳密に 128。
#[test]
fn flat_gray_has_flat_percentiles() {
    let img = RgbaImage::from_pixel(64, 64, Rgba([128, 128, 128, 255]));
    let s = stats_of(&img);
    assert_eq!(s.luma_p0_2, 128.0);
    assert_eq!(s.luma_p1, 128.0);
    assert_eq!(s.luma_p50, 128.0);
    assert_eq!(s.luma_p99, 128.0);
    assert_eq!(s.luma_p99_8, 128.0);
    assert_eq!((s.mean_r, s.mean_g, s.mean_b), (128.0, 128.0, 128.0));
}

/// 256x256 の水平グレーグラデーション(画素値 = x)。
///
/// BT.709 係数の和はちょうど 1 なので luma == x、ビン i のカウントは一律 256、
/// N = 65536(<= 262144 なので間引きなし = 厳密)。nearest-rank から:
/// - p0.2 : rank = ceil(0.002*65536) = 132  → (i+1)*256 >= 132  → i = 0
/// - p1   : rank = ceil(0.01 *65536) = 656  → (i+1)*256 >= 656  → i = 2
/// - p50  : rank = ceil(0.5  *65536) = 32768→ (i+1)*256 >= 32768→ i = 127
/// - p99  : rank = ceil(0.99 *65536) = 64881→ (i+1)*256 >= 64881→ i = 253
/// - p99.8: rank = ceil(0.998*65536) = 65405→ (i+1)*256 >= 65405→ i = 255
///
/// 平均は 0..=255 の算術平均 = 127.5。
#[test]
fn gray_gradient_percentiles_match_hand_computed_ranks() {
    let img = RgbaImage::from_fn(256, 256, |x, _| {
        let v = x as u8;
        Rgba([v, v, v, 255])
    });
    let s = stats_of(&img);
    assert_eq!(s.luma_p0_2, 0.0);
    assert_eq!(s.luma_p1, 2.0);
    assert_eq!(s.luma_p50, 127.0, "中央値はほぼ中央(nearest-rank で 127)");
    assert_eq!(s.luma_p99, 253.0);
    assert_eq!(s.luma_p99_8, 255.0);
    assert_eq!((s.mean_r, s.mean_g, s.mean_b), (127.5, 127.5, 127.5));
}

/// 列ごとに 2 色が交互に並ぶ画像 → チャンネル平均は 2 色の算術平均に厳密一致。
/// (200+40)/2 = 120, (100+60)/2 = 80, (50+80)/2 = 65。
///
/// 輝度は 0.2126*200+0.7152*100+0.0722*50 = 117.65 → ビン 118、
/// 0.2126*40+0.7152*60+0.0722*80 = 57.16 → ビン 57。各 50% ずつなので
/// p50 は rank = N/2 をちょうど満たす下側の 57、p99 / p99.8 は 118。
#[test]
fn two_color_image_means_are_analytic() {
    let img = RgbaImage::from_fn(32, 32, |x, _| {
        if x % 2 == 0 {
            Rgba([200, 100, 50, 255])
        } else {
            Rgba([40, 60, 80, 255])
        }
    });
    let s = stats_of(&img);
    assert_eq!((s.mean_r, s.mean_g, s.mean_b), (120.0, 80.0, 65.0));
    assert_eq!(s.luma_p0_2, 57.0);
    assert_eq!(s.luma_p50, 57.0);
    assert_eq!(s.luma_p99, 118.0);
    assert_eq!(s.luma_p99_8, 118.0);
}

/// アルファは統計に影響しない(格納されている RGB 符号値をそのまま使う)。
#[test]
fn alpha_is_ignored() {
    let opaque = RgbaImage::from_pixel(16, 16, Rgba([90, 90, 90, 255]));
    let transparent = RgbaImage::from_pixel(16, 16, Rgba([90, 90, 90, 0]));
    assert_eq!(stats_of(&opaque), stats_of(&transparent));
}

/// 同じバイト列を 2 回 inspect したら統計はビット単位で同一。
#[test]
fn stats_are_deterministic() {
    let a = inspect_bytes(FIXTURE, &Limits::default()).unwrap();
    let b = inspect_bytes(FIXTURE, &Limits::default()).unwrap();
    assert_eq!(a.stats, b.stats);
    assert!(a.stats.is_some());
}

/// 512x512(= 262144 画素)までは間引きなし = 全画素の厳密値。
///
/// 「同じ絵を別サイズにしても統計が一致する」ことは**主張しない**:
/// 間引きは画素グリッド上の決定論的な部分集合であって、リサイズや
/// 別解像度の同一被写体に対する統計的な不変量ではないため
/// (例えば周期パターンは刻み幅と共鳴して分布が偏りうる)。
/// ここで保証するのは「同一バイト列 → 同一値」と「小さい画像は厳密」の 2 点だけ。
#[test]
fn exact_path_covers_up_to_512x512() {
    assert_eq!(grid_stride(512, 512), 1);
    assert_eq!(u64::from(512u32) * 512, TARGET_SAMPLES);
    assert!(grid_stride(513, 512) > 1);

    // 512x512 の厳密経路: 半分が 0、半分が 255 の縦縞 → p50 は下側の 0。
    let img = RgbaImage::from_fn(512, 512, |x, _| {
        let v = if x % 2 == 0 { 0u8 } else { 255u8 };
        Rgba([v, v, v, 255])
    });
    let s = stats_of(&img);
    assert_eq!(s.luma_p0_2, 0.0);
    assert_eq!(s.luma_p50, 0.0);
    assert_eq!(s.luma_p99, 255.0);
    assert_eq!((s.mean_r, s.mean_g, s.mean_b), (127.5, 127.5, 127.5));

    // 同じ画像をもう一度 inspect しても完全一致(間引きなし経路の決定論)。
    assert_eq!(stats_of(&img), s);
}

/// フィクスチャ(1477x1108, k=3 の間引き経路)は統計を得つつ既存フィールドは不変。
#[test]
fn fixture_gains_stats_without_changing_existing_fields() {
    let t = std::time::Instant::now();
    let info = inspect_bytes(FIXTURE, &Limits::default()).unwrap();
    let elapsed = t.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "inspect は debug ビルドでも 2 秒未満であること: {elapsed:?}"
    );

    // 既存フィールド(engine.rs の inspect テストと同じ期待値)。
    assert_eq!((info.width, info.height), (1477, 1108));
    assert_eq!(info.mime_type, "image/jpeg");
    assert!(!info.has_alpha);
    assert_eq!(info.exif_orientation, None);
    assert!(!info.has_gps);

    let s = info.stats.expect("JPEG フィクスチャは統計を持つ");
    assert_eq!(grid_stride(1477, 1108), 3);
    // 写真らしい合成画像: 単調増加する分位点で、両端は飽和していない。
    assert!(s.luma_p0_2 <= s.luma_p1);
    assert!(s.luma_p1 < s.luma_p50);
    assert!(s.luma_p50 < s.luma_p99);
    assert!(s.luma_p99 <= s.luma_p99_8);
    assert!((0.0..=255.0).contains(&s.luma_p50));
    for m in [s.mean_r, s.mean_g, s.mean_b] {
        assert!((0.0..=255.0).contains(&m));
    }
}

/// 統計は JSON へそのまま載る(MCP の structuredContent 経路)。
#[test]
fn stats_serialize_with_snake_case_names() {
    let info = inspect_bytes(FIXTURE, &Limits::default()).unwrap();
    let v = serde_json::to_value(&info).unwrap();
    let s = &v["stats"];
    for key in [
        "luma_p0_2",
        "luma_p1",
        "luma_p50",
        "luma_p99",
        "luma_p99_8",
        "mean_r",
        "mean_g",
        "mean_b",
    ] {
        assert!(s[key].is_number(), "missing stats.{key}");
    }
}

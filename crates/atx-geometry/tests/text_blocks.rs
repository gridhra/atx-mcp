//! detect_text_blocks の検出・棄却・決定論テスト。
//!
//! 合成書類フィクスチャ `tests/fixtures/synthetic_document.png` の既知値は
//! `crates/atx-core/examples/gen_fixture.rs` の `draw_synthetic_document` /
//! `draw_document_body` から算術で求めたもの(下記 DOC_* 定数のコメント参照)。

use atx_geometry::{detect_text_blocks, TextBlockDetection, TextBlockParams};
use image::{DynamicImage, GrayImage, Luma, RgbImage};

/// 合成書類(900x1200)の既知の版面。`draw_synthetic_document` より:
/// 用紙 = (60,60)-(840,1140)、pad = 780/10 = 78、text_w = 624、
/// バー高 bar_h = 1080/62 = 17、行送り line_step = 51、単語間 space = 12。
const DOC_W: u32 = 900;
const DOC_H: u32 = 1200;
/// 本文ブロックの左端(= 60 + 78)。
const DOC_TEXT_X: u32 = 138;
/// 本文バーの高さ(= 行の「文字高さ」)。
const DOC_BAR_H: u32 = 17;
/// 行送り(バーの縦ピッチ)。
const DOC_LINE_STEP: u32 = 51;
/// 段落 1 の先頭行の上端(= 60 + 51*4)と段落 2 の先頭行の上端(= 672)。
const DOC_PARA1_Y: u32 = 264;
const DOC_PARA2_Y: u32 = 672;
/// 各段落は 7 行(0..=6 で改段)。
const DOC_PARA_LINES: u32 = 7;

fn fixture(relative: &str) -> DynamicImage {
    let path = concat_path(relative);
    image::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"))
}

fn concat_path(relative: &str) -> String {
    format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), relative)
}

fn run(image: &DynamicImage) -> TextBlockDetection {
    detect_text_blocks(image, &TextBlockParams::default())
}

/// 許容誤差つきの近似一致(tol は絶対値)。
fn near(actual: u32, expected: u32, tol: u32) -> bool {
    actual.abs_diff(expected) <= tol
}

#[test]
fn synthetic_document_finds_heading_and_two_paragraphs() {
    let img = fixture("tests/fixtures/synthetic_document.png");
    assert_eq!((img.width(), img.height()), (DOC_W, DOC_H));
    let d = run(&img);

    // 見出し + 2 段落 で 3 ± 1。
    assert!(
        (2..=4).contains(&d.blocks.len()),
        "expected 3 +/- 1 blocks, got {}: {:?}",
        d.blocks.len(),
        d.blocks
    );
    assert!(
        d.warnings.is_empty(),
        "unexpected warnings: {:?}",
        d.warnings
    );

    // 行高はバーの高さと ±15%。
    let median = d.median_line_height_px.expect("median line height");
    let tol = DOC_BAR_H * 15 / 100 + 1;
    assert!(
        near(median, DOC_BAR_H, tol),
        "median line height {median} is not within +/-15% of bar height {DOC_BAR_H}"
    );

    // 読み順は上から(y 単調増加)。
    let ys: Vec<u32> = d.blocks.iter().map(|b| b.rect.y).collect();
    assert!(
        ys.windows(2).all(|w| w[0] <= w[1]),
        "blocks are not in top-to-bottom order: {ys:?}"
    );

    // 本文の 2 段落は既知位置 ±3%(画像寸法に対する割合)。
    let tol_x = DOC_W * 3 / 100;
    let tol_y = DOC_H * 3 / 100;
    let para1 = d
        .blocks
        .iter()
        .find(|b| near(b.rect.y, DOC_PARA1_Y, tol_y))
        .unwrap_or_else(|| {
            panic!(
                "no block near paragraph 1 (y={DOC_PARA1_Y}): {:?}",
                d.blocks
            )
        });
    let para2 = d
        .blocks
        .iter()
        .find(|b| near(b.rect.y, DOC_PARA2_Y, tol_y))
        .unwrap_or_else(|| {
            panic!(
                "no block near paragraph 2 (y={DOC_PARA2_Y}): {:?}",
                d.blocks
            )
        });
    for (name, b) in [("paragraph 1", para1), ("paragraph 2", para2)] {
        assert!(
            near(b.rect.x, DOC_TEXT_X, tol_x),
            "{name} x={} is not within 3% of {DOC_TEXT_X}",
            b.rect.x
        );
        assert_eq!(b.line_count, DOC_PARA_LINES, "{name} line count");
        assert!(
            near(
                b.rect.height,
                DOC_LINE_STEP * (DOC_PARA_LINES - 1) + DOC_BAR_H,
                tol_y
            ),
            "{name} height={} does not match 7 lines at pitch {DOC_LINE_STEP}",
            b.rect.height
        );
        assert!(
            b.ink_ratio > 0.0 && b.ink_ratio < 1.0,
            "{name} ink_ratio out of range: {}",
            b.ink_ratio
        );
    }

    // 文字らしい面積は紙面のおおよそ 4 割(本文 2 段落 + 見出し)。
    assert!(
        d.text_like_area_ratio > 0.2 && d.text_like_area_ratio < 0.7,
        "text_like_area_ratio = {}",
        d.text_like_area_ratio
    );

    // 900x1200 を長辺 1568 にすると行高は 17 * 1568/1200 = 22.2px で十分読める
    // → 帯は画像全体 1 つ。
    let leg = d.legibility.as_ref().expect("legibility");
    assert!(
        leg.line_height_at_1568_px > 16.0,
        "line_height_at_1568_px = {}",
        leg.line_height_at_1568_px
    );
    assert_eq!(leg.recommended_bands.len(), 1);
    let band = &leg.recommended_bands[0];
    assert_eq!(band.op, "crop");
    assert_eq!(
        (band.rect.x, band.rect.y, band.rect.width, band.rect.height),
        (0, 0, DOC_W, DOC_H)
    );
}

#[test]
fn rectified_photo_finds_at_least_two_paragraph_blocks() {
    let img = fixture("evals/fixtures/document_photo_rectified.png");
    let d = run(&img);
    assert!(
        d.blocks.len() >= 2,
        "expected >= 2 blocks on the rectified document photo, got {}: {:?}",
        d.blocks.len(),
        d.blocks
    );
    assert!(d.median_line_height_px.is_some());
    let leg = d.legibility.as_ref().expect("legibility");
    assert!(!leg.recommended_bands.is_empty());
    assert!(leg.recommended_bands.len() <= 16);
    for band in &leg.recommended_bands {
        assert!(band.rect.y + band.rect.height <= img.height());
        assert_eq!(band.rect.width, img.width());
    }
}

#[test]
fn building_scene_has_little_text_like_area_and_stays_stable() {
    let img = fixture("tests/fixtures/synthetic_scene.jpg");
    let d = run(&img);
    // 建物写真は「文字らしい」領域がほとんど無い。何かが取れても警告なしで安定。
    assert!(
        d.text_like_area_ratio < 0.15 || d.blocks.is_empty(),
        "scene text_like_area_ratio = {} with {} blocks",
        d.text_like_area_ratio,
        d.blocks.len()
    );
    let again = run(&img);
    assert_eq!(
        serde_json::to_string(&d).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
}

#[test]
fn uniform_image_reports_no_text_like_regions() {
    let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(400, 300, Luma([210])));
    let d = run(&img);
    assert!(d.blocks.is_empty());
    assert_eq!(d.warnings, vec!["no_text_like_regions".to_string()]);
    assert!(d.median_line_height_px.is_none());
    assert!(d.legibility.is_none());
    assert_eq!(d.text_like_area_ratio, 0.0);
}

#[test]
fn detection_is_deterministic_across_calls() {
    let img = fixture("tests/fixtures/synthetic_document.png");
    let a = serde_json::to_string(&run(&img)).unwrap();
    let b = serde_json::to_string(&run(&img)).unwrap();
    assert_eq!(a, b);
}

#[test]
fn max_blocks_is_clamped_and_respected() {
    let img = fixture("tests/fixtures/synthetic_document.png");
    let d = detect_text_blocks(
        &img,
        &TextBlockParams {
            max_blocks: 1,
            ..TextBlockParams::default()
        },
    );
    assert_eq!(d.blocks.len(), 1);
    // 0 は 1 に、1000 は 128 にクランプされる(panic しない)。
    let zero = detect_text_blocks(
        &img,
        &TextBlockParams {
            max_blocks: 0,
            ..TextBlockParams::default()
        },
    );
    assert_eq!(zero.blocks.len(), 1);
    let huge = detect_text_blocks(
        &img,
        &TextBlockParams {
            max_blocks: 1000,
            ..TextBlockParams::default()
        },
    );
    assert!(huge.blocks.len() <= 128);
}

/// 小さい文字の版面: 行高が 1568 換算で 16px を割るので複数の帯に分かれる。
#[test]
fn small_text_is_split_into_overlapping_bands() {
    // 1000x6000 の細長い紙面に高さ 12px の行を 100 行(行送り 48px)。
    // 長辺 6000 → 1568 換算の行高 = 12 * 1568/6000 = 3.1px で読めない。
    // 帯の高さ上限 = 1568 * 12 / 16 = 1176px(幅 1000 はこれ以下なので帯分割で救える)。
    const W: u32 = 1000;
    const H: u32 = 6000;
    let mut img = GrayImage::from_pixel(W, H, Luma([240]));
    for i in 0..100u32 {
        let y0 = 100 + i * 48;
        for y in y0..y0 + 12 {
            for x in 120..880 {
                img.put_pixel(x, y, Luma([20]));
            }
        }
    }
    let d = run(&DynamicImage::ImageLuma8(img));
    let leg = d.legibility.as_ref().expect("legibility");
    assert!(
        leg.line_height_at_1568_px < 16.0,
        "line_height_at_1568_px = {}",
        leg.line_height_at_1568_px
    );
    let bands = &leg.recommended_bands;
    assert!(
        bands.len() > 1 && bands.len() <= 16,
        "expected multiple bands, got {}: {bands:?}",
        bands.len()
    );
    for w in bands.windows(2) {
        let (a, b) = (&w[0].rect, &w[1].rect);
        assert!(b.y > a.y, "bands must advance: {a:?} then {b:?}");
        assert!(
            b.y < a.y + a.height,
            "consecutive bands must overlap by one line: {a:?} then {b:?}"
        );
    }
    for band in bands {
        assert_eq!(band.op, "crop");
        assert_eq!(band.rect.x, 0);
        assert_eq!(band.rect.width, W);
        assert!(band.rect.y + band.rect.height <= H);
        // 各帯の長辺は「1568 換算で行高 16px」を満たす上限以下。
        let long = band.rect.width.max(band.rect.height);
        assert!(
            long <= 1176,
            "band long edge {long} exceeds the legible limit 1176: {band:?}"
        );
    }
    // 帯は行のある範囲(100..4864)を覆う。
    assert_eq!(bands[0].rect.y, 0);
    let last = bands.last().unwrap().rect;
    assert!(
        last.y + last.height >= 4864,
        "bands stop at {} before the last line at 4864",
        last.y + last.height
    );
}

/// 白地黒文字と黒地白文字で同じブロックが取れる(インク = 少数派の規則)。
#[test]
fn inverted_polarity_gives_the_same_blocks() {
    let make = |bg: u8, fg: u8| {
        let mut img = GrayImage::from_pixel(600, 400, Luma([bg]));
        for i in 0..6u32 {
            let y0 = 60 + i * 30;
            for y in y0..y0 + 12 {
                for x in 80..520 {
                    img.put_pixel(x, y, Luma([fg]));
                }
            }
        }
        DynamicImage::ImageLuma8(img)
    };
    let light = run(&make(235, 25));
    let dark = run(&make(25, 235));
    assert_eq!(
        light.blocks.iter().map(|b| b.rect).collect::<Vec<_>>(),
        dark.blocks.iter().map(|b| b.rect).collect::<Vec<_>>()
    );
}

/// RGB 入力でも輝度化を経て同じ結果になる(前処理の共用確認)。
#[test]
fn rgb_and_gray_inputs_agree() {
    let mut rgb = RgbImage::from_pixel(500, 360, image::Rgb([240, 240, 240]));
    for i in 0..5u32 {
        let y0 = 40 + i * 40;
        for y in y0..y0 + 14 {
            for x in 50..450 {
                rgb.put_pixel(x, y, image::Rgb([30, 30, 30]));
            }
        }
    }
    let a = run(&DynamicImage::ImageRgb8(rgb.clone()));
    let b = run(&DynamicImage::ImageLuma8(
        DynamicImage::ImageRgb8(rgb).to_luma8(),
    ));
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

// ---------------------------------------------------------------------------
// 原寸への戻し(縮小なし = 厳密一致で固定できる場合)
// ---------------------------------------------------------------------------

/// 縮小が起きない大きさなら、返る矩形はインクの外接矩形と**厳密に**一致する。
///
/// 以前は包含端 `x1` を排他端へ写した後にさらに +1 していたので、全ブロックが
/// 縦横 1px ずつ大きく出ていた(原寸でクランプされる右端・下端だけは正しく見えるので
/// 近似一致のテストでは気づけなかった)。
#[test]
fn block_rect_matches_the_ink_bounds_exactly_without_downscaling() {
    // 600x400(長辺 600 < working_long_edge 1536 なので作業解像度 = 原寸)。
    // バー: x 80..=519、高さ 12、縦ピッチ 25、7 本 → y 60..=221。
    let mut img = GrayImage::from_pixel(600, 400, Luma([240]));
    for i in 0..7u32 {
        let y0 = 60 + i * 25;
        for y in y0..y0 + 12 {
            for x in 80..520 {
                img.put_pixel(x, y, Luma([25]));
            }
        }
    }
    let d = run(&DynamicImage::ImageLuma8(img));
    assert_eq!(d.blocks.len(), 1, "1 ブロックに収まること: {:?}", d.blocks);
    let rect = d.blocks[0].rect;
    assert_eq!(
        (rect.x, rect.y, rect.width, rect.height),
        (80, 60, 440, 162),
        "インクは x 80..=519 / y 60..=221 なので幅 440・高さ 162: {rect:?}"
    );
}

// ---------------------------------------------------------------------------
// 帯分割(recommended_bands)の取りこぼし
// ---------------------------------------------------------------------------

/// 縦に細長い版面を作る(行の高さ `line_h`、縦ピッチ `pitch`、x は `x0..x1`)。
fn striped(width: u32, height: u32, line_h: u32, pitch: u32, x0: u32, x1: u32) -> DynamicImage {
    let mut img = GrayImage::from_pixel(width, height, Luma([240]));
    let mut y0 = 0;
    while y0 + line_h <= height {
        for y in y0..y0 + line_h {
            for x in x0..x1 {
                img.put_pixel(x, y, Luma([25]));
            }
        }
        y0 += pitch;
    }
    DynamicImage::ImageLuma8(img)
}

/// 縮小しない params(作業解像度 = 原寸。行の高さをそのまま固定できる)。
fn no_downscale() -> TextBlockParams {
    TextBlockParams {
        working_long_edge: 8192,
        ..TextBlockParams::default()
    }
}

/// 16 本の帯で文字を覆いきれないときは、残りを名指しする警告を出す。
///
/// 以前は `MAX_BANDS` に達したところで黙って打ち切っていたので、
/// 「返された帯を全部読めば全文読める」と誤解できる出力になっていた。
#[test]
fn bands_warn_when_sixteen_are_not_enough() {
    // 200x6000、行高 3・ピッチ 10 → 行高の中央値 3、帯の高さ 1568*3/16 = 294。
    // 16 本で 4704px しか覆えないのに、文字は 5993px まで続く。
    let img = striped(200, 6000, 3, 10, 20, 180);
    let d = detect_text_blocks(&img, &no_downscale());
    let legibility = d.legibility.expect("legibility");
    assert_eq!(
        legibility.recommended_bands.len(),
        16,
        "帯は 16 本で打ち切られること"
    );
    let warning = d
        .warnings
        .iter()
        .find(|w| w.starts_with("recommended_bands cover only the first"))
        .unwrap_or_else(|| panic!("覆いきれない旨の警告が必要: {:?}", d.warnings));
    assert!(
        warning.contains("re-run detect_text_blocks on a crop of the remainder"),
        "警告は次の一手を書くこと: {warning}"
    );
}

/// `max_blocks` で切られたブロックの行も帯の計算に入れる。
///
/// 以前は帯の行情報を「切った後のブロック」からしか集めていなかったので、
/// ブロックが多いページでは帯が最後のブロックまで届かなかった。
#[test]
fn bands_reach_the_last_block_even_past_max_blocks() {
    // 40 ブロック(1 ブロック = 行高 12・ピッチ 24 の 3 行 = 高さ 60)を 160px 間隔で並べる。
    // 既定の max_blocks は 32 なので、返るブロックは 32 件で切られる。
    const BLOCKS: u32 = 40;
    const BLOCK_PITCH: u32 = 160;
    let height = BLOCKS * BLOCK_PITCH;
    let mut img = GrayImage::from_pixel(200, height, Luma([240]));
    for b in 0..BLOCKS {
        for line in 0..3u32 {
            let y0 = b * BLOCK_PITCH + line * 24;
            for y in y0..y0 + 12 {
                for x in 20..180 {
                    img.put_pixel(x, y, Luma([25]));
                }
            }
        }
    }
    // 最後の行の下端(包含)。
    let ink_end = (BLOCKS - 1) * BLOCK_PITCH + 2 * 24 + 12 - 1;

    let d = detect_text_blocks(&DynamicImage::ImageLuma8(img), &no_downscale());
    assert_eq!(
        d.blocks.len(),
        32,
        "この版面は max_blocks で切られる前提のテスト"
    );
    let bands = d.legibility.expect("legibility").recommended_bands;
    let last = bands.last().expect("帯が 1 本以上").rect;
    assert!(
        last.y + last.height > ink_end,
        "帯は最後のインク行({ink_end})まで届くこと: 最後の帯は {} で終わっている ({} 本)",
        last.y + last.height,
        bands.len()
    );
}

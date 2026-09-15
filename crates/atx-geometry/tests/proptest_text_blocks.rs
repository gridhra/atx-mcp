//! プロパティテスト: detect_text_blocks の不変条件。
//!
//! 任意寸法(64..512)の画像にランダムな矩形群を描き、
//! パニックしないこと・返る矩形が画像内に収まること・件数が上限を超えないこと・
//! 同一入力で同一出力(決定論)であることを確かめる。

use atx_geometry::{detect_text_blocks, TextBlockParams};
use image::{DynamicImage, GrayImage, Luma};
use proptest::prelude::*;

/// (幅, 高さ, 矩形群 (x 比, y 比, 幅比, 高さ比, 明度)) から画像を作る。
type Bar = (f32, f32, f32, f32, u8);

fn arb_image() -> impl Strategy<Value = (u32, u32, Vec<Bar>)> {
    (
        64u32..512,
        64u32..512,
        prop::collection::vec(
            (
                0.0f32..1.0,
                0.0f32..1.0,
                0.0f32..0.5,
                0.0f32..0.3,
                any::<u8>(),
            ),
            0..24,
        ),
    )
}

fn render(w: u32, h: u32, bars: &[Bar], bg: u8) -> DynamicImage {
    let mut img = GrayImage::from_pixel(w, h, Luma([bg]));
    for &(rx, ry, rw, rh, v) in bars {
        let x0 = (rx * w as f32) as u32;
        let y0 = (ry * h as f32) as u32;
        let bw = ((rw * w as f32) as u32).max(1);
        let bh = ((rh * h as f32) as u32).max(1);
        for y in y0..(y0 + bh).min(h) {
            for x in x0..(x0 + bw).min(w) {
                img.put_pixel(x, y, Luma([v]));
            }
        }
    }
    DynamicImage::ImageLuma8(img)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn detect_text_blocks_is_bounded_and_deterministic((w, h, bars) in arb_image()) {
        let image = render(w, h, &bars, 235);
        let params = TextBlockParams { max_blocks: 8, ..TextBlockParams::default() };

        let a = detect_text_blocks(&image, &params);
        let b = detect_text_blocks(&image, &params);
        prop_assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "detect_text_blocks must be deterministic for identical input"
        );

        prop_assert!(a.blocks.len() <= params.max_blocks);
        prop_assert!((0.0..=1.0).contains(&a.text_like_area_ratio));
        for block in &a.blocks {
            let r = block.rect;
            prop_assert!(r.width >= 1 && r.height >= 1, "degenerate rect: {:?}", r);
            prop_assert!(
                r.x + r.width <= w && r.y + r.height <= h,
                "rect {:?} escapes the {}x{} image",
                r, w, h
            );
            prop_assert!((0.0..=1.0).contains(&block.ink_ratio));
            prop_assert!(block.line_count >= 1);
            prop_assert!(block.median_line_height_px >= 1);
        }

        match (&a.legibility, a.blocks.is_empty()) {
            // ブロックが無いときは理由が先頭に来る(縦書きの疑いなど、後続の警告は付きうる)。
            (None, true) => prop_assert_eq!(
                a.warnings.first().map(String::as_str),
                Some("no_text_like_regions")
            ),
            (Some(leg), false) => {
                prop_assert!(!leg.recommended_bands.is_empty());
                prop_assert!(leg.recommended_bands.len() <= 16);
                for band in &leg.recommended_bands {
                    prop_assert_eq!(&band.op, "crop");
                    prop_assert!(band.rect.height >= 1);
                    prop_assert!(
                        band.rect.y + band.rect.height <= h,
                        "band {:?} escapes height {}", band.rect, h
                    );
                    prop_assert!(band.rect.x + band.rect.width <= w);
                }
            }
            (leg, empty) => prop_assert!(
                false,
                "legibility present = {} but blocks empty = {}",
                leg.is_some(), empty
            ),
        }
    }
}

//! エンコーダの寸法境界(2026-10-22 のレビューで見つかった panic の回帰テスト)。
//!
//! libwebp は 1 辺 16383px までしか受け付けない。超える寸法を渡すとエンコードに失敗し、
//! `webp` クレートは失敗を panic にするため、1 要求がプロセス全体を落としていた
//! (ツール層が panic を応答に変えるようになった後も、エラーとして返すのが正しい)。

use atx_core::{apply_recipe, Limits, TransformRecipe};

fn tiny_png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(4, 4, |x, y| image::Rgb([x as u8 * 60, y as u8 * 60, 128]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).unwrap()
}

#[test]
fn webp_refuses_dimensions_over_16383_instead_of_panicking() {
    let input = tiny_png();
    // 16384 x 2 = 32768 画素: 画素数上限(100MP)にはまったく届かない。
    let result = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"resize","width":16384,"height":2,"fit":"fill","without_enlargement":false},{"op":"encode","format":"webp"}]}"#,
        ),
        &Limits::default(),
    );
    match result {
        Err(atx_core::AtxError::Encode(message)) => {
            assert!(message.contains("16383"), "{message}");
        }
        other => panic!("expected an Encode error, got {other:?}"),
    }
}

#[test]
fn webp_still_encodes_at_the_limit() {
    let input = tiny_png();
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"resize","width":16383,"height":2,"fit":"fill","without_enlargement":false},{"op":"encode","format":"webp"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (16383, 2));
    assert_eq!(out.mime_type, "image/webp");
}

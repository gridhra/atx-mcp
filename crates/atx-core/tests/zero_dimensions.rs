//! 寸法 0 の入力画像の扱い(2026-10-22 のレビューの回帰テスト)。
//!
//! PNM(PPM / PGM)のデコーダはヘッダの `0 4` をそのまま受理し、幅 0 の画像を返す。
//! 以前は画素数検査(0 <= 上限)を通って op に流れ、rotate の warp が空バッファで
//! panic していた。入口(寸法検査)で Decode エラーにする。

use atx_core::{apply_recipe, inspect_bytes, AtxError, Limits, TransformRecipe};

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).unwrap()
}

#[test]
fn zero_dimension_inputs_are_rejected_at_decode_instead_of_panicking() {
    for (name, bytes) in [
        ("ppm 0x0", b"P6\n0 0\n255\n".to_vec()),
        ("ppm 0x4", b"P6\n0 4\n255\n".to_vec()),
        ("pgm 4x0", b"P5\n4 0\n255\n".to_vec()),
    ] {
        match inspect_bytes(&bytes, &Limits::default()) {
            Err(AtxError::Decode(message)) => {
                assert!(message.contains("zero"), "{name}: {message}")
            }
            other => panic!("{name}: expected a Decode error from inspect, got {other:?}"),
        }
        let rotated = apply_recipe(
            &bytes,
            &recipe(
                r#"{"operations":[{"op":"rotate","angle_degrees":1.0},{"op":"encode","format":"png"}]}"#,
            ),
            &Limits::default(),
        );
        assert!(
            matches!(rotated, Err(AtxError::Decode(_))),
            "{name}: expected a Decode error from apply_recipe, got {rotated:?}"
        );
    }
}

/// 1x1 は最小の正当な画像で、従来どおり通る。
#[test]
fn one_pixel_images_still_work() {
    let bytes = b"P6\n1 1\n255\n\x10\x20\x30".to_vec();
    let info = inspect_bytes(&bytes, &Limits::default()).unwrap();
    assert_eq!((info.width, info.height), (1, 1));
    let out = apply_recipe(
        &bytes,
        &recipe(r#"{"operations":[{"op":"rotate","angle_degrees":1.0},{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert!(out.width >= 1 && out.height >= 1);
}

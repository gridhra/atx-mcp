//! op が**作ろうとする**画像寸法にも `Limits::max_pixels` が効くことのテスト
//! (セキュリティ点検、DESIGN.md §9.14)。
//!
//! 入力デコード時の画素上限(100MP)だけでは、`resize` / `crop`(pad)/ `perspective`
//! (quad)/ `rotate`(full)が桁違いに大きい中間画像を確保できてしまっていた
//! (実測: `resize` 4294967295x4294967295 で CPU 75 秒・3.6GB 超)。
//! 確保の**前に**寸法を検査し、デコード時と同じ `AtxError::LimitExceeded` で返す。
//!
//! 小さな `max_pixels` を渡す版は「修正前は成功してしまう」ことが安く確かめられる形、
//! 既定の上限で巨大寸法を要求する版は「確保せずに即座に失敗する」ことを固定する形。

use std::time::{Duration, Instant};

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, AtxError, Limits};
use image::{ImageFormat, Rgba, RgbaImage};
use proptest::prelude::*;

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, y| Rgba([(x * 7) as u8, (y * 11) as u8, 90, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn small_limits() -> Limits {
    Limits {
        max_pixels: 1_000,
        ..Limits::default()
    }
}

#[track_caller]
fn assert_limit_error(result: atx_core::Result<atx_core::EncodedOutput>, needle: &str) {
    match result {
        Err(AtxError::LimitExceeded(message)) => {
            assert!(message.contains(needle), "{message}");
            assert!(message.contains("limit is"), "{message}");
        }
        Err(other) => panic!("expected LimitExceeded, got {other}"),
        Ok(out) => panic!(
            "expected LimitExceeded, but the recipe produced a {}x{} image",
            out.width, out.height
        ),
    }
}

/// 巨大寸法の要求が「確保せず即座に」失敗することの目安(debug ビルドでも十分余裕がある)。
const FAST: Duration = Duration::from_secs(5);

// --- 小さな上限: 入力は上限内、op の出力だけが上限を超える -----------------

#[test]
fn resize_output_over_the_pixel_limit_is_rejected() {
    // 入力 20x20 = 400 画素 ≤ 1000、出力 100x100 = 10000 画素 > 1000。
    let r = recipe(
        r#"{"operations":[{"op":"resize","width":100,"height":100,"fit":"fill","without_enlargement":false}]}"#,
    );
    assert_limit_error(
        apply_recipe(&png(20, 20), &r, &small_limits()),
        "operations[0] (resize)",
    );
}

/// 横パスの中間バッファ(出力幅 × 入力高さ)も検査対象。
/// 30x30 → 1000x1 は出力 1000 画素で上限内だが、中間は 1000x30 = 30000 画素。
#[test]
fn resize_intermediate_buffer_over_the_pixel_limit_is_rejected() {
    let r = recipe(
        r#"{"operations":[{"op":"resize","width":1000,"height":1,"fit":"fill","without_enlargement":false}]}"#,
    );
    assert_limit_error(
        apply_recipe(&png(30, 30), &r, &small_limits()),
        "operations[0] (resize)",
    );
}

#[test]
fn crop_pad_output_over_the_pixel_limit_is_rejected() {
    let r = recipe(r#"{"operations":[{"op":"crop","aspect_ratio":"100:1","mode":"pad"}]}"#);
    assert_limit_error(
        apply_recipe(&png(20, 20), &r, &small_limits()),
        "operations[0] (crop)",
    );
}

#[test]
fn perspective_quad_output_over_the_pixel_limit_is_rejected() {
    // quad は画像の外へはみ出してよい(validate は有限・凸性のみ)。出力は平均辺長 = 100x100。
    let r =
        recipe(r#"{"operations":[{"op":"perspective","quad":[[0,0],[100,0],[100,100],[0,100]]}]}"#);
    assert_limit_error(
        apply_recipe(&png(20, 20), &r, &small_limits()),
        "operations[0] (perspective)",
    );
}

#[test]
fn rotate_full_output_over_the_pixel_limit_is_rejected() {
    // 30x30 = 900 ≤ 1000。45° の外接キャンバスは 43x43 = 1849 > 1000。
    let r = recipe(r#"{"operations":[{"op":"rotate","angle_degrees":45,"crop":"full"}]}"#);
    assert_limit_error(
        apply_recipe(&png(30, 30), &r, &small_limits()),
        "operations[0] (rotate)",
    );
}

/// レイヤー内の op も同じ検査を通る。
#[test]
fn ops_inside_layers_are_limited_too() {
    let r = recipe(
        r#"{"layers":[{"source":"base","ops":[{"op":"resize","width":100,"height":100,"fit":"fill","without_enlargement":false}]}],"operations":[]}"#,
    );
    assert_limit_error(
        apply_recipe(&png(20, 20), &r, &small_limits()),
        "operations[0] (resize)",
    );
}

/// 上限ちょうどは通る(境界の取り違え防止)。
#[test]
fn output_exactly_at_the_pixel_limit_is_allowed() {
    let r = recipe(
        r#"{"operations":[{"op":"resize","width":40,"height":25,"fit":"fill","without_enlargement":false}]}"#,
    );
    let out = apply_recipe(&png(20, 20), &r, &small_limits()).expect("1000 pixels is allowed");
    assert_eq!((out.width, out.height), (40, 25));
}

// --- 既定の上限: 巨大寸法を要求しても確保せず即座に失敗する ------------------

#[test]
fn huge_resize_fails_fast_with_default_limits() {
    let r = recipe(
        r#"{"operations":[{"op":"resize","width":4294967295,"height":4294967295,"fit":"fill","without_enlargement":false}]}"#,
    );
    let start = Instant::now();
    assert_limit_error(
        apply_recipe(&png(16, 16), &r, &Limits::default()),
        "operations[0] (resize)",
    );
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

#[test]
fn huge_crop_pad_fails_fast_with_default_limits() {
    let r = recipe(r#"{"operations":[{"op":"crop","aspect_ratio":"4294967295:1","mode":"pad"}]}"#);
    let start = Instant::now();
    assert_limit_error(
        apply_recipe(&png(16, 16), &r, &Limits::default()),
        "operations[0] (crop)",
    );
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

#[test]
fn huge_perspective_quad_fails_fast_with_default_limits() {
    let r =
        recipe(r#"{"operations":[{"op":"perspective","quad":[[0,0],[1e9,0],[1e9,1e9],[0,1e9]]}]}"#);
    let start = Instant::now();
    assert_limit_error(
        apply_recipe(&png(16, 16), &r, &Limits::default()),
        "operations[0] (perspective)",
    );
    assert!(start.elapsed() < FAST, "took {:?}", start.elapsed());
}

// --- 性質: どんな resize 寸法でも「上限エラー」か「上限内の出力」のどちらか ------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn resize_never_exceeds_the_pixel_limit(
        w in 1u32..=200,
        h in 1u32..=200,
        fit in prop::sample::select(vec!["fill", "contain", "cover"]),
    ) {
        let json = format!(
            r#"{{"operations":[{{"op":"resize","width":{w},"height":{h},"fit":"{fit}","without_enlargement":false}}]}}"#
        );
        let limits = Limits { max_pixels: 2_000, ..Limits::default() };
        match apply_recipe(&png(12, 9), &recipe(&json), &limits) {
            Ok(out) => prop_assert!(out.width as u64 * out.height as u64 <= limits.max_pixels),
            Err(AtxError::LimitExceeded(_)) => {}
            Err(other) => prop_assert!(false, "unexpected error: {other}"),
        }
    }
}

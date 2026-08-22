//! `gradient_map` / `pixelate` / `auto_levels`(v0.3.0 の3op)のテスト。
//!
//! すべて `apply_recipe`(JSON レシピ)経由の end-to-end 検証(recipe.rs の validate +
//! engine.rs の dispatch + 各 ops モジュールの画素演算を一括で回帰させるため。
//! `ops` モジュールは `pub(crate)` なので統合テストからは直接触れない)。
//!
//! 入出力は PNG(可逆)を使い、期待値は各 op のモジュールドキュメントに書かれた
//! 定義式から手計算する(実装の出力を貼り付けたものではない)。
//!
//! JSON リテラルは色 hex(`#rrggbb`)を含むため、raw string のデリミタには
//! 衝突しないよう `r###"..."###`(3 ハッシュ)を統一して使う。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn apply(bytes: &[u8], json: &str) -> atx_core::EncodedOutput {
    apply_recipe(bytes, &recipe(json), &Limits::default()).expect("apply_recipe should succeed")
}

fn apply_err(bytes: &[u8], json: &str) -> String {
    apply_recipe(bytes, &recipe(json), &Limits::default())
        .unwrap_err()
        .to_string()
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode_rgba(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgba8()
}

/// PNG 経由(可逆)でレシピを適用し、結果の RGBA8 を返す。
fn run(img: &RgbaImage, json: &str) -> RgbaImage {
    decode_rgba(&apply(&encode_png(img), json).bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 誤差 `tol` 以内で近いかを見るための小さなヘルパ。
fn approx(a: u8, b: u8, tol: i32) -> bool {
    (a as i32 - b as i32).abs() <= tol
}

// ---------------------------------------------------------------------------
// gradient_map
// ---------------------------------------------------------------------------

/// 水平グレーランプ。gray なので BT.709 輝度 = v/255 と厳密一致し、
/// 期待値の手計算が単純になる。
fn gray_ramp(values: &[u8]) -> RgbaImage {
    let mut img = RgbaImage::new(values.len() as u32, 1);
    for (x, &v) in values.iter().enumerate() {
        img.put_pixel(x as u32, 0, Rgba([v, v, v, 255]));
    }
    img
}

#[test]
fn gradient_map_duotone_maps_dark_to_navy_and_light_to_cream() {
    let img = gray_ramp(&[0, 128, 255]);
    let out = run(
        &img,
        r###"{"operations":[{"op":"gradient_map","stops":[
            {"position":0.0,"color":"#000080"},
            {"position":1.0,"color":"#fffdd0"}
        ]},{"op":"encode","format":"png"}]}"###,
    );

    // v = 0 (luma = 0.0) -> exactly navy.
    let navy = out.get_pixel(0, 0);
    assert!(approx(navy[0], 0x00, 2) && approx(navy[1], 0x00, 2) && approx(navy[2], 0x80, 2));

    // v = 255 (luma = 1.0) -> exactly cream.
    let cream = out.get_pixel(2, 0);
    assert!(approx(cream[0], 0xff, 2) && approx(cream[1], 0xfd, 2) && approx(cream[2], 0xd0, 2));

    // v = 128 (luma ~= 0.502) -> linear interpolation between navy and cream.
    // R = 0 + t*255, G = 0 + t*253, B = 128 + t*(208-128), t = 128/255.
    let t = 128.0f64 / 255.0;
    let expected_r = (0.0 + t * 255.0).round() as u8;
    let expected_g = (0.0 + t * 253.0).round() as u8;
    let expected_b = (128.0 + t * (208.0 - 128.0)).round() as u8;
    let mid = out.get_pixel(1, 0);
    assert!(
        approx(mid[0], expected_r, 2)
            && approx(mid[1], expected_g, 2)
            && approx(mid[2], expected_b, 2),
        "mid pixel {:?} not close to expected ({expected_r}, {expected_g}, {expected_b})",
        mid.0
    );
}

#[test]
fn gradient_map_three_stops_selects_the_right_segment() {
    // red @0.0 -> green @0.5 -> blue @1.0
    let json = r###"{"operations":[{"op":"gradient_map","stops":[
        {"position":0.0,"color":"#ff0000"},
        {"position":0.5,"color":"#00ff00"},
        {"position":1.0,"color":"#0000ff"}
    ]},{"op":"encode","format":"png"}]}"###;

    // luma ~= 0.25 -> lower segment (red -> green), t ~= 0.5 -> (128, 128, 0).
    let img_low = gray_ramp(&[64]);
    let out_low = run(&img_low, json);
    let px = out_low.get_pixel(0, 0);
    assert!(approx(px[0], 128, 3) && approx(px[1], 128, 3) && approx(px[2], 0, 3));

    // luma ~= 0.75 -> upper segment (green -> blue), t ~= 0.5 -> (0, 128, 128).
    let img_high = gray_ramp(&[191]);
    let out_high = run(&img_high, json);
    let px = out_high.get_pixel(0, 0);
    assert!(approx(px[0], 0, 3) && approx(px[1], 128, 3) && approx(px[2], 128, 3));
}

#[test]
fn gradient_map_validate_rejects_single_stop() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"gradient_map","stops":[{"position":0.0,"color":"#000000"}]}]}"###,
    );
    assert!(msg.contains("gradient_map"), "{msg}");
}

#[test]
fn gradient_map_validate_rejects_non_increasing_positions() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"gradient_map","stops":[
            {"position":0.5,"color":"#000000"},
            {"position":0.3,"color":"#ffffff"}
        ]}]}"###,
    );
    assert!(msg.contains("gradient_map"), "{msg}");
}

#[test]
fn gradient_map_validate_rejects_bad_hex() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"gradient_map","stops":[
            {"position":0.0,"color":"not-a-color"},
            {"position":1.0,"color":"#ffffff"}
        ]}]}"###,
    );
    assert!(msg.contains("gradient_map"), "{msg}");
}

#[test]
fn gradient_map_validate_rejects_more_than_eight_stops() {
    let stops: String = (0..9)
        .map(|i| format!(r###"{{"position":{},"color":"#ffffff"}}"###, i as f64 / 8.0))
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r###"{{"operations":[{{"op":"gradient_map","stops":[{stops}]}}]}}"###);
    let msg = apply_err(FIXTURE, &json);
    assert!(msg.contains("gradient_map"), "{msg}");
}

// ---------------------------------------------------------------------------
// pixelate
// ---------------------------------------------------------------------------

#[test]
fn pixelate_uniform_image_is_byte_identical() {
    let img = RgbaImage::from_pixel(8, 8, Rgba([100, 150, 200, 255]));
    let out = run(
        &img,
        r###"{"operations":[{"op":"pixelate","block_size":4},{"op":"encode","format":"png"}]}"###,
    );
    assert_eq!(out, img);
}

#[test]
fn pixelate_checkerboard_block_is_flat_at_the_linear_mean() {
    // 2x2 checkerboard, one block covering the whole image (block_size=2, aligned).
    // Pixelate averages in LINEAR light: mean of {1.0, 0.0, 0.0, 1.0} (linear) = 0.5 linear,
    // which re-encodes to sRGB ~= 188 (not 128 — linear averaging is brighter than naive
    // sRGB averaging because the sRGB transfer function is concave near black).
    let mut img = RgbaImage::new(2, 2);
    img.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
    img.put_pixel(1, 0, Rgba([0, 0, 0, 255]));
    img.put_pixel(0, 1, Rgba([0, 0, 0, 255]));
    img.put_pixel(1, 1, Rgba([255, 255, 255, 255]));

    let out = run(
        &img,
        r###"{"operations":[{"op":"pixelate","block_size":2},{"op":"encode","format":"png"}]}"###,
    );

    let p0 = out.get_pixel(0, 0);
    for y in 0..2 {
        for x in 0..2 {
            assert_eq!(out.get_pixel(x, y), p0, "block must be flat");
        }
    }
    // Expected: srgb_oetf(0.5) * 255 ~= 187.5 (rounds to 188).
    assert!(
        approx(p0[0], 188, 2) && approx(p0[1], 188, 2) && approx(p0[2], 188, 2),
        "checkerboard block mean {:?} not close to linear-light expectation (~188)",
        p0.0
    );
    assert_eq!(p0[3], 255);
}

#[test]
fn pixelate_region_leaves_outside_pixels_byte_identical() {
    let mut img = RgbaImage::new(4, 4);
    for y in 0..4 {
        for x in 0..4 {
            img.put_pixel(x, y, Rgba([(x * 40) as u8, (y * 40) as u8, 128, 255]));
        }
    }
    let out = run(
        &img,
        r###"{"operations":[
            {"op":"pixelate","block_size":2,"region":{"x":2,"y":0,"width":2,"height":2}},
            {"op":"encode","format":"png"}
        ]}"###,
    );
    for y in 0..4 {
        for x in 0..4 {
            let inside_region = (2..4).contains(&x) && (0..2).contains(&y);
            if !inside_region {
                assert_eq!(
                    out.get_pixel(x, y),
                    img.get_pixel(x, y),
                    "pixel ({x},{y}) outside region must be untouched"
                );
            }
        }
    }
}

/// region が画像から外れたときのエラー。engine が `AtxError::Operation` へ
/// **1 回だけ**包む(op 側は平文メッセージを返す)。
/// 以前は op 側でも `InvalidRecipe` を作っていたため、
/// "operation 0 (pixelate) failed: invalid recipe: pixelate: ..." のように
/// "invalid recipe:" が二重に付いていた。
#[test]
fn pixelate_region_fully_outside_image_is_an_error() {
    let img = RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255]));
    let msg = apply_err(
        &encode_png(&img),
        r###"{"operations":[{"op":"pixelate","block_size":2,"region":{"x":100,"y":100,"width":4,"height":4}}]}"###,
    );
    assert!(msg.contains("pixelate"), "{msg}");
    assert!(msg.contains("does not intersect"), "{msg}");
    assert!(
        !msg.contains("invalid recipe"),
        "the engine wraps this as an operation failure, not a recipe error: {msg}"
    );
}

/// 回帰: ブロック平均はプリマルチプライで取る(透明画素が色を持ち込まない)。
///
/// 不透明な赤 + 完全に透明な緑の 2 画素を 1 ブロックで平均する。
/// ストレートアルファのまま平均していた頃は、リニア光の平均 0.5 が符号値 188 に
/// なって `[188, 188, 0, 128]` = くすんだ黄色が返っていた。プリマルチプライすれば
/// 色は赤のまま、アルファだけが半分になる(blur / resize と同じ規約)。
#[test]
fn pixelate_does_not_bleed_transparent_pixel_color() {
    let mut img = RgbaImage::new(2, 1);
    img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    img.put_pixel(1, 0, Rgba([0, 255, 0, 0]));
    let out = run(
        &img,
        r###"{"operations":[{"op":"pixelate","block_size":2},{"op":"encode","format":"png"}]}"###,
    );
    for x in 0..2 {
        let p = out.get_pixel(x, 0).0;
        assert_eq!([p[0], p[1], p[2]], [255, 0, 0], "pixel {x}: {p:?}");
        assert!(approx(p[3], 128, 1), "pixel {x} alpha: {p:?}");
    }
}

#[test]
fn pixelate_validate_rejects_block_size_out_of_range() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"pixelate","block_size":1}]}"###,
    );
    assert!(msg.contains("pixelate"), "{msg}");

    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"pixelate","block_size":257}]}"###,
    );
    assert!(msg.contains("pixelate"), "{msg}");
}

#[test]
fn pixelate_validate_rejects_zero_size_region() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"pixelate","block_size":4,"region":{"x":0,"y":0,"width":0,"height":4}}]}"###,
    );
    assert!(msg.contains("pixelate"), "{msg}");
}

// ---------------------------------------------------------------------------
// auto_levels
// ---------------------------------------------------------------------------

#[test]
fn auto_levels_stretches_low_contrast_image_to_full_range() {
    let mut img = RgbaImage::new(51, 1);
    for x in 0..51u32 {
        let v = 100 + x; // 100..=150
        img.put_pixel(x, 0, Rgba([v as u8, v as u8, v as u8, 255]));
    }
    let out = run(
        &img,
        r###"{"operations":[{"op":"auto_levels","clip_percent":0.0},{"op":"encode","format":"png"}]}"###,
    );
    let min = out.pixels().map(|p| p[0]).min().unwrap();
    let max = out.pixels().map(|p| p[0]).max().unwrap();
    assert!(min <= 2, "min after stretch should be near 0, got {min}");
    assert!(
        max >= 253,
        "max after stretch should be near 255, got {max}"
    );
}

#[test]
fn auto_levels_already_full_range_is_almost_unchanged() {
    let mut img = RgbaImage::new(256, 1);
    for x in 0..256u32 {
        img.put_pixel(x, 0, Rgba([x as u8, x as u8, x as u8, 255]));
    }
    let out = run(
        &img,
        r###"{"operations":[{"op":"auto_levels","clip_percent":0.5},{"op":"encode","format":"png"}]}"###,
    );
    // Small clip tolerance: most pixels should stay close to their original value
    // (interior values are barely affected by a 0.5% symmetric clip+stretch).
    let mid = out.get_pixel(128, 0);
    assert!(
        approx(mid[0], 128, 10),
        "mid pixel drifted too much: {:?}",
        mid.0
    );
}

#[test]
fn auto_levels_flat_image_is_byte_identical_guard() {
    let img = RgbaImage::from_pixel(4, 4, Rgba([77, 77, 77, 255]));
    let out = run(
        &img,
        r###"{"operations":[{"op":"auto_levels","clip_percent":0.5},{"op":"encode","format":"png"}]}"###,
    );
    assert_eq!(out, img, "flat image must be a no-op (hi<=lo guard)");
}

#[test]
fn auto_levels_per_channel_reduces_color_cast() {
    // R ranges 100..200, G ranges 50..150, B ranges 0..100: a strong color cast
    // with different per-channel means. per_channel=true stretches each channel
    // independently to 0..255, which should pull the channel means closer together.
    let mut img = RgbaImage::new(101, 1);
    for x in 0..101u32 {
        img.put_pixel(x, 0, Rgba([(100 + x) as u8, (50 + x) as u8, x as u8, 255]));
    }

    let mean = |im: &RgbaImage, c: usize| -> f64 {
        im.pixels().map(|p| p[c] as f64).sum::<f64>() / im.pixels().len() as f64
    };
    let before_spread = (mean(&img, 0) - mean(&img, 2)).abs();

    let out = run(
        &img,
        r###"{"operations":[{"op":"auto_levels","clip_percent":0.0,"per_channel":true},{"op":"encode","format":"png"}]}"###,
    );
    let after_spread = (mean(&out, 0) - mean(&out, 2)).abs();

    assert!(
        after_spread < before_spread,
        "per_channel auto_levels should reduce the R/B mean spread: before={before_spread}, after={after_spread}"
    );
}

#[test]
fn auto_levels_is_deterministic() {
    let mut img = RgbaImage::new(32, 32);
    for y in 0..32u32 {
        for x in 0..32u32 {
            img.put_pixel(
                x,
                y,
                Rgba([(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]),
            );
        }
    }
    let json = r###"{"operations":[{"op":"auto_levels","clip_percent":1.0,"per_channel":true},{"op":"encode","format":"png"}]}"###;
    let a = apply(&encode_png(&img), json).bytes;
    let b = apply(&encode_png(&img), json).bytes;
    assert_eq!(a, b, "auto_levels must be deterministic");
}

#[test]
fn auto_levels_validate_rejects_out_of_range_clip_percent() {
    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"auto_levels","clip_percent":10.1}]}"###,
    );
    assert!(msg.contains("auto_levels"), "{msg}");

    let msg = apply_err(
        FIXTURE,
        r###"{"operations":[{"op":"auto_levels","clip_percent":-0.1}]}"###,
    );
    assert!(msg.contains("auto_levels"), "{msg}");
}

// ---------------------------------------------------------------------------
// フルパイプラインのゴールデン(sha256)
// ---------------------------------------------------------------------------

/// auto_levels + gradient_map(デュオトーン)+ jpeg の一気通貫ゴールデン。
///
/// このハッシュは `tests/fixtures/synthetic_scene.jpg` の内容、
/// ops/auto_levels.rs・ops/gradient.rs の累算順序・丸め規則、および
/// jpeg エンコーダの挙動に依存する。フィクスチャや量子化規則、エンコーダの
/// バージョンを変えたら値を更新すること(`ENGINE_VERSION` を上げるべき変更かどうかも
/// 合わせて検討する)。
#[test]
fn full_pipeline_golden_auto_levels_gradient_map_jpeg() {
    let out = apply(
        FIXTURE,
        r###"{"operations":[
            {"op":"auto_orient"},
            {"op":"auto_levels","clip_percent":0.5},
            {"op":"gradient_map","stops":[
                {"position":0.0,"color":"#1a2a6c"},
                {"position":1.0,"color":"#fdbb2d"}
            ]},
            {"op":"encode","format":"jpeg","quality":90}
        ]}"###,
    );
    let hash = sha256_hex(&out.bytes);
    assert_eq!(
        hash, "9e627cc80026fb09023597a1da06cddcc224ca1cb7a0a45b6b997e84c105f403",
        "golden hash mismatch: full pipeline output changed (recompute and update if intentional)"
    );
}

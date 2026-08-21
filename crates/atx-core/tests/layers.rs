//! レイヤーグラフ(v0.6)のテスト。
//!
//! `ops::blend` は crate 非公開なので、合成の振る舞いは
//! `apply_recipe_with_assets` + モックの `AssetResolver` 経由で end-to-end に検証する
//! (recipe.rs の serde/validate + engine.rs のレイヤー合成 + ops/blend.rs の式を
//! 一括で回帰させられる)。W3C の separable ブレンド関数そのものの表駆動テストは
//! `src/ops/blend.rs` のユニットテストにある。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{Rgba, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

struct MockAssets(HashMap<String, Vec<u8>>);

impl MockAssets {
    fn new(pairs: &[(&str, Vec<u8>)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }
}

impl AssetResolver for MockAssets {
    fn read_revision(&self, revision_id: &str) -> Result<Vec<u8>> {
        self.0.get(revision_id).cloned().ok_or_else(|| {
            atx_core::AtxError::InvalidRecipe(format!("unknown revision {revision_id}"))
        })
    }
}

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode_rgba(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgba8()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn solid_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
    }
    encode_png(&img)
}

fn gray_png(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = f(x, y);
            img.put_pixel(x, y, Rgba([v, v, v, 255]));
        }
    }
    encode_png(&img)
}

/// 左半分が黒、右半分が白の PNG。
fn left_black_right_white(w: u32, h: u32) -> Vec<u8> {
    gray_png(w, h, |x, _| if x < w / 2 { 0 } else { 255 })
}

/// 中心が白、外周へ黒に落ちる放射グラデーション(マスク用)。
fn radial_mask(w: u32, h: u32) -> Vec<u8> {
    let cx = (w as f64 - 1.0) / 2.0;
    let cy = (h as f64 - 1.0) / 2.0;
    let r = cx.max(cy);
    gray_png(w, h, |x, y| {
        let dx = x as f64 - cx;
        let dy = y as f64 - cy;
        let d = (dx * dx + dy * dy).sqrt() / r;
        (((1.0 - d).clamp(0.0, 1.0)) * 255.0).round() as u8
    })
}

fn run(input: &[u8], json: &str, assets: &MockAssets) -> Vec<u8> {
    apply_recipe_with_assets(input, &recipe(json), &Limits::default(), assets)
        .expect("recipe should apply")
        .bytes
}

fn err(input: &[u8], json: &str, assets: &MockAssets) -> String {
    apply_recipe_with_assets(input, &recipe(json), &Limits::default(), assets)
        .expect_err("recipe should fail")
        .to_string()
}

fn validate_err(json: &str) -> String {
    let r = recipe(json);
    apply_recipe(SCENE, &r, &Limits::default())
        .expect_err("recipe should fail validation")
        .to_string()
}

// ---------------------------------------------------------------------------
// レシピ形状(atx-mcp との契約)
// ---------------------------------------------------------------------------

/// v0.1 から据え置きのゴールデン。layers 追加でレシピハッシュが動いていないこと。
#[test]
fn pinned_recipe_hash_is_unchanged_by_layer_support() {
    let r = recipe(
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"},
            {"op":"crop","aspect_ratio":"16:9","anchor":"center"},
            {"op":"resize","width":800,"fit":"cover"},
            {"op":"adjust","brightness":0.05,"contrast":0.02,"saturation":0.03,"sharpness":0.2},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    assert_eq!(
        atx_core::recipe_hash(&r).unwrap(),
        "884ea169e1027cf26d9140f6d2f7543904b2ca344667640f87820f528eaa175d"
    );
    let canonical = atx_core::canonical_json(&r).unwrap();
    assert!(
        !canonical.contains("\"layers\""),
        "a v1 recipe must not serialize a layers key: {canonical}"
    );
}

/// レイヤーを書いたときの正規化 JSON の形(atx-mcp が依存する契約)。
#[test]
fn layered_canonical_json_shape() {
    let r = recipe(
        r#"{
          "layers":[
            {"source":"base"},
            {"source":{"revision_id":"rev_tex1"},
             "ops":[{"op":"blur","sigma":2.0}],
             "mask":{"revision_id":"rev_m1","feather_px":4.0},
             "blend_mode":"multiply",
             "opacity":0.5}
          ],
          "operations":[{"op":"encode","format":"png"}]
        }"#,
    );
    let canonical = atx_core::canonical_json(&r).unwrap();
    assert_eq!(
        canonical,
        r#"{"layers":[{"blend_mode":"normal","opacity":1.0,"ops":[],"source":"base"},{"blend_mode":"multiply","mask":{"feather_px":4.0,"invert":false,"revision_id":"rev_m1"},"opacity":0.5,"ops":[{"op":"blur","sigma":2.0}],"source":{"revision_id":"rev_tex1"}}],"operations":[{"format":"png","op":"encode"}]}"#
    );
}

/// `source` の 2 形(文字列 "base" / オブジェクト {revision_id})が往復すること。
#[test]
fn layer_source_serde_roundtrip() {
    use atx_core::LayerSource;
    let base: LayerSource = serde_json::from_str(r#""base""#).unwrap();
    assert!(base.is_base());
    assert_eq!(serde_json::to_string(&base).unwrap(), r#""base""#);

    let rev: LayerSource = serde_json::from_str(r#"{"revision_id":"rev_x"}"#).unwrap();
    assert_eq!(rev.revision_id(), Some("rev_x"));
    assert_eq!(
        serde_json::to_string(&rev).unwrap(),
        r#"{"revision_id":"rev_x"}"#
    );
    assert_eq!(
        serde_json::to_string(&LayerSource::base()).unwrap(),
        r#""base""#
    );

    // 未知のキーは拒否される。
    assert!(serde_json::from_str::<LayerSource>(r#"{"revision":"rev_x"}"#).is_err());
    assert!(serde_json::from_str::<LayerSource>(r#""baseline""#).is_err());
}

/// レイヤーの未知フィールドは deny_unknown_fields で弾かれる。
#[test]
fn unknown_layer_field_is_rejected() {
    let e = serde_json::from_str::<TransformRecipe>(
        r#"{"layers":[{"source":"base","z_index":3}],"operations":[]}"#,
    )
    .expect_err("unknown field must be rejected");
    assert!(e.to_string().contains("z_index"), "{e}");
}

// ---------------------------------------------------------------------------
// validate マトリクス
// ---------------------------------------------------------------------------

#[test]
fn empty_layers_is_rejected() {
    let e = validate_err(r#"{"layers":[],"operations":[{"op":"encode","format":"png"}]}"#);
    assert!(e.contains("layers must not be empty"), "{e}");
}

#[test]
fn backdrop_must_not_carry_blend_mode_opacity_or_mask() {
    let e =
        validate_err(r#"{"layers":[{"source":"base","blend_mode":"multiply"}],"operations":[]}"#);
    assert!(
        e.contains("layers[0]") && e.contains("backdrop") && e.contains("normal"),
        "{e}"
    );

    let e = validate_err(r#"{"layers":[{"source":"base","opacity":0.5}],"operations":[]}"#);
    assert!(
        e.contains("layers[0]") && e.contains("opacity must be 1.0"),
        "{e}"
    );

    let e = validate_err(
        r#"{"layers":[{"source":"base","mask":{"revision_id":"rev_m"}}],"operations":[]}"#,
    );
    assert!(
        e.contains("layers[0]") && e.contains("must not carry a mask"),
        "{e}"
    );
}

#[test]
fn finishing_pass_only_ops_are_rejected_inside_layers() {
    let e = validate_err(
        r#"{"layers":[
             {"source":"base"},
             {"source":{"revision_id":"rev_a"},
              "ops":[{"op":"blur","sigma":1.0},{"op":"encode","format":"png"}]}
           ],"operations":[]}"#,
    );
    assert!(
        e.contains("layers[1].ops[1]") && e.contains("encode"),
        "{e}"
    );

    let e = validate_err(
        r#"{"layers":[
             {"source":"base"},
             {"source":{"revision_id":"rev_a"},"ops":[{"op":"strip_metadata"}]}
           ],"operations":[]}"#,
    );
    assert!(
        e.contains("layers[1].ops[0]") && e.contains("strip_metadata"),
        "{e}"
    );
}

#[test]
fn layer_opacity_must_be_within_zero_and_one() {
    for bad in ["1.5", "-0.1"] {
        let e = validate_err(&format!(
            r#"{{"layers":[{{"source":"base"}},
                 {{"source":{{"revision_id":"rev_a"}},"opacity":{bad}}}],"operations":[]}}"#
        ));
        assert!(
            e.contains("layers[1]") && e.contains("opacity must be within 0.0..=1.0"),
            "{e}"
        );
    }
}

#[test]
fn layer_source_revision_id_must_look_like_a_revision() {
    let e = validate_err(
        r#"{"layers":[{"source":"base"},{"source":{"revision_id":"abc"}}],"operations":[]}"#,
    );
    assert!(
        e.contains("layers[1] (source)") && e.contains("rev_"),
        "{e}"
    );
}

/// レイヤー内 op の通常のバリデーションも効き、レイヤー番号が名指しされる。
#[test]
fn layer_ops_are_validated_with_the_layer_index() {
    let e = validate_err(
        r#"{"layers":[{"source":"base"},
             {"source":{"revision_id":"rev_a"},"ops":[{"op":"blur","sigma":999.0}]}],
           "operations":[]}"#,
    );
    assert!(e.contains("layers[1].ops") && e.contains("sigma"), "{e}");
}

/// layers があるときに限り、トップレベル operations は空でよい。
#[test]
fn empty_operations_is_only_allowed_with_layers() {
    let e = validate_err(r#"{"operations":[]}"#);
    assert!(e.contains("operations must not be empty"), "{e}");

    let assets = MockAssets::new(&[]);
    let out = run(
        &solid_png(8, 8, [10, 20, 30, 255]),
        r#"{"layers":[{"source":"base"}],"operations":[]}"#,
        &assets,
    );
    let img = decode_rgba(&out);
    assert_eq!(img.get_pixel(0, 0).0, [10, 20, 30, 255]);
}

// ---------------------------------------------------------------------------
// 寸法ルール
// ---------------------------------------------------------------------------

#[test]
fn layer_dimension_mismatch_names_the_layer_and_both_dimensions() {
    let assets = MockAssets::new(&[("rev_small", solid_png(4, 4, [255, 0, 0, 255]))]);
    let e = err(
        &solid_png(16, 12, [0, 0, 0, 255]),
        r#"{"layers":[{"source":"base"},
             {"source":{"revision_id":"rev_small"},"blend_mode":"multiply"}],
           "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    assert!(e.contains("layers[1]"), "{e}");
    assert!(e.contains("4x4"), "{e}");
    assert!(e.contains("16x12"), "{e}");
    assert!(e.contains("resize") || e.contains("crop"), "{e}");
}

/// レイヤー内で resize すれば寸法を合わせられる(エラーメッセージの提案どおり)。
#[test]
fn layer_ops_can_resize_the_layer_to_the_backdrop() {
    let assets = MockAssets::new(&[("rev_small", solid_png(4, 4, [255, 255, 255, 255]))]);
    let out = run(
        &solid_png(16, 12, [0, 0, 0, 255]),
        r#"{"layers":[{"source":"base"},
             {"source":{"revision_id":"rev_small"},
              "ops":[{"op":"resize","width":16,"height":12,"fit":"fill","without_enlargement":false}]}],
           "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let img = decode_rgba(&out);
    assert_eq!(img.dimensions(), (16, 12));
    assert_eq!(img.get_pixel(8, 6).0, [255, 255, 255, 255]);
}

// ---------------------------------------------------------------------------
// 合成の数値(W3C 式の独立計算と突き合わせ)
// ---------------------------------------------------------------------------

/// backdrop = フィクスチャ(縮小)、レイヤー = 単色、multiply 50%。
/// 期待値は W3C の式をテスト内で f64 で独立に計算する。
#[test]
fn multiply_at_half_opacity_matches_the_w3c_formula() {
    // backdrop はレイヤーパイプラインを通した値そのものを基準にする。
    let assets = MockAssets::new(&[]);
    let backdrop = run(
        SCENE,
        r#"{"layers":[{"source":"base","ops":[{"op":"resize","width":64,"fit":"contain"}]}],
            "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let backdrop = decode_rgba(&backdrop);
    let (w, h) = backdrop.dimensions();

    const SOLID: [u8; 4] = [200, 100, 50, 255];
    let assets = MockAssets::new(&[("rev_solid", solid_png(w, h, SOLID))]);
    let out = run(
        SCENE,
        r#"{"layers":[
             {"source":"base","ops":[{"op":"resize","width":64,"fit":"contain"}]},
             {"source":{"revision_id":"rev_solid"},"blend_mode":"multiply","opacity":0.5}],
           "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let out = decode_rgba(&out);
    assert_eq!(out.dimensions(), (w, h));

    // αs = 1 × 0.5、αb = 1 → αo = 1、
    // Co = 0.5 × (1−1) × Cs + 0.5 × 1 × (Cb×Cs) + (1−0.5) × 1 × Cb
    //    = 0.5 × Cb × Cs + 0.5 × Cb
    for (x, y) in [(0u32, 0u32), (7, 5), (31, 17), (w - 1, h - 1), (13, h / 2)] {
        let cb = backdrop.get_pixel(x, y).0;
        let got = out.get_pixel(x, y).0;
        for c in 0..3 {
            let b = cb[c] as f64 / 255.0;
            let s = SOLID[c] as f64 / 255.0;
            let co = 0.5 * (b * s) + 0.5 * b;
            let want = (co * 255.0).round() as i32;
            assert!(
                (got[c] as i32 - want).abs() <= 1,
                "({x},{y}) ch{c}: got {} want {want} (Cb={} Cs={})",
                got[c],
                cb[c],
                SOLID[c]
            );
        }
        assert_eq!(got[3], 255);
    }
}

/// normal / opacity 1 / 不透明なレイヤーは「そのレイヤー単体」とバイト同一。
#[test]
fn normal_opaque_layer_equals_the_layer_alone() {
    let layer = gray_png(24, 16, |x, y| ((x * 7 + y * 11) % 256) as u8);
    let base = solid_png(24, 16, [3, 250, 128, 255]);
    let assets = MockAssets::new(&[("rev_layer", layer.clone())]);

    let composited = run(
        &base,
        r#"{"layers":[{"source":"base"},{"source":{"revision_id":"rev_layer"}}],
            "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let alone = apply_recipe(
        &layer,
        &recipe(r#"{"operations":[{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap()
    .bytes;
    assert_eq!(sha256_hex(&composited), sha256_hex(&alone));
}

/// opacity 0 は backdrop とバイト同一。
#[test]
fn zero_opacity_layer_is_backdrop_byte_identical() {
    let base = gray_png(24, 16, |x, y| ((x * 3 + y * 5) % 256) as u8);
    let assets = MockAssets::new(&[("rev_layer", solid_png(24, 16, [255, 0, 255, 255]))]);

    let with_layer = run(
        &base,
        r#"{"layers":[{"source":"base"},
             {"source":{"revision_id":"rev_layer"},"blend_mode":"screen","opacity":0.0}],
           "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let backdrop_only = run(
        &base,
        r#"{"layers":[{"source":"base"}],"operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    assert_eq!(sha256_hex(&with_layer), sha256_hex(&backdrop_only));
}

/// レイヤー内 op(blur)+ 合成マスクの組み合わせ。
/// マスク白の帯だけがレイヤーで置き換わり、黒の帯は backdrop がビット単位で残る。
#[test]
fn layer_with_ops_and_mask_composites_only_in_the_masked_zone() {
    const W: u32 = 32;
    const H: u32 = 32;
    let base = solid_png(W, H, [64, 64, 64, 255]);
    let assets = MockAssets::new(&[
        ("rev_edge", left_black_right_white(W, H)),
        (
            "rev_mask",
            gray_png(W, H, |_, y| if y < H / 2 { 255 } else { 0 }),
        ),
    ]);
    let out = run(
        &base,
        r#"{"layers":[
             {"source":"base"},
             {"source":{"revision_id":"rev_edge"},
              "ops":[{"op":"blur","sigma":3.0}],
              "mask":{"revision_id":"rev_mask"}}],
           "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    let img = decode_rgba(&out);

    // マスク 0 の下半分は backdrop のまま(ビット単位)。
    for y in H / 2..H {
        for x in 0..W {
            assert_eq!(img.get_pixel(x, y).0, [64, 64, 64, 255], "({x},{y})");
        }
    }
    // マスク 1 の上半分はレイヤー(ぼかし済みエッジ)そのもの。
    let row = 4;
    assert_eq!(img.get_pixel(0, row).0[0], 0, "far left must stay black");
    assert_eq!(
        img.get_pixel(W - 1, row).0[0],
        255,
        "far right must stay white"
    );
    // 境界付近はぼけて中間値になっている(= レイヤー内 op が効いている)。
    let mid = img.get_pixel(W / 2 - 1, row).0[0];
    assert!(
        mid > 0 && mid < 255,
        "blurred edge should be mid-tone, got {mid}"
    );
    // 上半分は x について単調非減少。
    for x in 1..W {
        assert!(
            img.get_pixel(x, row).0[0] >= img.get_pixel(x - 1, row).0[0],
            "not monotone at x={x}"
        );
    }
}

/// revision ソースが読めない/デコードできないときはレイヤー番号を名指しする。
#[test]
fn unresolvable_layer_source_names_the_layer() {
    let assets = MockAssets::new(&[]);
    let e = err(
        &solid_png(8, 8, [0, 0, 0, 255]),
        r#"{"layers":[{"source":"base"},{"source":{"revision_id":"rev_missing"}}],
            "operations":[{"op":"encode","format":"png"}]}"#,
        &assets,
    );
    assert!(
        e.contains("layers[1] (source)") && e.contains("rev_missing"),
        "{e}"
    );
}

/// 同じ入力・同じレシピは 2 回実行でバイト同一。
#[test]
fn layered_pipeline_is_deterministic() {
    let assets = three_layer_assets();
    let json = three_layer_recipe_json();
    let a = run(SCENE, &json, &assets);
    let b = run(SCENE, &json, &assets);
    assert_eq!(sha256_hex(&a), sha256_hex(&b));
}

// ---------------------------------------------------------------------------
// ゴールデン
// ---------------------------------------------------------------------------

/// 3 レイヤー(base / 単色 × 放射グラデーションマスク × screen / エッジ画像 × multiply)
/// + 仕上げパス(adjust + jpeg)。合成の挙動をバイト単位でピン留めする。
fn three_layer_recipe_json() -> String {
    r#"{
      "layers":[
        {"source":"base","ops":[{"op":"resize","width":320,"fit":"contain"}]},
        {"source":{"revision_id":"rev_glow"},
         "mask":{"revision_id":"rev_mask","feather_px":3.0},
         "blend_mode":"screen","opacity":0.75},
        {"source":{"revision_id":"rev_edge"},
         "ops":[{"op":"blur","sigma":2.0}],
         "blend_mode":"multiply","opacity":0.4}
      ],
      "operations":[
        {"op":"adjust","brightness":0.02,"contrast":0.03},
        {"op":"encode","format":"jpeg","quality":85}
      ]
    }"#
    .to_string()
}

/// ゴールデン用アセット。寸法は base レイヤーの resize 後(320 × 240)に合わせる。
fn three_layer_assets() -> MockAssets {
    const W: u32 = 320;
    const H: u32 = 240;
    MockAssets::new(&[
        ("rev_glow", solid_png(W, H, [255, 200, 120, 255])),
        ("rev_mask", radial_mask(W, H)),
        ("rev_edge", left_black_right_white(W, H)),
    ])
}

#[test]
fn golden_three_layer_composite_sha256() {
    let assets = three_layer_assets();
    let json = three_layer_recipe_json();
    let out = apply_recipe_with_assets(SCENE, &recipe(&json), &Limits::default(), &assets).unwrap();
    assert_eq!(out.width, 320);
    assert_eq!(out.height, 240);
    assert_eq!(out.mime_type, "image/jpeg");
    assert_eq!(
        sha256_hex(&out.bytes),
        "1c7dadf752c7f01268d4a87bf1f5c84b4f32ef616d395b792e7bc5b71631d47f"
    );
    assert_eq!(
        atx_core::recipe_hash(&recipe(&json)).unwrap(),
        "2db63d57ec216738fa97c52c76509ac1e42681829945b277a7a07e7d56c8297e"
    );
}

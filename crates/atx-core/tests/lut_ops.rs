//! lut(.cube 1D/3D)op のテスト(v0.3)。
//!
//! `ops` モジュールは crate 非公開のため、全て `apply_recipe_with_assets` +
//! モックの `AssetResolver` 経由で検証する(recipe.rs の validate + engine.rs の
//! dispatch + ops/lut.rs のパース・画素演算を一括で回帰させられる)。
//! パースエラーは `AtxError::Operation` にラップされるが、行番号付きの本文はそのまま届く。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{Rgba, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");
const IDENTITY_8: &str = include_str!("../../../tests/fixtures/identity_8.cube");
const WARM_8: &str = include_str!("../../../tests/fixtures/warm_8.cube");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

/// テスト用のインメモリ・アセットリゾルバ。
struct MockAssets(HashMap<String, Vec<u8>>);

impl MockAssets {
    fn new(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
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

/// プローブ画素を横 1 列に並べた画像。
fn probe_image(pixels: &[[u8; 4]]) -> RgbaImage {
    let mut img = RgbaImage::new(pixels.len() as u32, 1);
    for (x, p) in pixels.iter().enumerate() {
        img.put_pixel(x as u32, 0, Rgba(*p));
    }
    img
}

/// 網羅的ではないが偏りのないプローブ画素群(アルファも非 255 を混ぜる)。
fn probes() -> Vec<[u8; 4]> {
    let mut v = Vec::new();
    for r in [0u8, 1, 17, 36, 64, 128, 181, 219, 254, 255] {
        for g in [0u8, 36, 73, 128, 200, 255] {
            for b in [0u8, 36, 109, 128, 222, 255] {
                v.push([r, g, b, 200]);
            }
        }
    }
    v
}

fn lut_recipe(strength: Option<f64>) -> String {
    let s = match strength {
        Some(s) => format!(r#","strength":{s}"#),
        None => String::new(),
    };
    format!(
        r#"{{"operations":[{{"op":"lut","lut_revision_id":"rev_x"{s}}},
           {{"op":"encode","format":"png"}}]}}"#
    )
}

/// cube テキストを与えて画像へ適用し、結果の RGBA を返す(PNG 往復なので画素は無損失)。
fn apply_cube(img: &RgbaImage, cube: &str, strength: Option<f64>) -> RgbaImage {
    let assets = MockAssets::new(&[("rev_x", cube)]);
    let out = apply_recipe_with_assets(
        &encode_png(img),
        &recipe(&lut_recipe(strength)),
        &Limits::default(),
        &assets,
    )
    .expect("apply_recipe_with_assets should succeed");
    decode_rgba(&out.bytes)
}

/// cube テキストのパース/適用が失敗することを期待し、エラー文字列を返す。
fn cube_err(cube: &str) -> String {
    let assets = MockAssets::new(&[("rev_x", cube)]);
    apply_recipe_with_assets(
        &encode_png(&probe_image(&[[10, 20, 30, 255]])),
        &recipe(&lut_recipe(None)),
        &Limits::default(),
        &assets,
    )
    .expect_err("should fail")
    .to_string()
}

// ---------------------------------------------------------------------------
// validate(recipe レベル)
// ---------------------------------------------------------------------------

fn validate_err(json: &str) -> String {
    let assets = MockAssets::new(&[("rev_x", IDENTITY_8)]);
    apply_recipe_with_assets(
        &encode_png(&probe_image(&[[10, 20, 30, 255]])),
        &recipe(json),
        &Limits::default(),
        &assets,
    )
    .expect_err("should fail")
    .to_string()
}

#[test]
fn validate_accepts_reasonable_values() {
    let img = probe_image(&[[10, 20, 30, 255]]);
    for s in [Some(0.0), Some(0.5), Some(1.0), None] {
        apply_cube(&img, IDENTITY_8, s);
    }
}

#[test]
fn validate_rejects_empty_revision_id() {
    let e = validate_err(r#"{"operations":[{"op":"lut","lut_revision_id":""}]}"#);
    assert!(e.contains("operations[0]"), "{e}");
    assert!(e.contains("must not be empty"), "{e}");
}

#[test]
fn validate_rejects_revision_id_without_prefix() {
    let e = validate_err(r#"{"operations":[{"op":"lut","lut_revision_id":"abc123"}]}"#);
    assert!(e.contains("rev_"), "{e}");
}

#[test]
fn validate_rejects_out_of_range_strength() {
    for s in ["-0.001", "1.001"] {
        let json = format!(
            r#"{{"operations":[{{"op":"lut","lut_revision_id":"rev_x","strength":{s}}}]}}"#
        );
        let e = validate_err(&json);
        assert!(e.contains("strength"), "{e}");
        assert!(e.contains("operations[0]"), "{e}");
    }
}

// ---------------------------------------------------------------------------
// パース: エラー系(行番号)
// ---------------------------------------------------------------------------

#[test]
fn parse_rejects_data_before_size_line() {
    let e = cube_err("TITLE \"x\"\n0.0 0.0 0.0\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("before LUT_1D_SIZE"), "{e}");
}

#[test]
fn parse_rejects_missing_size_line() {
    let e = cube_err("# only a comment\n\n");
    assert!(e.contains("missing LUT_1D_SIZE"), "{e}");
}

#[test]
fn parse_rejects_both_size_lines() {
    let e = cube_err("LUT_1D_SIZE 2\nLUT_3D_SIZE 2\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("duplicate LUT size line"), "{e}");
}

#[test]
fn parse_rejects_size_line_after_data() {
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\n1 1 1\nLUT_3D_SIZE 2\n");
    assert!(e.contains("line 4"), "{e}");
    assert!(e.contains("duplicate LUT size line"), "{e}");
}

#[test]
fn parse_rejects_out_of_range_size() {
    let e = cube_err("LUT_3D_SIZE 1\n");
    assert!(e.contains("line 1") && e.contains("2..=129"), "{e}");
    let e = cube_err("LUT_3D_SIZE 130\n");
    assert!(e.contains("2..=129"), "{e}");
    let e = cube_err("LUT_1D_SIZE 65537\n");
    assert!(e.contains("2..=65536"), "{e}");
    let e = cube_err("LUT_1D_SIZE x\n");
    assert!(e.contains("line 1") && e.contains("not an integer"), "{e}");
}

#[test]
fn parse_rejects_wrong_data_count() {
    // 足りない
    let e = cube_err("LUT_1D_SIZE 3\n0 0 0\n1 1 1\n");
    assert!(e.contains("expected 3 data lines, got 2"), "{e}");

    // 多すぎる(超過した最初の行の行番号が出る)
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\n1 1 1\n0.5 0.5 0.5\n");
    assert!(e.contains("line 4"), "{e}");
    assert!(e.contains("too many data lines"), "{e}");

    // 1 行の要素数が違う
    let e = cube_err("LUT_1D_SIZE 2\n0 0\n1 1 1\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("expects 3 numbers"), "{e}");
}

#[test]
fn parse_rejects_non_finite_and_non_numeric() {
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\nnan 1 1\n");
    assert!(e.contains("line 3") && e.contains("not finite"), "{e}");
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\ninf 1 1\n");
    assert!(e.contains("line 3") && e.contains("not finite"), "{e}");
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\nzero 1 1\n");
    assert!(e.contains("line 3") && e.contains("not a number"), "{e}");
}

#[test]
fn parse_rejects_wild_data_values() {
    let e = cube_err("LUT_1D_SIZE 2\n0 0 0\n1000 1 1\n");
    assert!(e.contains("line 3"), "{e}");
    assert!(e.contains("outside the expected"), "{e}");
}

#[test]
fn parse_rejects_bad_domain() {
    // 両方指定した場合は後から来た行で検出される。
    let e = cube_err("LUT_3D_SIZE 2\nDOMAIN_MIN 0.5 0.0 0.0\nDOMAIN_MAX 0.4 1.0 1.0\n");
    assert!(e.contains("line 3"), "{e}");
    assert!(e.contains("DOMAIN_MIN must be smaller"), "{e}");

    // 既定の DOMAIN_MAX(1 1 1)に対しても検査する。
    let e = cube_err("LUT_3D_SIZE 2\nDOMAIN_MIN 1.0 0.0 0.0\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("DOMAIN_MIN must be smaller"), "{e}");

    let e = cube_err("LUT_3D_SIZE 2\nDOMAIN_MAX 0.0 1.0 1.0\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("DOMAIN_MIN must be smaller"), "{e}");

    let e = cube_err("LUT_3D_SIZE 2\nDOMAIN_MIN 0.0 0.0\n");
    assert!(e.contains("line 2"), "{e}");
    assert!(e.contains("expects 3 numbers"), "{e}");

    let e = cube_err("LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MIN 0 0 0\n");
    assert!(e.contains("line 3"), "{e}");
    assert!(e.contains("duplicate DOMAIN_MIN"), "{e}");
}

// ---------------------------------------------------------------------------
// パース: 正常系(コメント・空行・大小文字)
// ---------------------------------------------------------------------------

#[test]
fn parse_accepts_comments_blank_lines_and_inline_comments() {
    // 反転 1D LUT。コメント・空行・BOM・インラインコメントが混ざっていても通る。
    let text = "\u{feff}\n# leading comment\n\nTITLE \"t\"\nlut_1d_size 2\n\n\
                1.0 1.0 1.0 # inline comment\n0.0 0.0 0.0\n\n# trailing\n";
    let out = apply_cube(&probe_image(&[[0, 100, 255, 255]]), text, Some(1.0));
    assert_eq!(out.get_pixel(0, 0), &Rgba([255, 155, 0, 255]));
}

#[test]
fn parse_quantizes_values_to_1e6_grid() {
    // 0.1234564 と 0.1234561 は 1e-6 グリッドでは同一(0.123456)になる。
    let img = probe_image(&probes());
    let a = apply_cube(
        &img,
        "LUT_1D_SIZE 2\n0.1234564 0.1234564 0.1234564\n1 1 1\n",
        Some(1.0),
    );
    let b = apply_cube(
        &img,
        "LUT_1D_SIZE 2\n0.1234561 0.1234561 0.1234561\n1 1 1\n",
        Some(1.0),
    );
    assert_eq!(a.as_raw(), b.as_raw());
}

// ---------------------------------------------------------------------------
// 恒等・strength
// ---------------------------------------------------------------------------

#[test]
fn identity_cube_is_near_identity() {
    let img = probe_image(&probes());
    let out = apply_cube(&img, IDENTITY_8, Some(1.0));
    let mut max_dev = 0i32;
    for (a, b) in img.pixels().zip(out.pixels()) {
        assert_eq!(a[3], b[3], "alpha must be unchanged");
        for ch in 0..3 {
            max_dev = max_dev.max((a[ch] as i32 - b[ch] as i32).abs());
        }
    }
    assert!(max_dev <= 1, "identity LUT deviated by {max_dev}");
}

#[test]
fn identity_cube_is_exact_on_grid_aligned_inputs() {
    // size 8 の格子点は v01 = k/7。255*k/7 が整数になるのは k=0 と k=7 のみなので、
    // 補間を経ないことが保証されるこの 2 値でバイト一致を確認する。
    let mut pixels = Vec::new();
    for r in [0u8, 255] {
        for g in [0u8, 255] {
            for b in [0u8, 255] {
                pixels.push([r, g, b, 255]);
            }
        }
    }
    let img = probe_image(&pixels);
    let out = apply_cube(&img, IDENTITY_8, Some(1.0));
    assert_eq!(img.as_raw(), out.as_raw());
}

#[test]
fn strength_zero_is_byte_identical() {
    let img = probe_image(&probes());
    let out = apply_cube(&img, WARM_8, Some(0.0));
    assert_eq!(img.as_raw(), out.as_raw());
}

#[test]
fn strength_blends_between_original_and_full() {
    let img = probe_image(&[[120, 120, 120, 255]]);
    let full = apply_cube(&img, WARM_8, Some(1.0));
    let half = apply_cube(&img, WARM_8, Some(0.5));
    let (o, h, f) = (
        120i32,
        half.get_pixel(0, 0)[0] as i32,
        full.get_pixel(0, 0)[0] as i32,
    );
    assert!(o < h && h < f, "red: expected {o} < {h} < {f}");
    let (o, h, f) = (
        120i32,
        half.get_pixel(0, 0)[2] as i32,
        full.get_pixel(0, 0)[2] as i32,
    );
    assert!(o > h && h > f, "blue: expected {o} > {h} > {f}");
}

#[test]
fn strength_defaults_to_one() {
    let img = probe_image(&probes());
    let a = apply_cube(&img, WARM_8, Some(1.0));
    let b = apply_cube(&img, WARM_8, None);
    assert_eq!(a.as_raw(), b.as_raw());
}

#[test]
fn warm_cube_shifts_red_up_and_blue_down() {
    let img = probe_image(&probes());
    let out = apply_cube(&img, WARM_8, Some(1.0));
    for (a, b) in img.pixels().zip(out.pixels()) {
        assert!(b[0] >= a[0], "red must not drop: {a:?} -> {b:?}");
        assert!(b[2] <= a[2], "blue must not rise: {a:?} -> {b:?}");
        assert_eq!(a[3], b[3], "alpha must be unchanged");
        if a[0] == 128 {
            assert!(b[0] > a[0], "red should rise at 128: {a:?} -> {b:?}");
        }
        if a[2] == 128 {
            assert!(b[2] < a[2], "blue should fall at 128: {a:?} -> {b:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// 補間の数値
// ---------------------------------------------------------------------------

/// size 2 の 3D LUT。データ順は仕様どおり red が最速:
///   c000=(0,0,0) c100=(.9,.1,0) c010=(0,.8,.2) c110=(.7,.7,.1)
///   c001=(.1,0,.9) c101=(.8,.2,.8) c011=(.2,.6,1) c111=(1,1,1)
const TETRA_CUBE: &str = "\
LUT_3D_SIZE 2
0.0 0.0 0.0
0.9 0.1 0.0
0.0 0.8 0.2
0.7 0.7 0.1
0.1 0.0 0.9
0.8 0.2 0.8
0.2 0.6 1.0
1.0 1.0 1.0
";

/// 四面体補間の値を手計算値と突き合わせる。
///
/// 入力 (200,100,50) → dr=200/255 > dg=100/255 > db=50/255 なので四面体 R>G>B:
///   out = c000 + dr*(c100-c000) + dg*(c110-c100) + db*(c111-c110)
/// 手計算:
///   R = .9*dr - .2*dg + .3*db = (0.9*200 - 0.2*100 + 0.3*50)/255 = 175/255
///   G = .1*dr + .6*dg + .3*db = (0.1*200 + 0.6*100 + 0.3*50)/255 =  95/255
///   B =   0*dr + .1*dg + .9*db = (0.1*100 + 0.9*50)/255          =  55/255
#[test]
fn tetrahedral_matches_hand_computed_value() {
    let out = apply_cube(&probe_image(&[[200, 100, 50, 255]]), TETRA_CUBE, Some(1.0));
    assert_eq!(out.get_pixel(0, 0), &Rgba([175, 95, 55, 255]));
}

#[test]
fn tetrahedral_reproduces_corners_exactly() {
    let img = probe_image(&[[0, 0, 0, 255], [255, 0, 0, 255], [255, 255, 255, 255]]);
    let out = apply_cube(&img, TETRA_CUBE, Some(1.0));
    assert_eq!(out.get_pixel(0, 0), &Rgba([0, 0, 0, 255]));
    // c100 = (0.9, 0.1, 0.0) → 229.5 → 230(half-away-from-zero)/ 25.5 → 26 / 0
    assert_eq!(out.get_pixel(1, 0), &Rgba([230, 26, 0, 255]));
    assert_eq!(out.get_pixel(2, 0), &Rgba([255, 255, 255, 255]));
}

#[test]
fn lut_1d_inverts_linearly() {
    let cube = "LUT_1D_SIZE 2\n1 1 1\n0 0 0\n";
    let img = probe_image(&[[0, 100, 255, 128], [64, 128, 192, 255]]);
    let out = apply_cube(&img, cube, Some(1.0));
    assert_eq!(out.get_pixel(0, 0), &Rgba([255, 155, 0, 128]));
    assert_eq!(out.get_pixel(1, 0), &Rgba([191, 127, 63, 255]));
}

#[test]
fn lut_1d_interpolates_between_entries() {
    // 3 エントリ: 0.0 / 0.25 / 1.0。
    let cube = "LUT_1D_SIZE 3\n0 0 0\n0.25 0.25 0.25\n1 1 1\n";
    let out = apply_cube(&probe_image(&[[128, 64, 192, 255]]), cube, Some(1.0));
    let px = *out.get_pixel(0, 0);
    // R: v01 = 128/255 ≈ 0.50196 → 格子 1 のすぐ上 → ≈ 0.25294 → 64.5 → 65
    assert_eq!(px[0], 65);
    // G: v01 = 64/255 ≈ 0.25098 → 0.50196*0.25 ≈ 0.12549 → 32
    assert_eq!(px[1], 32);
    // B: v01 = 192/255 ≈ 0.75294 → 0.25 + 0.50588*0.75 ≈ 0.62941 → 161
    assert_eq!(px[2], 161);
}

#[test]
fn domain_narrows_the_input_range() {
    // domain 0.25..0.75 → 入力 0.25 以下は下端、0.75 以上は上端へ張り付く。
    let cube =
        "LUT_1D_SIZE 2\nDOMAIN_MIN 0.25 0.25 0.25\nDOMAIN_MAX 0.75 0.75 0.75\n0 0 0\n1 1 1\n";
    let img = probe_image(&[[0, 64, 255, 255], [160, 128, 0, 255]]);
    let out = apply_cube(&img, cube, Some(1.0));
    assert_eq!(out.get_pixel(0, 0)[0], 0);
    // G=64 → (64/255-0.25)/0.5 ≈ 0.00196 → 0.5 → 1(half-away-from-zero)
    assert_eq!(out.get_pixel(0, 0)[1], 1);
    assert_eq!(out.get_pixel(0, 0)[2], 255);
    // domain 内は 2v-127.5 に写る: 160 → 192.5 → 193(half-away-from-zero)
    assert_eq!(out.get_pixel(1, 0)[0], 193);
}

// ---------------------------------------------------------------------------
// 決定論
// ---------------------------------------------------------------------------

#[test]
fn lut_is_deterministic() {
    let assets = MockAssets::new(&[("rev_x", WARM_8)]);
    let json = lut_recipe(Some(0.63));
    let input = encode_png(&probe_image(&probes()));
    let a = apply_recipe_with_assets(&input, &recipe(&json), &Limits::default(), &assets).unwrap();
    let b = apply_recipe_with_assets(&input, &recipe(&json), &Limits::default(), &assets).unwrap();
    assert_eq!(a.bytes, b.bytes, "output bytes must be identical");
    assert_eq!((a.width, a.height), (b.width, b.height));
}

// ---------------------------------------------------------------------------
// アセット解決のエラー経路
// ---------------------------------------------------------------------------

#[test]
fn missing_resolver_errors_and_mentions_revision_id() {
    let json = lut_recipe(None);
    let input = encode_png(&probe_image(&[[10, 20, 30, 255]]));
    let e = apply_recipe(&input, &recipe(&json), &Limits::default())
        .unwrap_err()
        .to_string();
    assert!(e.contains("rev_x"), "{e}");
    assert!(e.contains("lut"), "{e}");
}

#[test]
fn unknown_revision_errors_through_resolver() {
    let assets = MockAssets::new(&[("rev_other", WARM_8)]);
    let input = encode_png(&probe_image(&[[10, 20, 30, 255]]));
    let e = apply_recipe_with_assets(
        &input,
        &recipe(&lut_recipe(None)),
        &Limits::default(),
        &assets,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("rev_x"), "{e}");
}

#[test]
fn non_utf8_asset_errors() {
    let assets = MockAssets(HashMap::from([(
        "rev_x".to_string(),
        vec![0xff, 0xfe, 0x00, 0x01],
    )]));
    let input = encode_png(&probe_image(&[[10, 20, 30, 255]]));
    let e = apply_recipe_with_assets(
        &input,
        &recipe(&lut_recipe(None)),
        &Limits::default(),
        &assets,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("not a text .cube file"), "{e}");
}

// ---------------------------------------------------------------------------
// ゴールデン
// ---------------------------------------------------------------------------

/// synthetic_scene.jpg + warm_8.cube(strength 0.75)+ JPEG q90 のバイト列を固定する。
/// **意図的なピン留め**: この値が変わるということは LUT のパース・補間・丸め・
/// エンコード経路のいずれかが変わったということ。挙動を意図して変えたときのみ更新する。
#[test]
fn golden_warm_lut_jpeg() {
    let assets = MockAssets::new(&[("rev_x", WARM_8)]);
    let json = r#"{"operations":[
        {"op":"lut","lut_revision_id":"rev_x","strength":0.75},
        {"op":"encode","format":"jpeg","quality":90}]}"#;
    let out = apply_recipe_with_assets(SCENE, &recipe(json), &Limits::default(), &assets)
        .expect("should succeed");
    assert_eq!(
        sha256_hex(&out.bytes),
        // v2 (f32 linear) golden; v1 value was 181bd614df27d30e970e928391d2090e52ab7d3583ab1311da74a52a73d9708e
        "18fa1d0a75ad649c8d7fb385abe86329114b77bb98834983ed34ced1e95b1d6f",
        "lut golden changed"
    );
}

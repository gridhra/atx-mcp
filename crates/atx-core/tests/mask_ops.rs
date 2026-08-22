//! 局所適用マスク(v0.5)のテスト。
//!
//! `ops::mask` は crate 非公開なので、全て `apply_recipe_with_assets` +
//! モックの `AssetResolver` 経由で end-to-end に検証する
//! (recipe.rs の serde/validate + engine.rs の汎用ブレンド + ops/mask.rs の
//! 重み解決を一括で回帰させられる)。

use std::collections::HashMap;

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe_with_assets, AssetResolver, Limits, Result};
use image::{Rgba, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

/// テスト用のインメモリ・アセットリゾルバ(バイト列をそのまま返す)。
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

/// グレースケール PNG を作る(`f(x, y)` が 0..=255 の重み)。
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

fn solid_mask(w: u32, h: u32, v: u8) -> Vec<u8> {
    gray_png(w, h, |_, _| v)
}

/// 左半分が白、右半分が黒のマスク。
fn left_half_mask(w: u32, h: u32) -> Vec<u8> {
    gray_png(w, h, |x, _| if x < w / 2 { 255 } else { 0 })
}

/// 中心が白、外周へ向かって黒に落ちる放射グラデーション(ゴールデン用)。
fn radial_mask(size: u32) -> Vec<u8> {
    let c = (size as f64 - 1.0) / 2.0;
    gray_png(size, size, |x, y| {
        let dx = x as f64 - c;
        let dy = y as f64 - c;
        let d = (dx * dx + dy * dy).sqrt() / c;
        let v = (1.0 - d).clamp(0.0, 1.0) * 255.0;
        v.round() as u8
    })
}

/// 一様グレーの入力画像(マスクの重みがそのまま出力値に現れる)。
fn gray_input(w: u32, h: u32, v: u8) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba([v, v, v, 255]);
    }
    encode_png(&img)
}

/// master カーブ(暗部を持ち上げる)+ png。`mask` は JSON 断片(先頭カンマ込み)。
fn curves_recipe(mask: &str) -> String {
    format!(
        r#"{{"operations":[
             {{"op":"curves","master":[[0,0],[128,200],[255,255]]{mask}}},
             {{"op":"encode","format":"png"}}]}}"#
    )
}

fn blur_recipe(mask: &str) -> String {
    format!(
        r#"{{"operations":[
             {{"op":"blur","sigma":4.0{mask}}},
             {{"op":"encode","format":"png"}}]}}"#
    )
}

fn mask_json(id: &str, invert: bool, feather: f64) -> String {
    format!(r#","mask":{{"revision_id":"{id}","invert":{invert},"feather_px":{feather}}}"#)
}

fn run(input: &[u8], recipe_json: &str, assets: &[(&str, Vec<u8>)]) -> Vec<u8> {
    apply_recipe_with_assets(
        input,
        &recipe(recipe_json),
        &Limits::default(),
        &MockAssets::new(assets),
    )
    .expect("apply_recipe_with_assets should succeed")
    .bytes
}

fn run_err(input: &[u8], recipe_json: &str, assets: &[(&str, Vec<u8>)]) -> String {
    apply_recipe_with_assets(
        input,
        &recipe(recipe_json),
        &Limits::default(),
        &MockAssets::new(assets),
    )
    .expect_err("should fail")
    .to_string()
}

/// 入力バイト列をそのままパイプラインへ通しただけの参照出力(空間変換を挟まない
/// レシピなので画素は無損失)。
fn passthrough(input: &[u8]) -> RgbaImage {
    decode_rgba(&run(
        input,
        r#"{"operations":[{"op":"encode","format":"png"}]}"#,
        &[],
    ))
}

// ---------------------------------------------------------------------------
// 1. ハッシュ安定性(既存レシピのバイト列を 1 ビットも動かさない)
// ---------------------------------------------------------------------------

#[test]
fn maskless_recipe_canonical_json_has_no_mask_key() {
    let r = recipe(
        r#"{"operations":[
             {"op":"adjust","brightness":0.05},
             {"op":"curves","master":[[0,0],[255,255]]},
             {"op":"blur","sigma":2.0},
             {"op":"median","radius":1},
             {"op":"unsharp_mask","amount":0.5,"radius":1.0},
             {"op":"convolve","kernel":[0,0,0,0,1,0,0,0,0],"size":3},
             {"op":"levels","in_black":5},
             {"op":"white_balance","temperature":10},
             {"op":"hsl","red":{"hue":5}},
             {"op":"color_matrix","matrix":[1,0,0,0,0,0,1,0,0,0,0,0,1,0,0,0,0,0,1,0]},
             {"op":"encode","format":"png"}]}"#,
    );
    let canonical = atx_core::canonical_json(&r).unwrap();
    assert!(
        !canonical.contains("\"mask\""),
        "maskless recipe must not serialize a mask key: {canonical}"
    );
}

/// v0.1 から据え置きのゴールデン。マスク追加でレシピハッシュが動いていないこと。
#[test]
fn pinned_recipe_hash_is_unchanged_by_mask_support() {
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
}

/// マスクを書いたときの正規化 JSON の形(atx-mcp が依存する契約)。
#[test]
fn mask_canonical_json_shape() {
    let r = recipe(
        r#"{"operations":[
             {"op":"curves","master":[[0,0],[255,255]],"mask":{"revision_id":"rev_m1","feather_px":4.0}},
             {"op":"encode","format":"png"}]}"#,
    );
    let canonical = atx_core::canonical_json(&r).unwrap();
    assert_eq!(
        canonical,
        r#"{"operations":[{"mask":{"feather_px":4.0,"invert":false,"revision_id":"rev_m1"},"master":[[0,0],[255,255]],"op":"curves"},{"format":"png","op":"encode"}]}"#
    );
}

#[test]
fn mask_rejects_unknown_fields() {
    let err = serde_json::from_str::<TransformRecipe>(
        r#"{"operations":[{"op":"curves","mask":{"revision_id":"rev_m","opacity":0.5}}]}"#,
    )
    .expect_err("unknown mask field must be rejected");
    assert!(err.to_string().contains("opacity"), "{err}");
}

/// 幾何 op / encode / strip はマスクを受け付けない(deny_unknown_fields)。
#[test]
fn geometry_ops_do_not_accept_a_mask() {
    for json in [
        r#"{"operations":[{"op":"resize","width":10,"mask":{"revision_id":"rev_m"}}]}"#,
        r#"{"operations":[{"op":"rotate","angle_degrees":1,"mask":{"revision_id":"rev_m"}}]}"#,
        r#"{"operations":[{"op":"encode","format":"png","mask":{"revision_id":"rev_m"}}]}"#,
    ] {
        let err = serde_json::from_str::<TransformRecipe>(json)
            .expect_err("mask must not be accepted here");
        assert!(err.to_string().contains("mask"), "{err}");
    }
}

// ---------------------------------------------------------------------------
// 2. 端点(全白 = マスク無し、全黒 = 恒等)
// ---------------------------------------------------------------------------

#[test]
fn full_white_mask_equals_the_unmasked_op() {
    let input = gray_input(48, 16, 128);
    let masked = run(
        &input,
        &curves_recipe(&mask_json("rev_white", false, 0.0)),
        &[("rev_white", solid_mask(48, 16, 255))],
    );
    let plain = run(&input, &curves_recipe(""), &[]);
    assert_eq!(
        sha256_hex(&masked),
        sha256_hex(&plain),
        "a full-white mask must be byte-identical to the unmasked op"
    );
}

#[test]
fn full_black_mask_is_a_no_op() {
    let input = gray_input(48, 16, 128);
    let masked = run(
        &input,
        &curves_recipe(&mask_json("rev_black", false, 0.0)),
        &[("rev_black", solid_mask(48, 16, 0))],
    );
    assert_eq!(decode_rgba(&masked), passthrough(&input));
}

#[test]
fn invert_swaps_the_two_endpoints() {
    let input = gray_input(48, 16, 128);
    let plain = run(&input, &curves_recipe(""), &[]);

    // 全黒 + invert = 全白
    let inverted_black = run(
        &input,
        &curves_recipe(&mask_json("rev_black", true, 0.0)),
        &[("rev_black", solid_mask(48, 16, 0))],
    );
    assert_eq!(sha256_hex(&inverted_black), sha256_hex(&plain));

    // 全白 + invert = 恒等
    let inverted_white = run(
        &input,
        &curves_recipe(&mask_json("rev_white", true, 0.0)),
        &[("rev_white", solid_mask(48, 16, 255))],
    );
    assert_eq!(decode_rgba(&inverted_white), passthrough(&input));
}

// ---------------------------------------------------------------------------
// 3. 部分適用(左半分)
// ---------------------------------------------------------------------------

#[test]
fn left_half_mask_only_changes_the_left_half() {
    let (w, h) = (48u32, 16u32);
    let input = gray_input(w, h, 128);
    let base = passthrough(&input);
    let full = decode_rgba(&run(&input, &curves_recipe(""), &[]));

    let out = decode_rgba(&run(
        &input,
        &curves_recipe(&mask_json("rev_half", false, 0.0)),
        &[("rev_half", left_half_mask(w, h))],
    ));

    for y in 0..h {
        for x in 0..w {
            let expected = if x < w / 2 { &full } else { &base };
            assert_eq!(
                out.get_pixel(x, y),
                expected.get_pixel(x, y),
                "pixel ({x}, {y})"
            );
        }
    }
    // 左右で実際に違いが出ていること(テストが自明に通らないことの担保)。
    assert_ne!(out.get_pixel(0, 0), out.get_pixel(w - 1, 0));
}

#[test]
fn invert_flips_which_half_changes() {
    let (w, h) = (48u32, 16u32);
    let input = gray_input(w, h, 128);
    let base = passthrough(&input);
    let full = decode_rgba(&run(&input, &curves_recipe(""), &[]));

    let out = decode_rgba(&run(
        &input,
        &curves_recipe(&mask_json("rev_half", true, 0.0)),
        &[("rev_half", left_half_mask(w, h))],
    ));

    for y in 0..h {
        for x in 0..w {
            let expected = if x < w / 2 { &base } else { &full };
            assert_eq!(
                out.get_pixel(x, y),
                expected.get_pixel(x, y),
                "pixel ({x}, {y})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. フェザ(境界に中間の重みが生まれる)
// ---------------------------------------------------------------------------

#[test]
fn feather_creates_a_monotone_transition_band() {
    let (w, h) = (64u32, 16u32);
    let input = gray_input(w, h, 128);
    let base = passthrough(&input).get_pixel(0, 0).0[0];
    let full = decode_rgba(&run(&input, &curves_recipe(""), &[]))
        .get_pixel(0, 0)
        .0[0];
    assert!(full > base, "the curve must brighten the probe value");

    let hard = decode_rgba(&run(
        &input,
        &curves_recipe(&mask_json("rev_half", false, 0.0)),
        &[("rev_half", left_half_mask(w, h))],
    ));
    let soft = decode_rgba(&run(
        &input,
        &curves_recipe(&mask_json("rev_half", false, 6.0)),
        &[("rev_half", left_half_mask(w, h))],
    ));

    // 硬いマスクは 2 値、フェザ付きは境界に中間値が並ぶ。
    let row: Vec<u8> = (0..w).map(|x| soft.get_pixel(x, 0).0[0]).collect();
    let hard_row: Vec<u8> = (0..w).map(|x| hard.get_pixel(x, 0).0[0]).collect();
    assert!(
        hard_row.iter().all(|v| *v == base || *v == full),
        "unfeathered mask must be binary: {hard_row:?}"
    );

    // 単調減少(左 = 適用、右 = 非適用)。
    for pair in row.windows(2) {
        assert!(pair[1] <= pair[0], "row must be monotone: {row:?}");
    }
    assert_eq!(row[0], full, "far left stays fully applied");
    assert_eq!(row[(w - 1) as usize], base, "far right stays untouched");

    // 境界(x = 32)の周りに厳密な中間値が複数並ぶ = 遷移帯がある。
    let band = row.iter().filter(|v| **v > base && **v < full).count();
    assert!(band >= 8, "expected a transition band, got {band}: {row:?}");

    // フェザなしとは違う結果であること。
    assert_ne!(row, hard_row);
}

// ---------------------------------------------------------------------------
// 5. 自動リサイズ / 作業空間 / 決定論
// ---------------------------------------------------------------------------

/// 32x32 のマスクを 1477x1108 のフィクスチャへ適用できる(双線形で拡大される)。
#[test]
fn mask_is_resized_to_the_current_image() {
    let base = passthrough(SCENE);
    assert_eq!(base.dimensions(), (1477, 1108));

    let out = decode_rgba(&run(
        SCENE,
        &curves_recipe(&mask_json("rev_small", false, 0.0)),
        &[("rev_small", left_half_mask(32, 32))],
    ));
    let full = decode_rgba(&run(SCENE, &curves_recipe(""), &[]));

    assert_eq!(out.dimensions(), base.dimensions());
    // 左端は完全適用、右端は完全非適用(拡大でできるランプは境界付近だけ)。
    for y in [0u32, 500, 1107] {
        assert_eq!(
            out.get_pixel(4, y),
            full.get_pixel(4, y),
            "left edge, y={y}"
        );
        assert_eq!(
            out.get_pixel(1470, y),
            base.get_pixel(1470, y),
            "right edge, y={y}"
        );
    }
}

/// 線形空間の op(blur)でも sRGB 空間の op(curves)でもマスクが正しく効く。
#[test]
fn mask_works_in_both_working_spaces() {
    let (w, h) = (64u32, 32u32);
    // 縦縞のあるカラー画像(blur で必ず変化する)。
    let mut img = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = if (x / 4) % 2 == 0 { 30u8 } else { 220 };
            img.put_pixel(x, y, Rgba([v, 255 - v, 128, 255]));
        }
    }
    let input = encode_png(&img);
    let base = passthrough(&input);

    for make in [blur_recipe as fn(&str) -> String, curves_recipe] {
        let plain = run(&input, &make(""), &[]);

        // 全白 = マスク無しとバイト同一。
        let white = run(
            &input,
            &make(&mask_json("rev_white", false, 0.0)),
            &[("rev_white", solid_mask(w, h, 255))],
        );
        assert_eq!(sha256_hex(&white), sha256_hex(&plain));

        // 全黒 = 恒等。
        let black = run(
            &input,
            &make(&mask_json("rev_black", false, 0.0)),
            &[("rev_black", solid_mask(w, h, 0))],
        );
        assert_eq!(decode_rgba(&black), base);

        // 右半分だけ変化する(左半分は入力どおり)。
        let half = decode_rgba(&run(
            &input,
            &make(&mask_json("rev_half", true, 0.0)),
            &[("rev_half", left_half_mask(w, h))],
        ));
        let full = decode_rgba(&plain);
        assert_eq!(half.get_pixel(2, 2), base.get_pixel(2, 2));
        assert_eq!(half.get_pixel(w - 3, 2), full.get_pixel(w - 3, 2));
        assert_ne!(base.get_pixel(w - 3, 2), full.get_pixel(w - 3, 2));
    }
}

/// 同じマスクを複数 op で共有しても(= キャッシュを経由しても)結果は同じで、
/// 2 回実行はバイト同一。
#[test]
fn masked_pipeline_is_deterministic_and_cache_safe() {
    let json = format!(
        r#"{{"operations":[
             {{"op":"curves","master":[[0,0],[128,200],[255,255]]{m}}},
             {{"op":"blur","sigma":3.0{m}}},
             {{"op":"white_balance","temperature":30{m}}},
             {{"op":"encode","format":"png"}}]}}"#,
        m = mask_json("rev_r", false, 3.0)
    );
    let assets = [("rev_r", radial_mask(48))];
    let a = run(SCENE, &json, &assets);
    let b = run(SCENE, &json, &assets);
    assert_eq!(sha256_hex(&a), sha256_hex(&b));
}

// ---------------------------------------------------------------------------
// 6. エラー(静的 validate と実行時の解決失敗)
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_a_bad_revision_id() {
    let msg = run_err(
        &gray_input(8, 8, 128),
        &curves_recipe(r#","mask":{"revision_id":"mask_1"}"#),
        &[],
    );
    assert!(msg.contains("operations[0]"), "{msg}");
    assert!(msg.contains("mask"), "{msg}");
    assert!(msg.contains("rev_"), "{msg}");

    let msg = run_err(
        &gray_input(8, 8, 128),
        &curves_recipe(r#","mask":{"revision_id":""}"#),
        &[],
    );
    assert!(msg.contains("must not be empty"), "{msg}");
}

#[test]
fn validate_rejects_feather_out_of_range() {
    for feather in ["-1.0", "200.5", "1e9"] {
        let msg = run_err(
            &gray_input(8, 8, 128),
            &curves_recipe(&format!(
                r#","mask":{{"revision_id":"rev_m","feather_px":{feather}}}"#
            )),
            &[],
        );
        assert!(msg.contains("feather_px"), "{feather}: {msg}");
        assert!(msg.contains("0.0..=200"), "{feather}: {msg}");
    }
}

#[test]
fn unknown_mask_revision_reports_the_id() {
    let msg = run_err(
        &gray_input(8, 8, 128),
        &curves_recipe(&mask_json("rev_missing", false, 0.0)),
        &[("rev_other", solid_mask(4, 4, 255))],
    );
    assert!(msg.contains("rev_missing"), "{msg}");
    assert!(msg.contains("operation 0"), "{msg}");
    assert!(msg.contains("curves"), "{msg}");
}

#[test]
fn undecodable_mask_reports_the_id() {
    let msg = run_err(
        &gray_input(8, 8, 128),
        &curves_recipe(&mask_json("rev_bad", false, 0.0)),
        &[("rev_bad", b"not an image at all".to_vec())],
    );
    assert!(msg.contains("rev_bad"), "{msg}");
    assert!(msg.contains("image"), "{msg}");
}

/// 回帰: マスク画像のデコードにも `Limits` が効く。
///
/// 以前は `image::load_from_memory` を無検査で呼んでいたため、本体の入力には
/// 上限があるのに **マスク経由なら任意サイズのアセットをデコードできた**
/// (デコード爆弾の抜け道)。今は入力画像と同じ検査を通し、超過は
/// `AtxError::LimitExceeded` として構造化されたまま返る。
#[test]
fn oversized_mask_bytes_hit_the_limit_instead_of_allocating() {
    let input = gray_input(8, 8, 128);
    // 8x8 の入力より大きく、かつバイト上限を跨ぐマスク(ノイズ入りで圧縮を効かせない)。
    let mask = gray_png(256, 256, |x, y| {
        (x.wrapping_mul(37) ^ y.wrapping_mul(101)) as u8
    });
    assert!(
        input.len() < mask.len(),
        "the fixture must fit under the cap"
    );
    let limits = Limits {
        max_bytes: (mask.len() - 1) as u64,
        ..Limits::default()
    };

    let err = apply_recipe_with_assets(
        &input,
        &recipe(&curves_recipe(&mask_json("rev_big", false, 0.0))),
        &limits,
        &MockAssets::new(&[("rev_big", mask.clone())]),
    )
    .expect_err("an over-limit mask must be rejected");
    assert!(
        matches!(err, atx_core::AtxError::LimitExceeded(_)),
        "expected a structural limit error, got {err:?}"
    );

    // 画素数の上限でも同じ(ヘッダの寸法だけで弾ける = フルデコードしない)。
    let limits = Limits {
        max_pixels: 1_000,
        ..Limits::default()
    };
    let err = apply_recipe_with_assets(
        &input,
        &recipe(&curves_recipe(&mask_json("rev_big", false, 0.0))),
        &limits,
        &MockAssets::new(&[("rev_big", mask)]),
    )
    .expect_err("an over-limit mask must be rejected");
    assert!(
        matches!(err, atx_core::AtxError::LimitExceeded(_)),
        "expected a structural limit error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. ゴールデン(フィクスチャ + 放射マスク + curves + jpeg)
// ---------------------------------------------------------------------------

#[test]
fn golden_radial_masked_curves_jpeg() {
    let json = format!(
        r#"{{"operations":[
             {{"op":"curves","master":[[0,0],[64,110],[128,190],[255,255]]{m}}},
             {{"op":"encode","format":"jpeg","quality":85}}]}}"#,
        m = mask_json("rev_radial", false, 5.0)
    );
    let mask = radial_mask(64);
    // マスク画像自体もゴールデンで固定する(生成器が動いたら気づけるように)。
    assert_eq!(
        sha256_hex(&mask),
        "9004e7f9ea34d577cda16617c5c470c25097ade98a5294e5431779051a91ed89"
    );
    let out = run(SCENE, &json, &[("rev_radial", mask)]);
    assert_eq!(
        sha256_hex(&out),
        "224c0e1b76137b7bc3283079998ac5496740adf39c68d9d670aab0cea6eeb770"
    );
}

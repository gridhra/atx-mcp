//! レシピ正規化・ハッシュ・検証のテスト(DESIGN §6「レシピ正規化」)。

use atx_core::recipe::{self, TransformRecipe};

fn parse(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

/// 意味的に同一なレシピ(キー順・デフォルト値の省略・null 明示・空白の違い)は
/// 正規化 JSON もハッシュも一致する。
#[test]
fn semantically_identical_recipes_hash_equal() {
    let a = parse(
        r#"{
          "operations": [
            { "op": "rotate", "angle_degrees": -1.8, "crop": "largest_inscribed_rect" },
            { "op": "crop", "aspect_ratio": "16:9", "anchor": "center", "mode": "crop", "rect": null, "pad_color": null },
            { "op": "resize", "width": 1600, "fit": "cover", "without_enlargement": true },
            { "op": "encode", "format": "webp", "quality": 82 }
          ]
        }"#,
    );
    // 同じ意味だが: キーの順序が違う / デフォルト値を省略 / 整形が違う
    let b = parse(
        r#"{"operations":[
            {"crop":"largest_inscribed_rect","angle_degrees":-1.8,"op":"rotate"},
            {"aspect_ratio":"16:9","op":"crop"},
            {"fit":"cover","op":"resize","width":1600},
            {"quality":82,"op":"encode","format":"webp"}
        ]}"#,
    );

    assert_eq!(
        recipe::canonical_json(&a).unwrap(),
        recipe::canonical_json(&b).unwrap()
    );
    assert_eq!(
        recipe::recipe_hash(&a).unwrap(),
        recipe::recipe_hash(&b).unwrap()
    );
}

/// 正規化 JSON はキーが全階層で辞書順に並ぶ。
#[test]
fn canonical_json_sorts_keys_at_every_level() {
    let r = parse(r#"{"operations":[{"op":"resize","width":100,"fit":"contain"}]}"#);
    let canonical = recipe::canonical_json(&r).unwrap();
    assert_eq!(
        canonical,
        r#"{"operations":[{"fit":"contain","op":"resize","width":100,"without_enlargement":true}]}"#
    );
}

/// ハッシュは sha256 hex(小文字 64 桁)。
#[test]
fn recipe_hash_is_lowercase_sha256_hex() {
    use sha2::{Digest, Sha256};
    let r = parse(r#"{"operations":[{"op":"auto_orient"}]}"#);
    let hash = recipe::recipe_hash(&r).unwrap();
    assert_eq!(hash.len(), 64);
    assert!(hash
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));

    let expected = hex::encode(Sha256::digest(
        recipe::canonical_json(&r).unwrap().as_bytes(),
    ));
    assert_eq!(hash, expected);
}

/// 正規化 JSON の f64 は 1e-6 グリッドに量子化される。
///
/// これにより `serde_json::from_str` が最短往復可能表現から 1 ULP ずれた値を
/// 返しても(実測で -1.0..=1.0 の一様乱数の約 10% で発生する)、正規化形と
/// ハッシュは変わらない。単純な値の表現は従来どおり。
#[test]
fn canonical_json_quantizes_floats_to_1e6_grid() {
    // 1 ULP ずれが観測された実際の値。往復してもハッシュが変わらないこと。
    for raw in [
        "0.018742357423833362",
        "0.9818949935030845",
        "0.21692818439298694",
    ] {
        let a = parse(&format!(
            r#"{{"operations":[{{"op":"adjust","brightness":{raw}}}]}}"#
        ));
        let json = recipe::canonical_json(&a).unwrap();
        let b: TransformRecipe = serde_json::from_str(&json).expect("canonical json parses");
        assert_eq!(recipe::canonical_json(&b).unwrap(), json, "raw={raw}");
    }

    // 1e-6 未満の差は同一視される / 1e-6 の差は区別される。
    let a = parse(r#"{"operations":[{"op":"rotate","angle_degrees":-1.8}]}"#);
    let a2 = parse(r#"{"operations":[{"op":"rotate","angle_degrees":-1.8000001}]}"#);
    let b = parse(r#"{"operations":[{"op":"rotate","angle_degrees":-1.800001}]}"#);
    assert_eq!(
        recipe::recipe_hash(&a).unwrap(),
        recipe::recipe_hash(&a2).unwrap()
    );
    assert_ne!(
        recipe::recipe_hash(&a).unwrap(),
        recipe::recipe_hash(&b).unwrap()
    );

    // 単純な値・整数値の表現は変わらない。
    assert_eq!(
        recipe::canonical_json(&a).unwrap(),
        r#"{"operations":[{"angle_degrees":-1.8,"crop":"largest_inscribed_rect","op":"rotate"}]}"#
    );
    assert_eq!(
        recipe::canonical_json(&parse(
            r#"{"operations":[{"op":"rotate","angle_degrees":90}]}"#
        ))
        .unwrap(),
        r#"{"operations":[{"angle_degrees":90.0,"crop":"largest_inscribed_rect","op":"rotate"}]}"#
    );
}

/// 意味が違えばハッシュも違う。
#[test]
fn different_recipes_hash_differently() {
    let a = parse(r#"{"operations":[{"op":"resize","width":100}]}"#);
    let b = parse(r#"{"operations":[{"op":"resize","width":101}]}"#);
    assert_ne!(
        recipe::recipe_hash(&a).unwrap(),
        recipe::recipe_hash(&b).unwrap()
    );
}

fn assert_invalid(json: &str, needle: &str) {
    let r = parse(json);
    let err = recipe::validate(&r).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(needle),
        "expected error containing {needle:?}, got {msg:?}"
    );
}

#[test]
fn validate_accepts_full_pipeline() {
    let r = parse(
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"rotate","angle_degrees":-1.8},
            {"op":"crop","aspect_ratio":"16:9"},
            {"op":"resize","width":1600,"fit":"cover"},
            {"op":"adjust","brightness":0.05,"sharpness":0.3},
            {"op":"strip_metadata","scope":"gps"},
            {"op":"encode","format":"webp","quality":82}
        ]}"#,
    );
    recipe::validate(&r).unwrap();
}

#[test]
fn validate_rejects_empty_operations() {
    assert_invalid(r#"{"operations":[]}"#, "must not be empty");
}

#[test]
fn validate_rejects_encode_not_last() {
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"png"},{"op":"auto_orient"}]}"#,
        "must be the last operation",
    );
}

#[test]
fn validate_rejects_multiple_encode() {
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"png"},{"op":"encode","format":"jpeg"}]}"#,
        "must be the last operation",
    );
}

#[test]
fn validate_rejects_crop_without_target() {
    assert_invalid(
        r#"{"operations":[{"op":"crop"}]}"#,
        "one of aspect_ratio or rect is required",
    );
}

#[test]
fn validate_rejects_crop_with_both_targets() {
    assert_invalid(
        r#"{"operations":[{"op":"crop","aspect_ratio":"1:1","rect":{"x":0,"y":0,"width":10,"height":10}}]}"#,
        "not both",
    );
}

#[test]
fn validate_rejects_bad_aspect_ratio() {
    for bad in ["16x9", "16:0", "0:9", "16:", ":9", "-16:9", "1.5:1"] {
        assert_invalid(
            &format!(r#"{{"operations":[{{"op":"crop","aspect_ratio":"{bad}"}}]}}"#),
            "aspect_ratio must be",
        );
    }
}

#[test]
fn validate_rejects_resize_without_dimension() {
    assert_invalid(
        r#"{"operations":[{"op":"resize","fit":"cover"}]}"#,
        "at least one of width or height",
    );
}

#[test]
fn validate_rejects_out_of_range_adjust() {
    assert_invalid(
        r#"{"operations":[{"op":"adjust","brightness":1.5}]}"#,
        "brightness must be within",
    );
    assert_invalid(
        r#"{"operations":[{"op":"adjust","saturation":-2.0}]}"#,
        "saturation must be within",
    );
    assert_invalid(
        r#"{"operations":[{"op":"adjust","sharpness":-0.1}]}"#,
        "sharpness must be within",
    );
}

#[test]
fn validate_rejects_out_of_range_quality() {
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"jpeg","quality":0}]}"#,
        "quality must be within",
    );
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"jpeg","quality":101}]}"#,
        "quality must be within",
    );
}

#[test]
fn validate_rejects_out_of_range_rotation() {
    assert_invalid(
        r#"{"operations":[{"op":"rotate","angle_degrees":361.0}]}"#,
        "angle_degrees must be within",
    );
}

/// 未知フィールドは deny_unknown_fields でパース時に落ちる。
#[test]
fn unknown_fields_are_rejected_at_parse_time() {
    let err = serde_json::from_str::<TransformRecipe>(
        r#"{"operations":[{"op":"resize","width":10,"wdith":10}]}"#,
    );
    assert!(err.is_err());
}

// ---------------------------------------------------------------------------
// v0.4: encode.bit_depth(16bit 出力)
// ---------------------------------------------------------------------------

/// `bit_depth` は 8 / 16 のみ。16 は png 出力に限る。
#[test]
fn validate_checks_bit_depth() {
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"png","bit_depth":12}]}"#,
        "bit_depth must be 8 or 16",
    );
    assert_invalid(
        r#"{"operations":[{"op":"encode","format":"jpeg","bit_depth":16}]}"#,
        "bit_depth 16 is only supported for png output",
    );
    // 有効な組み合わせ。
    for json in [
        r#"{"operations":[{"op":"encode","format":"png","bit_depth":16}]}"#,
        r#"{"operations":[{"op":"encode","format":"png","bit_depth":8}]}"#,
        r#"{"operations":[{"op":"encode","format":"jpeg","bit_depth":8}]}"#,
    ] {
        recipe::validate(&parse(json)).expect("should be valid");
    }
}

/// **`bit_depth` を書かないレシピの canonical JSON / hash は v0.3 以前と 1 バイトも変わらない。**
///
/// `#[serde(default, skip_serializing_if = "Option::is_none")]` なので、
/// 既定値のときはフィールドが正規化 JSON に現れない(v0.3 の `coordinate_space` と同じ手口)。
/// 既存 revision の冪等キーが壊れないことのゲート。
#[test]
fn omitted_bit_depth_is_invisible_to_the_canonical_form() {
    let without = parse(r#"{"operations":[{"op":"encode","format":"png"}]}"#);
    let explicit_default =
        parse(r#"{"operations":[{"op":"encode","format":"png","bit_depth":8}]}"#);
    let sixteen = parse(r#"{"operations":[{"op":"encode","format":"png","bit_depth":16}]}"#);

    let canonical = recipe::canonical_json(&without).unwrap();
    assert_eq!(
        canonical, r#"{"operations":[{"format":"png","op":"encode"}]}"#,
        "omitting bit_depth must keep the pre-v0.4 canonical form"
    );
    // 明示した場合は当然フィールドが出るので、別ハッシュになる(意味が違うので正しい)。
    assert_ne!(
        recipe::recipe_hash(&without).unwrap(),
        recipe::recipe_hash(&explicit_default).unwrap()
    );
    assert_ne!(
        recipe::recipe_hash(&without).unwrap(),
        recipe::recipe_hash(&sixteen).unwrap()
    );
}

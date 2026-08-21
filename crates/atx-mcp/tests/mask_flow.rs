//! v0.5 マスク層の統合テスト: `generate_mask` → op の `mask` 参照 → `render_preview`
//! の `overlay: "mask"` まで、実ワークスペース(tempdir)と実画像で通す。
//!
//! ROADMAP Phase C / v0.5。stdio / JSON-RPC は経由せず [`AtxTools`] を直接叩く。

use std::path::PathBuf;

use atx_mcp::mask::GenerateMaskParams;
use atx_mcp::tools::{AtxTools, ImportAssetParams, RenderPreviewParams, TransformParams};
use image::RgbImage;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

/// フィクスチャの実効寸法(EXIF Orientation 適用後)。マスクはこれと厳密に一致すること。
const FIXTURE_WIDTH: u32 = 1477;
const FIXTURE_HEIGHT: u32 = 1108;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/synthetic_scene.jpg")
        .canonicalize()
        .expect("fixture image must exist")
}

#[track_caller]
fn structured(result: &CallToolResult) -> Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "tool returned an error: {:?}",
        result.content
    );
    assert!(
        matches!(result.content.first(), Some(ContentBlock::Text(_))),
        "every successful result must start with a human-readable text summary"
    );
    result
        .structured_content
        .clone()
        .expect("every tool must return structuredContent")
}

#[track_caller]
fn error_payload(result: &CallToolResult) -> Value {
    assert_eq!(result.is_error, Some(true), "expected a tool-level error");
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find_map(|t| serde_json::from_str::<Value>(&t.text).ok())
        .expect("error results must carry a structured JSON block")
}

fn preview_jpeg_bytes(result: &CallToolResult) -> Vec<u8> {
    let image = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image(img) => Some(img),
            _ => None,
        })
        .expect("render_preview must return an inline image");
    assert_eq!(image.mime_type, "image/jpeg");
    base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        image.data.as_str(),
    )
    .expect("inline image must be valid base64")
}

/// フィクスチャを取り込んだワークスペースを用意する。
fn workspace_with_fixture() -> (tempfile::TempDir, AtxTools, String) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string();
    (workspace, tools, rev)
}

fn params(reference: &str, kind: &str) -> GenerateMaskParams {
    GenerateMaskParams {
        reference_revision_id: reference.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
}

/// structuredContent の `path` から画像を読む。
fn load_rgb(out: &Value, key: &str) -> RgbImage {
    let path = out[key].as_str().expect("a path field");
    image::open(path)
        .unwrap_or_else(|e| panic!("failed to open {path}: {e}"))
        .to_rgb8()
}

fn channel_delta(a: &RgbImage, b: &RgbImage, x: u32, y: u32) -> i32 {
    let pa = a.get_pixel(x, y).0;
    let pb = b.get_pixel(x, y).0;
    (0..3)
        .map(|i| (pa[i] as i32 - pb[i] as i32).abs())
        .max()
        .unwrap_or(0)
}

/// 4種すべてが「参照と同寸法の 8bit グレースケール PNG revision」を生み、
/// 同じ params での再生成が同じ revision(sha256 dedup による冪等)になること。
#[test]
fn every_kind_produces_a_grayscale_png_revision_and_is_idempotent() {
    let (_ws, tools, source) = workspace_with_fixture();

    let mut cases: Vec<GenerateMaskParams> = Vec::new();

    let mut linear = params(&source, "linear_gradient");
    linear.angle_degrees = Some(0.0);
    linear.start = Some(0.2);
    linear.end = Some(0.8);
    cases.push(linear);

    let mut radial = params(&source, "radial_gradient");
    radial.center_x = Some(0.5);
    radial.center_y = Some(0.5);
    radial.radius = Some(0.3);
    radial.feather = Some(0.2);
    cases.push(radial);

    let mut luminosity = params(&source, "luminosity_range");
    luminosity.min = Some(128);
    luminosity.max = Some(255);
    luminosity.feather = Some(24.0);
    cases.push(luminosity);

    let mut color = params(&source, "color_range");
    color.hue_center = Some(210.0);
    color.hue_width = Some(40.0);
    color.feather = Some(20.0);
    cases.push(color);

    let mut seen: Vec<String> = Vec::new();
    for case in &cases {
        let out = structured(&tools.generate_mask(case));
        assert_eq!(out["kind"], case.kind.as_str());
        assert_eq!(
            out["reused"],
            Value::Bool(false),
            "{} must be new",
            case.kind
        );
        assert_eq!(out["reference_revision_id"], source.as_str());
        assert_eq!(out["width"], FIXTURE_WIDTH);
        assert_eq!(out["height"], FIXTURE_HEIGHT);
        assert_eq!(out["revision"]["mime_type"], "image/png");
        assert_eq!(out["revision"]["width"], FIXTURE_WIDTH);
        assert_eq!(out["revision"]["height"], FIXTURE_HEIGHT);
        // 次の一手(op の mask フィールドから参照する)が返却に含まれること。
        assert!(out["next"].as_str().unwrap().contains("\"mask\""));

        // 画素も参照と同寸法で、実際にグレー(3チャンネルが同値)であること。
        let mask = load_rgb(&out["revision"], "path");
        assert_eq!(mask.dimensions(), (FIXTURE_WIDTH, FIXTURE_HEIGHT));
        for (x, y) in [(0, 0), (FIXTURE_WIDTH / 2, FIXTURE_HEIGHT / 2)] {
            let p = mask.get_pixel(x, y).0;
            assert_eq!(p[0], p[1], "{}: mask must be gray", case.kind);
            assert_eq!(p[1], p[2], "{}: mask must be gray", case.kind);
        }

        let revision_id = out["revision"]["revision_id"].as_str().unwrap().to_string();
        assert!(
            !seen.contains(&revision_id),
            "{}: different kinds must not collide",
            case.kind
        );

        // 冪等: 同じ params + 同じ参照 → 同じ PNG バイト列 → 同じ revision。
        let again = structured(&tools.generate_mask(case));
        assert_eq!(again["reused"], Value::Bool(true), "{}", case.kind);
        assert_eq!(again["revision"]["revision_id"], revision_id.as_str());
        assert_eq!(
            again["revision"]["sha256"], out["revision"]["sha256"],
            "{}: the same spec must produce byte-identical png",
            case.kind
        );
        seen.push(revision_id);
    }
    assert_eq!(seen.len(), 4);
}

/// 実画像に対する luminosity_range は「全部白でも全部黒でもない」もっともらしい
/// 被覆になること(定数マスクを返す退化を検出する)。
#[test]
fn luminosity_range_covers_a_plausible_part_of_the_fixture() {
    let (_ws, tools, source) = workspace_with_fixture();
    let mut spec = params(&source, "luminosity_range");
    spec.min = Some(140);
    spec.max = Some(255);
    spec.feather = Some(20.0);

    let out = structured(&tools.generate_mask(&spec));
    let mean = out["mean_weight"].as_f64().expect("mean_weight");
    assert!(
        (0.05..0.95).contains(&mean),
        "a luminance mask over a real photo must be neither empty nor full, got {mean}"
    );

    // 中身も確認: 白と黒の両方が実在すること。
    let mask = load_rgb(&out["revision"], "path");
    let mut has_bright = false;
    let mut has_dark = false;
    for p in mask.pixels() {
        if p.0[0] > 200 {
            has_bright = true;
        }
        if p.0[0] < 55 {
            has_dark = true;
        }
    }
    assert!(
        has_bright && has_dark,
        "expected both selected and rejected pixels"
    );
}

/// E2E: 生成したリニアグラデーションマスクを curves に付けると、
/// 白ゾーンではマスクなしの結果と一致し、黒ゾーンでは入力のままになること。
#[test]
fn a_masked_curves_only_changes_the_white_zone() {
    let (_ws, tools, source) = workspace_with_fixture();

    // 上 45% が白、下 55% から黒(angle 0 = 上が白)。
    let mut spec = params(&source, "linear_gradient");
    spec.angle_degrees = Some(0.0);
    spec.start = Some(0.45);
    spec.end = Some(0.55);
    let mask = structured(&tools.generate_mask(&spec));
    let mask_id = mask["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let curve = serde_json::json!({"master": [[0, 0], [128, 220], [255, 255]]});
    let identity = structured(
        &tools.apply_transform(&TransformParams {
            revision_id: source.clone(),
            recipe: Some(
                serde_json::from_value(serde_json::json!({
                    "operations": [{"op": "encode", "format": "png"}]
                }))
                .unwrap(),
            ),
            preset: None,
        }),
    );
    let unmasked = structured(
        &tools.apply_transform(&TransformParams {
            revision_id: source.clone(),
            recipe: Some(
                serde_json::from_value(serde_json::json!({
                    "operations": [
                        {"op": "curves", "master": curve["master"]},
                        {"op": "encode", "format": "png"}
                    ]
                }))
                .unwrap(),
            ),
            preset: None,
        }),
    );
    let masked = structured(&tools.apply_transform(&TransformParams {
        revision_id: source.clone(),
        recipe: Some(
            serde_json::from_value(serde_json::json!({
                "operations": [
                    {"op": "curves", "master": curve["master"], "mask": {"revision_id": mask_id}},
                    {"op": "encode", "format": "png"}
                ]
            }))
            .unwrap(),
        ),
        preset: None,
    }));

    let identity_img = load_rgb(&identity["revision"], "path");
    let unmasked_img = load_rgb(&unmasked["revision"], "path");
    let masked_img = load_rgb(&masked["revision"], "path");
    assert_eq!(masked_img.dimensions(), identity_img.dimensions());

    // 白ゾーン(上部): マスクありはマスクなしと一致し、入力とは大きく違う。
    let (wx, wy) = (700, 100);
    assert!(
        channel_delta(&masked_img, &unmasked_img, wx, wy) <= 1,
        "in the white zone the masked result must match the unmasked one"
    );
    assert!(
        channel_delta(&masked_img, &identity_img, wx, wy) > 8,
        "in the white zone the curve must actually have changed the pixel"
    );

    // 黒ゾーン(下部): マスクありは入力と一致し、マスクなしとは大きく違う。
    let (bx, by) = (700, 1000);
    assert!(
        channel_delta(&masked_img, &identity_img, bx, by) <= 1,
        "in the black zone the masked result must be the untouched input"
    );
    assert!(
        channel_delta(&masked_img, &unmasked_img, bx, by) > 8,
        "in the black zone the masked result must differ from the unmasked one"
    );
}

/// `overlay: "mask"` はプレーンなプレビューと違う inline jpeg を返し、
/// 互いのキャッシュを上書きせず共存すること。マスク違いも別キャッシュになること。
#[test]
fn preview_mask_overlay_differs_from_the_plain_preview_and_coexists() {
    let (_ws, tools, source) = workspace_with_fixture();

    let mut top = params(&source, "linear_gradient");
    top.start = Some(0.4);
    top.end = Some(0.5);
    let mask_a = structured(&tools.generate_mask(&top));
    let mask_a_id = mask_a["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut centre = params(&source, "radial_gradient");
    centre.radius = Some(0.2);
    centre.feather = Some(0.05);
    let mask_b = structured(&tools.generate_mask(&centre));
    let mask_b_id = mask_b["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [{"op": "resize", "width": 900, "fit": "contain"}]
    }))
    .unwrap();

    let plain = tools.render_preview(&RenderPreviewParams {
        revision_id: source.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    let plain_out = structured(&plain);
    let plain_bytes = preview_jpeg_bytes(&plain);

    let overlaid = tools.render_preview(&RenderPreviewParams {
        revision_id: source.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
        overlay: Some("mask".to_string()),
        mask_revision_id: Some(mask_a_id.clone()),
    });
    let overlaid_out = structured(&overlaid);
    let overlaid_bytes = preview_jpeg_bytes(&overlaid);

    assert_eq!(overlaid_out["overlay"], "mask");
    assert_eq!(overlaid_out["mask_revision_id"], mask_a_id.as_str());
    assert_ne!(
        overlaid_bytes, plain_bytes,
        "the mask overlay must visibly change the preview"
    );
    assert_eq!(
        overlaid_out["width"], plain_out["width"],
        "the overlay must not change the preview dimensions"
    );

    // キャッシュ共存: overlay あり / なし / 別マスク が互いを上書きしないこと。
    let other = tools.render_preview(&RenderPreviewParams {
        revision_id: source.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
        overlay: Some("mask".to_string()),
        mask_revision_id: Some(mask_b_id.clone()),
    });
    let other_out = structured(&other);
    let paths = [
        plain_out["preview_path"].as_str().unwrap(),
        overlaid_out["preview_path"].as_str().unwrap(),
        other_out["preview_path"].as_str().unwrap(),
    ];
    assert_ne!(paths[0], paths[1]);
    assert_ne!(
        paths[1], paths[2],
        "the cache key must include the mask revision id"
    );
    for path in paths {
        assert!(PathBuf::from(path).is_file(), "{path} must still exist");
    }
    assert_ne!(preview_jpeg_bytes(&other), overlaid_bytes);

    // 同じ呼び出しの繰り返しはキャッシュヒットでバイト同一。
    let again = tools.render_preview(&RenderPreviewParams {
        revision_id: source,
        recipe: Some(recipe),
        preset: None,
        overlay: Some("mask".to_string()),
        mask_revision_id: Some(mask_a_id),
    });
    assert_eq!(preview_jpeg_bytes(&again), overlaid_bytes);
}

/// `overlay: "mask"` と `mask_revision_id` は相互に必須・排他で、
/// どちらの誤りも回復手順つきの構造化エラーになること。
#[test]
fn mask_overlay_argument_errors_are_structured() {
    let (_ws, tools, source) = workspace_with_fixture();
    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [{"op": "resize", "width": 600, "fit": "contain"}]
    }))
    .unwrap();

    // 1. overlay="mask" なのに mask_revision_id がない。
    let missing = tools.render_preview(&RenderPreviewParams {
        revision_id: source.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
        overlay: Some("mask".to_string()),
        mask_revision_id: None,
    });
    let payload = error_payload(&missing);
    assert_eq!(payload["error"]["code"], "mask_revision_id_required");
    assert!(payload["error"]["details"]["recovery"]
        .as_str()
        .unwrap()
        .contains("generate_mask"));

    // 2. mask_revision_id だけ渡して overlay を指定していない。
    let stray = tools.render_preview(&RenderPreviewParams {
        revision_id: source.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
        overlay: None,
        mask_revision_id: Some("rev_whatever".to_string()),
    });
    assert_eq!(
        error_payload(&stray)["error"]["code"],
        "mask_revision_id_without_mask_overlay"
    );

    // 3. 存在しないマスクは revision_not_found として返る。
    let unknown = tools.render_preview(&RenderPreviewParams {
        revision_id: source,
        recipe: Some(recipe),
        preset: None,
        overlay: Some("mask".to_string()),
        mask_revision_id: Some("rev_does_not_exist".to_string()),
    });
    assert_eq!(
        error_payload(&unknown)["error"]["code"],
        "revision_not_found"
    );
}

/// generate_mask の引数エラーは、その kind の語彙と回復手順を添えて返ること。
#[test]
fn generate_mask_argument_errors_are_teachers() {
    let (_ws, tools, source) = workspace_with_fixture();

    let unknown = tools.generate_mask(&params(&source, "subject"));
    let payload = error_payload(&unknown);
    assert_eq!(payload["error"]["code"], "invalid_mask_kind");
    let valid: Vec<&str> = payload["error"]["details"]["valid_values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        valid,
        [
            "linear_gradient",
            "radial_gradient",
            "luminosity_range",
            "color_range"
        ]
    );

    // 別 kind のフィールドは、その kind が受け付ける名前を列挙して弾く。
    let mut wrong = params(&source, "luminosity_range");
    wrong.angle_degrees = Some(30.0);
    let payload = error_payload(&tools.generate_mask(&wrong));
    assert_eq!(payload["error"]["code"], "unexpected_mask_param");
    assert!(payload["error"]["details"]["accepted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "feather"));

    // 値域はクランプせず検証する。
    let mut bad = params(&source, "radial_gradient");
    bad.radius = Some(4.0);
    assert_eq!(
        error_payload(&tools.generate_mask(&bad))["error"]["code"],
        "invalid_mask_param"
    );

    // 画像でない revision は参照できない。
    let cube = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/identity_2.cube");
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: cube.to_string_lossy().into_owned(),
    }));
    let cube_rev = imported["revision"]["revision_id"].as_str().unwrap();
    assert_eq!(
        error_payload(&tools.generate_mask(&params(cube_rev, "linear_gradient")))["error"]["code"],
        "not_an_image"
    );
}

//! ドキュメント前処理(DESIGN §9.12)の統合テスト:
//! `import_asset` → `detect_document` → 返ってきた `suggested_operation` を
//! そのまま `apply_transform` の先頭 op に置く、という「貼るだけ」フローを通す。
//!
//! stdio / JSON-RPC は経由せず [`AtxTools`] を直接叩く(他の *_flow.rs と同じ方針)。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, DetectDocumentParams, ImportAssetParams, RenderPreviewParams, TransformParams,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{json, Value};

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
        .canonicalize()
        .unwrap_or_else(|e| panic!("fixture {relative} must exist: {e}"))
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

fn summary(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected a text summary, got {other:?}"),
    }
}

/// フィクスチャを1つ取り込んだワークスペースを用意する。
fn workspace_with(relative: &str) -> (tempfile::TempDir, AtxTools, String) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture(relative).to_string_lossy().into_owned(),
    )));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string();
    (workspace, tools, rev)
}

fn detect(tools: &AtxTools, revision_id: &str) -> Value {
    structured(&tools.detect_document(&DetectDocumentParams {
        revision_id: revision_id.to_string(),
        min_area_ratio: None,
    }))
}

/// 検出 → `suggested_operation` を貼るだけで `perspective` が通り、
/// 出力寸法が `output_size_hint` と一致する。
#[test]
fn suggested_operation_pastes_straight_into_a_recipe() {
    let (_workspace, tools, rev) = workspace_with("evals/fixtures/document_photo.jpg");
    let detection = detect(&tools, &rev)["detection"].clone();

    assert!(
        detection["quad"].is_array(),
        "the document photo must yield a quad: {detection}"
    );
    assert_eq!(detection["method"], "contour");
    let suggested = detection["suggested_operation"].clone();
    assert_eq!(suggested["op"], "perspective");
    assert_eq!(suggested["quad"], detection["quad"]);
    let hint = detection["output_size_hint"].clone();

    // 「レシピの先頭 op として貼る」— 加工は一切しない。
    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(rev.clone()),
        revision_ids: None,
        recipe: Some(atx_mcp::tools::RecipeJson(json!({
            "operations": [suggested, {"op": "encode", "format": "jpeg", "quality": 85}],
        }))),
        preset: None,
    }));
    assert_eq!(
        applied["revision"]["width"], hint["width"],
        "output width must match output_size_hint ({applied})"
    );
    assert_eq!(
        applied["revision"]["height"], hint["height"],
        "output height must match output_size_hint ({applied})"
    );

    // テキストサマリは英語で「貼れ」と言う(ホスト AI が読む面)。
    let text = summary(&tools.detect_document(&DetectDocumentParams {
        revision_id: rev,
        min_area_ratio: None,
    }));
    assert!(text.contains("quad found"), "{text}");
    assert!(text.contains("paste suggested_operation"), "{text}");
}

/// 同じ画像を 2 回検出したら structuredContent は完全に同一(決定論)。
#[test]
fn detection_is_deterministic_on_the_building_scene() {
    let (_workspace, tools, rev) = workspace_with("tests/fixtures/synthetic_scene.jpg");
    let first = detect(&tools, &rev);
    let second = detect(&tools, &rev);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "detect_document must be byte-identical across calls"
    );
    // 建物写真で用紙でないもの(ビル)を拾うことはありうる。要件は
    // 「安定していること」と「confidence が正直であること」だけ。
    let detection = &first["detection"];
    let confidence = detection["confidence"].as_f64().unwrap_or(0.0);
    assert!(
        (0.0..=1.0).contains(&confidence),
        "confidence must stay in 0..=1: {detection}"
    );
    if detection["quad"].is_null() {
        assert!(
            !detection["warnings"]
                .as_array()
                .expect("warnings")
                .is_empty(),
            "a null quad must always come with a reason: {detection}"
        );
    }
}

/// 範囲外の `min_area_ratio` は有効範囲を添えた構造化エラーになる。
#[test]
fn min_area_ratio_out_of_range_is_a_structured_error() {
    let (_workspace, tools, rev) = workspace_with("tests/fixtures/synthetic_scene.jpg");
    for bad in [0.0, 0.04, 1.5] {
        let payload = error_payload(&tools.detect_document(&DetectDocumentParams {
            revision_id: rev.clone(),
            min_area_ratio: Some(bad),
        }));
        assert_eq!(
            payload["error"]["code"], "invalid_min_area_ratio",
            "{payload}"
        );
        let details = &payload["error"]["details"];
        assert_eq!(details["min"], 0.05, "{payload}");
        assert_eq!(details["max"], 1.0, "{payload}");
        assert!(details["recovery"].is_string(), "{payload}");
    }
}

/// 画像でない revision(.cube LUT)は detect_tilt と同じ `not_an_image`。
#[test]
fn a_non_image_revision_is_rejected() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(
        &tools.import_asset(&ImportAssetParams::single(
            fixture("tests/fixtures/identity_8.cube")
                .to_string_lossy()
                .into_owned(),
        )),
    );
    let rev = imported["revision"]["revision_id"].as_str().unwrap();
    let payload = error_payload(&tools.detect_document(&DetectDocumentParams {
        revision_id: rev.to_string(),
        min_area_ratio: None,
    }));
    assert_eq!(payload["error"]["code"], "not_an_image", "{payload}");
}

fn preview(tools: &AtxTools, rev: &str, recipe: Value, long_edge: Option<u32>) -> CallToolResult {
    tools.render_preview(&RenderPreviewParams {
        revision_id: rev.to_string(),
        recipe: Some(atx_mcp::tools::RecipeJson(recipe)),
        preset: None,
        overlay: None,
        mask_revision_id: None,
        long_edge,
    })
}

/// 既定は従来どおり長辺 ≤ 768。
#[test]
fn the_default_preview_long_edge_is_unchanged() {
    let (_workspace, tools, rev) = workspace_with("evals/fixtures/document_photo.jpg");
    let out = structured(&preview(
        &tools,
        &rev,
        json!({"operations": [{"op": "auto_orient"}]}),
        None,
    ));
    let long = out["width"]
        .as_u64()
        .unwrap()
        .max(out["height"].as_u64().unwrap());
    assert_eq!(long, 768, "{out}");
}

/// `long_edge: 1568` は「入力がそれ以上のとき」ちょうど 1568 になる。
#[test]
fn long_edge_1568_produces_a_1568px_preview() {
    let (_workspace, tools, rev) = workspace_with("evals/fixtures/document_photo.jpg");
    // フィクスチャの長辺は 1400 なので、まず 2000px へ拡大してから縮小させる
    // (`without_enlargement: true` のプレビュー resize は拡大しないため)。
    let recipe = json!({"operations": [{"op": "resize", "width": 2000, "fit": "contain", "without_enlargement": false}]});
    let out = structured(&preview(&tools, &rev, recipe.clone(), Some(1568)));
    let long = out["width"]
        .as_u64()
        .unwrap()
        .max(out["height"].as_u64().unwrap());
    assert_eq!(long, 1568, "{out}");

    // 既定(768)と別ファイルにキャッシュされる(長辺がキーに入っている)。
    let small = structured(&preview(&tools, &rev, recipe, None));
    assert_ne!(small["preview_path"], out["preview_path"], "{out} {small}");
}

/// 範囲外の `long_edge` は有効範囲を添えた構造化エラーになる。
#[test]
fn long_edge_out_of_range_is_a_structured_error() {
    let (_workspace, tools, rev) = workspace_with("evals/fixtures/document_photo.jpg");
    for bad in [1u32, 255, 1569, 4096] {
        let payload = error_payload(&preview(
            &tools,
            &rev,
            json!({"operations": [{"op": "auto_orient"}]}),
            Some(bad),
        ));
        assert_eq!(payload["error"]["code"], "invalid_long_edge", "{payload}");
        let details = &payload["error"]["details"];
        assert_eq!(details["min"], 256, "{payload}");
        assert_eq!(details["max"], 1568, "{payload}");
        assert_eq!(details["default"], 768, "{payload}");
    }
}

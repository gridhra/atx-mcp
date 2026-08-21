//! v0.3「レシピ → アセット参照」の統合テスト。
//!
//! .cube LUT の取り込み(画像ではないアセット)、非画像 revision に対する
//! inspect_image の構造化エラー、そして lut op からの参照解決を検証する。

use std::path::PathBuf;

use atx_mcp::tools::{AtxTools, ImportAssetParams, RevisionParams, TransformParams, CUBE_MIME};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

fn image_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/synthetic_scene.jpg")
        .canonicalize()
        .expect("fixture image must exist")
}

fn cube_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/identity_2.cube")
        .canonicalize()
        .expect("fixture .cube must exist")
}

#[track_caller]
fn structured(result: &CallToolResult) -> Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "tool returned an error: {:?}",
        result.content
    );
    result
        .structured_content
        .clone()
        .expect("every tool must return structuredContent")
}

#[track_caller]
fn text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected a text summary, got {other:?}"),
    }
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

fn tools() -> (tempfile::TempDir, AtxTools) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    (workspace, tools)
}

/// .cube を import すると、画像検査を通さずに LUT アセットとして台帳に載る。
#[test]
fn importing_a_cube_produces_a_lut_asset_revision() {
    let (_ws, tools) = tools();
    let result = tools.import_asset(&ImportAssetParams {
        path: cube_fixture().to_string_lossy().into_owned(),
    });
    let out = structured(&result);
    let body = text(&result);

    let revision = &out["revision"];
    assert_eq!(revision["mime_type"], CUBE_MIME);
    assert_eq!(revision["width"], 0);
    assert_eq!(revision["height"], 0);
    let path = revision["path"].as_str().expect("path must be a string");
    assert!(
        path.ends_with(".cube"),
        "a LUT object must keep the .cube extension in the store, got {path}"
    );
    assert!(
        std::path::Path::new(path).is_file(),
        "the stored object must exist at {path}"
    );
    // テキストサマリはホスト AI に次の一手(lut op からの参照)を示すこと。
    assert!(body.contains("lut_revision_id"), "summary was: {body}");

    // 再取り込みは冪等。
    let again = structured(&tools.import_asset(&ImportAssetParams {
        path: cube_fixture().to_string_lossy().into_owned(),
    }));
    assert_eq!(again["reused"], Value::Bool(true));
    assert_eq!(again["revision"]["revision_id"], revision["revision_id"]);
}

/// LUT revision を inspect_image に渡してもデコードを試みず、構造化エラーで返る。
#[test]
fn inspect_image_rejects_a_cube_revision_with_a_structured_error() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: cube_fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = tools.inspect_image(&RevisionParams {
        revision_id: rev.clone(),
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "not_an_image");
    assert_eq!(payload["error"]["details"]["mime_type"], CUBE_MIME);
    assert_eq!(payload["error"]["details"]["revision_id"], rev.as_str());
    assert!(payload["error"]["details"]["recovery"]
        .as_str()
        .unwrap()
        .contains("lut"));
}

/// lut op が存在しない revision を参照した場合、画素処理に入る前に
/// 「どの op の、どの参照が壊れているか」を含む構造化エラーで返る。
/// これはリゾルバ段の失敗なので、atx-core の lut 実装状況に依存しない。
#[test]
fn a_lut_referencing_an_unknown_revision_fails_with_an_actionable_error() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: image_fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [
            {"op": "lut", "lut_revision_id": "rev_DOES_NOT_EXIST"},
            {"op": "encode", "format": "png"}
        ]
    }))
    .expect("the lut op must deserialize");

    let result = tools.apply_transform(&TransformParams {
        revision_id: rev,
        recipe: Some(recipe),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "operation_failed");
    assert_eq!(payload["error"]["details"]["op"], "lut");
    assert_eq!(payload["error"]["details"]["operation_index"], 0);
    let reason = payload["error"]["details"]["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("rev_DOES_NOT_EXIST"),
        "the error must name the missing reference: {reason}"
    );
    assert!(
        reason.contains("not found in this workspace"),
        "the error must say the id is unknown to this workspace: {reason}"
    );
    assert!(
        reason.contains("import_asset"),
        "the error must point at the recovery step: {reason}"
    );
}

/// import した .cube を参照する lut レシピが end-to-end で通ること
/// (import → revision_id を lut op から参照 → 冪等ショートサーキット)。
#[test]
fn applying_an_imported_lut_end_to_end() {
    let (_ws, tools) = tools();
    let image = structured(&tools.import_asset(&ImportAssetParams {
        path: image_fixture().to_string_lossy().into_owned(),
    }));
    let image_rev = image["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let lut = structured(&tools.import_asset(&ImportAssetParams {
        path: cube_fixture().to_string_lossy().into_owned(),
    }));
    let lut_rev = lut["revision"]["revision_id"].as_str().unwrap().to_string();

    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [
            {"op": "resize", "width": 320, "fit": "contain"},
            {"op": "lut", "lut_revision_id": lut_rev, "strength": 1.0},
            {"op": "encode", "format": "png"}
        ]
    }))
    .expect("the lut recipe must deserialize");

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: image_rev.clone(),
        recipe: Some(recipe.clone()),
        preset: None,
    }));
    assert_eq!(applied["reused"], Value::Bool(false));
    assert_eq!(applied["revision"]["mime_type"], "image/png");
    let first_revision = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 冪等ショートサーキット: 同じ (revision, recipe) は再変換せず同じ revision を返す。
    let again = structured(&tools.apply_transform(&TransformParams {
        revision_id: image_rev,
        recipe: Some(recipe),
        preset: None,
    }));
    assert_eq!(again["reused"], Value::Bool(true));
    assert_eq!(again["revision"]["revision_id"], first_revision.as_str());
}

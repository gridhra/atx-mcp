//! v0.8「SVG オーバレイ」の統合テスト。
//!
//! SVG の取り込み(ラスタ画像ではないアセット)、SVG revision に対する
//! inspect_image / apply_transform の構造化エラー、`svg_overlay` op からの
//! 参照解決、そして語彙カタログの件数を検証する。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, CompareLayout, CompareRevisionsParams, ExplainOperationParams, ImportAssetParams,
    ListOperationsParams, RevisionParams, TransformParams, SVG_MIME,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

fn image_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/synthetic_scene.jpg")
        .canonicalize()
        .expect("fixture image must exist")
}

fn badge_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/badge.svg")
        .canonicalize()
        .expect("fixture .svg must exist")
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

fn import(tools: &AtxTools, path: PathBuf) -> Value {
    structured(&tools.import_asset(&ImportAssetParams {
        path: path.to_string_lossy().into_owned(),
    }))
}

fn revision_id(imported: &Value) -> String {
    imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string()
}

/// .svg を import すると、画像としてのデコードを通さずに
/// **固有サイズ付きの**ベクタアセットとして台帳に載る。
#[test]
fn importing_an_svg_records_its_intrinsic_size() {
    let (_ws, tools) = tools();
    let result = tools.import_asset(&ImportAssetParams {
        path: badge_fixture().to_string_lossy().into_owned(),
    });
    let out = structured(&result);
    let body = text(&result);

    let revision = &out["revision"];
    assert_eq!(revision["mime_type"], SVG_MIME);
    assert_eq!(revision["width"], 40, "from the fixture's width attribute");
    assert_eq!(revision["height"], 20);
    let path = revision["path"].as_str().expect("path must be a string");
    assert!(
        path.ends_with(".svg"),
        "an SVG object must keep the .svg extension in the store, got {path}"
    );
    assert!(std::path::Path::new(path).is_file());

    // テキストサマリはホスト AI に次の一手とフォント規約の両方を示すこと。
    assert!(body.contains("svg_overlay"), "summary was: {body}");
    assert!(
        body.contains("Text is NOT rendered"),
        "the summary must warn about text up front: {body}"
    );

    // 再取り込みは冪等。
    let again = import(&tools, badge_fixture());
    assert_eq!(again["reused"], Value::Bool(true));
    assert_eq!(again["revision"]["revision_id"], revision["revision_id"]);
}

/// SVG revision を inspect_image に渡してもデコードを試みず、構造化エラーで返る。
#[test]
fn inspect_image_rejects_an_svg_revision_with_a_structured_error() {
    let (_ws, tools) = tools();
    let rev = revision_id(&import(&tools, badge_fixture()));

    let payload = error_payload(&tools.inspect_image(&RevisionParams {
        revision_id: rev.clone(),
    }));
    assert_eq!(payload["error"]["code"], "not_an_image");
    assert_eq!(payload["error"]["details"]["mime_type"], SVG_MIME);
    assert_eq!(payload["error"]["details"]["revision_id"], rev.as_str());
    let recovery = payload["error"]["details"]["recovery"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(recovery.contains("svg_overlay"), "{recovery}");
    assert!(recovery.contains("VECTOR"), "{recovery}");
}

/// SVG を**変換の入力**に指定するのも同じ誤りなので、同じ形のエラーで返す。
#[test]
fn apply_transform_rejects_an_svg_source() {
    let (_ws, tools) = tools();
    let rev = revision_id(&import(&tools, badge_fixture()));

    let recipe: atx_core::TransformRecipe =
        serde_json::from_value(serde_json::json!({"operations": [{"op": "resize", "width": 10}]}))
            .unwrap();
    let payload = error_payload(&tools.apply_transform(&TransformParams {
        revision_id: rev,
        recipe: Some(recipe),
        preset: None,
    }));
    assert_eq!(payload["error"]["code"], "not_an_image");
}

/// `svg_overlay` が存在しない revision を参照した場合、画素処理に入る前に
/// 「どの op の、どの参照が壊れているか」を含む構造化エラーで返る。
#[test]
fn an_overlay_referencing_an_unknown_revision_fails_with_an_actionable_error() {
    let (_ws, tools) = tools();
    let rev = revision_id(&import(&tools, image_fixture()));

    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [
            {"op": "svg_overlay", "svg_revision_id": "rev_DOES_NOT_EXIST", "x": 0, "y": 0},
            {"op": "encode", "format": "png"}
        ]
    }))
    .expect("the svg_overlay op must deserialize");

    let payload = error_payload(&tools.apply_transform(&TransformParams {
        revision_id: rev,
        recipe: Some(recipe),
        preset: None,
    }));
    assert_eq!(payload["error"]["code"], "operation_failed");
    assert_eq!(payload["error"]["details"]["op"], "svg_overlay");
    assert_eq!(payload["error"]["details"]["operation_index"], 0);
    let reason = payload["error"]["details"]["reason"].as_str().unwrap();
    assert!(reason.contains("rev_DOES_NOT_EXIST"), "{reason}");
    assert!(reason.contains("not found in this workspace"), "{reason}");
    assert!(reason.contains("import_asset"), "{reason}");
}

/// E2E: import → 縮小 → 右下へウォーターマーク → png。
/// **押した矩形の中だけ**が変わり、それ以外は 1 画素も動かないことを確認する。
#[test]
fn stamping_an_imported_svg_end_to_end_changes_only_the_stamped_region() {
    let (_ws, tools) = tools();
    let image_rev = revision_id(&import(&tools, image_fixture()));
    let svg_rev = revision_id(&import(&tools, badge_fixture()));

    let base_recipe = serde_json::json!({
        "operations": [
            {"op": "resize", "width": 320, "height": 240, "fit": "cover"},
            {"op": "encode", "format": "png"}
        ]
    });
    // ウォーターマークは (240, 210) に 72x36(width だけ指定 = 縦横比 40:20 を保つ)。
    let stamped_recipe = serde_json::json!({
        "operations": [
            {"op": "resize", "width": 320, "height": 240, "fit": "cover"},
            {"op": "svg_overlay", "svg_revision_id": svg_rev,
             "x": 240, "y": 210, "width": 72, "opacity": 0.35},
            {"op": "encode", "format": "png"}
        ]
    });

    let load = |recipe: serde_json::Value| -> image::RgbaImage {
        let applied = structured(&tools.apply_transform(&TransformParams {
            revision_id: image_rev.clone(),
            recipe: Some(serde_json::from_value(recipe).expect("recipe must deserialize")),
            preset: None,
        }));
        let path = applied["revision"]["path"].as_str().unwrap();
        image::open(path)
            .expect("output png must decode")
            .to_rgba8()
    };

    let plain = load(base_recipe);
    let stamped = load(stamped_recipe);
    assert_eq!(plain.dimensions(), (320, 240));
    assert_eq!(stamped.dimensions(), (320, 240));

    // 押した矩形は (240..312, 210..240) — 下端 6px は画像外なのでクリップされる。
    let mut changed_inside = 0usize;
    for y in 0..240u32 {
        for x in 0..320u32 {
            let inside = (240..312).contains(&x) && (210..240).contains(&y);
            let differs = plain.get_pixel(x, y) != stamped.get_pixel(x, y);
            if inside {
                if differs {
                    changed_inside += 1;
                }
            } else {
                assert!(
                    !differs,
                    "pixel ({x},{y}) is outside the stamped rect but changed"
                );
            }
        }
    }
    assert!(
        changed_inside > 1500,
        "the watermark should visibly cover its rect, only {changed_inside} pixels changed"
    );
}

/// compare_revisions は SVG(ベクタ)側を `not_an_image` で弾き、
/// **どちら側 (a/b) が悪いのか**をエラーに含めること。
#[test]
fn compare_revisions_rejects_an_svg_side_naming_it() {
    let (_ws, tools) = tools();
    let raster = revision_id(&import(&tools, image_fixture()));
    let svg = revision_id(&import(&tools, badge_fixture()));

    for (a, b, bad_side) in [
        (svg.clone(), raster.clone(), "a"),
        (raster.clone(), svg.clone(), "b"),
    ] {
        let result = tools.compare_revisions(&CompareRevisionsParams {
            revision_id_a: a,
            revision_id_b: b,
            layout: CompareLayout::SideBySide,
        });
        let payload = error_payload(&result);
        assert_eq!(payload["error"]["code"], "not_an_image");
        assert_eq!(payload["error"]["details"]["mime_type"], SVG_MIME);
        assert_eq!(payload["error"]["details"]["side"], bad_side);
        assert_eq!(payload["error"]["details"]["revision_id"], svg.as_str());
        assert!(
            payload["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&format!("revision_id_{bad_side}")),
            "the message must name the offending side: {:?}",
            payload["error"]["message"]
        );
    }
}

/// 語彙カタログに `svg_overlay` が 1 件だけ載り、総数が 27 になっていること。
#[test]
fn the_catalog_lists_twenty_seven_operations_including_svg_overlay() {
    let (_ws, tools) = tools();
    let out = structured(&tools.list_operations(&ListOperationsParams::default()));
    let ops = out["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 27, "v0.3.0 has 27 operations");
    let overlay: Vec<_> = ops
        .iter()
        .filter(|op| op["name"] == "svg_overlay")
        .collect();
    assert_eq!(overlay.len(), 1);
    assert_eq!(overlay[0]["category"], "filter");

    // explain_operation はフォント規約を必ず伝えること。
    let explained = structured(&tools.explain_operation(&ExplainOperationParams {
        operation: "svg_overlay".to_string(),
    }));
    let warnings = serde_json::to_string(&explained["warnings"]).unwrap();
    assert!(warnings.contains("TEXT IS NOT RENDERED"), "{warnings}");
    assert!(warnings.contains("Convert text to paths"), "{warnings}");
}

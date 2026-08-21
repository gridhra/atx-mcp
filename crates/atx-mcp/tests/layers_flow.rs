//! v0.6 レイヤーグラフの MCP 層 E2E テスト。
//!
//! import → (2レイヤーの)apply_transform → 冪等性 → render_preview → 寸法不一致エラー →
//! explain_operation("layers") を、実ワークスペース(tempdir)と実画像で通す。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, ExplainOperationParams, ImportAssetParams, ListAssetsParams, RenderPreviewParams,
    TransformParams,
};
use base64::Engine as _;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

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

fn tools() -> (tempfile::TempDir, AtxTools) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    (workspace, tools)
}

fn import_fixture(tools: &AtxTools) -> String {
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string()
}

/// backdrop(base) + 同一 revision を参照する2枚目のレイヤー(blur, multiply 60%)+
/// 仕上げパス(resize + encode)のレシピ。
fn layered_recipe(second_layer_source_rev: &str) -> Value {
    serde_json::json!({
        "layers": [
            { "source": "base", "ops": [] },
            {
                "source": { "revision_id": second_layer_source_rev },
                "ops": [{ "op": "blur", "sigma": 6.0 }],
                "blend_mode": "multiply",
                "opacity": 0.6
            }
        ],
        "operations": [
            { "op": "resize", "width": 800, "fit": "cover", "without_enlargement": true },
            { "op": "encode", "format": "webp", "quality": 80 }
        ]
    })
}

/// 同じ仕上げ op だけを持つ、layers 無しの比較用レシピ。
fn flat_recipe_with_same_finishing_ops() -> Value {
    serde_json::json!({
        "operations": [
            { "op": "resize", "width": 800, "fit": "cover", "without_enlargement": true },
            { "op": "encode", "format": "webp", "quality": 80 }
        ]
    })
}

/// 2枚目のレイヤーの ops が backdrop と寸法不一致になるレシピ(構造化エラー狙い)。
fn dims_mismatched_layered_recipe(second_layer_source_rev: &str) -> Value {
    serde_json::json!({
        "layers": [
            { "source": "base", "ops": [] },
            {
                "source": { "revision_id": second_layer_source_rev },
                "ops": [{ "op": "resize", "width": 700, "height": 500, "fit": "fill", "without_enlargement": false }],
                "blend_mode": "normal",
                "opacity": 1.0
            }
        ],
        "operations": [
            { "op": "encode", "format": "jpeg" }
        ]
    })
}

/// import → 2レイヤー(base backdrop + 同一 revision を blur/multiply 60% で重ねる)+
/// 仕上げ resize/encode を apply_transform → 期待寸法・mime の revision が発行され、
/// 同じレシピの再適用は recipe_hash が一致して再変換なし(冪等ショートサーキット)。
#[test]
fn layered_apply_transform_composites_and_short_circuits_on_reapply() {
    let (_ws, tools) = tools();
    let source_rev = import_fixture(&tools);

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(layered_recipe(&source_rev)).unwrap()),
        preset: None,
    }));
    assert_eq!(applied["reused"], Value::Bool(false));
    assert_eq!(applied["revision"]["mime_type"], "image/webp");
    // finishing pass は resize(width=800, fit=cover)なので幅は必ず 800。
    assert_eq!(applied["revision"]["width"], 800);
    assert!(applied["revision"]["height"].as_u64().unwrap() > 0);
    let derived_rev = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 台帳に載っていること(base + derived の2件)。
    let listed = structured(&tools.list_assets(&ListAssetsParams { asset_id: None }));
    assert_eq!(listed["count"], 2);

    // 冪等性: 同じレシピを再適用すると再変換されず、同じ revision が返る。
    let again = structured(&tools.apply_transform(&TransformParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(layered_recipe(&source_rev)).unwrap()),
        preset: None,
    }));
    assert_eq!(again["reused"], Value::Bool(true));
    assert_eq!(again["revision"]["revision_id"], derived_rev.as_str());
    assert_eq!(again["recipe_hash"], applied["recipe_hash"]);
    let listed_again = structured(&tools.list_assets(&ListAssetsParams { asset_id: None }));
    assert_eq!(
        listed_again["count"], 2,
        "idempotent apply must not add rows"
    );
}

/// render_preview of a layered recipe returns an inline jpeg, the preview rewrite
/// keeps the layers stack (composite still visible in the downscaled preview), and
/// the result differs from a non-layered preview built from the same finishing ops.
#[test]
fn render_preview_of_layered_recipe_keeps_the_composite() {
    let (_ws, tools) = tools();
    let source_rev = import_fixture(&tools);

    fn preview_jpeg_bytes(result: &CallToolResult) -> Vec<u8> {
        let image = result
            .content
            .iter()
            .find_map(|c| match c {
                ContentBlock::Image(img) => Some(img),
                _ => None,
            })
            .expect("render_preview must return an inline image block");
        assert_eq!(image.mime_type, "image/jpeg");
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("inline image must be valid base64")
    }

    let layered_result = tools.render_preview(&RenderPreviewParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(layered_recipe(&source_rev)).unwrap()),
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    let layered_structured = structured(&layered_result);
    assert_eq!(layered_structured["mime_type"], "image/jpeg");
    let layered_bytes = preview_jpeg_bytes(&layered_result);
    let decoded_layered =
        image::load_from_memory(&layered_bytes).expect("layered preview jpeg must decode");

    let flat_result = tools.render_preview(&RenderPreviewParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(flat_recipe_with_same_finishing_ops()).unwrap()),
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    let flat_bytes = preview_jpeg_bytes(&flat_result);
    let decoded_flat = image::load_from_memory(&flat_bytes).expect("flat preview jpeg must decode");

    // 同じ base 画像・同じ仕上げ op から作っているので寸法は一致するが、
    // レイヤー合成(multiply 60% blur)がある分、画素は別物になる。
    assert_eq!(
        (decoded_layered.width(), decoded_layered.height()),
        (decoded_flat.width(), decoded_flat.height()),
        "layered and flat previews must share the same finishing-pass dimensions"
    );
    assert_ne!(
        layered_bytes, flat_bytes,
        "preview_recipe_of must preserve layers: a layered preview must differ from the \
         equivalent flat (non-layered) preview"
    );
}

/// backdrop と寸法が合わないレイヤーは、画素処理に入る前に構造化エラーで返り、
/// atx-core のメッセージ(「どのレイヤーが」「どの寸法で」食い違うか)がそのまま乗ること。
#[test]
fn dims_mismatched_layer_is_a_structured_error_with_the_core_message() {
    let (_ws, tools) = tools();
    let source_rev = import_fixture(&tools);

    let result = tools.apply_transform(&TransformParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(dims_mismatched_layered_recipe(&source_rev)).unwrap()),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("layers[1]") && message.contains("backdrop"),
        "error must name the offending layer and explain the backdrop dimension rule: {message}"
    );
}

/// explain_operation("layers") はレシピ構造のリファレンスを返し、16(separable 12 +
/// non-separable 4)のブレンドモード名がすべて載ること。カタログ(list_operations)の
/// op 数には数えられず、unknown_operation エラーの valid_values にも出ないこと
/// (instructions 側で発見する設計)。
#[test]
fn explain_operation_layers_documents_the_recipe_structure() {
    let (_ws, tools) = tools();

    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "layers".to_string(),
    });
    let out = structured(&result);
    assert_eq!(out["name"], "layers");
    assert_eq!(out["category"], "structure");

    let body = serde_json::to_string(&out).unwrap();
    for mode in [
        "normal",
        "multiply",
        "screen",
        "overlay",
        "darken",
        "lighten",
        "color_dodge",
        "color_burn",
        "hard_light",
        "soft_light",
        "difference",
        "exclusion",
        "hue",
        "saturation",
        "color",
        "luminosity",
    ] {
        assert!(
            body.contains(mode),
            "explain_operation(\"layers\") must document blend mode {mode:?}: {body}"
        );
    }

    // list_operations のカタログは op のみで、layers は含まれない(構造は別リファレンス)。
    let catalog = structured(&tools.list_operations(&Default::default()));
    let names: Vec<&str> = catalog["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"layers"),
        "the op catalog must stay op-only; layers is recipe structure, not an op"
    );

    // 未知の op 名のエラーにも layers は出ない(instructions の一文から発見する設計)。
    let unknown = tools.explain_operation(&ExplainOperationParams {
        operation: "not_a_real_op".to_string(),
    });
    let payload = error_payload(&unknown);
    let valid: Vec<&str> = payload["error"]["details"]["valid_values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!valid.contains(&"layers"));
}

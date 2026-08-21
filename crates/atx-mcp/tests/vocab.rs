//! 語彙参照ツール(`list_operations` / `explain_operation`)とビルトインプリセットの
//! 統合テスト。ROADMAP §Agent UX の規律 #2(段階的開示)/ #3(プリセット = 語彙の圧縮)。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, ExplainOperationParams, ImportAssetParams, ListOperationsParams, RenderPreviewParams,
    TransformParams,
};
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

const V03_OPERATIONS: [&str; 21] = [
    "auto_orient",
    "rotate",
    "crop",
    "resize",
    "adjust",
    "perspective",
    "color_matrix",
    "curves",
    "levels",
    "lut",
    "white_balance",
    "hsl",
    "blur",
    "median",
    "unsharp_mask",
    "convolve",
    "clone",
    "heal",
    "svg_overlay",
    "encode",
    "strip_metadata",
];

const BUILTIN_PRESETS: [&str; 7] = [
    "eyecatch_16_9",
    "film_soft",
    "grayscale",
    "product_clean",
    "sepia",
    "thumbnail_square",
    "web_optimize",
];

#[test]
fn list_operations_returns_every_op_and_the_presets() {
    let (_ws, tools) = tools();
    let result = tools.list_operations(&ListOperationsParams::default());
    let out = structured(&result);
    let body = text(&result);

    assert_eq!(out["count"], 21);
    let names: Vec<&str> = out["operations"]
        .as_array()
        .expect("operations must be an array")
        .iter()
        .map(|op| op["name"].as_str().unwrap())
        .collect();
    for expected in V03_OPERATIONS {
        assert!(
            names.contains(&expected),
            "missing op {expected} in catalog"
        );
        assert!(
            body.contains(expected),
            "missing op {expected} in the text catalog"
        );
    }

    let presets: Vec<&str> = out["presets"]
        .as_array()
        .expect("presets must be an array")
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(presets, BUILTIN_PRESETS);
    for preset in BUILTIN_PRESETS {
        assert!(
            body.contains(preset),
            "preset {preset} must appear in the text catalog"
        );
    }

    // 代表的なパラメータのヒントが載っていること(型・値域つき)。
    assert!(body.contains("angle_degrees"));
    assert!(body.contains("sigma"));
    assert!(body.contains("0.1..100"));

    // 段階的開示: カタログはトークン的に軽いこと
    // (v0.8 で 21 op + 11 op に付く mask の1行、目安 ~1150 tokens ≒ 4800 chars)。
    assert!(
        body.len() < 4800,
        "the catalog must stay compact, got {} chars",
        body.len()
    );
    assert!(out["next"].as_str().unwrap().contains("explain_operation"));
}

#[test]
fn list_operations_filters_by_category_and_rejects_unknown_ones() {
    let (_ws, tools) = tools();
    let filtered = structured(&tools.list_operations(&ListOperationsParams {
        category: Some("filter".to_string()),
    }));
    let names: Vec<&str> = filtered["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "blur",
            "median",
            "unsharp_mask",
            "convolve",
            "clone",
            "heal",
            "svg_overlay"
        ]
    );
    // プリセットは分類で絞っても常に出す(語彙の圧縮層は分類に属さない)。
    assert_eq!(
        filtered["presets"].as_array().unwrap().len(),
        BUILTIN_PRESETS.len()
    );

    let invalid = tools.list_operations(&ListOperationsParams {
        category: Some("vibes".to_string()),
    });
    let payload = error_payload(&invalid);
    assert_eq!(payload["error"]["code"], "invalid_category");
    assert!(payload["error"]["details"]["valid_values"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "geometry"));
}

#[test]
fn explain_operation_returns_the_full_parameter_table() {
    let (_ws, tools) = tools();

    let rotate = tools.explain_operation(&ExplainOperationParams {
        operation: "rotate".to_string(),
    });
    let out = structured(&rotate);
    let body = text(&rotate);
    assert_eq!(out["name"], "rotate");
    let params: Vec<&str> = out["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(params, ["angle_degrees", "crop"]);
    assert!(body.contains("angle_degrees"));
    assert!(body.contains("crop"));
    // 落とし穴: 内接矩形クロップで縮むこと。
    assert!(
        out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("SHRINK")),
        "rotate must warn about the inscribed-rect shrink"
    );
    assert!(!out["examples"].as_array().unwrap().is_empty());

    let curves = tools.explain_operation(&ExplainOperationParams {
        operation: "curves".to_string(),
    });
    let out = structured(&curves);
    let params: Vec<&str> = out["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    // v0.5: 局所適用マスクが調整系 op の共有パラメータとして最後に並ぶ。
    assert_eq!(params, ["master", "red", "green", "blue", "mask"]);
    assert!(
        out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("strictly increasing")),
        "curves must warn about the monotonic x requirement"
    );

    // crop は coordinate_space の source 意味論を説明していること。
    let crop = structured(&tools.explain_operation(&ExplainOperationParams {
        operation: "crop".to_string(),
    }));
    let semantics = crop["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "coordinate_space")
        .expect("crop must document coordinate_space")["semantics"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(semantics.contains("ORIGINAL"));

    // 全 op が説明可能であること。
    for op in V03_OPERATIONS {
        let result = tools.explain_operation(&ExplainOperationParams {
            operation: op.to_string(),
        });
        assert_ne!(result.is_error, Some(true), "{op} must be explainable");
    }
}

/// lut の説明は「先に .cube を import_asset する」ワークフローを明示すること
/// (v0.3 のアセット参照は、この一手を飛ばすと必ず失敗する)。
#[test]
fn explain_lut_documents_the_import_first_workflow() {
    let (_ws, tools) = tools();
    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "lut".to_string(),
    });
    let out = structured(&result);
    let body = text(&result);
    assert_eq!(out["name"], "lut");
    assert_eq!(out["category"], "color");

    let params: Vec<&str> = out["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(params, ["lut_revision_id", "strength", "mask"]);

    assert!(
        body.contains("import_asset"),
        "explain_operation(\"lut\") must spell out the import_asset step: {body}"
    );
    assert!(
        out["params"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["semantics"].as_str().unwrap().contains("import_asset")),
        "the lut_revision_id semantics must name import_asset"
    );
    assert!(
        out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("import_asset")),
        "lut must warn that the LUT has to be imported first"
    );
}

#[test]
fn explain_operation_rejects_unknown_names_with_the_valid_list() {
    let (_ws, tools) = tools();
    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "blurr".to_string(),
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "unknown_operation");
    let valid: Vec<&str> = payload["error"]["details"]["valid_values"]
        .as_array()
        .expect("valid_values must be listed")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(valid.len(), 21);
    for op in V03_OPERATIONS {
        assert!(valid.contains(&op));
    }
    let suggestions = payload["error"]["details"]["did_you_mean"]
        .as_array()
        .expect("did_you_mean must be present");
    assert!(
        suggestions.iter().any(|s| s == "blur"),
        "\"blurr\" should suggest blur, got {suggestions:?}"
    );
}

/// 埋め込みプリセットは名前・説明・レシピを持ち、レシピとしてデシリアライズできること。
/// atx-core の validate は v0.1 op のみのプリセットで確認する
/// (color 系 op の validate は v0.2 の並行作業で実装中)。
#[test]
fn embedded_presets_are_well_formed() {
    let presets = atx_mcp::presets::all();
    assert_eq!(presets.len(), BUILTIN_PRESETS.len());
    for preset in &presets {
        assert!(BUILTIN_PRESETS.contains(&preset.name.as_str()));
        assert!(!preset.description.is_empty());
        assert!(!preset.recipe.operations.is_empty());
    }
    for name in ["eyecatch_16_9", "thumbnail_square", "web_optimize"] {
        let preset = atx_mcp::presets::resolve(name).expect("preset must resolve");
        atx_core::recipe::validate(&preset.recipe)
            .unwrap_or_else(|e| panic!("preset {name} must pass atx-core validate: {e}"));
    }
}

/// v0.3 op(white_balance)を使うプリセットの validate。
#[test]
fn v03_presets_pass_core_validate() {
    let preset = atx_mcp::presets::resolve("product_clean").expect("preset must resolve");
    atx_core::recipe::validate(&preset.recipe)
        .unwrap_or_else(|e| panic!("preset product_clean must pass atx-core validate: {e}"));
}

/// color_matrix / curves を使うプリセットの validate(v0.2 の color op で実装済み)。
#[test]
fn color_presets_pass_core_validate() {
    for name in ["film_soft", "grayscale", "sepia"] {
        let preset = atx_mcp::presets::resolve(name).expect("preset must resolve");
        atx_core::recipe::validate(&preset.recipe)
            .unwrap_or_else(|e| panic!("preset {name} must pass atx-core validate: {e}"));
    }
}

/// preset での apply_transform が end-to-end で通り、かつ「解決後のレシピの生指定」と
/// 同じ revision(= 同じ recipe_hash)に落ちること = プリセットは純粋な糖衣。
#[test]
fn preset_apply_is_pure_sugar_over_the_resolved_recipe() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: rev.clone(),
        recipe: None,
        preset: Some("web_optimize".to_string()),
    }));
    assert_eq!(applied["reused"], Value::Bool(false));
    assert_eq!(applied["revision"]["mime_type"], "image/webp");
    let preset_hash = applied["recipe_hash"].as_str().unwrap().to_string();
    let preset_revision = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 同じ内容を生レシピで渡すと、冪等ショートサーキットで同じ revision が返る。
    let resolved = atx_mcp::presets::resolve("web_optimize").unwrap().recipe;
    let raw = structured(&tools.apply_transform(&TransformParams {
        revision_id: rev.clone(),
        recipe: Some(resolved),
        preset: None,
    }));
    assert_eq!(raw["recipe_hash"].as_str().unwrap(), preset_hash);
    assert_eq!(raw["revision"]["revision_id"], preset_revision.as_str());
    assert_eq!(raw["reused"], Value::Bool(true));

    // render_preview も preset を受ける。
    let preview = structured(&tools.render_preview(&RenderPreviewParams {
        revision_id: rev,
        recipe: None,
        preset: Some("web_optimize".to_string()),
        overlay: None,
        mask_revision_id: None,
    }));
    assert_eq!(preview["recipe_hash"].as_str().unwrap(), preset_hash);
    assert_eq!(preview["mime_type"], "image/jpeg");
}

#[test]
fn recipe_and_preset_are_mutually_exclusive_and_one_is_required() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [{"op": "encode", "format": "png"}]
    }))
    .unwrap();

    let both = tools.apply_transform(&TransformParams {
        revision_id: rev.clone(),
        recipe: Some(recipe.clone()),
        preset: Some("web_optimize".to_string()),
    });
    assert_eq!(
        error_payload(&both)["error"]["code"],
        "recipe_and_preset_conflict"
    );

    let neither = tools.apply_transform(&TransformParams {
        revision_id: rev.clone(),
        recipe: None,
        preset: None,
    });
    let payload = error_payload(&neither);
    assert_eq!(payload["error"]["code"], "recipe_or_preset_required");
    assert!(payload["error"]["details"]["valid_presets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "web_optimize"));

    let unknown = tools.apply_transform(&TransformParams {
        revision_id: rev.clone(),
        recipe: None,
        preset: Some("filmic_dream".to_string()),
    });
    let payload = error_payload(&unknown);
    assert_eq!(payload["error"]["code"], "unknown_preset");
    let valid: Vec<&str> = payload["error"]["details"]["valid_values"]
        .as_array()
        .expect("unknown preset errors must list the valid names")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(valid, BUILTIN_PRESETS);

    // render_preview も同じ契約。
    let preview_neither = tools.render_preview(&RenderPreviewParams {
        revision_id: rev,
        recipe: None,
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    assert_eq!(
        error_payload(&preview_neither)["error"]["code"],
        "recipe_or_preset_required"
    );
}

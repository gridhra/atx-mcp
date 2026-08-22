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

const V03_OPERATIONS: [&str; 27] = [
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
    "flip",
    "vignette",
    "grain",
    "gradient_map",
    "pixelate",
    "auto_levels",
    "encode",
    "strip_metadata",
];

const BUILTIN_PRESETS: [&str; 30] = [
    "architecture_clean",
    "bw_high_contrast",
    "bw_neutral",
    "bw_red_filter",
    "bw_soft",
    "cinema_teal_orange",
    "duotone_navy_cream",
    "eyecatch_16_9",
    "film_cool",
    "film_grain_strong",
    "film_soft",
    "film_warm",
    "food_vivid",
    "grain_fine",
    "grayscale",
    "hero_2400",
    "instagram_portrait_4_5",
    "instagram_square_1080",
    "landscape_punch",
    "matte_fade",
    "og_1200x630",
    "portrait_soft",
    "product_clean",
    "product_white",
    "sepia",
    "soft_vignette",
    "thumbnail_square",
    "web_optimize",
    "x_wide_16_9",
    "youtube_thumb_1280x720",
];

#[test]
fn list_operations_returns_every_op_and_the_presets() {
    let (_ws, tools) = tools();
    let result = tools.list_operations(&ListOperationsParams::default());
    let out = structured(&result);
    let body = text(&result);

    assert_eq!(out["count"], 27);
    let names: Vec<&str> = out["ops"]
        .as_array()
        .expect("ops must be an array")
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
    // structuredContent は機械可読の最小形(name + category のみ)。
    // 散文(summary / params)はテキストサマリ側にだけ載る。
    for op in out["ops"].as_array().unwrap() {
        let mut keys: Vec<&str> = op.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["category", "name"], "op entry must stay compact");
    }

    let presets: Vec<&str> = out["presets"]
        .as_array()
        .expect("presets must be an array")
        .iter()
        .map(|p| p.as_str().expect("presets must be plain names"))
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
    // (v0.3.0 で 27 op + 14 op に付く mask の1行 + 30 プリセット名、目安 ~2100 tokens ≒ 8700 chars)。
    assert!(
        body.len() < 8700,
        "the catalog must stay compact, got {} chars",
        body.len()
    );
    assert!(body.contains("explain_operation"));

    // 二重払いの禁止: structuredContent はテキストの散文を繰り返さないこと。
    // (以前は text と structured の両方に summary + params を積んでいて、
    //  1 呼び出しで ~4.7k tokens を焼いていた。)
    let structured_len = serde_json::to_string(&out).unwrap().len();
    assert!(
        structured_len < body.len() / 3,
        "structuredContent must stay a compact machine form, got {structured_len} chars vs {} of text",
        body.len()
    );
}

#[test]
fn list_operations_filters_by_category_and_rejects_unknown_ones() {
    let (_ws, tools) = tools();
    let filtered = structured(&tools.list_operations(&ListOperationsParams {
        category: Some("filter".to_string()),
    }));
    let names: Vec<&str> = filtered["ops"]
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
            "svg_overlay",
            "vignette",
            "grain",
            "pixelate",
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
    assert_eq!(valid.len(), 27);
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
    // 全 30 プリセットが atx-core の validate を通ること(名前を手で並べない)。
    for name in BUILTIN_PRESETS {
        let preset = atx_mcp::presets::resolve(name).expect("preset must resolve");
        atx_core::recipe::validate(&preset.recipe)
            .unwrap_or_else(|e| panic!("preset {name} must pass atx-core validate: {e}"));
    }
}

/// 「正確な寸法」を約束するプリセットは without_enlargement=false であること
/// (true だと小さい入力で黙って約束を破る)。
#[test]
fn exact_size_presets_do_not_refuse_to_enlarge() {
    for name in [
        "eyecatch_16_9",
        "thumbnail_square",
        "og_1200x630",
        "instagram_square_1080",
        "instagram_portrait_4_5",
        "x_wide_16_9",
        "youtube_thumb_1280x720",
    ] {
        let preset = atx_mcp::presets::resolve(name).expect("preset must resolve");
        let json = serde_json::to_value(&preset.recipe).unwrap();
        let resize = json["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|op| op["op"] == "resize")
            .unwrap_or_else(|| panic!("preset {name} must contain a resize op"));
        assert_eq!(
            resize["without_enlargement"],
            Value::Bool(false),
            "preset {name} promises an exact size, so it must be allowed to upscale"
        );
    }
}

/// 単体 op のビルディングブロックは「プリセットの連鎖」を約束しないこと
/// (ツール面は preset XOR recipe で、2 回適用すれば二重にロスあり再エンコードされる)。
#[test]
fn building_block_presets_do_not_promise_chaining() {
    for name in ["grain_fine", "soft_vignette"] {
        let preset = atx_mcp::presets::resolve(name).expect("preset must resolve");
        assert!(
            !preset.description.contains("for stacking"),
            "preset {name} must not promise preset stacking: {}",
            preset.description
        );
        assert!(
            preset.description.contains("recipe"),
            "preset {name} must point at copying the op into a recipe: {}",
            preset.description
        );
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

/// vocab.rs が書いている値域が、atx-core の validate と実際に一致していること
/// (境界のすぐ外がエラーになる = ドキュメントが嘘をついていない)。
#[test]
fn documented_ranges_match_core_validate() {
    let reject = |json: serde_json::Value, what: &str| {
        let recipe: atx_core::TransformRecipe =
            serde_json::from_value(json).expect("must deserialize");
        assert!(
            atx_core::recipe::validate(&recipe).is_err(),
            "{what} must be rejected by atx-core validate"
        );
    };
    let accept = |json: serde_json::Value, what: &str| {
        let recipe: atx_core::TransformRecipe =
            serde_json::from_value(json).expect("must deserialize");
        atx_core::recipe::validate(&recipe)
            .unwrap_or_else(|e| panic!("{what} must be accepted by atx-core validate: {e}"));
    };

    // perspective: |degrees| <= 45
    accept(
        serde_json::json!({"operations": [{"op": "perspective", "vertical_degrees": 45.0}]}),
        "vertical_degrees = 45",
    );
    reject(
        serde_json::json!({"operations": [{"op": "perspective", "vertical_degrees": 45.1}]}),
        "vertical_degrees = 45.1",
    );
    reject(
        serde_json::json!({"operations": [{"op": "perspective", "horizontal_degrees": -46.0}]}),
        "horizontal_degrees = -46",
    );
    // perspective: quad は tl,tr,br,bl の順で厳密に凸(逆順は順序エラー)
    reject(
        serde_json::json!({"operations": [{"op": "perspective",
            "quad": [[0.0,0.0],[0.0,10.0],[10.0,10.0],[10.0,0.0]]}]}),
        "a reversed-order quad",
    );

    // color_matrix: |v| <= 8
    let mut matrix = vec![0.0f64; 20];
    matrix[0] = 8.0;
    accept(
        serde_json::json!({"operations": [{"op": "color_matrix", "matrix": matrix.clone()}]}),
        "matrix element 8.0",
    );
    matrix[0] = 8.1;
    reject(
        serde_json::json!({"operations": [{"op": "color_matrix", "matrix": matrix}]}),
        "matrix element 8.1",
    );

    // curves: 1..=32 points per channel
    let points: Vec<[u16; 2]> = (0..=32u16).map(|i| [i * 7, i * 7]).collect();
    reject(
        serde_json::json!({"operations": [{"op": "curves", "master": points}]}),
        "33 control points",
    );
    reject(
        serde_json::json!({"operations": [{"op": "curves", "master": []}]}),
        "an empty control point list",
    );

    // levels: out_black <= out_white(反転は不可、潰しは可)
    accept(
        serde_json::json!({"operations": [{"op": "levels", "out_black": 100, "out_white": 100}]}),
        "out_black == out_white",
    );
    reject(
        serde_json::json!({"operations": [{"op": "levels", "out_black": 200, "out_white": 100}]}),
        "out_black > out_white",
    );

    // clone / heal: radius 1..=2048, feather_px 0..=200
    for op in ["clone", "heal"] {
        accept(
            serde_json::json!({"operations": [{"op": op, "src_x": 0, "src_y": 0, "dest_x": 1,
                "dest_y": 1, "radius": 2048, "feather_px": 200.0}]}),
            "radius 2048 / feather 200",
        );
        reject(
            serde_json::json!({"operations": [{"op": op, "src_x": 0, "src_y": 0, "dest_x": 1,
                "dest_y": 1, "radius": 2049}]}),
            "radius 2049",
        );
        reject(
            serde_json::json!({"operations": [{"op": op, "src_x": 0, "src_y": 0, "dest_x": 1,
                "dest_y": 1, "radius": 8, "feather_px": 200.1}]}),
            "feather_px 200.1",
        );
        // feather_px は省略可能(既定 0)
        accept(
            serde_json::json!({"operations": [{"op": op, "src_x": 0, "src_y": 0, "dest_x": 1,
                "dest_y": 1, "radius": 8}]}),
            "an omitted feather_px",
        );
    }
}

/// list_operations の総ペイロード(テキスト + structuredContent)のサイズを記録する。
/// `cargo test -p atx-mcp --test vocab -- --nocapture list_operations_payload_size`
#[test]
fn list_operations_payload_size() {
    let (_ws, tools) = tools();
    let result = tools.list_operations(&ListOperationsParams::default());
    let body = text(&result);
    let structured_json = serde_json::to_string(&structured(&result)).unwrap();
    println!(
        "list_operations: text {} chars + structuredContent {} chars = {} chars total (~{} tokens)",
        body.len(),
        structured_json.len(),
        body.len() + structured_json.len(),
        (body.len() + structured_json.len()) / 4
    );
    assert!(body.len() + structured_json.len() < 11_000);
}

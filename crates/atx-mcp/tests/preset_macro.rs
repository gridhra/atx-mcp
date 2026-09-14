//! レシピ内プリセットマクロ `{"op":"preset","name":"<preset>"}` の統合テスト。
//!
//! マクロは MCP 層だけの糖衣で、atx-core の DSL には `preset` op は無い。
//! `apply_transform` / `render_preview` が deserialize の前にその場で展開するので、
//! 「プリセット名で呼んだ場合」「マクロで書いた場合」「手で展開して書いた場合」の
//! 3 つは同じ revision(同じ recipe_hash)に落ちる。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, ExplainOperationParams, ImportAssetParams, RecipeJson, RenderPreviewParams,
    TransformParams,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{json, Value};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/synthetic_scene.jpg")
        .canonicalize()
        .expect("fixture image must exist")
}

fn tools() -> (tempfile::TempDir, AtxTools) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    (workspace, tools)
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

#[track_caller]
fn text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected a text summary, got {other:?}"),
    }
}

/// fixture を取り込んで revision_id を返す。
fn import_fixture(tools: &AtxTools) -> String {
    let out = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    out["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string()
}

/// プリセットの `operations` を JSON 配列として取り出す(手で展開したレシピを組むため)。
fn preset_ops(name: &str) -> Vec<Value> {
    let preset = atx_mcp::presets::resolve(name).expect("built-in preset");
    preset
        .recipe
        .operations
        .iter()
        .map(|op| serde_json::to_value(op).expect("an operation always serializes"))
        .collect()
}

fn apply(tools: &AtxTools, revision_id: &str, recipe: Value) -> CallToolResult {
    tools.apply_transform(&TransformParams {
        revision_id: Some(revision_id.to_string()),
        revision_ids: None,
        recipe: Some(RecipeJson(recipe)),
        preset: None,
    })
}

/// プリセット名で呼んだ場合・マクロで書いた場合・手で展開した場合が同一 revision。
#[test]
fn the_macro_the_preset_argument_and_the_expanded_recipe_agree() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let by_name = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(rev.clone()),
        revision_ids: None,
        recipe: None,
        preset: Some("ocr_document".to_string()),
    }));
    let by_macro = structured(&apply(
        &tools,
        &rev,
        json!({"operations": [{"op": "preset", "name": "ocr_document"}]}),
    ));
    let by_hand = structured(&apply(
        &tools,
        &rev,
        json!({"operations": preset_ops("ocr_document")}),
    ));

    assert_eq!(
        by_macro["revision"]["revision_id"], by_name["revision"]["revision_id"],
        "the macro must hash like the preset argument: {by_macro} vs {by_name}"
    );
    assert_eq!(
        by_hand["revision"]["revision_id"], by_name["revision"]["revision_id"],
        "the hand-expanded recipe must hash the same too"
    );
    assert_eq!(by_macro["recipe_hash"], by_name["recipe_hash"]);
    // 2番目・3番目の呼び出しは冪等ショートサーキットに落ちる。
    assert_eq!(by_macro["reused"], Value::Bool(true));
    assert_eq!(by_hand["reused"], Value::Bool(true));
}

/// マクロは自分の op に挟んで使える(前後の op はそのまま残る)。
#[test]
fn the_macro_splices_into_the_surrounding_operations() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let with_macro = structured(&apply(
        &tools,
        &rev,
        json!({"operations": [
            {"op": "resize", "width": 400, "fit": "contain"},
            {"op": "preset", "name": "grayscale"},
            {"op": "encode", "format": "png"},
        ]}),
    ));
    let mut hand = vec![json!({"op": "resize", "width": 400, "fit": "contain"})];
    hand.extend(preset_ops("grayscale"));
    hand.push(json!({"op": "encode", "format": "png"}));
    let expanded = structured(&apply(&tools, &rev, json!({"operations": hand})));

    assert_eq!(
        with_macro["revision"]["revision_id"],
        expanded["revision"]["revision_id"]
    );
    assert_eq!(with_macro["revision"]["mime_type"], "image/png");
    assert_eq!(with_macro["revision"]["width"], 400);
}

/// `layers[].ops` の中でも展開される。
#[test]
fn the_macro_expands_inside_a_layer() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let layered = |ops: Value| {
        json!({
            "layers": [
                {"source": "base"},
                {"source": "base", "ops": ops, "blend_mode": "multiply", "opacity": 0.5},
            ],
            "operations": [{"op": "encode", "format": "png"}],
        })
    };

    let with_macro = structured(&apply(
        &tools,
        &rev,
        layered(json!([{"op": "preset", "name": "grayscale"}])),
    ));
    let expanded = structured(&apply(
        &tools,
        &rev,
        layered(Value::Array(preset_ops("grayscale"))),
    ));
    assert_eq!(
        with_macro["revision"]["revision_id"], expanded["revision"]["revision_id"],
        "a macro inside layers[].ops must expand to the same recipe"
    );
}

/// `name` が無いマクロは、有効なプリセット一覧付きの構造化エラー。
#[test]
fn a_macro_without_a_name_says_so() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let payload = error_payload(&apply(
        &tools,
        &rev,
        json!({"operations": [{"op": "preset"}]}),
    ));
    assert_eq!(payload["error"]["code"], "preset_macro_missing_name");
    assert_eq!(payload["error"]["details"]["location"], "operations[0]");
    let valid = payload["error"]["details"]["valid_presets"]
        .as_array()
        .expect("valid_presets must be listed");
    assert!(valid.iter().any(|v| v == "ocr_document"), "{payload}");
}

/// 未知のプリセット名は `preset` 引数と同じ `unknown_preset` で返る。
#[test]
fn an_unknown_preset_name_in_a_macro_matches_the_preset_argument_error() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let from_macro = error_payload(&apply(
        &tools,
        &rev,
        json!({"operations": [{"op": "preset", "name": "nope"}]}),
    ));
    let from_argument = error_payload(&tools.apply_transform(&TransformParams {
        revision_id: Some(rev.clone()),
        revision_ids: None,
        recipe: None,
        preset: Some("nope".to_string()),
    }));
    assert_eq!(from_macro["error"]["code"], "unknown_preset");
    assert_eq!(from_macro["error"], from_argument["error"]);
}

/// 展開後の位置を指す validate エラーに、展開元プリセット名が付く。
#[test]
fn an_error_inside_an_expansion_names_the_preset_it_came_from() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    // web_optimize は encode で終わるので、その後ろに op を足すと
    // 「encode は最後でなければならない」で落ちる(位置は展開後の添字)。
    let payload = error_payload(&apply(
        &tools,
        &rev,
        json!({"operations": [
            {"op": "preset", "name": "web_optimize"},
            {"op": "blur", "sigma": 2.0},
        ]}),
    ));
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("operations[1]") && message.contains("encode"),
        "the message must still point at the expanded index: {message}"
    );
    assert!(
        message.ends_with("(expanded from preset \"web_optimize\")"),
        "the message must name the preset the operation came from: {message}"
    );
    assert_eq!(
        payload["error"]["details"]["expanded_from_preset"],
        "web_optimize"
    );

    // 展開に関係ない位置のエラーには付かない。
    let unrelated = error_payload(&apply(
        &tools,
        &rev,
        json!({"operations": [
            {"op": "encode", "format": "png"},
            {"op": "preset", "name": "grayscale"},
        ]}),
    ));
    let message = unrelated["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("expanded from preset"),
        "operations[0] is the user's own op: {message}"
    );
}

/// `render_preview` でもマクロは効き、recipe_hash は apply と一致する。
#[test]
fn render_preview_expands_macros_too() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let preview = structured(&tools.render_preview(&RenderPreviewParams {
        revision_id: rev.clone(),
        recipe: Some(RecipeJson(
            json!({"operations": [{"op": "preset", "name": "grayscale"}]}),
        )),
        preset: None,
        overlay: None,
        mask_revision_id: None,
        long_edge: None,
    }));
    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(rev.clone()),
        revision_ids: None,
        recipe: None,
        preset: Some("grayscale".to_string()),
    }));
    assert_eq!(
        preview["recipe_hash"], applied["recipe_hash"],
        "the preview hashes the expanded recipe, like apply_transform"
    );
}

/// `explain_operation "preset"` はマクロの説明を返す(unknown 扱いにしない)。
#[test]
fn explain_operation_documents_the_preset_macro() {
    let (_ws, tools) = tools();
    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "preset".to_string(),
    });
    let out = structured(&result);
    assert_eq!(out["name"], "preset");
    assert_eq!(out["kind"], "operation");
    let example = out["examples"]
        .as_array()
        .expect("examples")
        .first()
        .and_then(|v| v.as_str())
        .expect("one example");
    assert!(
        example.contains("\"op\": \"preset\""),
        "the example must be pasteable JSON: {example}"
    );
    let summary = text(&result);
    assert!(summary.contains("preset"), "{summary}");

    // カタログ(list_operations の op 一覧)には入れない = op 語彙は 29 件のまま。
    assert!(
        atx_mcp::vocab::find("preset").is_none(),
        "the macro must not be part of the OPERATIONS table"
    );
}

/// `list_operations` のプリセット節がマクロの書き方を示すこと。
#[test]
fn the_catalog_mentions_the_macro() {
    let (_ws, tools) = tools();
    let summary = text(&tools.list_operations(&atx_mcp::tools::ListOperationsParams::default()));
    assert!(
        summary.contains("{\"op\":\"preset\",\"name\":\"...\"}"),
        "the presets section must show how to inline one: {summary}"
    );
}

/// レイヤー内にあるユーザー自身の op のエラーが、無関係なプリセットに帰属されない。
///
/// 以前は「エラーメッセージが `layers[` で始まるか」で位置を読んでいたが、実際の文言は
/// `invalid recipe: ` / `invalid recipe at ` で始まるのでレイヤー分岐が一度も成立せず、
/// `layers[1].ops: operations[0] ...` を**トップレベルの** `operations[0]` と読んでいた。
/// その結果、トップレベルに置いたマクロの名前がレイヤー内のエラーに付いていた。
#[test]
fn an_error_in_a_layer_is_not_blamed_on_a_top_level_macro() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    let payload = error_payload(&apply(
        &tools,
        &rev,
        json!({
            "layers": [
                {"source": "base"},
                // sigma は 0.1..=100.0。これは利用者が自分で書いた不正な op。
                {"source": "base", "ops": [{"op": "blur", "sigma": 0.0}]},
            ],
            // トップレベルにマクロ(展開は operations[0..2])。
            "operations": [{"op": "preset", "name": "grayscale"}],
        }),
    ));
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("layers[1].ops") && message.contains("blur"),
        "the message must point at the layer's own op: {message}"
    );
    assert!(
        !message.contains("expanded from preset"),
        "layers[1].ops[0] is the user's own op, not part of any expansion: {message}"
    );
    assert_eq!(
        payload["error"]["details"]["expanded_from_preset"],
        Value::Null,
        "{payload}"
    );
}

/// レイヤー内のマクロ展開で不正になった場合は、そのレイヤーの位置で展開元が付く。
#[test]
fn an_error_inside_a_layer_expansion_names_the_preset() {
    let (_ws, tools) = tools();
    let rev = import_fixture(&tools);

    // web_optimize は encode で終わる。encode は仕上げパス専用なので
    // layers[].ops の中では不正になる(展開後の位置 layers[1].ops[1])。
    let payload = error_payload(&apply(
        &tools,
        &rev,
        json!({
            "layers": [
                {"source": "base"},
                {"source": "base", "ops": [{"op": "preset", "name": "web_optimize"}]},
            ],
            "operations": [{"op": "encode", "format": "png"}],
        }),
    ));
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("layers[1].ops"),
        "the message must keep the expanded position: {message}"
    );
    assert!(
        message.ends_with("(expanded from preset \"web_optimize\")"),
        "the message must name the preset the operation came from: {message}"
    );
    assert_eq!(
        payload["error"]["details"]["expanded_from_preset"],
        "web_optimize"
    );
}

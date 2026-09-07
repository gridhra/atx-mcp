//! 実運用フィードバック(28 枚の一括処理セッション)から出た改善の統合テスト。
//!
//! - バッチ import / apply(1件の失敗でバッチを止めない)
//! - 不透明レシピ(inputSchema から op 語彙を外した代わりの、実行時エラーの質)
//! - 二重適用検出(export したバイト列を再 import したときの警告)
//! - プリセットの可視化(explain_operation がプリセット名も受ける)

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, ExplainOperationParams, ExportAssetParams, ImportAssetParams, ListOperationsParams,
    RecipeJson, TransformParams,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

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

fn recipe(json: Value) -> RecipeJson {
    RecipeJson(json)
}

/// 同じ画像の「見た目は同じでバイト列が違う」コピーを n 枚作る
/// (バッチが入力順を保つこと・件数を数えることを確かめるための素材)。
fn copies(dir: &std::path::Path, n: usize) -> Vec<String> {
    let bytes = std::fs::read(fixture()).expect("fixture");
    (0..n)
        .map(|i| {
            let path = dir.join(format!("copy_{i}.jpg"));
            let mut with_tail = bytes.clone();
            // JPEG の EOI 以降のゴミはデコーダに無視されるが sha256 は変わる。
            with_tail.extend(std::iter::repeat_n(0u8, i + 1));
            std::fs::write(&path, &with_tail).expect("write copy");
            path.to_string_lossy().into_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// FIX 1: バッチ
// ---------------------------------------------------------------------------

#[test]
fn batch_import_keeps_going_after_a_bad_path() {
    let (workspace, tools) = tools();
    let mut paths = copies(workspace.path(), 3);
    paths.insert(2, "/definitely/not/here.jpg".to_string());

    let result = tools.import_asset(&ImportAssetParams::batch(paths.clone()));
    let out = structured(&result);

    assert_eq!(out["count"], 3, "3 of 4 must have been imported: {out}");
    let imported = out["imported"].as_array().expect("imported array");
    // 入力順が保たれる(失敗した1件を除いた順序)。
    let seen: Vec<&str> = imported
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(seen, vec![&paths[0][..], &paths[1][..], &paths[3][..]]);
    for entry in imported {
        assert!(entry["revision"]["revision_id"]
            .as_str()
            .unwrap()
            .starts_with("rev_"));
        assert_eq!(entry["reused"], Value::Bool(false));
    }

    let failed = out["failed"].as_array().expect("failed array");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["path"], "/definitely/not/here.jpg");
    assert_eq!(failed[0]["error"]["code"], "path_not_found");

    let summary = text(&result);
    assert!(summary.contains("Imported 3 of 4"), "{summary}");
    assert!(summary.contains("failed:"), "{summary}");
}

#[test]
fn batch_import_reports_reuse_and_fails_only_when_nothing_worked() {
    let (workspace, tools) = tools();
    let paths = copies(workspace.path(), 2);
    let _ = structured(&tools.import_asset(&ImportAssetParams::batch(paths.clone())));

    // 2回目は両方とも冪等ヒット。
    let again = structured(&tools.import_asset(&ImportAssetParams::batch(paths)));
    assert_eq!(again["count"], 2);
    for entry in again["imported"].as_array().unwrap() {
        assert_eq!(entry["reused"], Value::Bool(true));
    }

    // 全滅したときだけツールとしてエラーになり、全件の理由が並ぶ。
    let all_bad = tools.import_asset(&ImportAssetParams::batch(vec![
        "/nope/a.jpg",
        "/nope/b.jpg",
    ]));
    let payload = error_payload(&all_bad);
    assert_eq!(payload["error"]["code"], "import_failed");
    assert_eq!(
        payload["error"]["details"]["failed"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(message.contains("/nope/a.jpg") && message.contains("/nope/b.jpg"));
}

#[test]
fn import_requires_exactly_one_of_path_and_paths() {
    let (_ws, tools) = tools();

    let neither = tools.import_asset(&ImportAssetParams {
        path: None,
        paths: None,
    });
    assert_eq!(
        error_payload(&neither)["error"]["code"],
        "path_or_paths_required"
    );

    let both = tools.import_asset(&ImportAssetParams {
        path: Some(fixture().to_string_lossy().into_owned()),
        paths: Some(vec![fixture().to_string_lossy().into_owned()]),
    });
    assert_eq!(
        error_payload(&both)["error"]["code"],
        "path_and_paths_conflict"
    );

    let empty = tools.import_asset(&ImportAssetParams::batch(Vec::<String>::new()));
    assert_eq!(error_payload(&empty)["error"]["code"], "invalid_batch_size");
}

#[test]
fn batch_apply_applies_one_recipe_to_many_revisions() {
    let (workspace, tools) = tools();
    let paths = copies(workspace.path(), 3);
    let imported = structured(&tools.import_asset(&ImportAssetParams::batch(paths)));
    let mut revision_ids: Vec<String> = imported["imported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["revision"]["revision_id"].as_str().unwrap().to_string())
        .collect();
    // 存在しない revision を1件混ぜても、他は処理される。
    revision_ids.insert(1, "rev_DOES_NOT_EXIST".to_string());

    let result = tools.apply_transform(&TransformParams {
        revision_id: None,
        revision_ids: Some(revision_ids.clone()),
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "resize", "width": 320, "fit": "contain"}]
        }))),
        preset: None,
    });
    let out = structured(&result);

    assert_eq!(out["count"], 4);
    assert_eq!(out["succeeded"], 3);
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    // 入力順が保たれ、失敗した要素だけが error を持つ。
    for (i, entry) in results.iter().enumerate() {
        assert_eq!(entry["revision_id"], revision_ids[i].as_str());
        if i == 1 {
            assert_eq!(entry["error"]["code"], "revision_not_found");
            assert!(entry["revision"].is_null());
        } else {
            assert_eq!(entry["reused"], Value::Bool(false));
            assert_eq!(entry["revision"]["width"], 320);
        }
    }
    assert!(out["recipe_hash"].as_str().unwrap().len() >= 8);
    assert!(out["warnings"].is_array());
    assert!(text(&result).contains("FAILED"));

    // 冪等ショートサーキットは revision ごとに個別に効く。
    let again = structured(&tools.apply_transform(&TransformParams {
        revision_id: None,
        revision_ids: Some(revision_ids.clone()),
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "resize", "width": 320, "fit": "contain"}]
        }))),
        preset: None,
    }));
    for (i, entry) in again["results"].as_array().unwrap().iter().enumerate() {
        if i != 1 {
            assert_eq!(entry["reused"], Value::Bool(true), "{entry}");
        }
    }
}

#[test]
fn batch_apply_fails_only_when_every_revision_failed() {
    let (_ws, tools) = tools();
    let result = tools.apply_transform(&TransformParams {
        revision_id: None,
        revision_ids: Some(vec!["rev_a".to_string(), "rev_b".to_string()]),
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "auto_orient"}]
        }))),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "apply_failed");
    assert_eq!(
        payload["error"]["details"]["failed"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn apply_requires_exactly_one_of_revision_id_and_revision_ids() {
    let (_ws, tools) = tools();
    let with = |revision_id: Option<String>, revision_ids: Option<Vec<String>>| TransformParams {
        revision_id,
        revision_ids,
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "auto_orient"}]
        }))),
        preset: None,
    };

    assert_eq!(
        error_payload(&tools.apply_transform(&with(None, None)))["error"]["code"],
        "revision_id_or_revision_ids_required"
    );
    assert_eq!(
        error_payload(&tools.apply_transform(&with(
            Some("rev_x".to_string()),
            Some(vec!["rev_y".to_string()])
        )))["error"]["code"],
        "revision_id_and_revision_ids_conflict"
    );
    assert_eq!(
        error_payload(&tools.apply_transform(&with(None, Some(Vec::new()))))["error"]["code"],
        "invalid_batch_size"
    );
}

// ---------------------------------------------------------------------------
// FIX 2: 不透明レシピでもエラーは場所を名指しする
// ---------------------------------------------------------------------------

#[test]
fn a_typo_in_an_op_name_names_the_index_and_suggests_the_right_one() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = tools.apply_transform(&TransformParams {
        revision_id: Some(rev.clone()),
        revision_ids: None,
        recipe: Some(recipe(serde_json::json!({
            "operations": [
                {"op": "auto_orient"},
                {"op": "resiez", "width": 100}
            ]
        }))),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("operations[1]"),
        "the error must name the offending operation index: {message}"
    );
    assert!(
        message.contains("resiez") || message.contains("unknown variant"),
        "the error must carry serde's reason: {message}"
    );
    assert_eq!(payload["error"]["details"]["location"], "operations[1]");
    let suggestions = payload["error"]["details"]["did_you_mean"]
        .as_array()
        .unwrap();
    assert!(
        suggestions.iter().any(|s| s == "resize"),
        "did_you_mean must offer resize: {suggestions:?}"
    );
}

#[test]
fn a_wrongly_typed_field_names_the_field_and_the_expected_type() {
    let (_ws, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = tools.apply_transform(&TransformParams {
        revision_id: Some(rev),
        revision_ids: None,
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "rotate", "angle_degrees": "quite a lot"}]
        }))),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    assert_eq!(
        payload["error"]["details"]["location"], "operations[0].angle_degrees",
        "the error must name the offending field, not just the operation"
    );
    let reason = payload["error"]["details"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("expected f64"),
        "the error must carry serde's expected type: {reason}"
    );
    assert!(payload["error"]["message"]
        .as_str()
        .unwrap()
        .contains("angle_degrees"));
}

#[test]
fn a_recipe_that_is_not_an_object_is_reported_as_such() {
    let (_ws, tools) = tools();
    let result = tools.apply_transform(&TransformParams {
        revision_id: Some("rev_x".to_string()),
        revision_ids: None,
        recipe: Some(recipe(serde_json::json!([{"op": "auto_orient"}]))),
        preset: None,
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    assert_eq!(payload["error"]["details"]["location"], "recipe");
    assert!(payload["error"]["message"]
        .as_str()
        .unwrap()
        .contains("\"operations\""));
}

// ---------------------------------------------------------------------------
// FIX 4: プリセットの中身が見える
// ---------------------------------------------------------------------------

#[test]
fn explain_operation_also_explains_a_preset() {
    let (_ws, tools) = tools();
    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "web_optimize".to_string(),
    });
    let out = structured(&result);

    assert_eq!(out["kind"], "preset");
    assert_eq!(out["name"], "web_optimize");
    assert!(!out["description"].as_str().unwrap().is_empty());
    let ops = out["ops"].as_array().expect("ops array");
    assert!(!ops.is_empty());
    for op in ops {
        assert!(op["op"].is_string(), "each op must carry its tag: {op}");
    }

    let body = text(&result);
    assert!(body.contains("[preset]"), "{body}");
    assert!(body.contains("preset=\"web_optimize\""), "{body}");
    // レシピ本文がそのまま読める(= 生 DSL へ降りられる)。
    assert!(body.contains("\"op\""), "{body}");
}

#[test]
fn an_unknown_name_lists_operations_and_presets_separately() {
    let (_ws, tools) = tools();
    let result = tools.explain_operation(&ExplainOperationParams {
        operation: "web_optimise".to_string(),
    });
    let payload = error_payload(&result);
    assert_eq!(payload["error"]["code"], "unknown_operation");

    let details = &payload["error"]["details"];
    assert_eq!(details["valid_operations"].as_array().unwrap().len(), 29);
    assert!(details["valid_presets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "web_optimize"));
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Valid operations:") && message.contains("Valid presets:"),
        "the two vocabularies must be grouped: {message}"
    );
}

#[test]
fn the_catalog_shows_what_each_preset_actually_does() {
    let (_ws, tools) = tools();
    let result = tools.list_operations(&ListOperationsParams::default());
    let body = text(&result);

    // プリセット行に op の骨組みが載る(例: "grayscale — cmatrix: ...")。
    assert!(
        body.contains("web_optimize — "),
        "preset lines must carry an op summary: {body}"
    );
    assert!(
        body.contains("→") || body.lines().any(|l| l.starts_with("- grayscale — ")),
        "multi-op presets must show their chain joined by an arrow"
    );
    // DESIGN.md §9.12 で op 2 本(trim / threshold)と ocr_* プリセット 4 本が
    // 増えた分だけ上限を広げている(1 行あたり ~170 chars × 6 行)。
    assert!(
        body.len() < 10_500,
        "the catalog must stay compact, got {} chars",
        body.len()
    );
}

// ---------------------------------------------------------------------------
// FIX 5: 二重適用検出
// ---------------------------------------------------------------------------

#[test]
fn re_importing_an_exported_derivative_warns_about_double_processing() {
    let (workspace, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    let source_rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(imported["already_derived_from"].is_null());

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(source_rev.clone()),
        revision_ids: None,
        recipe: Some(recipe(serde_json::json!({
            "operations": [
                {"op": "adjust", "brightness": 0.1},
                {"op": "encode", "format": "png"}
            ]
        }))),
        preset: None,
    }));
    let derived_rev = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let recipe_hash = applied["recipe_hash"].as_str().unwrap().to_string();

    let dest = workspace
        .path()
        .parent()
        .unwrap()
        .join(format!("atx-double-apply-{}.png", std::process::id()));
    let exported = structured(&tools.export_asset(&ExportAssetParams {
        revision_id: derived_rev.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: true,
    }));
    let exported_path = exported["path"].as_str().unwrap().to_string();

    // 書き出したバイト列をそのまま取り込み直す(実運用でよくある往復)。
    let result = tools.import_asset(&ImportAssetParams::single(exported_path.clone()));
    let out = structured(&result);
    std::fs::remove_file(&dest).ok();

    let origin = &out["already_derived_from"];
    assert_eq!(origin["revision_id"], derived_rev.as_str());
    assert_eq!(origin["recipe_hash"], recipe_hash.as_str());
    assert_eq!(origin["source_revision_id"], source_rev.as_str());

    let warnings = out["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1);
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.contains(&recipe_hash[..8]), "{warning}");
    assert!(warning.contains(&source_rev), "{warning}");
    assert!(warning.contains("double-process"), "{warning}");
    assert!(text(&result).contains("double-process"));

    // 素の画像を取り込んでも誤検出しない。
    let clean = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    assert!(clean["already_derived_from"].is_null());
}

#[test]
fn double_apply_detection_works_per_file_in_a_batch() {
    let (workspace, tools) = tools();
    let imported = structured(&tools.import_asset(&ImportAssetParams::single(
        fixture().to_string_lossy().into_owned(),
    )));
    let source_rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(source_rev.clone()),
        revision_ids: None,
        recipe: Some(recipe(serde_json::json!({
            "operations": [{"op": "adjust", "contrast": 0.2}, {"op": "encode", "format": "png"}]
        }))),
        preset: None,
    }));
    let derived_rev = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let outside = workspace.path().parent().unwrap();
    let dest = outside.join(format!("atx-batch-double-{}.png", std::process::id()));
    structured(&tools.export_asset(&ExportAssetParams {
        revision_id: derived_rev.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: true,
    }));
    let fresh = copies(workspace.path(), 1).remove(0);

    let result = tools.import_asset(&ImportAssetParams::batch(vec![
        fresh.clone(),
        dest.to_string_lossy().into_owned(),
    ]));
    let out = structured(&result);
    let summary = text(&result);
    std::fs::remove_file(&dest).ok();

    let entries = out["imported"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0]["already_derived_from"].is_null());
    assert_eq!(
        entries[1]["already_derived_from"]["revision_id"],
        derived_rev.as_str()
    );
    assert!(entries[1]["warnings"][0]
        .as_str()
        .unwrap()
        .contains("double-process"));
    assert!(
        summary.contains("already the output of a recipe"),
        "the batch summary must surface the double-apply warning: {summary}"
    );
}

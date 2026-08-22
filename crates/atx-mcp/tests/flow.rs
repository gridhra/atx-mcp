//! import → inspect → detect_tilt → render_preview → apply_transform → export の
//! 一連のフローを、実ワークスペース(tempdir)と実画像で通す統合テスト。
//!
//! stdio / JSON-RPC は経由せず [`AtxTools`] を直接叩く(ツール本体はトランスポート非依存)。
//! 併せて rmcp のツール登録(名前・annotations・schema)も検証する。

use std::path::PathBuf;

use atx_mcp::tools::{
    AtxTools, CompareLayout, CompareRevisionsParams, DetectTiltParams, ExportAssetParams,
    ImportAssetParams, ListAssetsParams, RenderPreviewParams, RevisionParams, TransformParams,
    COMPARE_GAP_PX, PREVIEW_LONG_EDGE,
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

/// 成功結果から structuredContent を取り出す。テキストサマリの存在も検査する。
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

/// 成功結果の人間可読テキストサマリを取り出す。
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
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find_map(|t| serde_json::from_str::<Value>(&t.text).ok())
        .expect("error results must carry a structured JSON block");
    text
}

fn recipe() -> serde_json::Value {
    serde_json::json!({
        "operations": [
            {"op": "rotate", "angle_degrees": -0.5, "crop": "largest_inscribed_rect"},
            {"op": "crop", "aspect_ratio": "16:9", "anchor": "center"},
            {"op": "resize", "width": 1200, "fit": "cover", "without_enlargement": true},
            {"op": "encode", "format": "webp", "quality": 82}
        ]
    })
}

#[test]
fn full_flow_import_inspect_detect_preview_apply_export() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");

    // --- 1. import ---------------------------------------------------------
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    assert_eq!(imported["reused"], Value::Bool(false));
    let source_rev = imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string();
    assert!(source_rev.starts_with("rev_"));
    assert_eq!(imported["revision"]["mime_type"], "image/jpeg");
    assert_eq!(imported["revision"]["width"], 1477);
    assert_eq!(imported["revision"]["height"], 1108);
    assert!(PathBuf::from(imported["revision"]["path"].as_str().unwrap()).is_file());

    // import は冪等: 同じファイルをもう一度読んでも revision は増えない。
    let reimported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    assert_eq!(reimported["reused"], Value::Bool(true));
    assert_eq!(reimported["revision"]["revision_id"], source_rev.as_str());

    // --- 2. inspect --------------------------------------------------------
    let inspected = structured(&tools.inspect_image(&RevisionParams {
        revision_id: source_rev.clone(),
    }));
    assert_eq!(inspected["info"]["width"], 1477);
    assert_eq!(inspected["info"]["height"], 1108);
    assert_eq!(inspected["info"]["mime_type"], "image/jpeg");
    // フィクスチャは EXIF 無劣化除去済み(public repo のプライバシー配慮)。
    // 具体値をピンせず、実ファイルサイズと一致することだけを見る。
    assert_eq!(
        inspected["info"]["byte_size"],
        std::fs::metadata(fixture()).unwrap().len()
    );

    // --- 3. detect_tilt(角度は出ても出なくてもよいが、panic せず構造を保つこと)---
    let detect_result = tools.detect_tilt(&DetectTiltParams {
        revision_id: source_rev.clone(),
        max_abs_angle: None,
    });
    let detected = structured(&detect_result);
    let angle = &detected["detection"]["recommended_angle_degrees"];
    assert!(angle.is_null() || angle.is_number(), "angle: {angle:?}");
    if let Some(a) = angle.as_f64() {
        assert!(a.abs() <= 15.0, "detected angle out of search range: {a}");
    }
    let confidence = detected["detection"]["confidence"].as_f64().unwrap();
    assert!((0.0..=1.0).contains(&confidence));

    // 水平族 / 垂直族は別々に返る(ロールかパースかの判断材料)。
    let det = &detected["detection"];
    for (angle_key, conf_key, support_key) in [
        (
            "horizontal_angle_degrees",
            "horizontal_confidence",
            "horizontal_support",
        ),
        (
            "vertical_angle_degrees",
            "vertical_confidence",
            "vertical_support",
        ),
    ] {
        let a = &det[angle_key];
        assert!(a.is_null() || a.is_number(), "{angle_key}: {a:?}");
        if let Some(v) = a.as_f64() {
            assert!(v.abs() <= 15.0, "{angle_key} out of range: {v}");
        }
        for key in [conf_key, support_key] {
            let v = det[key]
                .as_f64()
                .unwrap_or_else(|| panic!("{key} must be a number: {det:?}"));
            assert!((0.0..=1.0).contains(&v), "{key}: {v}");
        }
    }

    // スコア曲線: 300 点以内・0..1 正規化・補正角の昇順で、ピークは推奨角。
    let curve = det["score_curve"].as_array().expect("score_curve");
    assert!(!curve.is_empty() && curve.len() <= 300, "{}", curve.len());
    let mut prev = f64::NEG_INFINITY;
    let mut peak = (f64::NAN, f64::NEG_INFINITY);
    for p in curve {
        let angle = p["angle_degrees"].as_f64().unwrap();
        let score = p["score"].as_f64().unwrap();
        assert!(angle > prev, "score_curve must be sorted by angle");
        assert!((0.0..=1.0).contains(&score), "score out of range: {score}");
        assert!(angle.abs() <= 15.0, "angle out of range: {angle}");
        if score > peak.1 {
            peak = (angle, score);
        }
        prev = angle;
    }
    assert_eq!(peak.1, 1.0, "score_curve must be normalized to 1.0 at peak");
    if let Some(a) = angle.as_f64() {
        assert_eq!(peak.0, a, "score_curve peak must be the recommended angle");
    }

    // テキストサマリにも族ごとの内訳が出る。
    let summary = text(&detect_result);
    assert!(
        summary.contains("horizontal lines:") && summary.contains("vertical lines:"),
        "summary must break the answer down per family: {summary}"
    );

    // max_abs_angle を絞っても壊れない。
    let narrow = structured(&tools.detect_tilt(&DetectTiltParams {
        revision_id: source_rev.clone(),
        max_abs_angle: Some(3.0),
    }));
    if let Some(a) = narrow["detection"]["recommended_angle_degrees"].as_f64() {
        assert!(a.abs() <= 3.0);
    }

    // --- 4. render_preview -------------------------------------------------
    let preview_result = tools.render_preview(&RenderPreviewParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    let preview = structured(&preview_result);
    assert_eq!(preview["mime_type"], "image/jpeg");
    assert_eq!(preview["engine_version"], atx_core::ENGINE_VERSION);
    let (pw, ph) = (
        preview["width"].as_u64().unwrap() as u32,
        preview["height"].as_u64().unwrap() as u32,
    );
    assert!(
        pw.max(ph) <= PREVIEW_LONG_EDGE,
        "preview long edge must be <= {PREVIEW_LONG_EDGE}, got {pw}x{ph}"
    );
    assert!(PathBuf::from(preview["preview_path"].as_str().unwrap()).is_file());
    // inline image content block(base64 jpeg)が付いていること。
    let image = preview_result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image(img) => Some(img),
            _ => None,
        })
        .expect("render_preview must return an inline image block");
    assert_eq!(image.mime_type, "image/jpeg");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .expect("inline image must be valid base64");
    assert_eq!(&decoded[..2], &[0xff, 0xd8], "inline image must be a JPEG");
    let decoded_image = image::load_from_memory(&decoded).expect("inline jpeg must decode");
    assert_eq!((decoded_image.width(), decoded_image.height()), (pw, ph));

    // --- 5. apply_transform ------------------------------------------------
    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
        preset: None,
    }));
    assert_eq!(applied["reused"], Value::Bool(false));
    assert_eq!(applied["engine_version"], atx_core::ENGINE_VERSION);
    assert_eq!(applied["source_revision_id"], source_rev.as_str());
    let derived_rev = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(applied["revision"]["mime_type"], "image/webp");
    assert_eq!(applied["revision"]["width"], 1200);
    assert_eq!(
        applied["revision"]["source_revision_id"],
        source_rev.as_str()
    );
    let derived_path = PathBuf::from(applied["revision"]["path"].as_str().unwrap());
    assert!(derived_path.is_file());

    // 派生 revision が台帳に載っていること。
    let listed = structured(&tools.list_assets(&ListAssetsParams { asset_id: None }));
    let ids: Vec<&str> = listed["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["revision_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&source_rev.as_str()));
    assert!(ids.contains(&derived_rev.as_str()));
    assert_eq!(listed["count"], 2);

    // asset_id での絞り込み(派生は元と同じ asset を共有する)。
    let asset_id = applied["revision"]["asset_id"]
        .as_str()
        .unwrap()
        .to_string();
    let per_asset = structured(&tools.list_assets(&ListAssetsParams {
        asset_id: Some(asset_id),
    }));
    assert_eq!(per_asset["count"], 2);

    // --- 6. 冪等性: 同じレシピをもう一度 → 同一 revision、再変換なし ---------
    let again = structured(&tools.apply_transform(&TransformParams {
        revision_id: source_rev.clone(),
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
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

    // --- 7. export ---------------------------------------------------------
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let dest = out_dir.path().join("hero.webp");
    let exported = structured(&tools.export_asset(&ExportAssetParams {
        revision_id: derived_rev.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: false,
    }));
    assert_eq!(exported["overwritten"], Value::Bool(false));
    assert_eq!(exported["path"], dest.to_string_lossy().as_ref());
    assert!(dest.is_file());
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        std::fs::read(&derived_path).unwrap()
    );

    // 上書きなしの再 export は構造化エラー。
    let refused = tools.export_asset(&ExportAssetParams {
        revision_id: derived_rev.clone(),
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: false,
    });
    let payload = error_payload(&refused);
    assert_eq!(payload["error"]["code"], "dest_exists");
    assert!(payload["error"]["details"]["recovery"]
        .as_str()
        .unwrap()
        .contains("overwrite=true"));

    // overwrite=true なら通る。
    let overwritten = structured(&tools.export_asset(&ExportAssetParams {
        revision_id: derived_rev,
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: true,
    }));
    assert_eq!(overwritten["overwritten"], Value::Bool(true));
}

#[test]
fn export_into_the_workspace_store_is_refused() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // objects / previews だけでなく、台帳(assets.jsonl)も root 直下の任意のファイルも
    // すべて拒否する: 不変ストアの中へ export するのが正当なケースは存在しない。
    for rel in [
        "objects/sneaky.jpg",
        "previews/sneaky.jpg",
        "assets.jsonl",
        "whatever.jpg",
    ] {
        let dest = tools.store().root().join(rel);
        let ledger_before = std::fs::read(tools.store().root().join("assets.jsonl")).ok();
        let refused = tools.export_asset(&ExportAssetParams {
            revision_id: rev.clone(),
            dest_path: dest.to_string_lossy().into_owned(),
            overwrite: true,
        });
        assert_eq!(
            error_payload(&refused)["error"]["code"],
            "dest_inside_workspace",
            "export to {rel} must be refused"
        );
        if rel == "assets.jsonl" {
            assert_eq!(
                std::fs::read(&dest).ok(),
                ledger_before,
                "the ledger must be untouched"
            );
        } else {
            assert!(!dest.exists());
        }
    }
}

/// シンボリックリンク経由でワークスペース内を指すパスも拒否すること。
/// (macOS の /tmp → /private/tmp のように、素の文字列比較では素通りしてしまう。)
#[test]
fn export_through_a_symlink_into_the_workspace_is_refused() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // <outside>/link -> <workspace>/objects
    let outside = tempfile::tempdir().expect("tempdir");
    let link = outside.path().join("link");
    std::os::unix::fs::symlink(tools.store().root().join("objects"), &link).expect("symlink");

    let dest = link.join("sneaky.jpg");
    let refused = tools.export_asset(&ExportAssetParams {
        revision_id: rev,
        dest_path: dest.to_string_lossy().into_owned(),
        overwrite: true,
    });
    assert_eq!(
        error_payload(&refused)["error"]["code"],
        "dest_inside_workspace",
        "a symlinked path into the store must be refused"
    );
    assert!(!dest.exists());
}

#[test]
fn errors_are_structured_and_actionable() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");

    // 存在しない revision
    let missing = tools.inspect_image(&RevisionParams {
        revision_id: "rev_nope".to_string(),
    });
    assert_eq!(
        error_payload(&missing)["error"]["code"],
        "revision_not_found"
    );

    // 存在しないパス
    let bad_path = tools.import_asset(&ImportAssetParams {
        path: "/definitely/not/here.jpg".to_string(),
    });
    assert_eq!(error_payload(&bad_path)["error"]["code"], "path_not_found");

    // 不正なレシピ(encode が最後でない)は op 位置つきで返る。
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let invalid = tools.apply_transform(&TransformParams {
        revision_id: rev,
        recipe: Some(
            serde_json::from_value(serde_json::json!({
                "operations": [
                    {"op": "encode", "format": "png"},
                    {"op": "auto_orient"}
                ]
            }))
            .unwrap(),
        ),
        preset: None,
    });
    let payload = error_payload(&invalid);
    assert_eq!(payload["error"]["code"], "invalid_recipe");
    assert!(payload["error"]["message"]
        .as_str()
        .unwrap()
        .contains("encode must be the last operation"));
}

/// rmcp 側の登録内容(名前・annotations・schema・instructions)を検証する。
#[test]
fn tool_registration_matches_the_design_contract() {
    use rmcp::ServerHandler;

    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let server = atx_mcp::AtxServer::new(std::sync::Arc::new(tools));

    let listed = server.router().list_all();
    let mut names: Vec<&str> = listed.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "apply_transform",
            "compare_revisions",
            "detect_tilt",
            "explain_operation",
            "export_asset",
            "generate_mask",
            "import_asset",
            "inspect_image",
            "list_assets",
            "list_operations",
            "render_preview",
        ]
    );

    for tool in &listed {
        let ann = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("{} must declare annotations", tool.name));
        assert_eq!(
            ann.open_world_hint,
            Some(false),
            "{}: openWorldHint must be false (local-only, no URL fetching)",
            tool.name
        );
        assert!(
            tool.output_schema.is_some(),
            "{}: outputSchema must be declared",
            tool.name
        );
        assert!(
            tool.description.is_some(),
            "{}: needs a description",
            tool.name
        );

        let read_only = matches!(
            tool.name.as_ref(),
            "inspect_image"
                | "detect_tilt"
                | "list_assets"
                | "list_operations"
                | "explain_operation"
        );
        assert_eq!(ann.read_only_hint, Some(read_only), "{}", tool.name);
        match tool.name.as_ref() {
            "import_asset" | "apply_transform" | "render_preview" | "compare_revisions"
            | "generate_mask" => {
                assert_eq!(ann.destructive_hint, Some(false), "{}", tool.name);
                assert_eq!(ann.idempotent_hint, Some(true), "{}", tool.name);
            }
            // 上書きしうるので destructive。
            "export_asset" => assert_eq!(ann.destructive_hint, Some(true)),
            _ => {}
        }
    }

    let info = server.get_info();
    assert_eq!(info.server_info.name, "asset-transform-mcp");
    assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    let instructions = info.instructions.expect("instructions are required");
    assert!(instructions.contains("immutable"));
    assert!(instructions.contains("\"operations\""));
    assert!(instructions.contains("import_asset"));
    assert!(instructions.contains("export_asset"));
    assert!(
        instructions.contains("ICC"),
        "instructions must call out the png/webp/avif ICC drop upfront"
    );
    // 語彙参照ツールとプリセットが「発見の道筋」として instructions に載っていること
    // (ROADMAP §Agent UX の規律 #2 / #3)。
    assert!(instructions.contains("list_operations"));
    // v0.5: マスクの作業手順(generate_mask → op の mask フィールド)が一文で載っていること。
    assert!(
        instructions.contains("generate_mask"),
        "instructions must point at the mask workflow"
    );
    assert!(instructions.contains("\"mask\": {\"revision_id\""));
    assert!(instructions.contains("explain_operation"));
    assert!(instructions.contains("preset"));
    // op を instructions 側で列挙しないこと(列挙は list_operations の役目)。
    assert!(
        !instructions.contains("auto_orient | rotate"),
        "instructions must not enumerate the operation vocabulary inline"
    );
    // instructions は毎セッションの固定費なので予算を持つ。JSON 例は1つ(flat recipe)だけ。
    assert!(
        instructions.len() < 4700,
        "instructions must stay within budget, got {} chars",
        instructions.len()
    );
    assert_eq!(
        instructions.matches("\n{\"operations\": [\n").count(),
        1,
        "only the flat recipe example belongs inline; layers go through explain_operation"
    );
    assert!(
        instructions.contains("explain_operation {\"operation\":\"layers\"}"),
        "the layers reference must be discoverable from the instructions"
    );
    // v0.6 の寸法規則を instructions 側で誤って言い切らないこと
    // (実際は「各レイヤーの ops の後に backdrop と比較」)。
    assert!(
        !instructions.contains("must match the base image's dimensions"),
        "the layer dimension rule must not be restated (wrongly) here"
    );
}

/// 各 overlay 値で render_preview が成功し、overlay なしのプレビューと差分があり、
/// 寸法は一致すること。同じ呼び出しを2回すればキャッシュヒットでバイト列が一致すること。
/// overlay あり/なしのプレビューは互いを上書きせず共存すること。invalid overlay は構造化エラー。
#[test]
fn render_preview_overlay_variants() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");
    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

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

    let base_result = tools.render_preview(&RenderPreviewParams {
        revision_id: rev.clone(),
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
        preset: None,
        overlay: None,
        mask_revision_id: None,
    });
    let base_structured = structured(&base_result);
    let base_bytes = preview_jpeg_bytes(&base_result);
    let base_path = base_structured["preview_path"]
        .as_str()
        .unwrap()
        .to_string();
    let (base_w, base_h) = {
        let img = image::load_from_memory(&base_bytes).unwrap();
        (img.width(), img.height())
    };
    assert!(base_structured["overlay"].is_null());

    for overlay in ["grid", "thirds", "horizon"] {
        let result = tools.render_preview(&RenderPreviewParams {
            revision_id: rev.clone(),
            recipe: Some(serde_json::from_value(recipe()).unwrap()),
            preset: None,
            overlay: Some(overlay.to_string()),
            mask_revision_id: None,
        });
        let structured_out = structured(&result);
        assert_eq!(structured_out["overlay"], overlay);
        let bytes = preview_jpeg_bytes(&result);
        assert_ne!(
            bytes, base_bytes,
            "overlay={overlay}: bytes must differ from the non-overlay preview"
        );

        let decoded = image::load_from_memory(&bytes).expect("overlaid jpeg must decode");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (base_w, base_h),
            "overlay={overlay}: dims must match the non-overlay preview"
        );

        let overlay_path = structured_out["preview_path"].as_str().unwrap().to_string();
        assert_ne!(
            overlay_path, base_path,
            "overlay={overlay}: overlay preview must be cached under a different path"
        );
        assert!(PathBuf::from(&overlay_path).is_file());
        assert!(
            PathBuf::from(&base_path).is_file(),
            "overlay={overlay}: non-overlay preview must still exist (coexistence)"
        );

        // 同じ呼び出しをもう一度 -> キャッシュヒットでバイト列は完全一致。
        let again = tools.render_preview(&RenderPreviewParams {
            revision_id: rev.clone(),
            recipe: Some(serde_json::from_value(recipe()).unwrap()),
            preset: None,
            overlay: Some(overlay.to_string()),
            mask_revision_id: None,
        });
        let again_bytes = preview_jpeg_bytes(&again);
        assert_eq!(
            again_bytes, bytes,
            "overlay={overlay}: repeated call must hit the cache and return identical bytes"
        );
        let again_structured = structured(&again);
        assert_eq!(again_structured["preview_path"], overlay_path.as_str());
    }

    // invalid overlay -> 構造化エラーで有効値一覧を返す。
    let invalid = tools.render_preview(&RenderPreviewParams {
        revision_id: rev,
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
        preset: None,
        overlay: Some("scanlines".to_string()),
        mask_revision_id: None,
    });
    let payload = error_payload(&invalid);
    assert_eq!(payload["error"]["code"], "invalid_overlay");
    let valid_values = payload["error"]["details"]["valid_values"]
        .as_array()
        .expect("valid_values must be listed");
    let valid_values: Vec<&str> = valid_values.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(valid_values, ["grid", "thirds", "horizon", "mask"]);
}

/// compare_revisions: side_by_side / stacked の両方で合成寸法が期待通りになること、
/// structuredContent に A/B が順序通りに載ること、存在しない revision は構造化エラー、
/// 同じ呼び出しの繰り返しはバイト同一(冪等)であること。
#[test]
fn compare_revisions_flow() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");

    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev_a = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: rev_a.clone(),
        recipe: Some(serde_json::from_value(recipe()).unwrap()),
        preset: None,
    }));
    let rev_b = applied["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    fn compare_jpeg(result: &CallToolResult) -> (Vec<u8>, (u32, u32)) {
        let image = result
            .content
            .iter()
            .find_map(|c| match c {
                ContentBlock::Image(img) => Some(img),
                _ => None,
            })
            .expect("compare_revisions must return an inline image block");
        assert_eq!(image.mime_type, "image/jpeg");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("inline image must be valid base64");
        let decoded = image::load_from_memory(&bytes).expect("comparison jpeg must decode");
        (bytes, (decoded.width(), decoded.height()))
    }

    fn scaled_long_edge(w: u32, h: u32, cap: u32) -> (u32, u32) {
        let long = w.max(h);
        if long <= cap {
            return (w, h);
        }
        let scale = cap as f64 / long as f64;
        (
            ((w as f64 * scale).round() as u32).max(1),
            ((h as f64 * scale).round() as u32).max(1),
        )
    }

    let a_dims = (
        imported["revision"]["width"].as_u64().unwrap() as u32,
        imported["revision"]["height"].as_u64().unwrap() as u32,
    );
    let b_dims = (
        applied["revision"]["width"].as_u64().unwrap() as u32,
        applied["revision"]["height"].as_u64().unwrap() as u32,
    );
    let a_scaled = scaled_long_edge(a_dims.0, a_dims.1, 640);
    let b_scaled = scaled_long_edge(b_dims.0, b_dims.1, 640);

    // --- side_by_side ---
    let side_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_b.clone(),
        layout: CompareLayout::SideBySide,
    });
    let side = structured(&side_result);
    assert_eq!(side["layout"], "side_by_side");
    assert_eq!(side["a"]["revision_id"], rev_a.as_str());
    assert_eq!(side["b"]["revision_id"], rev_b.as_str());
    assert_eq!(side["a_position"], "left");
    assert_eq!(side["b_position"], "right");
    let (_, (cw, ch)) = compare_jpeg(&side_result);
    assert_eq!(cw, a_scaled.0 + COMPARE_GAP_PX + b_scaled.0);
    assert_eq!(ch, a_scaled.1.max(b_scaled.1));
    assert_eq!(side["width"].as_u64().unwrap() as u32, cw);
    assert_eq!(side["height"].as_u64().unwrap() as u32, ch);

    // --- stacked ---
    let stacked_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_b.clone(),
        layout: CompareLayout::Stacked,
    });
    let stacked = structured(&stacked_result);
    assert_eq!(stacked["layout"], "stacked");
    assert_eq!(stacked["a_position"], "top");
    assert_eq!(stacked["b_position"], "bottom");
    let (_, (sw, sh)) = compare_jpeg(&stacked_result);
    assert_eq!(sw, a_scaled.0.max(b_scaled.0));
    assert_eq!(sh, a_scaled.1 + COMPARE_GAP_PX + b_scaled.1);

    // --- 冪等性: 同じ呼び出しを繰り返すとバイト同一 ---
    let again_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_b.clone(),
        layout: CompareLayout::SideBySide,
    });
    let (again_bytes, _) = compare_jpeg(&again_result);
    let (side_bytes, _) = compare_jpeg(&side_result);
    assert_eq!(again_bytes, side_bytes);
    let again = structured(&again_result);
    assert_eq!(
        again["preview_path"],
        side["preview_path"].as_str().unwrap()
    );

    // --- 存在しない revision は構造化エラー ---
    let missing = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a,
        revision_id_b: "rev_does_not_exist".to_string(),
        layout: CompareLayout::SideBySide,
    });
    let payload = error_payload(&missing);
    assert_eq!(payload["error"]["code"], "revision_not_found");
    assert_eq!(payload["error"]["details"]["side"], "b");
}

/// compare_revisions layout="diff"(v0.7): ヒートマップ画像 + 統計3値、
/// 同一 revision 同士は差分ゼロ、寸法不一致は構造化エラー、
/// 繰り返し呼び出しはキャッシュヒットでバイト同一(冪等)であること。
#[test]
fn compare_revisions_diff_flow() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");

    let imported = structured(&tools.import_asset(&ImportAssetParams {
        path: fixture().to_string_lossy().into_owned(),
    }));
    let rev_a = imported["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 同寸法のまま明るさだけ変えた派生 revision(diff の対象として妥当な組)。
    let brighten_recipe: atx_core::TransformRecipe = serde_json::from_value(serde_json::json!({
        "operations": [
            {"op": "adjust", "brightness": 0.3},
            {"op": "encode", "format": "png"}
        ]
    }))
    .unwrap();
    let brightened = structured(&tools.apply_transform(&TransformParams {
        revision_id: rev_a.clone(),
        recipe: Some(brighten_recipe),
        preset: None,
    }));
    let rev_bright = brightened["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();

    fn inline_image(result: &CallToolResult) -> Vec<u8> {
        let image = result
            .content
            .iter()
            .find_map(|c| match c {
                ContentBlock::Image(img) => Some(img),
                _ => None,
            })
            .expect("compare_revisions diff must return an inline image block");
        assert_eq!(image.mime_type, "image/jpeg");
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("inline image must be valid base64")
    }

    // --- diff: 明るさ違いのある A/B ---
    let diff_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_bright.clone(),
        layout: CompareLayout::Diff,
    });
    let diff = structured(&diff_result);
    assert_eq!(diff["layout"], "diff");
    assert_eq!(diff["a"]["revision_id"], rev_a.as_str());
    assert_eq!(diff["b"]["revision_id"], rev_bright.as_str());

    let diff_bytes = inline_image(&diff_result);
    image::load_from_memory(&diff_bytes).expect("diff heatmap jpeg must decode");

    let mean_abs_diff = diff["mean_abs_diff"]
        .as_f64()
        .expect("mean_abs_diff must be a number");
    let max_abs_diff = diff["max_abs_diff"]
        .as_u64()
        .expect("max_abs_diff must be a number");
    let changed_pixel_ratio = diff["changed_pixel_ratio"]
        .as_f64()
        .expect("changed_pixel_ratio must be a number");
    assert!(
        mean_abs_diff > 0.0,
        "a visibly brightened image must have mean_abs_diff > 0, got {mean_abs_diff}"
    );
    assert!(
        changed_pixel_ratio > 0.0 && changed_pixel_ratio <= 1.0,
        "changed_pixel_ratio must be in (0, 1], got {changed_pixel_ratio}"
    );
    assert!(max_abs_diff > 0, "max_abs_diff must be > 0");

    let summary = text(&diff_result);
    assert!(summary.contains("mean_abs_diff"));
    assert!(summary.contains("max_abs_diff"));
    assert!(summary.contains("changed_pixel_ratio"));

    // --- 冪等性: 同じ呼び出しの繰り返しはバイト同一(キャッシュヒット) ---
    let again_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_bright.clone(),
        layout: CompareLayout::Diff,
    });
    let again_bytes = inline_image(&again_result);
    assert_eq!(
        again_bytes, diff_bytes,
        "repeated diff compare must hit the cache and return identical bytes"
    );
    let again = structured(&again_result);
    assert_eq!(
        again["preview_path"],
        diff["preview_path"].as_str().unwrap()
    );

    // --- 同一 revision 同士は差分ゼロ ---
    let same_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a.clone(),
        revision_id_b: rev_a.clone(),
        layout: CompareLayout::Diff,
    });
    let same = structured(&same_result);
    assert_eq!(same["max_abs_diff"].as_u64().unwrap(), 0);
    assert_eq!(same["changed_pixel_ratio"].as_f64().unwrap(), 0.0);
    assert_eq!(same["mean_abs_diff"].as_f64().unwrap(), 0.0);

    // --- 寸法不一致は構造化エラー(resize/crop で寸法が変わった派生と比較) ---
    let resized_recipe: atx_core::TransformRecipe = serde_json::from_value(recipe()).unwrap();
    let resized = structured(&tools.apply_transform(&TransformParams {
        revision_id: rev_a.clone(),
        recipe: Some(resized_recipe),
        preset: None,
    }));
    let rev_resized = resized["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mismatch_result = tools.compare_revisions(&CompareRevisionsParams {
        revision_id_a: rev_a,
        revision_id_b: rev_resized,
        layout: CompareLayout::Diff,
    });
    let payload = error_payload(&mismatch_result);
    assert_eq!(payload["error"]["code"], "dimension_mismatch");
    assert!(payload["error"]["details"]["a_width"].is_u64());
    assert!(payload["error"]["details"]["b_width"].is_u64());
}

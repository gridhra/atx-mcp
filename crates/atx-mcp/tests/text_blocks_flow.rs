//! `detect_text_blocks`(13 本目のツール、v0.6)の統合テスト。
//!
//! 検出アルゴリズム自体の性質は atx-geometry 側のテストで押さえてある。
//! ここで見るのは MCP 層の配線だけ:
//! - 合成書類フィクスチャで文字らしいブロックが 2 つ以上返る
//! - `legibility.recommended_bands[0]` を**そのまま** `crop` op として
//!   apply_transform に渡せる(= レシピに貼れる形で返している)
//! - 2 回呼んで structuredContent が完全一致(read-only / 決定論)
//! - ラスタ画像でない revision は既存の `not_an_image` で弾く
//! - 範囲外のパラメータは有効範囲つきの構造化エラー

use std::path::PathBuf;

use atx_mcp::tools::{AtxTools, DetectTextBlocksParams, ImportAssetParams, TransformParams};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

fn document_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/synthetic_document.png")
        .canonicalize()
        .expect("the synthetic document fixture must exist")
}

fn cube_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/identity_8.cube")
        .canonicalize()
        .expect("the .cube fixture must exist")
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

fn import(tools: &AtxTools, path: PathBuf) -> String {
    structured(&tools.import_asset(&ImportAssetParams::single(
        path.to_string_lossy().into_owned(),
    )))["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string()
}

/// import → detect_text_blocks: 合成書類(見出し帯 + 段落 2 つ)で
/// ブロックが 2 つ以上返り、読み順・行高・帯分割が揃っていること。
#[test]
fn detecting_text_blocks_on_the_synthetic_document() {
    let (_ws, tools) = tools();
    let rev = import(&tools, document_fixture());

    let result = tools.detect_text_blocks(&DetectTextBlocksParams::new(rev.clone()));
    let out = structured(&result);
    let body = text(&result);

    assert_eq!(out["revision_id"], rev.as_str());
    let blocks = out["detection"]["blocks"]
        .as_array()
        .expect("blocks must be an array");
    assert!(
        blocks.len() >= 2,
        "the fixture has a headline plus two paragraphs, got {} block(s): {body}",
        blocks.len()
    );

    // 読み順(上から下)。
    let tops: Vec<u64> = blocks
        .iter()
        .map(|b| b["rect"]["y"].as_u64().expect("rect.y"))
        .collect();
    assert!(
        tops.windows(2).all(|w| w[0] <= w[1]),
        "blocks must come back in reading order, got tops {tops:?}"
    );

    // 各ブロックは行数・行高・インク比を持つ。
    for block in blocks {
        assert!(block["line_count"].as_u64().expect("line_count") >= 1);
        assert!(block["median_line_height_px"].as_u64().expect("height") >= 1);
        let ink = block["ink_ratio"].as_f64().expect("ink_ratio");
        assert!((0.0..=1.0).contains(&ink), "ink_ratio out of range: {ink}");
    }

    assert!(out["detection"]["median_line_height_px"].is_u64());
    let ratio = out["detection"]["text_like_area_ratio"]
        .as_f64()
        .expect("text_like_area_ratio");
    assert!((0.0..=1.0).contains(&ratio));

    let bands = out["detection"]["legibility"]["recommended_bands"]
        .as_array()
        .expect("recommended_bands must be an array");
    assert!(!bands.is_empty(), "at least one band must be suggested");
    for band in bands {
        assert_eq!(
            band["op"], "crop",
            "a band must be a ready-to-paste crop op"
        );
    }

    // テキストサマリはホスト AI に「何ブロックあって、どう読むか」を伝えること。
    assert!(body.contains("text-like block"), "{body}");
    assert!(
        body.contains("long_edge 1568"),
        "the summary must name the preview size it is reasoning about: {body}"
    );
}

/// `recommended_bands[i]` は本当にレシピに貼れる `crop` op であること
/// (= 返している形が apply_transform の入力としてそのまま通る)。
#[test]
fn a_recommended_band_pastes_straight_into_a_recipe() {
    let (_ws, tools) = tools();
    let rev = import(&tools, document_fixture());

    let out = structured(&tools.detect_text_blocks(&DetectTextBlocksParams::new(rev.clone())));
    let band = out["detection"]["legibility"]["recommended_bands"][0].clone();
    assert_eq!(band["op"], "crop");

    let recipe = serde_json::json!({"operations": [band, {"op": "encode", "format": "png"}]});
    let recipe: atx_core::TransformRecipe = serde_json::from_value(recipe)
        .expect("a recommended band must deserialize as a crop operation");

    let applied = structured(&tools.apply_transform(&TransformParams {
        revision_id: Some(rev),
        revision_ids: None,
        recipe: Some(recipe.into()),
        preset: None,
    }));
    let (w, h) = (
        applied["revision"]["width"].as_u64().expect("width"),
        applied["revision"]["height"].as_u64().expect("height"),
    );
    assert_eq!(
        w,
        out["detection"]["legibility"]["recommended_bands"][0]["rect"]["width"]
            .as_u64()
            .expect("band width")
    );
    assert_eq!(
        h,
        out["detection"]["legibility"]["recommended_bands"][0]["rect"]["height"]
            .as_u64()
            .expect("band height")
    );
}

/// read-only かつ決定論: 2 回呼べば structuredContent は完全一致する。
#[test]
fn detect_text_blocks_is_deterministic() {
    let (_ws, tools) = tools();
    let rev = import(&tools, document_fixture());

    let first = structured(&tools.detect_text_blocks(&DetectTextBlocksParams::new(rev.clone())));
    let second = structured(&tools.detect_text_blocks(&DetectTextBlocksParams::new(rev)));
    assert_eq!(first, second);
}

/// ラスタ画像でない revision(.cube LUT)は既存の `not_an_image` で弾く。
#[test]
fn detect_text_blocks_rejects_a_non_raster_revision() {
    let (_ws, tools) = tools();
    let rev = import(&tools, cube_fixture());

    let payload = error_payload(&tools.detect_text_blocks(&DetectTextBlocksParams::new(rev)));
    assert_eq!(payload["error"]["code"], "not_an_image");
}

/// 範囲外のパラメータは「有効範囲と回復手順」つきの構造化エラーで返る。
#[test]
fn out_of_range_parameters_are_structured_errors() {
    let (_ws, tools) = tools();
    let rev = import(&tools, document_fixture());

    for bad in [0usize, 129] {
        let payload = error_payload(&tools.detect_text_blocks(&DetectTextBlocksParams {
            revision_id: rev.clone(),
            max_blocks: Some(bad),
            min_block_area_ratio: None,
        }));
        assert_eq!(payload["error"]["code"], "invalid_max_blocks");
        assert_eq!(payload["error"]["details"]["min"], 1);
        assert_eq!(payload["error"]["details"]["max"], 128);
        assert!(payload["error"]["details"]["recovery"]
            .as_str()
            .unwrap()
            .contains("max_blocks"));
    }

    for bad in [-0.1f64, 1.5] {
        let payload = error_payload(&tools.detect_text_blocks(&DetectTextBlocksParams {
            revision_id: rev.clone(),
            max_blocks: None,
            min_block_area_ratio: Some(bad),
        }));
        assert_eq!(payload["error"]["code"], "invalid_min_block_area_ratio");
        assert_eq!(payload["error"]["details"]["min"], 0.0);
        assert_eq!(payload["error"]["details"]["max"], 1.0);
    }

    // 上限ちょうどは受理される(境界の向きを固定する)。
    let ok = tools.detect_text_blocks(&DetectTextBlocksParams {
        revision_id: rev,
        max_blocks: Some(128),
        min_block_area_ratio: Some(1.0),
    });
    assert_ne!(ok.is_error, Some(true));
}

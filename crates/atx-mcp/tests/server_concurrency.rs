//! MCP サーバ層(`AtxServer`)の並行性。
//!
//! rmcp は要求ごとに別の tokio タスクを起こすので、ツールの実処理(数秒かかる同期処理)を
//! async 関数の中で直接呼ぶと、その間 tokio のワーカースレッドが塞がる。ワーカー数は
//! CPU 数が既定なので、同時要求がそれを超えると(CPU 1 個のコンテナでは 1 件で)
//! ping・キャンセル通知・軽いツールまで止まる。ツール本体は `spawn_blocking` で
//! 専用スレッドへ逃がす(DESIGN.md §9.19)。

use std::sync::Arc;
use std::time::{Duration, Instant};

use atx_mcp::tools::{ImportAssetParams, ListOperationsParams, RecipeJson, RenderPreviewParams};
use atx_mcp::{AtxServer, AtxTools};
use rmcp::handler::server::wrapper::Parameters;

/// ワーカー 1 本の runtime で、重い render_preview の最中に list_operations が
/// すぐ返ること。修正前は重い呼び出しが唯一のワーカーを占有し、軽い呼び出しは
/// 重い方が終わるまで(数秒)始まらなかった。
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_heavy_tool_call_does_not_starve_light_calls() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tools = AtxTools::open(workspace.path()).expect("open workspace");

    // 合成画像(1000x1000、EXIF なし)を書いて取り込む。
    let path = workspace.path().join("big.png");
    let img = image::RgbImage::from_fn(1000, 1000, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8])
    });
    img.save(&path).expect("write synthetic png");
    let imported = tools.import_asset(&ImportAssetParams::single(path.to_string_lossy()));
    let revision_id = imported.structured_content.as_ref().expect("structured")["revision"]
        ["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string();

    let server = AtxServer::new(Arc::new(tools));

    // 重い呼び出し: 大きな σ のガウスぼかし(デバッグビルドで数秒かかる)。
    let heavy = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .render_preview(Parameters(RenderPreviewParams {
                    revision_id,
                    recipe: Some(RecipeJson(serde_json::json!({
                        "operations": [{"op": "blur", "sigma": 40.0}]
                    }))),
                    preset: None,
                    overlay: None,
                    mask_revision_id: None,
                    long_edge: None,
                }))
                .await
        })
    };
    // 重い方が先にワーカーを掴むまで待つ(このスレッドは runtime のワーカーではない)。
    std::thread::sleep(Duration::from_millis(200));

    let light = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .list_operations(Parameters(ListOperationsParams::default()))
                .await
        })
    };
    let started = Instant::now();
    let light = light.await.expect("list_operations task must not panic");
    let waited = started.elapsed();
    assert_ne!(light.is_error, Some(true));
    assert!(
        waited < Duration::from_secs(1),
        "list_operations waited {waited:?} behind a heavy call: the worker thread was blocked"
    );

    let heavy = heavy.await.expect("render_preview task must not panic");
    assert_ne!(heavy.is_error, Some(true), "{:?}", heavy.content);
}

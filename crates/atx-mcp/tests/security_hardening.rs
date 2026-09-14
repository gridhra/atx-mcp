//! セキュリティ点検(DESIGN.md §9.14)で見つかった import / export 経路の回帰テスト。
//!
//! - import_asset: ファイル全体を読む**前に**バイト上限を検査する
//! - import_asset: `.cube` / `.svg` として取り込むものは中身がパースできることを要求する
//!   (任意内容のファイルを「アセット」として台帳へ通し、export で書き出す経路を塞ぐ)
//! - export_asset: 書き出し先のシンボリックリンク・ハードリンクを辿ってワークスペースの
//!   不変ストア(objects/)を書き換えない

use std::path::{Path, PathBuf};

use atx_mcp::tools::{AtxTools, ExportAssetParams, ImportAssetParams};
use rmcp::model::CallToolResult;
use serde_json::Value;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
        .canonicalize()
        .expect("fixture must exist")
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
    assert_eq!(
        result.is_error,
        Some(true),
        "expected a tool-level error, got {:?}",
        result.content
    );
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

fn import(tools: &AtxTools, path: &Path) -> CallToolResult {
    tools.import_asset(&ImportAssetParams::single(
        path.to_string_lossy().into_owned(),
    ))
}

fn revision_id(imported: &Value) -> String {
    imported["revision"]["revision_id"]
        .as_str()
        .expect("revision_id")
        .to_string()
}

fn ledger_count(tools: &AtxTools) -> usize {
    tools.store().list_revisions(None).unwrap().len()
}

// ---------------------------------------------------------------------------
// import: 読む前のサイズ検査
// ---------------------------------------------------------------------------

/// 上限(既定 128MiB)を 1 バイト超える**疎ファイル**は、中身を読まずに
/// `limit_exceeded` で拒否される。
///
/// 「読まずに」を観測するため、Unix では読み取り権限を外したファイルでも確かめる:
/// 修正前は `fs::read` が先に走るので `io_error`(permission denied)になっていた。
#[test]
fn oversized_file_is_rejected_before_reading() {
    let (_ws, tools) = tools();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.png");
    let limit = atx_core::Limits::default().max_bytes;
    std::fs::File::create(&path)
        .unwrap()
        .set_len(limit + 1)
        .unwrap();

    let payload = error_payload(&import(&tools, &path));
    assert_eq!(payload["error"]["code"], "limit_exceeded", "{payload}");
    assert_eq!(ledger_count(&tools), 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root は権限を無視して読めるので、その環境では「読まずに」の観測にならない。
        if std::fs::File::open(&path).is_err() {
            let payload = error_payload(&import(&tools, &path));
            assert_eq!(payload["error"]["code"], "limit_exceeded", "{payload}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

// ---------------------------------------------------------------------------
// import: .cube / .svg の中身検証
// ---------------------------------------------------------------------------

#[test]
fn garbage_cube_file_is_rejected() {
    let (_ws, tools) = tools();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evil.cube");
    std::fs::write(&path, b"#!/bin/sh\necho this is not a LUT\n").unwrap();

    let payload = error_payload(&import(&tools, &path));
    assert_eq!(payload["error"]["code"], "invalid_asset", "{payload}");
    assert!(
        payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains(".cube"),
        "{payload}"
    );
    assert_eq!(ledger_count(&tools), 0);
}

#[test]
fn garbage_svg_file_is_rejected() {
    let (_ws, tools) = tools();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evil.svg");
    std::fs::write(&path, b"\x00\x01 definitely not xml <<<").unwrap();

    let payload = error_payload(&import(&tools, &path));
    assert_eq!(payload["error"]["code"], "invalid_asset", "{payload}");
    assert!(
        payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("SVG"),
        "{payload}"
    );
    assert_eq!(ledger_count(&tools), 0);
}

/// 拡張子が `.svg` でも、ルート要素が `<svg>` でない XML は SVG アセットではない。
#[test]
fn xml_that_is_not_svg_is_rejected() {
    let (_ws, tools) = tools();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.svg");
    std::fs::write(&path, b"<note><svg/></note>").unwrap();

    let payload = error_payload(&import(&tools, &path));
    assert_eq!(payload["error"]["code"], "invalid_asset", "{payload}");
    assert_eq!(ledger_count(&tools), 0);
}

/// 正しい合成フィクスチャは従来どおり取り込める。
#[test]
fn valid_cube_and_svg_fixtures_still_import() {
    let (_ws, tools) = tools();
    for rel in [
        "tests/fixtures/badge.svg",
        "tests/fixtures/identity_8.cube",
        "tests/fixtures/warm_8.cube",
        "crates/atx-mcp/tests/fixtures/identity_2.cube",
    ] {
        structured(&import(&tools, &fixture(rel)));
    }
    assert_eq!(ledger_count(&tools), 4);
}

// ---------------------------------------------------------------------------
// export: リンクを辿らない
// ---------------------------------------------------------------------------

/// 書き出し先が**ワークスペース内を指す宙ぶらりんのシンボリックリンク**でも拒否され、
/// リンク先(objects/ 配下)には何も作られない。
///
/// 修正前は `exists()` が false(リンク先が無い)で、ワークスペース内判定も
/// リンク名の側で行われるため素通りし、`fs::write` がリンクを辿って objects/ に
/// ファイルを作っていた。
#[cfg(unix)]
#[test]
fn dangling_symlink_into_the_workspace_is_refused() {
    let (ws, tools) = tools();
    let rev = revision_id(&structured(&import(
        &tools,
        &fixture("tests/fixtures/synthetic_scene.jpg"),
    )));

    let target = ws.path().join("objects").join("planted.jpg");
    let out = tempfile::tempdir().unwrap();
    let link = out.path().join("export.jpg");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    for overwrite in [false, true] {
        let mut params = ExportAssetParams::single(rev.clone(), link.to_string_lossy());
        params.overwrite = overwrite;
        let payload = error_payload(&tools.export_asset(&params));
        assert_eq!(payload["error"]["code"], "dest_is_symlink", "{payload}");
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "nothing may be created through the link"
        );
    }
}

/// 書き出し先が objects/ のファイルへの**ハードリンク**でも、overwrite=true の書き出しは
/// ストアの実体を書き換えない(ディレクトリエントリを置き換える)。
///
/// 修正前は `fs::write` が既存 inode をその場で切り詰めて書いていたので、
/// 同じ inode を共有する objects/ のファイル = 不変であるべき revision が壊れていた。
#[cfg(unix)]
#[test]
fn overwrite_through_a_hardlink_does_not_touch_the_store() {
    let (_ws, tools) = tools();
    let scene = structured(&import(
        &tools,
        &fixture("tests/fixtures/synthetic_scene.jpg"),
    ));
    let badge = structured(&import(&tools, &fixture("tests/fixtures/badge.svg")));
    let scene_rev = tools.store().get_revision(&revision_id(&scene)).unwrap();
    let object = tools.store().abs_path(&scene_rev);
    let original = std::fs::read(&object).unwrap();

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("linked.jpg");
    std::fs::hard_link(&object, &dest).unwrap();

    structured(&tools.export_asset(
        &ExportAssetParams::single(revision_id(&badge), dest.to_string_lossy()).with_overwrite(),
    ));
    assert!(
        std::fs::read(&object).unwrap() == original,
        "the stored object must stay byte-identical"
    );
    assert!(
        std::fs::read(&dest).unwrap()
            == std::fs::read(fixture("tests/fixtures/badge.svg")).unwrap(),
        "the destination must now hold the exported revision"
    );
}

/// 通常の新規書き出しと上書き書き出しは従来どおり動き、一時ファイルを残さない。
#[test]
fn plain_export_and_overwrite_still_work_without_leftovers() {
    let (_ws, tools) = tools();
    let rev = revision_id(&structured(&import(
        &tools,
        &fixture("tests/fixtures/synthetic_scene.jpg"),
    )));
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("out.jpg");
    for overwrite in [false, true] {
        let mut params = ExportAssetParams::single(rev.clone(), dest.to_string_lossy());
        params.overwrite = overwrite;
        let exported = structured(&tools.export_asset(&params));
        assert_eq!(exported["overwritten"], Value::Bool(overwrite));
    }
    let entries: Vec<_> = std::fs::read_dir(out.path()).unwrap().collect();
    assert_eq!(entries.len(), 1, "no temporary files may be left behind");
}

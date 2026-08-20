use std::collections::BTreeMap;

use atx_core::recipe::{Operation, TransformRecipe};
use atx_store::{AssetStore, StoreError};

fn minimal_recipe() -> TransformRecipe {
    TransformRecipe {
        operations: vec![Operation::AutoOrient],
    }
}

#[test]
fn import_is_idempotent_by_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    let bytes = b"fake jpeg bytes";
    let origin: BTreeMap<String, String> = BTreeMap::new();

    let first = store
        .import_bytes(bytes, "image/jpeg", 100, 200, origin.clone())
        .unwrap();
    let second = store
        .import_bytes(bytes, "image/jpeg", 100, 200, origin)
        .unwrap();

    // Same bytes -> same revision_id and asset_id (idempotent import).
    assert_eq!(first.revision_id, second.revision_id);
    assert_eq!(first.asset_id, second.asset_id);
    assert_eq!(first.sha256, second.sha256);

    // The ledger must only grow by one entry: a second import of identical
    // bytes does not append a new line.
    let all = store.list_revisions(None).unwrap();
    assert_eq!(all.len(), 1);
}

#[test]
fn derivation_is_idempotent_by_source_and_recipe_hash() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    let source = store
        .import_bytes(b"source bytes", "image/jpeg", 100, 100, BTreeMap::new())
        .unwrap();

    let recipe = minimal_recipe();
    let recipe_hash = "hash_abc123";

    let derived_bytes = b"derived bytes";
    let first = store
        .record_derivation(
            &source.revision_id,
            &recipe,
            recipe_hash,
            derived_bytes,
            "image/webp",
            50,
            50,
        )
        .unwrap();

    let second = store
        .record_derivation(
            &source.revision_id,
            &recipe,
            recipe_hash,
            derived_bytes,
            "image/webp",
            50,
            50,
        )
        .unwrap();

    assert_eq!(first.revision_id, second.revision_id);

    let all = store.list_revisions(None).unwrap();
    // one import + one derivation, no duplicate on second call
    assert_eq!(all.len(), 2);
}

#[test]
fn derivation_inherits_asset_id_across_chain() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    let source = store
        .import_bytes(b"root bytes", "image/jpeg", 100, 100, BTreeMap::new())
        .unwrap();

    let recipe = minimal_recipe();

    let derived1 = store
        .record_derivation(
            &source.revision_id,
            &recipe,
            "hash_1",
            b"derived1",
            "image/webp",
            50,
            50,
        )
        .unwrap();
    assert_eq!(derived1.asset_id, source.asset_id);

    // chain: derive from derived1
    let derived2 = store
        .record_derivation(
            &derived1.revision_id,
            &recipe,
            "hash_2",
            b"derived2",
            "image/png",
            25,
            25,
        )
        .unwrap();
    assert_eq!(derived2.asset_id, source.asset_id);
    assert_eq!(derived2.source_revision_id.as_deref(), Some(derived1.revision_id.as_str()));
}

#[test]
fn derivation_missing_source_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();
    let recipe = minimal_recipe();

    let result = store.record_derivation(
        "rev_does_not_exist",
        &recipe,
        "hash_x",
        b"bytes",
        "image/png",
        10,
        10,
    );

    assert!(matches!(result, Err(StoreError::RevisionNotFound(_))));
}

#[test]
fn list_revisions_filters_by_asset_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    let a = store
        .import_bytes(b"asset a bytes", "image/jpeg", 10, 10, BTreeMap::new())
        .unwrap();
    let b = store
        .import_bytes(b"asset b bytes", "image/png", 20, 20, BTreeMap::new())
        .unwrap();

    let recipe = minimal_recipe();
    let a_derived = store
        .record_derivation(
            &a.revision_id,
            &recipe,
            "hash_a",
            b"a derived bytes",
            "image/webp",
            5,
            5,
        )
        .unwrap();

    let filtered = store.list_revisions(Some(&a.asset_id)).unwrap();
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|r| r.asset_id == a.asset_id));
    assert!(filtered.iter().any(|r| r.revision_id == a.revision_id));
    assert!(filtered.iter().any(|r| r.revision_id == a_derived.revision_id));

    let all = store.list_revisions(None).unwrap();
    assert_eq!(all.len(), 3);

    let b_only = store.list_revisions(Some(&b.asset_id)).unwrap();
    assert_eq!(b_only.len(), 1);
    assert_eq!(b_only[0].revision_id, b.revision_id);
}

#[test]
fn read_bytes_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    let bytes = b"round trip content".to_vec();
    let revision = store
        .import_bytes(&bytes, "image/jpeg", 1, 1, BTreeMap::new())
        .unwrap();

    let read_back = store.read_bytes(&revision.revision_id).unwrap();
    assert_eq!(read_back, bytes);

    let abs_path = store.abs_path(&revision);
    assert!(abs_path.exists());
    assert!(abs_path.starts_with(store.root()));
}

#[test]
fn corrupted_ledger_line_errors_with_line_number() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    // Write one valid revision, then corrupt the ledger by appending garbage.
    store
        .import_bytes(b"valid bytes", "image/jpeg", 1, 1, BTreeMap::new())
        .unwrap();

    let ledger_path = dir.path().join("assets.jsonl");
    let mut existing = std::fs::read_to_string(&ledger_path).unwrap();
    existing.push_str("{ this is not valid json\n");
    std::fs::write(&ledger_path, existing).unwrap();

    let result = store.list_revisions(None);
    match result {
        Err(StoreError::LedgerCorrupted(msg)) => {
            assert!(msg.contains("line 2"), "expected line 2 in message: {msg}");
        }
        other => panic!("expected LedgerCorrupted, got {other:?}"),
    }
}

#[test]
fn preview_key_sanitization_rejects_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::open(dir.path()).unwrap();

    assert!(matches!(
        store.put_preview("../escape", "jpg", b"data"),
        Err(StoreError::InvalidPath(_))
    ));
    assert!(matches!(
        store.put_preview("sub/dir", "jpg", b"data"),
        Err(StoreError::InvalidPath(_))
    ));
    assert!(matches!(
        store.put_preview("back\\slash", "jpg", b"data"),
        Err(StoreError::InvalidPath(_))
    ));

    // valid key works and is deterministic (no rewrite on repeat call)
    let path1 = store.put_preview("rev_abc_hash_def", "webp", b"preview bytes").unwrap();
    let path2 = store
        .put_preview("rev_abc_hash_def", "webp", b"different bytes should not overwrite")
        .unwrap();
    assert_eq!(path1, path2);
    let contents = std::fs::read(&path1).unwrap();
    assert_eq!(contents, b"preview bytes");
}

#[test]
fn reopening_store_finds_existing_revisions() {
    let dir = tempfile::tempdir().unwrap();

    let revision_id = {
        let store = AssetStore::open(dir.path()).unwrap();
        let revision = store
            .import_bytes(b"persisted bytes", "image/png", 3, 3, BTreeMap::new())
            .unwrap();
        revision.revision_id
    };

    // reopen
    let store = AssetStore::open(dir.path()).unwrap();
    let found = store.get_revision(&revision_id).unwrap();
    assert_eq!(found.revision_id, revision_id);
    assert_eq!(store.read_bytes(&revision_id).unwrap(), b"persisted bytes");
}

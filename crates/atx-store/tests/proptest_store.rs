//! プロパティテスト: atx-store の冪等性・コンテンツアドレス性・パス安全性(DESIGN §3.2)。

use std::collections::BTreeMap;

use atx_core::recipe::{Operation, TransformRecipe};
use atx_store::{AssetStore, StoreError};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

fn minimal_recipe() -> TransformRecipe {
    TransformRecipe {
        operations: vec![Operation::AutoOrient],
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 11. put_preview のキー(パス文字を含む任意文字列)。
fn arb_key() -> impl Strategy<Value = String> {
    prop_oneof![
        // 任意の Unicode 文字列(制御文字含む)
        3 => "\\PC{0,16}",
        // path traversal / セパレータを狙い撃ちしたパターン
        1 => prop_oneof![
            Just("..".to_string()),
            Just("../escape".to_string()),
            Just("a/b".to_string()),
            Just("a\\b".to_string()),
            Just("..\\..\\windows".to_string()),
            Just(".".to_string()),
            Just("".to_string()),
            Just("日本語/パス".to_string()),
            Just("😀/emoji".to_string()),
            Just("....//....//etc".to_string()),
        ],
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    /// 9. import は冪等: 任意バイト列を2回 import すると同一 revision_id になり、
    ///    台帳は1行しか増えない。
    #[test]
    fn import_is_idempotent(bytes in prop::collection::vec(any::<u8>(), 1..4096)) {
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::open(dir.path()).unwrap();

        let first = store.import_bytes(&bytes, "image/jpeg", 10, 10, BTreeMap::new()).unwrap();
        let second = store.import_bytes(&bytes, "image/jpeg", 10, 10, BTreeMap::new()).unwrap();

        prop_assert_eq!(first.revision_id.clone(), second.revision_id);
        prop_assert_eq!(store.list_revisions(None).unwrap().len(), 1);
    }

    /// 10. 異なるバイト列は異なる sha256/rel_path を持ち、read_bytes は元のバイト列を
    ///     正確に復元する。
    #[test]
    fn distinct_bytes_yield_distinct_identity_and_roundtrip(
        a in prop::collection::vec(any::<u8>(), 1..2048),
        b in prop::collection::vec(any::<u8>(), 1..2048),
    ) {
        prop_assume!(a != b);
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::open(dir.path()).unwrap();

        let ra = store.import_bytes(&a, "image/png", 5, 5, BTreeMap::new()).unwrap();
        let rb = store.import_bytes(&b, "image/png", 5, 5, BTreeMap::new()).unwrap();

        prop_assert_eq!(ra.sha256.clone(), sha256_hex(&a));
        prop_assert_eq!(rb.sha256.clone(), sha256_hex(&b));
        prop_assert_ne!(ra.sha256, rb.sha256);
        prop_assert_ne!(ra.rel_path.clone(), rb.rel_path.clone());

        prop_assert_eq!(store.read_bytes(&ra.revision_id).unwrap(), a);
        prop_assert_eq!(store.read_bytes(&rb.revision_id).unwrap(), b);
    }

    /// 11. put_preview は任意のキー文字列に対して、InvalidPath を返すか、
    ///     もしくは previews/ 配下に留まる安全なパスを返す。パニックしない。
    #[test]
    fn put_preview_key_is_always_safe_or_rejected(key in arb_key()) {
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::open(dir.path()).unwrap();
        let previews_root = store.root().join("previews").canonicalize().unwrap();

        match store.put_preview(&key, "bin", b"payload") {
            Err(StoreError::InvalidPath(_)) => {}
            Err(other) => prop_assert!(false, "unexpected error variant: {other:?}"),
            Ok(path) => {
                prop_assert!(path.exists(), "put_preview returned a path that was not created");
                let canon = path.canonicalize().unwrap();
                prop_assert!(
                    canon.starts_with(&previews_root),
                    "preview path {canon:?} escaped previews root {previews_root:?}"
                );
            }
        }
    }

    /// 12. 派生チェーン(深さ 1..5): asset_id は根から一貫し、
    ///     (source, recipe_hash) の組は冪等にデデュープされる。
    #[test]
    fn derivation_chain_preserves_asset_id_and_dedups(
        (_depth, hashes) in (1usize..=5usize).prop_flat_map(|d| {
            (Just(d), prop::collection::vec("[a-z0-9]{1,10}", d))
        })
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = AssetStore::open(dir.path()).unwrap();

        let source = store
            .import_bytes(b"chain root bytes", "image/png", 10, 10, BTreeMap::new())
            .unwrap();
        let asset_id = source.asset_id.clone();
        let recipe = minimal_recipe();

        let mut current_id = source.revision_id.clone();
        for (i, h) in hashes.iter().enumerate() {
            let bytes = format!("derived-{i}").into_bytes();

            let first = store
                .record_derivation(&current_id, &recipe, h, &bytes, "image/png", 5, 5)
                .unwrap();
            prop_assert_eq!(first.asset_id.clone(), asset_id.clone());

            let before = store.list_revisions(None).unwrap().len();
            let second = store
                .record_derivation(&current_id, &recipe, h, &bytes, "image/png", 5, 5)
                .unwrap();
            let after = store.list_revisions(None).unwrap().len();

            prop_assert_eq!(second.revision_id.clone(), first.revision_id.clone());
            prop_assert_eq!(before, after, "(source, recipe_hash) dedup must not append a ledger line");

            current_id = first.revision_id;
        }
    }
}

//! atx-store: local-first の immutable アセットストア。
//!
//! レイアウト:
//! ```text
//! <workspace>/
//! ├── objects/<sha256[0..2]>/<sha256>.<ext>   # content-addressed、追記のみ
//! ├── assets.jsonl                            # revision 台帳(追記型 JSONL)
//! └── previews/                               # 低解像度プレビュー(掃除可能)
//! ```
//!
//! 不変条件:
//! - objects/ 配下の上書き・削除 API は存在しない
//! - import は同一 sha256 → 既存 revision を返す(冪等)
//! - record_derivation は (source_revision_id, recipe_hash) が既存なら既存 revision を返す(冪等)
//!
//! 実装メモ(v1):
//! - 台帳の読み出しはメソッド呼び出しごとに `assets.jsonl` 全体を走査する(スキャン方式)。
//!   このスケール(ローカルワークスペース、数千 revision 程度)では十分高速であり、
//!   プロセス内可変状態(キャッシュ・インデックス)を持たないことで複数プロセスからの
//!   同時オープンや再起動後の一貫性を単純に保つ。台帳への追記は open(append) + 1行書き込み
//!   + flush で行う(OS のファイル append はプロセス間である程度アトミック)。
//! - 台帳を変える操作(`import_bytes` / `record_derivation`)は、「走査して無ければ追記する」
//!   手順をプロセス内の `Mutex` で直列化する。MCP サーバは要求ごとに別タスクで並行実行する
//!   ため、排他が無いと同じバイト列の同時 import が重複 revision を作っていた
//!   (2026-10-22 のレビューで 16 本同時の import から 15 件の revision ができることを実測)。
//!   別プロセスとの排他は行わない(冪等性が破れても objects/ は内容アドレスなので壊れない)。
//! - 台帳の末尾に「書きかけの行」(クラッシュや容量不足で途中で切れた JSON)が残ることがある。
//!   読み出しでは **JSON が途中で切れている行(serde_json の EOF 分類)だけ**を読み飛ばし、
//!   それ以外の壊れ方(構文エラー・型違い)は従来どおり `LedgerCorrupted` にする。
//!   追記では、ファイルが改行で終わっていなければ先に改行を書いてから行を足し、
//!   書きかけの断片に新しい行が連結されないようにする。

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use atx_core::recipe::TransformRecipe;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ledger corrupted: {0}")]
    LedgerCorrupted(String),
    #[error("revision not found: {0}")]
    RevisionNotFound(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// 不変スナップショット。台帳(assets.jsonl)の1行。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AssetRevision {
    /// 論理アセット ID("ast_" + ULID)。派生 revision は元と同じ asset_id を共有する。
    pub asset_id: String,
    /// 不変 revision ID("rev_" + ULID)。
    pub revision_id: String,
    /// 派生元 revision(import 起点は None)。
    pub source_revision_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub byte_size: u64,
    pub sha256: String,
    /// workspace ルートからの相対パス(例 "objects/ab/ab12....jpg")。
    pub rel_path: String,
    /// この revision を生んだレシピ(import 起点は None)。
    pub recipe: Option<TransformRecipe>,
    pub recipe_hash: Option<String>,
    /// import 時の元ファイル名等の由来情報。
    pub origin: BTreeMap<String, String>,
    /// RFC3339 UTC。
    pub created_at: String,
}

/// ワークスペースディレクトリに紐づくストアハンドル。
pub struct AssetStore {
    root: PathBuf,
    /// 台帳を変える操作(走査 → 不在なら追記)をプロセス内で直列化する。
    /// 読み出しだけの操作はロックを取らない。
    write_lock: Mutex<()>,
}

/// MIME からストア内オブジェクトの拡張子を決める。
///
/// **この表が唯一の実装**。`export_asset` のファイル名テンプレート `{ext}` も
/// ここを呼ぶ(MCP 層に同じ表を置いていたら、font/ttf の追加がそちらに入らず
/// フォントの export だけ `.bin` になっていた)。
pub fn ext_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/avif" => "avif",
        // 画像ではないアセット(v0.3: レシピから参照される .cube 3D LUT)。
        "application/x-cube" => "cube",
        // ベクタアセット(v0.8: `svg_overlay` から参照される SVG)。
        // ラスタ画像ではないが MIME は image/ で始まるので、寸法検査や
        // デコードの対象にはならない(atx-mcp 側で分岐する)。
        "image/svg+xml" => "svg",
        // フォントアセット(v0.6: `svg_overlay` の文字描画が参照する .ttf / .otf)。
        // ラスタ画像ではないので寸法検査やデコードの対象にはならない(atx-mcp 側で分岐する)。
        "font/ttf" => "ttf",
        "font/otf" => "otf",
        _ => "bin",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting of OffsetDateTime::now_utc never fails")
}

impl AssetStore {
    /// workspace を開く(なければディレクトリ構造を作成)。
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("previews"))?;
        let ledger_path = root.join("assets.jsonl");
        if !ledger_path.exists() {
            // touch
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&ledger_path)?;
        }
        let root = root.canonicalize()?;
        Ok(Self {
            root,
            write_lock: Mutex::new(()),
        })
    }

    /// 台帳を変える操作のための排他。別スレッドが panic したままでも
    /// (poisoned)台帳自体は壊れていないので、そのまま使い続ける。
    fn lock_for_write(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ledger_path(&self) -> PathBuf {
        self.root.join("assets.jsonl")
    }

    /// assets.jsonl を全走査して revision のリストを返す(ファイル出現順)。
    fn scan_ledger(&self) -> Result<Vec<AssetRevision>> {
        let path = self.ledger_path();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AssetRevision>(&line) {
                Ok(revision) => out.push(revision),
                // JSON が途中で切れている行 = 書きかけのまま残った追記(クラッシュ・容量不足)。
                // 完全な JSON の真の接頭辞は必ず EOF 分類になり、構文エラーや型違いとは
                // 区別できる。読み飛ばして残りの台帳を生かす(モジュールドキュメント参照)。
                Err(e) if e.classify() == serde_json::error::Category::Eof => continue,
                Err(e) => {
                    return Err(StoreError::LedgerCorrupted(format!(
                        "line {}: {}",
                        idx + 1,
                        e
                    )))
                }
            }
        }
        Ok(out)
    }

    /// 台帳に1行追記する(open append + write + flush)。
    ///
    /// ファイルが改行で終わっていなければ(= 末尾に書きかけの行がある)、先に改行を
    /// 書いて断片を自分の行に隔離してから追記する。こうしないと新しい行が断片に連結され、
    /// 完全な revision まで読めなくなる。
    fn append_ledger(&self, revision: &AssetRevision) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(self.ledger_path())?;
        let len = file.metadata()?.len();
        let needs_separator = if len == 0 {
            false
        } else {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            last[0] != b'\n'
        };
        let mut line = String::new();
        if needs_separator {
            line.push('\n');
        }
        line.push_str(
            &serde_json::to_string(revision).expect("AssetRevision serialization never fails"),
        );
        line.push('\n');
        // append モードなので、上の seek に関わらず末尾へ書かれる。
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    /// 与えられた sha256/mime のバイト列を content-addressed に格納する。
    /// 既に同名ファイルが存在する場合は書き込みをスキップする(objects/ は上書きしない)。
    /// 戻り値はワークスペースルートからの相対パス。
    fn store_object(&self, sha256: &str, mime_type: &str, bytes: &[u8]) -> Result<String> {
        let ext = ext_for_mime(mime_type);
        let prefix = &sha256[0..2];
        let dir = self.root.join("objects").join(prefix);
        fs::create_dir_all(&dir)?;
        let filename = format!("{sha256}.{ext}");
        let final_path = dir.join(&filename);
        if !final_path.exists() {
            // 一時ファイルに書いてから rename でアトミックに配置する。
            let tmp_path = dir.join(format!(".{filename}.tmp-{}", ulid::Ulid::generate()));
            {
                let mut tmp_file = File::create(&tmp_path)?;
                tmp_file.write_all(bytes)?;
                tmp_file.flush()?;
            }
            fs::rename(&tmp_path, &final_path)?;
        }
        let rel = PathBuf::from("objects").join(prefix).join(&filename);
        Ok(rel.to_string_lossy().into_owned())
    }

    /// バイト列を objects/ へ格納し revision を発行する(import 起点)。
    /// 同一 sha256 の import 済み revision があればそれを返す。
    pub fn import_bytes(
        &self,
        bytes: &[u8],
        mime_type: &str,
        width: u32,
        height: u32,
        origin: BTreeMap<String, String>,
    ) -> Result<AssetRevision> {
        let sha256 = sha256_hex(bytes);

        // 冪等性: 同一 sha256 かつ import 起点(source_revision_id == None)の revision が
        // あればそれを返す。走査から追記までを `write_lock` で直列化する
        // (並行する同一バイト列の import が重複 revision を作らないため)。
        let _guard = self.lock_for_write();
        let existing = self.scan_ledger()?;
        if let Some(found) = existing
            .iter()
            .find(|r| r.sha256 == sha256 && r.source_revision_id.is_none())
        {
            return Ok(found.clone());
        }

        let rel_path = self.store_object(&sha256, mime_type, bytes)?;

        let revision = AssetRevision {
            asset_id: format!("ast_{}", ulid::Ulid::generate()),
            revision_id: format!("rev_{}", ulid::Ulid::generate()),
            source_revision_id: None,
            width,
            height,
            mime_type: mime_type.to_string(),
            byte_size: bytes.len() as u64,
            sha256,
            rel_path,
            recipe: None,
            recipe_hash: None,
            origin,
            created_at: now_rfc3339(),
        };
        self.append_ledger(&revision)?;
        Ok(revision)
    }

    /// 変換結果を格納し派生 revision を発行する。
    /// (source_revision_id, recipe_hash) が台帳に既存ならバイト格納をスキップして既存を返す。
    #[allow(clippy::too_many_arguments)]
    pub fn record_derivation(
        &self,
        source_revision_id: &str,
        recipe: &TransformRecipe,
        recipe_hash: &str,
        bytes: &[u8],
        mime_type: &str,
        width: u32,
        height: u32,
    ) -> Result<AssetRevision> {
        // 走査から追記までを直列化する(`import_bytes` と同じ理由)。
        let _guard = self.lock_for_write();
        let existing = self.scan_ledger()?;

        // 冪等性: 同一 (source_revision_id, recipe_hash) の派生 revision があればそれを返す。
        if let Some(found) = existing.iter().find(|r| {
            r.source_revision_id.as_deref() == Some(source_revision_id)
                && r.recipe_hash.as_deref() == Some(recipe_hash)
        }) {
            return Ok(found.clone());
        }

        // 派生元 revision から asset_id を継承する。
        let source = existing
            .iter()
            .find(|r| r.revision_id == source_revision_id)
            .ok_or_else(|| StoreError::RevisionNotFound(source_revision_id.to_string()))?;
        let asset_id = source.asset_id.clone();

        let sha256 = sha256_hex(bytes);
        let rel_path = self.store_object(&sha256, mime_type, bytes)?;

        let revision = AssetRevision {
            asset_id,
            revision_id: format!("rev_{}", ulid::Ulid::generate()),
            source_revision_id: Some(source_revision_id.to_string()),
            width,
            height,
            mime_type: mime_type.to_string(),
            byte_size: bytes.len() as u64,
            sha256,
            rel_path,
            recipe: Some(recipe.clone()),
            recipe_hash: Some(recipe_hash.to_string()),
            origin: BTreeMap::new(),
            created_at: now_rfc3339(),
        };
        self.append_ledger(&revision)?;
        Ok(revision)
    }

    pub fn get_revision(&self, revision_id: &str) -> Result<AssetRevision> {
        self.scan_ledger()?
            .into_iter()
            .find(|r| r.revision_id == revision_id)
            .ok_or_else(|| StoreError::RevisionNotFound(revision_id.to_string()))
    }

    /// revision の実体バイトを読む。
    pub fn read_bytes(&self, revision_id: &str) -> Result<Vec<u8>> {
        let revision = self.get_revision(revision_id)?;
        let bytes = fs::read(self.abs_path(&revision))?;
        Ok(bytes)
    }

    /// revision の絶対パス(resource_link 用)。
    pub fn abs_path(&self, revision: &AssetRevision) -> PathBuf {
        self.root.join(&revision.rel_path)
    }

    /// 全 revision を列挙(created_at 昇順、同値は台帳出現順)。asset_id でのフィルタ可。
    pub fn list_revisions(&self, asset_id: Option<&str>) -> Result<Vec<AssetRevision>> {
        let mut revisions = self.scan_ledger()?;
        if let Some(asset_id) = asset_id {
            revisions.retain(|r| r.asset_id == asset_id);
        }
        // Vec::sort_by is stable, so equal created_at values keep ledger order.
        revisions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(revisions)
    }

    /// プレビュー画像を previews/ に書き、絶対パスを返す。命名は決定論的
    /// (source revision + recipe_hash 由来)で、同一キーなら再生成せず既存を返す。
    pub fn put_preview(&self, key: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf> {
        // キー・拡張子は [A-Za-z0-9._-] のみ許可(".." は不可)。
        // 呼び出し側のキーは常に内部生成(sha256 hex + スラッグ)なのでこれで十分であり、
        // 任意 Unicode を許すとファイルシステム依存の失敗(macOS の EILSEQ 等)が
        // 生の Io エラーとして漏れる。
        fn safe_component(s: &str) -> bool {
            !s.is_empty()
                && !s.contains("..")
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        if !safe_component(key) {
            return Err(StoreError::InvalidPath(key.to_string()));
        }
        if !safe_component(ext) {
            return Err(StoreError::InvalidPath(ext.to_string()));
        }
        let previews_dir = self.root.join("previews");
        fs::create_dir_all(&previews_dir)?;
        let path = previews_dir.join(format!("{key}.{ext}"));
        if !path.exists() {
            let tmp_path =
                previews_dir.join(format!(".{key}.{ext}.tmp-{}", ulid::Ulid::generate()));
            {
                let mut tmp_file = File::create(&tmp_path)?;
                tmp_file.write_all(bytes)?;
                tmp_file.flush()?;
            }
            fs::rename(&tmp_path, &path)?;
        }
        Ok(path)
    }
}

#[cfg(test)]
mod ext_tests {
    use super::ext_for_mime;

    #[test]
    fn cube_luts_keep_their_extension() {
        assert_eq!(ext_for_mime("application/x-cube"), "cube");
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("application/whatever"), "bin");
    }

    #[test]
    fn svg_assets_keep_their_extension() {
        assert_eq!(ext_for_mime("image/svg+xml"), "svg");
    }
}

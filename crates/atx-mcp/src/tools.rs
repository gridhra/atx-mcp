//! ツールの実処理(トランスポート非依存・同期)。
//!
//! rmcp の `#[tool]` 関数([`crate::server`])は、ここのメソッドを呼ぶだけの薄いラッパである。
//! こうしておくと統合テストが stdio / JSON-RPC を一切経由せずにフロー全体を検証できる。
//!
//! 返却規約(DESIGN.md §4.1):
//! - 常に「人間可読のテキストサマリ(パス込み)」+ `structuredContent`(機械可読 JSON)の両方を返す
//! - `render_preview` のみ inline ImageContent(base64 jpeg、長辺 ≤ 768)を追加する
//! - エラーは `CallToolResult::error`(is_error=true)で、原因と回復手順を構造化して返す

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use atx_core::recipe::{Fit, Operation, OutputFormat};
use atx_core::{AtxError, ImageInfo, Limits, TransformRecipe, ENGINE_VERSION};
use atx_geometry::{DetectParams, TiltDetection};
use atx_store::{AssetRevision, AssetStore, StoreError};
use base64::Engine as _;
use image::{ImageEncoder, RgbImage};
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// プレビューの長辺上限(DESIGN.md §4.1)。
pub const PREVIEW_LONG_EDGE: u32 = 768;
/// プレビューの JPEG 品質(レシピの encode 指定に関わらず固定)。
pub const PREVIEW_JPEG_QUALITY: u8 = 80;
/// `compare_revisions` で各辺を縮小する際の長辺上限。
pub const COMPARE_LONG_EDGE: u32 = 640;
/// `compare_revisions` の合成キャンバスで2枚の画像を隔てる隙間(px)。
pub const COMPARE_GAP_PX: u32 = 8;
/// `render_preview` の overlay で使う有効値。
/// `"mask"` だけはガイド線ではなくマスクの可視化で、`mask_revision_id` を伴う(v0.5)。
pub const OVERLAY_VALUES: [&str; 4] = ["grid", "thirds", "horizon", "mask"];
/// マスク可視化 overlay で「被覆している」と塗り分ける重みのしきい値。
pub const MASK_OVERLAY_THRESHOLD: f64 = 0.5;
/// マスク可視化 overlay の被覆域を塗る赤の混合率。
const MASK_OVERLAY_ALPHA: f32 = 0.6;
/// マスク可視化 overlay で非被覆域を落とす係数(被覆域とのコントラストを付ける)。
const MASK_OVERLAY_DIM: f32 = 0.75;
/// マスク可視化 overlay の被覆色。
const MASK_OVERLAY_COLOR: [u8; 3] = [0xFF, 0x22, 0x22];
/// .cube 3D LUT アセットの MIME type(v0.3「レシピ → アセット参照」)。
///
/// 画像ではないアセットをストアの台帳上で区別するための擬似 MIME。
/// atx-store の `ext_for_mime` がこれを `.cube` 拡張子に写す。
pub const CUBE_MIME: &str = "application/x-cube";
/// .cube 取り込みのサイズ上限。33^3 の 3D LUT でも 1MiB に満たないので、
/// 16MiB は「テキスト LUT としてありえない大きさ」を弾く実務的なサニティ上限。
pub const MAX_CUBE_BYTES: u64 = 16 * 1024 * 1024;
/// .cube 判定でヘッダとして読むバイト数の上限。
const CUBE_SNIFF_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 入力パラメータ(tool inputSchema はこれらから生成される)
// ---------------------------------------------------------------------------

/// `import_asset` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ImportAssetParams {
    /// 取り込むローカルファイルの絶対パス(または cwd からの相対パス)。
    pub path: String,
}

/// revision を1つ指定するだけのツールの引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RevisionParams {
    /// 対象 revision ID("rev_...")。
    pub revision_id: String,
}

/// `detect_tilt` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct DetectTiltParams {
    /// 対象 revision ID("rev_...")。
    pub revision_id: String,
    /// 探索する最大傾き角(度、0.5..=45)。省略時は 15。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_abs_angle: Option<f64>,
}

/// `apply_transform` / `render_preview` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TransformParams {
    /// 入力 revision ID("rev_...")。
    pub revision_id: String,
    /// 変換レシピ。`{"operations": [...]}`。`preset` とはどちらか一方のみ指定する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<TransformRecipe>,
    /// ビルトインプリセット名(`list_operations` の presets セクション参照)。
    /// 指定するとそのプリセットのレシピが使われる。`recipe` とは排他。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// `render_preview` の引数(`TransformParams` に guide overlay を足したもの)。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RenderPreviewParams {
    /// 入力 revision ID("rev_...")。
    pub revision_id: String,
    /// 変換レシピ。`{"operations": [...]}`。`preset` とはどちらか一方のみ指定する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<TransformRecipe>,
    /// ビルトインプリセット名。`recipe` とは排他。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 構図確認用のガイド線。`"grid"`(1/8 刻みの格子)| `"thirds"`(三分割法)|
    /// `"horizon"`(1/12 刻みの水平線のみ、傾き目視用)| `"mask"`(マスクの被覆可視化。
    /// `mask_revision_id` が必須)。省略時はオーバレイなし。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<String>,
    /// `overlay: "mask"` で可視化するマスク画像 revision ID。
    /// `overlay` が `"mask"` のときのみ指定でき、そのときは必須。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_revision_id: Option<String>,
}

/// `list_operations` の引数。
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ListOperationsParams {
    /// 絞り込む分類(`"geometry"` | `"color"` | `"filter"` | `"output"`)。省略時は全件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// `explain_operation` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ExplainOperationParams {
    /// 説明したい op 名(`{"op": "..."}` に書く名前)。
    pub operation: String,
}

/// `compare_revisions` のレイアウト。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompareLayout {
    #[default]
    SideBySide,
    Stacked,
}

impl CompareLayout {
    fn as_str(self) -> &'static str {
        match self {
            CompareLayout::SideBySide => "side_by_side",
            CompareLayout::Stacked => "stacked",
        }
    }
}

/// `compare_revisions` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CompareRevisionsParams {
    /// 比較対象 A の revision ID("rev_...")。合成画像の左(または上)に置かれる。
    pub revision_id_a: String,
    /// 比較対象 B の revision ID("rev_...")。合成画像の右(または下)に置かれる。
    pub revision_id_b: String,
    /// `"side_by_side"`(既定、水平に並べる)| `"stacked"`(垂直に並べる)。
    #[serde(default)]
    pub layout: CompareLayout,
}

/// `list_assets` の引数。
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ListAssetsParams {
    /// 指定した asset_id の revision だけに絞る。省略時は全件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
}

/// `export_asset` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ExportAssetParams {
    /// 書き出す revision ID("rev_...")。
    pub revision_id: String,
    /// 書き出し先パス(ワークスペース外)。
    pub dest_path: String,
    /// 既存ファイルを上書きしてよいか。既定 false(既存なら失敗する)。
    #[serde(default)]
    pub overwrite: bool,
}

// ---------------------------------------------------------------------------
// 出力(structuredContent / outputSchema)
// ---------------------------------------------------------------------------

/// revision の要約。台帳の1行 + 絶対パス。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevisionSummary {
    pub revision_id: String,
    pub asset_id: String,
    pub source_revision_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub byte_size: u64,
    pub sha256: String,
    /// ワークスペース内の絶対パス。
    pub path: String,
    pub recipe_hash: Option<String>,
    pub created_at: String,
}

impl RevisionSummary {
    fn new(store: &AssetStore, revision: &AssetRevision) -> Self {
        Self {
            revision_id: revision.revision_id.clone(),
            asset_id: revision.asset_id.clone(),
            source_revision_id: revision.source_revision_id.clone(),
            width: revision.width,
            height: revision.height,
            mime_type: revision.mime_type.clone(),
            byte_size: revision.byte_size,
            sha256: revision.sha256.clone(),
            path: store.abs_path(revision).to_string_lossy().into_owned(),
            recipe_hash: revision.recipe_hash.clone(),
            created_at: revision.created_at.clone(),
        }
    }
}

/// `import_asset` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportOutput {
    pub revision: RevisionSummary,
    /// 同一内容が既に取り込まれていて既存 revision を返した場合 true(冪等ヒット)。
    pub reused: bool,
    /// 取り込み元の正規化済みパス。
    pub source_path: String,
}

/// `inspect_image` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InspectOutput {
    pub revision_id: String,
    pub path: String,
    pub info: ImageInfo,
}

/// `detect_tilt` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DetectTiltOutput {
    pub revision_id: String,
    pub detection: TiltDetection,
}

/// `apply_transform` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyTransformOutput {
    pub revision: RevisionSummary,
    pub source_revision_id: String,
    pub recipe_hash: String,
    pub engine_version: String,
    /// 既存 revision を返した(再変換をスキップした)場合 true。
    pub reused: bool,
    pub warnings: Vec<String>,
}

/// `render_preview` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenderPreviewOutput {
    pub source_revision_id: String,
    pub recipe_hash: String,
    pub engine_version: String,
    /// プレビュー画像の絶対パス。
    pub preview_path: String,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub mime_type: String,
    pub warnings: Vec<String>,
    /// 適用した overlay。未指定なら null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<String>,
    /// `overlay: "mask"` で可視化したマスクの revision ID。それ以外では null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_revision_id: Option<String>,
}

/// `generate_mask` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GenerateMaskOutput {
    pub revision: RevisionSummary,
    /// 生成した種別(`linear_gradient` 等)。
    pub kind: String,
    /// 参照した画像 revision(寸法・画素の供給元)。
    pub reference_revision_id: String,
    pub width: u32,
    pub height: u32,
    /// 既定値まで解決したパラメータの正規化 JSON(origin の generator と同一文字列)。
    pub generator: String,
    /// マスクの平均重み(0..1)。1 に近いほど広く、0 に近いほど狭い被覆。
    pub mean_weight: f64,
    /// 同じマスクが既に生成済みで既存 revision を返した場合 true(冪等ヒット)。
    pub reused: bool,
    /// 次の一手(op への参照の仕方)。
    pub next: String,
}

/// `compare_revisions` の片側(A または B)の要約。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompareSide {
    pub revision_id: String,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub byte_size: u64,
    pub recipe_hash: Option<String>,
}

/// `compare_revisions` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompareRevisionsOutput {
    pub layout: String,
    pub a: CompareSide,
    pub b: CompareSide,
    /// A が合成画像のどこに置かれるか("left" | "top")。
    pub a_position: String,
    /// B が合成画像のどこに置かれるか("right" | "bottom")。
    pub b_position: String,
    /// 合成画像(比較プレビュー)の寸法・容量。
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub byte_size: u64,
    /// 比較プレビュー画像の絶対パス。
    pub preview_path: String,
}

/// `list_assets` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListAssetsOutput {
    pub count: usize,
    pub revisions: Vec<RevisionSummary>,
}

/// `list_operations` のカタログ1行。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OperationCatalogEntry {
    pub name: String,
    pub category: String,
    pub summary: String,
    /// `"name*: type range"`(`*` = 必須、`=x` = 既定値、`?` = 任意)の簡潔な列。
    pub params: Vec<String>,
}

/// プリセットのカタログ1行。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PresetCatalogEntry {
    pub name: String,
    pub description: String,
}

/// `list_operations` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListOperationsOutput {
    pub count: usize,
    pub operations: Vec<OperationCatalogEntry>,
    /// ビルトインプリセット(`apply_transform` / `render_preview` の `preset` に渡せる名前)。
    pub presets: Vec<PresetCatalogEntry>,
    /// 次の一手(完全なスキーマは explain_operation)。
    pub next: String,
}

/// `explain_operation` のパラメータ1行。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExplainParamEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub type_hint: String,
    /// `"required"` | `"optional"` | `"default: ..."`。
    pub requirement: String,
    pub semantics: String,
}

/// `explain_operation` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExplainOperationOutput {
    pub name: String,
    pub category: String,
    pub summary: String,
    pub params: Vec<ExplainParamEntry>,
    /// そのまま `operations` に入れられる JSON 断片(文字列)。
    pub examples: Vec<String>,
    pub warnings: Vec<String>,
}

/// `export_asset` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExportAssetOutput {
    pub revision_id: String,
    /// 実際に書き出した絶対パス。
    pub path: String,
    pub byte_size: u64,
    /// 既存ファイルを上書きした場合 true。
    pub overwritten: bool,
}

// ---------------------------------------------------------------------------
// 結果の組み立て
// ---------------------------------------------------------------------------

/// テキストサマリ + structuredContent を持つ成功結果を作る。
fn ok_result<T: Serialize>(summary: impl Into<String>, value: &T) -> CallToolResult {
    ok_result_with(summary, value, Vec::new())
}

/// 追加の content ブロック(inline image 等)付きの成功結果。
fn ok_result_with<T: Serialize>(
    summary: impl Into<String>,
    value: &T,
    extra: Vec<ContentBlock>,
) -> CallToolResult {
    let structured = match serde_json::to_value(value) {
        Ok(v) => v,
        Err(e) => {
            return tool_error(
                "internal_serialization_failed",
                format!("failed to serialize tool output: {e}"),
                serde_json::Value::Null,
            )
        }
    };
    let mut content = vec![ContentBlock::text(summary)];
    content.extend(extra);
    let mut result = CallToolResult::success(content);
    result.structured_content = Some(structured);
    result
}

/// 構造化エラー結果(is_error=true)。
///
/// content[0] は人間可読メッセージ、content[1] は `{"error": {...}}` の JSON テキスト。
/// outputSchema に適合しないので `structured_content` には載せない。
fn tool_error(
    code: &str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> CallToolResult {
    let message = message.into();
    let payload = serde_json::json!({
        "error": { "code": code, "message": message, "details": details }
    });
    CallToolResult::error(vec![
        ContentBlock::text(message),
        ContentBlock::text(payload.to_string()),
    ])
}

/// [`StoreError`] を構造化エラーに変換する。
fn store_error(err: StoreError) -> CallToolResult {
    match err {
        StoreError::RevisionNotFound(id) => tool_error(
            "revision_not_found",
            format!("revision {id:?} does not exist in this workspace"),
            serde_json::json!({
                "revision_id": id,
                "recovery": "call list_assets to see the available revision_ids, or import_asset first",
            }),
        ),
        other => tool_error(
            "store_error",
            format!("asset store error: {other}"),
            serde_json::Value::Null,
        ),
    }
}

/// [`AtxError`] を構造化エラーに変換する(op index を保持する)。
fn atx_error(err: AtxError) -> CallToolResult {
    match err {
        AtxError::Operation { index, op, message } => tool_error(
            "operation_failed",
            format!("operations[{index}] ({op}) failed: {message}"),
            serde_json::json!({
                "operation_index": index,
                "op": op,
                "reason": message,
                "recovery": "fix that operation in the recipe and call the tool again",
            }),
        ),
        AtxError::InvalidRecipe(message) => tool_error(
            "invalid_recipe",
            format!("invalid recipe: {message}"),
            serde_json::json!({
                "reason": message,
                "recovery": "adjust the recipe to satisfy the constraint above; encode must be the last operation and may appear at most once",
            }),
        ),
        AtxError::Decode(message) => tool_error(
            "decode_failed",
            format!("failed to decode the image: {message}"),
            serde_json::json!({ "reason": message }),
        ),
        AtxError::Encode(message) => tool_error(
            "encode_failed",
            format!("failed to encode the image: {message}"),
            serde_json::json!({ "reason": message }),
        ),
        AtxError::LimitExceeded(message) => tool_error(
            "limit_exceeded",
            format!("input exceeds the configured limits: {message}"),
            serde_json::json!({ "reason": message }),
        ),
        AtxError::Io(e) => tool_error(
            "io_error",
            format!("io error: {e}"),
            serde_json::Value::Null,
        ),
    }
}

/// 画像でない revision に画像ツールを向けたときの構造化エラー。
fn not_an_image(revision_id: &str, mime_type: &str) -> CallToolResult {
    let hint = if mime_type == CUBE_MIME {
        "this is an imported .cube 3D LUT asset, not an image; reference it from a recipe as {\"op\": \"lut\", \"lut_revision_id\": \"...\"} instead of inspecting it"
    } else {
        "call list_assets and pick a revision whose mime_type starts with \"image/\""
    };
    tool_error(
        "not_an_image",
        format!(
            "revision {revision_id:?} has mime_type {mime_type:?} and is not an image, so it cannot be inspected"
        ),
        serde_json::json!({
            "revision_id": revision_id,
            "mime_type": mime_type,
            "recovery": hint,
        }),
    )
}

/// ファイルが .cube 3D LUT かどうかを判定する。
///
/// 判定規則(どちらか一方を満たせば LUT とみなす):
/// 1. 拡張子が `.cube`(大文字小文字を無視)
/// 2. 先頭 64KiB のうち、コメント(`#`)と空行を除いた最初の数行に
///    `LUT_1D_SIZE` / `LUT_3D_SIZE` キーワードがある(Adobe Cube LUT Spec 1.0)
///
/// 画像のマジックバイト判定より**先に**呼ぶ。テキストである .cube は
/// `inspect_bytes` に渡すと「未知フォーマット」エラーになるため。
fn looks_like_cube(path: &Path, bytes: &[u8]) -> bool {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cube"))
    {
        return true;
    }
    let head = &bytes[..bytes.len().min(CUBE_SNIFF_BYTES)];
    let text = String::from_utf8_lossy(head);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        // ヘッダは先頭付近にあるので、無関係なテキストを深追いしない。
        .take(16)
        .any(|line| {
            let upper = line.to_ascii_uppercase();
            upper.starts_with("LUT_1D_SIZE") || upper.starts_with("LUT_3D_SIZE")
        })
}

/// レシピが参照するアセット(v0.3 では `lut` の `lut_revision_id`)を
/// ワークスペースの [`AssetStore`] から解決する [`atx_core::AssetResolver`]。
///
/// revision は不変なので、レシピが id を参照するだけで決定論が保たれる
/// (ROADMAP v0.3 §設計判断)。参照先が無い場合は「ワークスペースに存在しない」ことを
/// 明示し、回復手順(import_asset / list_assets)を含むメッセージにする。
/// engine 側でこのメッセージは `AtxError::Operation { index, op }` に包まれるので、
/// ホスト AI には「どの op の、どの参照が壊れているか」が両方届く。
struct StoreAssets<'a>(&'a AssetStore);

impl atx_core::AssetResolver for StoreAssets<'_> {
    fn read_revision(&self, revision_id: &str) -> atx_core::Result<Vec<u8>> {
        self.0.read_bytes(revision_id).map_err(|e| match e {
            StoreError::RevisionNotFound(id) => AtxError::InvalidRecipe(format!(
                "referenced asset {id:?} was not found in this workspace; \
                 import the .cube file with import_asset first and use the revision_id it \
                 returns, or call list_assets to see the available revision_ids"
            )),
            other => AtxError::InvalidRecipe(format!(
                "referenced asset {revision_id:?} could not be read: {other}"
            )),
        })
    }
}

/// `Result` を早期 return するための小さなマクロ代替。
macro_rules! tri {
    ($expr:expr, $map:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return $map(e),
        }
    };
}

// ---------------------------------------------------------------------------
// ツール本体
// ---------------------------------------------------------------------------

/// ワークスペース1つに紐づくツール実装。プロセス内可変状態は持たない。
pub struct AtxTools {
    store: AssetStore,
    limits: Limits,
}

impl AtxTools {
    /// workspace ディレクトリを開く(存在しなければ作成される)。
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, StoreError> {
        Ok(Self {
            store: AssetStore::open(workspace)?,
            limits: Limits::default(),
        })
    }

    pub fn store(&self) -> &AssetStore {
        &self.store
    }

    // -- 1. import_asset ----------------------------------------------------

    /// ローカルパスからワークスペースへ取り込む。同一内容なら既存 revision を返す(冪等)。
    pub fn import_asset(&self, params: &ImportAssetParams) -> CallToolResult {
        let raw = PathBuf::from(&params.path);
        let path = match raw.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return tool_error(
                    "path_not_found",
                    format!("cannot access {:?}: {e}", params.path),
                    serde_json::json!({
                        "path": params.path,
                        "recovery": "pass an absolute path to an existing local image file",
                    }),
                )
            }
        };
        if !path.is_file() {
            return tool_error(
                "not_a_file",
                format!("{} is not a regular file", path.display()),
                serde_json::json!({ "path": path.to_string_lossy(), "recovery": "pass a path to a file, not a directory" }),
            );
        }

        let bytes = tri!(std::fs::read(&path), |e: std::io::Error| tool_error(
            "io_error",
            format!("failed to read {}: {e}", path.display()),
            serde_json::Value::Null
        ));

        // 画像ではないアセット(v0.3: レシピから参照される .cube 3D LUT)は
        // 画像としての検査を行わず、寸法 0x0 の擬似 MIME で台帳に載せる。
        let is_cube = looks_like_cube(&path, &bytes);
        if is_cube && bytes.len() as u64 > MAX_CUBE_BYTES {
            return tool_error(
                "limit_exceeded",
                format!(
                    "{} looks like a .cube LUT but is {} bytes, over the {MAX_CUBE_BYTES} byte limit for LUT assets",
                    path.display(),
                    bytes.len()
                ),
                serde_json::json!({
                    "path": path.to_string_lossy(),
                    "byte_size": bytes.len(),
                    "max_bytes": MAX_CUBE_BYTES,
                    "recovery": "a .cube file this large is almost certainly not a LUT; check the file, or use a smaller LUT size",
                }),
            );
        }
        let (mime_type, width, height) = if is_cube {
            (CUBE_MIME.to_string(), 0, 0)
        } else {
            let info = tri!(atx_core::inspect_bytes(&bytes, &self.limits), atx_error);
            // 寸法は EXIF Orientation 適用後の実効値を記録する(atx-core はデコード時に
            // 必ず Orientation を焼き込むため、以降の変換もこの向きが基準)。
            (info.mime_type, info.oriented_width, info.oriented_height)
        };

        let mut origin = BTreeMap::new();
        origin.insert(
            "source_path".to_string(),
            path.to_string_lossy().into_owned(),
        );
        origin.insert(
            "file_name".to_string(),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        if is_cube {
            origin.insert("asset_kind".to_string(), "lut".to_string());
        }

        let known_before = tri!(self.known_revision_ids(), store_error);
        let revision = tri!(
            self.store
                .import_bytes(&bytes, &mime_type, width, height, origin),
            store_error
        );
        let reused = known_before.contains(&revision.revision_id);
        let summary = RevisionSummary::new(&self.store, &revision);

        let verb = if reused {
            "Reused existing import of"
        } else {
            "Imported"
        };
        let text = if is_cube {
            format!(
                "{verb} {} as {} (.cube LUT asset, {}, {} bytes). It is not an image: reference it from a recipe as {{\"op\": \"lut\", \"lut_revision_id\": \"{}\"}}.\npath: {}",
                path.display(),
                summary.revision_id,
                summary.mime_type,
                summary.byte_size,
                summary.revision_id,
                summary.path,
            )
        } else {
            format!(
                "{verb} {} as {} ({}x{} {}, {} bytes)\npath: {}",
                path.display(),
                summary.revision_id,
                summary.width,
                summary.height,
                summary.mime_type,
                summary.byte_size,
                summary.path,
            )
        };
        ok_result(
            text,
            &ImportOutput {
                revision: summary,
                reused,
                source_path: path.to_string_lossy().into_owned(),
            },
        )
    }

    // -- 2. inspect_image ---------------------------------------------------

    /// revision の寸法・フォーマット・EXIF 要約・色情報・容量を返す(read-only)。
    pub fn inspect_image(&self, params: &RevisionParams) -> CallToolResult {
        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT 等)はデコードを試みず、構造化エラーで返す。
        if !revision.mime_type.starts_with("image/") {
            return not_an_image(&params.revision_id, &revision.mime_type);
        }
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        let info = tri!(atx_core::inspect_bytes(&bytes, &self.limits), atx_error);
        let path = self
            .store
            .abs_path(&revision)
            .to_string_lossy()
            .into_owned();

        let text = format!(
            "{}: {}x{} {} ({} bytes){}{}\npath: {}",
            params.revision_id,
            info.width,
            info.height,
            info.mime_type,
            info.byte_size,
            match info.exif_orientation {
                Some(o) if o != 1 => format!(
                    ", EXIF orientation {o} (effective {}x{})",
                    info.oriented_width, info.oriented_height
                ),
                _ => String::new(),
            },
            if info.has_gps {
                ", contains GPS EXIF (PII)"
            } else {
                ""
            },
            path,
        );
        ok_result(
            text,
            &InspectOutput {
                revision_id: params.revision_id.clone(),
                path,
                info,
            },
        )
    }

    // -- 3. detect_tilt -----------------------------------------------------

    /// 傾き角候補を返す(read-only、自動適用はしない)。
    pub fn detect_tilt(&self, params: &DetectTiltParams) -> CallToolResult {
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        let info = tri!(atx_core::inspect_bytes(&bytes, &self.limits), atx_error);
        let image = tri!(
            image::load_from_memory(&bytes).map_err(|e| AtxError::Decode(e.to_string())),
            atx_error
        );
        // atx-core はデコード時に必ず Orientation を正規化する。検出も同じ向きで行う。
        let image = apply_orientation(image, info.exif_orientation.unwrap_or(1));

        let detect_params = DetectParams {
            max_abs_angle: params
                .max_abs_angle
                .unwrap_or(DetectParams::default().max_abs_angle),
            ..DetectParams::default()
        };
        let detection = atx_geometry::detect_tilt(&image, &detect_params);

        let text = match detection.recommended_angle_degrees {
            Some(angle) => format!(
                "{}: recommended rotation {angle:+.2}deg (confidence {:.2}, method {}). Apply with an explicit apply_transform rotate operation if it matches the intent.",
                params.revision_id, detection.confidence, detection.method
            ),
            None => format!(
                "{}: no reliable tilt detected (confidence {:.2}); leaving the image as-is is the correct answer.",
                params.revision_id, detection.confidence
            ),
        };
        // 水平族 / 垂直族の内訳はテキスト側にも出す(ホスト AI が
        // 「ロールなのか、カメラ位置・パースなのか」を判断できるように)。
        let text = match (
            detection.horizontal_angle_degrees,
            detection.vertical_angle_degrees,
        ) {
            (None, None) => text,
            (h, v) => format!(
                "{text}\nhorizontal lines: {} (confidence {:.2}, support {:.2}); vertical lines: {} (confidence {:.2}, support {:.2})",
                fmt_angle(h),
                detection.horizontal_confidence,
                detection.horizontal_support,
                fmt_angle(v),
                detection.vertical_confidence,
                detection.vertical_support,
            ),
        };
        let text = if detection.warnings.is_empty() {
            text
        } else {
            format!("{text}\nwarnings: {}", detection.warnings.join("; "))
        };
        ok_result(
            text,
            &DetectTiltOutput {
                revision_id: params.revision_id.clone(),
                detection,
            },
        )
    }

    // -- 3b. list_operations ------------------------------------------------

    /// レシピ語彙の軽量カタログを返す(read-only)。
    ///
    /// ROADMAP §Agent UX #2「語彙の段階的開示」: `apply_transform` の inputSchema に
    /// 全 op を埋め込まず、必要になった時だけこのツールで一覧を取る。
    /// 完全なパラメータ表・例・注意点は `explain_operation` 側。
    pub fn list_operations(&self, params: &ListOperationsParams) -> CallToolResult {
        if let Some(category) = params.category.as_deref() {
            if !crate::vocab::CATEGORIES.contains(&category) {
                return tool_error(
                    "invalid_category",
                    format!(
                        "unknown category {category:?}; valid values are {:?}",
                        crate::vocab::CATEGORIES
                    ),
                    serde_json::json!({
                        "given": category,
                        "valid_values": crate::vocab::CATEGORIES,
                        "recovery": "call list_operations again with category omitted or one of the valid values",
                    }),
                );
            }
        }

        let selected: Vec<&'static crate::vocab::OpDoc> = crate::vocab::OPERATIONS
            .iter()
            .filter(|op| match params.category.as_deref() {
                Some(category) => op.category == category,
                None => true,
            })
            .collect();

        let entries: Vec<OperationCatalogEntry> = selected
            .iter()
            .map(|op| OperationCatalogEntry {
                name: op.name.to_string(),
                category: op.category.to_string(),
                summary: op.summary.to_string(),
                params: op.params.iter().map(|p| p.compact()).collect(),
            })
            .collect();

        let presets: Vec<PresetCatalogEntry> = crate::presets::all()
            .into_iter()
            .map(|p| PresetCatalogEntry {
                name: p.name,
                description: p.description,
            })
            .collect();

        let mut text = format!(
            "{} recipe operations{} (params: name* = required, name? = optional, =x or (def) = default):",
            entries.len(),
            match params.category.as_deref() {
                Some(c) => format!(" in category {c}"),
                None => String::new(),
            }
        );
        for entry in &entries {
            text.push_str(&format!(
                "\n- {} [{}] {}{}",
                entry.name,
                entry.category,
                entry.summary,
                if entry.params.is_empty() {
                    String::new()
                } else {
                    format!(" | {}", entry.params.join(", "))
                }
            ));
        }
        if !presets.is_empty() {
            text.push_str("\nPresets (pass preset=<name> instead of recipe):");
            for preset in &presets {
                text.push_str(&format!("\n- {}: {}", preset.name, preset.description));
            }
        }
        let next =
            "call explain_operation {\"operation\":\"<name>\"} for full params, examples and gotchas".to_string();
        text.push_str(&format!("\n{next}."));

        ok_result(
            text,
            &ListOperationsOutput {
                count: entries.len(),
                operations: entries,
                presets,
                next,
            },
        )
    }

    // -- 3c. explain_operation ----------------------------------------------

    /// 1つの op の完全な仕様(パラメータ表・例・落とし穴)を返す(read-only)。
    pub fn explain_operation(&self, params: &ExplainOperationParams) -> CallToolResult {
        let doc = match crate::vocab::find(params.operation.trim()) {
            Some(doc) => doc,
            None => {
                let valid = crate::vocab::operation_names();
                let suggestions = crate::vocab::did_you_mean(&params.operation);
                return tool_error(
                    "unknown_operation",
                    format!(
                        "unknown operation {:?}; valid operations are {}",
                        params.operation,
                        valid.join(", ")
                    ),
                    serde_json::json!({
                        "given": params.operation,
                        "valid_values": valid,
                        "did_you_mean": suggestions,
                        "recovery": "call explain_operation again with one of valid_values, or list_operations for the catalog",
                    }),
                );
            }
        };

        let param_entries: Vec<ExplainParamEntry> = doc
            .params
            .iter()
            .map(|p| ExplainParamEntry {
                name: p.name.to_string(),
                type_hint: p.type_hint.to_string(),
                requirement: p.requirement.to_string(),
                semantics: p.semantics.to_string(),
            })
            .collect();

        let mut text = format!("{} [{}] — {}\n", doc.name, doc.category, doc.summary);
        if param_entries.is_empty() {
            text.push_str("Parameters: none.\n");
        } else {
            text.push_str("Parameters:\n");
            for p in &param_entries {
                text.push_str(&format!(
                    "- {} ({}, {}): {}\n",
                    p.name, p.type_hint, p.requirement, p.semantics
                ));
            }
        }
        text.push_str("Examples (drop straight into \"operations\"):\n");
        for example in doc.examples {
            text.push_str(&format!("- {example}\n"));
        }
        if !doc.warnings.is_empty() {
            text.push_str("Watch out:\n");
            for warning in doc.warnings {
                text.push_str(&format!("- {warning}\n"));
            }
        }

        ok_result(
            text.trim_end(),
            &ExplainOperationOutput {
                name: doc.name.to_string(),
                category: doc.category.to_string(),
                summary: doc.summary.to_string(),
                params: param_entries,
                examples: doc.examples.iter().map(|e| e.to_string()).collect(),
                warnings: doc.warnings.iter().map(|w| w.to_string()).collect(),
            },
        )
    }

    // -- 3d. generate_mask --------------------------------------------------

    /// 決定論的にグレースケールマスクを生成し、画像 revision として発行する(v0.5)。
    ///
    /// ROADMAP §Agent UX の規律 #1 が許す「生成系/検出系」のツール追加にあたる:
    /// op を増やすのではなく、**マスクという第一級アセットを作る動詞**を1つ足す。
    ///
    /// 冪等性: 同じ params + 同じ参照画像 → 同じ PNG バイト列 → ストアの sha256 dedup で
    /// 既存 revision がそのまま返る(`reused: true`)。
    pub fn generate_mask(&self, params: &crate::mask::GenerateMaskParams) -> CallToolResult {
        let spec = match crate::mask::build(params) {
            Ok(spec) => spec,
            Err(e) => return tool_error(e.code, e.message, e.details),
        };

        let reference = tri!(
            self.store.get_revision(&params.reference_revision_id),
            store_error
        );
        if !reference.mime_type.starts_with("image/") {
            return not_an_image(&params.reference_revision_id, &reference.mime_type);
        }
        let bytes = tri!(
            self.store.read_bytes(&params.reference_revision_id),
            store_error
        );
        let info = tri!(atx_core::inspect_bytes(&bytes, &self.limits), atx_error);
        let image = tri!(
            image::load_from_memory(&bytes).map_err(|e| AtxError::Decode(e.to_string())),
            atx_error
        );
        // atx-core はデコード時に必ず Orientation を焼き込むので、マスクも同じ向き
        // ・同じ寸法(= 実効寸法)で作る。そうでないと op 側で寸法が食い違う。
        let image = apply_orientation(image, info.exif_orientation.unwrap_or(1)).to_rgb8();

        let rendered = spec.render(&image);
        let (width, height) = rendered.dimensions();
        let mean_weight = crate::mask::mean_weight(&rendered);
        let png = tri!(crate::mask::encode_png(&rendered), |e: String| tool_error(
            "encode_failed",
            format!("failed to encode the mask as png: {e}"),
            serde_json::Value::Null
        ));

        let generator = spec.canonical_json();
        let mut origin = BTreeMap::new();
        origin.insert("asset_kind".to_string(), "mask".to_string());
        origin.insert("generator".to_string(), generator.clone());

        let known_before = tri!(self.known_revision_ids(), store_error);
        let revision = tri!(
            self.store
                .import_bytes(&png, "image/png", width, height, origin),
            store_error
        );
        let reused = known_before.contains(&revision.revision_id);
        let summary = RevisionSummary::new(&self.store, &revision);

        let next = format!(
            "reference it from any tone/filter op as \"mask\": {{\"revision_id\": \"{}\"}} (optionally with \"invert\": true or \"feather_px\": <sigma>), or visualise it with render_preview overlay=\"mask\"",
            summary.revision_id
        );
        let text = format!(
            "{} a {} mask as {} ({}x{} 8-bit grayscale png, {} bytes, mean weight {:.3}). White = the op applies fully, black = not at all.\n{}\nparams: {}\npath: {}",
            if reused {
                "Reused the identical"
            } else {
                "Generated"
            },
            spec.kind(),
            summary.revision_id,
            summary.width,
            summary.height,
            summary.byte_size,
            mean_weight,
            next,
            generator,
            summary.path,
        );
        ok_result(
            text,
            &GenerateMaskOutput {
                revision: summary,
                kind: spec.kind().to_string(),
                reference_revision_id: params.reference_revision_id.clone(),
                width,
                height,
                generator,
                mean_weight,
                reused,
                next,
            },
        )
    }

    // -- 4. apply_transform -------------------------------------------------

    /// レシピを高解像度で適用し、新しい revision を発行する。
    ///
    /// 冪等性: `(source_revision_id, recipe_hash)` が台帳に既存なら、
    /// 変換自体を走らせずに既存 revision を返す(ショートサーキット)。
    ///
    /// `preset` を渡した場合は解決後のレシピがそのまま以降の処理に流れる
    /// (= `recipe_hash` は解決後のレシピに対して計算される。プリセットは純粋な糖衣)。
    pub fn apply_transform(&self, params: &TransformParams) -> CallToolResult {
        let recipe = match resolve_recipe(params.recipe.as_ref(), params.preset.as_deref()) {
            Ok(recipe) => recipe,
            Err(result) => return result,
        };
        tri!(atx_core::recipe::validate(&recipe), atx_error);
        let recipe_hash = tri!(atx_core::recipe_hash(&recipe), atx_error);
        let source = tri!(self.store.get_revision(&params.revision_id), store_error);

        // --- 冪等ショートサーキット: 既存派生があれば再変換しない ---
        let existing = tri!(self.store.list_revisions(None), store_error)
            .into_iter()
            .find(|r| {
                r.source_revision_id.as_deref() == Some(params.revision_id.as_str())
                    && r.recipe_hash.as_deref() == Some(recipe_hash.as_str())
            });
        if let Some(revision) = existing {
            let summary = RevisionSummary::new(&self.store, &revision);
            let text = format!(
                "Reused existing revision {} for this recipe (no re-transform): {}x{} {} ({} bytes)\npath: {}",
                summary.revision_id, summary.width, summary.height, summary.mime_type, summary.byte_size, summary.path
            );
            return ok_result(
                text,
                &ApplyTransformOutput {
                    revision: summary,
                    source_revision_id: params.revision_id.clone(),
                    recipe_hash,
                    engine_version: ENGINE_VERSION.to_string(),
                    reused: true,
                    warnings: Vec::new(),
                },
            );
        }

        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        let output = tri!(
            atx_core::apply_recipe_with_assets(
                &bytes,
                &recipe,
                &self.limits,
                &StoreAssets(&self.store)
            ),
            atx_error
        );
        let revision = tri!(
            self.store.record_derivation(
                &source.revision_id,
                &recipe,
                &recipe_hash,
                &output.bytes,
                &output.mime_type,
                output.width,
                output.height,
            ),
            store_error
        );
        let summary = RevisionSummary::new(&self.store, &revision);
        let text = format!(
            "Applied {}recipe to {} -> {} ({}x{} {}, {} bytes){}\npath: {}",
            preset_note(params.preset.as_deref()),
            params.revision_id,
            summary.revision_id,
            summary.width,
            summary.height,
            summary.mime_type,
            summary.byte_size,
            if output.warnings.is_empty() {
                String::new()
            } else {
                format!("\nwarnings: {}", output.warnings.join("; "))
            },
            summary.path,
        );
        ok_result(
            text,
            &ApplyTransformOutput {
                revision: summary,
                source_revision_id: params.revision_id.clone(),
                recipe_hash,
                engine_version: ENGINE_VERSION.to_string(),
                reused: false,
                warnings: output.warnings,
            },
        )
    }

    // -- 5. render_preview --------------------------------------------------

    /// レシピを適用したうえで長辺 ≤ 768 に縮小し、jpeg で inline 返却する。
    ///
    /// # コスト
    ///
    /// v1 はレシピを**フル解像度で**適用してから縮小する(単一パス: レシピの
    /// encode op だけを差し替え、末尾に `resize(contain 768) + encode(jpeg 80)` を足す)。
    /// 事前縮小してから適用する最適化はしていないため、プレビューでも
    /// `apply_transform` と同等の画素処理コストがかかる。
    /// 代わりに「プレビューで見た構図 = 本適用の構図」が厳密に一致する。
    pub fn render_preview(&self, params: &RenderPreviewParams) -> CallToolResult {
        let recipe = match resolve_recipe(params.recipe.as_ref(), params.preset.as_deref()) {
            Ok(recipe) => recipe,
            Err(result) => return result,
        };
        tri!(atx_core::recipe::validate(&recipe), atx_error);
        if let Some(overlay) = params.overlay.as_deref() {
            if !OVERLAY_VALUES.contains(&overlay) {
                return tool_error(
                    "invalid_overlay",
                    format!("unknown overlay {overlay:?}; valid values are {OVERLAY_VALUES:?}"),
                    serde_json::json!({
                        "given": overlay,
                        "valid_values": OVERLAY_VALUES,
                        "recovery": "call render_preview again with overlay omitted or one of the valid values",
                    }),
                );
            }
        }
        // overlay="mask" と mask_revision_id は相互に必須・排他(片方だけでは意味がない)。
        let mask_revision_id = match (params.overlay.as_deref(), params.mask_revision_id.as_deref())
        {
            (Some("mask"), Some(id)) => Some(id.to_string()),
            (Some("mask"), None) => {
                return tool_error(
                    "mask_revision_id_required",
                    "overlay \"mask\" visualises a mask, so mask_revision_id is required",
                    serde_json::json!({
                        "overlay": "mask",
                        "recovery": "pass mask_revision_id (call generate_mask to create one, or import_asset an existing grayscale image), or use a different overlay",
                    }),
                )
            }
            (other, Some(id)) => {
                return tool_error(
                    "mask_revision_id_without_mask_overlay",
                    format!(
                        "mask_revision_id was given but overlay is {}; it is only meaningful with overlay=\"mask\"",
                        match other { Some(o) => format!("{o:?}"), None => "not set".to_string() }
                    ),
                    serde_json::json!({
                        "given_overlay": other,
                        "mask_revision_id": id,
                        "recovery": "set overlay=\"mask\" to visualise it, or drop mask_revision_id",
                    }),
                )
            }
            (_, None) => None,
        };
        // 冪等キーは(プリセット解決後の)ユーザレシピのハッシュ。
        // プレビュー用に差し替えた encode は含めない。
        let recipe_hash = tri!(atx_core::recipe_hash(&recipe), atx_error);
        tri!(self.store.get_revision(&params.revision_id), store_error);

        let preview_recipe = preview_recipe_of(&recipe);
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        let output = tri!(
            atx_core::apply_recipe_with_assets(
                &bytes,
                &preview_recipe,
                &self.limits,
                &StoreAssets(&self.store)
            ),
            atx_error
        );

        // guide overlay はプレビュー jpeg パイプラインの後段(デコード済み画素へ)で描く。
        // 描画色は per-pixel のコントラスト適応ではなく、固定の高視認性色
        // #FF3355 を ~60% 不透明度でブレンドする(実装が単純で、どんな背景でも
        // 見失いにくいため。per-pixel 適応は今回は見送った)。
        // overlay="mask" だけは別経路: 参照マスクをプレビュー寸法へ合わせ、
        // 重み > 0.5 の被覆域を赤で染め、それ以外を軽く落として被覆を目視できるようにする。
        let final_bytes = match (params.overlay.as_deref(), mask_revision_id.as_deref()) {
            (Some("mask"), Some(mask_id)) => {
                let mask_revision = tri!(self.store.get_revision(mask_id), store_error);
                if !mask_revision.mime_type.starts_with("image/") {
                    return not_an_image(mask_id, &mask_revision.mime_type);
                }
                let mask_bytes = tri!(self.store.read_bytes(mask_id), store_error);
                tri!(
                    draw_mask_overlay_jpeg(&output.bytes, &mask_bytes),
                    |e: String| tool_error(
                        "overlay_render_failed",
                        format!("failed to draw the mask overlay for {mask_id:?}: {e}"),
                        serde_json::json!({
                            "mask_revision_id": mask_id,
                            "recovery": "make sure mask_revision_id points at a decodable image revision",
                        }),
                    )
                )
            }
            (Some(overlay), _) => tri!(draw_overlay_jpeg(&output.bytes, overlay), |e: String| {
                tool_error(
                    "overlay_render_failed",
                    format!("failed to draw overlay {overlay:?}: {e}"),
                    serde_json::Value::Null,
                )
            }),
            (None, _) => output.bytes.clone(),
        };

        let key = preview_key(
            &params.revision_id,
            &recipe_hash,
            params.overlay.as_deref(),
            mask_revision_id.as_deref(),
        );
        let path = tri!(
            self.store.put_preview(&key, "jpg", &final_bytes),
            store_error
        );
        let path = path.to_string_lossy().into_owned();

        let text = format!(
            "Preview of {} with this {}recipe: {}x{} jpeg ({} bytes, long edge <= {}).{} This is a downscaled proof; call apply_transform with the same recipe for the full-resolution revision.{}\npath: {}",
            params.revision_id,
            preset_note(params.preset.as_deref()),
            output.width,
            output.height,
            final_bytes.len(),
            PREVIEW_LONG_EDGE,
            match (params.overlay.as_deref(), mask_revision_id.as_deref()) {
                (Some("mask"), Some(id)) => format!(
                    " Mask overlay: {id} is tinted red where its weight exceeds {MASK_OVERLAY_THRESHOLD}, and the rest is dimmed."
                ),
                (Some(o), _) => format!(" Guide overlay: {o}."),
                (None, _) => String::new(),
            },
            if output.warnings.is_empty() {
                String::new()
            } else {
                format!("\nwarnings: {}", output.warnings.join("; "))
            },
            path,
        );
        let image_block = ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(&final_bytes),
            "image/jpeg",
        );
        ok_result_with(
            text,
            &RenderPreviewOutput {
                source_revision_id: params.revision_id.clone(),
                recipe_hash,
                engine_version: ENGINE_VERSION.to_string(),
                preview_path: path,
                width: output.width,
                height: output.height,
                byte_size: final_bytes.len() as u64,
                mime_type: output.mime_type,
                warnings: output.warnings,
                overlay: params.overlay.clone(),
                mask_revision_id,
            },
            vec![image_block],
        )
    }

    // -- 5b. compare_revisions ----------------------------------------------

    /// 2つの revision を長辺 <= 640 に縮小し、1枚のキャンバスへ並べて jpeg で返す。
    pub fn compare_revisions(&self, params: &CompareRevisionsParams) -> CallToolResult {
        let rev_a = match self.store.get_revision(&params.revision_id_a) {
            Ok(r) => r,
            Err(StoreError::RevisionNotFound(id)) => return revision_not_found_side(&id, "a"),
            Err(e) => return store_error(e),
        };
        let rev_b = match self.store.get_revision(&params.revision_id_b) {
            Ok(r) => r,
            Err(StoreError::RevisionNotFound(id)) => return revision_not_found_side(&id, "b"),
            Err(e) => return store_error(e),
        };

        let bytes_a = tri!(self.store.read_bytes(&params.revision_id_a), store_error);
        let bytes_b = tri!(self.store.read_bytes(&params.revision_id_b), store_error);

        let img_a = tri!(
            image::load_from_memory(&bytes_a)
                .map_err(|e| AtxError::Decode(format!("revision {}: {e}", rev_a.revision_id))),
            atx_error
        );
        let img_b = tri!(
            image::load_from_memory(&bytes_b)
                .map_err(|e| AtxError::Decode(format!("revision {}: {e}", rev_b.revision_id))),
            atx_error
        );

        let scaled_a = scale_contain(img_a, COMPARE_LONG_EDGE).to_rgb8();
        let scaled_b = scale_contain(img_b, COMPARE_LONG_EDGE).to_rgb8();

        let canvas = match params.layout {
            CompareLayout::SideBySide => compose_side_by_side(&scaled_a, &scaled_b, COMPARE_GAP_PX),
            CompareLayout::Stacked => compose_stacked(&scaled_a, &scaled_b, COMPARE_GAP_PX),
        };
        let (cw, ch) = canvas.dimensions();
        let composed_bytes = tri!(encode_jpeg_q80(&canvas), |e: String| tool_error(
            "encode_failed",
            format!("failed to encode comparison jpeg: {e}"),
            serde_json::Value::Null
        ));

        let key = compare_key(
            &params.revision_id_a,
            &params.revision_id_b,
            params.layout.as_str(),
        );
        let path = tri!(
            self.store.put_preview(&key, "jpg", &composed_bytes),
            store_error
        );
        let path = path.to_string_lossy().into_owned();

        let side_a = CompareSide {
            revision_id: rev_a.revision_id.clone(),
            width: rev_a.width,
            height: rev_a.height,
            mime_type: rev_a.mime_type.clone(),
            byte_size: rev_a.byte_size,
            recipe_hash: rev_a.recipe_hash.clone(),
        };
        let side_b = CompareSide {
            revision_id: rev_b.revision_id.clone(),
            width: rev_b.width,
            height: rev_b.height,
            mime_type: rev_b.mime_type.clone(),
            byte_size: rev_b.byte_size,
            recipe_hash: rev_b.recipe_hash.clone(),
        };
        let (a_position, b_position) = match params.layout {
            CompareLayout::SideBySide => ("left", "right"),
            CompareLayout::Stacked => ("top", "bottom"),
        };

        let text = format!(
            "Compared A={} ({}x{} {}, {} bytes) vs B={} ({}x{} {}, {} bytes), layout={}: composed {}x{} jpeg ({} bytes)\npath: {}",
            side_a.revision_id,
            side_a.width,
            side_a.height,
            side_a.mime_type,
            side_a.byte_size,
            side_b.revision_id,
            side_b.width,
            side_b.height,
            side_b.mime_type,
            side_b.byte_size,
            params.layout.as_str(),
            cw,
            ch,
            composed_bytes.len(),
            path,
        );
        let image_block = ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(&composed_bytes),
            "image/jpeg",
        );
        ok_result_with(
            text,
            &CompareRevisionsOutput {
                layout: params.layout.as_str().to_string(),
                a: side_a,
                b: side_b,
                a_position: a_position.to_string(),
                b_position: b_position.to_string(),
                width: cw,
                height: ch,
                mime_type: "image/jpeg".to_string(),
                byte_size: composed_bytes.len() as u64,
                preview_path: path,
            },
            vec![image_block],
        )
    }

    // -- 6. list_assets -----------------------------------------------------

    /// 台帳を列挙する(read-only)。
    pub fn list_assets(&self, params: &ListAssetsParams) -> CallToolResult {
        let revisions = tri!(
            self.store.list_revisions(params.asset_id.as_deref()),
            store_error
        );
        let summaries: Vec<RevisionSummary> = revisions
            .iter()
            .map(|r| RevisionSummary::new(&self.store, r))
            .collect();

        let mut text = format!(
            "{} revision(s){}",
            summaries.len(),
            match &params.asset_id {
                Some(id) => format!(" for asset {id}"),
                None => String::new(),
            }
        );
        for s in &summaries {
            text.push_str(&format!(
                "\n- {} ({}x{} {}, {} bytes){}",
                s.revision_id,
                s.width,
                s.height,
                s.mime_type,
                s.byte_size,
                match &s.source_revision_id {
                    Some(src) => format!(" derived from {src}"),
                    None => " imported".to_string(),
                }
            ));
        }
        ok_result(
            text,
            &ListAssetsOutput {
                count: summaries.len(),
                revisions: summaries,
            },
        )
    }

    // -- 7. export_asset ----------------------------------------------------

    /// revision をワークスペース外へ書き出す。既存ファイルは `overwrite: true` の明示が必要。
    pub fn export_asset(&self, params: &ExportAssetParams) -> CallToolResult {
        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);

        let dest = match absolutize(&params.dest_path) {
            Ok(p) => p,
            Err(e) => {
                return tool_error(
                    "invalid_dest_path",
                    format!("cannot resolve dest_path {:?}: {e}", params.dest_path),
                    serde_json::json!({ "dest_path": params.dest_path }),
                )
            }
        };

        // ワークスペースの管理領域(objects / previews)への書き込みは常に拒否する。
        let objects = self.store.root().join("objects");
        let previews = self.store.root().join("previews");
        if dest.starts_with(&objects) || dest.starts_with(&previews) {
            return tool_error(
                "dest_inside_workspace",
                format!(
                    "{} is inside the immutable workspace store; exporting there is not allowed",
                    dest.display()
                ),
                serde_json::json!({
                    "dest_path": dest.to_string_lossy(),
                    "workspace": self.store.root().to_string_lossy(),
                    "recovery": "choose a destination outside <workspace>/objects and <workspace>/previews",
                }),
            );
        }

        let exists = dest.exists();
        if exists && !params.overwrite {
            return tool_error(
                "dest_exists",
                format!(
                    "{} already exists; refusing to overwrite it",
                    dest.display()
                ),
                serde_json::json!({
                    "dest_path": dest.to_string_lossy(),
                    "recovery": "ask the user to confirm, then call export_asset again with overwrite=true, or pick a different dest_path",
                }),
            );
        }
        if exists && dest.is_dir() {
            return tool_error(
                "dest_is_directory",
                format!("{} is a directory", dest.display()),
                serde_json::json!({ "dest_path": dest.to_string_lossy(), "recovery": "pass a full file path including the file name" }),
            );
        }
        if let Some(parent) = dest.parent() {
            if !parent.exists() {
                return tool_error(
                    "dest_parent_missing",
                    format!("directory {} does not exist", parent.display()),
                    serde_json::json!({
                        "dest_path": dest.to_string_lossy(),
                        "recovery": "create the directory first, or choose an existing one",
                    }),
                );
            }
        }

        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        tri!(std::fs::write(&dest, &bytes), |e: std::io::Error| {
            tool_error(
                "io_error",
                format!("failed to write {}: {e}", dest.display()),
                serde_json::Value::Null,
            )
        });

        let path = dest.to_string_lossy().into_owned();
        let text = format!(
            "Exported {} ({}x{} {}, {} bytes) to {}{}",
            params.revision_id,
            revision.width,
            revision.height,
            revision.mime_type,
            bytes.len(),
            path,
            if exists {
                " (overwrote the existing file)"
            } else {
                ""
            },
        );
        ok_result(
            text,
            &ExportAssetOutput {
                revision_id: params.revision_id.clone(),
                path,
                byte_size: bytes.len() as u64,
                overwritten: exists,
            },
        )
    }

    // -- helpers ------------------------------------------------------------

    fn known_revision_ids(&self) -> Result<Vec<String>, StoreError> {
        Ok(self
            .store
            .list_revisions(None)?
            .into_iter()
            .map(|r| r.revision_id)
            .collect())
    }
}

/// ユーザレシピからプレビュー用レシピを作る。
///
/// ユーザ指定の encode を落とし、末尾に「長辺 768 に収める contain リサイズ」+
/// 「jpeg quality 80」を足す。拡大はしない(`without_enlargement: true`)。
/// 角度をテキストサマリ用に整形する(未検出は "n/a")。
fn fmt_angle(a: Option<f64>) -> String {
    match a {
        Some(v) => format!("{v:+.2}deg"),
        None => "n/a".to_string(),
    }
}

/// `recipe` / `preset` のどちらか一方から、実際に適用するレシピを決める。
///
/// プリセットは**純粋な糖衣**である: 解決後は生レシピと完全に同じ経路を通り、
/// `recipe_hash`(= 冪等キー)は**解決後のレシピ**に対して計算される。
/// したがって `preset: "web_optimize"` と、その中身をそのまま書いた生レシピは
/// 同じ revision に落ちる。
fn resolve_recipe(
    recipe: Option<&TransformRecipe>,
    preset: Option<&str>,
) -> Result<TransformRecipe, CallToolResult> {
    match (recipe, preset) {
        (Some(_), Some(preset)) => Err(tool_error(
            "recipe_and_preset_conflict",
            format!(
                "recipe and preset are mutually exclusive, but both were given (preset {preset:?})"
            ),
            serde_json::json!({
                "preset": preset,
                "valid_presets": crate::presets::preset_names(),
                "recovery": "call again with either recipe (a raw {\"operations\": [...]} DSL) or preset (a built-in name), not both",
            }),
        )),
        (None, None) => Err(tool_error(
            "recipe_or_preset_required",
            "one of recipe or preset is required",
            serde_json::json!({
                "valid_presets": crate::presets::preset_names(),
                "recovery": "pass recipe = {\"operations\": [...]} (call list_operations / explain_operation for the vocabulary), or preset = one of valid_presets",
            }),
        )),
        (Some(recipe), None) => Ok(recipe.clone()),
        (None, Some(name)) => match crate::presets::resolve(name) {
            Ok(preset) => Ok(preset.recipe),
            Err(crate::presets::PresetError::Unknown) => Err(tool_error(
                "unknown_preset",
                format!(
                    "unknown preset {name:?}; valid presets are {}",
                    crate::presets::preset_names().join(", ")
                ),
                serde_json::json!({
                    "given": name,
                    "valid_values": crate::presets::preset_names(),
                    "recovery": "call list_operations to see the presets with their descriptions, then retry with one of valid_values (or pass a raw recipe instead)",
                }),
            )),
            Err(crate::presets::PresetError::Malformed(reason)) => Err(tool_error(
                "preset_malformed",
                format!("built-in preset {name:?} could not be parsed: {reason}"),
                serde_json::json!({
                    "given": name,
                    "reason": reason,
                    "recovery": "this is a server bug; pass an explicit recipe instead",
                }),
            )),
        },
    }
}

/// テキストサマリ用: プリセット由来なら `"preset \"x\" "` を、生レシピなら空文字を返す。
fn preset_note(preset: Option<&str>) -> String {
    match preset {
        Some(name) => format!("preset {name:?} "),
        None => String::new(),
    }
}

fn preview_recipe_of(recipe: &TransformRecipe) -> TransformRecipe {
    let mut operations: Vec<Operation> = recipe
        .operations
        .iter()
        .filter(|op| !matches!(op, Operation::Encode { .. }))
        .cloned()
        .collect();
    operations.push(Operation::Resize {
        width: Some(PREVIEW_LONG_EDGE),
        height: Some(PREVIEW_LONG_EDGE),
        fit: Fit::Contain,
        without_enlargement: true,
    });
    operations.push(Operation::Encode {
        format: OutputFormat::Jpeg,
        quality: Some(PREVIEW_JPEG_QUALITY),
        bit_depth: None,
    });
    TransformRecipe { operations }
}

/// プレビューのキャッシュキー:
/// sha256(source_revision + recipe_hash + overlay + mask_revision_id) の先頭 32 文字。
///
/// `overlay` をハッシュ入力に含めることで、同じ (revision, recipe) でも
/// overlay の有無・種類ごとに別ファイルとしてキャッシュされ、
/// overlay 付きプレビューが overlay なしプレビューを上書きしない。
/// `overlay="mask"` は可視化するマスクごとに絵が変わるので、
/// マスクの revision id もキーに含める(でないと別マスクの結果を掴む)。
fn preview_key(
    source_revision_id: &str,
    recipe_hash: &str,
    overlay: Option<&str>,
    mask_revision_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_revision_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(recipe_hash.as_bytes());
    hasher.update([0u8]);
    hasher.update(overlay.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(mask_revision_id.unwrap_or("").as_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..32].to_string()
}

/// `compare_revisions` のキャッシュキー: sha256(a + b + layout) の先頭 32 文字に
/// 目視でそれと分かる接頭辞を付けたもの。
fn compare_key(revision_id_a: &str, revision_id_b: &str, layout: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(revision_id_a.as_bytes());
    hasher.update([0u8]);
    hasher.update(revision_id_b.as_bytes());
    hasher.update([0u8]);
    hasher.update(layout.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("compare_{}", &digest[..32])
}

/// [`StoreError::RevisionNotFound`] を、A/B のどちら側で起きたか分かる形の
/// 構造化エラーにする(`compare_revisions` 用)。
fn revision_not_found_side(revision_id: &str, side: &str) -> CallToolResult {
    tool_error(
        "revision_not_found",
        format!("revision {revision_id:?} (side {side}) does not exist in this workspace"),
        serde_json::json!({
            "revision_id": revision_id,
            "side": side,
            "recovery": "call list_assets to see the available revision_ids, or import_asset first",
        }),
    )
}

/// 画像を「長辺 <= max_edge」に収まるよう縦横比を保ったまま縮小する(contain)。
/// 既に収まっている場合は拡大しない。
fn scale_contain(img: image::DynamicImage, max_edge: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let long_edge = w.max(h);
    if long_edge == 0 || long_edge <= max_edge {
        return img;
    }
    let scale = max_edge as f64 / long_edge as f64;
    let new_w = ((w as f64 * scale).round() as u32).max(1);
    let new_h = ((h as f64 * scale).round() as u32).max(1);
    img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3)
}

/// `src` を `canvas` の (x0, y0) へそのまま貼り付ける(アルファ合成なし、完全上書き)。
fn paste(canvas: &mut RgbImage, src: &RgbImage, x0: u32, y0: u32) {
    for (x, y, p) in src.enumerate_pixels() {
        canvas.put_pixel(x0 + x, y0 + y, *p);
    }
}

/// `compare_revisions` の合成背景色(中立グレー)。どちらの画像とも喧嘩しにくい明るさ。
const COMPARE_BG: image::Rgb<u8> = image::Rgb([222, 222, 222]);

/// A・B を水平に並べる(A が左)。短い方は縦方向中央揃え。
fn compose_side_by_side(a: &RgbImage, b: &RgbImage, gap: u32) -> RgbImage {
    let width = a.width() + gap + b.width();
    let height = a.height().max(b.height());
    let mut canvas = RgbImage::from_pixel(width, height, COMPARE_BG);
    let ay = (height - a.height()) / 2;
    let by = (height - b.height()) / 2;
    paste(&mut canvas, a, 0, ay);
    paste(&mut canvas, b, a.width() + gap, by);
    canvas
}

/// A・B を垂直に並べる(A が上)。短い方は水平方向中央揃え。
fn compose_stacked(a: &RgbImage, b: &RgbImage, gap: u32) -> RgbImage {
    let width = a.width().max(b.width());
    let height = a.height() + gap + b.height();
    let mut canvas = RgbImage::from_pixel(width, height, COMPARE_BG);
    let ax = (width - a.width()) / 2;
    let bx = (width - b.width()) / 2;
    paste(&mut canvas, a, ax, 0);
    paste(&mut canvas, b, bx, a.height() + gap);
    canvas
}

/// RGB 画像を jpeg quality 80 でエンコードする(`image` クレート内蔵エンコーダ)。
fn encode_jpeg_q80(img: &RgbImage) -> Result<Vec<u8>, String> {
    let (w, h) = img.dimensions();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, PREVIEW_JPEG_QUALITY)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

/// guide overlay の描画色: 固定の高視認性色 #FF3355 を ~60% 不透明度でブレンドする。
/// per-pixel でコントラストに応じて白/黒を切り替える方式も検討したが、
/// 実装がシンプルで明暗どちらの背景でも視認できる固定色を採用した。
const OVERLAY_COLOR: [u8; 3] = [0xFF, 0x33, 0x55];
const OVERLAY_ALPHA: f32 = 0.6;

/// jpeg バイト列をデコードし、overlay の格子/三分割/水平線を描いて jpeg quality 80 で
/// 再エンコードする。`overlay` は事前に [`OVERLAY_VALUES`] に含まれることを検証しておくこと。
fn draw_overlay_jpeg(jpeg_bytes: &[u8], overlay: &str) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(jpeg_bytes)
        .map_err(|e| format!("failed to decode preview jpeg: {e}"))?;
    let mut rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();

    let (h_lines, v_lines): (Vec<u32>, Vec<u32>) = match overlay {
        "grid" => (
            (1..8u32).map(|i| h.saturating_mul(i) / 8).collect(),
            (1..8u32).map(|i| w.saturating_mul(i) / 8).collect(),
        ),
        "thirds" => (vec![h / 3, (h * 2) / 3], vec![w / 3, (w * 2) / 3]),
        "horizon" => (
            (1..12u32).map(|i| h.saturating_mul(i) / 12).collect(),
            Vec::new(),
        ),
        other => return Err(format!("unknown overlay {other:?}")),
    };

    let blend_pixel = |p: &mut image::Rgb<u8>| {
        for (channel, &dst) in p.0.iter_mut().zip(OVERLAY_COLOR.iter()) {
            let src = *channel as f32;
            *channel = (src * (1.0 - OVERLAY_ALPHA) + dst as f32 * OVERLAY_ALPHA).round() as u8;
        }
    };
    for y in h_lines {
        if y < h {
            for x in 0..w {
                blend_pixel(rgb.get_pixel_mut(x, y));
            }
        }
    }
    for x in v_lines {
        if x < w {
            for y in 0..h {
                blend_pixel(rgb.get_pixel_mut(x, y));
            }
        }
    }

    encode_jpeg_q80(&rgb)
}

/// プレビュー jpeg にマスクの被覆を焼き込む(`overlay: "mask"`)。
///
/// マスクの重みは `atx_core::recipe::MaskRef` と同じ規約
/// (sRGB 符号値上の BT.709 輝度、白 = 1.0)で読む。寸法が違えばプレビュー寸法へ
/// 双線形で合わせる(マスクは参照画像と同寸法だが、プレビューは縮小済みのため)。
///
/// 塗り分けは2値: 重み > [`MASK_OVERLAY_THRESHOLD`] を赤 60% でブレンドし、
/// それ以外は 0.75 倍に落とす。連続階調でなく2値にするのは
/// 「どこに効くか」を一目で掴ませるのが目的だから(強度の確認は apply 後の比較で行う)。
fn draw_mask_overlay_jpeg(jpeg_bytes: &[u8], mask_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let preview = image::load_from_memory(jpeg_bytes)
        .map_err(|e| format!("failed to decode preview jpeg: {e}"))?;
    let mut rgb = preview.to_rgb8();
    let (w, h) = rgb.dimensions();

    let mask = image::load_from_memory(mask_bytes)
        .map_err(|e| format!("failed to decode the mask image: {e}"))?
        .to_luma8();
    let mask = if mask.dimensions() == (w, h) {
        mask
    } else {
        image::imageops::resize(&mask, w, h, image::imageops::FilterType::Triangle)
    };

    let threshold = (MASK_OVERLAY_THRESHOLD * 255.0).round() as u8;
    for y in 0..h {
        for x in 0..w {
            let covered = mask.get_pixel(x, y).0[0] > threshold;
            let pixel = rgb.get_pixel_mut(x, y);
            for (channel, &tint) in pixel.0.iter_mut().zip(MASK_OVERLAY_COLOR.iter()) {
                let src = *channel as f32;
                *channel = if covered {
                    (src * (1.0 - MASK_OVERLAY_ALPHA) + tint as f32 * MASK_OVERLAY_ALPHA).round()
                        as u8
                } else {
                    (src * MASK_OVERLAY_DIM).round() as u8
                };
            }
        }
    }

    encode_jpeg_q80(&rgb)
}

/// 相対パスを cwd 基準の絶対パスにし、`.` / `..` を字句的に畳む。
/// 書き出し先はまだ存在しないことがあるので `canonicalize` は使えない。
fn absolutize(path: &str) -> std::io::Result<PathBuf> {
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// EXIF Orientation(1-8)を画素に焼き込む。atx-core のデコード時正規化と同じ規約。
fn apply_orientation(image: image::DynamicImage, orientation: u16) -> image::DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_recipe_drops_user_encode_and_appends_downscale() {
        let recipe = TransformRecipe {
            operations: vec![
                Operation::AutoOrient,
                Operation::Encode {
                    format: OutputFormat::Webp,
                    quality: Some(90),
                    bit_depth: None,
                },
            ],
        };
        let preview = preview_recipe_of(&recipe);
        assert_eq!(preview.operations.len(), 3);
        assert!(matches!(
            preview.operations[2],
            Operation::Encode {
                format: OutputFormat::Jpeg,
                quality: Some(PREVIEW_JPEG_QUALITY),
                ..
            }
        ));
        assert!(atx_core::recipe::validate(&preview).is_ok());
    }

    #[test]
    fn preview_key_is_deterministic_and_path_safe() {
        let a = preview_key("rev_1", "abc", None, None);
        assert_eq!(a, preview_key("rev_1", "abc", None, None));
        assert_ne!(a, preview_key("rev_1", "abd", None, None));
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn preview_key_differs_by_overlay() {
        let base = preview_key("rev_1", "abc", None, None);
        let grid = preview_key("rev_1", "abc", Some("grid"), None);
        let thirds = preview_key("rev_1", "abc", Some("thirds"), None);
        assert_ne!(base, grid);
        assert_ne!(base, thirds);
        assert_ne!(grid, thirds);
    }

    /// 同じ (revision, recipe, overlay="mask") でもマスクが違えば別キーになること。
    #[test]
    fn preview_key_differs_by_mask_revision() {
        let m1 = preview_key("rev_1", "abc", Some("mask"), Some("rev_m1"));
        let m2 = preview_key("rev_1", "abc", Some("mask"), Some("rev_m2"));
        let none = preview_key("rev_1", "abc", Some("mask"), None);
        assert_ne!(m1, m2);
        assert_ne!(m1, none);
    }

    #[test]
    fn cube_detection_uses_the_extension_or_the_lut_size_header() {
        let image_path = Path::new("/tmp/photo.jpg");
        let cube_path = Path::new("/tmp/look.CUBE");

        // 1. 拡張子だけで判定する(中身がまだ読めなくても LUT 扱い)。
        assert!(looks_like_cube(cube_path, b"whatever"));

        // 2. 拡張子が違っても、コメント・空行を飛ばしたヘッダに LUT_*_SIZE があれば LUT。
        let body = b"# a comment\n\nTITLE \"x\"\nLUT_3D_SIZE 2\n0 0 0\n";
        assert!(looks_like_cube(image_path, body));
        assert!(looks_like_cube(image_path, b"lut_1d_size 16\n"));

        // 3. ただの画像・ただのテキストは LUT ではない。
        assert!(!looks_like_cube(image_path, &[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!looks_like_cube(image_path, b"hello world\n"));
        assert!(!looks_like_cube(image_path, b""));
    }

    #[test]
    fn absolutize_folds_dot_segments() {
        let p = absolutize("/tmp/a/./b/../c").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/a/c"));
    }
}

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
/// SVG ベクタアセットの MIME type(v0.8「`svg_overlay`」)。
///
/// `image/` で始まるが**ラスタ画像ではない**。`inspect_image` / `generate_mask` /
/// マスクオーバレイのようにデコードを前提とするツールは、この MIME を
/// [`not_an_image`] で弾く([`is_raster_image`] が唯一の判定点)。
/// atx-store の `ext_for_mime` がこれを `.svg` 拡張子に写す。
pub const SVG_MIME: &str = "image/svg+xml";
/// SVG 取り込みのサイズ上限。ロゴ・ウォーターマークの SVG が 16MiB を超えることは
/// 実務上ありえないので、テキストとしての暴走を弾く実務的なサニティ上限。
pub const MAX_SVG_BYTES: u64 = 16 * 1024 * 1024;
/// SVG 判定で内容を覗くバイト数の上限(先頭 4KiB)。
const SVG_SNIFF_BYTES: usize = 4 * 1024;

// ---------------------------------------------------------------------------
// 入力パラメータ(tool inputSchema はこれらから生成される)
// ---------------------------------------------------------------------------

/// バッチ(`paths` / `revision_ids`)の1回あたりの上限。
///
/// 実運用 FB(28 枚を 1 枚ずつ import → apply で 56 往復)を 2 往復に畳むための
/// 上限であり、1回の呼び出しが返すテキスト・structuredContent が
/// ホスト側のコンテキストを食い潰さない実務的な境界でもある。
pub const MAX_BATCH: usize = 64;

/// `import_asset` の引数。`path`(単一)と `paths`(バッチ)のどちらか一方。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ImportAssetParams {
    /// 取り込むローカルファイルの絶対パス(または cwd からの相対パス)。
    /// `paths` とは排他で、どちらか一方が必須。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 複数ファイルを1回で取り込む(1..=64 件)。`path` とは排他。
    /// 1件が失敗してもバッチは中断せず、`failed` に理由が入る。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl ImportAssetParams {
    /// 単一パスの引数を作る(テスト・呼び出し側の糖衣)。
    pub fn single(path: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
            paths: None,
        }
    }

    /// バッチ引数を作る。
    pub fn batch<S: Into<String>>(paths: impl IntoIterator<Item = S>) -> Self {
        Self {
            path: None,
            paths: Some(paths.into_iter().map(Into::into).collect()),
        }
    }
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
    /// 探索範囲全体のスコア曲線(最大 300 点)を結果に含めるか。既定 false。
    /// ピークの鋭さ・多峰性を自分で読みたいときだけ true にする(既定では省かれる)。
    #[serde(default)]
    pub include_score_curve: bool,
}

/// レシピ引数の**不透明な JSON オブジェクト**。
///
/// ROADMAP §Agent UX の規律 #2「語彙の段階的開示」の徹底:
/// `TransformRecipe` の JsonSchema をそのまま inputSchema に埋めると
/// 27 op の Operation enum + Layer + `$defs` が **apply_transform と
/// render_preview の両方に**展開され、tools/list が接続ごとに重くなる。
/// スキーマ上は `type: "object"` の1行だけを晒し、実際の検証は
/// [`deserialize_recipe`] が実行時に行って(op index 付きの)構造化エラーで返す。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct RecipeJson(pub serde_json::Value);

impl From<TransformRecipe> for RecipeJson {
    fn from(recipe: TransformRecipe) -> Self {
        RecipeJson(serde_json::to_value(recipe).expect("a recipe always serializes"))
    }
}

impl JsonSchema for RecipeJson {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RecipeJson".into()
    }

    // $defs へ切り出さずその場に展開する(1行のスキーマなので参照する意味がない)。
    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "Transform recipe: {\"operations\": [{\"op\": \"...\", ...}, ...]} applied in order, with at most one \"encode\" which must be last (an optional \"layers\" array may precede it). The operation vocabulary is deliberately not inlined here: call list_operations for the catalog and explain_operation for one operation's full schema. An invalid recipe comes back as a structured error naming the offending operation index and field.",
        })
    }
}

/// `apply_transform` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TransformParams {
    /// 入力 revision ID("rev_...")。`revision_ids` とは排他で、どちらか一方が必須。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// 同じレシピを複数 revision に適用する(1..=64 件)。`revision_id` とは排他。
    /// 1件が失敗してもバッチは中断せず、その要素に error が入る。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_ids: Option<Vec<String>>,
    /// 変換レシピ。`{"operations": [...]}`。`preset` とはどちらか一方のみ指定する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<RecipeJson>,
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
    pub recipe: Option<RecipeJson>,
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
///
/// `"diff"`(v0.7)だけは他の2つと質が違う: 2枚を並べるのではなく、
/// 同寸法の A/B から画素単位の差分ヒートマップを1枚合成する
/// ([`AtxTools::compare_revisions`] の diff 分岐を参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompareLayout {
    #[default]
    SideBySide,
    Stacked,
    Diff,
}

impl CompareLayout {
    fn as_str(self) -> &'static str {
        match self {
            CompareLayout::SideBySide => "side_by_side",
            CompareLayout::Stacked => "stacked",
            CompareLayout::Diff => "diff",
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
    /// `"side_by_side"`(既定、水平に並べる)| `"stacked"`(垂直に並べる)|
    /// `"diff"`(並べる代わりに1枚の差分ヒートマップを作る。A/B の寸法が完全一致している必要がある)。
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

/// 取り込んだバイト列が「既に別レシピの出力として台帳に居る」ことの記録(二重適用検出)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AlreadyDerivedFrom {
    /// 同じ sha256 を持つ**派生** revision の ID。
    pub revision_id: String,
    /// その派生を生んだレシピのハッシュ。
    pub recipe_hash: Option<String>,
    /// その派生の入力 revision(= 元画像)。
    pub source_revision_id: String,
}

/// `import_asset`(単一パス)の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportOutput {
    pub revision: RevisionSummary,
    /// 同一内容が既に取り込まれていて既存 revision を返した場合 true(冪等ヒット)。
    pub reused: bool,
    /// 取り込み元の正規化済みパス。
    pub source_path: String,
    /// 注意喚起(現状は二重適用検出のみ)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// 取り込んだバイト列が既にこのワークスペースの**派生** revision と同一だった場合の
    /// その派生の素性(= 同じレシピをもう一度当てると二重処理になる、というサイン)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub already_derived_from: Option<AlreadyDerivedFrom>,
}

/// バッチの1件分の失敗(パスと構造化エラーの要点)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchFailure {
    /// 入力で与えられたパス(または revision ID)。
    pub path: String,
    pub error: BatchError,
}

/// バッチ要素の失敗理由。単一呼び出しが返す構造化エラーと同じ code / message。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchError {
    pub code: String,
    pub message: String,
}

/// `import_asset`(バッチ)の1件分。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportEntry {
    /// 入力で与えられたパス(正規化前。入力順を保つ)。
    pub path: String,
    #[serde(flatten)]
    pub result: ImportOutput,
}

/// `import_asset`(バッチ)の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportBatchOutput {
    /// 取り込みに成功した件数(= `imported` の長さ)。
    pub count: usize,
    /// 入力順の成功結果。
    pub imported: Vec<ImportEntry>,
    /// 失敗したファイル(1件の失敗でバッチは中断しない)。
    pub failed: Vec<BatchFailure>,
}

/// `import_asset` の structuredContent(単一 = 従来どおり / バッチ)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ImportResult {
    Single(Box<ImportOutput>),
    Batch(Box<ImportBatchOutput>),
}

/// `inspect_image` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InspectOutput {
    pub revision_id: String,
    pub path: String,
    pub info: ImageInfo,
}

/// `detect_tilt` の検出結果のビュー。
///
/// `include_score_curve: false`(既定)のときは `score_curve`(最大 300 点 ≒ 数 KB)を
/// **丸ごと落として**返す。実運用 FB: 曲線は要らないのに毎回返ってきてコンテキストを
/// 食っていた。必要なときだけフラグで取り寄せる(段階的開示)。
#[derive(Debug, Clone)]
pub struct DetectionView {
    pub detection: TiltDetection,
    pub include_score_curve: bool,
}

impl Serialize for DetectionView {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.include_score_curve {
            return self.detection.serialize(serializer);
        }
        let mut value = serde_json::to_value(&self.detection).map_err(serde::ser::Error::custom)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("score_curve");
        }
        value.serialize(serializer)
    }
}

/// `detect_tilt` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DetectTiltOutput {
    pub revision_id: String,
    /// 検出結果。`score_curve` は `include_score_curve: true` のときだけ載る。
    #[schemars(with = "TiltDetection")]
    pub detection: DetectionView,
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

/// `apply_transform`(バッチ)の1件分。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyEntry {
    /// 入力 revision ID(入力順を保つ)。
    pub revision_id: String,
    /// 成功時の出力 revision。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<RevisionSummary>,
    /// 成功時のみ。既存派生を再利用した(再変換しなかった)場合 true。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused: Option<bool>,
    /// 失敗時のみ。単一呼び出しと同じ構造化エラーの要点。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchError>,
}

/// `apply_transform`(バッチ)の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplyBatchOutput {
    /// 入力件数(= `results` の長さ。成功・失敗の両方を含む)。
    pub count: usize,
    /// 成功した件数。
    pub succeeded: usize,
    /// 入力順の結果。
    pub results: Vec<ApplyEntry>,
    pub recipe_hash: String,
    pub engine_version: String,
    /// 全 revision 分の警告を `"<revision_id>: <warning>"` の形で集約したもの。
    pub warnings: Vec<String>,
}

/// `apply_transform` の structuredContent(単一 = 従来どおり / バッチ)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ApplyResult {
    Single(Box<ApplyTransformOutput>),
    Batch(Box<ApplyBatchOutput>),
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
    /// `layout: "diff"` のときだけ載る: 全チャンネル・全画素平均の絶対差(0..255 スケール)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_abs_diff: Option<f64>,
    /// `layout: "diff"` のときだけ載る: 画素ごとのチャンネル最大絶対差 d の、全画素中の最大値。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_abs_diff: Option<u8>,
    /// `layout: "diff"` のときだけ載る: d > 2 の画素が全体に占める割合(0.0..=1.0)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_pixel_ratio: Option<f64>,
}

/// `list_assets` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListAssetsOutput {
    pub count: usize,
    pub revisions: Vec<RevisionSummary>,
}

/// `list_operations` のカタログ1行(**機械可読の最小形**)。
///
/// 要約・パラメータ表記はテキストサマリ側にだけ載せる: structuredContent と
/// テキストの両方に同じ散文を積むと 1 回の呼び出しで ~4.7k tokens を焼いてしまうため
/// (ROADMAP §Agent UX の規律 #2「語彙の段階的開示」= カタログの予算を守る)。
/// 完全な仕様が要るときは `explain_operation` を呼ぶ。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OperationCatalogEntry {
    pub name: String,
    pub category: String,
}

/// `list_operations` の structuredContent(compact machine form)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListOperationsOutput {
    pub count: usize,
    /// op の名前と分類だけ。要約・パラメータはテキスト側 / `explain_operation` 側。
    pub ops: Vec<OperationCatalogEntry>,
    /// ビルトインプリセット名(`apply_transform` / `render_preview` の `preset` に渡せる)。
    /// 説明はテキストサマリ側にだけ載せる。
    pub presets: Vec<String>,
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

/// `explain_operation` の structuredContent(op を説明した場合)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExplainOperationOutput {
    /// 常に `"operation"`。プリセットを説明した場合は `"preset"` になる
    /// ([`ExplainPresetOutput`])。
    pub kind: String,
    pub name: String,
    pub category: String,
    pub summary: String,
    pub params: Vec<ExplainParamEntry>,
    /// そのまま `operations` に入れられる JSON 断片(文字列)。
    pub examples: Vec<String>,
    pub warnings: Vec<String>,
}

/// `explain_operation` の structuredContent(プリセットを説明した場合)。
///
/// 実運用 FB: プリセットは名前と1行説明しか見えないので「中身が分からないから使わない」
/// と敬遠された。中身(op 配列)をそのまま見せて、生 DSL との往復を可能にする
/// (ROADMAP §Agent UX の規律 #3「プリセット = 語彙の圧縮」= 2層言語の下層が見えること)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExplainPresetOutput {
    /// 常に `"preset"`。
    pub kind: String,
    pub name: String,
    pub description: String,
    /// プリセットのレシピの `operations` をそのまま JSON で。
    /// コピーして編集すれば生レシピとして使える。
    pub ops: Vec<serde_json::Value>,
    /// プリセットが `layers` を持つ場合のみ。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<serde_json::Value>,
}

/// `explain_operation` の structuredContent(op / プリセット)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ExplainResult {
    Operation(Box<ExplainOperationOutput>),
    Preset(Box<ExplainPresetOutput>),
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

/// 構造化エラー結果([`tool_error`] が作ったもの)から code / message を取り出す。
///
/// バッチ処理では1件の失敗でバッチを止めず、**単一呼び出しとまったく同じ**
/// 構造化エラーの要点をその要素に記録する。エラーの作り方を二重化しないために、
/// 生成済みの `CallToolResult` から読み戻す。
fn error_info(result: &CallToolResult) -> BatchError {
    let parsed = result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find_map(|t| serde_json::from_str::<serde_json::Value>(&t.text).ok());
    match parsed {
        Some(value) => BatchError {
            code: value["error"]["code"]
                .as_str()
                .unwrap_or("unknown_error")
                .to_string(),
            message: value["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
        None => BatchError {
            code: "unknown_error".to_string(),
            message: result
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.clone())
                .unwrap_or_default(),
        },
    }
}

/// 不透明な JSON([`RecipeJson`])を [`TransformRecipe`] に落とす。
///
/// inputSchema から Operation enum を外した代償として、**コンパイル時スキーマ検証と
/// 同じだけ具体的なエラー**を実行時に作るのがこの関数の仕事:
/// まず素直に deserialize し、失敗したら `operations[i]` / `layers[j].operations[k]` を
/// 1件ずつ試して**壊れている位置**を特定し、op 名の打ち間違いには
/// `did_you_mean` を添える。
fn deserialize_recipe(recipe: &RecipeJson) -> Result<TransformRecipe, CallToolResult> {
    let value = &recipe.0;
    match serde_json::from_value::<TransformRecipe>(value.clone()) {
        Ok(recipe) => Ok(recipe),
        Err(err) => {
            let (location, message, did_you_mean) = locate_recipe_error(value, &err);
            Err(tool_error(
                "invalid_recipe",
                format!("invalid recipe at {location}: {message}"),
                serde_json::json!({
                    "location": location,
                    "reason": message,
                    "did_you_mean": did_you_mean,
                    "recovery": "fix that field and call the tool again; call list_operations for the catalog of operation names, or explain_operation {\"operation\":\"<name>\"} for one operation's full parameter table",
                }),
            ))
        }
    }
}

/// serde のエラーを、レシピ内の位置(`operations[2]` 等)まで絞り込む。
///
/// 返すのは `(location, message, did_you_mean)`。位置が特定できないときは
/// `"recipe"` に serde の原文をそのまま添える(情報を失わないため)。
fn locate_recipe_error(
    value: &serde_json::Value,
    err: &serde_json::Error,
) -> (String, String, Vec<&'static str>) {
    let Some(object) = value.as_object() else {
        return (
            "recipe".to_string(),
            format!(
                "a recipe must be a JSON object like {{\"operations\": [...]}} , got {}",
                json_type_name(value)
            ),
            Vec::new(),
        );
    };

    // layers[j].operations[k] → operations[i] の順に、壊れている1件を名指しする。
    if let Some(layers) = object.get("layers").and_then(|l| l.as_array()) {
        for (j, layer) in layers.iter().enumerate() {
            if let Some(ops) = layer.get("operations").and_then(|o| o.as_array()) {
                if let Some((k, e)) = first_bad_operation(ops) {
                    let name = op_name_of(&ops[k]);
                    return (
                        format!("layers[{j}].operations[{k}]{}", field_suffix(&ops[k])),
                        e,
                        suggestions_for(name),
                    );
                }
            }
        }
    }
    match object.get("operations") {
        None => (
            "recipe".to_string(),
            "a recipe needs an \"operations\" array (it may be empty only together with \"layers\")"
                .to_string(),
            Vec::new(),
        ),
        Some(serde_json::Value::Array(ops)) => match first_bad_operation(ops) {
            Some((i, e)) => (
                format!("operations[{i}]{}", field_suffix(&ops[i])),
                e,
                suggestions_for(op_name_of(&ops[i])),
            ),
            None => ("recipe".to_string(), err.to_string(), Vec::new()),
        },
        Some(other) => (
            "recipe.operations".to_string(),
            format!("\"operations\" must be an array, got {}", json_type_name(other)),
            Vec::new(),
        ),
    }
}

/// 配列の中で最初に `Operation` として読めない要素の (index, serde メッセージ)。
fn first_bad_operation(ops: &[serde_json::Value]) -> Option<(usize, String)> {
    ops.iter().enumerate().find_map(|(i, op)| {
        serde_json::from_value::<Operation>(op.clone())
            .err()
            .map(|e| (i, e.to_string()))
    })
}

/// 壊れている op の中で、どのフィールドが原因かを `".field"` の形で返す(特定できなければ空)。
///
/// serde の internally-tagged enum のエラーは `invalid type: string ..., expected f64` のように
/// **フィールド名を落とす**。1つずつフィールドを抜いて deserialize し直すと、
/// 「抜いたら通った」= そのフィールドが原因、「抜いたら missing field になった」=
/// 必須フィールドの型違い、と切り分けられる(op は高々数フィールドなので安い)。
fn field_suffix(op: &serde_json::Value) -> String {
    let Some(object) = op.as_object() else {
        return String::new();
    };
    for key in object.keys().filter(|k| k.as_str() != "op") {
        let mut probe = object.clone();
        probe.remove(key);
        match serde_json::from_value::<Operation>(serde_json::Value::Object(probe)) {
            Ok(_) => return format!(".{key}"),
            Err(e) if e.to_string().contains(&format!("missing field `{key}`")) => {
                return format!(".{key}")
            }
            Err(_) => {}
        }
    }
    String::new()
}

/// `{"op": "..."}` の名前(無ければ None)。
fn op_name_of(op: &serde_json::Value) -> Option<&str> {
    op.get("op").and_then(|v| v.as_str())
}

/// 打ち間違えた op 名への「もしかして」。
fn suggestions_for(name: Option<&str>) -> Vec<&'static str> {
    match name {
        Some(name) if crate::vocab::find(name).is_none() => crate::vocab::did_you_mean(name),
        _ => Vec::new(),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
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
    not_an_image_side(revision_id, mime_type, None)
}

/// [`not_an_image`] の side 付き版(`compare_revisions` の A/B のように、
/// どちらの引数が悪いのかを名指しできる呼び出し元のため)。
fn not_an_image_side(revision_id: &str, mime_type: &str, side: Option<&str>) -> CallToolResult {
    let hint = if mime_type == CUBE_MIME {
        "this is an imported .cube 3D LUT asset, not an image; reference it from a recipe as {\"op\": \"lut\", \"lut_revision_id\": \"...\"} instead of inspecting it"
    } else if mime_type == SVG_MIME {
        "this is an imported SVG, a VECTOR asset with no pixels of its own, not a raster image; stamp it onto a raster revision with {\"op\": \"svg_overlay\", \"svg_revision_id\": \"...\", \"x\": 0, \"y\": 0} instead of inspecting it"
    } else {
        "call list_assets and pick a revision whose mime_type starts with \"image/\""
    };
    let where_ = match side {
        Some(side) => format!(" (revision_id_{side})"),
        None => String::new(),
    };
    tool_error(
        "not_an_image",
        format!(
            "revision {revision_id:?}{where_} has mime_type {mime_type:?} and is not an image, so it cannot be inspected"
        ),
        serde_json::json!({
            "revision_id": revision_id,
            "mime_type": mime_type,
            "side": side,
            "recovery": hint,
        }),
    )
}

/// revision が**デコードできるラスタ画像**かどうか。
///
/// `image/` で始まるだけでは足りない: v0.8 の SVG(`image/svg+xml`)は
/// `image/` 名前空間に居ながらベクタなので、デコードを前提とするツール
/// (`inspect_image` / `generate_mask` / マスクオーバレイ)からは弾く必要がある。
/// 判定を 1 箇所に閉じ込めておけば、将来ベクタ系アセットが増えてもここだけ直せばよい。
fn is_raster_image(mime_type: &str) -> bool {
    mime_type.starts_with("image/") && mime_type != SVG_MIME
}

/// ファイルが SVG ベクタアセットかどうかを判定する。
///
/// 判定規則(どちらか一方を満たせば SVG とみなす):
/// 1. 拡張子が `.svg`(大文字小文字を無視)
/// 2. 先頭 4KiB(UTF-8 として読めた範囲)に `<svg` が現れる
///    — XML 宣言・DOCTYPE・コメントが前置されていても拾える素朴な sniff
///
/// **画像のマジックバイト判定より先に**呼ぶ。SVG はテキストなので
/// `inspect_bytes` に渡すと「未知フォーマット」エラーになる(.cube と同じ理由)。
fn looks_like_svg(path: &Path, bytes: &[u8]) -> bool {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
    {
        return true;
    }
    let head = &bytes[..bytes.len().min(SVG_SNIFF_BYTES)];
    String::from_utf8_lossy(head).contains("<svg")
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

    /// ローカルパス(1件 or バッチ)からワークスペースへ取り込む。
    ///
    /// `path` と `paths` は排他でどちらか一方が必須。`paths` のときは
    /// **1件の失敗でバッチを止めない**(失敗は `failed` に積み、1件でも成功すれば
    /// ツールとしては成功)。実運用 FB: 28 枚を 1 枚ずつ import すると
    /// それだけで 28 往復かかっていた。
    pub fn import_asset(&self, params: &ImportAssetParams) -> CallToolResult {
        match (params.path.as_deref(), params.paths.as_deref()) {
            (Some(path), None) => match self.import_one(path) {
                Ok((output, text)) => ok_result(text, &ImportResult::Single(Box::new(output))),
                Err(result) => result,
            },
            (None, Some(paths)) => self.import_batch(paths),
            (Some(_), Some(_)) => tool_error(
                "path_and_paths_conflict",
                "path and paths are mutually exclusive, but both were given",
                serde_json::json!({
                    "recovery": "pass path for a single file, or paths for a batch of up to 64 files, not both",
                }),
            ),
            (None, None) => tool_error(
                "path_or_paths_required",
                "one of path (single file) or paths (batch of up to 64 files) is required",
                serde_json::json!({
                    "recovery": "pass path = \"/abs/file.jpg\", or paths = [\"/abs/a.jpg\", \"/abs/b.jpg\"]",
                }),
            ),
        }
    }

    /// バッチ取り込み。入力順を保ち、失敗しても続行する。
    fn import_batch(&self, paths: &[String]) -> CallToolResult {
        if let Err(result) = check_batch_size(paths.len(), "paths") {
            return result;
        }

        let mut imported: Vec<ImportEntry> = Vec::new();
        let mut failed: Vec<BatchFailure> = Vec::new();
        for path in paths {
            match self.import_one(path) {
                Ok((output, _)) => imported.push(ImportEntry {
                    path: path.clone(),
                    result: output,
                }),
                Err(result) => failed.push(BatchFailure {
                    path: path.clone(),
                    error: error_info(&result),
                }),
            }
        }

        if imported.is_empty() {
            let listed: Vec<String> = failed
                .iter()
                .map(|f| format!("- {}: [{}] {}", f.path, f.error.code, f.error.message))
                .collect();
            return tool_error(
                "import_failed",
                format!(
                    "all {} imports failed:\n{}",
                    failed.len(),
                    listed.join("\n")
                ),
                serde_json::json!({
                    "failed": failed,
                    "recovery": "fix the paths above (they must be existing local files) and call import_asset again",
                }),
            );
        }

        let reused = imported.iter().filter(|e| e.result.reused).count();
        let double_applied = imported
            .iter()
            .filter(|e| e.result.already_derived_from.is_some())
            .count();
        let mut text = format!(
            "Imported {} of {} file(s){}{}{}",
            imported.len(),
            imported.len() + failed.len(),
            if reused > 0 {
                format!(" ({reused} already in the workspace, reused)")
            } else {
                String::new()
            },
            if failed.is_empty() {
                String::new()
            } else {
                format!(", {} failed", failed.len())
            },
            if double_applied > 0 {
                format!(
                    "; {double_applied} of them are already the output of a recipe (see already_derived_from)"
                )
            } else {
                String::new()
            },
        );
        for entry in &imported {
            text.push_str(&format!(
                "\n- {} -> {} ({}x{} {}, {} bytes){}",
                entry.path,
                entry.result.revision.revision_id,
                entry.result.revision.width,
                entry.result.revision.height,
                entry.result.revision.mime_type,
                entry.result.revision.byte_size,
                if entry.result.reused { " [reused]" } else { "" },
            ));
            for warning in &entry.result.warnings {
                text.push_str(&format!("\n  warning: {warning}"));
            }
        }
        if !failed.is_empty() {
            text.push_str("\nfailed:");
            for failure in &failed {
                text.push_str(&format!(
                    "\n- {}: [{}] {}",
                    failure.path, failure.error.code, failure.error.message
                ));
            }
        }

        ok_result(
            text,
            &ImportResult::Batch(Box::new(ImportBatchOutput {
                count: imported.len(),
                imported,
                failed,
            })),
        )
    }

    /// 1ファイルを取り込む。成功なら `(structuredContent, テキストサマリ)`、
    /// 失敗なら単一呼び出しがそのまま返せる構造化エラー。
    fn import_one(&self, raw_path: &str) -> Result<(ImportOutput, String), CallToolResult> {
        macro_rules! bail {
            ($result:expr) => {
                return Err($result)
            };
        }
        macro_rules! tri {
            ($expr:expr, $map:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(e) => return Err($map(e)),
                }
            };
        }

        let raw = PathBuf::from(raw_path);
        let path = match raw.canonicalize() {
            Ok(p) => p,
            Err(e) => bail!(tool_error(
                "path_not_found",
                format!("cannot access {raw_path:?}: {e}"),
                serde_json::json!({
                    "path": raw_path,
                    "recovery": "pass an absolute path to an existing local image file",
                }),
            )),
        };
        if !path.is_file() {
            bail!(tool_error(
                "not_a_file",
                format!("{} is not a regular file", path.display()),
                serde_json::json!({ "path": path.to_string_lossy(), "recovery": "pass a path to a file, not a directory" }),
            ));
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
            bail!(tool_error(
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
            ));
        }
        // ベクタアセット(v0.8: レシピから参照される SVG)も画像としては検査しない。
        // 寸法は SVG の**固有サイズ**を記録し、持たない SVG は 0x0 のままにする
        // (0x0 は「この SVG は自分では大きさを決められない」の記録でもあり、
        //  svg_overlay で width/height を書けというサインになる)。
        let is_svg = !is_cube && looks_like_svg(&path, &bytes);
        if is_svg && bytes.len() as u64 > MAX_SVG_BYTES {
            bail!(tool_error(
                "limit_exceeded",
                format!(
                    "{} looks like an SVG but is {} bytes, over the {MAX_SVG_BYTES} byte limit for SVG assets",
                    path.display(),
                    bytes.len()
                ),
                serde_json::json!({
                    "path": path.to_string_lossy(),
                    "byte_size": bytes.len(),
                    "max_bytes": MAX_SVG_BYTES,
                    "recovery": "an SVG this large is almost certainly not a logo/watermark; check the file, or simplify the artwork",
                }),
            ));
        }
        let (mime_type, width, height) = if is_cube {
            (CUBE_MIME.to_string(), 0, 0)
        } else if is_svg {
            let (w, h) = atx_core::svg_intrinsic_size(&bytes).unwrap_or((0, 0));
            (SVG_MIME.to_string(), w, h)
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
        if is_svg {
            origin.insert("asset_kind".to_string(), "svg".to_string());
        }

        let known_before = tri!(self.known_revision_ids(), store_error);
        let revision = tri!(
            self.store
                .import_bytes(&bytes, &mime_type, width, height, origin),
            store_error
        );
        let reused = known_before.contains(&revision.revision_id);
        let summary = RevisionSummary::new(&self.store, &revision);

        // --- 二重適用検出(実運用 FB) ---
        // 取り込んだバイト列と同じ sha256 の**派生** revision が台帳に居るなら、
        // このファイルは既にどこかで export されたレシピの出力である。
        // 同じレシピをもう一度当てると二重処理になるので、警告として名指しする。
        let already_derived_from = tri!(self.find_derived_with_sha256(&revision), store_error);
        let mut warnings = Vec::new();
        if let Some(origin) = &already_derived_from {
            warnings.push(format!(
                "these bytes are already the output of recipe {} applied to {} (revision {}) - applying the same recipe again would double-process them",
                short_hash(origin.recipe_hash.as_deref()),
                origin.source_revision_id,
                origin.revision_id,
            ));
        }

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
        } else if is_svg {
            let size = if summary.width == 0 || summary.height == 0 {
                "no intrinsic size (no viewBox / absolute width+height): pass both width and \
                 height on svg_overlay"
                    .to_string()
            } else {
                format!("intrinsic size {}x{}", summary.width, summary.height)
            };
            format!(
                "{verb} {} as {} (SVG vector asset, {}, {size}, {} bytes). It is not a raster image: stamp it onto one with {{\"op\": \"svg_overlay\", \"svg_revision_id\": \"{}\", \"x\": 0, \"y\": 0}}. Text is NOT rendered (no fonts are loaded, for determinism) - convert text to paths.\npath: {}",
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
        let text = if warnings.is_empty() {
            text
        } else {
            format!("{text}\nwarnings: {}", warnings.join("; "))
        };
        Ok((
            ImportOutput {
                revision: summary,
                reused,
                source_path: path.to_string_lossy().into_owned(),
                warnings,
                already_derived_from,
            },
            text,
        ))
    }

    /// 取り込んだ revision と同じ sha256 を持つ**派生** revision を台帳から探す(読み取り専用)。
    fn find_derived_with_sha256(
        &self,
        imported: &AssetRevision,
    ) -> Result<Option<AlreadyDerivedFrom>, StoreError> {
        Ok(self
            .store
            .list_revisions(None)?
            .into_iter()
            .find(|r| {
                r.sha256 == imported.sha256
                    && r.revision_id != imported.revision_id
                    && r.source_revision_id.is_some()
            })
            .map(|r| AlreadyDerivedFrom {
                revision_id: r.revision_id,
                recipe_hash: r.recipe_hash,
                source_revision_id: r.source_revision_id.unwrap_or_default(),
            }))
    }

    // -- 2. inspect_image ---------------------------------------------------

    /// revision の寸法・フォーマット・EXIF 要約・色情報・容量を返す(read-only)。
    pub fn inspect_image(&self, params: &RevisionParams) -> CallToolResult {
        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT 等)はデコードを試みず、構造化エラーで返す。
        if !is_raster_image(&revision.mime_type) {
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
        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT / SVG 等)はデコードを試みず、
        // inspect_image と同じ構造化エラーで返す。
        if !is_raster_image(&revision.mime_type) {
            return not_an_image(&params.revision_id, &revision.mime_type);
        }
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
        // 既定では score_curve を返さない。要る人だけがフラグで取り寄せる。
        let text = if params.include_score_curve {
            format!(
                "{text}\nscore_curve: {} points over the search range (normalized to 1.0 at the peak).",
                detection.score_curve.len()
            )
        } else {
            format!(
                "{text}\n(score_curve omitted; pass include_score_curve=true to see the whole search range and judge the peak sharpness yourself)"
            )
        };
        ok_result(
            text,
            &DetectTiltOutput {
                revision_id: params.revision_id.clone(),
                detection: DetectionView {
                    detection,
                    include_score_curve: params.include_score_curve,
                },
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
            })
            .collect();
        // 人間可読テキスト用の詳細(要約 + パラメータ表記)。structuredContent には載せない。
        let params_line: Vec<String> = selected
            .iter()
            .map(|op| {
                op.params
                    .iter()
                    .map(|p| p.compact())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect();

        let presets = crate::presets::all();
        let preset_names: Vec<String> = presets.iter().map(|p| p.name.clone()).collect();

        let mut text = format!(
            "{} recipe operations{} (params: name* = required, name? = optional, =x or (def) = default):",
            entries.len(),
            match params.category.as_deref() {
                Some(c) => format!(" in category {c}"),
                None => String::new(),
            }
        );
        for (op, params) in selected.iter().zip(&params_line) {
            text.push_str(&format!(
                "\n- {} [{}] {}{}",
                op.name,
                op.category,
                op.summary,
                if params.is_empty() {
                    String::new()
                } else {
                    format!(" | {params}")
                }
            ));
        }
        if !presets.is_empty() {
            // 実運用 FB: 名前と1行説明だけでは「中身が見えない」ので敬遠された。
            // op タグを → でつないだ骨組みだけ添える(パラメータは explain_operation 側)。
            text.push_str("\nPresets (pass preset=<name> instead of recipe):");
            for preset in &presets {
                text.push_str(&format!(
                    "\n- {} — {}: {}",
                    preset.name,
                    preset_op_summary(&preset.recipe),
                    preset.description
                ));
            }
        }
        text.push_str(
            "\ncall explain_operation {\"operation\":\"<name>\"} for full params, examples and gotchas (it takes preset names too, and prints the preset's whole op list).",
        );

        ok_result(
            text,
            &ListOperationsOutput {
                count: entries.len(),
                ops: entries,
                presets: preset_names,
            },
        )
    }

    // -- 3c. explain_operation ----------------------------------------------

    /// 1つの op の完全な仕様(パラメータ表・例・落とし穴)を返す(read-only)。
    pub fn explain_operation(&self, params: &ExplainOperationParams) -> CallToolResult {
        // `layers`(v0.6)はレシピ構造のリファレンスであって op ではないので、
        // カタログ(`crate::vocab::OPERATIONS` / `find`)には入れず、ここで別経路で拾う。
        let trimmed = params.operation.trim();
        let doc = if trimmed == crate::vocab::LAYERS_DOC.name {
            &crate::vocab::LAYERS_DOC
        } else {
            match crate::vocab::find(trimmed) {
                Some(doc) => doc,
                // op でなければプリセット名として解決を試みる(実運用 FB:
                // プリセットの中身が見えないので使われなかった)。
                None => match crate::presets::resolve(trimmed) {
                    Ok(preset) => return explain_preset(&preset),
                    Err(_) => {
                        let valid = crate::vocab::operation_names();
                        let presets = crate::presets::preset_names();
                        let suggestions = did_you_mean_any(trimmed);
                        return tool_error(
                            "unknown_operation",
                            format!(
                                "unknown name {:?}. Valid operations: {}. Valid presets: {}.",
                                params.operation,
                                valid.join(", "),
                                presets.join(", ")
                            ),
                            serde_json::json!({
                                "given": params.operation,
                                "valid_operations": valid,
                                "valid_presets": presets,
                                // 旧クライアント互換: op 名の一覧はここにも残す。
                                "valid_values": valid,
                                "did_you_mean": suggestions,
                                "recovery": "call explain_operation again with one of valid_operations (a recipe op) or valid_presets (a built-in named recipe), or list_operations for the whole catalog",
                            }),
                        );
                    }
                },
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
            &ExplainResult::Operation(Box::new(ExplainOperationOutput {
                kind: "operation".to_string(),
                name: doc.name.to_string(),
                category: doc.category.to_string(),
                summary: doc.summary.to_string(),
                params: param_entries,
                examples: doc.examples.iter().map(|e| e.to_string()).collect(),
                warnings: doc.warnings.iter().map(|w| w.to_string()).collect(),
            })),
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
        if !is_raster_image(&reference.mime_type) {
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
    /// `revision_ids` を渡すと**同じレシピ**を各 revision に適用する
    /// (実運用 FB: 28 枚に同じ調整を当てるのに 28 往復かかっていた)。
    /// 冪等ショートサーキットは revision ごとに個別に効き、1件の失敗では
    /// バッチを止めずにその要素へ error を入れる。
    pub fn apply_transform(&self, params: &TransformParams) -> CallToolResult {
        let recipe = match resolve_recipe(params.recipe.as_ref(), params.preset.as_deref()) {
            Ok(recipe) => recipe,
            Err(result) => return result,
        };
        tri!(atx_core::recipe::validate(&recipe), atx_error);
        let recipe_hash = tri!(atx_core::recipe_hash(&recipe), atx_error);

        match (params.revision_id.as_deref(), params.revision_ids.as_deref()) {
            (Some(revision_id), None) => {
                match self.apply_one(revision_id, &recipe, &recipe_hash, params.preset.as_deref()) {
                    Ok((output, text)) => ok_result(text, &ApplyResult::Single(Box::new(output))),
                    Err(result) => result,
                }
            }
            (None, Some(revision_ids)) => {
                self.apply_batch(revision_ids, &recipe, &recipe_hash, params.preset.as_deref())
            }
            (Some(_), Some(_)) => tool_error(
                "revision_id_and_revision_ids_conflict",
                "revision_id and revision_ids are mutually exclusive, but both were given",
                serde_json::json!({
                    "recovery": "pass revision_id for one image, or revision_ids for a batch of up to 64, not both",
                }),
            ),
            (None, None) => tool_error(
                "revision_id_or_revision_ids_required",
                "one of revision_id (a single image) or revision_ids (a batch of up to 64) is required",
                serde_json::json!({
                    "recovery": "pass revision_id = \"rev_...\", or revision_ids = [\"rev_...\", \"rev_...\"]; call list_assets to see the available revision_ids",
                }),
            ),
        }
    }

    /// 同じレシピを複数 revision に適用する。入力順を保ち、失敗しても続行する。
    fn apply_batch(
        &self,
        revision_ids: &[String],
        recipe: &TransformRecipe,
        recipe_hash: &str,
        preset: Option<&str>,
    ) -> CallToolResult {
        if let Err(result) = check_batch_size(revision_ids.len(), "revision_ids") {
            return result;
        }

        let mut results: Vec<ApplyEntry> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut failed: Vec<BatchFailure> = Vec::new();
        for revision_id in revision_ids {
            match self.apply_one(revision_id, recipe, recipe_hash, preset) {
                Ok((output, _)) => {
                    warnings.extend(
                        output
                            .warnings
                            .iter()
                            .map(|w| format!("{revision_id}: {w}")),
                    );
                    results.push(ApplyEntry {
                        revision_id: revision_id.clone(),
                        revision: Some(output.revision),
                        reused: Some(output.reused),
                        error: None,
                    });
                }
                Err(result) => {
                    let error = error_info(&result);
                    failed.push(BatchFailure {
                        path: revision_id.clone(),
                        error: error.clone(),
                    });
                    results.push(ApplyEntry {
                        revision_id: revision_id.clone(),
                        revision: None,
                        reused: None,
                        error: Some(error),
                    });
                }
            }
        }

        let succeeded = results.len() - failed.len();
        if succeeded == 0 {
            let listed: Vec<String> = failed
                .iter()
                .map(|f| format!("- {}: [{}] {}", f.path, f.error.code, f.error.message))
                .collect();
            return tool_error(
                "apply_failed",
                format!(
                    "all {} transforms failed:\n{}",
                    failed.len(),
                    listed.join("\n")
                ),
                serde_json::json!({
                    "failed": failed,
                    "recipe_hash": recipe_hash,
                    "recovery": "fix the revision_ids or the recipe above and call apply_transform again; call list_assets to see the available revision_ids",
                }),
            );
        }

        let reused = results.iter().filter(|r| r.reused == Some(true)).count();
        let mut text = format!(
            "Applied the same {}recipe (hash {}) to {succeeded} of {} revision(s){}{}",
            preset_note(preset),
            short_hash(Some(recipe_hash)),
            results.len(),
            if reused > 0 {
                format!(" ({reused} reused an existing derivation)")
            } else {
                String::new()
            },
            if failed.is_empty() {
                String::new()
            } else {
                format!(", {} failed", failed.len())
            },
        );
        for entry in &results {
            match (&entry.revision, &entry.error) {
                (Some(revision), _) => text.push_str(&format!(
                    "\n- {} -> {} ({}x{} {}, {} bytes){}\n  path: {}",
                    entry.revision_id,
                    revision.revision_id,
                    revision.width,
                    revision.height,
                    revision.mime_type,
                    revision.byte_size,
                    if entry.reused == Some(true) {
                        " [reused]"
                    } else {
                        ""
                    },
                    revision.path,
                )),
                (None, Some(error)) => text.push_str(&format!(
                    "\n- {}: FAILED [{}] {}",
                    entry.revision_id, error.code, error.message
                )),
                (None, None) => {}
            }
        }
        if !warnings.is_empty() {
            text.push_str(&format!("\nwarnings: {}", warnings.join("; ")));
        }

        ok_result(
            text,
            &ApplyResult::Batch(Box::new(ApplyBatchOutput {
                count: results.len(),
                succeeded,
                results,
                recipe_hash: recipe_hash.to_string(),
                engine_version: ENGINE_VERSION.to_string(),
                warnings,
            })),
        )
    }

    /// 1 revision にレシピを適用する。成功なら `(structuredContent, テキストサマリ)`、
    /// 失敗なら単一呼び出しがそのまま返せる構造化エラー。
    fn apply_one(
        &self,
        revision_id: &str,
        recipe: &TransformRecipe,
        recipe_hash: &str,
        preset: Option<&str>,
    ) -> Result<(ApplyTransformOutput, String), CallToolResult> {
        macro_rules! tri {
            ($expr:expr, $map:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(e) => return Err($map(e)),
                }
            };
        }

        let source = tri!(self.store.get_revision(revision_id), store_error);
        // 変換の入力はラスタ画像でなければならない(SVG / .cube は
        // **レシピから参照される**アセットであって、パイプラインの入力ではない)。
        if !is_raster_image(&source.mime_type) {
            return Err(not_an_image(revision_id, &source.mime_type));
        }

        // --- 冪等ショートサーキット: 既存派生があれば再変換しない ---
        let existing = tri!(self.store.list_revisions(None), store_error)
            .into_iter()
            .find(|r| {
                r.source_revision_id.as_deref() == Some(revision_id)
                    && r.recipe_hash.as_deref() == Some(recipe_hash)
            });
        if let Some(revision) = existing {
            let summary = RevisionSummary::new(&self.store, &revision);
            let text = format!(
                "Reused existing revision {} for this recipe (no re-transform): {}x{} {} ({} bytes)\npath: {}",
                summary.revision_id, summary.width, summary.height, summary.mime_type, summary.byte_size, summary.path
            );
            return Ok((
                ApplyTransformOutput {
                    revision: summary,
                    source_revision_id: revision_id.to_string(),
                    recipe_hash: recipe_hash.to_string(),
                    engine_version: ENGINE_VERSION.to_string(),
                    reused: true,
                    warnings: Vec::new(),
                },
                text,
            ));
        }

        let bytes = tri!(self.store.read_bytes(revision_id), store_error);
        let output = tri!(
            atx_core::apply_recipe_with_assets(
                &bytes,
                recipe,
                &self.limits,
                &StoreAssets(&self.store)
            ),
            atx_error
        );
        let revision = tri!(
            self.store.record_derivation(
                &source.revision_id,
                recipe,
                recipe_hash,
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
            preset_note(preset),
            revision_id,
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
        Ok((
            ApplyTransformOutput {
                revision: summary,
                source_revision_id: revision_id.to_string(),
                recipe_hash: recipe_hash.to_string(),
                engine_version: ENGINE_VERSION.to_string(),
                reused: false,
                warnings: output.warnings,
            },
            text,
        ))
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
        let source = tri!(self.store.get_revision(&params.revision_id), store_error);
        if !is_raster_image(&source.mime_type) {
            return not_an_image(&params.revision_id, &source.mime_type);
        }

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
                if !is_raster_image(&mask_revision.mime_type) {
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
    /// `layout: "diff"`(v0.7)だけは並べる代わりに画素差分ヒートマップを1枚返す
    /// ([`Self::compare_revisions_diff`] に委譲)。
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

        // 画像でない revision(.cube LUT / SVG 等)はデコードを試みず、
        // inspect_image と同じ構造化エラーを「どちら側か」付きで返す。
        for (rev, side) in [(&rev_a, "a"), (&rev_b, "b")] {
            if !is_raster_image(&rev.mime_type) {
                return not_an_image_side(&rev.revision_id, &rev.mime_type, Some(side));
            }
        }

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

        if params.layout == CompareLayout::Diff {
            return self.compare_revisions_diff(params, &rev_a, &rev_b, img_a, img_b);
        }

        let scaled_a = scale_contain(img_a, COMPARE_LONG_EDGE).to_rgb8();
        let scaled_b = scale_contain(img_b, COMPARE_LONG_EDGE).to_rgb8();

        let canvas = match params.layout {
            CompareLayout::SideBySide => compose_side_by_side(&scaled_a, &scaled_b, COMPARE_GAP_PX),
            CompareLayout::Stacked => compose_stacked(&scaled_a, &scaled_b, COMPARE_GAP_PX),
            // layout="diff" は上の早期 return (compare_revisions_diff) で処理済み。
            CompareLayout::Diff => unreachable!("diff layout returns earlier"),
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
            CompareLayout::Diff => unreachable!("diff layout returns earlier"),
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
                mean_abs_diff: None,
                max_abs_diff: None,
                changed_pixel_ratio: None,
            },
            vec![image_block],
        )
    }

    // -- 5c. compare_revisions (layout: "diff") ------------------------------

    /// `compare_revisions` の diff レイアウト専用の経路。
    ///
    /// side_by_side/stacked と違い縮小前に**寸法が完全一致**していることを要求する
    /// (画素単位で差分を取るため)。ヒートマップは長辺 640 に縮めてから jpeg 化するが、
    /// 統計(mean/max/changed_pixel_ratio)は縮小前のフル解像度の差分から計算する
    /// (縮小はプレビューの都合であって統計を歪めてはいけないため)。
    fn compare_revisions_diff(
        &self,
        params: &CompareRevisionsParams,
        rev_a: &AssetRevision,
        rev_b: &AssetRevision,
        img_a: image::DynamicImage,
        img_b: image::DynamicImage,
    ) -> CallToolResult {
        let full_a = img_a.to_rgb8();
        let full_b = img_b.to_rgb8();
        if full_a.dimensions() != full_b.dimensions() {
            let (aw, ah) = full_a.dimensions();
            let (bw, bh) = full_b.dimensions();
            return tool_error(
                "dimension_mismatch",
                format!(
                    "compare_revisions layout=\"diff\" requires equal dimensions, but A={} is {aw}x{ah} and B={} is {bw}x{bh}",
                    rev_a.revision_id, rev_b.revision_id,
                ),
                serde_json::json!({
                    "revision_id_a": rev_a.revision_id,
                    "a_width": aw,
                    "a_height": ah,
                    "revision_id_b": rev_b.revision_id,
                    "b_width": bw,
                    "b_height": bh,
                    "recovery": "resize one revision to match the other's dimensions (apply_transform with a resize op) before comparing with layout=\"diff\", or use layout=\"side_by_side\"/\"stacked\" instead",
                }),
            );
        }

        let (heatmap, mean_abs_diff, max_abs_diff, changed_pixel_ratio) =
            diff_heatmap(&full_a, &full_b);
        let canvas =
            scale_contain(image::DynamicImage::ImageRgb8(heatmap), COMPARE_LONG_EDGE).to_rgb8();
        let (cw, ch) = canvas.dimensions();
        let composed_bytes = tri!(encode_jpeg_q80(&canvas), |e: String| tool_error(
            "encode_failed",
            format!("failed to encode diff heatmap jpeg: {e}"),
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

        let text = format!(
            "Diff heatmap A={} ({}x{} {}, {} bytes) vs B={} ({}x{} {}, {} bytes): mean_abs_diff={mean_abs_diff:.3}, max_abs_diff={max_abs_diff}, changed_pixel_ratio={changed_pixel_ratio:.4} -> {cw}x{ch} jpeg ({} bytes)\npath: {path}",
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
            composed_bytes.len(),
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
                // diff は1枚合成で空間的な左右/上下の配置が無いので位置は意味を持たない。
                a_position: "n/a".to_string(),
                b_position: "n/a".to_string(),
                width: cw,
                height: ch,
                mime_type: "image/jpeg".to_string(),
                byte_size: composed_bytes.len() as u64,
                preview_path: path,
                mean_abs_diff: Some(mean_abs_diff),
                max_abs_diff: Some(max_abs_diff),
                changed_pixel_ratio: Some(changed_pixel_ratio),
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

        // ワークスペース root 配下への書き込みは**一律に**拒否する。
        // objects / previews だけでなく台帳(assets.jsonl)も、将来増えるファイルも同じ:
        // 不変ストアの管理領域へ export するのが正当なケースは存在しない。
        //
        // 比較は**両側を canonicalize してから**行う: macOS の /tmp は
        // /private/tmp へのシンボリックリンクなので、文字列比較だけでは
        // シンボリックリンク経由のパスが素通りしてしまう。
        let ws_root = real_prefix(self.store.root());
        let dest_real = real_prefix(&dest);
        if dest_real.starts_with(&ws_root) {
            return tool_error(
                "dest_inside_workspace",
                format!(
                    "{} is inside the immutable workspace store; exporting there is not allowed",
                    dest.display()
                ),
                serde_json::json!({
                    "dest_path": dest.to_string_lossy(),
                    "resolved_dest_path": dest_real.to_string_lossy(),
                    "workspace": ws_root.to_string_lossy(),
                    "recovery": "choose a destination outside the workspace directory entirely (objects/, previews/ and the assets.jsonl ledger all live there)",
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
    recipe: Option<&RecipeJson>,
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
        (Some(recipe), None) => deserialize_recipe(recipe),
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

/// プリセットの中身(op 配列)をそのまま見せる `explain_operation` の分岐。
fn explain_preset(preset: &crate::presets::Preset) -> CallToolResult {
    let ops: Vec<serde_json::Value> = preset
        .recipe
        .operations
        .iter()
        .map(|op| serde_json::to_value(op).unwrap_or(serde_json::Value::Null))
        .collect();
    let layers = preset
        .recipe
        .layers
        .as_ref()
        .and_then(|l| serde_json::to_value(l).ok());

    let mut text = format!(
        "{} [preset] — {}\nShape: {}\nRecipe ({} operation(s), apply with preset=\"{}\" or paste it as a raw recipe):\n",
        preset.name,
        preset.description,
        preset_op_summary(&preset.recipe),
        ops.len(),
        preset.name,
    );
    for op in &ops {
        text.push_str(&format!("- {op}\n"));
    }
    if let Some(layers) = &layers {
        text.push_str(&format!("layers: {layers}\n"));
    }
    text.push_str(
        "A preset is pure sugar: the recipe_hash is computed on the resolved recipe above, so editing a copy of it lands on a different revision than the preset only where you actually changed something.",
    );

    ok_result(
        text.trim_end(),
        &ExplainResult::Preset(Box::new(ExplainPresetOutput {
            kind: "preset".to_string(),
            name: preset.name.clone(),
            description: preset.description.clone(),
            ops,
            layers,
        })),
    )
}

/// op 名とプリセット名の**両方**からの「もしかして」。
fn did_you_mean_any(given: &str) -> Vec<String> {
    let mut out: Vec<String> = crate::vocab::did_you_mean(given)
        .into_iter()
        .map(str::to_string)
        .collect();
    let lower = given.to_ascii_lowercase();
    out.extend(
        crate::presets::preset_names()
            .into_iter()
            .filter(|name| name.contains(&lower) || lower.contains(name))
            .map(str::to_string),
    );
    out
}

/// カタログ1行に収めるための op 名の短縮形(長い名前だけ縮める)。
const OP_ABBREVIATIONS: [(&str, &str); 6] = [
    ("white_balance", "wb"),
    ("unsharp_mask", "unsharp"),
    ("gradient_map", "gradmap"),
    ("strip_metadata", "strip"),
    ("color_matrix", "cmatrix"),
    ("svg_overlay", "svg"),
];

/// プリセットの中身を1行に圧縮する(`"wb→curves→grain"`)。パラメータは載せない。
///
/// `layers` を持つプリセットは、レイヤー段があることだけ `layers→` で示す。
fn preset_op_summary(recipe: &TransformRecipe) -> String {
    let mut tags: Vec<String> = Vec::new();
    if recipe.layers.is_some() {
        tags.push("layers".to_string());
    }
    tags.extend(recipe.operations.iter().map(|op| {
        let name = operation_tag(op);
        OP_ABBREVIATIONS
            .iter()
            .find(|(full, _)| *full == name)
            .map(|(_, short)| (*short).to_string())
            .unwrap_or(name)
    }));
    tags.join("→")
}

/// `Operation` の serde タグ(= `{"op": "..."}` に書く名前)を取り出す。
fn operation_tag(op: &Operation) -> String {
    serde_json::to_value(op)
        .ok()
        .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}

/// レシピハッシュの先頭 8 文字(人間が見比べるための短縮形)。
fn short_hash(hash: Option<&str>) -> String {
    match hash {
        Some(h) if h.len() >= 8 => h[..8].to_string(),
        Some(h) => h.to_string(),
        None => "unknown".to_string(),
    }
}

/// バッチ引数の件数(1..=[`MAX_BATCH`])を検査する。
fn check_batch_size(len: usize, field: &str) -> Result<(), CallToolResult> {
    if (1..=MAX_BATCH).contains(&len) {
        return Ok(());
    }
    Err(tool_error(
        "invalid_batch_size",
        format!("{field} must hold between 1 and {MAX_BATCH} entries, got {len}"),
        serde_json::json!({
            "given": len,
            "min": 1,
            "max": MAX_BATCH,
            "recovery": format!("split the work into chunks of at most {MAX_BATCH} and call the tool once per chunk"),
        }),
    ))
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
    // v0.6: `layers` はそのまま素通しする。トップレベル `operations` は
    // (layers があってもなくても)合成結果に対する仕上げパスなので、
    // ここで差し替えた「長辺 768 リサイズ + jpeg q80 encode」がそのまま
    // レイヤー合成後の縮小プレビューになる。layers を落とすと、
    // ユーザが layers で意図した合成そのものがプレビューから消えてしまう。
    TransformRecipe {
        operations,
        layers: recipe.layers.clone(),
    }
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

/// `compare_revisions` diff ヒートマップの色ランプの stop 点。
///
/// `(d, [r,g,b])` の4点を d の昇順に並べ、隣接する2点の間を線形補間する
/// (固定小数点ではなく f32 で計算するが、丸めは常に `round()` で行うので
/// 同じ入力からは常に同じバイト列が出る = 決定論)。
///
/// - `d=0`   -> `#101030`(near-black blue。無変化の背景を「黒」ではなく
///   わずかに青みがかった色で塗ることで、d=0 の領域と jpeg 圧縮由来の
///   黒つぶれを視覚的に区別できるようにする)
/// - `d=64`  -> `#0040FF`(blue。small-but-real な差分)
/// - `d=160` -> `#FFE800`(yellow。中程度の差分)
/// - `d=255` -> `#FF0000`(red。最大級の差分)
const DIFF_RAMP_STOPS: [(u8, [u8; 3]); 4] = [
    (0, [0x10, 0x10, 0x30]),
    (64, [0x00, 0x40, 0xFF]),
    (160, [0xFF, 0xE8, 0x00]),
    (255, [0xFF, 0x00, 0x00]),
];

/// 画素ごとの差分値 `d`(0..=255)を [`DIFF_RAMP_STOPS`] の区分線形ランプで色に写す。
fn diff_ramp_color(d: u8) -> [u8; 3] {
    for pair in DIFF_RAMP_STOPS.windows(2) {
        let (d0, c0) = pair[0];
        let (d1, c1) = pair[1];
        if d <= d1 {
            let span = (d1 - d0) as f32;
            let t = if span <= 0.0 {
                0.0
            } else {
                (d.saturating_sub(d0)) as f32 / span
            };
            let mut out = [0u8; 3];
            for i in 0..3 {
                let lo = c0[i] as f32;
                let hi = c1[i] as f32;
                out[i] = (lo + t * (hi - lo)).round() as u8;
            }
            return out;
        }
    }
    // d は u8 なので最終 stop (255) を超えることはなく、ここには到達しない。
    DIFF_RAMP_STOPS[DIFF_RAMP_STOPS.len() - 1].1
}

/// `compare_revisions` layout="diff" の中核: 同寸法 RGB8 画像2枚から
/// (ヒートマップ, mean_abs_diff, max_abs_diff, changed_pixel_ratio) を作る。
///
/// - 画素ごとの `d` = 3チャンネルの絶対差の**最大値**(u8)。ヒートマップの色と
///   `max_abs_diff` / `changed_pixel_ratio`(`d > 2` の画素の割合)はこの `d` を使う。
/// - `mean_abs_diff` は `d` ではなく、**全チャンネル・全画素**の絶対差の単純平均
///   (= R/G/B 3つの差分をまとめて均した「平均的などのくらいズレたか」)。
///   `max_abs_diff` が「最悪1点」、`mean_abs_diff` が「全体としての量」を表す。
///
/// 入力の寸法が 0 の場合(あり得ないが防御的に)は両方 0.0 を返す。
fn diff_heatmap(a: &RgbImage, b: &RgbImage) -> (RgbImage, f64, u8, f64) {
    let (w, h) = a.dimensions();
    let mut heatmap = RgbImage::new(w, h);
    let mut sum_abs: u64 = 0;
    let mut max_d: u8 = 0;
    let mut changed_pixels: u64 = 0;
    const CHANGED_THRESHOLD: u8 = 2;

    for y in 0..h {
        for x in 0..w {
            let pa = a.get_pixel(x, y).0;
            let pb = b.get_pixel(x, y).0;
            let mut d: u8 = 0;
            for c in 0..3 {
                let diff = (pa[c] as i16 - pb[c] as i16).unsigned_abs() as u8;
                sum_abs += diff as u64;
                if diff > d {
                    d = diff;
                }
            }
            if d > max_d {
                max_d = d;
            }
            if d > CHANGED_THRESHOLD {
                changed_pixels += 1;
            }
            heatmap.put_pixel(x, y, image::Rgb(diff_ramp_color(d)));
        }
    }

    let total_pixels = w as u64 * h as u64;
    let (mean_abs_diff, changed_pixel_ratio) = if total_pixels == 0 {
        (0.0, 0.0)
    } else {
        (
            sum_abs as f64 / (total_pixels * 3) as f64,
            changed_pixels as f64 / total_pixels as f64,
        )
    };

    (heatmap, mean_abs_diff, max_d, changed_pixel_ratio)
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

/// パスの「実在する最長の祖先」を `canonicalize` し、残りの成分をそのまま付け直す。
///
/// `export_asset` の書き出し先はまだ存在しないので `canonicalize` を直接は呼べないが、
/// 親ディレクトリまでは実在する。macOS の `/tmp` → `/private/tmp` のように
/// **祖先がシンボリックリンク**だと文字列比較でのワークスペース判定を素通りしてしまうため、
/// 「ワークスペース内か」を判定する前に両側をこの関数へ通す。
/// どの祖先も canonicalize できない場合は入力をそのまま返す(判定は字句比較に退化する)。
fn real_prefix(path: &Path) -> PathBuf {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(real) = cursor.canonicalize() {
            let mut out = real;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match cursor.file_name() {
            Some(name) => suffix.push(name.to_os_string()),
            None => return path.to_path_buf(),
        }
        if !cursor.pop() {
            return path.to_path_buf();
        }
    }
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
            layers: None,
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

    /// v0.6: `layers` を持つレシピをプレビュー用に書き換えても `layers` が
    /// 落ちないこと(仕上げパスの差し替えだけを行い、合成そのものは保つ)。
    #[test]
    fn preview_recipe_keeps_layers() {
        use atx_core::recipe::{Layer, LayerSource};

        let recipe = TransformRecipe {
            operations: vec![],
            layers: Some(vec![
                Layer {
                    source: LayerSource::base(),
                    ops: vec![],
                    mask: None,
                    blend_mode: Default::default(),
                    opacity: 1.0,
                },
                Layer {
                    source: LayerSource::base(),
                    ops: vec![],
                    mask: None,
                    blend_mode: Default::default(),
                    opacity: 0.5,
                },
            ]),
        };
        let preview = preview_recipe_of(&recipe);
        assert!(
            preview.layers.is_some(),
            "preview_recipe_of must not drop layers"
        );
        assert_eq!(preview.layers.as_ref().unwrap().len(), 2);
        // finishing pass(resize + encode)は普段どおり足される。
        assert_eq!(preview.operations.len(), 2);
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

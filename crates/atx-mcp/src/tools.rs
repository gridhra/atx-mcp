//! ツールの実処理(トランスポート非依存・同期)。
//!
//! rmcp の `#[tool]` 関数([`crate::server`])は、ここのメソッドを呼ぶだけの薄いラッパである。
//! こうしておくと統合テストが stdio / JSON-RPC を一切経由せずにフロー全体を検証できる。
//!
//! 返却規約(DESIGN.md §4.1):
//! - 常に「人間可読のテキストサマリ(パス込み)」+ `structuredContent`(機械可読 JSON)の両方を返す
//! - `render_preview` のみ inline ImageContent(base64 jpeg、長辺は既定 768、
//!   `long_edge` で 256..=1568 まで指定可)を追加する
//! - エラーは `CallToolResult::error`(is_error=true)で、原因と回復手順を構造化して返す

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use atx_core::recipe::{Fit, Operation, OutputFormat};
use atx_core::{AtxError, ImageInfo, Limits, TransformRecipe, ENGINE_VERSION};
use atx_geometry::{
    DetectParams, DocumentDetection, DocumentParams, TextBlockDetection, TextBlockParams,
    TiltDetection,
};
use atx_store::{ext_for_mime, AssetRevision, AssetStore, StoreError};
use base64::Engine as _;
use image::{ImageEncoder, RgbImage};
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// プレビューの長辺上限(DESIGN.md §4.1)。
pub const PREVIEW_LONG_EDGE: u32 = 768;
/// `inspect_image` のテキストサマリに EXIF 全量から抜粋する件数。
const EXIF_SUMMARY_HEAD: usize = 5;
/// `detect_document` の `min_area_ratio` に指定できる下限(DESIGN.md §9.12)。
pub const MIN_AREA_RATIO_MIN: f64 = 0.05;
/// `detect_document` の `min_area_ratio` に指定できる上限。
pub const MIN_AREA_RATIO_MAX: f64 = 1.0;
/// `render_preview` の `long_edge` に指定できる下限。
pub const PREVIEW_LONG_EDGE_MIN: u32 = 256;
/// `render_preview` の `long_edge` に指定できる上限。
///
/// VLM がインライン画像を受け取れる実務上の上限(長辺 ~1568px)に合わせてある。
/// 文字を読ませる用途では既定の 768 では足りないため、ここまで上げられる
/// (DESIGN.md §9.12)。
pub const PREVIEW_LONG_EDGE_MAX: u32 = 1568;
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
/// TrueType フォントアセットの MIME type(v0.6「`svg_overlay` の文字描画」)。
///
/// `image/` で始まらないので [`is_raster_image`] は最初から false を返す
/// (= inspect_image / detect_* / apply_transform は既存の `not_an_image` で弾く)。
/// atx-store の `ext_for_mime` がこれを `.ttf` 拡張子に写す。
pub const FONT_TTF_MIME: &str = "font/ttf";
/// OpenType(CFF アウトライン、`OTTO`)フォントアセットの MIME type。
pub const FONT_OTF_MIME: &str = "font/otf";
/// `detect_text_blocks` の `max_blocks` に指定できる下限。
pub const MAX_TEXT_BLOCKS_MIN: usize = 1;
/// `detect_text_blocks` の `max_blocks` に指定できる上限。
pub const MAX_TEXT_BLOCKS_MAX: usize = 128;
/// `detect_text_blocks` の `min_block_area_ratio` に指定できる下限。
pub const MIN_BLOCK_AREA_RATIO_MIN: f64 = 0.0;
/// `detect_text_blocks` の `min_block_area_ratio` に指定できる上限。
pub const MIN_BLOCK_AREA_RATIO_MAX: f64 = 1.0;

/// レシピ内プリセットマクロの op 名(`{"op":"preset","name":"<preset>"}`)。
///
/// atx-core の DSL には存在しない **MCP 層だけの糖衣**で、
/// [`expand_preset_macros`] が deserialize の前にその場で展開する。
pub const PRESET_MACRO_OP: &str = "preset";

/// `explain_operation "preset"` が返すマクロのリファレンス。
///
/// `vocab::OPERATIONS`(= `list_operations` のカタログ)には入れない:
/// これは core の op ではなく MCP 層の糖衣なので、op 件数を固定している
/// テストと語彙の定義を汚さないため(ROADMAP「Agent UX の規律」)。
const PRESET_MACRO_DOC: crate::vocab::OpDoc = crate::vocab::OpDoc {
    name: PRESET_MACRO_OP,
    category: "structure",
    summary: "Recipe macro (not a core op): inlines a built-in preset's operations at this position, so a preset can be combined with your own operations in one recipe.",
    params: &[crate::vocab::ParamDoc {
        name: "name",
        type_hint: "string (a built-in preset name)",
        requirement: "required",
        semantics: "The preset whose operations are spliced in here, in order. Call list_operations for the preset names, or explain_operation with a preset name to read the operations it will insert. A preset that carries a layers stack cannot be inlined (pass it as the top-level preset instead).",
    }],
    examples: &[
        r#"{"operations": [{"op": "perspective", "vertical_degrees": -3.5}, {"op": "trim"}, {"op": "preset", "name": "ocr_document"}, {"op": "encode", "format": "png"}]}"#,
    ],
    warnings: &[
        "The macro is expanded before anything else runs, and the recipe_hash is computed on the EXPANDED recipe: the macro, the equivalent hand-written operations and a plain preset=<name> call all land on the same revision.",
        "Expansion is flat: the preset's operations take the macro's place one after another, so an encode inside the preset must still end up last in the whole recipe (a preset ending in encode cannot be followed by more operations).",
        "It works in layers[].ops too. The error messages point at the EXPANDED index and name the preset they came from, e.g. operations[4] (encode) ... (expanded from preset \"web_optimize\").",
    ],
};

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

/// `inspect_image` の引数。
///
/// `include_exif` は既定 false なので、v0.5 までの
/// `{"revision_id": "rev_..."}` だけの呼び出しはそのまま同じ結果を返す。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct InspectImageParams {
    /// 対象 revision ID("rev_...")。
    pub revision_id: String,
    /// EXIF 全フィールドを `info.exif` に載せるか。既定 false。
    ///
    /// 既定で落としているのはプライバシーのため: 全量には GPS 座標や
    /// 個人名が入りうるので、明示的に要求されたときだけ返す
    /// (`exif_summary` と `has_gps` は従来どおり常に載る)。
    #[serde(default)]
    pub include_exif: bool,
}

impl InspectImageParams {
    /// EXIF 全量なしの引数を作る(テスト・呼び出し側の糖衣)。
    pub fn new(revision_id: impl Into<String>) -> Self {
        Self {
            revision_id: revision_id.into(),
            include_exif: false,
        }
    }

    /// `include_exif: true` にした自分を返す。
    pub fn with_exif(mut self) -> Self {
        self.include_exif = true;
        self
    }
}

/// `detect_text_blocks` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct DetectTextBlocksParams {
    /// 対象 revision ID("rev_...")。
    pub revision_id: String,
    /// 返すブロック数の上限(1..=128)。省略時は 32。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_blocks: Option<usize>,
    /// 作業画像面積に対する 1 ブロックの最小面積比(0.0..=1.0)。
    /// これ未満の成分はノイズとして捨てる。省略時は 0.00005。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_block_area_ratio: Option<f64>,
}

impl DetectTextBlocksParams {
    /// 既定パラメータの引数を作る(テスト・呼び出し側の糖衣)。
    pub fn new(revision_id: impl Into<String>) -> Self {
        Self {
            revision_id: revision_id.into(),
            max_blocks: None,
            min_block_area_ratio: None,
        }
    }
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

/// `detect_document` の引数。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct DetectDocumentParams {
    /// 対象 revision ID("rev_...")。
    pub revision_id: String,
    /// 画像面積に対する候補四角形の最小面積比(0.05..=1.0)。省略時は 0.2。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area_ratio: Option<f64>,
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
    /// プレビューの長辺(256..=1568)。省略時は 768。
    /// 文字を読む用途では 1568 まで上げる(DESIGN.md §9.12)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_edge: Option<u32>,
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
///
/// 単数形(`revision_id` + `dest_path`)と複数形(`revision_ids` + `dest_dir`
/// + 任意の `filename_template`)のどちらか一方。単数形は v0.5 までと完全に同じ挙動。
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ExportAssetParams {
    /// 書き出す revision ID("rev_...")。`revision_ids` とは排他で、どちらか一方が必須。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// 複数 revision を1回で書き出す(1..=64 件)。`revision_id` とは排他。
    /// このときは `dest_path` ではなく `dest_dir` を渡す。
    /// 1件が失敗してもバッチは中断せず、`failed` に理由が入る。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_ids: Option<Vec<String>>,
    /// 書き出し先パス(ワークスペース外)。`revision_id`(単数)専用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_path: Option<String>,
    /// 書き出し先ディレクトリ(ワークスペース外、既存のディレクトリ)。
    /// `revision_ids`(複数)専用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_dir: Option<String>,
    /// `dest_dir` 内のファイル名の組み立て方。既定 `"{revision_id}.{ext}"`。
    /// `revision_ids`(複数)専用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename_template: Option<String>,
    /// 既存ファイルを上書きしてよいか。既定 false(既存なら失敗する)。
    #[serde(default)]
    pub overwrite: bool,
}

impl ExportAssetParams {
    /// 単数形の引数を作る(テスト・呼び出し側の糖衣)。
    pub fn single(revision_id: impl Into<String>, dest_path: impl Into<String>) -> Self {
        Self {
            revision_id: Some(revision_id.into()),
            revision_ids: None,
            dest_path: Some(dest_path.into()),
            dest_dir: None,
            filename_template: None,
            overwrite: false,
        }
    }

    /// バッチ引数を作る。
    pub fn batch<S: Into<String>>(
        revision_ids: impl IntoIterator<Item = S>,
        dest_dir: impl Into<String>,
    ) -> Self {
        Self {
            revision_id: None,
            revision_ids: Some(revision_ids.into_iter().map(Into::into).collect()),
            dest_path: None,
            dest_dir: Some(dest_dir.into()),
            filename_template: None,
            overwrite: false,
        }
    }

    /// `overwrite: true` にした自分を返す(テスト・呼び出し側の糖衣)。
    pub fn with_overwrite(mut self) -> Self {
        self.overwrite = true;
        self
    }

    /// `filename_template` を差し替えた自分を返す。
    pub fn with_filename_template(mut self, template: impl Into<String>) -> Self {
        self.filename_template = Some(template.into());
        self
    }
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
    /// Font family names this asset provides, present only when the imported file is a
    /// font. Use one of these verbatim in the SVG's `font-family` and reference this
    /// revision from `svg_overlay.font_revision_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_families: Option<Vec<String>>,
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

/// `detect_document` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DetectDocumentOutput {
    pub revision_id: String,
    /// 検出結果。`quad` が null なら「補正しない」で、理由は `warnings` の先頭に入る。
    pub detection: DocumentDetection,
}

/// `detect_text_blocks` の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DetectTextBlocksOutput {
    pub revision_id: String,
    /// 検出結果。ブロックが無ければ `blocks` は空で `warnings` に理由が入る。
    pub detection: TextBlockDetection,
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
    /// Rough token cost of reading this preview inline: a rule of thumb for Anthropic
    /// vision models (pixels / 750, rounded up); other hosts differ. Use it to decide
    /// whether a larger `long_edge` (or reading a long document in bands) is worth it.
    pub estimated_vision_tokens: u32,
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
    /// Hamming distance (0..=64) between the two revisions' perceptual hashes (dHash),
    /// on every layout. 0 means the thumbnails are identical; 5 or less usually means
    /// the same picture re-encoded or resized; 20 or more means different pictures.
    /// Null when either side's pixels could not be hashed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perceptual_hash_distance: Option<u32>,
    /// Structural similarity (SSIM, 0..=1) over the two images' luma. Present only with
    /// `layout: "diff"` and equal dimensions. 1.0 is pixel-identical; above ~0.98 the
    /// difference is usually invisible; below ~0.9 it is a visible change of content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssim: Option<f64>,
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

/// `export_asset`(バッチ)の1件分。単数形の出力と同じ形。
pub type ExportEntry = ExportAssetOutput;

/// `export_asset`(バッチ)の structuredContent。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExportBatchOutput {
    /// 書き出しに成功した件数(= `exported` の長さ)。
    pub count: usize,
    /// 入力順の成功結果。
    pub exported: Vec<ExportEntry>,
    /// 失敗した revision(1件の失敗でバッチは中断しない)。
    pub failed: Vec<BatchFailure>,
    /// 書き出し先ディレクトリの絶対パス。
    pub dest_dir: String,
    /// 実際に使ったファイル名テンプレート(既定を含む)。
    pub filename_template: String,
}

/// `export_asset` の structuredContent(単一 = 従来どおり / バッチ)。
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ExportResult {
    Single(Box<ExportAssetOutput>),
    Batch(Box<ExportBatchOutput>),
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
/// まず素直に deserialize し、失敗したら `operations[i]` / `layers[j].ops[k]` を
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

    // layers[j].ops[k] → operations[i] の順に、壊れている1件を名指しする。
    if let Some(layers) = object.get("layers").and_then(|l| l.as_array()) {
        for (j, layer) in layers.iter().enumerate() {
            // `Layer` の serde 名は `ops`(`operations` ではない)。
            if let Some(ops) = layer.get("ops").and_then(|o| o.as_array()) {
                if let Some((k, e)) = first_bad_operation(ops) {
                    let name = op_name_of(&ops[k]);
                    return (
                        format!("layers[{j}].ops[{k}]{}", field_suffix(&ops[k])),
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
    } else if mime_type == FONT_TTF_MIME || mime_type == FONT_OTF_MIME {
        "this is an imported font asset (.ttf/.otf), not an image; list it in {\"op\": \"svg_overlay\", \"render_text\": true, \"font_revision_ids\": [\"...\"]} so the SVG's text can be drawn with it, instead of inspecting it"
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

/// パスの拡張子が `ext` か(大文字小文字を無視)。
fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// .cube / SVG として取り込もうとしたファイルの中身が不正なときの構造化エラー。
fn invalid_asset(path: &Path, kind: &str, reason: &str) -> CallToolResult {
    tool_error(
        "invalid_asset",
        format!(
            "{} looks like {kind} asset but its content is invalid: {reason}",
            path.display()
        ),
        serde_json::json!({
            "path": path.to_string_lossy(),
            "reason": reason,
            "recovery": "check that the file really is the asset its extension says (a text .cube LUT, a plain .svg, or a TrueType/OpenType .ttf/.otf font), or rename it if it is a raster image",
        }),
    )
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

/// 2 枚の画像の知覚ハッシュ(dHash)のハミング距離。
///
/// 画素をデコードできない/寸法 0 のときは `None`(「距離 0」= 同一と誤読させない)。
fn perceptual_distance(a: &image::DynamicImage, b: &image::DynamicImage) -> Option<u32> {
    let hash = |img: &image::DynamicImage| -> Option<u64> {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        Some(atx_core::dhash_rgba8(rgba.as_raw(), w, h))
    };
    Some(atx_core::hamming(hash(a)?, hash(b)?))
}

/// 知覚ハッシュ距離の読み方(テキストサマリに添える一句)。
fn hash_distance_hint(distance: u32) -> &'static str {
    if distance == 0 {
        "(identical thumbnails)"
    } else if distance <= 5 {
        "(<=5 usually means the same picture re-encoded or resized)"
    } else if distance < 20 {
        "(a visible change of content, but the same subject)"
    } else {
        "(>=20 usually means two different pictures)"
    }
}

/// ファイルがフォントアセット(.ttf / .otf)かどうかを判定する。
///
/// 判定規則(どちらか一方を満たせばフォントとみなす):
/// 1. 拡張子が `.ttf` / `.otf`(大文字小文字を無視)
/// 2. 先頭 4 バイトが TrueType / OpenType の署名
///    (`00 01 00 00` = TrueType アウトライン、`OTTO` = CFF アウトライン、`true` = 旧 Apple 形式)
///
/// 署名判定を持たせてあるのは、拡張子の無いフォントを渡されても
/// 「画像としてデコードできない」ではなく「フォントアセット」として扱えるようにするため。
/// コレクション(`ttcf` = .ttc)はここでは拾わない
/// ([`atx_core::validate_font_asset`] が理由付きで拒否する経路に乗せたいので、
///  拡張子が `.ttf` / `.otf` のときだけ拾われる)。
fn looks_like_font(path: &Path, bytes: &[u8]) -> bool {
    if has_extension(path, "ttf") || has_extension(path, "otf") {
        return true;
    }
    matches!(
        bytes.get(..4),
        Some([0x00, 0x01, 0x00, 0x00]) | Some(b"OTTO") | Some(b"true")
    )
}

/// フォントアセットの MIME type。`OTTO` 署名だけが OpenType(CFF)。
fn font_mime_for(bytes: &[u8]) -> &'static str {
    if bytes.get(..4) == Some(b"OTTO") {
        FONT_OTF_MIME
    } else {
        FONT_TTF_MIME
    }
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

        // 読む**前に**サイズを検査する(セキュリティ点検、DESIGN.md §9.14)。
        // 以前は `fs::read` でファイル全体をメモリへ載せてから上限を見ていたので、
        // 巨大なファイルを指すだけでメモリと I/O を使い切らせることができた。
        // この段階では種類(画像 / .cube / SVG)がまだ決まらないので、どの種類でも
        // 超えられない上限(全種類の上限の最大値)で先に切り、種類ごとの細かい上限は
        // 従来どおり読んだ後に検査する。読み取り自体も `take` で上限 + 1 バイトに縛り、
        // 検査とのあいだにファイルが伸びても上限以上は読まない。
        let max_read = self
            .limits
            .max_bytes
            .max(MAX_CUBE_BYTES)
            .max(MAX_SVG_BYTES)
            .max(atx_core::MAX_FONT_BYTES);
        let io_error = |e: std::io::Error| {
            tool_error(
                "io_error",
                format!("failed to read {}: {e}", path.display()),
                serde_json::Value::Null,
            )
        };
        let too_large = |len: u64| {
            tool_error(
                "limit_exceeded",
                format!(
                    "{} is {len} bytes, over the {max_read} byte limit for imported files",
                    path.display()
                ),
                serde_json::json!({
                    "path": path.to_string_lossy(),
                    "byte_size": len,
                    "max_bytes": max_read,
                    "recovery": "import a smaller file (downscale or re-encode it outside the workspace first)",
                }),
            )
        };
        let declared_len = tri!(std::fs::metadata(&path), io_error).len();
        if declared_len > max_read {
            bail!(too_large(declared_len));
        }
        let bytes = {
            use std::io::Read as _;
            let file = tri!(std::fs::File::open(&path), io_error);
            let mut bytes = Vec::with_capacity(declared_len as usize);
            tri!(file.take(max_read + 1).read_to_end(&mut bytes), io_error);
            bytes
        };
        if bytes.len() as u64 > max_read {
            bail!(too_large(bytes.len() as u64));
        }

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
        // 中身が本当に LUT かを検証する(セキュリティ点検、DESIGN.md §9.14)。
        // 拡張子だけで任意内容のファイルを台帳に載せると、export の上書きと組み合わせて
        // 「任意のファイルを任意の場所へ複写する」経路になる。
        if is_cube {
            if let Err(reason) = atx_core::validate_cube_asset(&bytes) {
                bail!(invalid_asset(&path, "a .cube LUT", &reason));
            }
        }
        // ベクタアセット(v0.8: レシピから参照される SVG)も画像としては検査しない。
        // 寸法は SVG の**固有サイズ**を記録し、持たない SVG は 0x0 のままにする
        // (0x0 は「この SVG は自分では大きさを決められない」の記録でもあり、
        //  svg_overlay で width/height を書けというサインになる)。
        let mut is_svg = !is_cube && looks_like_svg(&path, &bytes);
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
        // SVG も中身を検証する(.cube と同じ理由)。拡張子が `.svg` なら不正はエラー。
        // 拡張子が無く先頭の `<svg` だけで SVG と推定したものは、解析できなければ
        // 推定の誤り(例: XMP に `<svg` を含むラスタ画像)とみなして画像として扱い直す。
        let mut svg_size = None;
        if is_svg {
            match atx_core::validate_svg_asset(&bytes) {
                Ok(size) => svg_size = size,
                Err(reason) if has_extension(&path, "svg") => {
                    bail!(invalid_asset(&path, "an SVG", &reason));
                }
                Err(_) => is_svg = false,
            }
        }
        // フォントアセット(v0.6: `svg_overlay` の文字描画が参照する .ttf / .otf)も
        // 画像としては検査しない。寸法は持たないので 0x0 で台帳に載せる。
        // 拡張子が `.ttf` / `.otf` なら検証失敗はエラー。署名だけで推定したものは、
        // 検証に落ちたら推定の誤りとみなして画像として扱い直す(SVG と同じ規律)。
        let mut is_font = !is_cube && !is_svg && looks_like_font(&path, &bytes);
        let mut font_families: Option<Vec<String>> = None;
        if is_font {
            match atx_core::validate_font_asset(&bytes) {
                Ok(info) => font_families = Some(info.families),
                Err(reason) if has_extension(&path, "ttf") || has_extension(&path, "otf") => {
                    bail!(invalid_asset(&path, "a TrueType/OpenType font", &reason));
                }
                Err(_) => is_font = false,
            }
        }
        let (mime_type, width, height) = if is_cube {
            (CUBE_MIME.to_string(), 0, 0)
        } else if is_svg {
            let (w, h) = svg_size.unwrap_or((0, 0));
            (SVG_MIME.to_string(), w, h)
        } else if is_font {
            (font_mime_for(&bytes).to_string(), 0, 0)
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
        if is_font {
            origin.insert("asset_kind".to_string(), "font".to_string());
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
                "{verb} {} as {} (SVG vector asset, {}, {size}, {} bytes). It is not a raster image: stamp it onto one with {{\"op\": \"svg_overlay\", \"svg_revision_id\": \"{}\", \"x\": 0, \"y\": 0}}. Text is rendered only with render_text:true (bundled Roboto plus any font_revision_ids); otherwise <text> is skipped, so convert text to paths.\npath: {}",
                path.display(),
                summary.revision_id,
                summary.mime_type,
                summary.byte_size,
                summary.revision_id,
                summary.path,
            )
        } else if is_font {
            format!(
                "{verb} {} as {} (font asset: families [{}]; {}, {} bytes). It is not an image: reference it from svg_overlay.font_revision_ids and use one of these names in font-family, e.g. {{\"op\": \"svg_overlay\", \"svg_revision_id\": \"rev_...\", \"x\": 0, \"y\": 0, \"render_text\": true, \"font_revision_ids\": [\"{}\"]}}.\npath: {}",
                path.display(),
                summary.revision_id,
                font_families.as_deref().unwrap_or_default().join(", "),
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
                font_families,
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
    ///
    /// `include_exif: true` のときだけ EXIF 全フィールドを `info.exif` に載せる
    /// (既定で落としているのはプライバシーのため。DESIGN.md §9.15)。
    pub fn inspect_image(&self, params: &InspectImageParams) -> CallToolResult {
        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT / SVG / フォント等)はデコードを試みず、
        // 構造化エラーで返す。
        if !is_raster_image(&revision.mime_type) {
            return not_an_image(&params.revision_id, &revision.mime_type);
        }
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        let info = tri!(
            atx_core::inspect_bytes_with(&bytes, &self.limits, params.include_exif),
            atx_error
        );
        let path = self
            .store
            .abs_path(&revision)
            .to_string_lossy()
            .into_owned();

        // 読みやすさ(sharpness)と同一性(perceptual_hash)はテキストサマリにも
        // 1 行で出す: 「ぼけているか」「さっきの画像と同じものか」は
        // structuredContent を読まずに判断できるほうが往復が減る。
        let mut extra = String::new();
        if let Some(stats) = &info.stats {
            extra.push_str(&format!("\nsharpness: {:.1}", stats.sharpness));
            extra.push_str(
                " (relative: compare against a known-good capture of the same subject;                  for documents below ~30 usually means motion blur or defocus)",
            );
        }
        if let Some(hash) = &info.perceptual_hash {
            extra.push_str(&format!(
                "\nperceptual_hash: {hash} (dHash; compare two with compare_revisions,                  a distance of 5 or less usually means the same picture)"
            ));
        }
        if let Some(entries) = &info.exif {
            extra.push_str(&format!("\nexif: {} field(s)", entries.len()));
            if !entries.is_empty() {
                let head: Vec<String> = entries
                    .iter()
                    .take(EXIF_SUMMARY_HEAD)
                    .map(|e| format!("{}.{}={}", e.ifd, e.tag, e.value))
                    .collect();
                extra.push_str(&format!("; first {}: {}", head.len(), head.join(", ")));
                if entries.len() > head.len() {
                    extra.push_str(" (see info.exif for the rest)");
                }
            }
        }

        let text = format!(
            "{}: {}x{} {} ({} bytes){}{}{}\npath: {}",
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
            extra,
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
        // atx-core はデコード時に必ず Orientation を正規化する。検出も同じ向きで行う。
        // `decode_oriented` は limits 検査つきで「デコード + 向き正規化」だけを行う
        // (`inspect_bytes` を通すと、使わない stats / sharpness / dHash のために
        // 全画素を数回走ってしまう)。
        let image = tri!(atx_core::decode_oriented(&bytes, &self.limits), atx_error);

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

    // -- 3a. detect_document ------------------------------------------------

    /// 画像内の支配的な四角形(用紙・画面・ホワイトボード)を返す(read-only、適用はしない)。
    ///
    /// `detect_tilt` → `rotate` と同じ関係を `detect_document` → `perspective` で作る。
    /// 返す `suggested_operation` はそのままレシピの先頭に貼れる形(DESIGN.md §9.12)。
    pub fn detect_document(&self, params: &DetectDocumentParams) -> CallToolResult {
        let defaults = DocumentParams::default();
        // 範囲外は「有効範囲を含む構造化エラー」で返す(エラーは教師である)。
        let min_area_ratio = match params.min_area_ratio {
            None => defaults.min_area_ratio,
            Some(v) if v.is_finite() && (MIN_AREA_RATIO_MIN..=MIN_AREA_RATIO_MAX).contains(&v) => v,
            Some(v) => {
                return tool_error(
                    "invalid_min_area_ratio",
                    format!(
                        "min_area_ratio must be within {MIN_AREA_RATIO_MIN}..={MIN_AREA_RATIO_MAX}, got {v}"
                    ),
                    serde_json::json!({
                        "given": v,
                        "min": MIN_AREA_RATIO_MIN,
                        "max": MIN_AREA_RATIO_MAX,
                        "default": defaults.min_area_ratio,
                        "recovery": "call detect_document again with min_area_ratio omitted (defaults to 0.2) or a value inside the valid range",
                    }),
                )
            }
        };

        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT / SVG 等)は detect_tilt と同じ構造化エラー。
        if !is_raster_image(&revision.mime_type) {
            return not_an_image(&params.revision_id, &revision.mime_type);
        }
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        // atx-core はデコード時に必ず Orientation を正規化する。検出も同じ向きで行う
        // (返す quad は apply_transform の最初の op が見る座標系)。
        let image = tri!(atx_core::decode_oriented(&bytes, &self.limits), atx_error);

        let detect_params = DocumentParams {
            min_area_ratio,
            ..defaults
        };
        let detection = atx_geometry::detect_document(&image, &detect_params);

        let text = match (&detection.quad, &detection.output_size_hint) {
            (Some(_), Some(hint)) => format!(
                "{}: quad found (confidence {:.2}, covers {:.0}%): paste suggested_operation as the first op of your recipe. Applying it yields a {}x{} image.",
                params.revision_id,
                detection.confidence,
                detection.area_ratio * 100.0,
                hint.width,
                hint.height,
            ),
            _ => format!(
                "{}: no quad: {}",
                params.revision_id,
                detection
                    .warnings
                    .first()
                    .map(String::as_str)
                    .unwrap_or("no dominant quadrilateral was found"),
            ),
        };
        ok_result(
            text,
            &DetectDocumentOutput {
                revision_id: params.revision_id.clone(),
                detection,
            },
        )
    }

    // -- 3c. detect_text_blocks ---------------------------------------------

    /// 文字らしいブロック(見出し・段落)を読み順に返す(read-only、適用はしない)。
    ///
    /// 「この画像の文字は今のプレビュー寸法で読めるのか、帯に割るならどこで割るのか」を
    /// 1 回で答えるためのツール(DESIGN.md §9.15)。`legibility.recommended_bands[i]` は
    /// そのまま `crop` op としてレシピに貼れる。
    pub fn detect_text_blocks(&self, params: &DetectTextBlocksParams) -> CallToolResult {
        let defaults = TextBlockParams::default();
        // 範囲外は「有効範囲を含む構造化エラー」で返す(エラーは教師である)。
        let max_blocks = match params.max_blocks {
            None => defaults.max_blocks,
            Some(v) if (MAX_TEXT_BLOCKS_MIN..=MAX_TEXT_BLOCKS_MAX).contains(&v) => v,
            Some(v) => {
                return tool_error(
                    "invalid_max_blocks",
                    format!(
                        "max_blocks must be within {MAX_TEXT_BLOCKS_MIN}..={MAX_TEXT_BLOCKS_MAX}, got {v}"
                    ),
                    serde_json::json!({
                        "given": v,
                        "min": MAX_TEXT_BLOCKS_MIN,
                        "max": MAX_TEXT_BLOCKS_MAX,
                        "default": defaults.max_blocks,
                        "recovery": "call detect_text_blocks again with max_blocks omitted (defaults to 32) or a value inside the valid range",
                    }),
                )
            }
        };
        let min_block_area_ratio = match params.min_block_area_ratio {
            None => defaults.min_block_area_ratio,
            Some(v)
                if v.is_finite()
                    && (MIN_BLOCK_AREA_RATIO_MIN..=MIN_BLOCK_AREA_RATIO_MAX).contains(&v) =>
            {
                v
            }
            Some(v) => {
                return tool_error(
                    "invalid_min_block_area_ratio",
                    format!(
                        "min_block_area_ratio must be within {MIN_BLOCK_AREA_RATIO_MIN}..={MIN_BLOCK_AREA_RATIO_MAX}, got {v}"
                    ),
                    serde_json::json!({
                        "given": v,
                        "min": MIN_BLOCK_AREA_RATIO_MIN,
                        "max": MIN_BLOCK_AREA_RATIO_MAX,
                        "default": defaults.min_block_area_ratio,
                        "recovery": "call detect_text_blocks again with min_block_area_ratio omitted (defaults to 0.00005) or a value inside the valid range",
                    }),
                )
            }
        };

        let revision = tri!(self.store.get_revision(&params.revision_id), store_error);
        // 画像でない revision(.cube LUT / SVG / フォント等)は detect_document と同じ
        // 構造化エラー。
        if !is_raster_image(&revision.mime_type) {
            return not_an_image(&params.revision_id, &revision.mime_type);
        }
        let bytes = tri!(self.store.read_bytes(&params.revision_id), store_error);
        // detect_tilt / detect_document と同じく、EXIF Orientation 正規化後の画像で検出する
        // (返す rect は apply_transform の最初の op が見る座標系)。
        let image = tri!(atx_core::decode_oriented(&bytes, &self.limits), atx_error);

        let detect_params = TextBlockParams {
            max_blocks,
            min_block_area_ratio,
            ..defaults
        };
        let detection = atx_geometry::detect_text_blocks(&image, &detect_params);

        let text = match (&detection.median_line_height_px, &detection.legibility) {
            (Some(line_height), Some(legibility)) => {
                let bands = legibility.recommended_bands.len();
                // 切り方(strategy)で次の一手の言い方を変える。横に広い画像では
                // 横帯が効かないので「ブロック切り出し」と名指しする。
                let plan = match legibility.strategy.as_str() {
                    "blocks" => format!(
                        "read in {bands} block crops - paste legibility.recommended_bands[i] as a crop op before render_preview long_edge 1568"
                    ),
                    "bands" => format!(
                        "read in {bands} band{} - paste legibility.recommended_bands[i] as a crop op before render_preview long_edge 1568",
                        if bands == 1 { "" } else { "s" }
                    ),
                    _ => "large enough to read in one pass: render_preview with long_edge 1568"
                        .to_string(),
                };
                format!(
                    "{}: {} text-like block(s) (covering {:.0}% of the frame), median line height {line_height}px = {:.0}px at long_edge 1568: {plan}",
                    params.revision_id,
                    detection.blocks.len(),
                    detection.text_like_area_ratio * 100.0,
                    legibility.line_height_at_1568_px,
                )
            }
            // 「文字なし」だけでは手がかりが無いので、後続の警告(インクは多いが
            // 横書きの行が無い)をサマリにも併記する。
            _ => {
                let hint = detection
                    .warnings
                    .iter()
                    .find(|w| w.starts_with("dense ink but no horizontal text lines"))
                    .map(|w| format!(" - {w}"))
                    .unwrap_or_default();
                format!("{}: no text-like regions found{hint}", params.revision_id)
            }
        };
        ok_result(
            text,
            &DetectTextBlocksOutput {
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
            text.push_str(
                "\nPresets (pass preset=<name> instead of recipe, or inline one inside a recipe with {\"op\":\"preset\",\"name\":\"...\"} - see explain_operation \"preset\"):",
            );
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
        } else if trimmed == PRESET_MACRO_OP {
            // プリセットマクロは MCP 層だけの糖衣なので OPERATIONS 表には入れず、
            // ここで別経路で説明する(unknown 扱いにはしない)。
            &PRESET_MACRO_DOC
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
        // atx-core はデコード時に必ず Orientation を焼き込むので、マスクも同じ向き
        // ・同じ寸法(= 実効寸法)で作る。そうでないと op 側で寸法が食い違う。
        let image = tri!(atx_core::decode_oriented(&bytes, &self.limits), atx_error).to_rgb8();

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
        let (recipe, expansions) =
            match resolve_recipe(params.recipe.as_ref(), params.preset.as_deref()) {
                Ok(resolved) => resolved,
                Err(result) => return result,
            };
        // validate エラーは展開後の添字を指すので、マクロ由来なら展開元プリセット名を添える。
        if let Err(e) = atx_core::recipe::validate(&recipe) {
            return expansions.annotate(atx_error(e));
        }
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

    /// レシピを適用したうえで長辺 ≤ `long_edge`(既定 768)に縮小し、jpeg で inline 返却する。
    ///
    /// `long_edge` は 256..=[`PREVIEW_LONG_EDGE_MAX`]。文字を読ませたいときは
    /// VLM が受け取れる上限まで上げる(DESIGN.md §9.12)。既定値は変えていない
    /// (既存の挙動と eval の互換のため)。
    ///
    /// # コスト
    ///
    /// v1 はレシピを**フル解像度で**適用してから縮小する(単一パス: レシピの
    /// encode op だけを差し替え、末尾に `resize(contain 768) + encode(jpeg 80)` を足す)。
    /// 事前縮小してから適用する最適化はしていないため、プレビューでも
    /// `apply_transform` と同等の画素処理コストがかかる。
    /// 代わりに「プレビューで見た構図 = 本適用の構図」が厳密に一致する。
    pub fn render_preview(&self, params: &RenderPreviewParams) -> CallToolResult {
        let (recipe, expansions) =
            match resolve_recipe(params.recipe.as_ref(), params.preset.as_deref()) {
                Ok(resolved) => resolved,
                Err(result) => return result,
            };
        if let Err(e) = atx_core::recipe::validate(&recipe) {
            return expansions.annotate(atx_error(e));
        }
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
        // 長辺は 256..=1568。範囲外は有効範囲を添えた構造化エラーで返す。
        let long_edge = match params.long_edge {
            None => PREVIEW_LONG_EDGE,
            Some(v) if (PREVIEW_LONG_EDGE_MIN..=PREVIEW_LONG_EDGE_MAX).contains(&v) => v,
            Some(v) => {
                return tool_error(
                    "invalid_long_edge",
                    format!(
                        "long_edge must be within {PREVIEW_LONG_EDGE_MIN}..={PREVIEW_LONG_EDGE_MAX}, got {v}"
                    ),
                    serde_json::json!({
                        "given": v,
                        "min": PREVIEW_LONG_EDGE_MIN,
                        "max": PREVIEW_LONG_EDGE_MAX,
                        "default": PREVIEW_LONG_EDGE,
                        "recovery": "call render_preview again with long_edge omitted (defaults to 768) or a value inside the valid range; 1568 is the largest inline image a vision model can use",
                    }),
                )
            }
        };
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

        let preview_recipe = preview_recipe_of(&recipe, long_edge);
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
            long_edge,
        );
        let path = tri!(
            self.store.put_preview(&key, "jpg", &final_bytes),
            store_error
        );
        let path = path.to_string_lossy().into_owned();

        let estimated_vision_tokens = estimated_vision_tokens(output.width, output.height);
        let text = format!(
            "Preview of {} with this {}recipe: {}x{} jpeg ({} bytes, long edge <= {}) (~{estimated_vision_tokens} vision tokens).{} This is a downscaled proof; call apply_transform with the same recipe for the full-resolution revision.{}\npath: {}",
            params.revision_id,
            preset_note(params.preset.as_deref()),
            output.width,
            output.height,
            final_bytes.len(),
            long_edge,
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
                estimated_vision_tokens,
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

        // 知覚ハッシュ距離は全 layout で返す(「同じ絵か」は並べ方と無関係な問い)。
        // 縮小前の画素で計算する: dHash は自前で 9x8 に潰すので、
        // プレビュー用の縮小を先に掛けると結果が縮小の丸めに依存してしまう。
        let perceptual_hash_distance = perceptual_distance(&img_a, &img_b);

        if params.layout == CompareLayout::Diff {
            return self.compare_revisions_diff(
                params,
                &rev_a,
                &rev_b,
                img_a,
                img_b,
                perceptual_hash_distance,
            );
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
        let text = match perceptual_hash_distance {
            Some(d) => format!("{text}\nhash distance {d} {}", hash_distance_hint(d)),
            None => text,
        };
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
                perceptual_hash_distance,
                ssim: None,
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
        perceptual_hash_distance: Option<u32>,
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
        // SSIM は寸法一致が前提なので diff レイアウトでだけ計算できる。
        // グレー化は atx-core の輝度定義(`gray_from_rgb8`)で行う:
        // `image` の `to_luma8()` は丸めが違うので混ぜると値が揺れる。
        // RGB8 バッファを直接渡す(以前はフル解像度を clone して RGBA8 へ広げていたが、
        // 輝度は R/G/B からだけ作るのでアルファ列は 1 ビットも使われていなかった)。
        let ssim = {
            let (w, h) = full_a.dimensions();
            match (
                atx_core::similarity::gray_from_rgb8(full_a.as_raw(), w, h),
                atx_core::similarity::gray_from_rgb8(full_b.as_raw(), w, h),
            ) {
                (Some(ga), Some(gb)) => atx_core::ssim_gray(&ga, &gb),
                _ => None,
            }
        };
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
        let mut text = text;
        if let Some(d) = perceptual_hash_distance {
            text.push_str(&format!("\nhash distance {d} {}", hash_distance_hint(d)));
        }
        if let Some(v) = ssim {
            text.push_str(&format!(
                "\nSSIM {v:.4} (1.0 = identical; above ~0.98 the difference is usually invisible, below ~0.9 it is a visible change)"
            ));
        }
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
                perceptual_hash_distance,
                ssim,
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
    ///
    /// 単数形(`revision_id` + `dest_path`)と複数形(`revision_ids` + `dest_dir`)は排他で、
    /// どちらか一方が必須。複数形でも各件は単数形とまったく同じ検査
    /// (DESIGN.md §9.14: ワークスペース内拒否・symlink 拒否・create_new・temp+rename)を通る。
    pub fn export_asset(&self, params: &ExportAssetParams) -> CallToolResult {
        match (params.revision_id.as_deref(), params.revision_ids.as_deref()) {
            (Some(revision_id), None) => {
                if params.dest_dir.is_some() || params.filename_template.is_some() {
                    return export_params_mismatch(
                        "revision_id",
                        "dest_dir / filename_template",
                        "dest_path",
                    );
                }
                let Some(dest_path) = params.dest_path.as_deref() else {
                    return export_params_mismatch("revision_id", "no dest_path", "dest_path");
                };
                let dest = match absolutize(dest_path) {
                    Ok(p) => p,
                    Err(e) => {
                        return tool_error(
                            "invalid_dest_path",
                            format!("cannot resolve dest_path {dest_path:?}: {e}"),
                            serde_json::json!({ "dest_path": dest_path }),
                        )
                    }
                };
                match self.export_one(revision_id, &dest, params.overwrite) {
                    Ok((output, text)) => {
                        ok_result(text, &ExportResult::Single(Box::new(output)))
                    }
                    Err(result) => result,
                }
            }
            (None, Some(revision_ids)) => self.export_batch(revision_ids, params),
            (Some(_), Some(_)) => tool_error(
                "revision_id_and_revision_ids_conflict",
                "revision_id and revision_ids are mutually exclusive, but both were given",
                serde_json::json!({
                    "recovery": "pass revision_id + dest_path for one file, or revision_ids + dest_dir for a batch of up to 64, not both",
                }),
            ),
            (None, None) => tool_error(
                "revision_id_or_revision_ids_required",
                "one of revision_id (a single file) or revision_ids (a batch of up to 64) is required",
                serde_json::json!({
                    "recovery": "pass revision_id = \"rev_...\" with dest_path = \"/abs/out.jpg\", or revision_ids = [\"rev_...\", ...] with dest_dir = \"/abs/dir\"; call list_assets to see the available revision_ids",
                }),
            ),
        }
    }

    /// 複数 revision を1つのディレクトリへ書き出す。入力順を保ち、失敗しても続行する。
    ///
    /// 書き込みを1件も始める前に、ディレクトリの検査とファイル名の衝突検査を済ませる
    /// (途中まで書いてから「名前が衝突していた」と分かるのが最悪なので)。
    fn export_batch(&self, revision_ids: &[String], params: &ExportAssetParams) -> CallToolResult {
        if params.dest_path.is_some() {
            return export_params_mismatch("revision_ids", "dest_path", "dest_dir");
        }
        if let Err(result) = check_batch_size(revision_ids.len(), "revision_ids") {
            return result;
        }
        let Some(dest_dir) = params.dest_dir.as_deref() else {
            return export_params_mismatch("revision_ids", "no dest_dir", "dest_dir");
        };
        let template = params
            .filename_template
            .as_deref()
            .unwrap_or(DEFAULT_FILENAME_TEMPLATE);
        if let Err(result) = validate_filename_template(template) {
            return result;
        }

        let dir = match absolutize(dest_dir) {
            Ok(p) => p,
            Err(e) => {
                return tool_error(
                    "invalid_dest_path",
                    format!("cannot resolve dest_dir {dest_dir:?}: {e}"),
                    serde_json::json!({ "dest_dir": dest_dir }),
                )
            }
        };
        if let Err(result) = self.check_dest_inside_workspace(&dir, "dest_dir") {
            return result;
        }
        let dir_meta = std::fs::symlink_metadata(&dir).ok();
        if dir_meta
            .as_ref()
            .is_some_and(|m| m.file_type().is_symlink())
        {
            return tool_error(
                "dest_is_symlink",
                format!(
                    "{} is a symbolic link; exporting through a link is not allowed",
                    dir.display()
                ),
                serde_json::json!({
                    "dest_dir": dir.to_string_lossy(),
                    "recovery": "pass the real directory path you want to write into (not a symbolic link), or remove the link first",
                }),
            );
        }
        match dir_meta {
            None => {
                return tool_error(
                    "dest_dir_missing",
                    format!("directory {} does not exist", dir.display()),
                    serde_json::json!({
                        "dest_dir": dir.to_string_lossy(),
                        "recovery": "create the directory first, or pass an existing one",
                    }),
                )
            }
            Some(meta) if !meta.is_dir() => {
                return tool_error(
                    "dest_dir_not_a_directory",
                    format!("{} is not a directory", dir.display()),
                    serde_json::json!({
                        "dest_dir": dir.to_string_lossy(),
                        "recovery": "pass a directory for dest_dir (use revision_id + dest_path to write a single file)",
                    }),
                )
            }
            Some(_) => {}
        }

        // --- 書き込み前: ファイル名を全件組み立て、衝突を検出する ---
        //
        // 台帳(assets.jsonl)の走査はここで **1 回だけ**行い、revision の取得も
        // `{stem}` の系譜辿りもこの写像の中で済ませる。以前は 1 件ごとに
        // `get_revision` が、さらに `{stem}` の有無に関わらず系譜の 1 段ごとに
        // もう 1 回、台帳を全走査していた。
        let ledger = match self.store.list_revisions(None) {
            Ok(ledger) => ledger,
            Err(e) => return store_error(e),
        };
        let by_id: BTreeMap<&str, &AssetRevision> =
            ledger.iter().map(|r| (r.revision_id.as_str(), r)).collect();

        let width = revision_ids.len().to_string().len();
        let mut planned: Vec<(String, String)> = Vec::new(); // (revision_id, file name)
        let mut failed: Vec<BatchFailure> = Vec::new();
        for (i, revision_id) in revision_ids.iter().enumerate() {
            let Some(revision) = by_id.get(revision_id.as_str()).copied() else {
                failed.push(BatchFailure {
                    path: revision_id.clone(),
                    error: error_info(&store_error(StoreError::RevisionNotFound(
                        revision_id.clone(),
                    ))),
                });
                continue;
            };
            let name = match render_filename(template, revision, i + 1, width, &by_id) {
                Ok(name) => name,
                Err(result) => return result,
            };
            planned.push((revision_id.clone(), name));
        }
        let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (revision_id, name) in &planned {
            seen.entry(name.as_str()).or_default().push(revision_id);
        }
        let duplicates: Vec<(&&str, &Vec<&str>)> =
            seen.iter().filter(|(_, ids)| ids.len() > 1).collect();
        if !duplicates.is_empty() {
            let listed: Vec<String> = duplicates
                .iter()
                .map(|(name, ids)| format!("{name} ({})", ids.join(", ")))
                .collect();
            return tool_error(
                "filename_collision",
                format!(
                    "filename_template {template:?} maps more than one revision to the same file name: {}",
                    listed.join("; ")
                ),
                serde_json::json!({
                    "filename_template": template,
                    "duplicates": duplicates.iter().map(|(name, ids)| serde_json::json!({ "file_name": name, "revision_ids": ids })).collect::<Vec<_>>(),
                    "recovery": format!("include {{revision_id}} or {{index}} in filename_template so every entry gets its own name (nothing was written; the default is {DEFAULT_FILENAME_TEMPLATE:?})"),
                }),
            );
        }

        // --- 書き込み: 各件は単数形とまったく同じ経路(§9.14 の全検査)を通る ---
        let mut exported: Vec<ExportEntry> = Vec::new();
        for (revision_id, name) in &planned {
            match self.export_one(revision_id, &dir.join(name), params.overwrite) {
                Ok((output, _)) => exported.push(output),
                Err(result) => failed.push(BatchFailure {
                    path: revision_id.clone(),
                    error: error_info(&result),
                }),
            }
        }

        if exported.is_empty() {
            let listed: Vec<String> = failed
                .iter()
                .map(|f| format!("- {}: [{}] {}", f.path, f.error.code, f.error.message))
                .collect();
            return tool_error(
                "export_failed",
                format!(
                    "all {} exports failed:\n{}",
                    failed.len(),
                    listed.join("\n")
                ),
                serde_json::json!({
                    "failed": failed,
                    "dest_dir": dir.to_string_lossy(),
                    "recovery": "fix the revision_ids or the destination above and call export_asset again; call list_assets to see the available revision_ids",
                }),
            );
        }

        let overwritten = exported.iter().filter(|e| e.overwritten).count();
        let mut text = format!(
            "Exported {} of {} revision(s) to {}{}{}",
            exported.len(),
            revision_ids.len(),
            dir.display(),
            if overwritten > 0 {
                format!(" ({overwritten} overwrote an existing file)")
            } else {
                String::new()
            },
            if failed.is_empty() {
                String::new()
            } else {
                format!(", {} failed", failed.len())
            },
        );
        for entry in &exported {
            text.push_str(&format!(
                "\n- {} -> {} ({} bytes){}",
                entry.revision_id,
                entry.path,
                entry.byte_size,
                if entry.overwritten {
                    " [overwrote]"
                } else {
                    ""
                },
            ));
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
            &ExportResult::Batch(Box::new(ExportBatchOutput {
                count: exported.len(),
                exported,
                failed,
                dest_dir: dir.to_string_lossy().into_owned(),
                filename_template: template.to_string(),
            })),
        )
    }

    /// 書き出し先がワークスペース内なら構造化エラー。
    ///
    /// ワークスペース root 配下への書き込みは**一律に**拒否する。
    /// objects / previews だけでなく台帳(assets.jsonl)も、将来増えるファイルも同じ:
    /// 不変ストアの管理領域へ export するのが正当なケースは存在しない。
    ///
    /// 比較は**両側を canonicalize してから**行う: macOS の /tmp は
    /// /private/tmp へのシンボリックリンクなので、文字列比較だけでは
    /// シンボリックリンク経由のパスが素通りしてしまう。
    fn check_dest_inside_workspace(&self, dest: &Path, field: &str) -> Result<(), CallToolResult> {
        let ws_root = real_prefix(self.store.root());
        let dest_real = real_prefix(dest);
        if !dest_real.starts_with(&ws_root) {
            return Ok(());
        }
        Err(tool_error(
            "dest_inside_workspace",
            format!(
                "{} is inside the immutable workspace store; exporting there is not allowed",
                dest.display()
            ),
            serde_json::json!({
                field: dest.to_string_lossy(),
                "resolved_dest_path": dest_real.to_string_lossy(),
                "workspace": ws_root.to_string_lossy(),
                "recovery": "choose a destination outside the workspace directory entirely (objects/, previews/ and the assets.jsonl ledger all live there)",
            }),
        ))
    }

    /// 1 revision を1つの絶対パスへ書き出す。成功なら `(structuredContent, テキストサマリ)`、
    /// 失敗なら単一呼び出しがそのまま返せる構造化エラー。
    ///
    /// DESIGN.md §9.14 の検査順(ワークスペース内 → symlink → 既存 → ディレクトリ →
    /// 親の存在 → create_new / temp+rename)はここに1本化してある。
    fn export_one(
        &self,
        revision_id: &str,
        dest: &Path,
        overwrite: bool,
    ) -> Result<(ExportAssetOutput, String), CallToolResult> {
        macro_rules! tri {
            ($expr:expr, $map:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(e) => return Err($map(e)),
                }
            };
        }

        let revision = tri!(self.store.get_revision(revision_id), store_error);
        let dest = dest.to_path_buf();

        self.check_dest_inside_workspace(&dest, "dest_path")?;

        // 書き出し先そのものがシンボリックリンクなら拒否する(セキュリティ点検、DESIGN.md §9.14)。
        // リンク先が存在しない(宙ぶらりんの)リンクは `exists()` が false になり、
        // 上のワークスペース内判定もリンク名の側で行われるので素通りしていた。
        // その状態で書くとリンクを辿って objects/ 等にファイルが作られ、不変ストアが壊れる。
        // リンク先を検査してから辿る方式は検査と書き込みの間に差し替えられうるので、
        // 「リンクには書かない」と決めてしまう方が単純で強い。
        let dest_meta = std::fs::symlink_metadata(&dest).ok();
        if dest_meta
            .as_ref()
            .is_some_and(|m| m.file_type().is_symlink())
        {
            return Err(tool_error(
                "dest_is_symlink",
                format!(
                    "{} is a symbolic link; exporting through a link is not allowed",
                    dest.display()
                ),
                serde_json::json!({
                    "dest_path": dest.to_string_lossy(),
                    "recovery": "pass the real file path you want to write (not a symbolic link), or remove the link first",
                }),
            ));
        }

        let exists = dest_meta.is_some();
        if exists && !overwrite {
            return Err(tool_error(
                "dest_exists",
                format!(
                    "{} already exists; refusing to overwrite it",
                    dest.display()
                ),
                serde_json::json!({
                    "dest_path": dest.to_string_lossy(),
                    "recovery": "ask the user to confirm, then call export_asset again with overwrite=true, or pick a different dest_path",
                }),
            ));
        }
        if exists && dest.is_dir() {
            return Err(tool_error(
                "dest_is_directory",
                format!("{} is a directory", dest.display()),
                serde_json::json!({ "dest_path": dest.to_string_lossy(), "recovery": "pass a full file path including the file name" }),
            ));
        }
        if let Some(parent) = dest.parent() {
            if !parent.exists() {
                return Err(tool_error(
                    "dest_parent_missing",
                    format!("directory {} does not exist", parent.display()),
                    serde_json::json!({
                        "dest_path": dest.to_string_lossy(),
                        "recovery": "create the directory first, or choose an existing one",
                    }),
                ));
            }
        }

        let bytes = tri!(self.store.read_bytes(revision_id), store_error);
        tri!(write_export(&dest, &bytes, exists), |e: std::io::Error| {
            tool_error(
                "io_error",
                format!("failed to write {}: {e}", dest.display()),
                serde_json::Value::Null,
            )
        });

        let path = dest.to_string_lossy().into_owned();
        let text = format!(
            "Exported {} ({}x{} {}, {} bytes) to {}{}",
            revision_id,
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
        Ok((
            ExportAssetOutput {
                revision_id: revision_id.to_string(),
                path,
                byte_size: bytes.len() as u64,
                overwritten: exists,
            },
            text,
        ))
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
/// プリセットマクロ(`{"op":"preset","name":"..."}`)1件の展開跡。
///
/// 展開後のレシピは core にとっては「ただの op 列」なので、validate エラーの位置
/// (`operations[5]` 等)がどのマクロ由来なのかは core からは分からない。
/// 展開時にこの対応表を残しておき、エラーメッセージに展開元のプリセット名を付け直す。
#[derive(Debug, Clone)]
struct PresetExpansion {
    /// 展開した配列の位置。`"operations"` または `"layers[<j>].ops"`。
    path: String,
    /// 展開後の添字範囲(`start..end`)。
    start: usize,
    end: usize,
    /// 展開元のプリセット名。
    name: String,
}

/// 1レシピ分の展開跡(マクロを使っていなければ空)。
#[derive(Debug, Clone, Default)]
struct PresetExpansions(Vec<PresetExpansion>);

impl PresetExpansions {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// エラーメッセージが指す位置が展開範囲の中なら、展開元プリセット名を教える。
    fn preset_at(&self, path: &str, index: usize) -> Option<&str> {
        self.0
            .iter()
            .find(|e| e.path == path && (e.start..e.end).contains(&index))
            .map(|e| e.name.as_str())
    }

    /// 構造化エラーの message 末尾に ` (expanded from preset "<name>")` を付け直す。
    ///
    /// 展開後の添字を指すエラー(core の validate / レシピ deserialize)は、
    /// そのままではエージェントが自分の書いた JSON の中に該当する op を見つけられない。
    fn annotate(&self, result: CallToolResult) -> CallToolResult {
        if self.is_empty() {
            return result;
        }
        let Some(payload) = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .find_map(|t| serde_json::from_str::<serde_json::Value>(&t.text).ok())
        else {
            return result;
        };
        let error = &payload["error"];
        let (Some(code), Some(message)) = (error["code"].as_str(), error["message"].as_str())
        else {
            return result;
        };
        let Some((path, index)) = parse_error_location(message) else {
            return result;
        };
        let Some(name) = self.preset_at(&path, index) else {
            return result;
        };
        let mut details = error["details"].clone();
        if let Some(object) = details.as_object_mut() {
            object.insert(
                "expanded_from_preset".to_string(),
                serde_json::Value::from(name),
            );
        }
        tool_error(
            code,
            format!("{message} (expanded from preset {name:?})"),
            details,
        )
    }
}

/// エラーメッセージから `(配列の位置, 添字)` を読み取る。
///
/// 見る文言は 2 系統ある。どちらも**先頭に前置き**が付く
/// (`"invalid recipe: "` / `"invalid recipe at "`)ので、位置は文中から探す:
///
/// | 出どころ | 例 |
/// |---|---|
/// | core の validate(トップレベル) | `invalid recipe: operations[3] (encode): ...` |
/// | core の validate(レイヤー内) | `invalid recipe: layers[1].ops: operations[0] (blur): ...` |
/// | core の validate(レイヤー内・直接) | `invalid recipe: layers[1].ops[1] (encode): ...` |
/// | [`locate_recipe_error`] | `invalid recipe at operations[3].width: ...` / `... at layers[1].ops[0]: ...` |
///
/// 返す位置は [`PresetExpansion::path`] と同じ綴り(`"operations"` または
/// `"layers[<j>].ops"`)。以前は先頭が `layers[` で始まるかどうかで分岐していたため
/// レイヤー分岐が一度も成立せず、レイヤー内のエラーをトップレベルの添字と
/// 読み違えていた(無関係なプリセットの名前が付いていた)。
fn parse_error_location(message: &str) -> Option<(String, usize)> {
    if let Some(at) = message.find("layers[") {
        let rest = &message[at + "layers[".len()..];
        let (layer, rest) = rest.split_once(']')?;
        // 数字でなければ位置表記ではない(例: 散文中の "layers[...]")。
        layer.parse::<usize>().ok()?;
        let rest = rest.strip_prefix(".ops")?;
        let path = format!("layers[{layer}].ops");
        // `layers[j].ops[k]` なら直後の添字、`layers[j].ops: operations[k]` なら後者。
        if let Some(tail) = rest.strip_prefix('[') {
            let (index, _) = tail.split_once(']')?;
            return Some((path, index.parse().ok()?));
        }
        return Some((path, operations_index(rest)?));
    }
    Some(("operations".to_string(), operations_index(message)?))
}

/// 文中の最初の `operations[<i>]` の添字。
fn operations_index(message: &str) -> Option<usize> {
    let at = message.find("operations[")?;
    let tail = &message[at + "operations[".len()..];
    let (index, _) = tail.split_once(']')?;
    index.parse().ok()
}

/// レシピ JSON の中の `{"op":"preset","name":"<preset>"}` をその場で展開する。
///
/// マクロは **MCP 層だけの糖衣**で、atx-core の DSL には存在しない。展開後のレシピで
/// ハッシュを取るので「マクロで書いたレシピ」「手で展開したレシピ」「`preset` 引数で
/// 呼んだ場合」の 3 つは同じ revision に落ちる。
/// `operations[]` と `layers[*].ops[]` の両方を走査する。
fn expand_preset_macros(value: &mut serde_json::Value) -> Result<PresetExpansions, CallToolResult> {
    let mut expansions: Vec<PresetExpansion> = Vec::new();
    let Some(object) = value.as_object_mut() else {
        return Ok(PresetExpansions(expansions));
    };
    if let Some(layers) = object.get_mut("layers").and_then(|l| l.as_array_mut()) {
        for (j, layer) in layers.iter_mut().enumerate() {
            if let Some(ops) = layer.get_mut("ops").and_then(|o| o.as_array_mut()) {
                expand_preset_macros_in(ops, &format!("layers[{j}].ops"), &mut expansions)?;
            }
        }
    }
    if let Some(ops) = object.get_mut("operations").and_then(|o| o.as_array_mut()) {
        expand_preset_macros_in(ops, "operations", &mut expansions)?;
    }
    Ok(PresetExpansions(expansions))
}

/// op 配列1本のマクロ展開(splice)。
fn expand_preset_macros_in(
    ops: &mut Vec<serde_json::Value>,
    path: &str,
    expansions: &mut Vec<PresetExpansion>,
) -> Result<(), CallToolResult> {
    if !ops
        .iter()
        .any(|op| op.get("op").and_then(|v| v.as_str()) == Some(PRESET_MACRO_OP))
    {
        return Ok(());
    }
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        if op.get("op").and_then(|v| v.as_str()) != Some(PRESET_MACRO_OP) {
            out.push(op.clone());
            continue;
        }
        let Some(name) = op.get("name").and_then(|v| v.as_str()) else {
            return Err(tool_error(
                "preset_macro_missing_name",
                format!(
                    "{path}[{i}] is a preset macro but has no \"name\": write {{\"op\": \"preset\", \"name\": \"<preset>\"}}"
                ),
                serde_json::json!({
                    "location": format!("{path}[{i}]"),
                    "valid_presets": crate::presets::preset_names(),
                    "recovery": "add \"name\": \"<one of valid_presets>\" to that operation, or drop the macro and write the operations out",
                }),
            ));
        };
        let preset = match crate::presets::resolve(name) {
            Ok(preset) => preset,
            Err(crate::presets::PresetError::Unknown) => return Err(unknown_preset_error(name)),
            Err(crate::presets::PresetError::Malformed(reason)) => {
                return Err(preset_malformed_error(name, &reason))
            }
        };
        let (start, end) = inline_preset(name, &preset.recipe, path, i, &mut out)?;
        expansions.push(PresetExpansion {
            path: path.to_string(),
            start,
            end,
            name: name.to_string(),
        });
    }
    *ops = out;
    Ok(())
}

/// 解決済みプリセットの op 列を `out` の末尾へ差し込み、`(start, end)` を返す。
///
/// `layers` を持つプリセットは「レシピまるごと」なので op 列には差し込めない
/// (差し込めるのは `operations` だけで、レイヤー段は落ちてしまう)。
fn inline_preset(
    name: &str,
    recipe: &TransformRecipe,
    path: &str,
    macro_index: usize,
    out: &mut Vec<serde_json::Value>,
) -> Result<(usize, usize), CallToolResult> {
    if recipe.layers.is_some() {
        return Err(tool_error(
            "preset_not_inlinable",
            format!(
                "preset {name:?} carries a layers stack, so it cannot be inlined as one operation inside {path}"
            ),
            serde_json::json!({
                "preset": name,
                "location": format!("{path}[{macro_index}]"),
                "recovery": format!("pass preset=\"{name}\" as the top-level preset instead"),
            }),
        ));
    }
    let start = out.len();
    for op in &recipe.operations {
        match serde_json::to_value(op) {
            Ok(value) => out.push(value),
            Err(e) => {
                return Err(tool_error(
                    "internal_serialization_failed",
                    format!("failed to expand preset {name:?}: {e}"),
                    serde_json::Value::Null,
                ))
            }
        }
    }
    Ok((start, out.len()))
}

/// 未知のプリセット名の構造化エラー(`preset` 引数とマクロで同じ形を使う)。
fn unknown_preset_error(name: &str) -> CallToolResult {
    tool_error(
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
    )
}

/// 埋め込みプリセット JSON が壊れている(ビルド時のバグ)場合の構造化エラー。
fn preset_malformed_error(name: &str, reason: &str) -> CallToolResult {
    tool_error(
        "preset_malformed",
        format!("built-in preset {name:?} could not be parsed: {reason}"),
        serde_json::json!({
            "given": name,
            "reason": reason,
            "recovery": "this is a server bug; pass an explicit recipe instead",
        }),
    )
}

fn resolve_recipe(
    recipe: Option<&RecipeJson>,
    preset: Option<&str>,
) -> Result<(TransformRecipe, PresetExpansions), CallToolResult> {
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
        // 生レシピ: deserialize の**前に**プリセットマクロを展開する。
        // 以降(deserialize / validate / hash / 実行)は展開後のレシピしか見ない。
        (Some(recipe), None) => {
            let mut value = recipe.0.clone();
            let expansions = expand_preset_macros(&mut value)?;
            match deserialize_recipe(&RecipeJson(value)) {
                Ok(recipe) => Ok((recipe, expansions)),
                Err(result) => Err(expansions.annotate(result)),
            }
        }
        (None, Some(name)) => match crate::presets::resolve(name) {
            Ok(preset) => Ok((preset.recipe, PresetExpansions::default())),
            Err(crate::presets::PresetError::Unknown) => Err(unknown_preset_error(name)),
            Err(crate::presets::PresetError::Malformed(reason)) => {
                Err(preset_malformed_error(name, &reason))
            }
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

/// 単数形と複数形の引数を混ぜた場合の構造化エラー。
fn export_params_mismatch(given: &str, wrong: &str, expected: &str) -> CallToolResult {
    tool_error(
        "export_params_mismatch",
        format!("{given} was given with {wrong}; the {given} form takes {expected}"),
        serde_json::json!({
            "given": given,
            "expected": expected,
            "recovery": "pass revision_id + dest_path (one file), or revision_ids + dest_dir (+ optional filename_template) for a batch",
        }),
    )
}

/// `dest_dir` 内のファイル名テンプレートの既定値。
pub const DEFAULT_FILENAME_TEMPLATE: &str = "{revision_id}.{ext}";

/// `filename_template` で使えるプレースホルダ。
const FILENAME_PLACEHOLDERS: [&str; 4] = ["{revision_id}", "{index}", "{ext}", "{stem}"];

/// テンプレートそのものの検査(展開前)。
///
/// ファイル名の組み立てにパス区切りを許すと `dest_dir` の外へ書けてしまう
/// (`"../{revision_id}.{ext}"`)。テンプレートは**1つのファイル名**に限る。
fn validate_filename_template(template: &str) -> Result<(), CallToolResult> {
    let reject = |reason: &str| {
        Err(tool_error(
            "invalid_filename_template",
            format!("filename_template {template:?} is not usable: {reason}"),
            serde_json::json!({
                "given": template,
                "reason": reason,
                "default": DEFAULT_FILENAME_TEMPLATE,
                "valid_placeholders": FILENAME_PLACEHOLDERS,
                "recovery": format!("pass a plain file name built from the placeholders (no directories), for example {DEFAULT_FILENAME_TEMPLATE:?}"),
            }),
        ))
    };
    if template.trim().is_empty() {
        return reject("it is empty");
    }
    if template.contains('/') || template.contains('\\') {
        return reject("it contains a path separator; the file name must stay inside dest_dir");
    }
    if template.contains("..") {
        return reject("it contains \"..\"; the file name must stay inside dest_dir");
    }
    if template.contains('\0') {
        return reject("it contains a NUL byte");
    }
    // 未知のプレースホルダは黙って literal にせず、その場で教える。
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let tail = &rest[open..];
        let Some(close) = tail.find('}') else {
            return reject("it has an unclosed \"{\"");
        };
        let token = &tail[..=close];
        if !FILENAME_PLACEHOLDERS.contains(&token) {
            return reject(&format!("{token} is not a known placeholder"));
        }
        rest = &tail[close + 1..];
    }
    Ok(())
}

/// 展開後のファイル名の検査(`{stem}` 等の値が区切りを持ち込まないこと)。
fn check_file_name(name: &str, template: &str) -> Result<(), CallToolResult> {
    let bad = name.trim().is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
        || name == ".";
    if !bad {
        return Ok(());
    }
    Err(tool_error(
        "invalid_filename_template",
        format!(
            "filename_template {template:?} expanded to {name:?}, which is not a usable file name"
        ),
        serde_json::json!({
            "given": template,
            "expanded": name,
            "default": DEFAULT_FILENAME_TEMPLATE,
            "valid_placeholders": FILENAME_PLACEHOLDERS,
            "recovery": "the expanded name must be a single file name (no path separators, no \"..\"); use {revision_id} or {index} instead of {stem} if the source file name is unusual",
        }),
    ))
}

/// ファイル名テンプレートを1件分に展開する。
///
/// `{stem}` は**系譜の根**(`source_revision_id` を辿った import 元)の取り込み時
/// ファイル名の stem。台帳に由来情報が無ければ revision_id を使う。
/// `by_id` は呼び出し側が 1 回だけ走査した台帳(revision_id → revision)。
fn render_filename(
    template: &str,
    revision: &AssetRevision,
    index: usize,
    pad_width: usize,
    by_id: &BTreeMap<&str, &AssetRevision>,
) -> Result<String, CallToolResult> {
    let mut name = template
        .replace("{revision_id}", &revision.revision_id)
        .replace("{index}", &format!("{index:0pad_width$}"))
        .replace("{ext}", ext_for_mime(&revision.mime_type));
    // 系譜を辿るのはテンプレートが実際に `{stem}` を使うときだけ。
    if template.contains("{stem}") {
        name = name.replace("{stem}", &lineage_stem(revision, by_id));
    }
    check_file_name(&name, template)?;
    Ok(name)
}

/// 系譜の根(import 起点)の元ファイル名の stem。辿れなければ revision_id。
fn lineage_stem(revision: &AssetRevision, by_id: &BTreeMap<&str, &AssetRevision>) -> String {
    let mut current = revision;
    // 系譜は有限だが、台帳が壊れている場合に無限ループしないよう上限を置く。
    for _ in 0..64 {
        if let Some(name) = current.origin.get("file_name") {
            let stem = Path::new(name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !stem.is_empty() {
                return stem;
            }
        }
        let Some(parent) = current.source_revision_id.as_deref() else {
            break;
        };
        match by_id.get(parent) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    revision.revision_id.clone()
}

/// テキストサマリ用: プリセット由来なら `"preset \"x\" "` を、生レシピなら空文字を返す。
fn preset_note(preset: Option<&str>) -> String {
    match preset {
        Some(name) => format!("preset {name:?} "),
        None => String::new(),
    }
}

/// インライン画像1枚の概算トークン数 `ceil(width * height / 750)`。
///
/// Anthropic の vision モデル向けの目安(画素数 / 750)であり、他のホストでは違う。
/// プレビューの長辺を上げるか帯に分けるかの判断材料として返す。
fn estimated_vision_tokens(width: u32, height: u32) -> u32 {
    let pixels = u64::from(width) * u64::from(height);
    let tokens = pixels.div_ceil(750);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

/// プレビュー用レシピ: encode を落とし、末尾に `resize(contain long_edge) + jpeg` を足す。
fn preview_recipe_of(recipe: &TransformRecipe, long_edge: u32) -> TransformRecipe {
    let mut operations: Vec<Operation> = recipe
        .operations
        .iter()
        .filter(|op| !matches!(op, Operation::Encode { .. }))
        .cloned()
        .collect();
    operations.push(Operation::Resize {
        width: Some(long_edge),
        height: Some(long_edge),
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
    // ここで差し替えた「長辺 long_edge リサイズ + jpeg q80 encode」がそのまま
    // レイヤー合成後の縮小プレビューになる。layers を落とすと、
    // ユーザが layers で意図した合成そのものがプレビューから消えてしまう。
    TransformRecipe {
        operations,
        layers: recipe.layers.clone(),
    }
}

/// プレビューのキャッシュキー:
/// sha256(source_revision + recipe_hash + overlay + mask_revision_id + long_edge) の先頭 32 文字。
///
/// `overlay` をハッシュ入力に含めることで、同じ (revision, recipe) でも
/// overlay の有無・種類ごとに別ファイルとしてキャッシュされ、
/// overlay 付きプレビューが overlay なしプレビューを上書きしない。
/// `overlay="mask"` は可視化するマスクごとに絵が変わるので、
/// マスクの revision id もキーに含める(でないと別マスクの結果を掴む)。
/// `long_edge` も同様: 同じ (revision, recipe) でも寸法ごとに別ファイルにしないと、
/// 768 のプレビューが 1568 のプレビューを上書きしてしまう。
fn preview_key(
    source_revision_id: &str,
    recipe_hash: &str,
    overlay: Option<&str>,
    mask_revision_id: Option<&str>,
    long_edge: u32,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_revision_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(recipe_hash.as_bytes());
    hasher.update([0u8]);
    hasher.update(overlay.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(mask_revision_id.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(long_edge.to_le_bytes());
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

/// export の書き込み本体。**既存のリンクを辿らない**書き方に限定する
/// (セキュリティ点検、DESIGN.md §9.14)。
///
/// - 新規作成(`replace == false`): `create_new`(O_CREAT|O_EXCL)で開く。
///   その名前に何か(検査の後に置かれたシンボリックリンクを含む)があれば失敗し、辿らない
/// - 上書き(`replace == true`): 同じディレクトリの一時ファイルへ書いてから `rename` で
///   置き換える。`fs::write` は既存 inode をその場で切り詰めるので、書き出し先が
///   objects/ のファイルへの**ハードリンク**だとストアの実体まで書き換わっていた。
///   rename はディレクトリエントリを差し替えるだけなので、他の名前が指す実体は不変
fn write_export(dest: &Path, bytes: &[u8], replace: bool) -> std::io::Result<()> {
    use std::io::Write as _;

    let write_new = |path: &Path| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let result = file.write_all(bytes).and_then(|()| file.flush());
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    };

    if !replace {
        return write_new(dest);
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.atx-export-{}-{nonce}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    write_new(&tmp)?;
    std::fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// レイヤー内の壊れた op は `layers[j].ops[k]` の形で名指しされる
    /// (以前は `Layer` の serde 名と違う `operations` キーを探していて、レイヤー内の
    /// 位置特定が一度も効いていなかった)。
    #[test]
    fn recipe_error_inside_a_layer_is_located_by_the_ops_key() {
        let value = serde_json::json!({
            "operations": [],
            "layers": [
                {"source": "base", "ops": []},
                {"source": "base", "ops": [{"op": "blur", "sigma": 1.0}, {"op": "bogus_op"}]}
            ]
        });
        let err = serde_json::from_value::<TransformRecipe>(value.clone()).unwrap_err();
        let (location, _message, _did_you_mean) = locate_recipe_error(&value, &err);
        assert_eq!(location, "layers[1].ops[1]");
    }

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
        let preview = preview_recipe_of(&recipe, PREVIEW_LONG_EDGE);
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
        let preview = preview_recipe_of(&recipe, PREVIEW_LONG_EDGE);
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
        let a = preview_key("rev_1", "abc", None, None, PREVIEW_LONG_EDGE);
        assert_eq!(
            a,
            preview_key("rev_1", "abc", None, None, PREVIEW_LONG_EDGE)
        );
        assert_ne!(
            a,
            preview_key("rev_1", "abd", None, None, PREVIEW_LONG_EDGE)
        );
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 長辺が違えば別キーになること(768 のプレビューが 1568 を上書きしない)。
    #[test]
    fn preview_key_differs_by_long_edge() {
        let small = preview_key("rev_1", "abc", None, None, PREVIEW_LONG_EDGE);
        let large = preview_key("rev_1", "abc", None, None, PREVIEW_LONG_EDGE_MAX);
        assert_ne!(small, large);
    }

    #[test]
    fn preview_key_differs_by_overlay() {
        let base = preview_key("rev_1", "abc", None, None, PREVIEW_LONG_EDGE);
        let grid = preview_key("rev_1", "abc", Some("grid"), None, PREVIEW_LONG_EDGE);
        let thirds = preview_key("rev_1", "abc", Some("thirds"), None, PREVIEW_LONG_EDGE);
        assert_ne!(base, grid);
        assert_ne!(base, thirds);
        assert_ne!(grid, thirds);
    }

    /// 同じ (revision, recipe, overlay="mask") でもマスクが違えば別キーになること。
    #[test]
    fn preview_key_differs_by_mask_revision() {
        let m1 = preview_key(
            "rev_1",
            "abc",
            Some("mask"),
            Some("rev_m1"),
            PREVIEW_LONG_EDGE,
        );
        let m2 = preview_key(
            "rev_1",
            "abc",
            Some("mask"),
            Some("rev_m2"),
            PREVIEW_LONG_EDGE,
        );
        let none = preview_key("rev_1", "abc", Some("mask"), None, PREVIEW_LONG_EDGE);
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

    /// 構造化エラー([`tool_error`])から code を読む(ユニットテスト用)。
    fn error_code(result: &CallToolResult) -> String {
        error_info(result).code
    }

    /// `layers` を持つプリセットはマクロとして差し込めない。
    ///
    /// 現在の埋め込みプリセットに `layers` 持ちは無い(= この経路は将来のための門)ので、
    /// 差し込み関数を直接呼んで門が閉じていることを固定する。
    #[test]
    fn a_preset_with_layers_cannot_be_inlined() {
        use atx_core::recipe::{Layer, LayerSource};

        let layered = TransformRecipe {
            operations: vec![Operation::AutoOrient],
            layers: Some(vec![Layer {
                source: LayerSource::base(),
                ops: vec![],
                mask: None,
                blend_mode: Default::default(),
                opacity: 1.0,
            }]),
        };
        let mut out = Vec::new();
        let err = inline_preset("fake_layered", &layered, "operations", 0, &mut out)
            .expect_err("a layered preset must be refused");
        assert_eq!(error_code(&err), "preset_not_inlinable");
        assert!(out.is_empty(), "nothing may be spliced in on refusal");

        // layers を持たないプリセットは普通に差し込まれる。
        let flat = TransformRecipe {
            operations: vec![Operation::AutoOrient, Operation::AutoOrient],
            layers: None,
        };
        let mut out = vec![serde_json::json!({"op": "trim"})];
        let (start, end) =
            inline_preset("fake_flat", &flat, "operations", 0, &mut out).expect("must inline");
        assert_eq!((start, end), (1, 3));
        assert_eq!(out.len(), 3);
    }

    /// 展開後の位置を指すエラーから「どのマクロ由来か」を引けること。
    #[test]
    fn error_locations_are_mapped_back_to_the_expanded_preset() {
        let expansions = PresetExpansions(vec![
            PresetExpansion {
                path: "operations".to_string(),
                start: 1,
                end: 3,
                name: "web_optimize".to_string(),
            },
            PresetExpansion {
                path: "layers[1].ops".to_string(),
                start: 0,
                end: 1,
                name: "grayscale".to_string(),
            },
        ]);
        assert_eq!(
            parse_error_location("operations[2] (encode): encode must be the last operation"),
            Some(("operations".to_string(), 2))
        );
        assert_eq!(
            parse_error_location("layers[1].ops: operations[0] (encode): ..."),
            Some(("layers[1].ops".to_string(), 0))
        );
        assert_eq!(expansions.preset_at("operations", 2), Some("web_optimize"));
        assert_eq!(expansions.preset_at("operations", 0), None);
        assert_eq!(expansions.preset_at("layers[1].ops", 0), Some("grayscale"));
    }

    #[test]
    fn filename_templates_must_stay_inside_the_destination_directory() {
        assert!(validate_filename_template(DEFAULT_FILENAME_TEMPLATE).is_ok());
        assert!(validate_filename_template("{index}-{stem}.{ext}").is_ok());
        for bad in [
            "",
            "   ",
            "sub/{revision_id}.{ext}",
            "sub\\{revision_id}.{ext}",
            "../{revision_id}.{ext}",
            "{revision_id}\0.{ext}",
            "{nope}.{ext}",
            "{revision_id.{ext}",
        ] {
            let err = validate_filename_template(bad)
                .expect_err("{bad:?} must be refused as a file name template");
            assert_eq!(error_code(&err), "invalid_filename_template", "{bad:?}");
        }
        // 展開後に区切りが混ざった場合も同じ code で弾く。
        let err = check_file_name("../evil.jpg", "{stem}.{ext}").expect_err("must be refused");
        assert_eq!(error_code(&err), "invalid_filename_template");
    }

    #[test]
    fn vision_token_estimate_rounds_up() {
        // 768x576 = 442,368 px / 750 = 589.8 -> 590
        assert_eq!(estimated_vision_tokens(768, 576), 590);
        // 1568x1568 = 2,458,624 px / 750 = 3278.2 -> 3279
        assert_eq!(estimated_vision_tokens(1568, 1568), 3279);
        assert_eq!(estimated_vision_tokens(1, 1), 1);
        assert_eq!(estimated_vision_tokens(0, 0), 0);
    }

    #[test]
    fn export_extensions_follow_the_stored_mime_type() {
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("image/webp"), "webp");
        assert_eq!(ext_for_mime("image/avif"), "avif");
        assert_eq!(ext_for_mime(CUBE_MIME), "cube");
        assert_eq!(ext_for_mime(SVG_MIME), "svg");
        // フォントも表に載っている(二重表をやめて atx-store の 1 本に寄せた結果)。
        assert_eq!(ext_for_mime(FONT_TTF_MIME), "ttf");
        assert_eq!(ext_for_mime(FONT_OTF_MIME), "otf");
        assert_eq!(ext_for_mime("application/octet-stream"), "bin");
    }

    #[test]
    fn absolutize_folds_dot_segments() {
        let p = absolutize("/tmp/a/./b/../c").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/a/c"));
    }
}

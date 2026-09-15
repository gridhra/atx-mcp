//! rmcp のツール定義層。実処理は [`crate::tools::AtxTools`] に委譲する。
//!
//! - `#[tool_router]` / `#[tool]` / `#[tool_handler]`(rmcp 3.1)でツールを登録
//! - 返却は `CallToolResult` を自前で組み立て(テキスト + structuredContent)、
//!   `outputSchema` は `output_schema = object_output_schema::<T>()` で明示する
//! - annotations は DESIGN.md §4 の表に従って全ツールに設定する(`openWorldHint` は常に false)

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::schema_for_output;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, JsonObject, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::mask::GenerateMaskParams;
use crate::tools::{
    ApplyResult, AtxTools, CompareRevisionsOutput, CompareRevisionsParams, DetectDocumentOutput,
    DetectDocumentParams, DetectTextBlocksOutput, DetectTextBlocksParams, DetectTiltOutput,
    DetectTiltParams, ExplainOperationParams, ExplainResult, ExportAssetParams, ExportResult,
    GenerateMaskOutput, ImportAssetParams, ImportResult, InspectImageParams, InspectOutput,
    ListAssetsOutput, ListAssetsParams, ListOperationsOutput, ListOperationsParams,
    RenderPreviewOutput, RenderPreviewParams, TransformParams,
};

/// `schema_for_output` の最上位に `type: "object"` を保証した outputSchema。
///
/// MCP 仕様は outputSchema の最上位を `type: "object"` に固定している。
/// 単一/バッチを切り替える untagged enum の出力は schemars が最上位 `anyOf` だけを出すため、
/// 公式 TypeScript SDK のクライアントが tools/list 全体を拒否した(v0.5.0、mcp-proxy 経由の
/// Glama 内省で発覚)。各分岐はいずれも object なので、`type` を足しても受理する値は変わらない。
fn object_output_schema<T: schemars::JsonSchema + std::any::Any>() -> Arc<JsonObject> {
    let schema = schema_for_output::<T>();
    if schema.contains_key("type") {
        return schema;
    }
    let mut patched = (*schema).clone();
    patched.insert("type".to_owned(), serde_json::Value::from("object"));
    Arc::new(patched)
}

/// ホスト AI 向けの使い方。initialize の `instructions` として返す。
pub const INSTRUCTIONS: &str = r#"Deterministic, non-generative image transformation over an immutable local asset store.

Model of the world
- Every image lives in the workspace as an immutable *revision* ("rev_..."). Originals are never modified or deleted; every transform produces a NEW revision recording the recipe that produced it.
- Transforms are pure and deterministic: the same input revision plus the same recipe always yields the same revision (the server short-circuits and returns the existing one).
- Nothing here is generative. Pixels are only decoded, geometrically transformed, adjusted, and re-encoded.

Recipe DSL
A recipe is {"operations": [ ... ]}, applied strictly in order, with at most one "encode" which must be last.
Example:
{"operations": [
  {"op": "rotate", "angle_degrees": -1.8, "crop": "largest_inscribed_rect"},
  {"op": "crop", "aspect_ratio": "16:9", "anchor": "center"},
  {"op": "resize", "width": 1600, "fit": "cover", "without_enlargement": true},
  {"op": "adjust", "brightness": 0.05},
  {"op": "encode", "format": "webp", "quality": 82}
]}
EXIF orientation is always normalized into the pixels at decode time, so auto_orient is an explicit no-op.
LUT workflow: a .cube 3D LUT is an asset, not an image - import_asset it first, then reference the returned id as {"op": "lut", "lut_revision_id": "rev_...", "strength": 1.0}.
SVG / watermark workflow: an .svg is a VECTOR asset - import_asset it first, then stamp it as {"op": "svg_overlay", "svg_revision_id": "rev_...", "x": 24, "y": 24, "width": 320, "opacity": 0.25} (x/y = top-left in the CURRENT image). <text> is drawn only with "render_text": true, which loads the bundled Roboto plus any font assets you import and list in "font_revision_ids" (CJK needs an imported font); without it <text> is skipped, so convert text to paths.
Mask workflow (local adjustments): make a grayscale mask revision with generate_mask (or import_asset your own), attach it to any tone/filter operation as "mask": {"revision_id": "rev_...", "invert": false, "feather_px": 0}, and check the coverage with render_preview overlay="mask".
Layered recipes: a recipe may carry {"layers": [...]} - a bottom-to-top stack composited with the 16 W3C blend modes, after which the top-level operations run once as the finishing pass (resize and encode belong there). Call explain_operation {"operation":"layers"} for the full reference before writing one.

Discovering the vocabulary (the ops are deliberately NOT enumerated in the tool schemas)
- list_operations  - compact catalog of every operation (name, category) plus the built-in preset names. Start here.
- explain_operation - full parameter table, examples and gotchas for one operation, or the full op list behind a preset name.
Errors are teachers: an invalid recipe or an unknown name comes back with the valid values and a recovery step, so one round trip is enough to fix it.

Presets: apply_transform and render_preview take either `recipe` (the raw DSL) or `preset` (a built-in named recipe such as web_optimize) - mutually exclusive, exactly one required. A preset is pure sugar: the recipe_hash is computed on the RESOLVED recipe, so a preset call and the equivalent raw recipe land on the same revision. A preset can also be inlined inside a recipe as one operation - {"op": "preset", "name": "ocr_document"} - to combine it with your own ops; it expands in place before anything runs (explain_operation {"operation":"preset"} for the rules).

Recommended flow
0. list_operations / explain_operation - look up the recipe vocabulary on demand (or pick a preset).
1. import_asset  - bring a local file into the workspace, get a revision_id (or `paths` for up to 64 files in one call).
2. inspect_image - dimensions, format, EXIF summary, GPS/PII flag, byte size.
3. detect_tilt   - read-only tilt candidates with a confidence; a null angle means "do not correct".
3b. detect_document - read-only: the dominant quadrilateral (paper, screen, whiteboard) as a ready-to-paste perspective op; null means "do not correct".
3c. detect_text_blocks - read-only: text-like blocks in reading order, the median line height, and crop bands that make the text readable in a preview.
4. render_preview- run a candidate recipe, get an inline JPEG (long edge 768, up to 1568 via `long_edge`) plus a file path, to check composition cheaply.
5. apply_transform - run the same recipe at full resolution, producing a new revision (`revision_ids` applies it to up to 64 revisions in one call).
6. export_asset  - copy a revision out of the workspace (`revision_ids` + `dest_dir` writes up to 64 at once, named by `filename_template`; refuses to overwrite unless overwrite=true, ask the user first, and it never writes inside the workspace store).

Use list_assets to review the ledger (lineage, recipes, sizes). Every result carries a human-readable text summary with absolute paths plus machine-readable structuredContent; prefer the structured fields for chaining.

Note on ICC: for png/webp/avif encode output any ICC color profile on the source is dropped (embedding is jpeg-only) and reported as a warning, not an error.

Reading text in an image (documents, receipts, slides, screenshots)
1. detect_document - if it returns a quad, paste its `suggested_operation` as the FIRST op of your recipe; a null quad means fall back to detect_tilt + rotate.
2. apply_transform with a recipe of: the detected perspective op -> {"op": "trim"} (drop margins) -> {"op": "preset", "name": "ocr_document"} - that last entry is the preset macro, which inlines the preset's own ops (grayscale, auto_levels, unsharp_mask) at that position. Do NOT binarize for a vision model - thresholding thins strokes; `threshold` / ocr_binarize are for external OCR engines such as Tesseract.
3. detect_text_blocks on the rectified revision - if `legibility.line_height_at_1568_px` is under ~16 the whole page is too small to read at once, so paste `legibility.recommended_bands[i]` (already a crop op, in reading order) one at a time; `legibility.strategy` tells you what they are ("whole" = one preview is enough, "bands" = full-width bands, "blocks" = per-block crops for an image too wide for bands).
4. Read it with render_preview `long_edge: 1568`. A vision model reads a downscaled image, so dropping the margins BEFORE that downscale is what buys pixels per glyph; preview a long document in bands with crop.rect.

Visual verification: render_preview takes an optional `overlay` ("grid" | "thirds" | "horizon", or "mask" with a mask_revision_id); compare_revisions shows two revisions side by side or stacked inline, or layout="diff" (same dimensions) for a difference heatmap plus mean/max/changed-ratio stats."#;

/// MCP サーバ本体。ワークスペース1つに対応する。
#[derive(Clone)]
pub struct AtxServer {
    tools: Arc<AtxTools>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl AtxServer {
    pub fn new(tools: Arc<AtxTools>) -> Self {
        Self {
            tools,
            tool_router: Self::tool_router(),
        }
    }

    /// 登録済みツールのルータ(内省・テスト用)。
    pub fn router(&self) -> &ToolRouter<Self> {
        &self.tool_router
    }

    /// Import a local image file into the workspace and issue an immutable revision.
    /// Pass either `path` (one file) or `paths` (a batch of up to 64; one bad file does not
    /// abort the batch, it lands in `failed`) - exactly one of the two.
    /// Idempotent: importing the same bytes again returns the existing revision.
    /// If the imported bytes are already the output of a recipe held in this workspace,
    /// the result carries a warning plus `already_derived_from` so the same recipe is not
    /// applied twice.
    #[tool(
        name = "import_asset",
        output_schema = object_output_schema::<ImportResult>(),
        annotations(
            title = "Import asset",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn import_asset(
        &self,
        Parameters(params): Parameters<ImportAssetParams>,
    ) -> CallToolResult {
        self.tools.import_asset(&params)
    }

    /// Inspect a revision: dimensions, MIME type, byte size, alpha/ICC presence,
    /// EXIF orientation and summary, whether GPS (PII) metadata is present, luma
    /// statistics, a `sharpness` score (variance of the Laplacian - relative, so compare
    /// it against a known-good capture rather than an absolute; for documents below ~30
    /// usually means motion blur or defocus) and a `perceptual_hash` (dHash, 16 hex
    /// digits) for telling "is this the same picture?" without comparing pixels.
    /// Set include_exif=true to also get every EXIF field as {ifd, tag, value} entries;
    /// it is off by default because the full dump can carry GPS coordinates and names.
    #[tool(
        name = "inspect_image",
        output_schema = object_output_schema::<InspectOutput>(),
        annotations(
            title = "Inspect image",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn inspect_image(
        &self,
        Parameters(params): Parameters<InspectImageParams>,
    ) -> CallToolResult {
        self.tools.inspect_image(&params)
    }

    /// Detect the tilt (roll) of a revision: Canny + Hough dominant lines for coarse
    /// candidates, refined below 0.1 degree with an edge projection-profile search.
    /// Horizontal-only and vertical-only estimates are reported separately so a
    /// disagreement can be read as perspective/camera position rather than roll,
    /// Set include_score_curve=true to also get the whole search range as a score curve
    /// (omitted by default to keep the answer small).
    /// Read-only: it never modifies the image. A null recommended angle means "do not correct".
    #[tool(
        name = "detect_tilt",
        output_schema = object_output_schema::<DetectTiltOutput>(),
        annotations(
            title = "Detect tilt",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn detect_tilt(
        &self,
        Parameters(params): Parameters<DetectTiltParams>,
    ) -> CallToolResult {
        self.tools.detect_tilt(&params)
    }

    /// Detect the dominant quadrilateral (a sheet of paper, a screen, a whiteboard, a sign)
    /// in a revision with a contour-based search, and return it as a ready-to-paste
    /// perspective operation. This is to `perspective` what detect_tilt is to `rotate`:
    /// read-only, it never modifies the image, and a null quad means "do not correct".
    /// The quad is in post-EXIF-orientation pixel coordinates, ordered tl, tr, br, bl, and
    /// `output_size_hint` is exactly the size `perspective` will produce from it.
    /// Optional `min_area_ratio` (0.05..=1.0, default 0.2) is the smallest fraction of the
    /// frame a candidate may cover. When quad is null the reason is the first warning:
    /// no_quad_found, already_rectified (the page already fills the frame) or low_confidence.
    #[tool(
        name = "detect_document",
        output_schema = object_output_schema::<DetectDocumentOutput>(),
        annotations(
            title = "Detect document",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn detect_document(
        &self,
        Parameters(params): Parameters<DetectDocumentParams>,
    ) -> CallToolResult {
        self.tools.detect_document(&params)
    }

    /// Find the text-like blocks (headline, paragraphs, table, caption) of a revision with
    /// a binarize + run-length smearing + connected-components pass, and report whether the
    /// text will survive a preview downscale. Read-only: it never modifies the image.
    /// Blocks come back in reading order (top to bottom, then left to right) in
    /// post-EXIF-orientation pixel coordinates, each with its line_count,
    /// median_line_height_px and ink_ratio. `legibility.line_height_at_1568_px` is the
    /// median line height once the whole image is shrunk to long edge 1568 (below ~16 the
    /// text is usually unreadable), and `legibility.recommended_bands` splits the image into
    /// crops that clear that bar - each entry is already a `crop` operation, so paste one into
    /// a recipe before render_preview `long_edge: 1568` and read them in order.
    /// `legibility.strategy` says how it split: "whole" (no split needed), "bands"
    /// (full-width horizontal bands) or "blocks" (per-block crops, used when the image is too
    /// wide for full-width bands to help). Optional `max_blocks` (1..=128, default 32) and `min_block_area_ratio`
    /// (0.0..=1.0, default 0.00005) bound how many and how small the blocks may be.
    /// No block found means the image has no text-like structure, not an error.
    #[tool(
        name = "detect_text_blocks",
        output_schema = object_output_schema::<DetectTextBlocksOutput>(),
        annotations(
            title = "Detect text blocks",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn detect_text_blocks(
        &self,
        Parameters(params): Parameters<DetectTextBlocksParams>,
    ) -> CallToolResult {
        self.tools.detect_text_blocks(&params)
    }

    /// Compact catalog of the recipe vocabulary: every operation with a one-line
    /// description and its parameter names with terse type/range hints, plus the
    /// built-in preset names. Optional `category` ("geometry" | "color" | "filter" |
    /// "output") narrows the list. Call explain_operation for the full schema of one
    /// operation. Read-only.
    #[tool(
        name = "list_operations",
        output_schema = object_output_schema::<ListOperationsOutput>(),
        annotations(
            title = "List operations",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn list_operations(
        &self,
        Parameters(params): Parameters<ListOperationsParams>,
    ) -> CallToolResult {
        self.tools.list_operations(&params)
    }

    /// Full reference for one recipe operation: every parameter with its type, range,
    /// required/default status and semantics, one or two ready-to-paste JSON examples,
    /// and the gotchas worth knowing before using it. A built-in preset name works too and
    /// returns its full operation list, so a preset can be read and copied as a raw recipe.
    /// An unknown name returns a structured error listing every valid operation and preset.
    /// Read-only.
    #[tool(
        name = "explain_operation",
        output_schema = object_output_schema::<ExplainResult>(),
        annotations(
            title = "Explain operation",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn explain_operation(
        &self,
        Parameters(params): Parameters<ExplainOperationParams>,
    ) -> CallToolResult {
        self.tools.explain_operation(&params)
    }

    /// Generate a deterministic grayscale mask as a new PNG revision, with exactly the
    /// dimensions of `reference_revision_id`. `kind` is "linear_gradient" (angle_degrees,
    /// start, end), "radial_gradient" (center_x, center_y, radius, feather),
    /// "luminosity_range" (min, max, feather) or "color_range" (hue_center, hue_width,
    /// feather); the gradients use only the reference's dimensions, the other two compute
    /// weights from its pixels. White = the masked operation applies fully, black = not at
    /// all. Reference the returned revision_id from any tone/filter operation as
    /// "mask": {"revision_id": "rev_...", "invert": false, "feather_px": 0}, or visualise it
    /// with render_preview overlay="mask". Idempotent: the same params over the same
    /// reference produce byte-identical PNG bytes and return the existing revision.
    #[tool(
        name = "generate_mask",
        output_schema = object_output_schema::<GenerateMaskOutput>(),
        annotations(
            title = "Generate mask",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn generate_mask(
        &self,
        Parameters(params): Parameters<GenerateMaskParams>,
    ) -> CallToolResult {
        self.tools.generate_mask(&params)
    }

    /// Apply a transform recipe at full resolution and issue a new revision.
    /// Pass either `recipe` ({"operations": [...]}, applied in order, at most one
    /// "encode" and it must be last) or `preset` (a built-in named recipe) - exactly
    /// one of the two - and either `revision_id` (one image) or `revision_ids` (the same
    /// recipe over a batch of up to 64; one failure does not abort the rest). Call list_operations for the operation catalog and the preset
    /// names, explain_operation for one operation's full schema.
    /// Idempotent: the same (revision_id, resolved recipe) returns the existing derived
    /// revision; a preset hashes identically to the equivalent raw recipe.
    /// Note: if the recipe's encode format is png, webp, or avif, any ICC color profile
    /// on the source is dropped (embedding is only supported for jpeg output); this is
    /// reported as a warning, not an error.
    #[tool(
        name = "apply_transform",
        output_schema = object_output_schema::<ApplyResult>(),
        annotations(
            title = "Apply transform",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn apply_transform(
        &self,
        Parameters(params): Parameters<TransformParams>,
    ) -> CallToolResult {
        self.tools.apply_transform(&params)
    }

    /// Render a recipe as a small JPEG preview (long edge <= 768 by default) and return it
    /// inline plus a file path, so the composition can be checked before committing to
    /// apply_transform. Takes either `recipe` or `preset`, exactly like apply_transform.
    /// Optional `long_edge` (256..=1568) sets the preview size: raise it to 1568 when the
    /// point of the preview is to READ text in the image; 768 is too small for that.
    /// Optional `overlay` ("grid" | "thirds" | "horizon") draws semi-transparent composition
    /// guide lines on the returned preview only (never on the apply_transform output).
    /// overlay="mask" instead visualises a mask: pass `mask_revision_id` (required for this
    /// overlay and rejected for the others) and the preview is tinted red where the mask
    /// weight exceeds 0.5 and dimmed elsewhere, so the coverage can be eyeballed.
    /// Note: if the recipe's encode format is png, webp, or avif, any ICC color profile
    /// on the source is dropped (embedding is only supported for jpeg output); this is
    /// reported as a warning, not an error.
    #[tool(
        name = "render_preview",
        output_schema = object_output_schema::<RenderPreviewOutput>(),
        annotations(
            title = "Render preview",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn render_preview(
        &self,
        Parameters(params): Parameters<RenderPreviewParams>,
    ) -> CallToolResult {
        self.tools.render_preview(&params)
    }

    /// Scale two revisions to a long edge of 640px each and compose them on one canvas
    /// (side by side, or stacked) with an 8px gap, returned inline as a JPEG. A is placed
    /// left/top, B is placed right/bottom. Useful for before/after or A/B visual checks.
    /// layout="diff" instead requires A and B to share the exact same dimensions and returns
    /// a single pixel-difference heatmap plus mean_abs_diff/max_abs_diff/changed_pixel_ratio
    /// and an `ssim` score (structural similarity, 1.0 = identical).
    /// Every layout also reports `perceptual_hash_distance`: the Hamming distance between
    /// the two dHash values (0..=64), where 5 or less usually means the same picture
    /// re-encoded or resized and 20 or more means two different pictures.
    #[tool(
        name = "compare_revisions",
        output_schema = object_output_schema::<CompareRevisionsOutput>(),
        annotations(
            title = "Compare revisions",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn compare_revisions(
        &self,
        Parameters(params): Parameters<CompareRevisionsParams>,
    ) -> CallToolResult {
        self.tools.compare_revisions(&params)
    }

    /// List revisions in the workspace ledger (lineage, recipe hash, dimensions, sizes).
    #[tool(
        name = "list_assets",
        output_schema = object_output_schema::<ListAssetsOutput>(),
        annotations(
            title = "List assets",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn list_assets(
        &self,
        Parameters(params): Parameters<ListAssetsParams>,
    ) -> CallToolResult {
        self.tools.list_assets(&params)
    }

    /// Copy revisions' bytes out of the workspace. Pass either `revision_id` + `dest_path`
    /// (one file) or `revision_ids` + `dest_dir` (a batch of up to 64 into one existing
    /// directory; one failure does not abort the rest) - exactly one of the two forms.
    /// For a batch, `filename_template` (default "{revision_id}.{ext}") builds each file
    /// name from {revision_id} / {index} (1-based, zero-padded) / {ext} (from the MIME type) /
    /// {stem} (the file name the lineage was imported from); it must be a plain file name,
    /// and if two entries would collide nothing is written.
    /// Refuses to overwrite an existing file unless overwrite=true (ask the user first),
    /// and never writes inside the workspace store or through a symbolic link.
    #[tool(
        name = "export_asset",
        output_schema = object_output_schema::<ExportResult>(),
        annotations(
            title = "Export asset",
            read_only_hint = false,
            // overwrite=true でのみ既存ファイルを置き換えうるため destructive とする。
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn export_asset(
        &self,
        Parameters(params): Parameters<ExportAssetParams>,
    ) -> CallToolResult {
        self.tools.export_asset(&params)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AtxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("asset-transform-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("Asset Transform MCP"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

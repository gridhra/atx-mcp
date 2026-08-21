//! rmcp のツール定義層。実処理は [`crate::tools::AtxTools`] に委譲する。
//!
//! - `#[tool_router]` / `#[tool]` / `#[tool_handler]`(rmcp 3.1)でツールを登録
//! - 返却は `CallToolResult` を自前で組み立て(テキスト + structuredContent)、
//!   `outputSchema` は `output_schema = schema_for_output::<T>()` で明示する
//! - annotations は DESIGN.md §4 の表に従って全ツールに設定する(`openWorldHint` は常に false)

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::schema_for_output;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::tools::{
    ApplyTransformOutput, AtxTools, CompareRevisionsOutput, CompareRevisionsParams,
    DetectTiltOutput, DetectTiltParams, ExplainOperationOutput, ExplainOperationParams,
    ExportAssetOutput, ExportAssetParams, ImportAssetParams, ImportOutput, InspectOutput,
    ListAssetsOutput, ListAssetsParams, ListOperationsOutput, ListOperationsParams,
    RenderPreviewOutput, RenderPreviewParams, RevisionParams, TransformParams,
};

/// ホスト AI 向けの使い方。initialize の `instructions` として返す。
pub const INSTRUCTIONS: &str = r#"Deterministic, non-generative image transformation over an immutable local asset store.

Model of the world
- Every image lives in the workspace as an immutable *revision* ("rev_..."). Originals are never modified, overwritten, or deleted; every transform produces a NEW revision that records the recipe that produced it.
- Transforms are pure and deterministic: the same input revision plus the same recipe always yields the same revision (the server short-circuits and returns the existing one instead of re-encoding).
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
LUT workflow: a .cube 3D LUT is an asset, not an image - import_asset the .cube file first, then reference the revision_id it returns from the recipe as {"op": "lut", "lut_revision_id": "rev_...", "strength": 1.0}.

Discovering the vocabulary (the ops are deliberately NOT enumerated in the tool schemas)
- list_operations  - compact catalog of every operation (name, one-line summary, parameter names with type/range hints) plus the built-in preset names. Start here.
- explain_operation - full parameter table, worked examples and gotchas for one operation.
Errors are teachers: an invalid recipe or an unknown name comes back with the valid values and a recovery step, so one extra round trip is enough to fix it.

Presets (the compressed layer of the same language)
apply_transform and render_preview accept either `recipe` (the raw DSL) or `preset` (a built-in named recipe such as web_optimize); they are mutually exclusive and exactly one is required. A preset is pure sugar: it resolves to its recipe and flows through the normal pipeline, and the recipe_hash / idempotency key is computed on the RESOLVED recipe, so a preset call and the equivalent raw recipe land on the same revision. Drop down to a raw recipe whenever you need precise control.

Recommended flow
0. list_operations / explain_operation - look up the recipe vocabulary on demand (or pick a preset).
1. import_asset  - bring a local file into the workspace, get a revision_id.
2. inspect_image - dimensions, format, EXIF summary, GPS/PII flag, byte size.
3. detect_tilt   - read-only tilt candidates with a confidence. It never applies anything; a null angle means "do not correct".
4. render_preview- run a candidate recipe and get a <=768px inline JPEG plus a file path, to check the composition cheaply.
5. apply_transform - run the same recipe at full resolution, producing a new revision.
6. export_asset  - copy a revision out of the workspace. It refuses to overwrite an existing file unless you pass overwrite=true, which you should only do after the user confirms.

Use list_assets to review the ledger (lineage, recipes, sizes). All tool results carry both a human-readable text summary with absolute paths and machine-readable structuredContent; prefer the structured fields for chaining.

Note on ICC: when a recipe's encode output format is png, webp, or avif, any ICC color profile on the source is dropped (embedding is only supported for jpeg output), and apply_transform/render_preview report this as a warning rather than failing.

Visual verification: render_preview accepts an optional `overlay` ("grid" | "thirds" | "horizon") to draw composition guide lines on the returned preview; compare_revisions places two revisions side by side (or stacked) in one inline image so before/after or A/B differences can be checked without leaving the MCP."#;

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
    /// Idempotent: importing the same bytes again returns the existing revision.
    #[tool(
        name = "import_asset",
        output_schema = schema_for_output::<ImportOutput>(),
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
    /// EXIF orientation and summary, and whether GPS (PII) metadata is present.
    #[tool(
        name = "inspect_image",
        output_schema = schema_for_output::<InspectOutput>(),
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
        Parameters(params): Parameters<RevisionParams>,
    ) -> CallToolResult {
        self.tools.inspect_image(&params)
    }

    /// Detect the tilt (roll) of a revision: Canny + Hough dominant lines for coarse
    /// candidates, refined below 0.1 degree with an edge projection-profile search.
    /// Horizontal-only and vertical-only estimates are reported separately so a
    /// disagreement can be read as perspective/camera position rather than roll,
    /// and `score_curve` exposes the whole search range so peak sharpness is visible.
    /// Read-only: it never modifies the image. A null recommended angle means "do not correct".
    #[tool(
        name = "detect_tilt",
        output_schema = schema_for_output::<DetectTiltOutput>(),
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

    /// Compact catalog of the recipe vocabulary: every operation with a one-line
    /// description and its parameter names with terse type/range hints, plus the
    /// built-in preset names. Optional `category` ("geometry" | "color" | "filter" |
    /// "output") narrows the list. Call explain_operation for the full schema of one
    /// operation. Read-only.
    #[tool(
        name = "list_operations",
        output_schema = schema_for_output::<ListOperationsOutput>(),
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
    /// and the gotchas worth knowing before using it. An unknown name returns a
    /// structured error listing every valid operation. Read-only.
    #[tool(
        name = "explain_operation",
        output_schema = schema_for_output::<ExplainOperationOutput>(),
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

    /// Apply a transform recipe at full resolution and issue a new revision.
    /// Pass either `recipe` ({"operations": [...]}, applied in order, at most one
    /// "encode" and it must be last) or `preset` (a built-in named recipe) - exactly
    /// one of the two. Call list_operations for the operation catalog and the preset
    /// names, explain_operation for one operation's full schema.
    /// Idempotent: the same (revision_id, resolved recipe) returns the existing derived
    /// revision; a preset hashes identically to the equivalent raw recipe.
    /// Note: if the recipe's encode format is png, webp, or avif, any ICC color profile
    /// on the source is dropped (embedding is only supported for jpeg output); this is
    /// reported as a warning, not an error.
    #[tool(
        name = "apply_transform",
        output_schema = schema_for_output::<ApplyTransformOutput>(),
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

    /// Render a recipe as a small JPEG preview (long edge <= 768) and return it inline
    /// plus a file path, so the composition can be checked before committing to apply_transform.
    /// Takes either `recipe` or `preset`, exactly like apply_transform.
    /// Optional `overlay` ("grid" | "thirds" | "horizon") draws semi-transparent composition
    /// guide lines on the returned preview only (never on the apply_transform output).
    /// Note: if the recipe's encode format is png, webp, or avif, any ICC color profile
    /// on the source is dropped (embedding is only supported for jpeg output); this is
    /// reported as a warning, not an error.
    #[tool(
        name = "render_preview",
        output_schema = schema_for_output::<RenderPreviewOutput>(),
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
    #[tool(
        name = "compare_revisions",
        output_schema = schema_for_output::<CompareRevisionsOutput>(),
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
        output_schema = schema_for_output::<ListAssetsOutput>(),
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

    /// Copy a revision's bytes out of the workspace to a destination path.
    /// Refuses to overwrite an existing file unless overwrite=true (ask the user first).
    #[tool(
        name = "export_asset",
        output_schema = schema_for_output::<ExportAssetOutput>(),
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

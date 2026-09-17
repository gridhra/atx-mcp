# asset-transform-geometry

Read-only geometry detection for deterministic image transforms: scan tilt
(`detect_tilt`, Canny + a 0.1°-resolution Hough transform + projection-profile
refinement, ±0.3° on architectural and landscape photos), document
quadrilaterals (`detect_document`), and text blocks (`detect_text_blocks`).
Detection never applies anything — it reports an angle or a quad and leaves the
decision to the caller — and it is fully deterministic, so the same bytes
always yield the same answer. Part of the
[atx-mcp](https://github.com/gridhra/atx-mcp) MCP server.

> The crate is published as **`asset-transform-geometry`**, but its library target is
> `atx_geometry`: depend on `asset-transform-geometry = "0.6.2"` and write
> `use atx_geometry::…`.

MIT licensed.

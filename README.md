# atx-mcp

**English** | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

A deterministic (non-generative) asset transformation MCP server for general-purpose
AI agents, written in Rust.

It executes editing intent — "level the horizon, crop to 16:9, brighten it up a
touch" — as a declarative transform recipe, and tracks every result as an
immutable revision. The original asset is never modified.

See [docs/DESIGN.md](docs/DESIGN.md) for the full design.

## Install

No Rust toolchain required. Pick one of the following.

### 1. npx (easiest, recommended)

Node.js 18+ is all you need. The prebuilt native binary for your platform is
pulled in automatically via `optionalDependencies`.

```sh
claude mcp add asset-transform -- npx -y atx-mcp --workspace /path/to/asset-workspace
```

Or add it directly to your MCP client config:

```json
{
  "mcpServers": {
    "asset-transform": {
      "command": "npx",
      "args": ["-y", "atx-mcp", "--workspace", "/path/to/asset-workspace"]
    }
  }
}
```

### 2. Prebuilt binary

Installer scripts (default install location is `~/.local/bin`, or
`%LOCALAPPDATA%\Programs\atx-mcp` on Windows; the archive is verified against
SHA256SUMS before extraction):

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.ps1 | iex
```

To download manually, grab `atx-mcp-<version>-<target>.tar.gz` (`.zip` on
Windows) from [Releases](https://github.com/gridhra/atx-mcp/releases).
Supported targets:

| Platform | Target triple |
|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl` (statically linked, no glibc required) |
| Linux arm64 | `aarch64-unknown-linux-musl` (statically linked, no glibc required) |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

```sh
claude mcp add asset-transform -- ~/.local/bin/atx-mcp --workspace /path/to/asset-workspace
```

### 3. Build from source (any other platform)

All you need is a Rust toolchain and a C compiler (for building libwebp from
its vendored source).

```sh
cargo build --release
# => target/release/atx-mcp
claude mcp add asset-transform -- "$PWD/target/release/atx-mcp" --workspace /path/to/asset-workspace
```

---

`--workspace` (env: `ATX_WORKSPACE`) is the directory used as the asset store.
It is created automatically if it doesn't exist.

## Tools (10)

| Tool | Role |
|---|---|
| `list_operations` | Compact catalog of the recipe vocabulary: every operation with a one-line description and terse parameter hints, plus the built-in preset names. Optional `category:"geometry"\|"color"\|"filter"\|"output"` narrows it (read-only) |
| `explain_operation` | Full reference for one operation: parameter table (type, range, required/default, semantics), ready-to-paste JSON examples and gotchas. An unknown name returns the list of valid ones (read-only) |
| `import_asset` | Import a local image into the workspace (sha256-idempotent) |
| `inspect_image` | Inspect dimensions, EXIF, ICC profile, presence of GPS data, etc. (read-only) |
| `detect_tilt` | Estimate tilt angle via Canny+Hough (coarse) plus a projection profile (sub-0.1° refinement). Also returns horizontal/vertical family estimates and their score curves. Returns "do not correct" when confidence is low (read-only) |
| `render_preview` | Apply a recipe (or a `preset`) at low resolution (long edge ≤768) and return it as an inline image. `overlay:"grid"\|"thirds"\|"horizon"` overlays composition guide lines (drawn on the preview only; it has no effect on the actual transform) |
| `apply_transform` | Apply a recipe (or a `preset`) at full resolution and produce a new revision (the same recipe always yields the same revision) |
| `compare_revisions` | Downscale two revisions to long edge ≤640 and return them composited into a single inline image, arranged via `layout:"side_by_side"\|"stacked"` (for A/B and before/after visual comparison) |
| `list_assets` | Read the revision ledger (read-only) |
| `export_asset` | Write a revision out to a given path (an existing file is only overwritten when `overwrite:true` is explicitly set) |

## Recipe example

```json
{
  "operations": [
    { "op": "rotate", "angle_degrees": -1.8 },
    { "op": "crop", "aspect_ratio": "16:9" },
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

Supported ops: `auto_orient` / `rotate` / `perspective` / `crop` (crop, pad) /
`resize` (cover, contain, fill) / `adjust` / `color_matrix` / `curves` /
`levels` / `blur` / `median` / `unsharp_mask` / `encode` (jpeg, png, webp,
avif) / `strip_metadata`.
The operation vocabulary is deliberately kept out of the tool schemas: call
`list_operations` for the up-to-date catalog and `explain_operation` for one
operation's full schema, examples and gotchas.

## Presets

`apply_transform` and `render_preview` take either `recipe` (the raw DSL) or
`preset` (a built-in named recipe from [`presets/`](presets)) — exactly one of
the two:

| Preset | What it does |
|---|---|
| `eyecatch_16_9` | Center-crop to 16:9, resize to 1600px wide, WebP q82 |
| `thumbnail_square` | Center-crop to 1:1, resize to 800x800, WebP q80 |
| `web_optimize` | Fit inside 2000x2000 without upscaling, WebP q80 |
| `grayscale` | Black and white via a BT.709 luma `color_matrix` |
| `sepia` | Classic sepia tone via `color_matrix` |

A preset is pure sugar: it resolves to its recipe and flows through the normal
pipeline, and the `recipe_hash` (the idempotency key) is computed on the
**resolved** recipe — so a preset call and the equivalent raw recipe land on the
same revision.

## Guarantees

- **Deterministic**: the same input + the same recipe always produces
  byte-identical output (regression-checked with golden tests)
- **Idempotent**: recipes are normalized (keys sorted, `f64` values quantized
  to a 1e-6 grid) and hashed with sha256. If `(input revision, recipe hash)`
  matches an existing pair, the existing revision is returned instead of a new one
- **Originals are protected**: `objects/` is an append-only, content-addressed
  store — there is no delete or overwrite API

## Development

```sh
cargo test --workspace     # unit + integration + property (proptest) tests
cargo clippy --workspace --all-targets -- -D warnings
```

Crate layout: `atx-core` (recipe/transform engine) / `atx-geometry` (tilt
detection) / `atx-store` (immutable asset store) / `atx-mcp` (rmcp stdio server).

See [RELEASING.md](RELEASING.md) for the release process.

## License

MIT. See [LICENSE](LICENSE).

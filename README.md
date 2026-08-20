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

## Tools (8)

| Tool | Role |
|---|---|
| `import_asset` | Import a local image into the workspace (sha256-idempotent) |
| `inspect_image` | Inspect dimensions, EXIF, ICC profile, presence of GPS data, etc. (read-only) |
| `detect_tilt` | Estimate tilt angle via Canny+Hough (coarse) plus a projection profile (sub-0.1° refinement). Also returns horizontal/vertical family estimates and their score curves. Returns "do not correct" when confidence is low (read-only) |
| `render_preview` | Apply a recipe at low resolution (long edge ≤768) and return it as an inline image. `overlay:"grid"\|"thirds"\|"horizon"` overlays composition guide lines (drawn on the preview only; it has no effect on the actual transform) |
| `apply_transform` | Apply a recipe at full resolution and produce a new revision (the same recipe always yields the same revision) |
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

Supported ops: `auto_orient` / `rotate` / `crop` (crop, pad) / `resize`
(cover, contain, fill) / `adjust` (brightness, contrast, saturation,
sharpness) / `encode` (jpeg, png, webp, avif) / `strip_metadata`.

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

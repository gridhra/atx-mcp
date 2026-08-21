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

## Tools (11)

| Tool | Role |
|---|---|
| `list_operations` | Compact catalog of the recipe vocabulary: every operation with a one-line description and terse parameter hints, plus the built-in preset names. Optional `category:"geometry"\|"color"\|"filter"\|"output"` narrows it (read-only) |
| `explain_operation` | Full reference for one operation: parameter table (type, range, required/default, semantics), ready-to-paste JSON examples and gotchas. An unknown name returns the list of valid ones (read-only) |
| `import_asset` | Import a local image into the workspace (sha256-idempotent) |
| `inspect_image` | Inspect dimensions, EXIF, ICC profile, presence of GPS data, etc. (read-only) |
| `detect_tilt` | Estimate tilt angle via Canny+Hough (coarse) plus a projection profile (sub-0.1° refinement). Also returns horizontal/vertical family estimates and their score curves. Returns "do not correct" when confidence is low (read-only) |
| `generate_mask` | Generate a deterministic grayscale mask (`linear_gradient` / `radial_gradient` / `luminosity_range` / `color_range`) as a PNG revision with the same dimensions as the reference image, to be referenced from an operation's `mask` field (idempotent) |
| `render_preview` | Apply a recipe (or a `preset`) at low resolution (long edge ≤768) and return it as an inline image. `overlay:"grid"\|"thirds"\|"horizon"` overlays composition guide lines, and `overlay:"mask"` (with `mask_revision_id`) tints the coverage of a mask (drawn on the preview only; it has no effect on the actual transform) |
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

Supported ops (18): `auto_orient` / `rotate` / `perspective` / `crop` (crop,
pad) / `resize` (cover, contain, fill) / `adjust` / `color_matrix` / `curves` /
`levels` / `lut` / `white_balance` / `hsl` / `blur` / `median` /
`unsharp_mask` / `convolve` / `encode` (jpeg, png, webp, avif) /
`strip_metadata`.
The operation vocabulary is deliberately kept out of the tool schemas: call
`list_operations` for the up-to-date catalog and `explain_operation` for one
operation's full schema, examples and gotchas.

### LUT (.cube)

A `.cube` 3D/1D LUT is an *asset*, not an image: import it first, then point a
recipe at the revision it produced.

1. `import_asset` the `.cube` file. It is stored as an immutable revision with
   `mime_type: "application/x-cube"` (`inspect_image` refuses it on purpose —
   it is not an image).
2. Reference the returned `revision_id` from a recipe:

```json
{ "op": "lut", "lut_revision_id": "rev_...", "strength": 0.8 }
```

`strength` (0..1, default 1.0) blends linearly with the original. Because
revisions are immutable, including the referenced id in the `recipe_hash` keeps
the transform fully deterministic — but it also means the recipe is only
reproducible inside a workspace that holds that LUT, so move the `.cube`
alongside the recipe when you move a look between machines. Referencing an
unknown id fails with a structured error before any pixel work happens.

### Masks (local adjustments)

A mask is a *grayscale image revision*: its BT.709 luma is the weight, so white
means "apply this operation at full strength" and black means "leave the pixel
alone". Any of the 11 tone/filter ops (`adjust`, `color_matrix`, `curves`,
`levels`, `hsl`, `lut`, `white_balance`, `blur`, `median`, `unsharp_mask`,
`convolve`) accepts one.

1. `generate_mask` builds one deterministically against a reference image, with
   exactly that image's dimensions:

| `kind` | Parameters | What it selects |
|---|---|---|
| `linear_gradient` | `angle_degrees` (0 = white at the top fading down, positive = clockwise), `start`, `end` (0..1 positions along the axis where the weight goes 1→0) | A graduated filter (skies, foregrounds) |
| `radial_gradient` | `center_x`, `center_y` (0..1 relative), `radius` (0..1 of the half-diagonal), `feather` (0..1 extra falloff band) | A vignette or a subject spotlight |
| `luminosity_range` | `min`, `max` (0..255), `feather` (luma units of soft shoulder outside the range) | Highlights, midtones or shadows |
| `color_range` | `hue_center` (0..360), `hue_width` (1..180 half-width), `feather` (extra degrees) | One hue family (sky blue, foliage green) |

   You can also `import_asset` your own grayscale image instead.

2. Attach the returned `revision_id` to an operation:

```json
{ "op": "curves", "master": [[0,0],[128,168],[255,255]],
  "mask": { "revision_id": "rev_...", "invert": false, "feather_px": 8.0 } }
```

   `invert` (default `false`) flips the weight to `1-w`; `feather_px` (default
   `0.0`) blurs the mask edge by that gaussian sigma in pixels of the current
   image.

3. `render_preview` with `overlay:"mask"` and `mask_revision_id` tints the
   preview red where the weight exceeds 0.5 and dims it elsewhere, so the
   coverage can be checked before committing.

Masks are referenced by revision id exactly like LUTs, so the same caveat
applies: the recipe hash includes the id, and the recipe only reproduces inside
a workspace that holds that mask.

### Layers

A recipe may carry a `layers` stack instead of (or in addition to) a flat
`operations` list. Layers composite bottom-to-top, each layer's `ops` run
against its own source before it is blended onto the running composite:

```json
{
  "layers": [
    { "source": "base", "ops": [] },
    {
      "source": { "revision_id": "rev_..." },
      "ops": [{ "op": "blur", "sigma": 8 }],
      "blend_mode": "multiply",
      "opacity": 0.6
    }
  ],
  "operations": [
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

- `source` is either `"base"` (the input revision passed to `apply_transform`
  / `render_preview`) or `{"revision_id": "rev_..."}` (any other revision
  already in the workspace). Every layer's source must match the base image's
  dimensions exactly, or the recipe fails with a structured error before any
  pixel work happens.
- `ops` is a normal operations list, applied to that layer's source alone.
- `mask`, `blend_mode` (default `"normal"`) and `opacity` (default `1.0`)
  control how the layer composites onto the layers below it.
- Blend mode is one of the 12 W3C separable modes: `normal`, `multiply`,
  `screen`, `overlay`, `darken`, `lighten`, `color_dodge`, `color_burn`,
  `hard_light`, `soft_light`, `difference`, `exclusion`.
- When `layers` is present, the top-level `operations` becomes the
  **finishing pass**, applied once to the composited result — this is where
  `resize` and the final `encode` belong (`encode` must still be last and
  appear at most once).
- Call `explain_operation {"operation":"layers"}` for the full reference.

## Presets

`apply_transform` and `render_preview` take either `recipe` (the raw DSL) or
`preset` (a built-in named recipe from [`presets/`](presets)) — exactly one of
the two:

| Preset | What it does |
|---|---|
| `eyecatch_16_9` | Center-crop to 16:9, resize to 1600px wide, WebP q82 |
| `film_soft` | Soft film look: gentle S-curve plus a 15% pull towards luma |
| `product_clean` | Clean product shot: near-neutral white balance, levels lift, light sharpen |
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

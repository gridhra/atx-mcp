# asset-transform-core

Deterministic, non-generative image transform engine. This is the engine behind
the [atx-mcp](https://github.com/gridhra/atx-mcp) MCP server, published
separately so it can be used as a plain Rust library.

> The crate is published as **`asset-transform-core`**, but its library target is
> `atx_core`: depend on `asset-transform-core = "0.6.2"` and write `use atx_core::…`.
> (`atx-core` on crates.io is an unrelated project.)

## What it does

A *recipe* is a JSON document listing operations applied strictly in order:
geometry (`crop`, `resize`, `rotate`, `perspective`, `flip`), tone and color
(`adjust`, `curves`, `white_balance`, `hsl`, `grayscale`, `lut`), finishing
(`blur`, `sharpen`, `grain`, `vignette`, `gradient`, `pixelate`), a `layers`
stack composited with the 16 W3C blend modes, and at most one trailing
`encode` (JPEG / PNG / WebP / AVIF). Any tone or filter operation can be
restricted to a grayscale mask.

Nothing is generative: pixels are only decoded, transformed, and re-encoded.
No model, no network, no hallucinated detail.

## The determinism contract

- **Same input bytes + same recipe → byte-identical output**, on any machine
  and any OS. Integer and fixed-order floating-point paths only; no
  parallelism-dependent reduction order; no host-dependent inputs (fonts are
  bundled or supplied explicitly, never read from the system font database).
- **Recipes are canonicalized and hashed** (`recipe_hash`) so a transform has a
  stable identity independent of key order or omitted defaults.
- **EXIF orientation is normalized into the pixels at decode time**, so a
  recipe never has to reason about it.
- `ENGINE_VERSION` names the pixel-level generation. Output bytes are frozen
  within a generation; a change that would alter them requires a new
  generation.

## Minimal example

```toml
[dependencies]
asset-transform-core = "0.6.2"   # library name is `atx_core`
serde_json = "1"                 # recipes are plain JSON
```

```rust
use atx_core::{apply_recipe, Limits, TransformRecipe};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let recipe: TransformRecipe = serde_json::from_str(
        r#"{"operations":[
             {"op":"resize","width":1600,"fit":"cover","without_enlargement":true},
             {"op":"encode","format":"webp","quality":82}
           ]}"#,
    )?;

    let input = std::fs::read("photo.jpg")?;
    let out = apply_recipe(&input, &recipe, &Limits::default())?;

    println!("{} {}x{}, {} bytes", out.mime_type, out.width, out.height, out.bytes.len());
    std::fs::write("photo.webp", &out.bytes)?;
    Ok(())
}
```

Recipes that reference other assets (a `.cube` LUT, an SVG overlay, a mask) use
`apply_recipe_with_assets` with your own `AssetResolver`, so the engine never
touches the filesystem itself.

Also exported: `inspect_bytes` (dimensions, format, EXIF summary),
`ImageStats`, perceptual hashing (`dhash_hex`) and `ssim_gray` for comparing
revisions, and the `validate_*_asset` helpers used when importing LUT, SVG, and
font assets.

## Related crates

| Crate | Directory | Role |
|---|---|---|
| [`atx-mcp`](https://crates.io/crates/atx-mcp) | `crates/atx-mcp` | the MCP server binary |
| [`asset-transform-core`](https://crates.io/crates/asset-transform-core) | `crates/atx-core` | recipe DSL and transform engine (this crate) |
| [`asset-transform-geometry`](https://crates.io/crates/asset-transform-geometry) | `crates/atx-geometry` | tilt and document detection |
| [`asset-transform-store`](https://crates.io/crates/asset-transform-store) | `crates/atx-store` | immutable, content-addressed asset store |

Full documentation, tool reference, and design notes:
[github.com/gridhra/atx-mcp](https://github.com/gridhra/atx-mcp).

MIT licensed.

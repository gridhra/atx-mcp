# Third-party notices

atx-mcp itself is distributed under the MIT License (see `LICENSE`). Rust crate
dependencies keep their own licenses and are gated by `cargo deny check licenses`
(the allowed set and the reasoning are in `deny.toml`). This file covers the
non-crate material that ships **inside** the binary.

## Bundled font: Roboto Regular

`svg_overlay` can draw `<text>` deterministically (`"render_text": true`). To make
the result independent of the machine it runs on, atx-mcp never reads system fonts;
it renders with one font that is **embedded in the executable** (`include_bytes!`),
plus any font the user imports explicitly as an asset.

| | |
|---|---|
| Typeface | Roboto Regular |
| Version | 2.138 (static TTF, unhinted) |
| Source archive | <https://github.com/googlefonts/roboto/releases/download/v2.138/roboto-unhinted.zip> |
| File taken from the archive | `Roboto-Regular.ttf` |
| Path in this repository | `crates/atx-core/assets/fonts/Roboto-Regular.ttf` |
| Size | 349,400 bytes |
| SHA-256 | `f3edb8058e523f5612bfd99d0745e661568ad85e1b6217bc62f786fabae624c6` |
| SHA-256 of the source archive | `70f64c718510a601fbcf752aafe644314dacaeb85474dc689c89787c4a72a728` |
| License | Apache License 2.0 |
| License text | `crates/atx-core/assets/fonts/LICENSE-Roboto.txt` (the `LICENSE` file of that archive) |
| Copyright | Copyright 2011 Google Inc. All Rights Reserved. |

The 2.x releases of Roboto are licensed under the Apache License 2.0. (Roboto 3.x
and the Google Fonts distribution are offered under the SIL Open Font License
instead; this repository deliberately uses the Apache-2.0 release so that a single
notice file satisfies the redistribution terms of the embedded binary.)

This font is redistributed unmodified. The Apache-2.0 license requires that the
license text and the notice above travel with the binary — that is what this file
and `LICENSE-Roboto.txt` are for.

No CJK font is bundled (a usable one is 5 MB or more). To draw Japanese, Chinese
or Korean text, import a font of your own with `import_asset` and pass its
revision id in `svg_overlay.font_revision_ids`.

# atx-mcp

Deterministic, non-generative image transform MCP server: reproducible recipes,
immutable originals. A single static binary with no runtime dependencies.

mcp-name: io.github.gridhra/atx-mcp

Every image imported into the workspace becomes an immutable *revision*.
Transforms are pure: the same source revision plus the same recipe always yields
byte-identical output, on any machine, and the server returns the existing
revision instead of recomputing it. Originals are never modified or deleted, and
every revision records the recipe and source that produced it, so any published
image can be traced back and reproduced. Nothing is generative — pixels are only
decoded, geometrically transformed, adjusted, and re-encoded.

## Install

### cargo binstall (prebuilt binary, no compilation)

```sh
cargo binstall atx-mcp
```

Pulls the signed release archive for your platform from GitHub Releases.
Supported targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-pc-windows-msvc`.

### cargo install (build from source)

```sh
cargo install atx-mcp
```

Needs a Rust toolchain and a C compiler (libwebp is built from vendored source).

### Prebuilt binary without cargo

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.ps1 | iex
```

### Docker

```sh
docker run -i --rm -v "$PWD:/workspace" ghcr.io/gridhra/atx-mcp:0.6.2
```

Paths you pass to `import_asset` must be paths *inside the container*
(`/workspace/...`).

### npx (Node.js 18+, nothing to install)

```sh
npx -y atx-mcp --workspace /path/to/asset-workspace
```

## Register with an MCP client

```sh
claude mcp add --scope user asset-transform -- atx-mcp --workspace /path/to/asset-workspace
```

`--workspace` (env: `ATX_WORKSPACE`) is the directory used as the asset store.
It is created automatically if it does not exist.

## Using the engine as a library

The transform engine is published separately and does not depend on MCP:

| Crate | Directory | Role |
|---|---|---|
| [`asset-transform-core`](https://crates.io/crates/asset-transform-core) | `crates/atx-core` | recipe DSL and transform engine |
| [`asset-transform-geometry`](https://crates.io/crates/asset-transform-geometry) | `crates/atx-geometry` | tilt and document detection |
| [`asset-transform-store`](https://crates.io/crates/asset-transform-store) | `crates/atx-store` | immutable, content-addressed asset store |

Each crate's library target keeps its short name, so depend on
`asset-transform-core = "0.6.2"` and write `use atx_core::…`.

## Links

- Repository, full tool reference, recipe examples, and design notes:
  [github.com/gridhra/atx-mcp](https://github.com/gridhra/atx-mcp)
  ([日本語](https://github.com/gridhra/atx-mcp/blob/main/README.ja.md) ·
  [简体中文](https://github.com/gridhra/atx-mcp/blob/main/README.zh-CN.md))
- Bundled presets (named recipes):
  [`crates/atx-mcp/presets/`](https://github.com/gridhra/atx-mcp/tree/main/crates/atx-mcp/presets)
- Releases: [github.com/gridhra/atx-mcp/releases](https://github.com/gridhra/atx-mcp/releases)

MIT licensed.

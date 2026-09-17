# atx-mcp

Deterministic (non-generative) asset transformation MCP server, written in Rust.

This npm package is a thin launcher: it installs a prebuilt native binary
(`atx-mcp`) for your platform via `optionalDependencies` and execs it with
transparent stdio.

## Use

```sh
claude mcp add asset-transform -- npx -y atx-mcp --workspace /path/to/asset-workspace
```

Or in an MCP client config:

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

## Supported platforms

macOS arm64 / x64, Linux x64 / arm64 (static musl, no glibc requirement),
Windows x64. On any other platform, build from source with a Rust toolchain.

## Other ways to install

This npm launcher exists so that a Node-only environment can run the server
with nothing to install. The same binary is also distributed directly:

- `cargo binstall atx-mcp` — prebuilt binary via a Rust toolchain, no compilation
- `cargo install atx-mcp` — build from source (any platform Rust supports)
- `docker run -i --rm -v "$PWD:/workspace" ghcr.io/gridhra/atx-mcp:latest`
  (pin a version tag in production; paths passed to the tools are container
  paths under `/workspace`)
- Archives and installer scripts on the
  [Releases](https://github.com/gridhra/atx-mcp/releases) page

Full documentation: <https://github.com/gridhra/atx-mcp>

MIT licensed.

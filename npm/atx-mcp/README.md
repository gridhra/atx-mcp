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

Full documentation: <https://github.com/gridhra/atx-mcp>

MIT licensed.

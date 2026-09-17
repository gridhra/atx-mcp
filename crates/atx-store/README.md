# asset-transform-store

Local-first, append-only asset store for deterministic image transforms. Image
bytes live in `objects/` addressed by their SHA-256; an append-only JSONL ledger
records every *revision* together with the recipe and source revision that
produced it, so any derived image can be traced back to its original and
reproduced. There is no overwrite or delete API: importing the same bytes
returns the existing revision, and re-deriving the same `(source revision,
recipe hash)` pair returns the existing result instead of recomputing it. Part
of the [atx-mcp](https://github.com/gridhra/atx-mcp) MCP server.

> The crate is published as **`asset-transform-store`**, but its library target is
> `atx_store`: depend on `asset-transform-store = "0.6.2"` and write `use atx_store::…`.

MIT licensed.

# Security Policy

## Supported Versions

atx-mcp is pre-1.0. Only the **latest published 0.x release** is supported.
Fixes are shipped in a new patch or minor release; there are no backports to
older 0.x lines.

| Version           | Supported |
| ----------------- | --------- |
| latest 0.x        | Yes       |
| any older release | No        |

Check your version with:

```sh
atx-mcp --version
```

## Reporting a Vulnerability

**Please do not open a public issue for a security problem.**

Report privately through GitHub Private Vulnerability Reporting:

1. Go to <https://github.com/gridhra/atx-mcp/security/advisories/new>
   (or: repository → **Security** tab → **Report a vulnerability**).
2. Include the atx-mcp version, install channel (npx / prebuilt binary /
   built from source), OS and architecture, and — if the issue is triggered by
   an input file — the recipe JSON and a description of the input that
   reproduces it.
3. If the reproducer requires a specific image, describe how to generate it
   rather than attaching third-party or personal material. Fully synthetic
   fixtures are preferred.

### Response expectation

atx-mcp is maintained by a single developer on a best-effort basis. Expect:

- an acknowledgement within about 7 days,
- a first assessment (confirmed / not applicable / needs more information)
  within about 30 days,
- a fix released as soon as practical after confirmation, with credit in the
  advisory unless you ask otherwise.

These are goals, not guarantees. If you have not heard back within 30 days,
feel free to nudge in the advisory thread.

### Coordinated disclosure

Please give a reasonable window before public disclosure — 90 days is the
default assumption. A published GitHub Security Advisory and a release note
will accompany the fix.

## Scope

atx-mcp is a local, non-network MCP server. It reads and writes files under a
workspace directory that the operator chooses, speaks MCP over stdio, and makes
no outbound network requests.

### In scope

- **Image decoding and metadata parsing — the primary attack surface.**
  Untrusted image bytes flow into third-party decoders (JPEG, PNG, WebP, AVIF,
  SVG rasterization) and EXIF/ICC parsers. Memory-unsafety, panics that are
  reachable from a well-formed MCP request, unbounded allocation, or
  decompression bombs triggered by a crafted input file are in scope.
- **Path handling.** Any way to read or write outside the configured
  `--workspace` directory, including via asset identifiers, export paths,
  symlinks, or path traversal in a recipe.
- **Original-immutability violations.** atx-mcp guarantees that an imported
  original is never modified. Any input that causes an original to be
  overwritten or destroyed is a security-relevant bug.
- **Metadata stripping failures.** `strip_metadata` is used to remove GPS and
  other EXIF data before publication. An input for which stripping silently
  leaves location or identifying metadata in the output is in scope, and is
  treated as high severity.
- **Unsafe handling of SVG overlays.** SVG input is rasterized with system
  fonts and remote resource loading disabled; anything that reaches the network,
  the filesystem outside the workspace, or the system font database from an SVG
  is in scope.

### Determinism claims — treated as correctness, not security

atx-mcp claims that the same input plus the same recipe produces byte-identical
output, and that a recipe's canonical hash is stable. A violation of that
(non-determinism across runs, platforms, or versions; a hash collision from
distinct recipes; a change that silently alters an existing recipe's hash) is a
serious bug and we want to hear about it, but unless it is combined with an
actual security impact it is handled as a **normal public issue**, not an
advisory. Open it at
<https://github.com/gridhra/atx-mcp/issues> with the recipe JSON attached —
determinism makes such reports exactly reproducible.

### Out of scope

- Vulnerabilities in an MCP client, an LLM's choice of tool arguments, or
  prompt injection that leads a model to call a tool the user did not intend.
  atx-mcp performs the operations it is asked to perform within its workspace;
  deciding what to ask is the client's responsibility.
- Anything requiring the attacker to already have write access to the workspace
  directory or to the machine running the server.
- Denial of service from legitimately expensive-but-bounded work (very large
  images, large blur radii). Report those as performance issues.
- Advisories in dependencies that are not reachable from atx-mcp's code paths.
  These are still tracked in CI (`cargo audit` / `cargo deny`) and will be
  updated in the normal dependency cadence.

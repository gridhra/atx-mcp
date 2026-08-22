#!/usr/bin/env node
"use strict";

// Thin launcher for the native `atx-mcp` binary.
//
// The binary itself lives in a per-platform package listed in
// optionalDependencies; npm installs only the one matching the host's
// os/cpu fields. This file resolves it and execs it with fully transparent
// stdio, because atx-mcp speaks JSON-RPC over stdin/stdout (MCP stdio
// transport) and any buffering or re-encoding here would corrupt the stream.

const { spawn } = require("node:child_process");

const REPO_URL = "https://github.com/gridhra/atx-mcp";

const PLATFORMS = {
  "darwin arm64": "darwin-arm64",
  "darwin x64": "darwin-x64",
  "linux x64": "linux-x64",
  "linux arm64": "linux-arm64",
  "win32 x64": "win32-x64",
};

function resolveBinary() {
  const key = `${process.platform} ${process.arch}`;
  const slug = PLATFORMS[key];
  if (!slug) return { error: `unsupported platform: ${key}` };

  const pkg = `atx-mcp-${slug}`;
  const exe = process.platform === "win32" ? "atx-mcp.exe" : "atx-mcp";
  try {
    return { path: require.resolve(`${pkg}/bin/${exe}`) };
  } catch (_) {
    return {
      error:
        `the platform package "${pkg}" is not installed.\n` +
        `This usually means the install ran with --no-optional, or with a\n` +
        `different --os/--cpu than the machine you are running on. It can also\n` +
        `mean the platform package for this release hasn't been published yet\n` +
        `(win32-x64 in particular is sometimes delayed by npm's spam-detection\n` +
        `hold) — check the GitHub release page for available downloads:\n` +
        `  ${REPO_URL}/releases`,
    };
  }
}

const resolved = resolveBinary();
if (resolved.error) {
  process.stderr.write(
    `atx-mcp: ${resolved.error}\n\n` +
      `Prebuilt binaries are available for macOS (arm64, x64), Linux (x64, arm64)\n` +
      `and Windows (x64). For anything else, build from source:\n\n` +
      `  git clone ${REPO_URL}\n` +
      `  cd atx-mcp && cargo build --release\n` +
      `  # => target/release/atx-mcp\n\n` +
      `Requires a Rust toolchain and a C compiler. Issues and prebuilt downloads:\n` +
      `  ${REPO_URL}/releases\n`
  );
  process.exit(1);
}

// npm generally preserves the executable bit from the package tarball, but
// some mirrors and extraction paths do not. Repairing it is cheap.
if (process.platform !== "win32") {
  try {
    const { statSync, chmodSync } = require("node:fs");
    if ((statSync(resolved.path).mode & 0o111) === 0) {
      chmodSync(resolved.path, 0o755);
    }
  } catch (_) {
    /* best effort */
  }
}

const child = spawn(resolved.path, process.argv.slice(2), {
  stdio: "inherit", // real fd passthrough: no buffering, no re-encoding
  windowsHide: true,
});

const SIGNALS = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"];
const forward = (sig) => {
  if (child.exitCode === null && child.signalCode === null) {
    try {
      child.kill(sig);
    } catch (_) {
      /* already gone */
    }
  }
};
for (const sig of SIGNALS) process.on(sig, () => forward(sig));

child.on("error", (err) => {
  process.stderr.write(
    `atx-mcp: failed to start ${resolved.path}: ${err.message}\n`
  );
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    // Re-raise so the parent shell observes the same termination cause.
    for (const s of SIGNALS) process.removeAllListeners(s);
    try {
      process.kill(process.pid, signal);
      return;
    } catch (_) {
      process.exit(1);
    }
  }
  process.exit(code === null ? 1 : code);
});

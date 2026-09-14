#!/bin/sh
# Glama(MCP サーバの登録・評価サイト)向けリリース補助。手順は RELEASING.md「6. Glama」。
#
#   sh scripts/glama.sh form build-steps X.Y.Z   # フォーム「Build steps」に貼る JSON
#   sh scripts/glama.sh form cmd                 # フォーム「CMD arguments」に貼る JSON
#   sh scripts/glama.sh form placeholder         # フォーム「Placeholder parameters」に貼る JSON
#   sh scripts/glama.sh check X.Y.Z              # Glama と同じ構成のイメージで起動・内省を検証
#
# check は GitHub Release vX.Y.Z が公開済みであることが前提(バイナリをそこから取る)。
# 必要なもの: docker(デーモン起動済み)、node、curl。
set -eu

die() { printf 'glama.sh: %s\n' "$1" >&2; exit 1; }

PLACEHOLDER_WORKSPACE=/tmp/atx-workspace
# Glama が生成する Dockerfile の土台(2026-09 時点のフォーム既定値)。
# Glama の Dockerfile プレビューと食い違ったら、ここを合わせる。
GLAMA_BASE_IMAGE=debian:trixie-slim
GLAMA_NODE_MAJOR=26
GLAMA_MCP_PROXY=mcp-proxy@6.4.3

check_version() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "version must be X.Y.Z (no leading v): $1"
}

# ビルド手順(Glama は各要素を括弧で包み && で連結した 1 つの RUN にする)。
# 静的リンクの musl バイナリを GitHub Release から取り、SHA256SUMS で照合して置く。
build_steps_json() {
  node -e '
const v = process.argv[1];
const steps = [
  "apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*",
  "set -eu; case \"$(uname -m)\" in x86_64) t=x86_64-unknown-linux-musl ;; aarch64) t=aarch64-unknown-linux-musl ;; *) echo unsupported arch >&2; exit 1 ;; esac; " +
    `v=${v}; f=atx-mcp-$v-$t.tar.gz; mkdir -p /tmp/atx && cd /tmp/atx && ` +
    "curl -fsSLO https://github.com/gridhra/atx-mcp/releases/download/v$v/$f && " +
    "curl -fsSLO https://github.com/gridhra/atx-mcp/releases/download/v$v/SHA256SUMS && " +
    "grep \" $f\\$\" SHA256SUMS | sha256sum -c - && tar -xzf $f && " +
    "install -m 0755 \"$(find . -type f -name atx-mcp | head -n1)\" /usr/local/bin/atx-mcp && cd / && rm -rf /tmp/atx",
];
console.log(JSON.stringify(steps, null, 2));
' "$1"
}

cmd_json() { printf '["atx-mcp"]\n'; }
placeholder_json() { printf '{"ATX_WORKSPACE": "%s"}\n' "$PLACEHOLDER_WORKSPACE"; }

check() {
  version=$1
  command -v docker >/dev/null 2>&1 || die "docker not found"
  docker info >/dev/null 2>&1 || die "docker daemon is not running"
  command -v node >/dev/null 2>&1 || die "node not found"

  work=$(mktemp -d)
  name="atx-glama-check-$$"
  image="atx-glama-check:$version"
  trap 'docker rm -f "$name" >/dev/null 2>&1 || true; docker rmi "$image" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

  build_steps_json "$version" > "$work/steps.json"
  node -e '
const fs = require("fs");
const [stepsPath, base, nodeMajor, proxy] = process.argv.slice(1);
const steps = JSON.parse(fs.readFileSync(stepsPath, "utf8"));
const run = steps.map((s) => `(${s})`).join(" && ");
process.stdout.write(`FROM ${base}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git && curl -fsSL https://deb.nodesource.com/setup_${nodeMajor}.x | bash - && apt-get install -y --no-install-recommends nodejs && npm install -g ${proxy} && apt-get clean && rm -rf /var/lib/apt/lists/*
WORKDIR /app
RUN ${run}
CMD ["mcp-proxy","--","atx-mcp"]
`);
' "$work/steps.json" "$GLAMA_BASE_IMAGE" "$GLAMA_NODE_MAJOR" "$GLAMA_MCP_PROXY" > "$work/Dockerfile"

  echo "==> building Glama-equivalent image (linux/amd64) for v$version"
  docker build -q --platform linux/amd64 -t "$image" "$work" >/dev/null

  echo "==> starting server behind mcp-proxy"
  docker run -d --name "$name" --platform linux/amd64 -p 127.0.0.1::8080 \
    -e ATX_WORKSPACE="$PLACEHOLDER_WORKSPACE" "$image" >/dev/null
  port=$(docker port "$name" 8080/tcp | head -n1 | sed 's/.*://')
  url="http://127.0.0.1:$port/mcp"

  # mcp-proxy は公式 TypeScript SDK で tools/list を検証するため、
  # outputSchema の仕様違反などはここで error として返る(v0.5.0 の不具合はこれで出た)。
  node -e '
const url = process.argv[1], want = process.argv[2];
const H = { "Content-Type": "application/json", Accept: "application/json, text/event-stream" };
const parse = (t) => JSON.parse(t.split("\n").filter((l) => l.startsWith("data: ")).map((l) => l.slice(6)).pop() ?? t);
const post = (body, sid) => fetch(url, { method: "POST", headers: sid ? { ...H, "mcp-session-id": sid } : H, body: JSON.stringify(body) });
(async () => {
  let init;
  for (let i = 0; ; i++) {
    try { init = await post({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "glama-check", version: "0" } } }); break; }
    catch (e) { if (i >= 30) throw e; await new Promise((r) => setTimeout(r, 1000)); }
  }
  const sid = init.headers.get("mcp-session-id");
  const info = parse(await init.text()).result.serverInfo;
  await post({ jsonrpc: "2.0", method: "notifications/initialized" }, sid);
  const list = parse(await (await post({ jsonrpc: "2.0", id: 2, method: "tools/list" }, sid)).text());
  if (list.error) { console.error("tools/list failed:", JSON.stringify(list.error).slice(0, 800)); process.exit(1); }
  const names = list.result.tools.map((t) => t.name);
  console.log(`serverInfo: ${info.name} ${info.version}`);
  console.log(`tools/list: ${names.length} tools: ${names.join(", ")}`);
  if (info.version !== want) { console.error(`version mismatch: expected ${want}, got ${info.version}`); process.exit(1); }
  if (names.length === 0) { console.error("no tools listed"); process.exit(1); }
  console.log("OK");
})().catch((e) => { console.error(e); process.exit(1); });
' "$url" "$version"
}

[ $# -ge 1 ] || die "usage: see header of this script"
case "$1" in
  form)
    [ $# -ge 2 ] || die "usage: glama.sh form build-steps X.Y.Z | cmd | placeholder"
    case "$2" in
      build-steps) [ $# -eq 3 ] || die "usage: glama.sh form build-steps X.Y.Z"; check_version "$3"; build_steps_json "$3" ;;
      cmd) cmd_json ;;
      placeholder) placeholder_json ;;
      *) die "unknown form field: $2" ;;
    esac
    ;;
  check)
    [ $# -eq 2 ] || die "usage: glama.sh check X.Y.Z"
    check_version "$2"
    check "$2"
    ;;
  *) die "unknown command: $1" ;;
esac

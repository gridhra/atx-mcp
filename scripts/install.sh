#!/bin/sh
# Install a prebuilt atx-mcp binary (macOS / Linux).
#
#   curl -fsSL <raw url>/scripts/install.sh | sh
#
# Environment:
#   ATX_VERSION      version to install, e.g. v0.1.0 (default: latest release)
#   ATX_INSTALL_DIR  install directory (default: $HOME/.local/bin)
set -eu

REPO="gridhra/atx-mcp"

INSTALL_DIR="${ATX_INSTALL_DIR:-$HOME/.local/bin}"

die() { printf 'install.sh: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

need uname
need tar
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1"; }
  fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$1"; }
  fetch_to() { wget -qO "$2" "$1"; }
else
  die "need curl or wget"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)          target="aarch64-apple-darwin" ;;
  Darwin/x86_64)         target="x86_64-apple-darwin" ;;
  Linux/x86_64|Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64|Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  *) die "no prebuilt binary for $os/$arch; build from source: https://github.com/$REPO" ;;
esac

version="${ATX_VERSION:-}"
if [ -z "$version" ]; then
  version="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$version" ] || die "could not determine latest release for $REPO"
fi

name="atx-mcp-${version#v}-${target}"
base="https://github.com/$REPO/releases/download/$version"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'Downloading %s %s (%s)\n' "atx-mcp" "$version" "$target" >&2
fetch_to "$base/$name.tar.gz" "$tmp/$name.tar.gz" || die "download failed: $base/$name.tar.gz"

# Verify against the release's SHA256SUMS when a hashing tool is available.
if fetch_to "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)"
  else
    actual=""
  fi
  if [ -n "$actual" ]; then
    expected="$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 | head -n1)"
    [ -n "$expected" ] || die "$name.tar.gz not listed in SHA256SUMS"
    [ "$actual" = "$expected" ] || die "checksum mismatch (expected $expected, got $actual)"
    printf 'Checksum OK\n' >&2
  fi
fi

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$name/atx-mcp" "$INSTALL_DIR/atx-mcp" 2>/dev/null \
  || { cp "$tmp/$name/atx-mcp" "$INSTALL_DIR/atx-mcp" && chmod 755 "$INSTALL_DIR/atx-mcp"; }

printf 'Installed %s\n' "$INSTALL_DIR/atx-mcp" >&2
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf '\nNote: %s is not on your PATH. Add it, or use the full path in your MCP config.\n' "$INSTALL_DIR" >&2 ;;
esac

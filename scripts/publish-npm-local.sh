#!/bin/sh
# 初回 publish 用(Trusted Publishing 設定前にパッケージを作るための一回きりの手動経路)。
# 使い方: npm login 済みの状態で
#   sh scripts/publish-npm-local.sh          # Passkey/WebAuthn の場合(ブラウザ認証が開く)
#   sh scripts/publish-npm-local.sh 123456   # TOTP(認証アプリ)の場合
# 途中で認証が失効したら再実行すれば published 済みはスキップされる。
set -eu
OTP="${1:-}"
cd "$(dirname "$0")/../npm/dist"
# プラットフォームパッケージを先に、ランチャーを最後に(optionalDependencies の解決順)
for pkg in atx-mcp-darwin-arm64 atx-mcp-darwin-x64 atx-mcp-linux-x64 atx-mcp-linux-arm64 atx-mcp-win32-x64 atx-mcp; do
  ver=$(node -p "require('./$pkg/package.json').version")
  if npm view "$pkg@$ver" version >/dev/null 2>&1; then
    echo "==> $pkg@$ver already published, skip"
    continue
  fi
  echo "==> publishing $pkg@$ver"
  if [ -n "$OTP" ]; then
    (cd "$pkg" && npm publish --access public --otp "$OTP")
  else
    (cd "$pkg" && npm publish --access public)
  fi
done
echo "all done"

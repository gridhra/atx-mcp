# RELEASING

メンテナ向け。リリースはタグ push で全自動。

## 0. 一度だけ: `gridhra` プレースホルダの置換

リポジトリ作成前に書いたため、GitHub owner はリテラル `gridhra` になっている。
実際の owner に置換する:

```sh
# 対象を確認
grep -rn gridhra . --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules

# 置換(YOUR_ORG を実際の owner に)
grep -rl gridhra . --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules \
  | xargs sed -i '' 's/gridhra/YOUR_ORG/g'      # macOS / BSD sed
# GNU sed (Linux) の場合は:  xargs sed -i 's/gridhra/YOUR_ORG/g'
```

`RELEASING.md` 自身にも文字列が残るが無害(この節の説明文)。

> CI の中では、`npm/scripts/build-packages.mjs` が publish 時に `$GITHUB_REPOSITORY`
> を使って `gridhra/atx-mcp` を自動置換する。npm provenance は
> package.json の repository URL がビルド元リポジトリと一致することを要求するため、
> 置換を忘れても npm publish は壊れない。ただし README とインストーラの URL は
> 直らないので、上記の置換は必ず行うこと。

## 1. npm 認証 — Trusted Publishing(OIDC、トークン不要)

npm の Classic トークンは 2025-12 に全廃されたため、CI からの publish は
**Trusted Publishing(OIDC)** を使う。リポジトリ Secrets は不要
(`GITHUB_TOKEN` は自動供給。Release 作成のみに使用)。

### 一度だけの設定(パッケージごと)

npmjs.com の各パッケージページ → **Settings → Trusted Publisher** に登録する:

- Organization or user: `gridhra`
- Repository: `atx-mcp`
- Workflow filename: `release.yml`
- Environment: 空欄
- Allowed actions: `npm publish`

対象パッケージ(6個): `atx-mcp`, `atx-mcp-darwin-arm64`, `atx-mcp-darwin-x64`,
`atx-mcp-linux-x64`, `atx-mcp-linux-arm64`, `atx-mcp-win32-x64`

### 初回 publish(パッケージがまだ存在しない場合)

Trusted Publisher はパッケージ単位の設定で既存パッケージが前提。
新パッケージ追加時の初回だけ、npm login 済みのローカルから手動 publish する:

```sh
# GitHub Release の成果物から npm/dist を組み立てた上で
sh scripts/publish-npm-local.sh          # Passkey/WebAuthn(通常のターミナルで実行 — ブラウザ認証が開く)
sh scripts/publish-npm-local.sh 123456   # TOTP の場合
```

publish 後、上記の Trusted Publisher を登録すれば以後のバージョンは CI が自動 publish する。

既知の落とし穴: 類似名パッケージを連続 publish すると npm のスパム検知
(`Package name triggered spam detection`)に当たることがある。
その場合は https://npmjs.com/support に解除依頼を出す(名前変更は別名でも再検知されがちで非推奨)。

## 2. リリース手順

```sh
# 1) workspace version を上げる(Cargo.toml [workspace.package] の version 1 箇所)
vim Cargo.toml
cargo check --workspace          # Cargo.lock を追従させる
git add -A && git commit -m "Release v0.2.0"

# 2) タグを打って push
git tag v0.2.0
git push origin main
git push origin v0.2.0
```

以降は `.github/workflows/release.yml` が実行する:

1. **build**(5 ターゲット並列)— `atx-mcp-<version>-<target>.tar.gz` / `.zip`
   (バイナリ + LICENSE + README を同梱)を作成
2. **release** — 全アーカイブを集めて `SHA256SUMS` を生成し、GitHub Release を作成
3. **npm** — アーティファクトから npm パッケージ群を組み立て、
   `npm publish --provenance --access public` する(認証は Trusted Publishing / OIDC)

npm パッケージのバージョンは常にタグ(先頭 `v` を除いたもの)。
Cargo の version とタグは手動で一致させる必要がある — **不一致のままタグを打たないこと**
(`atx-mcp --version` が Cargo.toml の値を返すため)。

失敗したリリースを作り直す場合は、Actions の "Release" ワークフローを
`workflow_dispatch` で既存タグを指定して再実行できる。

## 3. ビルド環境の前提(変更時の注意)

- **nasm**: `ravif` → `rav1e` が x86_64 の SIMD カーネルを nasm でアセンブルする。
  x86_64 ターゲットのジョブでは nasm を明示インストールしている。aarch64 は
  `cc` 経由の gas を使うので不要。
- **musl**: Linux は静的リンク(glibc フロアなし)。`musl-tools` が提供する `musl-gcc`
  を `cc` crate が自動選択し、libwebp-sys の同梱 C ソースをビルドする。
  もし musl ビルドが通らなくなったら、`release.yml` の matrix で該当エントリの
  `target` を `*-unknown-linux-gnu` に変え、`musl: true` を消す。その場合の
  glibc フロアはランナーの Ubuntu 版に一致する(`ubuntu-24.04` → glibc 2.39)。
- **ランナー**: `ubuntu-22.04*` は 2026-09 に deprecation 開始のため使っていない。
  macOS Intel バイナリは `macos-14`(arm64)からのクロスコンパイル。GitHub の
  Intel macOS ランナーは 2027 年に廃止されるため、最初から依存しない構成にしてある。

## 4. 未実装 / 任意

- **Homebrew tap**: 別リポジトリ(`gridhra/homebrew-tap`)が必要なため未設定。
  欲しくなったら、release ジョブの後に tap リポジトリの Formula を
  SHA256SUMS から書き換えて push するステップを足す。
- **crates.io publish**: していない。`atx-mcp` は crates.io 上で未取得。

## 5. Docs

Note (English): whenever user-facing docs change, keep README.md, README.ja.md,
and README.zh-CN.md in sync — update all three together, not just one.

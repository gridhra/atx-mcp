# RELEASING

メンテナ向け。リリースはタグ push で全自動。

## 0. (履歴) `gridhra` プレースホルダの置換

リポジトリ作成前に `gridhra` を仮の owner 名として書いていたが、
現在は実在する GitHub owner(`gridhra/atx-mcp`)なので、この節の置換作業は
**もう不要**。過去に存在した手順の記録として残す。

なお `npm/scripts/build-packages.mjs` は publish 時に `$GITHUB_REPOSITORY`
(CI が自ら知っているビルド元リポジトリ)で `gridhra/atx-mcp` の文字列を
上書きする。npm provenance は package.json の repository URL がビルド元
リポジトリと一致することを要求するため、フォークや将来のリポジトリ移転が
あっても npm publish 自体は壊れないようにするための仕組み。

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
#    server.json(MCP レジストリ用)の version 2 箇所も同じ値にする
vim Cargo.toml server.json
cargo check --workspace          # Cargo.lock を追従させる
git add -A && git commit -m "Release vX.Y.Z"

# 2) タグを打って push
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z
```

(`vX.Y.Z` は実際のバージョン、例: `v0.3.0`、に読み替える)

タグ push 後の CI 完了を確認したら、**6. Glama** の手順で Glama 側のリリースも行う。

以降は `.github/workflows/release.yml` が実行する:

1. **build**(5 ターゲット並列)— `atx-mcp-<version>-<target>.tar.gz` / `.zip`
   (バイナリ + LICENSE + README を同梱)を作成
2. **release** — 全アーカイブを集めて `SHA256SUMS` を生成し、GitHub Release を作成
3. **npm** — アーティファクトから npm パッケージ群を組み立て、
   `npm publish --provenance --access public` する(認証は Trusted Publishing / OIDC)。
   各パッケージの publish 前に `npm view <name>@<version>` で既存 publish 有無を
   確認し、既に publish 済みならスキップする(部分失敗後の再実行を安全にするため)

npm パッケージのバージョンは常にタグ(先頭 `v` を除いたもの)。
Cargo の version とタグは手動で一致させる必要がある — **不一致のままタグを打たないこと**
(`atx-mcp --version` が Cargo.toml の値を返すため)。

失敗したリリースを作り直す場合は、Actions の "Release" ワークフローを
`workflow_dispatch` で既存タグを指定して再実行できる。npm publish ステップは
上記の idempotency guard によりすでに publish 済みのパッケージを安全に飛ばすので、
一部パッケージだけ失敗したケースの再実行にそのまま使える。

### win32 npm publish の既知の許容(意図的)

`atx-mcp-win32-x64` パッケージは npm のスパム誤検知にたびたび引っかかり、
解除申請中で publish が失敗することがある。`release.yml` はこのパッケージに限り
**publish 失敗を warning にとどめてワークフローを続行**する(他 5 パッケージの
publish は通常どおり止める)。この寛容化は暫定措置で、npm 側のスパムブロックが
解除され安定して publish できるようになったら外してよい。win32 パッケージが
未 publish の間、`npm/atx-mcp/bin/atx-mcp.js` は該当プラットフォームパッケージが
見つからないときのフォールバックメッセージで、この可能性(未 publish)と
GitHub Release ページへの誘導を出す。

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

## 6. Glama

Glama(https://glama.ai 、MCP サーバの登録・評価サイト)は、awesome-mcp-servers への
掲載条件になっている(claim 済み・Glama のチェック通過・スコアバッジ)。Glama は
**フォームから生成した Dockerfile でサーバを起動し、mcp-proxy 越しに tools/list を取れるか**を
チェックする。フォームのビルド手順はバイナリのバージョンを固定しているので、
**atx-mcp をリリースするたびに Glama 側もフォームを更新してリリースし直す**。

作業は Claude Code が Chrome(Claude in Chrome 拡張)で操作し、ログインなどの認証だけを
人に頼む前提で書く。

### 6.1 前提

- GitHub Release `vX.Y.Z` と npm publish が完了している(**2. リリース手順**)
- Docker デーモンが起動している(`docker info` が通る)
- Chrome に Claude in Chrome 拡張が入って接続されている。つながらないときは、
  Chrome が起動しているか、拡張が Claude Code と同じアカウントでログインしているかを確認する
- Glama に GitHub アカウント `gridhra` でログインしている。画面右上が「Sign Up」なら未ログイン。
  **ログインは人が行う**(Admin 画面の「Login with GitHub」から)。Claude はログイン画面に来たら止まって依頼する

### 6.2 ローカルで先に検証する

```sh
sh scripts/glama.sh check X.Y.Z
```

Glama が生成するのと同じ構成(`debian:trixie-slim` + Node + `mcp-proxy`)のイメージを
linux/amd64 でビルドし、mcp-proxy 越しに initialize と tools/list を送る。
`serverInfo` のバージョンが X.Y.Z で、ツール一覧が取れれば `OK` を出す。検証用イメージは終了時に消える。

- 由来: v0.5.0 は outputSchema の最上位に `type: "object"` が無いツールがあり、
  mcp-proxy(公式 TypeScript SDK)が tools/list 全体を拒否して Glama のチェックに通らなかった。
  Rust 側のテストでは検出できなかったので、Glama に出す前にこの経路で確かめる
  (DESIGN.md §9.13)。`sh scripts/glama.sh check 0.5.0` は今でもこの失敗を再現する
- 失敗したら Glama は触らずに原因を直す。Glama に出しても同じ理由で落ちる

### 6.3 Glama のフォームを更新してビルドする

画面: `https://glama.ai/mcp/servers/gridhra/atx-mcp/admin/dockerfile`(Admin → Dockerfile)

1. (任意)Admin → Repository の「Sync Server」で、Glama が把握している最新コミットを更新する
2. 各入力欄をスクリプトの出力で**置き換える**:

   | 欄 | 入れる値 |
   |---|---|
   | Base image | `debian:trixie-slim`(既定のまま) |
   | Node.js version / Python version | 既定のまま |
   | Build steps | `sh scripts/glama.sh form build-steps X.Y.Z` の出力 |
   | CMD arguments | `sh scripts/glama.sh form cmd` の出力(`["atx-mcp"]`。Glama が前に `mcp-proxy --` を付ける) |
   | Environment variables JSON schema | 既定のまま(Glama が README から `ATX_WORKSPACE` を必須として生成済み) |
   | Placeholder parameters | `sh scripts/glama.sh form placeholder` の出力(チェック時に使う仮の値) |
   | Pinned commit SHA | **空欄** |

3. 右側の Dockerfile プレビューを読み、`scripts/glama.sh` の土台(ベースイメージ、Node の版、
   `mcp-proxy` の版、RUN の連結方法)と食い違っていないか見る。違ったらスクリプトの先頭の変数を合わせ、
   6.2 をやり直す
4. **「Build」を押す**(「Build & Release」ではない)。テスト結果のページ
   (`.../admin/dockerfile/tests/<id>`)に移る

入力のコツ(Claude が Chrome で操作するとき):

- 入力欄は CodeMirror(コードエディタ部品)で、普通の textarea ではない。
  **値は `sh scripts/glama.sh form ... | pbcopy` でクリップボードに入れ、欄をクリック →
  cmd+a → cmd+v で貼る**。スクリーンショットで座標を取ってクリックする
  - 由来: JavaScript で DOM に値を入れようとしたら、既存の値の後ろに追記されたうえ、
    ページが応答しなくなった
  - 文字をタイプで入れると、エディタが括弧や引用符を自動で補うので崩れやすい
- Pinned commit SHA に Glama が同期していないコミットを入れると、Build 時に
  「Commit not found」で弾かれる。ビルド手順はリポジトリの中身を使わない(バイナリを Release から取る)
  ので、空欄でよい
  - 由来: v0.5.1 のとき新しいコミットを入れて弾かれ、空欄にして通した

### 6.4 結果を確認して Glama のリリースを作る

1. テスト結果のページは自動で更新されない。**再読み込みして** Status を見る
   (`testing` → `success`。v0.5.1 では 15 秒ほど)
2. 「Instance logs」に、`atx-mcp starting` と、`"result":{"tools":[...` を含む行があることを確かめる。
   `unknown format "uint32" ignored ...` などの警告は mcp-proxy 側の JSON Schema 検証器の出力で、無害
3. ページ下の「Create Release」を押す。Version 欄の既定値は `0.1.0` なので **`X.Y.Z` に直す**。
   Changelog は英語で一行(任意)。「Create & Publish Release」を押す
4. Admin → Releases に `X.Y.Z` が `latest` として出れば完了

失敗したとき: Docker build logs と Instance logs を読む。ビルド手順の失敗なら Release の
ファイル名や SHA256SUMS を、起動後の失敗なら 6.2 と同じ内省エラーを疑う。

### 6.5 注意

- **Auto-Release**(Admin → Releases の切り替え。GitHub Release のたびに Glama が自動で
  ビルド・リリースする)は**オフにしてある**(2026-09-14)。オンに戻さない
  - 由来: ビルド手順がバイナリのバージョンを固定しているため、自動で作られる版は
    フォームに入っている古いバージョンのバイナリになり、それが `latest` として出てしまう。
    Glama のリリースは毎回この節の手順で作る
  - 画面でオンになっていたら(Glama 側の既定変更などで)、オフに戻してから 6.3 に進む
- Admin → Score の「Tool Definition Quality」と「Server Coherence」は、Glama のリリース後に
  非同期で採点される。awesome-mcp-servers の掲載条件は claim・チェック通過・バッジで、点数の下限はない
- Glama の画面構成やフォームの既定値は変わりうる。この節の記述と画面が違ったら、
  画面を正として進め、終わったらこの節を直す

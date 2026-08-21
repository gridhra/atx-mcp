# atx-mcp

[English](README.md) | **日本語** | [简体中文](README.zh-CN.md)

汎用 AI エージェント向けの、決定論的(非生成)アセット変換 MCP サーバ。Rust 製。

編集意図(「水平にして 16:9 に整えて軽く明るく」)を宣言的な変換レシピとして実行し、
すべての結果を immutable な revision として追跡する。原本は決して変更されない。

設計の全体は [docs/DESIGN.md](docs/DESIGN.md) を参照。

## インストール

Rust toolchain は不要。以下のいずれかを選ぶ。

### 1. npx(最も手軽・推奨)

Node.js 18+ があれば他に何も要らない。プラットフォーム対応のネイティブバイナリが
`optionalDependencies` 経由で自動的に入る。

```sh
claude mcp add asset-transform -- npx -y atx-mcp --workspace /path/to/asset-workspace
```

MCP クライアントの設定ファイルに直接書く場合:

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

### 2. ビルド済みバイナリ

インストーラスクリプト(既定の設置先は `~/.local/bin`、Windows は
`%LOCALAPPDATA%\Programs\atx-mcp`。SHA256SUMS で検証してから展開する):

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/gridhra/atx-mcp/main/scripts/install.ps1 | iex
```

手動で落とす場合は [Releases](https://github.com/gridhra/atx-mcp/releases) から
`atx-mcp-<version>-<target>.tar.gz`(Windows は `.zip`)を取得する。提供ターゲット:

| プラットフォーム | ターゲットトリプル |
|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl`(静的リンク・glibc 不要) |
| Linux arm64 | `aarch64-unknown-linux-musl`(静的リンク・glibc 不要) |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

```sh
claude mcp add asset-transform -- ~/.local/bin/atx-mcp --workspace /path/to/asset-workspace
```

### 3. ソースからビルド(上記以外のプラットフォーム)

必要なもの: Rust toolchain と C コンパイラ(libwebp のソース同梱ビルド用)のみ。

```sh
cargo build --release
# => target/release/atx-mcp
claude mcp add asset-transform -- "$PWD/target/release/atx-mcp" --workspace /path/to/asset-workspace
```

---

`--workspace`(env: `ATX_WORKSPACE`)はアセットストアのディレクトリ。存在しなければ作成される。

## ツール(10)

| ツール | 役割 |
|---|---|
| `list_operations` | レシピ語彙の軽量カタログ。全 op を1行説明 + パラメータの型/値域ヒント付きで返し、末尾にビルトインプリセット名も載せる。`category:"geometry"\|"color"\|"filter"\|"output"` で絞り込み可(read-only) |
| `explain_operation` | 1つの op の完全なリファレンス。パラメータ表(型・値域・必須/既定値・意味)、そのまま貼れる JSON 例、落とし穴を返す。未知の名前には有効な op 名一覧を返す(read-only) |
| `import_asset` | ローカル画像をワークスペースへ取り込み(sha256 冪等) |
| `inspect_image` | 寸法・EXIF・ICC・GPS 有無などの検査(read-only) |
| `detect_tilt` | Canny+Hough(粗)+ 投影プロファイル(0.1° 未満の細分)による傾き角推定。水平族/垂直族の推定とスコア曲線も返す。confidence 低なら「補正しない」を返す(read-only) |
| `render_preview` | レシピ(または `preset`)を低解像度(長辺 ≤768)で適用、インライン画像付きで返却。`overlay:"grid"\|"thirds"\|"horizon"` で構図確認用のガイド線を重ねられる(プレビューのみに描画、本適用には影響しない) |
| `apply_transform` | レシピ(または `preset`)を高解像度適用し新 revision を発行(同一レシピ→同一 revision) |
| `compare_revisions` | 2つの revision を長辺 ≤640 に縮小し、`layout:"side_by_side"\|"stacked"` で1枚に並べてインライン画像で返却(A/B・before/after の視覚比較用) |
| `list_assets` | revision 台帳の参照(read-only) |
| `export_asset` | revision を指定パスへ書き出し(既存ファイルは `overwrite:true` 明示時のみ上書き) |

## レシピ例

```json
{
  "operations": [
    { "op": "rotate", "angle_degrees": -1.8 },
    { "op": "crop", "aspect_ratio": "16:9" },
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

対応 op(18種): `auto_orient` / `rotate` / `perspective` / `crop`(crop・pad)/ `resize`(cover・contain・fill)/
`adjust` / `color_matrix` / `curves` / `levels` / `lut` / `white_balance` / `hsl` /
`blur` / `median` / `unsharp_mask` / `convolve` /
`encode`(jpeg・png・webp・avif)/ `strip_metadata`。
op 一覧はツールのスキーマにあえて埋め込んでいない。最新のカタログは `list_operations`、
個々の op の完全なスキーマ・例・注意点は `explain_operation` で取得する。

### LUT(.cube)

`.cube` の 3D/1D LUT は画像ではなく**アセット**である。先に取り込み、
生成された revision をレシピから参照する。

1. `.cube` ファイルを `import_asset` する。`mime_type: "application/x-cube"` の
   不変 revision として格納される(画像ではないので `inspect_image` は意図的に
   構造化エラーを返す)。
2. 返ってきた `revision_id` をレシピから参照する:

```json
{ "op": "lut", "lut_revision_id": "rev_...", "strength": 0.8 }
```

`strength`(0..1、既定 1.0)は元画像との線形ブレンド。revision は不変なので、
参照 id を `recipe_hash` に含めるだけで決定論が保たれる。一方で、そのレシピの
再現はその LUT を持つワークスペース内でのみ保証されるため、ルックを別環境へ
移すときは `.cube` ごと移すこと。存在しない id を参照した場合は、画素処理に
入る前に構造化エラーで返る。

## プリセット

`apply_transform` / `render_preview` は `recipe`(生の DSL)と
`preset`([`presets/`](presets) 同梱の名前付きレシピ)のどちらか一方を受ける(排他・どちらか必須):

| プリセット | 内容 |
|---|---|
| `eyecatch_16_9` | 16:9 に中央クロップ → 幅 1600px → WebP q82 |
| `film_soft` | フィルム風の柔らかさ: 緩い S 字カーブ + 輝度側へ 15% の脱色 |
| `product_clean` | EC 商品向けの清潔感: ほぼ中立の WB + レベル調整 + 軽いシャープ |
| `thumbnail_square` | 1:1 に中央クロップ → 800x800 → WebP q80 |
| `web_optimize` | 拡大せず 2000x2000 に収める → WebP q80 |
| `grayscale` | BT.709 輝度の `color_matrix` による白黒化 |
| `sepia` | `color_matrix` による古典的セピア |

プリセットは純粋な糖衣である: 解決後は通常のレシピとして同じパイプラインを流れ、
`recipe_hash`(冪等キー)は**解決後のレシピ**に対して計算される。
つまり preset 指定と、同じ内容の生レシピ指定は同一 revision に落ちる。

## 保証

- **決定論**: 同一入力 + 同一レシピ → バイト同一出力(ゴールデンテストで回帰検証)
- **冪等**: レシピは正規化(キー順ソート + f64 の 1e-6 グリッド量子化)して sha256 化。
  `(入力 revision, レシピ hash)` が同じなら既存 revision を返す
- **原本保護**: objects/ は content-addressed の追記のみ。削除・上書き API は存在しない

## 開発

```sh
cargo test --workspace     # ユニット + 統合 + プロパティ(proptest)テスト
cargo clippy --workspace --all-targets -- -D warnings
```

クレート構成: `atx-core`(レシピ・変換エンジン)/ `atx-geometry`(傾き検出)/
`atx-store`(immutable アセットストア)/ `atx-mcp`(rmcp stdio サーバ)。

リリース手順は [RELEASING.md](RELEASING.md) を参照。

## ライセンス

MIT。[LICENSE](LICENSE) を参照。

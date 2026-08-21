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

## ツール(11)

| ツール | 役割 |
|---|---|
| `list_operations` | レシピ語彙の軽量カタログ。全 op を1行説明 + パラメータの型/値域ヒント付きで返し、末尾にビルトインプリセット名も載せる。`category:"geometry"\|"color"\|"filter"\|"output"` で絞り込み可(read-only) |
| `explain_operation` | 1つの op の完全なリファレンス。パラメータ表(型・値域・必須/既定値・意味)、そのまま貼れる JSON 例、落とし穴を返す。未知の名前には有効な op 名一覧を返す(read-only) |
| `import_asset` | ローカル画像をワークスペースへ取り込み(sha256 冪等) |
| `inspect_image` | 寸法・EXIF・ICC・GPS 有無などの検査(read-only) |
| `detect_tilt` | Canny+Hough(粗)+ 投影プロファイル(0.1° 未満の細分)による傾き角推定。水平族/垂直族の推定とスコア曲線も返す。confidence 低なら「補正しない」を返す(read-only) |
| `generate_mask` | 決定論的なグレースケールマスク(`linear_gradient` / `radial_gradient` / `luminosity_range` / `color_range`)を、参照画像と同寸法の PNG revision として生成する。op の `mask` フィールドから参照して使う(冪等) |
| `render_preview` | レシピ(または `preset`)を低解像度(長辺 ≤768)で適用、インライン画像付きで返却。`overlay:"grid"\|"thirds"\|"horizon"` で構図確認用のガイド線を、`overlay:"mask"`(+ `mask_revision_id`)でマスクの被覆を重ねられる(プレビューのみに描画、本適用には影響しない) |
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

### マスク(部分適用)

マスクは**グレースケールの画像 revision** である。BT.709 輝度がそのまま重みで、
白 = その op を全量適用、黒 = その画素には適用しない。トーン系・フィルタ系の 11 op
(`adjust` / `color_matrix` / `curves` / `levels` / `hsl` / `lut` / `white_balance` /
`blur` / `median` / `unsharp_mask` / `convolve`)が受け取れる。

1. `generate_mask` が、参照画像と**厳密に同じ寸法**のマスクを決定論的に作る:

| `kind` | パラメータ | 選択されるもの |
|---|---|---|
| `linear_gradient` | `angle_degrees`(0 = 上が白で下へ向かって黒、正で時計回り)、`start` / `end`(軸上で重みが 1→0 になる位置、0..1) | ハーフ ND(空・手前) |
| `radial_gradient` | `center_x` / `center_y`(0..1 の相対位置)、`radius`(対角線の半分に対する比 0..1)、`feather`(0..1 の追加減衰帯) | ビネット・被写体スポット |
| `luminosity_range` | `min` / `max`(0..255)、`feather`(帯の外側の減衰幅、輝度単位) | ハイライト・中間調・シャドウ |
| `color_range` | `hue_center`(0..360)、`hue_width`(片側幅 1..180)、`feather`(追加の度数) | 特定の色相域(空の青・葉の緑) |

   自前のグレースケール画像を `import_asset` して使ってもよい。

2. 返ってきた `revision_id` を op に付ける:

```json
{ "op": "curves", "master": [[0,0],[128,168],[255,255]],
  "mask": { "revision_id": "rev_...", "invert": false, "feather_px": 8.0 } }
```

   `invert`(既定 `false`)は重みを `1-w` に反転する。`feather_px`(既定 `0.0`)は
   マスク境界を、現在の画像座標でのガウス σ [px] だけぼかす。

3. `render_preview` に `overlay:"mask"` と `mask_revision_id` を渡すと、重みが 0.5 を
   超える領域を赤で、それ以外を少し暗く塗ったプレビューが返る。本適用の前に
   被覆を目視で確認できる。

マスクの参照は LUT と同じ仕組みなので、注意点も同じ: 参照 id は `recipe_hash` に
含まれ、そのレシピの再現はそのマスクを持つワークスペース内でのみ保証される。

### レイヤー

レシピは、直列の `operations` の代わりに(または併用で)`layers` スタックを
持てる。レイヤーは下から上へ合成され、各レイヤーの `ops` はまず自分のソース
に対して適用され、その結果が現在の合成結果へブレンドされる:

```json
{
  "layers": [
    { "source": "base", "ops": [] },
    {
      "source": { "revision_id": "rev_..." },
      "ops": [{ "op": "blur", "sigma": 8 }],
      "blend_mode": "multiply",
      "opacity": 0.6
    }
  ],
  "operations": [
    { "op": "resize", "width": 1600 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```

- `source` は `"base"`(`apply_transform` / `render_preview` に渡した入力
  revision)か `{"revision_id": "rev_..."}`(ワークスペース内の他の revision)
  のどちらか。全レイヤーのソースは base 画像と寸法が完全一致していなければ
  ならず、そうでなければ画素処理に入る前に構造化エラーで返る。
- `ops` は通常の operations 列で、そのレイヤーのソースだけに適用される。
- `mask` / `blend_mode`(既定 `"normal"`)/ `opacity`(既定 `1.0`)が、
  下のレイヤーへの合成のされ方を決める。
- ブレンドモードは W3C の separable 12 種のいずれか: `normal` / `multiply` /
  `screen` / `overlay` / `darken` / `lighten` / `color_dodge` / `color_burn` /
  `hard_light` / `soft_light` / `difference` / `exclusion`。
- `layers` がある場合、トップレベルの `operations` は合成結果に対する
  **仕上げパス**になる。`resize` や最後の `encode` はここに置く
  (`encode` は従来どおり最後に1回だけ)。
- 完全なリファレンスは `explain_operation {"operation":"layers"}` を呼ぶこと。

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

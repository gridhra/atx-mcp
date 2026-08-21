# asset-transform-mcp — 要件定義・設計書

作成日: 2026-08-21 / ステータス: v1 実装済(M0+M1+M2 の大半)。実装で確定した差分は §9 を参照

汎用 AI エージェント向けの、**決定論的(非生成)アセット変換 MCP サーバ**。Rust 製。
編集意図(「水平にして 16:9 に整えて軽く明るく」)を、再現可能・監査可能な変換レシピとして実行する層を提供する。

---

## 1. プロダクト定義

### 1.1 ゴール
- コンテンツメディア制作における画像素材の非生成的加工(傾き補正・クロップ・リサイズ・フォーマット変換・明るさ調整等)を、MCP 経由で AI に安全に委譲できるようにする
- LLM は画素を触らず、**意図 → 宣言的レシピ → 決定論的実行**というパイプラインに徹する
- 原本は不変(immutable)。すべての変換は新しい revision を生成し、レシピとともに追跡可能

### 1.2 非ゴール
- 画像生成・インペインティング等の生成的処理(外部モデルの責務)
- Photoshop 的な対話式編集 UI
- 動画処理(将来拡張の余地は残すが v1 スコープ外)

### 1.3 競合状況(2026-08 調査)
既存の画像系 MCP サーバは (a) 生成 API のラッパー、(b) ファイルパス入出力のみの単機能変換、のいずれかに偏っており、
**「ローカル決定論的変換 + アセットストア抽象 + トークン規律あるプレビュー返却 + structured output による連鎖可能性」を兼ね備えたものは存在しない**。ここが本プロダクトの差別化点。

---

## 2. 技術選定(調査結果に基づく)

### 2.1 MCP SDK
- **`rmcp`(公式 modelcontextprotocol/rust-sdk、v3.x)** を採用
  - MCP spec 2026-07-28 対応(2025-11-25 以前へも自動バージョンネゴシエーション)
  - `#[tool]` / `#[tool_router]` マクロ、`schemars` による JSON Schema 自動生成
  - `outputSchema` / `structuredContent`、tool annotations、resource、cursor ページネーション対応
  - tokio ベース
- トランスポートは **stdio を第一**(Claude Code / Claude Desktop のローカル利用)。Streamable HTTP は後段(spec はステートレス化に向かっているため設計上はステートレス前提を維持)

### 2.2 画像処理スタック(ポータビリティ優先)
| 責務 | クレート | 備考 |
|---|---|---|
| デコード/エンコードハブ | `image` 0.25.x | 純 Rust |
| リサイズ | `fast_image_resize` 6.x | SIMD、純 Rust、最速級 |
| 幾何変換・フィルタ・CV | `imageproc` 0.27.x + `image::imageops` | 回転(任意角・補間指定)、Canny、Hough |
| 傾き検出 | `imageproc::edges::canny` + `imageproc::hough` + 自前ヒューリスティック | 専用クレートは存在しない(自作が標準) |
| EXIF 読み取り | `kamadak-exif` 0.6.x | 読み取り専用、成熟 |
| EXIF 書き込み/剥離 | `little_exif` 0.6.x | 純 Rust で唯一の read+write |
| JPEG エンコード | `jpeg-encoder`(デフォルト)/ `mozjpeg`(feature flag でオプトイン) | デフォルトビルドを C 依存最小に保つ |
| WebP(lossy) | `webp` crate(libwebp FFI、ソース同梱ビルド) | 純 Rust の lossy WebP エンコーダは存在しない。C コンパイラのみ必要 |
| AVIF エンコード | `ravif` 0.13.x | 純 Rust(rav1e)。CPU 重いがバッチ用途では許容 |
| ICC | v1 では sRGB 前提 + プロファイル温存。必要になったら `lcms2`(軽量 C 依存)を feature flag で | |

**ビルド時のシステム依存: C コンパイラのみ**(libwebp-sys 用)。OpenCV / libvips は不採用
(ビルド・デプロイ負担が「cargo build で動く」という要件と矛盾。libvips は将来の性能エスケープハッチとして文書化のみ)。

---

## 3. アーキテクチャ

### 3.1 Cargo workspace 構成
```
asset-transform-mcp/
├── Cargo.toml                # workspace
├── crates/
│   ├── atx-core/             # レシピ型定義・正規化・ハッシュ・変換エンジン(MCP 非依存)
│   ├── atx-geometry/         # 傾き検出(Canny + Hough + 角度ヒューリスティック)
│   ├── atx-store/            # アセットストア(immutable revision、content-addressed)
│   └── atx-mcp/              # rmcp サーバ本体(bin)。ツール定義・annotations・プレビュー生成
└── docs/
```
`atx-core` を MCP から切り離すことで、CLI(`atx apply recipe.json in.jpg`)としても同じエンジンを公開できる(要件の「CLI との相性」)。

### 3.2 アセットストア(local-first)
- ワークスペースディレクトリは **起動時設定(CLI 引数 / env)で明示指定**(MCP Roots は 2026-07-28 RC で非推奨のため使わない)
- 構造:
```
<workspace>/
├── objects/<sha256[0..2]>/<sha256>.<ext>   # content-addressed、immutable
├── assets.jsonl                            # asset / revision メタデータ台帳(追記型)
└── previews/                               # 低解像度プレビュー(TTL 掃除対象)
```
- 原本の上書き・削除は API 上存在しない
- `apply_transform` は `(inputRevisionId, canonicalRecipeHash)` で冪等: 同一入力 + 同一正規化レシピ → 既存 revision を返す
- レシピは JSON 正規化(キー順序・数値表現の正規化)後に sha256 を取り、revision に永続化 → 再実行・監査・テンプレート化が可能

### 3.3 コア型
```rust
struct AssetRevision {
    asset_id: String,          // "ast_..." 論理アセット
    revision_id: String,       // "rev_..." 不変スナップショット
    source_revision_id: Option<String>,
    width: u32, height: u32,
    mime_type: String,
    byte_size: u64,
    sha256: String,
    path: PathBuf,             // workspace 内
    recipe: Option<TransformRecipe>,   // 由来レシピ
    recipe_hash: Option<String>,
    created_at: DateTime<Utc>,
}
```

### 3.4 変換レシピ DSL(serde tagged enum)
```json
{
  "input_revision_id": "rev_01J...",
  "operations": [
    { "op": "auto_orient" },
    { "op": "rotate", "angle_degrees": -1.8, "crop": "largest_inscribed_rect" },
    { "op": "crop", "aspect_ratio": "16:9", "anchor": "center" },
    { "op": "resize", "width": 1600, "fit": "cover", "without_enlargement": true },
    { "op": "adjust", "brightness": 0.05, "contrast": 0.0, "saturation": 0.0 },
    { "op": "encode", "format": "webp", "quality": 82 }
  ]
}
```
v1 のオペレーション: `auto_orient` / `rotate` / `crop`(aspect_ratio or 矩形指定, pad 対応)/ `resize` / `adjust`(brightness, contrast, saturation, sharpness)/ `encode`(jpeg, png, webp, avif)/ `strip_metadata`(exif/gps 選択剥離)。

パイプラインは検証 → 各 op を順次適用 → 単一パスでエンコード。失敗は op 単位でエラー位置を構造化返却。

---

## 4. MCP ツール仕様(v1)

| ツール | 役割 | annotations |
|---|---|---|
| `import_asset` | ローカルパスからワークスペースへ取り込み、revision 発行 | readOnly:false, destructive:false, idempotent:true(同一 sha256 → 同一 revision) |
| `inspect_image` | 寸法・フォーマット・EXIF 要約・色情報・容量 | readOnly:true |
| `detect_tilt` | Canny+Hough による傾き角候補 + confidence + warnings | readOnly:true |
| `apply_transform` | レシピを高解像度適用、新 revision 発行 | readOnly:false, destructive:false, idempotent:true |
| `render_preview` | レシピを低解像度適用、サムネイルを inline 返却 | readOnly:false(preview 生成), destructive:false, idempotent:true |
| `list_assets` / `get_asset` | 台帳参照(系譜・レシピ含む) | readOnly:true |
| `export_asset` | revision をワークスペース外の指定パスへ書き出し | readOnly:false, destructive:false(既存ファイルは上書きせずエラー、`overwrite:true` 明示時のみ可) |

全ツール共通で `openWorldHint: false`(ローカル完結。URL フェッチは v1 では提供しない = SSRF 面を最初から閉じる)。

### 4.1 返却パターン(トークン規律)
調査結果の要点: Claude 系クライアントは tool result の ImageContent を折りたたむ/端末では表示できないため、画像が「勝手に見える」前提にしない。

- `structuredContent` + `outputSchema`: revision_id、寸法、容量、フォーマット、sha256、警告、適用レシピを機械可読で返す(連鎖呼び出しの主経路)
- `content`:
  - テキストで結果サマリ + ファイルパス(人間・非対応クライアント向け)
  - `render_preview` のみ、長辺 ~768px の inline ImageContent(base64)を付与(モデルが見た目確認する用)
  - フル解像度は `resource_link`(`file://` URI)で参照返却し、バイナリは往復させない

### 4.2 detect_tilt の返却例
```json
{
  "recommended_angle_degrees": -1.8,
  "confidence": 0.87,
  "method": "hough_projection_fused",
  "alternatives": [ { "angle_degrees": -1.4, "score": 0.81 } ],
  "horizontal_angle_degrees": -1.82,
  "horizontal_confidence": 0.95,
  "horizontal_support": 0.62,
  "vertical_angle_degrees": -1.74,
  "vertical_confidence": 0.71,
  "vertical_support": 0.38,
  "score_curve": [ { "angle_degrees": -15.0, "score": 0.02 }, { "angle_degrees": -1.8, "score": 1.0 } ],
  "warnings": ["Rotation + crop will remove ~3.2% of pixels"]
}
```
- 角度推定は Hough(長い直線の粗い候補)+ 投影プロファイル(短く途切れたエッジに強い細分)の合成。
  Hough が直線を取れないシーンでは投影プロファイル単独(`method: "projection_profile"`)になる
- `horizontal_*` / `vertical_*` は水平族・垂直族**それぞれ単独**の推定。両者が食い違う
  (例: 水平 -0.5°、垂直 0°)場合はロールではなくカメラ位置・パースが原因であり、
  回転補正が正解とは限らない。警告にも出す
- `score_curve` は探索範囲全体の正規化スコア(補正角の昇順・最大 300 点)。ピークの鋭さ・
  多峰性をクライアントが判断するために返す。最大値 1.0 の点が `recommended_angle_degrees`
- confidence < 閾値(例 0.5)なら `recommended_angle_degrees: null` を返し「補正しない」を正解とする
- 人物アップ・商品単体・抽象写真は「検出不能」を正しく返すことを品質要件とする
- 自動適用はしない: 検出(read-only)と適用(apply_transform)を必ず分離し、判断はホスト AI 側に委ねる

---

## 5. 品質・安全要件

- **決定論**: 同一入力 + 同一レシピ → バイト同一出力(エンコーダのバージョンを Cargo.lock で固定。出力に engine version を記録)
- **原本保護**: objects/ は追記のみ。削除系ツールは v1 に存在しない
- **入力ガード**: 最大画素数(例 100MP)・最大ファイルサイズ・MIME スニッフィング(拡張子ではなくマジックバイト)・デコード爆弾対策(`image` の limits API)
- **パス検証**: import/export のパスはワークスペース設定・許可ディレクトリに対して正規化検証(traversal 防止)
- **メタデータ**: 変換時は EXIF Orientation を正規化(タグ 1 化 or 剥離)。GPS 等の PII は `strip_metadata` で明示的に落とせる。デフォルトは「ICC 温存・EXIF 温存(Orientation のみ正規化)」
- **エラー**: すべて構造化(op index、原因、リカバリ指針)。LLM が自己修復できる粒度で返す

## 6. テスト戦略

- ゴールデンテスト: 固定入力画像 + レシピ → 出力 sha256 一致(決定論の回帰検証)
- 傾き検出: 既知角度で人工的に回転させた画像セットで誤差 ±0.1° 以内を検証(格子・地平線・破線 + ノイズ)。
  検出不能ケース(単色・人物アップ)で null を返すことを検証。水平/垂直の分離とスコア曲線の健全性も検証
- レシピ正規化: 意味的に同一なレシピの hash 一致
- MCP 層: rmcp の in-process transport で tool call の統合テスト

## 7. マイルストーン

- **M0(基盤)**: workspace 雛形、atx-store(import/台帳/冪等)、inspect_image、MCP サーバ起動(stdio)
- **M1(変換 MVP)**: レシピ DSL、auto_orient / rotate / crop / resize / encode(jpeg,png,webp)、apply_transform、render_preview、export_asset、ゴールデンテスト
- **M2(知覚)**: detect_tilt(Canny+Hough)、adjust(明るさ等)、avif、strip_metadata、CLI バイナリ
- **M3(拡張候補)**: saliency / 顔検出を避けるスマートクロップ、CMS 別バリアントプリセット、Streamable HTTP、lcms2 カラーマネジメント

## 8. 既知のリスク・割り切り

1. lossy WebP は libwebp FFI 必須(純 Rust 実装が存在しない)→ C コンパイラ 1 個の依存は許容
2. AVIF エンコードは CPU 遅(rav1e)→ バッチ前提、preview は常に webp/jpeg
3. ImageContent のクライアント表示は不安定 → パス + structuredContent を常に正とする
4. 傾き検出は建築・風景に強く、被写体依存で不能ケースあり → confidence と「補正しない」規約でプロダクト信頼を守る
5. MCP spec がステートレス化へ移行中(2026-07-28 RC)→ サーバ内セッション状態を持たない設計を維持

## 9. 実装ノート(2026-08-21 実装時に確定した差分)

設計からの意図的な変更・確定事項。コードのドキュメントコメントにも同内容を記載済み。

1. **EXIF Orientation はデコード時に常に正規化**する(`auto_orient` op の有無に依らない)。
   v1 は再エンコード時に EXIF を落とすため、条件付きにすると誤回転画像を無警告で出力しうる。
   `auto_orient` は明示的な no-op として残置。
2. **レシピ正規化は f64 を 1e-6 グリッドに量子化**してからハッシュする。
   serde_json のテキスト往復で f64 が 1 ULP ずれる(約10%の値で発生、proptest により検出)ため、
   量子化なしでは「クライアントが canonical JSON をエコーバックすると別ハッシュ」となり
   冪等性保証が破れる。レシピの float フィールドは 1e-6 を意味精度と定義する。
3. **アスペクト比クロップ/パッドの寸法計算は固定点反復**(最大8回、実測最悪2回で収束)。
   丸めによる分岐反転で非冪等になるバグを proptest が検出したため。1回の適用で安定寸法に直行する。
4. **ICC プロファイルの温存は v1 では JPEG 出力のみ**。PNG/WebP/AVIF 出力では警告付きで破棄。
   `strip_metadata{gps}` は v1 では all と同挙動(EXIF ごと破棄、警告で明示)。
5. **`render_preview` は単一パス**: レシピ末尾の encode を差し替え、
   `resize(contain 768) + encode(jpeg q80)` を付加して原本から1回で生成する
   (中間成果物の再デコード不要、幾何は apply_transform と同一)。
   コストはフル解像度適用とほぼ同等である点に注意。
6. **傾き検出は自前 Hough**(0.1° ビン・水平/垂直帯域限定・勾配方向で帯域振り分け)。
   imageproc 標準の `detect_lines` は 1° 分解能固定で ±0.3° 要件を満たせないため。
   人工画像での実測精度は最悪 0.014°。
7. `ENGINE_VERSION = "atx-core/1"` を維持(クロップ固定点化は挙動変更だが、リリース前で
   既存ストアが存在しないため据え置き。リリース後の挙動変更からバンプする)。

### テスト実績(実装完了時点)

- ワークスペース全体: 89 テスト green(ユニット + 統合 + proptest 14 性質)、clippy -D warnings クリーン
- E2E(実バイナリ、stdio JSON-RPC、1477x1108 JPEG フィクスチャ):
  import → detect_tilt(+0.04°, conf 0.69)→ apply_transform(rotate + 16:9 + resize + webp)
  → 再適用で reused:true(冪等)→ preview(インライン 768px JPEG)→ export、全工程確認済み
- proptest が発見し修正した実バグ2件: §9-2, §9-3

### 9.1 追補(2026-08-21): SOURCE 座標系クロップ と strip_metadata{exif}

現場フィードバック起点の atx-core 拡張。既存レシピの `recipe_hash` とゴールデン出力は不変。

8. **`Crop` に `coordinate_space`(`current` 既定 / `source`)を追加**。
   `rotate` + `largest_inscribed_rect` の後は画像が縮む(1477x1108 のフィクスチャで 1467x1095)ため、
   利用者が新座標系でのクロップ原点を手計算する必要があった。`source` を指定すると
   **入力画像(EXIF orientation 正規化前)の座標系**で矩形を書ける。
   - serde は `#[serde(default, skip_serializing_if = "CoordinateSpace::is_current")]`。
     既定値のときは正規化 JSON にフィールドが現れないので、`coordinate_space` を書かない
     既存レシピの canonical JSON はバイト単位で従来と一致し、ハッシュも不変
     (§9-7 のゴールデン `884ea1…` は据え置き)。
   - `source` は `rect` 専用。`aspect_ratio` との併用は validate エラー
     (アスペクト比には写す座標系が存在しないため)。
9. **エンジンが「SOURCE 画素座標 → CURRENT パイプライン座標」のアフィン変換(2x3 f64)を保持する**
   (`crates/atx-core/src/transform.rs`)。幾何 op ごとに合成していく:
   - EXIF orientation 正規化(反転・四半回転)/ `rotate`(中心回転 + 内接矩形または
     全体キャンバスのオフセット平行移動)/ `crop`(負の平行移動)/ `pad`(正の平行移動)/
     `resize`(スケール、`fit=cover` の内部中央クロップ平行移動を含む)
   - `adjust` / `encode` / `strip_metadata` は座標を動かさない
   - 行列は**連続座標**(画素 index `i` は `[i, i+1)`)で定義する。リサイズ `u' = s·u` と
     反転 `u' = w - u` が連続座標でのみ厳密に線形になるため。
     `imageproc` の `warp` 系は index 座標で回転中心を `(w/2, h/2)` に置くので、
     任意角回転だけは連続座標へ換算した `(w/2 + 0.5, h/2 + 0.5)` を中心に使う。
   - `source` 矩形は 4 隅を写して**軸並行外接矩形**を取り(= 回転後は「見た目上傾いた四角形」
     ではなくその外接矩形。元矩形よりわずかに大きい)、half-away-from-zero で丸め、
     現在の画像範囲へクランプする。クランプ時は `EncodedOutput::warnings` に記録し、
     交差が空なら写像後の座標を含む構造化エラー(`AtxError::Operation`)を返す。
10. **`strip_metadata` に scope `exif` を追加**: EXIF(GPS 含む)が確実に無いことを保証しつつ
    **ICC プロファイルは温存する**(Web 配信で色が動かないことを優先)。
    `all` は従来どおり ICC も落とす。`exif` でも ICC を実際に埋め込めるのは JPEG 出力のみで、
    PNG/WebP/AVIF では従来どおり警告付きで破棄(§9-4 の制約は据え置き)。
    enum の variant 追加は既存 2 値の正規化表現を変えないため、ハッシュも不変。
11. `ENGINE_VERSION` は `atx-core/1` のまま。既定レシピ(新フィールド無し)の出力バイト列は
    1 バイトも変わっていない(この時点のゴールデンは `99b05d96…`。§9.2 のフィクスチャ差し替えで値のみ移動)。

#### 追補時点のテスト実績

- `cargo test -p atx-core`: 82 テスト green(lib ユニット 5 / derived 7 / engine 43 /
  proptest_engine 6 性質 / proptest_recipe 4 性質 / recipe 17)、
  `clippy -p atx-core --all-targets -D warnings` クリーン、`cargo check --workspace --all-targets` も通過
- 追加した検証: マーカー矩形つき合成画像による source 座標追従(rotate -1.8° 内接 / EXIF
  orientation=6 / resize / rotate90+cover+crop の連鎖)、幾何 op が無いときの
  source ≡ current 等価性、クランプ警告と空交差エラー、`coordinate_space` 既定値の
  ハッシュ不可視性、`StripScope::Exif` の ICC 温存 + EXIF 不在(出力バイト列に
  `Exif\0\0` が残らないことまで確認)
- 追加した性質(proptest #9): SOURCE 全域を指す矩形は、前段の幾何チェーン
  (rotate 小角度 / resize / aspect crop)が何であってもエラーにならず、
  出力寸法はその時点の画像寸法を超えない

### 9.2 追補(2026-08-21): テストフィクスチャ方針

12. **リポジトリに置くテスト画像は完全合成のもののみ**とする。`tests/fixtures/synthetic_scene.jpg`
    (1477x1108、APP2 ICC あり / EXIF なし)は `cargo run -p atx-core --example gen_fixture` で
    決定論的に再生成でき、第三者・個人の写真素材はリポジトリに含めない。
    エンジン挙動は不変なので `ENGINE_VERSION` は据え置き、ゴールデン出力 sha256 のみ
    新フィクスチャに対して張り直した(`bc05827c…`)。

### 9.3 v0.2 追補(2026-08-21)

- op を 7→14 に拡張: `perspective`(quad / キーストーン角、変換追跡は 3x3 射影行列へ拡張)、
  `color_matrix`(4×5)、`curves`(Fritsch–Carlson 単調3次 → 256 LUT)、`levels`、
  `blur` / `median` / `unsharp_mask`(ガウスカーネルは f64 生成 → 1e-6 量子化で libm 差遮断)
- ツールを 8→10 に拡張(語彙参照系): `list_operations` / `explain_operation`。
  §4 の表は v1 時点の記録として残し、現行のツール一覧は README とコード(vocab.rs)を正とする
- `apply_transform` / `render_preview` に `preset`(recipe と排他)。同梱プリセット5種
  (presets/)。冪等性キーは解決後レシピの hash
- evals/ にエージェント eval ハーネス(実務タスク10本、リリース前の手動ゲート)
- tests/f32_spike.rs: Phase B 前提の f32 クロスプラットフォーム決定論スパイク
  (CI 両アームの green で実証完了となる)

### 9.4 v0.3 追補(2026-08-21)

- **レシピからの他アセット参照を導入**(このリリースの設計判断):
  atx-core に `AssetResolver` トレイトと `apply_recipe_with_assets` を追加。
  core はストア非依存のまま、atx-mcp が `AssetStore` バックドのリゾルバを渡す。
  参照は revision id で行い、revision 不変性によりハッシュ決定論が保たれる。
  レシピの再現性はワークスペース内スコープ(他環境へは参照先アセットごと移す)。
  このパターンは v0.5 のマスク参照・v0.6 のレイヤー source 参照の先行実装
- op 14→18: `lut`(.cube 1D/3D、四面体補間、strength)、`white_balance`
  (輝度正規化ゲイン)、`hsl`(8色相域 + 三角フェザ。無シフト往復は全 u8 三つ組で
  バイト同一を検証済み)、`convolve`(≤9×9、RGB のみ)
- `.cube` は import_asset が拡張子/内容で検出し mime `application/x-cube` で格納。
  inspect_image は非画像 revision を構造化エラー(not_an_image)で拒否
- プリセット 5→7(film_soft / product_clean。アセット参照を含むプリセットは
  埋め込み LUT の自動 import 設計が必要なため次回以降に分離)
- eval t01 は傾き付き合成フィクスチャ(evals/fixtures/tilted_scene.jpg、生成器が
  detect_tilt で自己検証)に差し替え。eval 10/10

### 9.5 v0.4 追補(2026-08-21): 画素エンジン v2(f32 リニアライト)

ROADMAP の Phase B。**プロジェクト唯一の破壊的リリース**で、内部表現を
`RgbaImage`(RGBA8・sRGB 符号値)から **f32 リニアライト**へ移行した。

#### 何が変わったか

13. **内部表現 = `LinearImage { width, height, data: Vec<[f32; 4]> }`**
    (`crates/atx-core/src/linear.rs`)。RGB は 0..1、**アルファは常に線形 0..1**
    (アルファには伝達関数を掛けない = ストレートアルファ)。
    - デコード: u8 は 256 エントリ、u16(PNG16)は 65536 エントリの EOTF LUT
      (f64 計算 → 1e-9 量子化 → f32)
    - 空間変換: **4096 エントリ + 線形補間**の量子化 LUT を双方向で使う。
      素引きは暗部で誤差 1.2e-4(u16 で 8 LSB)に達するが、補間すると
      `|f''|h²/8 ≈ 1.8e-5`(u8 で 0.005 段、u16 で約 1.2 段)まで落ちる。
      逆方向(EOTF)は 2.3e-8 と桁違いに正確。**16bit 出力のために補間は必須**
    - エンコード: 出口で 1 回だけ half-away-from-zero 丸め(u8 / u16)
14. **op ごとの作業空間(このリリースの中核設計)**。中間バッファは 1 つだが、
    「その数値を線形光と見るか sRGB 符号値と見るか」は op ごとに違う。

    | 作業空間 | op | 理由 |
    |---|---|---|
    | **線形光** | `resize` / `rotate` / `perspective` / `blur` / `median` / `unsharp_mask` / `convolve` / `white_balance` | 画素の**混合**(加重平均・畳み込み)と**露出のスケール**は光量に対して行ってはじめて物理的に正しい |
    | **sRGB 符号値** | `adjust` / `color_matrix` / `curves` / `levels` / `hsl` / `lut` | スライダの効き・制御点座標・`.cube` の定義域が符号値上の慣習で決まっている |
    | **空間非依存** | `auto_orient` / `crop`(pad 含む)/ `strip_metadata` / `encode` | 添字操作・メタデータ操作なので両空間でビット同一(pad 色だけ現在の空間へ写す) |

    エンジンは現在の空間を**遅延追跡**し、必要になった瞬間にだけ変換する。
    さらに **デコード先の空間も「最初に現れる空間依存 op」から決める**ので、
    「トーン系 op だけのレシピ」は伝達関数を一度も通らず、u8 のビット精度が
    最後まで保たれる(変換 1 回あたり符号値で 1.8e-5 の補間誤差が乗るため、
    これは性能だけでなく精度のための設計である)。
15. **アルファ: 画素を混ぜる op はプリマルチプライしてから畳み込む**
    (`resize` / `rotate` / `perspective` / `blur` / `convolve`)。解くときは
    `a < 1e-6` なら RGB を 0 にする固定規則。α が全画素 1.0 の画像では
    プリマルチプライは 1.0 倍 = 厳密な恒等なので、専用の高速パスを持たなくても
    不透明画像の結果はビット同一になる。`median` は加重平均を取らない順序統計
    フィルタなので対象外(f32 の中央値は `f32::total_cmp` の全順序でソートして取る)。
16. **リサイズを自前実装に置き換えた**。`fast_image_resize` は整数画素型専用で
    f32 リニアライトを扱えないため、分離可能 Lanczos3 を自前で書いた
    (係数は f64 の `sin` で生成 → 1e-6 量子化 → 量子化後の合計で正規化 → f32。
    画素累算は f32 固定順序)。回転・射影は `imageproc` の warp を
    `Rgba<f32>` 画素で使い続けている(幾何写像・行列量子化・出力寸法の規則は v1 と同一)。
17. **決定論の規約は `tests/f32_spike.rs` の契約をそのまま全面適用**:
    超越関数は LUT 構築時のみ / `mul_add`(FMA)禁止 / 総和は走査順の左結合。
    行分割の並列化(`std::thread::scope`)は画素間の実行順序しか変えないので出力に影響しない。
    クロスプラットフォーム一致は CI(macos-14 arm64 + ubuntu-24.04 x86_64)の
    ゴールデンが最終判定。
18. **16bit I/O**: PNG16 の入出力に対応。`Operation::Encode` に
    `bit_depth: Option<u8>`(8 / 16、16 は png のみ)を追加した。
    serde は `#[serde(default, skip_serializing_if = "Option::is_none")]` なので、
    **`bit_depth` を書かない既存レシピの canonical JSON はバイト単位で従来と一致し、
    `recipe_hash` は不変**(§9.1-8 の `coordinate_space` と同じ手口)。
19. `ENGINE_VERSION = "atx-core/2"`。**レシピ DSL・正規化・ハッシュは 1 ビットも
    変わっていない**(§9-7 のゴールデン `884ea169…` は据え置きで green)。
    変わったのは出力画素だけなので、世代分離は engine version のみで足りる。

#### 挙動が変わる点(利用者向け)

- **リサイズ・ぼかしが暗く沈まなくなった**。白黒市松を縮小すると線形光 0.5
  = sRGB **188** が返る(v1 は符号値を平均して 128 = 線形 0.216 を返していた)
- **ホワイトバランスが物理的に正しくなった**。スライダの写像(§9.3 のゲインモデル)は
  変えていないが、同じゲインが**線形光の倍率**として掛かるため効き方が穏やかになる
  (符号値 128 に `temperature: +100` で R は v1 の 165 → v2 は 144)
- **トーン系 op を重ねてもポスタリゼーションが出ない**。curves/levels の 256 ノード表は
  ノード値が f32 になり(位置は従来どおり u8 格子 = 制御点の意味論は不変)、
  ノード間は f32 線形補間になった
- `convolve` の `offset` と `unsharp_mask` の `threshold` は u8 スケールの指定のまま
  受け取り、**線形光では `/255` して使う**。エンボスの `offset: 128` は
  線形 0.502 = 符号値 188 になる(符号値の中間グレーに寄せたいなら 55 前後)
- ICC は従来どおり「温存(JPEG 出力のみ埋め込み)」。作業色空間の実体化
  (lcms2 feature flag)は今回のスコープから外し、需要駆動で後続に回した

#### テスト実績

- `cargo test --workspace` green、`cargo clippy --workspace --all-targets -- -D warnings` クリーン
- ピン留めゴールデン 8 本すべてを v2 の値へ更新(旧値は各テストのコメントに
  `v1 value was …` として archive)。レシピハッシュのゴールデンは据え置き
- 新設 `crates/atx-core/tests/engine_v2_quality.rs`(このリリースの存在証明):
  - 交互に 8 回かけて恒等に戻る curves スタックが**入力とバイト同一**
    (v1 は 256→223 段の圧縮で必ずポスタライズした)
  - 白黒市松の 8 倍縮小の平均が sRGB 188(線形光 0.5)± 2
  - 全 18 op のパイプラインが 2 回実行でバイト同一
  - PNG16 は 1024 段のグラデーションを保持(8bit 出力は 256 段が上限)
- `linear` のユニットテストが硬いゲート:
  `u8 → 線形 → sRGB f32 → 線形 → u8` が全 256 値でバイト同一
- `hsl` の無シフト往復が全 u8 三つ組でバイト同一(v1 からの品質ゲートを維持)

### 9.6 v0.5 追補(2026-08-21): 局所適用マスク(core 側)

ROADMAP の Phase C 前半。調整系 op に `mask` を付けると、その op が
**マスクの重みに応じて部分的にだけ効く**ようになる。実装は
`crates/atx-core/src/ops/mask.rs` と `engine.rs` の op ループ 1 箇所。

#### レシピ形状(atx-mcp との契約)

```jsonc
{"op":"curves","master":[[0,0],[128,180],[255,255]],
 "mask":{"revision_id":"rev_m1","invert":false,"feather_px":4.0}}
```

- `MaskRef { revision_id: String, invert: bool = false, feather_px: f64 = 0.0 }`
  (`deny_unknown_fields`)。参照は **画像 revision**(任意ラスタフォーマット)で、
  §9.4 の LUT 参照と同じ「revision 不変性 → ハッシュ決定論」のパターンに乗る
- `mask` を持てるのは **調整系 11 op のみ**:
  `adjust` / `color_matrix` / `curves` / `levels` / `hsl` / `lut` /
  `white_balance` / `blur` / `median` / `unsharp_mask` / `convolve`。
  幾何 op(`resize` / `rotate` / `crop` / `perspective`)は「一部だけリサイズ」に
  意味が無いので対象外、`encode` / `strip_metadata` / `auto_orient` も同様
  (`deny_unknown_fields` により静的に弾かれる)
- serde は `#[serde(default, skip_serializing_if = "Option::is_none")]`。
  **`mask` を書かない既存レシピの canonical JSON はバイト単位で従来と一致し、
  `recipe_hash` は不変**(§9.1-8 の `coordinate_space`、§9.5-18 の `bit_depth` と
  同じ手口)。ピン留めゴールデン `884ea169…` は据え置きで green

#### ブレンドは op ループ 1 箇所の汎用処理

個々の op はマスクの存在を知らない。エンジンは各 op について:

1. `op.mask()` が `Some` なら、**先にその op の作業空間へ移して**から
   `before = img.clone()` を退避する(空間変換を挟む前後の値を混ぜないため)
2. op を従来どおり実行する(`ensure_space` は既に目的の空間なので no-op)
3. `out = before + (after - before) * w` を **RGBA 4 チャンネル**に、
   **その op の作業空間のまま**、固定順序の f32 で適用する

**端点だけは式ではなく分岐で確定させる**: `w == 1.0` なら `after`、`w == 0.0` なら
`before` をそのまま採る。f32 では `x + (y - x) * 1.0` が `y` と一致しないことがあり
(例: `0.5 + (0.1 - 0.5) = 0.099999994`)、式のままでは「全白マスク = マスク無し」
「全黒マスク = 恒等」がバイト同一にならない。中間値の計算には影響しない。

この設計のおかげで **op を増やしてもマスク対応の追加実装が要らない**
(v0.6 のレイヤーグラフでも同じブレンド規則をそのまま使える)。
アルファも重みでブレンドされるので、マスクは「op の効果の適用量」であって
合成用のアルファではない、という意味論が一貫する。

#### 重み平面の作り方(輝度 = 被覆率という判断)

`revision → 重み平面(0..1)` の順序は **輝度 → リサイズ → invert → feather → クランプ**。

- **輝度は sRGB 符号値上の BT.709 luma**(`0.2126R + 0.7152G + 0.0722B`)。
  ここが v0.5 の設計判断で、**マスクは「光」ではなく「被覆率」**だから
  線形光へ戻さない。戻すと 50% グレー(符号値 128)が線形 0.216 になり、
  「半分効かせたい」つもりのマスクが 2 割しか効かない。符号値のまま取れば
  中間グレー ≒ 50% 適用で、マスクを描いた人間の直感と一致する
  (同じ理由で `.cube` LUT を符号値で引くのと一貫している。§9.5-14)。
  マスクのアルファチャンネルは無視する
- **リサイズは双線形**(係数は f64 → 1e-6 量子化 → f32、端はクランプ)。
  マスクは低周波の重み平面なので Lanczos3 のリンギングは害でしかなく、
  オーバーシュートで 0..1 を外れないぶん双線形が正しい。これにより
  小さなマスク(32×32 等)を大きな画像へそのまま適用できる
- **feather は `ops::blur` と同じ量子化ガウスカーネル**を単一チャンネルに掛ける
  (係数生成関数を `pub(crate)` に上げて共有しただけで、blur 自体の挙動は不変)。
  σ = `feather_px`、現在の画像座標系。0.0..=200.0
- validate: `revision_id` は `"rev_"` 始まり、`feather_px` は有限かつ 0..=200

#### キャッシュ

同じ `MaskRef` を複数の op が参照するのは典型的な使い方なので、
1 回の `apply_recipe` 呼び出しの中で
`(revision_id, invert, feather_px のビット表現, 幅, 高さ)` をキーに解決結果を
キャッシュする(デコード + リサイズ + ガウスは重い)。呼び出しを跨いだ
キャッシュは持たない(revision は不変なので上位層で自由に足せる)。

#### テスト実績

`crates/atx-core/tests/mask_ops.rs`(19 本、すべて `apply_recipe_with_assets` +
モックリゾルバ経由の end-to-end):

- 既存レシピの canonical JSON に `"mask"` キーが現れないこと、
  ピン留め `recipe_hash 884ea169…` が不変であること
- 全白マスク = マスク無しとバイト同一 / 全黒マスク = 入力と画素同一 / invert が両者を入れ替える
- 左半分マスクで左半分だけが変わり、右半分は入力と画素同一(invert で反転)
- feather で境界に単調な遷移帯ができる(フェザなしは 2 値)
- 32×32 のマスクを 1477×1108 のフィクスチャへ自動リサイズして適用
- 線形空間 op(`blur`)と sRGB 空間 op(`curves`)の双方で端点・部分適用が正しい
- 同じマスクを 3 op で共有したパイプラインが 2 回実行でバイト同一(キャッシュ健全性)
- validate 拒否(`rev_` 以外の id、空 id、`feather_px` の範囲外)、
  未知 revision / デコード不能マスクが revision id を含む実行時エラーになる
- ゴールデン: フィクスチャ + テスト内生成の放射グラデーションマスク +
  feather 5px + curves + jpeg85(マスク画像自身の sha256 も同時にピン留め)

### 9.7 v0.6 追補(2026-08-21): レイヤーグラフ前半(core 側)

ROADMAP の Phase D 前半。レシピに `layers` を書くと、複数のソース画像を
**W3C の separable ブレンド 12 種**で合成し、その結果に従来どおりの
`operations` を仕上げパスとして掛けられるようになる。実装は
`crates/atx-core/src/ops/blend.rs`(式)と `engine.rs`(合成ループ + op ループの共通化)。

#### レシピ DSL v2 の形(atx-mcp との契約)

```jsonc
{
  "layers": [
    {"source": "base", "ops": [{"op": "resize", "width": 320, "fit": "contain"}]},
    {"source": {"revision_id": "rev_glow"},
     "mask": {"revision_id": "rev_mask", "feather_px": 3.0},
     "blend_mode": "screen", "opacity": 0.75},
    {"source": {"revision_id": "rev_edge"},
     "ops": [{"op": "blur", "sigma": 2.0}],
     "blend_mode": "multiply", "opacity": 0.4}
  ],
  "operations": [
    {"op": "adjust", "brightness": 0.02, "contrast": 0.03},
    {"op": "encode", "format": "jpeg", "quality": 85}
  ]
}
```

正規化 JSON(キー辞書順・デフォルト明示)は次の形になる:

```json
{"layers":[{"blend_mode":"normal","opacity":1.0,"ops":[],"source":"base"},
{"blend_mode":"multiply","mask":{"feather_px":4.0,"invert":false,"revision_id":"rev_m1"},
"opacity":0.5,"ops":[{"op":"blur","sigma":2.0}],"source":{"revision_id":"rev_tex1"}}],
"operations":[{"format":"png","op":"encode"}]}
```

- `layers: Option<Vec<Layer>>` は
  `#[serde(default, skip_serializing_if = "Option::is_none")]`。
  **`layers` を書かない v1 レシピの canonical JSON はバイト単位で従来と一致し、
  `recipe_hash` は不変**(§9.1-8 の `coordinate_space`、§9.5-18 の `bit_depth`、
  §9.6 の `mask` と同じ手口)。ピン留めゴールデン `884ea169…` は据え置きで green。
  ROADMAP は「ハッシュは世代分離」と書いていたが、**世代を分ける必要が無かった**:
  v1 レシピの正規化表現が 1 ビットも動かないので、既存の冪等キーがそのまま使える
  (`recipe_version` フィールドも追加していない。JSON の形そのものが世代を表す)
- `Layer { source, ops = [], mask = null, blend_mode = "normal", opacity = 1.0 }`
  (`deny_unknown_fields`)
- **`source` は untagged enum**: `"base"`(入力画像)か
  `{"revision_id": "rev_..."}`(ワークスペースの別 revision)。
  2 形が JSON の型レベル(文字列 / オブジェクト)で排他なので曖昧さが無く、
  `{"kind": "base"}` のようなラッパを増やさずに済み(トークン規律)、
  JSON Schema でも `anyOf` として表現でき、キー順の揺れが無いので canonical も安定。
  serde の untagged ユニットバリアントは `null` としか往復しないため、
  `base` は 1 バリアントだけの文字列 enum `BaseKeyword` を包んで表現している
- **`blend_mode`** は snake_case の 12 種:
  `normal`(既定)/ `multiply` / `screen` / `overlay` / `darken` / `lighten` /
  `color_dodge` / `color_burn` / `hard_light` / `soft_light` / `difference` / `exclusion`。
  非 separable 系(hue / saturation / color / luminosity)は v0.7

#### validate(静的制約)

- `layers` が `Some` なら空であってはならない
- **先頭レイヤーは backdrop**: 下に合成相手が居ないので
  `blend_mode: normal` / `opacity: 1.0` / `mask` 無しでなければならない
  (エラー文がその理由を説明する)
- レイヤー `ops` に `encode` / `strip_metadata` は書けない
  (**仕上げパス専用 op**。エラーはレイヤー番号と op 番号の両方を名指しする)。
  それ以外の op のバリデーションはトップレベルと同じ関数を共有し、
  メッセージに `layers[i].ops:` を前置する
- `opacity` は有限かつ 0.0..=1.0、`source` の `revision_id` は `"rev_"` 始まり
- **トップレベル `operations` は layers があるときに限り空でよい**
  (従来は空 = エラー。今回そのケースだけ緩めた)。encode は末尾 1 回までなど
  他の規則は従来どおりで、掛かる対象が合成結果になるだけ

#### 合成空間の判断: **sRGB 符号値**(線形光ではない)

v0.4 で「画素を混ぜる処理は線形光で」と決めた(§9.5)のに対し、
**レイヤー合成は sRGB 符号値空間で行う**。理由:

- ブレンド関数は「Cb / Cs が 0..1 の符号値」である前提で定義されている。
  `multiply` で中間グレー同士が中間より暗くなること、`screen` の対称性、
  `soft_light` の `D(Cb)` の分岐点 0.25 — いずれも符号値上の慣習。
  線形光で同じ式を適用すると Photoshop / CSS / Figma と見た目が一致しない
- 「合成は結果が一致することに意味がある」語彙なので、物理的正しさより
  **既存ツールとの一致**を採る(`.cube` LUT を符号値で引く §9.5-14、
  マスクの輝度を符号値で取る §9.6 と同じ判断軸)
- したがって `layers` があるときは**入力もレイヤーソースも sRGB 符号値でデコード**する。
  u8 → sRGB f32 は伝達関数を通さない厳密な `/255` なので情報は落ちない。
  レイヤー内の `ops` は従来どおり自分の作業空間へ遅延で移り、
  合成直前に sRGB 符号値へ戻る

#### 合成式(W3C conformance)

ストレートアルファのまま [compositing-1](https://www.w3.org/TR/compositing-1/) の式をそのまま書く:

```text
αs = レイヤーのアルファ × opacity × マスク重み
αo = αs + αb × (1 − αs)
Co = ( αs × (1 − αb) × Cs + αs × αb × B(Cb, Cs) + (1 − αs) × αb × Cb ) / αo
```

- `αo == 0` は RGBA すべて 0(仕様上 Co は未定義)
- 固定順序の f32、FMA 禁止(§ops/mod.rs の決定論規約)。
  `soft_light` の `sqrt` だけは **IEEE-754 で厳密に丸められる**演算なので
  画素ループ内で呼んでよい(libm 依存の exp / pow とは扱いが違う)
- **端点は式ではなく分岐で確定させる**(§9.6 のマスクブレンドと同じ理由):
  `αs == 0` なら backdrop をそのまま残す。`(αb × Cb) / αb` は f32 で `Cb` に
  戻らないことがあり、式のままでは「opacity 0 = 恒等」がバイト同一にならない。
  一方 `αs == 1 かつ αb == 1` は式のままで厳密に `B(Cb, Cs)` になるので分岐不要
- ブレンド関数の入力は 0..1 へクランプしてから渡す
  (`color_burn` の除算や `soft_light` の sqrt が定義域外の値で NaN を出さないため)

**W3C 準拠の担保は表駆動のユニットテスト**(`src/ops/blend.rs`)。
12 モードそれぞれについて `(Cb, Cs) → 期待 B` を**仕様本文から手で導いて**表に置き、
各行に代入式をコメントで残している(実装を読み直して作った表ではないことが
レビューで確認できる)。0 / 1 / 0.5 の端点、`color_dodge` / `color_burn` の
0 除算分岐(分岐順は仕様どおり Cb が先)、`hard_light` の 0.5 境界、
`soft_light` の `Cb <= 0.25` 多項式ブランチと sqrt ブランチの境界を含む
55 行。許容差 1e-6。

#### 寸法ルール

合成後のレイヤーは backdrop(先頭レイヤーの ops 適用後)と**同寸法**でなければならない。
自動リサイズはしない — 「勝手に伸ばした」より「どう合わせるかを書け」の方が
エージェントにとって直しやすいため。エラーはレイヤー番号・両方の寸法・
「そのレイヤーの `ops` に resize / crop を足せ」という提案を含む構造化メッセージを返す。
マスクだけは例外で、op マスク(§9.6)と同じ双線形リサイズで backdrop 寸法へ合わせる
(マスクは低周波の重み平面で、画素そのものではないため)。

#### エンジンの構造(op ループの共通化)

レイヤーの `ops` は「ネストしたパイプライン」なので、**op ループの本体を
`OpRunner::run_ops(&mut PipelineState, &[Operation])` に括り出して共有した**
(複製せず 1 実装)。これにより:

- レイヤー内でも v0.5 の op マスク・LUT 等のアセット参照がそのまま使える
- マスク解決キャッシュ(§9.6)は `OpRunner` が持つのでレイヤーを跨いで効く
- デコードも `decode_normalized()` に括り出し、入力画像とレイヤーソース revision が
  **同じ EXIF orientation 正規化経路**を通る(レイヤーに載せた写真も向きが直る)
- 仕上げパスへ引き継ぐアフィン変換(`coordinate_space: "source"` 用)は
  **backdrop レイヤーのもの**。キャンバスの幾何は backdrop が決めるため
- ICC / EXIF / 出力フォーマット判定は従来どおり**入力画像**由来。
  出力アルファは全レイヤーソースの論理和

`ENGINE_VERSION` は据え置き(`atx-core/2`)。`layers` を書かないレシピの
出力バイト列は 1 ビットも動いていない(既存ゴールデン 8 本すべて green)。

#### テスト実績

- `cargo test -p atx-core` green(既存スイート・ゴールデンすべて据え置きのまま)、
  `cargo clippy -p atx-core --all-targets -- -D warnings` クリーン
- 新設 `crates/atx-core/tests/layers.rs`(20 本):
  - v1 レシピの `recipe_hash 884ea169…` 不変 + canonical に `"layers"` が出ないこと
  - レイヤー付きレシピの canonical JSON をピン留め(atx-mcp との契約)
  - `source` の 2 形の serde 往復、未知キー / 未知バリアントの拒否
  - validate マトリクス(空 layers、backdrop の blend/opacity/mask、
    レイヤー内 encode / strip_metadata、opacity 範囲、`rev_` 始まり、
    レイヤー内 op の値域、`operations` 空の可否)
  - 寸法不一致エラーがレイヤー番号・両寸法・resize 提案を含むこと、
    レイヤー内 resize で解消できること
  - **multiply 50% の画素値をテスト内で f64 で独立計算**した期待値と ±1 で一致
  - normal / opacity 1 / 不透明レイヤーは「そのレイヤー単体」とバイト同一
  - opacity 0 は backdrop とバイト同一
  - レイヤー内 `blur` + 合成マスクで、マスク 0 の帯は backdrop がビット単位で残り、
    マスク 1 の帯はぼけたエッジ(単調)になる
  - 未解決 revision がレイヤー番号を名指しする
  - 3 レイヤー合成の 2 回実行がバイト同一(決定論)
- ゴールデン: 3 レイヤー(base 縮小 / 単色 × 放射グラデーションマスク feather 3px ×
  screen 0.75 / エッジ画像 × blur × multiply 0.4)+ 仕上げ adjust + jpeg85 の
  出力 sha256 と `recipe_hash` を同時にピン留め

# evals/ — エージェント eval ハーネス(v0.2)

ROADMAP.md「Agent UX の規律 #5」対応: 実務タスクを Claude Code の headless 実行(`claude -p`)で
実際に atx-mcp サーバへ接続させ、成功率・往復数を計測する。**リリース前ゲート**として手動実行する。
CI には組み込まない(§下記コスト注意)。

## ⚠️ コスト注意

`evals/run.sh` を(`--dry-run` なしで)実行すると、タスクごとに実際の Claude API トークンを消費する。
10 タスク × 数ターンの実行になるので、CI やプッシュ毎の自動実行には絶対に使わないこと。
リリース候補を切る前に手動で1回まわす、という運用を想定している。

## 前提

```sh
cargo build --release -p atx-mcp   # target/release/atx-mcp を用意する
which claude                        # Claude Code CLI が PATH にあること
```

## 使い方

```sh
# 1. まずコマンド列挙だけ確認する(トークン消費なし)
evals/run.sh --dry-run

# 2. 全10タスクを実行する(課金される)
evals/run.sh

# 3. 1タスクだけ実行する(デバッグ用、courseの絞り込み)
evals/run.sh t01_straighten_eyecatch
```

実行すると `evals/results/.run-<timestamp>/<task_id>/` に

- `workspace/` — そのタスク専用の atx-mcp ワークスペース(`assets.jsonl` 台帳を含む)
- `mcp-config.json` — `claude -p --mcp-config` に渡した設定
- `transcript.json` — `claude -p --output-format json` の生出力
- `stderr.log` — atx-mcp / claude CLI の stderr

が残り、最終的に `evals/results/<timestamp>.json` に全タスクの pass/fail サマリが書かれる。

## 採点の仕組み(evals/score.py)

各タスクの `success_criteria` は、その**タスク専用ワークスペースの `assets.jsonl` 台帳**
(追記型 JSONL、`crates/atx-store` が書く不変ログ)を直接パースして判定する。
エージェントの発話やテキスト要約は信用しない — 台帳に対応する revision が実際に
記録されているかどうかだけを見る。

`success_criteria` に書けるキー(すべて AND 条件):

- `expect_revision`: `mime_type` / `width` / `height` / `aspect_ratio`("W:H"、相対誤差2%まで許容) /
  `recipe_contains_ops`(op名の配列。`{"op": "...", "fields": {...}}` の形式でフィールド値まで指定可) /
  `min_matches`(条件に合う派生 revision の最小数、既定1) / `max_derived_total`
  (台帳全体の派生 revision 数の上限。冪等な再適用でrevisionが重複していないことの確認に使う)
- `expect_no_new_revision`: `true` なら、台帳に派生 revision(`source_revision_id` が非null)が
  1件も無いことを要求する(detect_tilt のような読み取り専用タスク用)
- `expect_export`: `dest_path` / `must_not_modify_existing`(+ `sentinel_text` か
  `sentinel_sha256`)/ `must_exist_matching_ledger`。ワークスペース外のファイルシステムを直接見る
  (`export_asset` は台帳に行を残さないため)

`dest_path` 等の文字列内の `{{TASK_DIR}}` は、そのタスク専用の実行ディレクトリの絶対パスに
置換される(`run.sh` と `score.py` の双方で同じ置換ロジックを使う)。

### 単体デバッグ

`score.py` は `run.sh` から呼ばれる以外に、既存のワークスペースに対して単独実行できる:

```sh
python3 evals/score.py evals/tasks/t01_straighten_eyecatch.json \
  --task-dir evals/results/.run-20260821T.../t01_straighten_eyecatch
```

### 自己テスト(トークン消費なし)

```sh
python3 evals/score.py --selftest
```

合成した `assets.jsonl` 相当のデータ(pass/fail 両方のケース)だけで `evaluate()` を検証する。
`score.py` のロジックを変更したら必ずこれを通すこと。

## タスク一覧(evals/tasks/*.json)

実運用フィードバック(docs/DESIGN.md §9)と ROADMAP の Agent UX 規律に由来する10本:

| id | 検証する挙動 |
|---|---|
| `t01_straighten_eyecatch` | 傾き補正 + 16:9クロップ + リサイズ + WebPエンコードの複合レシピ |
| `t02_tilt_report_only` | detect_tilt は読み取り専用(適用しない)ことの報告フロー |
| `t03_preview_before_apply` | render_preview で確認してから apply_transform する2段階フロー |
| `t04_export_overwrite_refusal` | export_asset の上書き拒否(overwrite未指定時は既存ファイルを保護) |
| `t05_compare_before_after` | compare_revisions で before/after を確認してから確定するフロー |
| `t06_idempotent_reapply` | 同一レシピの再適用で revision が重複生成されないこと(冪等性) |
| `t07_metadata_strip` | strip_metadata の scope 選択(exif は ICC を温存する) |
| `t08_source_space_crop` | crop の `coordinate_space: source`(回転前ピクセル座標での指定) |
| `t09_preset_use` | ビルトインプリセット名での指定 |
| `t10_error_self_correction` | 意図的に不整合なレシピ(aspect_ratio と rect の同時指定)を投げ、構造化エラーの `recovery` に従って1往復で自己修復する |

各タスクの `input_fixture` は基本的に共通で `tests/fixtures/synthetic_scene.jpg`
(完全合成・決定論的に再生成可能なフィクスチャ。docs/DESIGN.md §9.2 参照)。
例外は `t01_straighten_eyecatch`: `synthetic_scene.jpg` はほぼ水平なため「まっすぐにして」で
`rotate` を省く正しい振る舞いが誤って失敗扱いになっていた。`evals/fixtures/tilted_scene.jpg`
(同じ合成シーンを `-2.4°` 回転させた、客観的に傾いたフィクスチャ。`crates/atx-core/examples/gen_fixture.rs`
が `synthetic_scene.jpg` と同時に生成し、`detect_tilt` の推奨補正角 ~+2.4° を自己検証する)を使う。

## タスクの追加方法

1. `evals/tasks/tNN_<name>.json` を追加する。フィールドは既存タスクを参照:
   `id` / `prompt`(日本語、実際の編集者が言いそうな自然な依頼文) / `input_fixture` /
   `success_criteria`(上記スキーマ) / `max_turns` / (必要なら) `setup.seed_files`
   (`{"path": "...", "text": "..."}` の配列。`run.sh` がタスク実行ディレクトリ配下に
   事前配置する。export の上書き拒否テストのように、実行前から特定のファイルが
   存在する状況を再現したいときに使う)
2. `evals/run.sh --dry-run` で生成コマンドを確認する(トークン消費なし)
3. `score.py` に新しい `success_criteria` の形が必要なら `evaluate()` に評価関数を足し、
   `run_selftests()` に pass/fail 両方のケースを追加してから `python3 score.py --selftest`
   を通す
4. 実トークンを使う確認は最後に1回だけ: `evals/run.sh <new_task_id>`

## 既知の制約

- `t09_preset_use` はビルトインプリセット(`presets/` + `apply_transform` の `preset` 引数、
  ROADMAP v0.2)を前提にしている。プリセット機構が未実装の間は意図的に失敗しうる
  (別トラックで並行実装中)。実装後にこのタスクが green になることも回帰確認の一部。
- 採点は台帳(`assets.jsonl`)ベースが基本。往復数・トークン消費量は `transcript.json` に
  残るが、`run.sh` の集計 JSON では現状 `claude_exit_code` のみを記録している。
  ターン数の詳細比較が要る場合は `transcript.json` を直接見ること。

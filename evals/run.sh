#!/bin/sh
# evals/run.sh — 10 タスクを実際の claude CLI(headless, --print)で MCP 経由に実行し、
# 各タスクの成果を evals/score.py で採点、evals/results/<timestamp>.json にまとめる。
#
# ⚠️ 課金注意: --dry-run を付けない実行は毎回 Claude の実トークンを消費する。
#   リリース前の手動ゲートとして使うこと。CI に組み込まない(ROADMAP v0.2)。
#
# 使い方:
#   evals/run.sh --dry-run         実行コマンドを表示するだけ(トークン消費なし)
#   evals/run.sh                   全 10 タスクを実行し、結果を集計する
#   evals/run.sh t01_straighten_eyecatch   1タスクだけ実行する
#
# 前提: cargo build --release -p atx-mcp 済み(target/release/atx-mcp が存在すること)。
#       claude CLI が PATH にあること。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"
ATX_MCP_BIN="$REPO_ROOT/target/release/atx-mcp"
FIXTURE="$REPO_ROOT/tests/fixtures/synthetic_scene.jpg"

DRY_RUN=0
ONLY_TASK=""

for arg in "$@"; do
    case "$arg" in
        --dry-run)
            DRY_RUN=1
            ;;
        -h|--help)
            sed -n '1,20p' "$0"
            exit 0
            ;;
        *)
            ONLY_TASK="$arg"
            ;;
    esac
done

if [ "$DRY_RUN" -eq 0 ]; then
    if ! command -v claude >/dev/null 2>&1; then
        echo "error: claude CLI not found in PATH" >&2
        exit 1
    fi
    if [ ! -x "$ATX_MCP_BIN" ]; then
        echo "error: $ATX_MCP_BIN not found. run: cargo build --release -p atx-mcp" >&2
        exit 1
    fi
fi

if [ ! -f "$FIXTURE" ]; then
    echo "error: fixture not found: $FIXTURE" >&2
    exit 1
fi

TS=$(date -u +%Y%m%dT%H%M%SZ)
RUN_DIR="$RESULTS_DIR/.run-$TS"
mkdir -p "$RESULTS_DIR"

# --dry-run では一時ディレクトリを実際には作らない(コマンド列挙のみ)。
if [ "$DRY_RUN" -eq 0 ]; then
    mkdir -p "$RUN_DIR"
fi

echo "# evals/run.sh: $([ "$DRY_RUN" -eq 1 ] && echo '[dry-run] ' || echo '')run at $TS" >&2

# 集計用の一時ファイル(結果 JSON オブジェクトを1行1件、後でまとめて配列化する)。
SUMMARY_JSONL="$RUN_DIR/summary.jsonl"

run_one_task() {
    task_json="$1"
    task_id=$(basename "$task_json" .json)

    if [ -n "$ONLY_TASK" ] && [ "$ONLY_TASK" != "$task_id" ]; then
        return 0
    fi

    prompt=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['prompt'])" "$task_json")
    max_turns=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('max_turns', 10))" "$task_json")
    input_fixture_rel=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('input_fixture',''))" "$task_json")

    task_dir="$RUN_DIR/$task_id"
    workspace_dir="$task_dir/workspace"
    mcp_config="$task_dir/mcp-config.json"
    transcript="$task_dir/transcript.json"

    # {{TASK_DIR}} プレースホルダをこのタスクの実ディレクトリで解決する
    # (score.py 側でも同じ置換をするので、ここでは表示用/セットアップ用のみ)。
    resolved_prompt=$(printf '%s' "$prompt" | sed "s#{{TASK_DIR}}#$task_dir#g")

    if [ "$DRY_RUN" -eq 1 ]; then
        echo "----------------------------------------------------------------------"
        echo "task: $task_id"
        echo "would mkdir -p: $workspace_dir"
        echo "would write mcp-config to: $mcp_config"
        echo "  { \"mcpServers\": { \"atx\": { \"command\": \"$ATX_MCP_BIN\", \"args\": [\"--workspace\", \"$workspace_dir\"] } } }"
        echo "would run:"
        echo "  claude -p \"$resolved_prompt\" \\"
        echo "    --mcp-config \"$mcp_config\" --strict-mcp-config \\"
        echo "    --max-turns $max_turns \\"
        echo "    --output-format json \\"
        echo "    --allowedTools 'mcp__atx__*' \\"
        echo "    > \"$transcript\""
        echo "would import fixture ($REPO_ROOT/$input_fixture_rel) as the task's first turn context"
        echo "would score with:"
        echo "  python3 \"$SCRIPT_DIR/score.py\" \"$task_json\" --task-dir \"$task_dir\" --workspace \"$workspace_dir\""
        return 0
    fi

    mkdir -p "$task_dir" "$workspace_dir"

    # setup.seed_files: 事前にファイルを仕込むタスク(export overwrite refusal 等)。
    python3 - "$task_json" "$task_dir" <<'PYEOF'
import json, sys, pathlib
task = json.load(open(sys.argv[1]))
task_dir = pathlib.Path(sys.argv[2])
for f in task.get("setup", {}).get("seed_files", []):
    p = task_dir / f["path"]
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(f["text"], encoding="utf-8")
PYEOF

    cat > "$mcp_config" <<EOF
{
  "mcpServers": {
    "atx": {
      "command": "$ATX_MCP_BIN",
      "args": ["--workspace", "$workspace_dir"]
    }
  }
}
EOF

    echo "running task: $task_id (max_turns=$max_turns)" >&2

    # プロンプトの先頭で fixture の絶対パスを明示し、エージェントが import_asset で
    # 迷わないようにする(タスク文自体は「利用者の発話」を模すため import 手順を書かない)。
    full_prompt=$(printf 'この画像ファイルを使って作業して: %s/%s\n\n%s' "$REPO_ROOT" "$input_fixture_rel" "$resolved_prompt")

    set +e
    claude -p "$full_prompt" \
        --mcp-config "$mcp_config" --strict-mcp-config \
        --max-turns "$max_turns" \
        --output-format json \
        --allowedTools 'mcp__atx__*' \
        > "$transcript" 2> "$task_dir/stderr.log"
    claude_exit=$?
    set -e

    score_json=$(python3 "$SCRIPT_DIR/score.py" "$task_json" --task-dir "$task_dir" --workspace "$workspace_dir" || true)
    score_passed=$(printf '%s' "$score_json" | python3 -c "import json,sys; print(json.load(sys.stdin).get('passed', False))" 2>/dev/null || echo "False")
    # claude_exit を pass 判定へ折り込む: claude プロセスが非0終了(クラッシュ/タイムアウト等)
    # した回は、たとえ台帳の状態が偶然 score.py の条件を満たしていても FAIL 扱いにする。
    # これがないと expect_no_new_revision タスクは「何もしていない」ことしか見ないため、
    # claude が異常終了して本当に何もできなかった実行が PASS になってしまう。
    if [ "$claude_exit" -eq 0 ] && [ "$score_passed" = "True" ]; then
        passed="True"
    else
        passed="False"
    fi

    python3 -c "
import json, sys
entry = {
    'id': sys.argv[1],
    'passed': sys.argv[2] == 'True',
    'claude_exit_code': int(sys.argv[3]),
    'score': json.loads(sys.argv[4]) if sys.argv[4] else None,
}
print(json.dumps(entry, ensure_ascii=False))
" "$task_id" "$passed" "$claude_exit" "$score_json" >> "$SUMMARY_JSONL"

    echo "  -> $task_id: passed=$passed (claude exit=$claude_exit)" >&2
}

for task_json in "$TASKS_DIR"/*.json; do
    run_one_task "$task_json"
done

if [ "$DRY_RUN" -eq 1 ]; then
    exit 0
fi

RESULT_FILE="$RESULTS_DIR/$TS.json"
python3 - "$SUMMARY_JSONL" "$RESULT_FILE" "$TS" <<'PYEOF'
import json, sys

summary_path, out_path, ts = sys.argv[1], sys.argv[2], sys.argv[3]
entries = []
with open(summary_path, encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if line:
            entries.append(json.loads(line))

total = len(entries)
passed = sum(1 for e in entries if e["passed"])
out = {
    "timestamp": ts,
    "total": total,
    "passed": passed,
    "failed": total - passed,
    "pass_rate": (passed / total) if total else 0.0,
    "tasks": entries,
}
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(out, f, ensure_ascii=False, indent=2)

print(f"\n=== summary: {passed}/{total} passed ({out['pass_rate']:.0%}) ===")
for e in entries:
    mark = "PASS" if e["passed"] else "FAIL"
    print(f"  [{mark}] {e['id']}")
print(f"\nwritten: {out_path}")
PYEOF

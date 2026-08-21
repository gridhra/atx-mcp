#!/usr/bin/env python3
"""evals/score.py — assets.jsonl 台帳を success_criteria と突き合わせて pass/fail を出す。

設計方針:
- 唯一の信頼できる証跡は台帳(<workspace>/assets.jsonl、追記型 JSONL)。
  エージェントが「やった」と report.txt などで自己申告しても、台帳に対応する
  derivation が記録されていなければ fail にする。
- 台帳から読めない検証(export_asset はワークスペース外にファイルを書くのでノーライン)
  だけファイルシステムを直接見る(expect_export)。
- run.sh からは1タスクにつき1回呼ばれるが、デバッグ用に単体でも実行できる:
    python3 score.py <task.json> --task-dir <dir> [--workspace <dir>]
- `--selftest` はネットワークも実プロセスも使わず、evaluate() だけを
  合成 assets.jsonl で検証する(このファイル自身の回帰テスト)。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import Any


# ---------------------------------------------------------------------------
# 台帳ユーティリティ
# ---------------------------------------------------------------------------


def load_ledger(ledger_path: Path) -> list[dict[str, Any]]:
    """assets.jsonl を読む。存在しなければ空リスト(import すら起きていない)。"""
    if not ledger_path.exists():
        return []
    out: list[dict[str, Any]] = []
    for i, line in enumerate(ledger_path.read_text(encoding="utf-8").splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError as e:
            raise ValueError(f"ledger corrupted at line {i + 1}: {e}") from e
    return out


def derived_revisions(ledger: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """import 起点(source_revision_id is None)を除いた派生 revision のみ。"""
    return [r for r in ledger if r.get("source_revision_id")]


def op_names(recipe: dict[str, Any] | None) -> list[str]:
    if not recipe:
        return []
    return [op.get("op") for op in recipe.get("operations", [])]


def recipe_has_op(recipe: dict[str, Any] | None, spec: str | dict[str, Any]) -> bool:
    """recipe が spec を満たす operation を含むか。

    spec が文字列なら op 名一致のみ。dict なら {"op": "...", "fields": {...}} の形式で、
    fields の各 key/value が該当 operation のフィールドと一致することも要求する
    (フィールドが無いオペレーションはデフォルト値が serde 側で決まるため、値の比較は
    台帳に保存された recipe のフィールドそのものと突き合わせる = 実際に serialize
    された値。デフォルト省略と明示指定を区別したい場合は呼び出し側で注意)。
    """
    if not recipe:
        return False
    if isinstance(spec, str):
        return any(op.get("op") == spec for op in recipe.get("operations", []))
    want_op = spec["op"]
    fields = spec.get("fields", {})
    for op in recipe.get("operations", []):
        if op.get("op") != want_op:
            continue
        if all(op.get(k) == v for k, v in fields.items()):
            return True
    return False


def aspect_ratio_matches(width: int, height: int, target: str, tol: float = 0.02) -> bool:
    """"W:H" とのアスペクト比一致を相対誤差 tol 以内で判定する。"""
    if height <= 0:
        return False
    try:
        w_str, h_str = target.split(":")
        target_ratio = float(w_str) / float(h_str)
    except (ValueError, ZeroDivisionError):
        return False
    actual_ratio = width / height
    if target_ratio == 0:
        return False
    return abs(actual_ratio - target_ratio) / target_ratio <= tol


# ---------------------------------------------------------------------------
# 評価本体
# ---------------------------------------------------------------------------


def _eval_expect_revision(spec: dict[str, Any], ledger: list[dict[str, Any]]) -> tuple[bool, dict[str, Any]]:
    candidates = derived_revisions(ledger)
    matched = []
    for rev in candidates:
        if "mime_type" in spec and rev.get("mime_type") != spec["mime_type"]:
            continue
        if "width" in spec and rev.get("width") != spec["width"]:
            continue
        if "height" in spec and rev.get("height") != spec["height"]:
            continue
        if "aspect_ratio" in spec and not aspect_ratio_matches(
            rev.get("width", 0), rev.get("height", 0), spec["aspect_ratio"]
        ):
            continue
        ops_ok = True
        for op_spec in spec.get("recipe_contains_ops", []):
            if not recipe_has_op(rev.get("recipe"), op_spec):
                ops_ok = False
                break
        if not ops_ok:
            continue
        matched.append(rev)

    min_matches = spec.get("min_matches", 1)
    detail: dict[str, Any] = {
        "candidates_total": len(candidates),
        "matched": len(matched),
        "min_matches_required": min_matches,
        "matched_revision_ids": [r["revision_id"] for r in matched],
    }
    passed = len(matched) >= min_matches

    if "max_derived_total" in spec:
        detail["derived_total"] = len(candidates)
        detail["max_derived_total_allowed"] = spec["max_derived_total"]
        if len(candidates) > spec["max_derived_total"]:
            passed = False

    return passed, detail


def _eval_expect_no_new_revision(ledger: list[dict[str, Any]]) -> tuple[bool, dict[str, Any]]:
    candidates = derived_revisions(ledger)
    detail = {
        "derived_total": len(candidates),
        "derived_revision_ids": [r["revision_id"] for r in candidates],
    }
    return len(candidates) == 0, detail


def _sha256_file(path: Path) -> str | None:
    if not path.exists() or not path.is_file():
        return None
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _eval_expect_export(spec: dict[str, Any], ledger: list[dict[str, Any]]) -> tuple[bool, dict[str, Any]]:
    dest = Path(spec["dest_path"])
    actual_sha = _sha256_file(dest)
    detail: dict[str, Any] = {"dest_path": str(dest), "dest_exists": dest.exists(), "actual_sha256": actual_sha}

    passed = True

    if spec.get("must_not_modify_existing"):
        sentinel_text = spec.get("sentinel_text")
        sentinel_sha = spec.get("sentinel_sha256") or (
            hashlib.sha256(sentinel_text.encode("utf-8")).hexdigest() if sentinel_text is not None else None
        )
        detail["sentinel_sha256"] = sentinel_sha
        if sentinel_sha is None or actual_sha != sentinel_sha:
            passed = False

    if spec.get("must_exist_matching_ledger"):
        all_shas = {r.get("sha256") for r in ledger}
        detail["ledger_sha256_set_size"] = len(all_shas)
        if actual_sha not in all_shas:
            passed = False

    return passed, detail


def evaluate(criteria: dict[str, Any], ledger: list[dict[str, Any]]) -> tuple[bool, dict[str, Any]]:
    """success_criteria オブジェクト1つを台帳(+すでに解決済みのファイルパス)で評価する。

    criteria は複数キーを持てる(すべて AND)。現状のタスク定義は1キーのみ使うが、
    将来複合条件を書けるようにしておく。
    """
    if not criteria:
        return False, {"reason": "empty success_criteria"}

    results: dict[str, Any] = {}
    passed = True

    if "expect_revision" in criteria:
        ok, detail = _eval_expect_revision(criteria["expect_revision"], ledger)
        results["expect_revision"] = detail
        passed = passed and ok

    if "expect_no_new_revision" in criteria:
        want = criteria["expect_no_new_revision"]
        ok, detail = _eval_expect_no_new_revision(ledger)
        if not want:
            # expect_no_new_revision: false は現状使わないが、意味的に反転しておく。
            ok = not ok
        results["expect_no_new_revision"] = detail
        passed = passed and ok

    if "expect_export" in criteria:
        ok, detail = _eval_expect_export(criteria["expect_export"], ledger)
        results["expect_export"] = detail
        passed = passed and ok

    return passed, results


# ---------------------------------------------------------------------------
# テンプレート置換({{TASK_DIR}})
# ---------------------------------------------------------------------------


def substitute_task_dir(obj: Any, task_dir: str) -> Any:
    if isinstance(obj, str):
        return obj.replace("{{TASK_DIR}}", task_dir)
    if isinstance(obj, dict):
        return {k: substitute_task_dir(v, task_dir) for k, v in obj.items()}
    if isinstance(obj, list):
        return [substitute_task_dir(v, task_dir) for v in obj]
    return obj


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def score_task_file(task_json_path: Path, task_dir: Path, workspace_dir: Path | None) -> dict[str, Any]:
    task = json.loads(task_json_path.read_text(encoding="utf-8"))
    task = substitute_task_dir(task, str(task_dir))

    ws = workspace_dir if workspace_dir is not None else (task_dir / "workspace")
    ledger_path = ws / "assets.jsonl"
    ledger = load_ledger(ledger_path)

    passed, detail = evaluate(task["success_criteria"], ledger)
    return {
        "id": task["id"],
        "passed": passed,
        "max_turns": task.get("max_turns"),
        "workspace": str(ws),
        "ledger_lines": len(ledger),
        "detail": detail,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("task_json", nargs="?", help="evals/tasks/*.json へのパス")
    parser.add_argument("--task-dir", help="{{TASK_DIR}} に代入する絶対パス(export_asset の宛先解決用)")
    parser.add_argument("--workspace", help="atx-mcp ワークスペースディレクトリ(省略時は <task-dir>/workspace)")
    parser.add_argument("--selftest", action="store_true", help="合成台帳による自己テストのみ実行して終了")
    args = parser.parse_args()

    if args.selftest:
        run_selftests()
        print("score.py --selftest: all self-tests passed")
        return 0

    if not args.task_json or not args.task_dir:
        parser.error("task_json と --task-dir が必要です(--selftest 以外の場合)")

    result = score_task_file(
        Path(args.task_json),
        Path(args.task_dir),
        Path(args.workspace) if args.workspace else None,
    )
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["passed"] else 1


# ---------------------------------------------------------------------------
# 自己テスト(--selftest。実プロセス・ネットワーク不使用)
# ---------------------------------------------------------------------------


def _mk_import(revision_id: str = "rev_import01", sha256: str = "a" * 64) -> dict[str, Any]:
    return {
        "asset_id": "ast_01",
        "revision_id": revision_id,
        "source_revision_id": None,
        "width": 1477,
        "height": 1108,
        "mime_type": "image/jpeg",
        "byte_size": 12345,
        "sha256": sha256,
        "rel_path": f"objects/aa/{sha256}.jpg",
        "recipe": None,
        "recipe_hash": None,
        "origin": {},
        "created_at": "2026-08-21T00:00:00Z",
    }


def _mk_derived(
    revision_id: str,
    source_revision_id: str,
    width: int,
    height: int,
    mime_type: str,
    operations: list[dict[str, Any]],
    sha256: str = "b" * 64,
) -> dict[str, Any]:
    return {
        "asset_id": "ast_01",
        "revision_id": revision_id,
        "source_revision_id": source_revision_id,
        "width": width,
        "height": height,
        "mime_type": mime_type,
        "byte_size": 6789,
        "sha256": sha256,
        "rel_path": f"objects/bb/{sha256}.{mime_type.split('/')[-1]}",
        "recipe": {"operations": operations},
        "recipe_hash": "h_" + revision_id,
        "origin": {},
        "created_at": "2026-08-21T00:01:00Z",
    }


def run_selftests() -> None:
    # --- expect_revision: passing case (t01 相当) ---
    ledger = [
        _mk_import(),
        _mk_derived(
            "rev_derived01",
            "rev_import01",
            1600,
            900,
            "image/webp",
            [
                {"op": "rotate", "angle_degrees": 0.5},
                {"op": "crop", "aspect_ratio": "16:9"},
                {"op": "resize", "width": 1600},
                {"op": "encode", "format": "webp"},
            ],
        ),
    ]
    criteria = {
        "expect_revision": {
            "mime_type": "image/webp",
            "width": 1600,
            "aspect_ratio": "16:9",
            "recipe_contains_ops": ["rotate", "crop", "resize", "encode"],
            "min_matches": 1,
        }
    }
    ok, detail = evaluate(criteria, ledger)
    assert ok, f"expected pass, got fail: {detail}"
    assert detail["expect_revision"]["matched"] == 1

    # --- expect_revision: failing case (wrong width) ---
    ledger_bad = [
        _mk_import(),
        _mk_derived(
            "rev_derived02",
            "rev_import01",
            1200,  # wrong width
            675,
            "image/webp",
            [{"op": "crop", "aspect_ratio": "16:9"}, {"op": "encode", "format": "webp"}],
        ),
    ]
    ok, detail = evaluate(criteria, ledger_bad)
    assert not ok, "expected fail on wrong width, got pass"
    assert detail["expect_revision"]["matched"] == 0

    # --- expect_revision: op missing (no apply_transform at all) ---
    ledger_noop = [_mk_import()]
    ok, detail = evaluate(criteria, ledger_noop)
    assert not ok, "expected fail when no derivation exists"

    # --- expect_no_new_revision: passing (t02 相当, read-only report) ---
    criteria_readonly = {"expect_no_new_revision": True}
    ok, detail = evaluate(criteria_readonly, [_mk_import()])
    assert ok, f"expected pass (no derivations), got fail: {detail}"

    # --- expect_no_new_revision: failing (agent applied something anyway) ---
    ledger_applied = [
        _mk_import(),
        _mk_derived("rev_derived03", "rev_import01", 1477, 1108, "image/jpeg", [{"op": "auto_orient"}]),
    ]
    ok, detail = evaluate(criteria_readonly, ledger_applied)
    assert not ok, "expected fail when a derivation exists but none was wanted"

    # --- recipe_contains_ops with fields (t07 相当: strip_metadata scope=exif) ---
    criteria_strip = {
        "expect_revision": {
            "mime_type": "image/jpeg",
            "recipe_contains_ops": [{"op": "strip_metadata", "fields": {"scope": "exif"}}],
            "min_matches": 1,
        }
    }
    ledger_strip_ok = [
        _mk_import(),
        _mk_derived(
            "rev_derived04",
            "rev_import01",
            1477,
            1108,
            "image/jpeg",
            [{"op": "strip_metadata", "scope": "exif"}],
        ),
    ]
    ok, _ = evaluate(criteria_strip, ledger_strip_ok)
    assert ok, "expected pass for strip_metadata scope=exif"

    ledger_strip_wrong_scope = [
        _mk_import(),
        _mk_derived(
            "rev_derived05",
            "rev_import01",
            1477,
            1108,
            "image/jpeg",
            [{"op": "strip_metadata", "scope": "all"}],
        ),
    ]
    ok, _ = evaluate(criteria_strip, ledger_strip_wrong_scope)
    assert not ok, "expected fail for strip_metadata scope=all when exif was required"

    # --- max_derived_total (t06 相当: idempotent re-apply must not duplicate) ---
    criteria_idempotent = {
        "expect_revision": {
            "width": 1200,
            "aspect_ratio": "16:9",
            "recipe_contains_ops": ["crop", "resize"],
            "min_matches": 1,
            "max_derived_total": 1,
        }
    }
    ledger_single = [
        _mk_import(),
        _mk_derived(
            "rev_derived06",
            "rev_import01",
            1200,
            675,
            "image/jpeg",
            [{"op": "crop", "aspect_ratio": "16:9"}, {"op": "resize", "width": 1200}],
        ),
    ]
    ok, detail = evaluate(criteria_idempotent, ledger_single)
    assert ok, f"expected pass with exactly one derivation: {detail}"

    ledger_duplicated = ledger_single + [
        _mk_derived(
            "rev_derived07",
            "rev_import01",
            1200,
            675,
            "image/jpeg",
            [{"op": "crop", "aspect_ratio": "16:9"}, {"op": "resize", "width": 1200}],
            sha256="c" * 64,
        )
    ]
    ok, detail = evaluate(criteria_idempotent, ledger_duplicated)
    assert not ok, "expected fail when a second (non-reused) derivation appears"
    assert detail["expect_revision"]["derived_total"] == 2

    # --- aspect_ratio tolerance ---
    assert aspect_ratio_matches(1600, 900, "16:9")
    assert aspect_ratio_matches(1601, 901, "16:9")  # within 2% tolerance
    assert not aspect_ratio_matches(1600, 1200, "16:9")  # 4:3, well outside tolerance

    # --- expect_export: must_not_modify_existing ---
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        dest = Path(td) / "output.jpg"
        sentinel = "sentinel: pre-existing file that must survive this task unchanged\n"
        dest.write_text(sentinel, encoding="utf-8")
        criteria_export = {
            "expect_export": {
                "dest_path": str(dest),
                "must_not_modify_existing": True,
                "sentinel_text": sentinel,
            }
        }
        ok, detail = evaluate(criteria_export, [])
        assert ok, f"expected pass: sentinel untouched, got {detail}"

        # Agent overwrote it without permission -> must fail.
        dest.write_text("overwritten by agent without confirmation\n", encoding="utf-8")
        ok, detail = evaluate(criteria_export, [])
        assert not ok, "expected fail: file was modified without confirmation"

    # --- substitute_task_dir ---
    substituted = substitute_task_dir({"dest_path": "{{TASK_DIR}}/export/output.jpg"}, "/tmp/task42")
    assert substituted["dest_path"] == "/tmp/task42/export/output.jpg"


if __name__ == "__main__":
    sys.exit(main())

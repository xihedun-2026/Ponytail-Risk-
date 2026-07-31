#!/usr/bin/env python3
"""导出 Python 风控实现在差分语料上的输出，供与 Rust 实现逐字段比对。

交接报告 §7 阶段 4 要求：Python 与 Rust 对同一 fixture 输出相同标签、
证据字段和基础分后才能切换。本脚本只跑纯逻辑函数，不连数据库，
因此用一个桩模块顶替 pymysql，避免为了跑校验而安装数据库驱动。

用法：
    python3 tools/parity_python.py [fixtures.json]
输出：stdout 一份 JSON，结构与 `cargo test -p wdsf-engine --test parity` 内部生成的一致。
"""
from __future__ import annotations

import json
import sqlite3
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parent

# wdsf_live_data 在导入时就 import pymysql，但本脚本只用纯逻辑函数。
# 放一个桩模块进 sys.modules，避免把数据库驱动变成跑校验的前置条件。
if "pymysql" not in sys.modules:
    stub = types.ModuleType("pymysql")
    stub.cursors = types.SimpleNamespace(DictCursor=object)

    def _unavailable(*_args, **_kwargs):
        raise RuntimeError("差分校验不应触碰数据库")

    stub.connect = _unavailable
    sys.modules["pymysql"] = stub

sys.path.insert(0, str(ROOT))
import wdsf_live_data as engine  # noqa: E402


def run_risk_score(cases: list[dict]) -> list[dict]:
    results = []
    for case in cases:
        score, tags, reasons = engine.risk_score(case["facts"])
        results.append({"name": case["name"], "score": score, "tags": tags, "reasons": reasons})
    return results


def run_gold_snapshot_jumps(cases: list[dict]) -> list[dict]:
    return [
        {"name": case["name"], "jumps": engine.gold_snapshot_jumps(case["rows"])}
        for case in cases
    ]


def run_transfer_timeline_event(cases: list[dict]) -> list[dict]:
    results = []
    for case in cases:
        event = engine.transfer_timeline_event(case["row"], case["gid"])
        results.append({"name": case["name"], "event": list(event) if event else None})
    return results


def run_apply_snapshot(cases: list[dict]) -> list[dict]:
    results = []
    for case in cases:
        with sqlite3.connect(":memory:") as ledger:
            scans = [
                engine.apply_snapshot(ledger, scan["rows"], scan["scanned_at"])
                for scan in case["scans"]
            ]
        results.append({"name": case["name"], "results": scans})
    return results


def main() -> int:
    fixtures_path = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "parity_fixtures.json"
    fixtures = json.loads(fixtures_path.read_text(encoding="utf-8"))

    report = {
        "risk_score": run_risk_score(fixtures["risk_score"]),
        "number": [engine.number(value) for value in fixtures["number"]],
        "stamp_label": [engine.stamp_label(value) for value in fixtures["stamp_label"]],
        "normalized_iid": [engine.normalized_iid(value) for value in fixtures["normalized_iid"]],
        "reward_change": [engine.reward_change(row) for row in fixtures["reward_change"]],
        "activity_direction": [
            list(engine.activity_direction(action)) for action in fixtures["activity_direction"]
        ],
        "gold_snapshot_jumps": run_gold_snapshot_jumps(fixtures["gold_snapshot_jumps"]),
        "transfer_timeline_event": run_transfer_timeline_event(fixtures["transfer_timeline_event"]),
        "transfer_trace_action": [
            engine.transfer_trace_action(row) for row in fixtures["transfer_trace_action"]
        ],
        "apply_snapshot": run_apply_snapshot(fixtures["apply_snapshot"]),
    }
    json.dump(report, sys.stdout, ensure_ascii=False, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

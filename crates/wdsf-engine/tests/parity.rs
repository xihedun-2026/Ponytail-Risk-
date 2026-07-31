//! Python ↔ Rust 差分验证。
//!
//! 交接报告 §7 阶段 4：「Python 与 Rust 对同一 fixture 输出相同标签、
//! 证据字段和基础分后才能切换。」本测试就是那道闸门。
//!
//! 运行时会调用 `python3 tools/parity_python.py`（或 `python`）拿到基准输出，
//! 与 Rust 实现逐字段比对。阶段 5 停用 Python 数据层之后，
//! 可以设 `PARITY_SKIP_PYTHON=1` 只保留 Rust 侧快照校验。

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use risk_core::{
    activity_direction, gold_snapshot_jumps, normalized_iid, number, reward_change, risk_score,
    stamp_label, transfer_timeline_event, transfer_trace_action, CoinSnapshot, Facts, RewardRow,
    TransferRow, DEFAULT_JUMP_MINIMUM,
};
use risk_ledger::{apply_snapshot, prepare_ledger, AssetRow};

fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/wdsf-engine
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("定位项目根目录失败")
        .to_path_buf()
}

fn load_fixtures() -> Value {
    let path = project_root().join("tools").join("parity_fixtures.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取语料 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("语料不是合法 JSON")
}

fn facts_from(value: &Value) -> Facts {
    serde_json::from_value(value.clone()).expect("facts 反序列化失败")
}

fn transfer_row_from(value: &Value) -> TransferRow {
    TransferRow {
        action: value["action"].as_str().unwrap_or_default().to_string(),
        item_amount: value["item_amount"].as_i64().unwrap_or(0),
        item_name: value["item_name"].as_str().unwrap_or_default().to_string(),
        item_iid: value["item_iid"].as_str().unwrap_or_default().to_string(),
        gid_from: value["gid_from"].as_str().unwrap_or_default().to_string(),
        gid_to: value["gid_to"].as_str().unwrap_or_default().to_string(),
    }
}

fn asset_row_from(value: &Value) -> AssetRow {
    AssetRow {
        iid: value["iid"].as_str().unwrap_or_default().to_string(),
        name: value["name"].as_str().unwrap_or_default().to_string(),
        owner: value["owner"].as_str().unwrap_or_default().to_string(),
        owner_name: value["owner_name"].as_str().unwrap_or_default().to_string(),
        env: value["env"].as_str().unwrap_or_default().to_string(),
        pos: value["pos"].as_i64().unwrap_or(0),
        amount: value["amount"].as_i64().unwrap_or(0),
    }
}

/// 用 Rust 实现跑一遍全部语料，产出与 `tools/parity_python.py` 同构的报告。
fn rust_report(fixtures: &Value) -> Value {
    let mut report = Map::new();

    report.insert(
        "risk_score".into(),
        Value::Array(
            fixtures["risk_score"]
                .as_array()
                .unwrap()
                .iter()
                .map(|case| {
                    let assessment = risk_score(&facts_from(&case["facts"]));
                    json!({
                        "name": case["name"],
                        "score": assessment.score,
                        "tags": assessment.tags,
                        "reasons": assessment.reasons,
                    })
                })
                .collect(),
        ),
    );

    report.insert(
        "number".into(),
        Value::Array(
            fixtures["number"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| Value::String(number(value.as_i64().unwrap())))
                .collect(),
        ),
    );

    report.insert(
        "stamp_label".into(),
        Value::Array(
            fixtures["stamp_label"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| Value::String(stamp_label(value.as_str().unwrap())))
                .collect(),
        ),
    );

    report.insert(
        "normalized_iid".into(),
        Value::Array(
            fixtures["normalized_iid"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| Value::String(normalized_iid(value.as_str().unwrap())))
                .collect(),
        ),
    );

    report.insert(
        "reward_change".into(),
        Value::Array(
            fixtures["reward_change"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    Value::String(reward_change(&RewardRow {
                        bonus_type: row["bonus_type"].as_i64().unwrap_or(0),
                        bonus_name: row["bonus_name"].as_str().unwrap_or_default().to_string(),
                        bonus_prop: row["bonus_prop"].as_str().unwrap_or_default().to_string(),
                    }))
                })
                .collect(),
        ),
    );

    report.insert(
        "activity_direction".into(),
        Value::Array(
            fixtures["activity_direction"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| {
                    let (title, prefix) = activity_direction(value.as_str().unwrap());
                    json!([title, prefix])
                })
                .collect(),
        ),
    );

    report.insert(
        "gold_snapshot_jumps".into(),
        Value::Array(
            fixtures["gold_snapshot_jumps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|case| {
                    let rows: Vec<CoinSnapshot> = case["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| CoinSnapshot {
                            update_time: row["update_time"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            gold_coin: row["gold_coin"].as_i64().unwrap_or(0),
                        })
                        .collect();
                    json!({
                        "name": case["name"],
                        "jumps": gold_snapshot_jumps(&rows, DEFAULT_JUMP_MINIMUM),
                    })
                })
                .collect(),
        ),
    );

    report.insert(
        "transfer_timeline_event".into(),
        Value::Array(
            fixtures["transfer_timeline_event"]
                .as_array()
                .unwrap()
                .iter()
                .map(|case| {
                    let event = transfer_timeline_event(
                        &transfer_row_from(&case["row"]),
                        case["gid"].as_str().unwrap(),
                    );
                    json!({
                        "name": case["name"],
                        "event": event.map(|event| json!([event.action, event.change, event.note])),
                    })
                })
                .collect(),
        ),
    );

    report.insert(
        "transfer_trace_action".into(),
        Value::Array(
            fixtures["transfer_trace_action"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| Value::String(transfer_trace_action(&transfer_row_from(row))))
                .collect(),
        ),
    );

    report.insert(
        "apply_snapshot".into(),
        Value::Array(
            fixtures["apply_snapshot"]
                .as_array()
                .unwrap()
                .iter()
                .map(|case| {
                    let ledger = Connection::open_in_memory().unwrap();
                    prepare_ledger(&ledger).unwrap();
                    let results: Vec<Value> = case["scans"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|scan| {
                            let rows: Vec<AssetRow> = scan["rows"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(asset_row_from)
                                .collect();
                            let result = apply_snapshot(
                                &ledger,
                                &rows,
                                scan["scanned_at"].as_str().unwrap(),
                            )
                            .unwrap();
                            json!({ "scanned": result.scanned, "changes": result.changes })
                        })
                        .collect();
                    json!({ "name": case["name"], "results": results })
                })
                .collect(),
        ),
    );

    Value::Object(report)
}

fn python_interpreter() -> Option<&'static str> {
    for candidate in ["python3", "python"] {
        let available = Command::new(candidate)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if available {
            return Some(candidate);
        }
    }
    None
}

/// 逐字段比对两侧报告，把第一处差异的路径和两边的值打印出来。
fn diff(path: &str, left: &Value, right: &Value, differences: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            let mut keys: Vec<&String> = left_map.keys().chain(right_map.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                diff(
                    &format!("{path}.{key}"),
                    left_map.get(key).unwrap_or(&Value::Null),
                    right_map.get(key).unwrap_or(&Value::Null),
                    differences,
                );
            }
        }
        (Value::Array(left_items), Value::Array(right_items)) => {
            if left_items.len() != right_items.len() {
                differences.push(format!(
                    "{path}: 长度不同 python={} rust={}",
                    left_items.len(),
                    right_items.len()
                ));
                return;
            }
            for (index, (left_item, right_item)) in
                left_items.iter().zip(right_items.iter()).enumerate()
            {
                diff(
                    &format!("{path}[{index}]"),
                    left_item,
                    right_item,
                    differences,
                );
            }
        }
        _ => {
            if left != right {
                differences.push(format!("{path}: python={left} rust={right}"));
            }
        }
    }
}

#[test]
fn rust_matches_python_on_every_fixture() {
    let fixtures = load_fixtures();
    let rust = rust_report(&fixtures);

    if std::env::var("PARITY_SKIP_PYTHON").is_ok() {
        eprintln!("PARITY_SKIP_PYTHON 已设置：跳过 Python 基准比对，只校验 Rust 侧可运行。");
        assert!(rust["risk_score"].as_array().unwrap().len() >= 15);
        return;
    }

    let interpreter = python_interpreter().expect(
        "找不到 python3/python，无法完成交接报告 §7 阶段 4 要求的双算比对。\
         如果 Python 数据层已按阶段 5 停用，请设 PARITY_SKIP_PYTHON=1 显式跳过。",
    );

    let root = project_root();
    let output = Command::new(interpreter)
        .arg(root.join("tools").join("parity_python.py"))
        .arg(root.join("tools").join("parity_fixtures.json"))
        .current_dir(&root)
        .output()
        .expect("运行 Python 基准脚本失败");

    assert!(
        output.status.success(),
        "Python 基准脚本退出码非零：\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let python: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Python 基准输出不是合法 JSON：{error}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let mut differences = Vec::new();
    diff("", &python, &rust, &mut differences);

    assert!(
        differences.is_empty(),
        "Python 与 Rust 输出存在 {} 处差异，未达到切换条件：\n{}",
        differences.len(),
        differences.join("\n")
    );
}

#[test]
fn fixture_corpus_covers_every_scoring_rule() {
    // 语料必须覆盖每一条会加分的规则，否则差分比对是"过了但没测到"。
    let fixtures = load_fixtures();
    let rust = rust_report(&fixtures);
    let produced: Vec<String> = rust["risk_score"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|case| {
            case["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tag| tag.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect();

    for tag in [
        "币值校验异常",
        "元宝存量偏离",
        "交易账本缺腿",
        "同设备交易",
        "多账号资产归集",
        "短时资产归集",
        "资产循环回流",
        "超长持续活跃",
        "机械周期行为",
        "奖励爆发异常",
        "奖励快速归集",
        "绕过交易转移",
        "元宝快照跳增",
        "高频流转",
        "未见强异常",
    ] {
        assert!(
            produced.iter().any(|item| item == tag),
            "差分语料没有覆盖规则「{tag}」"
        );
    }
}

#[test]
fn ledger_fixture_covers_every_event_kind() {
    let fixtures = load_fixtures();
    let rust = rust_report(&fixtures);
    let mut seen: Vec<&str> = Vec::new();
    for case in rust["apply_snapshot"].as_array().unwrap() {
        for result in case["results"].as_array().unwrap() {
            for kind in [
                "baseline",
                "first_seen",
                "owner_changed",
                "amount_changed",
                "missing",
            ] {
                if result["changes"][kind].as_i64().unwrap_or(0) > 0 && !seen.contains(&kind) {
                    seen.push(kind);
                }
            }
        }
    }
    assert_eq!(
        seen.len(),
        5,
        "账本语料未覆盖全部事件类型，实际覆盖：{seen:?}"
    );
}

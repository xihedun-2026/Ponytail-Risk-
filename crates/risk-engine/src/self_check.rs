//! 不依赖数据库的自检，对应 `tools/risk_live_data.py::self_check`。
//!
//! 这些断言是 Python 版与 Rust 版共同的行为契约。任何一条失败都说明
//! 移植过程中丢失了已确认的数据语义，必须停下来查，而不是改断言。

use anyhow::{ensure, Result};
use rusqlite::Connection;

use risk_core::{
    activity_direction, coin_label, gold_snapshot_jumps, normalized_iid, reward_change, risk_score,
    transfer_timeline_event, transfer_trace_action, CoinSnapshot, Facts, RewardRow, TransferRow,
    DEFAULT_JUMP_MINIMUM,
};
use risk_ledger::{apply_snapshot, prepare_ledger, AssetRow};

fn snapshot(update_time: &str, gold_coin: i64) -> CoinSnapshot {
    CoinSnapshot {
        update_time: update_time.to_string(),
        gold_coin,
    }
}

/// 运行全部自检项，返回通过的检查条数。
pub fn run() -> Result<usize> {
    let mut checks = 0usize;

    // 1-2 评分基线
    let normal = risk_score(&Facts {
        gold_coin: 1000,
        median_gold_coin: 1000,
        ..Default::default()
    });
    ensure!(
        normal.score == 0,
        "正常角色评分应为 0，实际 {}",
        normal.score
    );
    checks += 1;

    let suspicious = risk_score(&Facts {
        gold_coin: 900_000_000,
        median_gold_coin: 10_000_000,
        abnormal_coin: 2,
        unpaired_transfers: 1,
        ..Default::default()
    });
    ensure!(
        suspicious.score >= 70,
        "可疑角色评分应 >= 70，实际 {}",
        suspicious.score
    );
    checks += 1;

    ensure!(
        suspicious.tags.iter().any(|tag| tag == "交易账本缺腿"),
        "可疑角色应命中「交易账本缺腿」"
    );
    checks += 1;

    // 3 IID 规范化
    ensure!(
        normalized_iid(":6a617f69000102542fd9:") == "6A617F69000102542FD9",
        "IID 规范化结果不符"
    );
    checks += 1;

    // 4 奖励文案
    ensure!(
        reward_change(&RewardRow {
            bonus_type: 7,
            bonus_name: "9582200".to_string(),
            bonus_prop: String::new(),
        }) == "9,582,200 元宝",
        "元宝奖励文案不符"
    );
    checks += 1;

    // 5-7 action 方向：未知 action 必须保持中性
    ensure!(
        activity_direction("huilcbjl") == ("获得记录", "+"),
        "已确认获得 action 判定错误"
    );
    checks += 1;
    ensure!(
        activity_direction("yuancxl") == ("消耗记录", "-"),
        "已确认消耗 action 判定错误"
    );
    checks += 1;
    ensure!(
        activity_direction("shouszh") == ("资产事件", ""),
        "未知 action 必须保持中性"
    );
    checks += 1;

    // 8-9 丢弃拾取绕过交易
    let handoff = risk_score(&Facts {
        ground_handoffs: 1,
        ..Default::default()
    });
    ensure!(
        handoff.score == 35,
        "丢弃拾取单独计分应为 35，实际 {}",
        handoff.score
    );
    checks += 1;
    ensure!(
        handoff.tags.iter().any(|tag| tag == "绕过交易转移"),
        "丢弃拾取应命中「绕过交易转移」"
    );
    checks += 1;

    // 10-11 金元宝快照跳增阈值
    let below = gold_snapshot_jumps(
        &[
            snapshot("20260101000000", 10),
            snapshot("20260101000100", 1_000_009),
            snapshot("20260101000200", 1_000_010),
        ],
        DEFAULT_JUMP_MINIMUM,
    );
    ensure!(below.is_empty(), "低于阈值的快照差不应记为跳增");
    checks += 1;

    let exact = gold_snapshot_jumps(
        &[
            snapshot("20260101000000", 10),
            snapshot("20260101000100", 1_000_010),
        ],
        DEFAULT_JUMP_MINIMUM,
    );
    ensure!(
        exact.first().map(|jump| jump.amount) == Some(1_000_000),
        "恰好达到阈值的跳增未被识别"
    );
    checks += 1;

    // 12-15 丢弃/拾取时间线语义
    let drop_row = TransferRow {
        action: "diuqsq".to_string(),
        item_amount: 1,
        item_name: "测试道具".to_string(),
        item_iid: ":A1:".to_string(),
        gid_from: "p1".to_string(),
        gid_to: "(undefined)".to_string(),
    };
    let dropped = transfer_timeline_event(&drop_row, "p1").expect("丢弃方应产生事件");
    ensure!(
        dropped.action == "丢弃资产" && dropped.change == "-1 测试道具",
        "丢弃事件文案不符"
    );
    checks += 1;

    let pickup_row = TransferRow {
        gid_to: "p2".to_string(),
        ..drop_row.clone()
    };
    let picked = transfer_timeline_event(&pickup_row, "p2").expect("拾取方应产生事件");
    ensure!(
        picked.action == "拾取资产" && picked.change == "+1 测试道具",
        "拾取事件文案不符"
    );
    checks += 1;

    ensure!(
        transfer_timeline_event(&pickup_row, "p1").is_none(),
        "拾取行不应对原持有人产生事件"
    );
    checks += 1;

    ensure!(
        transfer_trace_action(&pickup_row) == "地面拾取",
        "溯源动作名不符"
    );
    checks += 1;

    // 16-19 快照账本生命周期
    let ledger = Connection::open_in_memory()?;
    prepare_ledger(&ledger)?;
    let base = AssetRow {
        iid: ":A1:".to_string(),
        name: "item".to_string(),
        owner: "p1".to_string(),
        owner_name: "one".to_string(),
        env: "bag".to_string(),
        pos: 1,
        amount: 1,
    };
    let first = apply_snapshot(&ledger, std::slice::from_ref(&base), "2026-01-01T00:00:00")?;
    ensure!(first.changes.baseline == 1, "首轮扫描应记为 baseline");
    checks += 1;

    let changed = vec![
        AssetRow {
            owner: "p2".to_string(),
            owner_name: "two".to_string(),
            amount: 2,
            ..base.clone()
        },
        AssetRow {
            iid: ":A2:".to_string(),
            ..base.clone()
        },
    ];
    let second = apply_snapshot(&ledger, &changed, "2026-01-01T00:01:00")?;
    ensure!(second.changes.owner_changed == 1, "持有人变化未被识别");
    checks += 1;
    ensure!(second.changes.amount_changed == 1, "数量变化未被识别");
    checks += 1;
    ensure!(second.changes.first_seen == 1, "新增资产未被识别");
    checks += 1;

    let third = apply_snapshot(&ledger, &[], "2026-01-01T00:02:00")?;
    ensure!(third.changes.missing == 2, "离开持有表未被识别");
    checks += 1;

    // 20 币种标签
    ensure!(
        coin_label("gold_coin") == Some("金元宝") && coin_label("silver_coin") == Some("银元宝"),
        "币种标签不符"
    );
    checks += 1;

    Ok(checks)
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_check_passes_all_assertions() {
        let checks = super::run().expect("自检不应失败");
        // Python 版声明 20 项；这里把「命中标签」与「分数」拆成独立断言，共 22 项。
        assert_eq!(checks, 22);
    }
}

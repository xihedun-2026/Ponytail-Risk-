//! WDSF 数据语义。这些常量与判定规则来自交接报告 §3.3「已确认的数据语义」，
//! 是逐个源码调用点确认过的，不要凭字段名或数值正负重新猜测。
//!
//! 未知 action 一律保持中性（`资产事件`），既不算获得也不算消耗。

use crate::format::{is_ascii_digits, number, number_loose};

/// 适配器覆盖的 16 类权威日志源（交接报告 §3.2）。
pub const ASSET_TABLES: [&str; 16] = [
    "money_log",
    "item_transfer_log",
    "equipment_log",
    "cost_coin_log",
    "apply_log",
    "important_log",
    "campaign_log",
    "errand_log",
    "user_log",
    "pet_log",
    "important_pet_log",
    "login_log",
    "coin_order_log",
    "gbuy_action_log",
    "gift_log",
    "important_action_log",
];

/// 奖励日志 `bonus_type` 语义。
pub fn reward_type_label(bonus_type: i64) -> Option<&'static str> {
    match bonus_type {
        1 => Some("道具"),
        2 => Some("经验"),
        3 => Some("道行"),
        7 => Some("元宝"),
        14 => Some("宠物"),
        _ => None,
    }
}

/// `cost_coin_log.cost_type` 币种标签。
pub fn coin_label(cost_type: &str) -> Option<&'static str> {
    match cost_type {
        "gold_coin" => Some("金元宝"),
        "silver_coin" => Some("银元宝"),
        _ => None,
    }
}

/// 已在源码调用点确认为「获得」的 action。扩充前必须先确认调用点。
pub const CONFIRMED_GAIN_ACTIONS: [&str; 11] = [
    "huilcbjl",
    "jinn",
    "guaiwgc",
    "meirdt",
    "hanjqd_sqcd",
    "huoydjqxt",
    "sizn",
    "bangpzyzdz",
    "xiaomsyhqx",
    "bangdmbjl",
    "shoujrz",
];

/// 已确认为「消耗」的 action。
pub const CONFIRMED_COST_ACTIONS: [&str; 1] = ["yuancxl"];

/// `user_log` 中与资产相关的 action。
pub const USER_ASSET_ACTIONS: [&str; 6] = [
    "drop",
    "get",
    "exchange",
    "buy",
    "take_stall_cash",
    "drop_pet",
];

pub fn is_confirmed_gain(action: &str) -> bool {
    CONFIRMED_GAIN_ACTIONS.contains(&action)
}

pub fn is_confirmed_cost(action: &str) -> bool {
    CONFIRMED_COST_ACTIONS.contains(&action)
}

/// 返回 (标题, 正负号前缀)。未知 action 保持中性。
pub fn activity_direction(action: &str) -> (&'static str, &'static str) {
    if is_confirmed_gain(action) {
        ("获得记录", "+")
    } else if is_confirmed_cost(action) {
        ("消耗记录", "-")
    } else {
        ("资产事件", "")
    }
}

/// gid 缺失判定：空串或 `(undefined)`。
pub fn missing_gid(value: &str) -> bool {
    value.is_empty() || value == "(undefined)"
}

/// 奖励日志的一行。
#[derive(Debug, Clone, Default)]
pub struct RewardRow {
    pub bonus_type: i64,
    pub bonus_name: String,
    pub bonus_prop: String,
}

/// 奖励内容的展示文案。
pub fn reward_change(row: &RewardRow) -> String {
    let kind_owned;
    let kind = match reward_type_label(row.bonus_type) {
        Some(label) => label,
        None => {
            kind_owned = format!("类型 {}", row.bonus_type);
            kind_owned.as_str()
        }
    };
    // Python 的 `a or b or c`：空串视为 falsy。
    let value = if !row.bonus_name.is_empty() {
        row.bonus_name.as_str()
    } else if !row.bonus_prop.is_empty() {
        row.bonus_prop.as_str()
    } else {
        "未记录数值"
    };
    match kind {
        "道具" => format!("道具 {value}"),
        "宠物" => format!("宠物 {value}"),
        _ => {
            if is_ascii_digits(value) {
                format!("{} {kind}", number_loose(value))
            } else {
                format!("{value} {kind}")
            }
        }
    }
}

/// `item_transfer_log` 的一行。
#[derive(Debug, Clone, Default)]
pub struct TransferRow {
    pub action: String,
    pub item_amount: i64,
    pub item_name: String,
    pub item_iid: String,
    pub gid_from: String,
    pub gid_to: String,
}

/// 时间线上的一个事件：(动作, 资产变化, 研判备注)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub action: String,
    pub change: String,
    pub note: String,
}

fn transfer_action_name(action: &str) -> &'static str {
    match action {
        "bait" => "摆摊",
        "jiaoy" => "玩家交易",
        _ => "道具转移",
    }
}

/// 把一条转移日志翻译成时间线事件。
///
/// `diuqsq` 是丢弃/拾取：同一 `transfer_id` 串联「A 丢地」与「B 拾取」。
/// 对于既不是收方也不是丢弃方的行，返回 `None`（该行与当前角色无关）。
pub fn transfer_timeline_event(row: &TransferRow, gid: &str) -> Option<TimelineEvent> {
    let amount = number(row.item_amount);
    let item_name = if row.item_name.is_empty() {
        "未知道具"
    } else {
        row.item_name.as_str()
    };
    let iid_note = if row.item_iid.is_empty() {
        "堆叠资产".to_string()
    } else {
        format!("IID {}", row.item_iid)
    };

    if row.action == "diuqsq" {
        if row.gid_to == gid {
            return Some(TimelineEvent {
                action: "拾取资产".to_string(),
                change: format!("+{amount} {item_name}"),
                note: format!("地面拾取 / 原持有人 {} / {iid_note}", row.gid_from),
            });
        }
        if row.gid_from == gid && missing_gid(&row.gid_to) {
            return Some(TimelineEvent {
                action: "丢弃资产".to_string(),
                change: format!("-{amount} {item_name}"),
                note: format!("进入地面或销毁 / {iid_note}"),
            });
        }
        return None;
    }

    let incoming = row.gid_to == gid;
    let sign = if incoming { "+" } else { "-" };
    let action_name = transfer_action_name(&row.action);

    if !row.item_iid.is_empty() {
        let note = if row.action == "bait" {
            "金钱腿与道具腿成对核对".to_string()
        } else {
            format!("原始动作 {}", row.action)
        };
        return Some(TimelineEvent {
            action: format!("{action_name}{}", if incoming { "收取" } else { "转出" }),
            change: format!("{sign}{amount} {item_name}"),
            note,
        });
    }

    Some(TimelineEvent {
        action: format!("{action_name}{}", if incoming { "收款" } else { "付款" }),
        change: format!("{sign}{amount} 金钱"),
        note: "交易流水".to_string(),
    })
}

/// 资产溯源链路上的节点动作名。
pub fn transfer_trace_action(row: &TransferRow) -> String {
    if row.action == "diuqsq" {
        return if !missing_gid(&row.gid_to) {
            "地面拾取".to_string()
        } else {
            "丢弃到地面".to_string()
        };
    }
    match row.action.as_str() {
        "bait" => "摆摊转移".to_string(),
        "jiaoy" => "玩家交易".to_string(),
        other => format!("资产转移 {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_row() -> TransferRow {
        TransferRow {
            action: "diuqsq".to_string(),
            item_amount: 1,
            item_name: "测试道具".to_string(),
            item_iid: ":A1:".to_string(),
            gid_from: "p1".to_string(),
            gid_to: "(undefined)".to_string(),
        }
    }

    #[test]
    fn activity_direction_matches_confirmed_sets() {
        // 对应 Python self_check 断言。
        assert_eq!(activity_direction("huilcbjl"), ("获得记录", "+"));
        assert_eq!(activity_direction("yuancxl"), ("消耗记录", "-"));
        // 未确认的 action 必须保持中性。
        assert_eq!(activity_direction("shouszh"), ("资产事件", ""));
        assert_eq!(activity_direction(""), ("资产事件", ""));
    }

    #[test]
    fn reward_change_formats_gold_coin_amount() {
        // 对应 Python self_check 断言。
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 7,
                bonus_name: "9582200".to_string(),
                bonus_prop: String::new(),
            }),
            "9,582,200 元宝"
        );
    }

    #[test]
    fn reward_change_covers_item_pet_and_unknown_types() {
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 1,
                bonus_name: "玄天令".to_string(),
                ..Default::default()
            }),
            "道具 玄天令"
        );
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 14,
                bonus_name: "雷极兽".to_string(),
                ..Default::default()
            }),
            "宠物 雷极兽"
        );
        // 未知 bonus_type 保留原始编号，不猜语义。
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 99,
                bonus_name: "123".to_string(),
                ..Default::default()
            }),
            "123 类型 99"
        );
        // bonus_name 为空时回落到 bonus_prop，再回落到占位符。
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 2,
                bonus_name: String::new(),
                bonus_prop: "副本".to_string(),
            }),
            "副本 经验"
        );
        assert_eq!(
            reward_change(&RewardRow {
                bonus_type: 3,
                ..Default::default()
            }),
            "未记录数值 道行"
        );
    }

    #[test]
    fn transfer_timeline_event_matches_python_drop_and_pickup() {
        // 对应 Python self_check 的三条断言。
        let drop = drop_row();
        let event = transfer_timeline_event(&drop, "p1").expect("丢弃方应产生事件");
        assert_eq!(event.action, "丢弃资产");
        assert_eq!(event.change, "-1 测试道具");

        let mut pickup = drop_row();
        pickup.gid_to = "p2".to_string();
        let event = transfer_timeline_event(&pickup, "p2").expect("拾取方应产生事件");
        assert_eq!(event.action, "拾取资产");
        assert_eq!(event.change, "+1 测试道具");

        // 拾取行对原持有人不产生事件（gid_to 已不是 undefined）。
        assert!(transfer_timeline_event(&pickup, "p1").is_none());
    }

    #[test]
    fn transfer_trace_action_matches_python() {
        let mut pickup = drop_row();
        pickup.gid_to = "p2".to_string();
        // 对应 Python self_check 断言。
        assert_eq!(transfer_trace_action(&pickup), "地面拾取");
        assert_eq!(transfer_trace_action(&drop_row()), "丢弃到地面");
        assert_eq!(
            transfer_trace_action(&TransferRow {
                action: "bait".to_string(),
                ..Default::default()
            }),
            "摆摊转移"
        );
        assert_eq!(
            transfer_trace_action(&TransferRow {
                action: "jiaoy".to_string(),
                ..Default::default()
            }),
            "玩家交易"
        );
        assert_eq!(
            transfer_trace_action(&TransferRow {
                action: "qita".to_string(),
                ..Default::default()
            }),
            "资产转移 qita"
        );
    }

    #[test]
    fn bait_and_cash_legs_render_distinct_wording() {
        let bait_item = TransferRow {
            action: "bait".to_string(),
            item_amount: 3,
            item_name: "玄天令".to_string(),
            item_iid: ":B1:".to_string(),
            gid_from: "p1".to_string(),
            gid_to: "p2".to_string(),
        };
        let event = transfer_timeline_event(&bait_item, "p2").unwrap();
        assert_eq!(event.action, "摆摊收取");
        assert_eq!(event.change, "+3 玄天令");
        assert_eq!(event.note, "金钱腿与道具腿成对核对");

        // 没有 item_iid 的腿是金钱腿。
        let bait_cash = TransferRow {
            item_iid: String::new(),
            ..bait_item.clone()
        };
        let event = transfer_timeline_event(&bait_cash, "p1").unwrap();
        assert_eq!(event.action, "摆摊付款");
        assert_eq!(event.change, "-3 金钱");
        assert_eq!(event.note, "交易流水");
    }

    #[test]
    fn coin_labels_match_python() {
        // 对应 Python self_check 断言。
        assert_eq!(coin_label("gold_coin"), Some("金元宝"));
        assert_eq!(coin_label("silver_coin"), Some("银元宝"));
        assert_eq!(coin_label("unknown"), None);
    }

    #[test]
    fn asset_tables_cover_sixteen_sources() {
        assert_eq!(ASSET_TABLES.len(), 16);
        assert!(ASSET_TABLES.contains(&"login_log"));
        assert!(ASSET_TABLES.contains(&"important_action_log"));
    }

    #[test]
    fn missing_gid_matches_python() {
        assert!(missing_gid(""));
        assert!(missing_gid("(undefined)"));
        assert!(!missing_gid("p1"));
    }
}

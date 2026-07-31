//! 游戏行为风控核心语义与评分。
//!
//! 本 crate 不做任何 I/O，全部逻辑可离线单测，
//! 对应交接报告 §7 阶段 4「Rust 账本和规则」中与数据源无关的那部分。
//!
//! 规则处于影子模式：只产出证据与复核建议，不自动封号、扣款或冻结。

pub mod format;
pub mod score;
pub mod semantics;

pub use format::{is_ascii_digits, normalized_iid, number, number_loose, stamp_label};
pub use score::{
    gold_snapshot_jumps, risk_score, status_for, Assessment, CoinSnapshot, Facts, GoldJump,
    DEFAULT_JUMP_MINIMUM,
};
pub use semantics::{
    activity_direction, coin_label, is_confirmed_cost, is_confirmed_gain, missing_gid,
    reward_change, reward_type_label, transfer_timeline_event, transfer_trace_action, RewardRow,
    TimelineEvent, TransferRow, ASSET_TABLES, CONFIRMED_COST_ACTIONS, CONFIRMED_GAIN_ACTIONS,
    USER_ASSET_ACTIONS,
};

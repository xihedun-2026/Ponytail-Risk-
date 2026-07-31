//! 风险评分。规则集与权重逐条对齐 `tools/wdsf_live_data.py::risk_score`。
//!
//! 交接报告 §3.4 明确：这些规则只产生证据和复核建议，
//! 不自动封号、扣款或冻结（影子模式）。

use serde::{Deserialize, Serialize};

use crate::format::number;

/// 金元宝快照跳增。`from`/`to` 是 `login_log.update_time`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldJump {
    #[serde(rename = "from")]
    pub from_time: String,
    #[serde(rename = "to")]
    pub to_time: String,
    pub amount: i64,
}

/// 单个角色的风险证据。字段名与 Python 的 evidence 字典保持一致，
/// 以便阶段 5 的双算差异报告可以逐字段比对。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Facts {
    #[serde(default)]
    pub cash: i64,
    #[serde(default)]
    pub gold_coin: i64,
    #[serde(default)]
    pub silver_coin: i64,
    #[serde(default)]
    pub coin_observed_at: String,
    #[serde(default)]
    pub median_gold_coin: i64,
    #[serde(default)]
    pub abnormal_coin: i64,
    #[serde(default)]
    pub transfer_count: i64,
    #[serde(default)]
    pub unpaired_transfers: i64,
    #[serde(default)]
    pub same_device_peers: i64,
    #[serde(default)]
    pub funnel_source_peers: i64,
    #[serde(default)]
    pub funnel_asset_rows: i64,
    #[serde(default)]
    pub burst_funnel_source_peers: i64,
    #[serde(default)]
    pub burst_funnel_asset_rows: i64,
    #[serde(default)]
    pub returned_asset_ids: i64,
    #[serde(default)]
    pub returned_asset_peers: i64,
    #[serde(default)]
    pub max_daily_active_span_minutes: i64,
    #[serde(default)]
    pub max_daily_active_events: i64,
    #[serde(default)]
    pub long_active_days: i64,
    #[serde(default)]
    pub mechanical_action: String,
    #[serde(default)]
    pub mechanical_action_events: i64,
    #[serde(default)]
    pub mechanical_interval_seconds: i64,
    #[serde(default)]
    pub mechanical_interval_ratio_permille: i64,
    #[serde(default)]
    pub mechanical_span_minutes: i64,
    #[serde(default)]
    pub reward_burst_action: String,
    #[serde(default)]
    pub reward_burst_events: i64,
    #[serde(default)]
    pub rapid_reward_outflows: i64,
    #[serde(default)]
    pub rapid_reward_outflow_days: i64,
    #[serde(default)]
    pub reward_outflow_target_peers: i64,
    #[serde(default)]
    pub configured_cap_action: String,
    #[serde(default)]
    pub configured_cap_daily_events: i64,
    #[serde(default)]
    pub configured_cap_daily_limit: i64,
    #[serde(default)]
    pub configured_cap_burst_events: i64,
    #[serde(default)]
    pub configured_cap_burst_limit: i64,
    #[serde(default)]
    pub shared_device_accounts: i64,
    #[serde(default)]
    pub shared_ip_accounts: i64,
    #[serde(default)]
    pub ground_handoffs: i64,
    #[serde(default)]
    pub unexplained_gold_jumps: i64,
    #[serde(default)]
    pub unexplained_gold_increase: i64,
    #[serde(default)]
    pub gold_jumps: Vec<GoldJump>,
    #[serde(default)]
    pub peers: i64,
    #[serde(default)]
    pub item_count: i64,
    #[serde(default)]
    pub pet_count: i64,
    #[serde(default)]
    pub reward_count: i64,
}

/// 评分结果：分数、标签、可读理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub score: i64,
    pub tags: Vec<String>,
    pub reasons: Vec<String>,
}

/// 元宝存量偏离的绝对下限：低于此值不论倍数都不告警。
const GOLD_COIN_ABSOLUTE_FLOOR: i64 = 100_000_000;
/// 元宝存量偏离的中位数倍数阈值。
const GOLD_COIN_MEDIAN_MULTIPLE: i64 = 8;
/// 高频流转的 30 天交易笔数阈值。
const HIGH_FREQUENCY_TRANSFERS: i64 = 20;
/// 单向向目标角色输入道具的不同来源角色数。
const FUNNEL_SOURCE_MINIMUM: i64 = 4;
/// 上述单向来源在 30 天内输入的道具流水行数。
const FUNNEL_ASSET_ROWS_MINIMUM: i64 = 8;
/// 短时归集沿用来源数与流水数双阈值，但要求发生在同一个 10 分钟窗口。
const BURST_FUNNEL_SOURCE_MINIMUM: i64 = 4;
const BURST_FUNNEL_ASSET_ROWS_MINIMUM: i64 = 8;
/// 同一批 IID 在角色与交易对手之间双向流转的最小数量。
const RETURNED_ASSET_ID_MINIMUM: i64 = 3;
const LONG_ACTIVE_DAYS_MINIMUM: i64 = 2;
const LONG_ACTIVE_SPAN_MINUTES: i64 = 18 * 60;
const LONG_ACTIVE_EVENTS_MINIMUM: i64 = 100;
const MECHANICAL_EVENTS_MINIMUM: i64 = 20;
const MECHANICAL_RATIO_PERMILLE: i64 = 800;
const MECHANICAL_MAX_INTERVAL_SECONDS: i64 = 300;
const MECHANICAL_SPAN_MINUTES: i64 = 30;
const REWARD_BURST_EVENTS_MINIMUM: i64 = 10;
const RAPID_REWARD_OUTFLOWS_MINIMUM: i64 = 5;
const RAPID_REWARD_OUTFLOW_DAYS_MINIMUM: i64 = 3;
const RAPID_REWARD_OUTFLOW_TARGETS_MAXIMUM: i64 = 2;

/// 逐条规则累加风险分。上限 100。
///
/// 交接报告 §4 限制 6：测试服样本量有限，这些阈值不能直接当生产阈值。
pub fn risk_score(facts: &Facts) -> Assessment {
    let mut score: i64 = 0;
    let mut tags: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    if facts.abnormal_coin != 0 {
        score += (20 + facts.abnormal_coin * 2).min(40);
        tags.push("币值校验异常".to_string());
        reasons.push(format!("出现 {} 次服务端币值校验异常", facts.abnormal_coin));
    }

    // max(1, ...) 避免中位数为 0 时倍数判定失效。
    let median_gold_coin = facts.median_gold_coin.max(1);
    if facts.gold_coin >= GOLD_COIN_ABSOLUTE_FLOOR
        && facts.gold_coin >= median_gold_coin.saturating_mul(GOLD_COIN_MEDIAN_MULTIPLE)
    {
        score += 25;
        tags.push("元宝存量偏离".to_string());
        reasons.push("当前元宝显著高于角色群体中位数".to_string());
    }

    if facts.unpaired_transfers != 0 {
        score += 30;
        tags.push("交易账本缺腿".to_string());
        reasons.push("存在无法同时匹配道具腿和金钱腿的交易".to_string());
    }

    if facts.same_device_peers != 0 {
        score += 12;
        tags.push("同设备交易".to_string());
        reasons.push("交易双方出现相同设备标识".to_string());
    }

    if facts.burst_funnel_source_peers >= BURST_FUNNEL_SOURCE_MINIMUM
        && facts.burst_funnel_asset_rows >= BURST_FUNNEL_ASSET_ROWS_MINIMUM
    {
        score += 35;
        tags.push("短时资产归集".to_string());
        reasons.push(format!(
            "10 分钟内有 {} 个角色输入资产，共 {} 条道具流水",
            facts.burst_funnel_source_peers, facts.burst_funnel_asset_rows
        ));
    } else if facts.funnel_source_peers >= FUNNEL_SOURCE_MINIMUM
        && facts.funnel_asset_rows >= FUNNEL_ASSET_ROWS_MINIMUM
    {
        score += 25;
        tags.push("多账号资产归集".to_string());
        reasons.push(format!(
            "近 30 天有 {} 个角色单向输入资产，共 {} 条道具流水",
            facts.funnel_source_peers, facts.funnel_asset_rows
        ));
    }

    if facts.returned_asset_ids >= RETURNED_ASSET_ID_MINIMUM {
        score += 20;
        tags.push("资产循环回流".to_string());
        reasons.push(format!(
            "近 30 天有 {} 个资产 IID 与 {} 个交易对手发生双向回流",
            facts.returned_asset_ids, facts.returned_asset_peers
        ));
    }

    if facts.long_active_days >= LONG_ACTIVE_DAYS_MINIMUM
        && facts.max_daily_active_span_minutes >= LONG_ACTIVE_SPAN_MINUTES
        && facts.max_daily_active_events >= LONG_ACTIVE_EVENTS_MINIMUM
    {
        score += 20;
        tags.push("超长持续活跃".to_string());
        reasons.push(format!(
            "近 30 天有 {} 天活跃超过 18 小时，单日最高 {} 个行为事件",
            facts.long_active_days, facts.max_daily_active_events
        ));
    }

    if facts.mechanical_action_events >= MECHANICAL_EVENTS_MINIMUM
        && (1..=MECHANICAL_MAX_INTERVAL_SECONDS).contains(&facts.mechanical_interval_seconds)
        && facts.mechanical_interval_ratio_permille >= MECHANICAL_RATIO_PERMILLE
        && facts.mechanical_span_minutes >= MECHANICAL_SPAN_MINUTES
    {
        score += 25;
        tags.push("机械周期行为".to_string());
        reasons.push(format!(
            "行为 {} 连续 {} 次，{} 秒间隔重复率 {}%",
            facts.mechanical_action,
            facts.mechanical_action_events,
            facts.mechanical_interval_seconds,
            facts.mechanical_interval_ratio_permille / 10
        ));
    }

    if facts.reward_burst_events >= REWARD_BURST_EVENTS_MINIMUM {
        score += 25;
        tags.push("奖励爆发异常".to_string());
        reasons.push(format!(
            "奖励动作 {} 在 10 分钟内出现 {} 次去重发放",
            facts.reward_burst_action, facts.reward_burst_events
        ));
    }

    let daily_cap_exceeded = facts.configured_cap_daily_limit > 0
        && facts.configured_cap_daily_events > facts.configured_cap_daily_limit;
    let burst_cap_exceeded = facts.configured_cap_burst_limit > 0
        && facts.configured_cap_burst_events > facts.configured_cap_burst_limit;
    if !facts.configured_cap_action.is_empty() && (daily_cap_exceeded || burst_cap_exceeded) {
        score += 40;
        tags.push("玩法产出超限".to_string());
        let mut limits = Vec::new();
        if daily_cap_exceeded {
            limits.push(format!(
                "单日 {}/{}",
                facts.configured_cap_daily_events, facts.configured_cap_daily_limit
            ));
        }
        if burst_cap_exceeded {
            limits.push(format!(
                "10 分钟 {}/{}",
                facts.configured_cap_burst_events, facts.configured_cap_burst_limit
            ));
        }
        reasons.push(format!(
            "玩法 {} 的去重奖励发放超过配置上限：{}",
            facts.configured_cap_action,
            limits.join("，")
        ));
    }

    if facts.rapid_reward_outflows >= RAPID_REWARD_OUTFLOWS_MINIMUM
        && facts.rapid_reward_outflow_days >= RAPID_REWARD_OUTFLOW_DAYS_MINIMUM
        && (1..=RAPID_REWARD_OUTFLOW_TARGETS_MAXIMUM).contains(&facts.reward_outflow_target_peers)
    {
        score += 20;
        tags.push("奖励快速归集".to_string());
        reasons.push(format!(
            "{} 次奖励后 10 分钟内转出道具，跨 {} 天集中到 {} 个目标角色",
            facts.rapid_reward_outflows,
            facts.rapid_reward_outflow_days,
            facts.reward_outflow_target_peers
        ));
    }

    if facts.ground_handoffs != 0 {
        score += 35;
        tags.push("绕过交易转移".to_string());
        reasons.push("存在角色丢到地面后由另一角色拾取的资产转移".to_string());
    }

    if facts.unexplained_gold_increase != 0 {
        score += (25 + facts.unexplained_gold_jumps * 3).min(40);
        tags.push("元宝快照跳增".to_string());
        reasons.push(format!(
            "金元宝快照出现 {} 次跳增，累计 {}，在已接入来源日志中未找到对应记录",
            facts.unexplained_gold_jumps,
            number(facts.unexplained_gold_increase)
        ));
    }

    if facts.transfer_count >= HIGH_FREQUENCY_TRANSFERS {
        score += 20;
        tags.push("高频流转".to_string());
        reasons.push("近 30 天资产流转次数较高".to_string());
    }

    if tags.is_empty() {
        tags.push("未见强异常".to_string());
    }

    Assessment {
        score: score.min(100),
        tags,
        reasons,
    }
}

/// 分数 -> (状态文案, 前端色调)。
pub fn status_for(score: i64) -> (&'static str, &'static str) {
    if score >= 70 {
        ("高风险", "danger")
    } else if score >= 35 {
        ("观察", "warning")
    } else {
        ("正常", "safe")
    }
}

/// `login_log` 的金元宝快照（按时间升序传入）。
#[derive(Debug, Clone, Default)]
pub struct CoinSnapshot {
    pub update_time: String,
    pub gold_coin: i64,
}

/// 默认跳增阈值：单次快照差 >= 100 万金元宝。
pub const DEFAULT_JUMP_MINIMUM: i64 = 1_000_000;

/// 找出相邻快照之间的大额跳增。
///
/// 交接报告 §4 限制 5：登录日志是余额快照而非复式账本，
/// 因此跳增只能作为复核信号，不能单独定性。
pub fn gold_snapshot_jumps(rows: &[CoinSnapshot], minimum: i64) -> Vec<GoldJump> {
    let mut jumps = Vec::new();
    for pair in rows.windows(2) {
        let delta = pair[1].gold_coin - pair[0].gold_coin;
        if delta >= minimum {
            jumps.push(GoldJump {
                from_time: pair[0].update_time.clone(),
                to_time: pair[1].update_time.clone(),
                amount: delta,
            });
        }
    }
    jumps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(update_time: &str, gold_coin: i64) -> CoinSnapshot {
        CoinSnapshot {
            update_time: update_time.to_string(),
            gold_coin,
        }
    }

    #[test]
    fn normal_player_scores_zero() {
        // 对应 Python self_check：normal == 0。
        let assessment = risk_score(&Facts {
            gold_coin: 1000,
            median_gold_coin: 1000,
            ..Default::default()
        });
        assert_eq!(assessment.score, 0);
        assert_eq!(assessment.tags, vec!["未见强异常".to_string()]);
        assert!(assessment.reasons.is_empty());
    }

    #[test]
    fn suspicious_player_crosses_high_risk_threshold() {
        // 对应 Python self_check：suspicious >= 70 且命中「交易账本缺腿」。
        let assessment = risk_score(&Facts {
            gold_coin: 900_000_000,
            median_gold_coin: 10_000_000,
            abnormal_coin: 2,
            unpaired_transfers: 1,
            ..Default::default()
        });
        // 24（币值异常）+ 25（存量偏离）+ 30（缺腿）= 79
        assert_eq!(assessment.score, 79);
        assert!(assessment.score >= 70);
        assert!(assessment.tags.contains(&"交易账本缺腿".to_string()));
        assert_eq!(status_for(assessment.score), ("高风险", "danger"));
    }

    #[test]
    fn ground_handoff_alone_scores_thirty_five() {
        // 对应 Python self_check：handoff_score == 35。
        let assessment = risk_score(&Facts {
            ground_handoffs: 1,
            ..Default::default()
        });
        assert_eq!(assessment.score, 35);
        assert!(assessment.tags.contains(&"绕过交易转移".to_string()));
    }

    #[test]
    fn abnormal_coin_points_are_capped_at_forty() {
        assert_eq!(
            risk_score(&Facts {
                abnormal_coin: 1,
                ..Default::default()
            })
            .score,
            22
        );
        // 20 + 100*2 会超过上限，必须夹到 40。
        assert_eq!(
            risk_score(&Facts {
                abnormal_coin: 100,
                ..Default::default()
            })
            .score,
            40
        );
    }

    #[test]
    fn unexplained_gold_points_are_capped_at_forty() {
        let assessment = risk_score(&Facts {
            unexplained_gold_increase: 5_000_000,
            unexplained_gold_jumps: 2,
            ..Default::default()
        });
        // 25 + 2*3 = 31
        assert_eq!(assessment.score, 31);
        assert!(assessment.reasons[0].contains("5,000,000"));

        let capped = risk_score(&Facts {
            unexplained_gold_increase: 1,
            unexplained_gold_jumps: 50,
            ..Default::default()
        });
        assert_eq!(capped.score, 40);
    }

    #[test]
    fn total_score_is_capped_at_hundred() {
        let assessment = risk_score(&Facts {
            abnormal_coin: 100,
            gold_coin: 900_000_000,
            median_gold_coin: 1,
            unpaired_transfers: 1,
            same_device_peers: 1,
            ground_handoffs: 1,
            unexplained_gold_increase: 1,
            unexplained_gold_jumps: 50,
            transfer_count: 100,
            ..Default::default()
        });
        assert_eq!(assessment.score, 100);
    }

    #[test]
    fn gold_coin_deviation_needs_both_floor_and_multiple() {
        // 达到倍数但没到绝对下限：不告警。
        assert_eq!(
            risk_score(&Facts {
                gold_coin: 80_000_000,
                median_gold_coin: 1_000_000,
                ..Default::default()
            })
            .score,
            0
        );
        // 达到下限但没到倍数：不告警。
        assert_eq!(
            risk_score(&Facts {
                gold_coin: 100_000_000,
                median_gold_coin: 50_000_000,
                ..Default::default()
            })
            .score,
            0
        );
        // 两者都满足：+25。
        assert_eq!(
            risk_score(&Facts {
                gold_coin: 100_000_000,
                median_gold_coin: 10_000_000,
                ..Default::default()
            })
            .score,
            25
        );
    }

    #[test]
    fn high_frequency_threshold_is_inclusive() {
        assert_eq!(
            risk_score(&Facts {
                transfer_count: 19,
                ..Default::default()
            })
            .score,
            0
        );
        assert_eq!(
            risk_score(&Facts {
                transfer_count: 20,
                ..Default::default()
            })
            .score,
            20
        );
    }

    #[test]
    fn asset_funnel_requires_both_thresholds() {
        assert_eq!(
            risk_score(&Facts {
                funnel_source_peers: FUNNEL_SOURCE_MINIMUM - 1,
                funnel_asset_rows: 100,
                ..Default::default()
            })
            .score,
            0
        );
        assert_eq!(
            risk_score(&Facts {
                funnel_source_peers: 100,
                funnel_asset_rows: FUNNEL_ASSET_ROWS_MINIMUM - 1,
                ..Default::default()
            })
            .score,
            0
        );

        let assessment = risk_score(&Facts {
            funnel_source_peers: FUNNEL_SOURCE_MINIMUM,
            funnel_asset_rows: FUNNEL_ASSET_ROWS_MINIMUM,
            ..Default::default()
        });
        assert_eq!(assessment.score, 25);
        assert!(assessment.tags.contains(&"多账号资产归集".to_string()));
        assert!(assessment.reasons[0].contains("4 个角色"));
        assert!(assessment.reasons[0].contains("8 条道具流水"));
    }

    #[test]
    fn shared_device_and_ip_groups_are_evidence_only() {
        let assessment = risk_score(&Facts {
            shared_device_accounts: 100,
            shared_ip_accounts: 100,
            ..Default::default()
        });
        assert_eq!(assessment.score, 0);
        assert_eq!(assessment.tags, vec!["未见强异常".to_string()]);
    }

    #[test]
    fn burst_funnel_requires_both_thresholds_and_supersedes_long_window_rule() {
        assert_eq!(
            risk_score(&Facts {
                burst_funnel_source_peers: BURST_FUNNEL_SOURCE_MINIMUM - 1,
                burst_funnel_asset_rows: 100,
                ..Default::default()
            })
            .score,
            0
        );
        assert_eq!(
            risk_score(&Facts {
                burst_funnel_source_peers: 100,
                burst_funnel_asset_rows: BURST_FUNNEL_ASSET_ROWS_MINIMUM - 1,
                ..Default::default()
            })
            .score,
            0
        );

        let assessment = risk_score(&Facts {
            funnel_source_peers: FUNNEL_SOURCE_MINIMUM,
            funnel_asset_rows: FUNNEL_ASSET_ROWS_MINIMUM,
            burst_funnel_source_peers: BURST_FUNNEL_SOURCE_MINIMUM,
            burst_funnel_asset_rows: BURST_FUNNEL_ASSET_ROWS_MINIMUM,
            ..Default::default()
        });
        assert_eq!(assessment.score, 35);
        assert_eq!(assessment.tags, vec!["短时资产归集".to_string()]);
    }

    #[test]
    fn returned_asset_ids_have_an_inclusive_threshold() {
        assert_eq!(
            risk_score(&Facts {
                returned_asset_ids: RETURNED_ASSET_ID_MINIMUM - 1,
                returned_asset_peers: 1,
                ..Default::default()
            })
            .score,
            0
        );
        let assessment = risk_score(&Facts {
            returned_asset_ids: RETURNED_ASSET_ID_MINIMUM,
            returned_asset_peers: 1,
            ..Default::default()
        });
        assert_eq!(assessment.score, 20);
        assert!(assessment.tags.contains(&"资产循环回流".to_string()));
    }

    #[test]
    fn long_activity_requires_span_density_and_multiple_days() {
        for facts in [
            Facts {
                long_active_days: 1,
                max_daily_active_span_minutes: LONG_ACTIVE_SPAN_MINUTES,
                max_daily_active_events: LONG_ACTIVE_EVENTS_MINIMUM,
                ..Default::default()
            },
            Facts {
                long_active_days: LONG_ACTIVE_DAYS_MINIMUM,
                max_daily_active_span_minutes: LONG_ACTIVE_SPAN_MINUTES - 1,
                max_daily_active_events: LONG_ACTIVE_EVENTS_MINIMUM,
                ..Default::default()
            },
            Facts {
                long_active_days: LONG_ACTIVE_DAYS_MINIMUM,
                max_daily_active_span_minutes: LONG_ACTIVE_SPAN_MINUTES,
                max_daily_active_events: LONG_ACTIVE_EVENTS_MINIMUM - 1,
                ..Default::default()
            },
        ] {
            assert_eq!(risk_score(&facts).score, 0);
        }
        assert_eq!(
            risk_score(&Facts {
                long_active_days: LONG_ACTIVE_DAYS_MINIMUM,
                max_daily_active_span_minutes: LONG_ACTIVE_SPAN_MINUTES,
                max_daily_active_events: LONG_ACTIVE_EVENTS_MINIMUM,
                ..Default::default()
            })
            .score,
            20
        );
    }

    #[test]
    fn mechanical_behavior_requires_every_guard() {
        let base = Facts {
            mechanical_action: "user:start_combat".to_string(),
            mechanical_action_events: MECHANICAL_EVENTS_MINIMUM,
            mechanical_interval_seconds: MECHANICAL_MAX_INTERVAL_SECONDS,
            mechanical_interval_ratio_permille: MECHANICAL_RATIO_PERMILLE,
            mechanical_span_minutes: MECHANICAL_SPAN_MINUTES,
            ..Default::default()
        };
        let assessment = risk_score(&base);
        assert_eq!(assessment.score, 25);
        assert!(assessment.tags.contains(&"机械周期行为".to_string()));

        for facts in [
            Facts {
                mechanical_action_events: MECHANICAL_EVENTS_MINIMUM - 1,
                ..base.clone()
            },
            Facts {
                mechanical_interval_seconds: 0,
                ..base.clone()
            },
            Facts {
                mechanical_interval_ratio_permille: MECHANICAL_RATIO_PERMILLE - 1,
                ..base.clone()
            },
            Facts {
                mechanical_span_minutes: MECHANICAL_SPAN_MINUTES - 1,
                ..base
            },
        ] {
            assert_eq!(risk_score(&facts).score, 0);
        }
    }

    #[test]
    fn reward_burst_threshold_is_inclusive() {
        assert_eq!(
            risk_score(&Facts {
                reward_burst_events: REWARD_BURST_EVENTS_MINIMUM - 1,
                ..Default::default()
            })
            .score,
            0
        );
        assert_eq!(
            risk_score(&Facts {
                reward_burst_action: "huilcbjl".to_string(),
                reward_burst_events: REWARD_BURST_EVENTS_MINIMUM,
                ..Default::default()
            })
            .score,
            25
        );
    }

    #[test]
    fn rapid_reward_outflow_requires_count_days_and_concentrated_targets() {
        let base = Facts {
            rapid_reward_outflows: RAPID_REWARD_OUTFLOWS_MINIMUM,
            rapid_reward_outflow_days: RAPID_REWARD_OUTFLOW_DAYS_MINIMUM,
            reward_outflow_target_peers: RAPID_REWARD_OUTFLOW_TARGETS_MAXIMUM,
            ..Default::default()
        };
        assert_eq!(risk_score(&base).score, 20);
        for facts in [
            Facts {
                rapid_reward_outflows: RAPID_REWARD_OUTFLOWS_MINIMUM - 1,
                ..base.clone()
            },
            Facts {
                rapid_reward_outflow_days: RAPID_REWARD_OUTFLOW_DAYS_MINIMUM - 1,
                ..base.clone()
            },
            Facts {
                reward_outflow_target_peers: 0,
                ..base.clone()
            },
            Facts {
                reward_outflow_target_peers: RAPID_REWARD_OUTFLOW_TARGETS_MAXIMUM + 1,
                ..base
            },
        ] {
            assert_eq!(risk_score(&facts).score, 0);
        }
    }

    #[test]
    fn configured_gameplay_cap_only_scores_above_the_limit() {
        let exact = Facts {
            configured_cap_action: "huilcbjl".to_string(),
            configured_cap_daily_events: 100,
            configured_cap_daily_limit: 100,
            configured_cap_burst_events: 10,
            configured_cap_burst_limit: 10,
            ..Default::default()
        };
        assert_eq!(risk_score(&exact).score, 0);
        let exceeded = Facts {
            configured_cap_daily_events: 101,
            ..exact
        };
        let assessment = risk_score(&exceeded);
        assert_eq!(assessment.score, 40);
        assert_eq!(assessment.tags, ["玩法产出超限"]);
        assert!(assessment.reasons[0].contains("单日 101/100"));
    }

    #[test]
    fn status_bands_match_python() {
        assert_eq!(status_for(0), ("正常", "safe"));
        assert_eq!(status_for(34), ("正常", "safe"));
        assert_eq!(status_for(35), ("观察", "warning"));
        assert_eq!(status_for(69), ("观察", "warning"));
        assert_eq!(status_for(70), ("高风险", "danger"));
        assert_eq!(status_for(100), ("高风险", "danger"));
    }

    #[test]
    fn gold_snapshot_jumps_ignore_below_threshold() {
        // 对应 Python self_check：这组快照不产生跳增。
        let rows = vec![
            snapshot("20260101000000", 10),
            snapshot("20260101000100", 1_000_009),
            snapshot("20260101000200", 1_000_010),
        ];
        assert!(gold_snapshot_jumps(&rows, DEFAULT_JUMP_MINIMUM).is_empty());
    }

    #[test]
    fn gold_snapshot_jumps_detect_exact_threshold() {
        // 对应 Python self_check：amount == 1_000_000。
        let rows = vec![
            snapshot("20260101000000", 10),
            snapshot("20260101000100", 1_000_010),
        ];
        let jumps = gold_snapshot_jumps(&rows, DEFAULT_JUMP_MINIMUM);
        assert_eq!(jumps.len(), 1);
        assert_eq!(jumps[0].amount, 1_000_000);
        assert_eq!(jumps[0].from_time, "20260101000000");
        assert_eq!(jumps[0].to_time, "20260101000100");
    }

    #[test]
    fn gold_snapshot_jumps_handle_short_input() {
        assert!(gold_snapshot_jumps(&[], DEFAULT_JUMP_MINIMUM).is_empty());
        assert!(
            gold_snapshot_jumps(&[snapshot("20260101000000", 1)], DEFAULT_JUMP_MINIMUM).is_empty()
        );
    }

    #[test]
    fn gold_jump_serializes_with_python_key_names() {
        let jump = GoldJump {
            from_time: "20260101000000".to_string(),
            to_time: "20260101000100".to_string(),
            amount: 1_000_000,
        };
        let value = serde_json::to_value(&jump).unwrap();
        assert_eq!(value["from"], "20260101000000");
        assert_eq!(value["to"], "20260101000100");
        assert_eq!(value["amount"], 1_000_000);
    }
}

//! 业务查询。每个函数对应 `tools/risk_live_data.py` 的同名函数，
//! 输出 JSON 结构与之保持一致，供阶段 5 的双算差异报告逐字段比对。
//!
//! SQL 里的 `dl_mdb_1` / `dl_ldb_1` 是占位库名，由 `GameDatabase::bind_databases` 换成实际库名。

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, Local, NaiveDateTime};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use risk_core::{
    activity_direction, coin_label, gold_snapshot_jumps, is_confirmed_cost, is_confirmed_gain,
    number, reward_change, risk_score, stamp_label, status_for, transfer_timeline_event,
    transfer_trace_action, Assessment, CoinSnapshot, Facts, GoldJump, RewardRow, TransferRow,
    ASSET_TABLES, DEFAULT_JUMP_MINIMUM,
};
use risk_ledger::{ledger_events, EventKind, LedgerEvent};

use crate::{GameDatabase, Param, Row};

/// 查不到目标时的错误。引擎会把它翻译成 `{"error": ...}` 并以退出码 2 结束，
/// `server.mjs` 据此返回 404。
#[derive(Debug)]
pub struct LookupError(pub String);

impl std::fmt::Display for LookupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for LookupError {}

/// 已确认的奖励 action 集合，内联进 SQL 的 IN 列表。
/// 这里是常量拼接而非用户输入，不构成注入面。
const GAIN_ACTION_SQL_LIST: &str = "'huilcbjl','jinn','guaiwgc','meirdt','hanjqd_sqcd','huoydjqxt','sizn','bangpzyzdz','xiaomsyhqx','bangdmbjl','shoujrz'";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GameplayCap {
    action: String,
    label: String,
    daily_limit: i64,
    burst10m_limit: i64,
    enabled: bool,
}

fn parse_gameplay_caps(raw: &str) -> Result<Vec<GameplayCap>> {
    let caps: Vec<GameplayCap> =
        serde_json::from_str(raw).context("RISK_GAMEPLAY_CAPS_JSON is invalid")?;
    if caps.len() > 100 {
        bail!("RISK_GAMEPLAY_CAPS_JSON has too many entries");
    }
    let mut actions = HashSet::new();
    for cap in &caps {
        let valid_action = !cap.action.is_empty()
            && cap.action.len() <= 64
            && cap
                .action
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'));
        if !valid_action || !actions.insert(cap.action.clone()) {
            bail!("gameplay cap action is invalid or duplicated");
        }
        if cap.label.trim().is_empty()
            || cap.label.chars().count() > 80
            || cap.label.chars().any(char::is_control)
        {
            bail!("gameplay cap label is invalid");
        }
        if !(0..=1_000_000).contains(&cap.daily_limit)
            || !(0..=100_000).contains(&cap.burst10m_limit)
            || (cap.enabled && cap.daily_limit == 0 && cap.burst10m_limit == 0)
        {
            bail!("gameplay cap limit is invalid");
        }
    }
    Ok(caps)
}

fn gameplay_caps_from_env() -> Result<Vec<GameplayCap>> {
    match env::var("RISK_GAMEPLAY_CAPS_JSON") {
        Ok(raw) if !raw.trim().is_empty() => parse_gameplay_caps(&raw),
        _ => Ok(Vec::new()),
    }
}

fn suggested_gameplay_limit(peak: i64) -> i64 {
    if peak <= 0 {
        return 0;
    }
    peak.saturating_add((peak / 5).max(3))
}

fn reward_action_sql_list(caps: &[GameplayCap]) -> String {
    let configured = caps
        .iter()
        .filter(|cap| cap.enabled && !is_confirmed_gain(&cap.action))
        .map(|cap| format!("'{}'", cap.action))
        .collect::<Vec<_>>();
    if configured.is_empty() {
        GAIN_ACTION_SQL_LIST.to_string()
    } else {
        // ponytail: action 已限制为 ASCII 标识符，因此直接拼 IN 列表安全且无需扩展数据库参数层。
        format!("{GAIN_ACTION_SQL_LIST},{}", configured.join(","))
    }
}

const TIMESTAMP_FORMAT: &str = "%Y%m%d%H%M%S";

fn now_stamp_minus(duration: ChronoDuration) -> String {
    (Local::now() - duration)
        .format(TIMESTAMP_FORMAT)
        .to_string()
}

/// Python `round()` 的 half-to-even 语义。用在风险占比和趋势柱上，
/// 保证与 Python 版在 .5 边界上给出相同数字。
fn python_round(value: f64, digits: u32) -> f64 {
    let scale = 10f64.powi(digits as i32);
    let scaled = value * scale;
    let floor = scaled.floor();
    let fraction = scaled - floor;
    let rounded = if (fraction - 0.5).abs() < 1e-9 {
        // 正好 .5：取偶数侧。
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    rounded / scale
}

/// Python `statistics.median` 后再 `int()` 截断。
fn median_int(values: &mut [i64]) -> i64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        // 偶数个取中间两个的平均，再向零截断。
        ((values[middle - 1] as f64 + values[middle] as f64) / 2.0) as i64
    }
}

fn nearest_rank(values: &mut [i64], percentile: usize) -> i64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = (percentile.clamp(1, 100) * values.len()).div_ceil(100);
    values[rank.saturating_sub(1)]
}

fn distribution(values: &[i64]) -> Value {
    let mut p50 = values.to_vec();
    let mut p90 = values.to_vec();
    let mut p95 = values.to_vec();
    let mut p99 = values.to_vec();
    json!({
        "groups": values.len(),
        "p50": nearest_rank(&mut p50, 50),
        "p90": nearest_rank(&mut p90, 90),
        "p95": nearest_rank(&mut p95, 95),
        "p99": nearest_rank(&mut p99, 99),
        "max": values.iter().copied().max().unwrap_or(0),
    })
}

/// 区服行为基线。只返回群体分布，不返回账号、角色、MAC 或 IP 原值。
pub fn behavior_profile(db: &mut GameDatabase) -> Result<Value> {
    let since = now_stamp_minus(ChronoDuration::days(30));
    let gameplay_caps = gameplay_caps_from_env()?;
    let reward_action_sql_list = reward_action_sql_list(&gameplay_caps);
    let coverage = db
        .fetch_one(
            "select count(*) characters,count(distinct account) accounts,
           sum(case when length(last_login_mac)>=8 then 1 else 0 end) device_covered,
           sum(case when length(last_login_ip)>=7 then 1 else 0 end) ip_covered
         from dl_mdb_1.char_info",
            &[],
        )?
        .unwrap_or_default();

    let device_rows = db.fetch_all(
        "select count(distinct account) accounts from dl_mdb_1.char_info
         where length(last_login_mac)>=8 group by last_login_mac",
        &[],
    )?;
    let ip_rows = db.fetch_all(
        "select count(distinct account) accounts from dl_mdb_1.char_info
         where length(last_login_ip)>=7 group by last_login_ip",
        &[],
    )?;
    let funnel_rows = db.fetch_all(
        "select count(distinct gid_from) sources,
           sum(case when item_iid<>'' then 1 else 0 end) asset_rows,
           count(distinct transfer_id) transfers
         from dl_ldb_1.item_transfer_log
         where update_time>=? and gid_from not in ('','(undefined)')
           and gid_to not in ('','(undefined)') and gid_from<>gid_to
         group by gid_to",
        &[Param::Str(since.clone())],
    )?;
    let action_rows = db.fetch_all(
        "select action,count(*) events,count(distinct transfer_id) transfers
         from dl_ldb_1.item_transfer_log where update_time>=?
         group by action order by events desc",
        &[Param::Str(since.clone())],
    )?;
    // ponytail: 画像命令按需把 30 天道具转移读入内存；高流水服超过百万行时升级为分日预聚合表。
    let graph_rows = db.fetch_all(
        "select update_time,item_iid,gid_from,gid_to from dl_ldb_1.item_transfer_log
         where update_time>=? and item_iid<>''",
        &[Param::Str(since.clone())],
    )?;
    let graph_profile = asset_graph_profile(&graph_rows);
    let rhythm_rows = db.fetch_all(
        "select gid actor,update_time,concat('campaign:',action) behavior
           from dl_ldb_1.campaign_log where update_time>=? and gid<>''
         union all select gid,update_time,concat('errand:',action)
           from dl_ldb_1.errand_log where update_time>=? and gid<>''
         union all select para1,update_time,concat('user:',action)
           from dl_ldb_1.user_log where update_time>=? and para1 not in ('','(undefined)')
         union all select gid_from,update_time,concat('transfer:',action)
           from dl_ldb_1.item_transfer_log where update_time>=? and gid_from not in ('','(undefined)')",
        &[
            Param::Str(since.clone()),
            Param::Str(since.clone()),
            Param::Str(since.clone()),
            Param::Str(since.clone()),
        ],
    )?;
    let rhythm_profile = behavior_rhythm_profile(&rhythm_rows);
    let reward_flow_rows = db.fetch_all(
        &format!(
            "select gid actor,update_time,action,'' target,'reward' kind
               from dl_ldb_1.campaign_log where update_time>=? and gid<>''
                 and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
             union all select gid,update_time,action,'','reward'
               from dl_ldb_1.errand_log where update_time>=? and gid<>''
                 and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
             union all select gid_from,update_time,action,gid_to,'transfer'
               from dl_ldb_1.item_transfer_log where update_time>=?
                 and gid_from not in ('','(undefined)') and gid_to not in ('','(undefined)')
                 and gid_to<>gid_from and item_iid<>''"
        ),
        &[
            Param::Str(since.clone()),
            Param::Str(since.clone()),
            Param::Str(since),
        ],
    )?;
    let reward_flow_profile = reward_flow_profile(&reward_flow_rows);

    Ok(json!({
        "windowDays": 30,
        "coverage": {
            "characters": coverage.int("characters"),
            "accounts": coverage.int("accounts"),
            "deviceCharacters": coverage.int("device_covered"),
            "ipCharacters": coverage.int("ip_covered"),
        },
        "sharedDeviceAccounts": distribution(&device_rows.iter().map(|row| row.int("accounts")).collect::<Vec<_>>()),
        "sharedIpAccounts": distribution(&ip_rows.iter().map(|row| row.int("accounts")).collect::<Vec<_>>()),
        "inboundSourcePlayers": distribution(&funnel_rows.iter().map(|row| row.int("sources")).collect::<Vec<_>>()),
        "inboundAssetRows": distribution(&funnel_rows.iter().map(|row| row.int("asset_rows")).collect::<Vec<_>>()),
        "inboundTransfers": distribution(&funnel_rows.iter().map(|row| row.int("transfers")).collect::<Vec<_>>()),
        "burst10mSourcePlayers": graph_profile["burst10mSourcePlayers"],
        "burst10mAssetRows": graph_profile["burst10mAssetRows"],
        "returnedAssetIds": graph_profile["returnedAssetIds"],
        "returnedAssetPeers": graph_profile["returnedAssetPeers"],
        "maxDailyActiveSpanMinutes": rhythm_profile["maxDailyActiveSpanMinutes"],
        "maxDailyActiveEvents": rhythm_profile["maxDailyActiveEvents"],
        "mechanicalIntervalRatioPermille": rhythm_profile["mechanicalIntervalRatioPermille"],
        "mechanicalActionEvents": rhythm_profile["mechanicalActionEvents"],
        "longActivePlayers": rhythm_profile["longActivePlayers"],
        "mechanicalPlayers": rhythm_profile["mechanicalPlayers"],
        "rewardBurst10m": reward_flow_profile["rewardBurst10m"],
        "rapidRewardOutflows": reward_flow_profile["rapidRewardOutflows"],
        "rapidRewardOutflowDays": reward_flow_profile["rapidRewardOutflowDays"],
        "rewardOutflowTargetPeers": reward_flow_profile["rewardOutflowTargetPeers"],
        "rewardBurstPlayers": reward_flow_profile["rewardBurstPlayers"],
        "rapidRewardOutflowPlayers": reward_flow_profile["rapidRewardOutflowPlayers"],
        "transferActions": action_rows.iter().map(|row| json!({
            "action": row.text("action"), "events": row.int("events"), "transfers": row.int("transfers")
        })).collect::<Vec<_>>(),
    }))
}

fn gameplay_catalog_label(action: &str, bonus_name: &str, bonus_prop: &str) -> String {
    if action == "huilcbjl" {
        return "回合奖励".to_string();
    }
    let sample = if !bonus_name.trim().is_empty() {
        bonus_name.trim()
    } else {
        bonus_prop.trim()
    };
    let descriptive = sample.chars().any(|character| character.is_alphabetic());
    if sample.is_empty() || !descriptive {
        format!("奖励行为 {action}")
    } else {
        format!("{}奖励", sample.chars().take(32).collect::<String>())
    }
}

/// 最近 30 天奖励 action 目录。只返回聚合值和奖励样例，不暴露玩家身份。
pub fn gameplay_catalog_result(db: &mut GameDatabase) -> Result<Value> {
    let since = now_stamp_minus(ChronoDuration::days(30));
    let params = || [Param::Str(since.clone()), Param::Str(since.clone())];
    let actions = db.fetch_all(
        "select action,sum(events) events,count(distinct gid) players,max(last_seen) last_seen,
           max(sample_name) sample_name,max(sample_prop) sample_prop
         from (
           select action,gid,count(*) events,max(update_time) last_seen,
             max(bonus_name) sample_name,max(bonus_prop) sample_prop
           from dl_ldb_1.campaign_log
           where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
           group by action,gid
           union all
           select action,gid,count(*) events,max(update_time) last_seen,
             max(bonus_name) sample_name,max(bonus_prop) sample_prop
           from dl_ldb_1.errand_log
           where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
           group by action,gid
         ) discovered
         group by action order by events desc limit 100",
        &params(),
    )?;
    let daily_rows = db.fetch_all(
        "select action,max(day_events) daily_peak from (
           select action,gid,day,sum(events) day_events from (
             select action,gid,left(update_time,8) day,count(*) events
             from dl_ldb_1.campaign_log
             where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
             group by action,gid,left(update_time,8)
             union all
             select action,gid,left(update_time,8) day,count(*) events
             from dl_ldb_1.errand_log
             where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
             group by action,gid,left(update_time,8)
           ) source_days group by action,gid,day
         ) player_days group by action",
        &params(),
    )?;
    // ponytail: 目录页用固定 10 分钟桶做低成本建议；规则判定仍使用精确滑窗。
    // 超大服需要更精确建议时，升级为按日预聚合后离线计算滑窗分位数。
    let burst_rows = db.fetch_all(
        "select action,max(bucket_events) burst_peak from (
           select action,gid,bucket,sum(events) bucket_events from (
             select action,gid,left(update_time,11) bucket,count(*) events
             from dl_ldb_1.campaign_log
             where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
             group by action,gid,left(update_time,11)
             union all
             select action,gid,left(update_time,11) bucket,count(*) events
             from dl_ldb_1.errand_log
             where update_time>=? and action<>'' and bonus_type in (1,2,3,7,14)
             group by action,gid,left(update_time,11)
           ) source_buckets group by action,gid,bucket
         ) player_buckets group by action",
        &params(),
    )?;
    let daily: HashMap<String, i64> = daily_rows
        .iter()
        .map(|row| (row.text("action"), row.int("daily_peak")))
        .collect();
    let burst: HashMap<String, i64> = burst_rows
        .iter()
        .map(|row| (row.text("action"), row.int("burst_peak")))
        .collect();
    let discovered = actions
        .iter()
        .filter(|row| !is_confirmed_cost(&row.text("action")))
        .map(|row| {
            let action = row.text("action");
            let daily_peak = daily.get(&action).copied().unwrap_or(0);
            let burst_peak = burst.get(&action).copied().unwrap_or(0);
            let sample_name = row.text("sample_name");
            let sample_prop = row.text("sample_prop");
            json!({
                "action": action,
                "label": gameplay_catalog_label(&action, &sample_name, &sample_prop),
                "confirmedGain": is_confirmed_gain(&action),
                "events": row.int("events"),
                "players": row.int("players"),
                "lastSeen": row.text("last_seen"),
                "sampleReward": if sample_prop.is_empty() { sample_name } else { sample_prop },
                "dailyPeak": daily_peak,
                "burst10mBucketPeak": burst_peak,
                "suggestedDailyLimit": suggested_gameplay_limit(daily_peak),
                "suggestedBurst10mLimit": suggested_gameplay_limit(burst_peak),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "connected": true, "windowDays": 30, "actions": discovered }))
}

fn transfer_row_from(row: &Row) -> TransferRow {
    TransferRow {
        action: row.text("action"),
        item_amount: row.int("item_amount"),
        item_name: row.text("item_name"),
        item_iid: row.text("item_iid"),
        gid_from: row.text("gid_from"),
        gid_to: row.text("gid_to"),
    }
}

// ---------------------------------------------------------------------------
// 玩家
// ---------------------------------------------------------------------------

const PLAYER_COLUMNS: &str = "update_time,dist,gid,name,account,create_time,last_login_time,\
     last_login_ip,last_login_mac,level,cash,balance,char_status";

/// 按角色 ID / 账号 / 角色名定位玩家；不传查询条件时取最近登录的一个。
pub fn find_player(db: &mut GameDatabase, query: Option<&str>) -> Result<Row> {
    let row = match query.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => {
            let query: String = raw.chars().take(128).collect();
            // 角色 ID 与账号是 ASCII；角色名可能是中文，交给 GBK 编码参数匹配。
            let ascii_query = if query.is_ascii() {
                query.clone()
            } else {
                String::new()
            };
            db.fetch_one(
                &format!(
                    "select {PLAYER_COLUMNS} from dl_mdb_1.char_info
                     where gid=? or account=? or name=? limit 1"
                ),
                &[
                    Param::Str(ascii_query.clone()),
                    Param::Str(ascii_query),
                    Param::Str(query),
                ],
            )?
        }
        None => db.fetch_one(
            &format!(
                "select {PLAYER_COLUMNS} from dl_mdb_1.char_info
                 order by last_login_time desc limit 1"
            ),
            &[],
        )?,
    };
    row.ok_or_else(|| LookupError("未找到匹配玩家".to_string()).into())
}

/// 找出在已接入来源日志中找不到对应记录的金元宝快照跳增。
///
/// 只复核金额最大的 8 次跳增（与 Python 版一致）；繁忙服若超出这个复核窗口，
/// 应改为一次聚合账本查询，而不是放大这个上限。
fn gold_jump_candidates(rows: &[Row]) -> Vec<GoldJump> {
    let snapshots: Vec<CoinSnapshot> = rows
        .iter()
        .rev()
        .map(|row| CoinSnapshot {
            update_time: row.text("update_time"),
            gold_coin: row.int("gold_coin"),
        })
        .collect();
    let mut candidates = gold_snapshot_jumps(&snapshots, DEFAULT_JUMP_MINIMUM);
    candidates.sort_by_key(|jump| std::cmp::Reverse(jump.amount));
    candidates.truncate(8);
    candidates
}

pub fn unexplained_gold_jumps(
    db: &mut GameDatabase,
    gid: &str,
    reward_action_sql_list: &str,
) -> Result<Vec<GoldJump>> {
    let rows = db.fetch_all(
        "select update_time,gold_coin from dl_ldb_1.login_log
         where gid=? order by update_time desc, id desc limit 500",
        &[Param::Str(gid.to_string())],
    )?;
    let candidates = gold_jump_candidates(&rows);

    let evidence_sql = format!(
        "select
           (select count(*) from dl_ldb_1.campaign_log where gid=? and update_time>? and update_time<=? and bonus_type=7 and action in ({reward_action_sql_list})) +
           (select count(*) from dl_ldb_1.errand_log where gid=? and update_time>? and update_time<=? and bonus_type=7 and action in ({reward_action_sql_list})) +
           (select count(*) from dl_ldb_1.coin_order_log where gid=? and update_time>? and update_time<=?) +
           (select count(*) from dl_ldb_1.gbuy_action_log where gid=? and update_time>? and update_time<=?) +
           (select count(*) from dl_ldb_1.gift_log where gid=? and update_time>? and update_time<=?) +
           (select count(*) from dl_ldb_1.important_action_log where gid=? and update_time>? and update_time<=?) count"
    );

    let mut unexplained = Vec::new();
    for jump in candidates {
        let mut params = Vec::with_capacity(18);
        for _ in 0..6 {
            params.push(Param::Str(gid.to_string()));
            params.push(Param::Str(jump.from_time.clone()));
            params.push(Param::Str(jump.to_time.clone()));
        }
        if db.fetch_count(&evidence_sql, &params, "count")? == 0 {
            unexplained.push(jump);
        }
    }
    unexplained.sort_by(|left, right| right.to_time.cmp(&left.to_time));
    Ok(unexplained)
}

// ponytail: 归集强度按道具流水行数计数，不猜测不同道具价值；接入道具估值表后再升级为价值加权。
fn record_asset_flow(
    flow_by_peer: &mut HashMap<String, (i64, i64)>,
    gid: &str,
    gid_from: &str,
    gid_to: &str,
    has_asset: bool,
) {
    if !has_asset {
        return;
    }
    if gid_to == gid && !gid_from.is_empty() && gid_from != "(undefined)" && gid_from != gid {
        flow_by_peer.entry(gid_from.to_string()).or_default().0 += 1;
    } else if gid_from == gid && !gid_to.is_empty() && gid_to != "(undefined)" && gid_to != gid {
        flow_by_peer.entry(gid_to.to_string()).or_default().1 += 1;
    }
}

fn asset_funnel_counts(flow_by_peer: &HashMap<String, (i64, i64)>) -> (i64, i64) {
    flow_by_peer
        .values()
        .filter(|(incoming, outgoing)| *incoming > 0 && *outgoing == 0)
        .fold((0, 0), |(peers, rows), (incoming, _)| {
            (peers + 1, rows + incoming)
        })
}

#[derive(Debug, Clone)]
struct InboundAssetEvent {
    at: NaiveDateTime,
    peer: String,
}

type AssetDirections = HashMap<(String, String), (bool, bool)>;

fn valid_actor(value: &str) -> bool {
    !value.is_empty() && value != "(undefined)"
}

fn record_inbound_asset_event(
    events: &mut Vec<InboundAssetEvent>,
    gid: &str,
    gid_from: &str,
    gid_to: &str,
    item_iid: &str,
    update_time: &str,
) {
    if risk_core::normalized_iid(item_iid).is_empty()
        || gid_to != gid
        || !valid_actor(gid_from)
        || gid_from == gid
    {
        return;
    }
    if let Ok(at) = NaiveDateTime::parse_from_str(update_time, TIMESTAMP_FORMAT) {
        events.push(InboundAssetEvent {
            at,
            peer: gid_from.to_string(),
        });
    }
}

fn burst_funnel_counts(events: &mut [InboundAssetEvent]) -> (i64, i64) {
    events.sort_unstable_by_key(|event| event.at);
    let mut left = 0usize;
    let mut peers: HashMap<String, usize> = HashMap::new();
    let mut best = (0i64, 0i64);

    for right in 0..events.len() {
        *peers.entry(events[right].peer.clone()).or_default() += 1;
        while events[right].at.signed_duration_since(events[left].at) > ChronoDuration::minutes(10)
        {
            let peer = &events[left].peer;
            if let Some(count) = peers.get_mut(peer) {
                *count -= 1;
                if *count == 0 {
                    peers.remove(peer);
                }
            }
            left += 1;
        }
        let candidate = (peers.len() as i64, (right - left + 1) as i64);
        if candidate.0 > best.0 || (candidate.0 == best.0 && candidate.1 > best.1) {
            best = candidate;
        }
    }
    best
}

fn record_asset_direction(
    directions: &mut AssetDirections,
    gid: &str,
    gid_from: &str,
    gid_to: &str,
    item_iid: &str,
) {
    let iid = risk_core::normalized_iid(item_iid);
    if iid.is_empty() {
        return;
    }
    if gid_to == gid && valid_actor(gid_from) && gid_from != gid {
        directions.entry((gid_from.to_string(), iid)).or_default().0 = true;
    } else if gid_from == gid && valid_actor(gid_to) && gid_to != gid {
        directions.entry((gid_to.to_string(), iid)).or_default().1 = true;
    }
}

fn asset_roundtrip_counts(directions: &AssetDirections) -> (i64, i64) {
    let mut iids = HashSet::new();
    let mut peers = HashSet::new();
    for ((peer, iid), (incoming, outgoing)) in directions {
        if *incoming && *outgoing {
            iids.insert(iid);
            peers.insert(peer);
        }
    }
    (iids.len() as i64, peers.len() as i64)
}

fn asset_graph_profile(rows: &[Row]) -> Value {
    let mut incoming_by_target: HashMap<String, Vec<InboundAssetEvent>> = HashMap::new();
    let mut directions_by_actor: HashMap<String, AssetDirections> = HashMap::new();

    for row in rows {
        let gid_from = row.text("gid_from");
        let gid_to = row.text("gid_to");
        let item_iid = row.text("item_iid");
        if !valid_actor(&gid_from)
            || !valid_actor(&gid_to)
            || gid_from == gid_to
            || risk_core::normalized_iid(&item_iid).is_empty()
        {
            continue;
        }
        record_inbound_asset_event(
            incoming_by_target.entry(gid_to.clone()).or_default(),
            &gid_to,
            &gid_from,
            &gid_to,
            &item_iid,
            &row.text("update_time"),
        );
        record_asset_direction(
            directions_by_actor.entry(gid_from.clone()).or_default(),
            &gid_from,
            &gid_from,
            &gid_to,
            &item_iid,
        );
        record_asset_direction(
            directions_by_actor.entry(gid_to.clone()).or_default(),
            &gid_to,
            &gid_from,
            &gid_to,
            &item_iid,
        );
    }

    let burst_counts: Vec<(i64, i64)> = incoming_by_target
        .values_mut()
        .map(|events| burst_funnel_counts(events))
        .collect();
    let roundtrip_counts: Vec<(i64, i64)> = directions_by_actor
        .values()
        .map(asset_roundtrip_counts)
        .filter(|(iids, _)| *iids > 0)
        .collect();
    json!({
        "burst10mSourcePlayers": distribution(&burst_counts.iter().map(|value| value.0).collect::<Vec<_>>()),
        "burst10mAssetRows": distribution(&burst_counts.iter().map(|value| value.1).collect::<Vec<_>>()),
        "returnedAssetIds": distribution(&roundtrip_counts.iter().map(|value| value.0).collect::<Vec<_>>()),
        "returnedAssetPeers": distribution(&roundtrip_counts.iter().map(|value| value.1).collect::<Vec<_>>()),
    })
}

#[derive(Debug, Clone)]
struct RhythmEvent {
    at: NaiveDateTime,
    behavior: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RhythmFacts {
    max_daily_span_minutes: i64,
    max_daily_events: i64,
    long_active_days: i64,
    mechanical_action: String,
    mechanical_events: i64,
    mechanical_interval_seconds: i64,
    mechanical_ratio_permille: i64,
    mechanical_span_minutes: i64,
}

fn analyze_rhythm_events(events: &[RhythmEvent]) -> RhythmFacts {
    let mut result = RhythmFacts::default();
    let mut unique = HashSet::new();
    let mut by_day = HashMap::new();
    let mut by_behavior = HashMap::new();

    for event in events {
        if !unique.insert((event.behavior.clone(), event.at)) {
            continue;
        }
        by_day
            .entry(event.at.date())
            .or_insert_with(Vec::new)
            .push(event.at);
        by_behavior
            .entry(event.behavior.clone())
            .or_insert_with(Vec::new)
            .push(event.at);
    }

    for times in by_day.values_mut() {
        times.sort_unstable();
        let span = times
            .last()
            .zip(times.first())
            .map(|(last, first)| last.signed_duration_since(*first).num_minutes())
            .unwrap_or(0);
        let count = times.len() as i64;
        result.max_daily_span_minutes = result.max_daily_span_minutes.max(span);
        result.max_daily_events = result.max_daily_events.max(count);
        if span >= 18 * 60 && count >= 100 {
            result.long_active_days += 1;
        }
    }

    for (behavior, times) in &mut by_behavior {
        times.sort_unstable();
        times.dedup();
        if times.len() < 20 {
            continue;
        }
        let deltas: Vec<i64> = times
            .windows(2)
            .map(|pair| pair[1].signed_duration_since(pair[0]).num_seconds())
            .filter(|delta| *delta > 0)
            .collect();
        if deltas.is_empty() {
            continue;
        }
        let mut interval_counts: HashMap<i64, i64> = HashMap::new();
        for delta in &deltas {
            if (1..=300).contains(delta) {
                *interval_counts.entry(*delta).or_default() += 1;
            }
        }
        let Some((interval, repeats)) = interval_counts
            .into_iter()
            .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        else {
            continue;
        };
        let ratio = repeats * 1000 / deltas.len() as i64;
        let span = times
            .last()
            .unwrap()
            .signed_duration_since(times[0])
            .num_minutes();
        let candidate = (ratio, times.len() as i64, span, interval, behavior);
        let current = (
            result.mechanical_ratio_permille,
            result.mechanical_events,
            result.mechanical_span_minutes,
            result.mechanical_interval_seconds,
            &result.mechanical_action,
        );
        if candidate > current {
            result.mechanical_ratio_permille = ratio;
            result.mechanical_events = times.len() as i64;
            result.mechanical_span_minutes = span;
            result.mechanical_interval_seconds = interval;
            result.mechanical_action = behavior.clone();
        }
    }
    result
}

fn behavior_rhythm(rows: &[Row]) -> RhythmFacts {
    let events = rows
        .iter()
        .filter_map(|row| {
            NaiveDateTime::parse_from_str(&row.text("update_time"), TIMESTAMP_FORMAT)
                .ok()
                .map(|at| RhythmEvent {
                    at,
                    behavior: row.text("behavior"),
                })
        })
        .collect::<Vec<_>>();
    analyze_rhythm_events(&events)
}

fn behavior_rhythm_profile(rows: &[Row]) -> Value {
    let mut by_actor: HashMap<String, Vec<RhythmEvent>> = HashMap::new();
    for row in rows {
        let actor = row.text("actor");
        let Ok(at) = NaiveDateTime::parse_from_str(&row.text("update_time"), TIMESTAMP_FORMAT)
        else {
            continue;
        };
        if valid_actor(&actor) {
            by_actor.entry(actor).or_default().push(RhythmEvent {
                at,
                behavior: row.text("behavior"),
            });
        }
    }
    let facts: Vec<RhythmFacts> = by_actor
        .values()
        .map(|events| analyze_rhythm_events(events))
        .collect();
    json!({
        "maxDailyActiveSpanMinutes": distribution(&facts.iter().map(|value| value.max_daily_span_minutes).collect::<Vec<_>>()),
        "maxDailyActiveEvents": distribution(&facts.iter().map(|value| value.max_daily_events).collect::<Vec<_>>()),
        "mechanicalIntervalRatioPermille": distribution(&facts.iter().map(|value| value.mechanical_ratio_permille).collect::<Vec<_>>()),
        "mechanicalActionEvents": distribution(&facts.iter().map(|value| value.mechanical_events).collect::<Vec<_>>()),
        "longActivePlayers": facts.iter().filter(|value| value.long_active_days >= 2).count(),
        "mechanicalPlayers": facts.iter().filter(|value| {
            value.mechanical_events >= 20
                && value.mechanical_interval_seconds >= 1
                && value.mechanical_interval_seconds <= 300
                && value.mechanical_ratio_permille >= 800
                && value.mechanical_span_minutes >= 30
        }).count(),
    })
}

#[derive(Debug, Clone)]
struct RewardEvent {
    at: NaiveDateTime,
    action: String,
}

#[derive(Debug, Clone)]
struct AssetOutflowEvent {
    at: NaiveDateTime,
    target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RewardFlowFacts {
    burst_action: String,
    burst_events: i64,
    rapid_outflows: i64,
    rapid_outflow_days: i64,
    target_peers: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GameplayCapFacts {
    action: String,
    daily_events: i64,
    daily_limit: i64,
    burst_events: i64,
    burst_limit: i64,
}

fn analyze_gameplay_caps(reward_events: &[RewardEvent], caps: &[GameplayCap]) -> GameplayCapFacts {
    let mut unique = HashSet::new();
    let mut by_action: HashMap<&str, Vec<NaiveDateTime>> = HashMap::new();
    for reward in reward_events {
        if unique.insert((reward.action.as_str(), reward.at)) {
            by_action.entry(&reward.action).or_default().push(reward.at);
        }
    }

    let mut best: Option<((i64, i64, String), GameplayCapFacts)> = None;
    for cap in caps.iter().filter(|cap| cap.enabled) {
        let mut times = by_action
            .get(cap.action.as_str())
            .cloned()
            .unwrap_or_default();
        times.sort_unstable();
        let mut daily: HashMap<_, i64> = HashMap::new();
        for at in &times {
            *daily.entry(at.date()).or_default() += 1;
        }
        let daily_events = daily.values().copied().max().unwrap_or(0);
        let mut burst_events = 0;
        let mut left = 0usize;
        for right in 0..times.len() {
            while times[right].signed_duration_since(times[left]) > ChronoDuration::minutes(10) {
                left += 1;
            }
            burst_events = burst_events.max((right - left + 1) as i64);
        }
        let daily_ratio = if cap.daily_limit > 0 {
            daily_events.saturating_mul(1000) / cap.daily_limit
        } else {
            0
        };
        let burst_ratio = if cap.burst10m_limit > 0 {
            burst_events.saturating_mul(1000) / cap.burst10m_limit
        } else {
            0
        };
        let facts = GameplayCapFacts {
            action: cap.action.clone(),
            daily_events,
            daily_limit: cap.daily_limit,
            burst_events,
            burst_limit: cap.burst10m_limit,
        };
        let rank = (
            daily_ratio.max(burst_ratio),
            daily_events.saturating_add(burst_events),
            cap.action.clone(),
        );
        if best.as_ref().is_none_or(|(current, _)| rank > *current) {
            best = Some((rank, facts));
        }
    }
    best.map(|(_, facts)| facts).unwrap_or_default()
}

fn analyze_reward_flow(
    reward_events: &[RewardEvent],
    outflow_events: &[AssetOutflowEvent],
) -> RewardFlowFacts {
    let mut result = RewardFlowFacts::default();
    let mut unique_rewards = HashSet::new();
    let mut rewards_by_action: HashMap<String, Vec<NaiveDateTime>> = HashMap::new();
    for reward in reward_events {
        if unique_rewards.insert((reward.action.clone(), reward.at)) {
            rewards_by_action
                .entry(reward.action.clone())
                .or_default()
                .push(reward.at);
        }
    }

    for (action, times) in &mut rewards_by_action {
        times.sort_unstable();
        let mut left = 0usize;
        for right in 0..times.len() {
            while times[right].signed_duration_since(times[left]) > ChronoDuration::minutes(10) {
                left += 1;
            }
            let count = (right - left + 1) as i64;
            if (count, action) > (result.burst_events, &result.burst_action) {
                result.burst_events = count;
                result.burst_action = action.clone();
            }
        }
    }

    let mut reward_times: Vec<NaiveDateTime> =
        unique_rewards.into_iter().map(|(_, at)| at).collect();
    reward_times.sort_unstable();
    let mut outflows = outflow_events.to_vec();
    outflows.sort_unstable_by_key(|event| event.at);
    let mut waiting = VecDeque::new();
    let mut reward_index = 0usize;
    let mut days = HashSet::new();
    let mut targets = HashSet::new();
    for outflow in outflows {
        while reward_index < reward_times.len() && reward_times[reward_index] <= outflow.at {
            waiting.push_back(reward_times[reward_index]);
            reward_index += 1;
        }
        while waiting.front().is_some_and(|reward| {
            outflow.at.signed_duration_since(*reward) > ChronoDuration::minutes(10)
        }) {
            waiting.pop_front();
        }
        if let Some(reward) = waiting.pop_front() {
            result.rapid_outflows += 1;
            days.insert(reward.date());
            targets.insert(outflow.target);
        }
    }
    result.rapid_outflow_days = days.len() as i64;
    result.target_peers = targets.len() as i64;
    result
}

fn reward_flow(rows: &[Row]) -> RewardFlowFacts {
    let mut rewards = Vec::new();
    let mut outflows = Vec::new();
    for row in rows {
        let Ok(at) = NaiveDateTime::parse_from_str(&row.text("update_time"), TIMESTAMP_FORMAT)
        else {
            continue;
        };
        if row.text("kind") == "reward" {
            rewards.push(RewardEvent {
                at,
                action: row.text("action"),
            });
        } else {
            let target = row.text("target");
            if valid_actor(&target) {
                outflows.push(AssetOutflowEvent { at, target });
            }
        }
    }
    analyze_reward_flow(&rewards, &outflows)
}

fn gameplay_cap_facts(rows: &[Row], caps: &[GameplayCap]) -> GameplayCapFacts {
    let rewards = rows
        .iter()
        .filter(|row| row.text("kind") == "reward")
        .filter_map(|row| {
            NaiveDateTime::parse_from_str(&row.text("update_time"), TIMESTAMP_FORMAT)
                .ok()
                .map(|at| RewardEvent {
                    at,
                    action: row.text("action"),
                })
        })
        .collect::<Vec<_>>();
    analyze_gameplay_caps(&rewards, caps)
}

fn reward_flow_profile(rows: &[Row]) -> Value {
    let mut by_actor: HashMap<String, Vec<Row>> = HashMap::new();
    for row in rows {
        let actor = row.text("actor");
        if valid_actor(&actor) {
            by_actor.entry(actor).or_default().push(row.clone());
        }
    }
    let facts: Vec<RewardFlowFacts> = by_actor
        .values()
        .map(|actor_rows| reward_flow(actor_rows))
        .collect();
    json!({
        "rewardBurst10m": distribution(&facts.iter().map(|value| value.burst_events).collect::<Vec<_>>()),
        "rapidRewardOutflows": distribution(&facts.iter().map(|value| value.rapid_outflows).collect::<Vec<_>>()),
        "rapidRewardOutflowDays": distribution(&facts.iter().map(|value| value.rapid_outflow_days).collect::<Vec<_>>()),
        "rewardOutflowTargetPeers": distribution(&facts.iter().map(|value| value.target_peers).collect::<Vec<_>>()),
        "rewardBurstPlayers": facts.iter().filter(|value| value.burst_events >= 10).count(),
        "rapidRewardOutflowPlayers": facts.iter().filter(|value| {
            value.rapid_outflows >= 5
                && value.rapid_outflow_days >= 3
                && value.target_peers >= 1
                && value.target_peers <= 2
        }).count(),
    })
}

struct PlayerFactInputs<'a> {
    abnormal_coin: i64,
    coin_balance: Option<&'a Row>,
    transfer_rows: &'a [Row],
    rhythm_rows: &'a [Row],
    reward_flow_rows: &'a [Row],
    shared_device_accounts: i64,
    shared_ip_accounts: i64,
    item_count: i64,
    pet_count: i64,
    ground_handoffs: i64,
    gold_jumps: Vec<GoldJump>,
    reward_count: i64,
}

fn facts_from_inputs(
    player: &Row,
    median_gold_coin: i64,
    gameplay_caps: &[GameplayCap],
    inputs: PlayerFactInputs<'_>,
) -> Facts {
    let gid = player.text("gid");
    let (gold_coin, silver_coin, coin_observed_at) = match inputs.coin_balance {
        Some(row) => (
            row.int("gold_coin"),
            row.int("silver_coin"),
            row.text("update_time"),
        ),
        None => (0, 0, String::new()),
    };

    let mut bait_legs: HashMap<String, (bool, bool)> = HashMap::new();
    let mut transfer_ids: HashSet<String> = HashSet::new();
    let mut peers: HashSet<String> = HashSet::new();
    let mut same_device_peers: HashSet<String> = HashSet::new();
    let mut asset_flow_by_peer: HashMap<String, (i64, i64)> = HashMap::new();
    let mut inbound_asset_events = Vec::new();
    let mut asset_directions = AssetDirections::new();

    for row in inputs.transfer_rows {
        let transfer_id = row.text("transfer_id");
        if !transfer_id.is_empty() {
            transfer_ids.insert(transfer_id.clone());
        }
        if row.text("action") == "bait" && !transfer_id.is_empty() {
            let legs = bait_legs.entry(transfer_id).or_insert((false, false));
            if row.truthy("item_iid") {
                legs.0 = true;
            } else {
                legs.1 = true;
            }
        }
        let gid_from = row.text("gid_from");
        let peer = if gid_from == gid {
            row.text("gid_to")
        } else {
            gid_from.clone()
        };
        if !peer.is_empty() {
            let mac_from = row.text("mac_from");
            if !mac_from.is_empty() && mac_from == row.text("mac_to") {
                same_device_peers.insert(peer.clone());
            }
            peers.insert(peer);
        }

        let gid_to = row.text("gid_to");
        record_asset_flow(
            &mut asset_flow_by_peer,
            &gid,
            &gid_from,
            &gid_to,
            row.truthy("item_iid"),
        );
        record_inbound_asset_event(
            &mut inbound_asset_events,
            &gid,
            &gid_from,
            &gid_to,
            &row.text("item_iid"),
            &row.text("update_time"),
        );
        record_asset_direction(
            &mut asset_directions,
            &gid,
            &gid_from,
            &gid_to,
            &row.text("item_iid"),
        );
    }
    let unpaired_transfers = bait_legs
        .values()
        .filter(|(item, coin)| !(*item && *coin))
        .count() as i64;
    let (funnel_source_peers, funnel_asset_rows) = asset_funnel_counts(&asset_flow_by_peer);
    let (burst_funnel_source_peers, burst_funnel_asset_rows) =
        burst_funnel_counts(&mut inbound_asset_events);
    let (returned_asset_ids, returned_asset_peers) = asset_roundtrip_counts(&asset_directions);
    let rhythm = behavior_rhythm(inputs.rhythm_rows);
    let reward_flow = reward_flow(inputs.reward_flow_rows);
    let gameplay_cap = gameplay_cap_facts(inputs.reward_flow_rows, gameplay_caps);
    let unexplained_gold_increase = inputs.gold_jumps.iter().map(|jump| jump.amount).sum();

    Facts {
        cash: player.int("cash"),
        gold_coin,
        silver_coin,
        coin_observed_at,
        median_gold_coin,
        abnormal_coin: inputs.abnormal_coin,
        transfer_count: transfer_ids.len() as i64,
        unpaired_transfers,
        same_device_peers: same_device_peers.len() as i64,
        funnel_source_peers,
        funnel_asset_rows,
        burst_funnel_source_peers,
        burst_funnel_asset_rows,
        returned_asset_ids,
        returned_asset_peers,
        max_daily_active_span_minutes: rhythm.max_daily_span_minutes,
        max_daily_active_events: rhythm.max_daily_events,
        long_active_days: rhythm.long_active_days,
        mechanical_action: rhythm.mechanical_action,
        mechanical_action_events: rhythm.mechanical_events,
        mechanical_interval_seconds: rhythm.mechanical_interval_seconds,
        mechanical_interval_ratio_permille: rhythm.mechanical_ratio_permille,
        mechanical_span_minutes: rhythm.mechanical_span_minutes,
        reward_burst_action: reward_flow.burst_action,
        reward_burst_events: reward_flow.burst_events,
        rapid_reward_outflows: reward_flow.rapid_outflows,
        rapid_reward_outflow_days: reward_flow.rapid_outflow_days,
        reward_outflow_target_peers: reward_flow.target_peers,
        configured_cap_action: gameplay_cap.action,
        configured_cap_daily_events: gameplay_cap.daily_events,
        configured_cap_daily_limit: gameplay_cap.daily_limit,
        configured_cap_burst_events: gameplay_cap.burst_events,
        configured_cap_burst_limit: gameplay_cap.burst_limit,
        shared_device_accounts: inputs.shared_device_accounts,
        shared_ip_accounts: inputs.shared_ip_accounts,
        ground_handoffs: inputs.ground_handoffs,
        unexplained_gold_jumps: inputs.gold_jumps.len() as i64,
        unexplained_gold_increase,
        gold_jumps: inputs.gold_jumps,
        peers: peers.len() as i64,
        item_count: inputs.item_count,
        pet_count: inputs.pet_count,
        reward_count: inputs.reward_count,
    }
}

/// 汇总单个角色的风险证据。
pub fn player_facts(db: &mut GameDatabase, player: &Row, median_gold_coin: i64) -> Result<Facts> {
    let gid = player.text("gid");
    let account = player.text("account");
    let since = now_stamp_minus(ChronoDuration::days(30));
    let gameplay_caps = gameplay_caps_from_env()?;
    let reward_action_sql_list = reward_action_sql_list(&gameplay_caps);

    let abnormal_coin = db.fetch_count(
        "select count(*) count from dl_ldb_1.important_log
         where type='check_coin' and action='abnormal_coin_num' and para1=?",
        &[Param::Str(account.clone())],
        "count",
    )?;

    let coin_balance = db.fetch_one(
        "select gold_coin,silver_coin,update_time from dl_ldb_1.login_log
         where gid=? order by update_time desc limit 1",
        &[Param::Str(gid.clone())],
    )?;
    let transfer_rows = db.fetch_all(
        "select update_time,transfer_id,action,item_iid,gid_from,gid_to,mac_from,mac_to
         from dl_ldb_1.item_transfer_log
         where update_time >= ? and (gid_from=? or gid_to=?)",
        &[
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(gid.clone()),
        ],
    )?;

    let rhythm_rows = db.fetch_all(
        "select update_time,concat('campaign:',action) behavior
           from dl_ldb_1.campaign_log where update_time>=? and gid=?
         union all select update_time,concat('errand:',action)
           from dl_ldb_1.errand_log where update_time>=? and gid=?
         union all select update_time,concat('user:',action)
           from dl_ldb_1.user_log where update_time>=? and para1=?
         union all select update_time,concat('transfer:',action)
           from dl_ldb_1.item_transfer_log where update_time>=? and gid_from=?",
        &[
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
        ],
    )?;
    let reward_flow_rows = db.fetch_all(
        &format!(
            "select update_time,action,'' target,'reward' kind
               from dl_ldb_1.campaign_log where update_time>=? and gid=?
                 and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
             union all select update_time,action,'','reward'
               from dl_ldb_1.errand_log where update_time>=? and gid=?
                 and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
             union all select update_time,action,gid_to,'transfer'
               from dl_ldb_1.item_transfer_log where update_time>=? and gid_from=?
                 and gid_to not in ('','(undefined)') and gid_to<>gid_from and item_iid<>''"
        ),
        &[
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
        ],
    )?;
    let last_login_mac = player.text("last_login_mac");
    let shared_device_accounts = if last_login_mac.len() >= 8 {
        db.fetch_count(
            "select count(distinct account) count from dl_mdb_1.char_info
             where last_login_mac=? and last_login_mac<>''",
            &[Param::Str(last_login_mac)],
            "count",
        )?
    } else {
        0
    };
    let last_login_ip = player.text("last_login_ip");
    let shared_ip_accounts = if last_login_ip.len() >= 7 {
        db.fetch_count(
            "select count(distinct account) count from dl_mdb_1.char_info
             where last_login_ip=? and last_login_ip<>''",
            &[Param::Str(last_login_ip)],
            "count",
        )?
    } else {
        0
    };

    let item_count = db.fetch_count(
        "select coalesce(sum(amount),0) count from dl_mdb_1.item_info where owner=?",
        &[Param::Str(gid.clone())],
        "count",
    )?;
    let pet_count = db.fetch_count(
        "select count(*) count from dl_mdb_1.pet_info where owner=?",
        &[Param::Str(gid.clone())],
        "count",
    )?;

    // 角色 A 丢到地面、角色 B 拾取：绕过交易系统的资产转移。
    let ground_handoffs = db.fetch_count(
        "select count(distinct transfer_id) count
         from dl_ldb_1.item_transfer_log
         where action='diuqsq' and gid_from=?
           and gid_to not in ('','(undefined)') and gid_to<>gid_from",
        &[Param::Str(gid.clone())],
        "count",
    )?;

    let gold_jumps = unexplained_gold_jumps(db, &gid, &reward_action_sql_list)?;

    let reward_count = db.fetch_count(
        &format!(
            "select
               (select count(*) from dl_ldb_1.campaign_log where update_time >= ? and gid=? and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)) +
               (select count(*) from dl_ldb_1.errand_log where update_time >= ? and gid=? and action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)) +
               (select count(*) from dl_ldb_1.pet_log where update_time >= ? and gid=? and action='jianglcw') count"
        ),
        &[
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since.clone()),
            Param::Str(gid.clone()),
            Param::Str(since),
            Param::Str(gid),
        ],
        "count",
    )?;

    Ok(facts_from_inputs(
        player,
        median_gold_coin,
        &gameplay_caps,
        PlayerFactInputs {
            abnormal_coin,
            coin_balance: coin_balance.as_ref(),
            transfer_rows: &transfer_rows,
            rhythm_rows: &rhythm_rows,
            reward_flow_rows: &reward_flow_rows,
            shared_device_accounts,
            shared_ip_accounts,
            item_count,
            pet_count,
            ground_handoffs,
            gold_jumps,
            reward_count,
        },
    ))
}

type TimelineEntry = (String, String, String, String);

/// 角色的资产与交易时间线，取最近 12 条。
pub fn timeline(db: &mut GameDatabase, player: &Row, facts: &Facts) -> Result<Vec<[String; 4]>> {
    let gid = player.text("gid");
    let account = player.text("account");
    let mut events: Vec<TimelineEntry> = Vec::new();

    for jump in &facts.gold_jumps {
        events.push((
            jump.to_time.clone(),
            "金元宝快照跳增".to_string(),
            format!("+{} 金元宝", number(jump.amount)),
            format!("{} 后未找到已接入来源", stamp_label(&jump.from_time)),
        ));
    }

    let transfers = db.fetch_all(
        "select update_time,action,gid_from,gid_to,item_iid,item_name,item_amount,memo
         from dl_ldb_1.item_transfer_log
         where gid_from=? or gid_to=? order by update_time desc, id desc limit 40",
        &[Param::Str(gid.clone()), Param::Str(gid.clone())],
    )?;
    for row in &transfers {
        if let Some(event) = transfer_timeline_event(&transfer_row_from(row), &gid) {
            events.push((
                row.text("update_time"),
                event.action,
                event.change,
                event.note,
            ));
        }
    }

    let user_events = db.fetch_all(
        "select update_time,type,action,para1,para2,para3,memo
         from dl_ldb_1.user_log
         where (para1=? or (action='exchange' and para3=?))
           and action in ('buy','take_stall_cash','drop_pet')
         order by update_time desc, id desc limit 20",
        &[Param::Str(gid.clone()), Param::Str(gid.clone())],
    )?;
    for row in &user_events {
        let update_time = row.text("update_time");
        let memo = row.text("memo");
        match row.text("action").as_str() {
            "take_stall_cash" => events.push((
                update_time,
                "摆摊资金取回".to_string(),
                format!("+{} 金钱", number(row.int("para3"))),
                if memo.is_empty() {
                    "摆摊账户".to_string()
                } else {
                    memo
                },
            )),
            "drop_pet" => events.push((
                update_time,
                "丢弃宠物".to_string(),
                format!("-1 {}", row.text("para3")),
                format!("宠物 IID {}", row.text("para2")),
            )),
            _ => {
                let para2 = row.text("para2");
                let iid_note = if !para2.is_empty() && para2 != "U" {
                    format!("IID {para2}")
                } else {
                    "堆叠道具".to_string()
                };
                events.push((
                    update_time,
                    "NPC 商店购买".to_string(),
                    "+道具".to_string(),
                    format!("{} / {iid_note}", row.text("para3")),
                ));
            }
        }
    }

    let costs = db.fetch_all(
        "select update_time,item_name,amount,cost,cost_type from dl_ldb_1.cost_coin_log
         where account=? or gid=? order by update_time desc, id desc limit 20",
        &[Param::Str(account), Param::Str(gid.clone())],
    )?;
    for row in &costs {
        let cost_type = row.text("cost_type");
        let coin_name = coin_label(&cost_type)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if cost_type.is_empty() {
                    "货币".to_string()
                } else {
                    cost_type.clone()
                }
            });
        events.push((
            row.text("update_time"),
            "商城购买".to_string(),
            format!(
                "-{} {coin_name} / +{} {}",
                number(row.int("cost")),
                row.text("amount"),
                row.text("item_name")
            ),
            cost_type,
        ));
    }

    // action 1/31/32 是摆摊相关记账，已在别处覆盖，这里排除避免重复。
    let adjustments = db.fetch_all(
        "select update_time,type,action,cash,memo from dl_ldb_1.money_log
         where gid=? and action not in (1,31,32) order by update_time desc, id desc limit 20",
        &[Param::Str(gid.clone())],
    )?;
    for row in &adjustments {
        let action = row.int("action");
        // 交接报告 §3.3：14 是装备修理消耗，26 是装备养成消耗。
        let label = match action {
            14 => Some("装备修理消耗"),
            26 => Some("装备养成消耗"),
            _ => None,
        };
        let memo = row.text("memo");
        events.push((
            row.text("update_time"),
            label
                .map(str::to_string)
                .unwrap_or_else(|| format!("金钱事件 #{action}")),
            format!(
                "{}{} 金钱",
                if label.is_some() { "-" } else { "" },
                number(row.int("cash"))
            ),
            if memo.is_empty() {
                "服务端记账".to_string()
            } else {
                memo
            },
        ));
    }

    let rewards = db.fetch_all(
        // 同一秒内常有多条奖励，必须用主键 id 兜底成全序，
        // 否则 LIMIT 会在并列行里任意截断，证据链不可复现。
        "select update_time,action,bonus_type,bonus_name,bonus_prop,'campaign_log' source_table,id
         from dl_ldb_1.campaign_log where gid=? and bonus_type in (1,2,3,7,14)
         union all
         select update_time,action,bonus_type,bonus_name,bonus_prop,'errand_log' source_table,id
         from dl_ldb_1.errand_log where gid=? and bonus_type in (1,2,3,7,14)
         order by update_time desc, source_table asc, id desc limit 30",
        &[Param::Str(gid.clone()), Param::Str(gid.clone())],
    )?;
    for row in &rewards {
        let bonus_type = row.int("bonus_type");
        let bonus_prop = row.text("bonus_prop");
        // 经验/道行的 bonus_prop 记的是来源，优先展示它。
        let source = if (bonus_type == 2 || bonus_type == 3) && !bonus_prop.is_empty() {
            bonus_prop.clone()
        } else {
            row.text("source_table")
        };
        let action = row.text("action");
        let (title, prefix) = activity_direction(&action);
        events.push((
            row.text("update_time"),
            format!("{title} · {action}"),
            format!(
                "{prefix}{}",
                reward_change(&RewardRow {
                    bonus_type,
                    bonus_name: row.text("bonus_name"),
                    bonus_prop,
                })
            ),
            source,
        ));
    }

    let pet_rewards = db.fetch_all(
        "select update_time,pet_name,pet_iid from dl_ldb_1.pet_log
         where gid=? and action='jianglcw' order by update_time desc, id desc limit 20",
        &[Param::Str(gid)],
    )?;
    for row in &pet_rewards {
        events.push((
            row.text("update_time"),
            "奖励获得宠物".to_string(),
            format!("+宠物 {}", row.text("pet_name")),
            format!("pet_log / IID {}", row.text("pet_iid")),
        ));
    }

    // 稳定降序排序：时间相同的事件保持插入顺序，与 Python 的 sort(reverse=True) 一致。
    events.sort_by(|left, right| right.0.cmp(&left.0));
    events.truncate(12);

    if events.is_empty() {
        return Ok(vec![[
            "-".to_string(),
            "暂无资产事件".to_string(),
            "0".to_string(),
            "当前日志范围内无记录".to_string(),
        ]]);
    }
    Ok(events
        .into_iter()
        .map(|(stamp, action, change, note)| [stamp_label(&stamp), action, change, note])
        .collect())
}

/// 全服最近一次登录快照的金元宝中位数，用于「存量偏离」判定。
pub fn median_gold_coin(db: &mut GameDatabase) -> Result<i64> {
    let rows = db.fetch_all(
        "select l.gid,l.gold_coin from dl_ldb_1.login_log l
         inner join (
           select gid,max(update_time) update_time from dl_ldb_1.login_log
           where gid<>'' group by gid
         ) latest on latest.gid=l.gid and latest.update_time=l.update_time",
        &[],
    )?;
    let mut values: Vec<i64> = rows.iter().map(|row| row.int("gold_coin")).collect();
    Ok(median_int(&mut values))
}

fn latest_gold_coin_values<I>(snapshots: I) -> Vec<i64>
where
    I: IntoIterator<Item = (String, String, i64)>,
{
    let mut latest_by_gid: HashMap<String, (String, Vec<i64>)> = HashMap::new();
    for (gid, update_time, gold_coin) in snapshots {
        match latest_by_gid.entry(gid) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((update_time, vec![gold_coin]));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let (latest_time, values) = entry.get_mut();
                match update_time.cmp(latest_time) {
                    std::cmp::Ordering::Greater => {
                        *latest_time = update_time;
                        *values = vec![gold_coin];
                    }
                    std::cmp::Ordering::Equal => values.push(gold_coin),
                    std::cmp::Ordering::Less => {}
                }
            }
        }
    }
    latest_by_gid
        .into_values()
        .flat_map(|(_, values)| values)
        .collect()
}

fn bulk_player_facts(db: &mut GameDatabase, players: &[Row]) -> Result<Vec<Facts>> {
    let since = now_stamp_minus(ChronoDuration::days(30));
    let gameplay_caps = gameplay_caps_from_env()?;
    let reward_action_sql_list = reward_action_sql_list(&gameplay_caps);
    let player_ids = players
        .iter()
        .map(|player| player.text("gid"))
        .collect::<HashSet<_>>();

    // ponytail: keep all gids because the established median includes deleted characters. The
    // 500-row cap is enough for jump detection; rank=1 preserves every tied latest median row.
    // At roughly one million login rows, replace the old-MySQL fallback with compacted snapshots.
    let login_rows = db
        .fetch_all(
            "select gid,update_time,gold_coin,silver_coin,id from (
               select l.gid,l.update_time,l.gold_coin,l.silver_coin,l.id,
                 row_number() over(partition by l.gid order by l.update_time desc,l.id desc) row_num,
                 rank() over(partition by l.gid order by l.update_time desc) time_rank
               from dl_ldb_1.login_log l
               where l.gid<>''
             ) ranked where row_num<=500 or time_rank=1
             order by gid,update_time desc,id desc",
            &[],
        )
        .or_else(|_| {
            db.fetch_all(
                "select l.gid,l.update_time,l.gold_coin,l.silver_coin,l.id
                 from dl_ldb_1.login_log l
                 where l.gid<>'' order by l.gid,l.update_time desc,l.id desc",
                &[],
            )
        })?;
    let mut median_values = latest_gold_coin_values(login_rows.iter().map(|row| {
        (
            row.text("gid"),
            row.text("update_time"),
            row.int("gold_coin"),
        )
    }));
    let median = median_int(&mut median_values);
    let mut login_by_gid: HashMap<String, Vec<Row>> = HashMap::new();
    for row in login_rows {
        let gid = row.text("gid");
        if !player_ids.contains(&gid) {
            continue;
        }
        let rows = login_by_gid.entry(gid).or_default();
        if rows.len() < 500 {
            rows.push(row);
        }
    }

    let mut jump_candidates: HashMap<String, Vec<GoldJump>> = HashMap::new();
    let mut earliest_jump = None::<String>;
    for (gid, rows) in &login_by_gid {
        let candidates = gold_jump_candidates(rows);
        for jump in &candidates {
            if earliest_jump
                .as_ref()
                .is_none_or(|current| jump.from_time < *current)
            {
                earliest_jump = Some(jump.from_time.clone());
            }
        }
        jump_candidates.insert(gid.clone(), candidates);
    }
    let evidence_since = earliest_jump.unwrap_or_else(|| "99999999999999".to_string());
    let mut gold_evidence_by_gid: HashMap<String, Vec<String>> = HashMap::new();
    let mut transfers_by_gid: HashMap<String, Vec<Row>> = HashMap::new();

    let activity_rows = db.fetch_all(
        &format!(
            "select gid actor,update_time,concat('campaign:',action) behavior,action,'' target,
                case when action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
                  then 'reward' else '' end kind,
                '' transfer_id,'' item_iid,'' gid_from,'' gid_to,'' mac_from,'' mac_to,'activity' row_kind
               from dl_ldb_1.campaign_log where update_time>=? and gid<>''
             union all select gid,update_time,concat('errand:',action),action,'',
                case when action in ({reward_action_sql_list}) and bonus_type in (1,2,3,7,14)
                  then 'reward' else '' end,'','','','','','','activity'
               from dl_ldb_1.errand_log where update_time>=? and gid<>''
             union all select para1,update_time,concat('user:',action),action,'','',
                '','','','','','','activity'
               from dl_ldb_1.user_log where update_time>=? and para1 not in ('','(undefined)')
             union all select gid_from,update_time,concat('transfer:',action),action,gid_to,
                case when gid_to not in ('','(undefined)') and gid_to<>gid_from and item_iid<>''
                  then 'transfer' else '' end,
                transfer_id,item_iid,gid_from,gid_to,mac_from,mac_to,'transfer_row'
               from dl_ldb_1.item_transfer_log where update_time>=?
             union all select gid,update_time,'',action,'','pet_reward',
                '','','','','','','pet_reward'
               from dl_ldb_1.pet_log where update_time>=? and gid<>'' and action='jianglcw'
             union all select gid,update_time,'','','','',
                '','','','','','','gold_evidence'
               from dl_ldb_1.campaign_log where update_time>? and bonus_type=7
                 and action in ({reward_action_sql_list})
             union all select gid,update_time,'','','','',
                '','','','','','','gold_evidence'
               from dl_ldb_1.errand_log where update_time>? and bonus_type=7
                 and action in ({reward_action_sql_list})
             union all select gid,update_time,'','','','','','','','','','','gold_evidence'
               from dl_ldb_1.coin_order_log where update_time>?
             union all select gid,update_time,'','','','','','','','','','','gold_evidence'
               from dl_ldb_1.gbuy_action_log where update_time>?
             union all select gid,update_time,'','','','','','','','','','','gold_evidence'
               from dl_ldb_1.gift_log where update_time>?
             union all select gid,update_time,'','','','','','','','','','','gold_evidence'
               from dl_ldb_1.important_action_log where update_time>?"
        ),
        &(0..5)
            .map(|_| Param::Str(since.clone()))
            .chain((0..6).map(|_| Param::Str(evidence_since.clone())))
            .collect::<Vec<_>>(),
    )?;
    let mut rhythms_by_gid: HashMap<String, Vec<Row>> = HashMap::new();
    let mut rewards_by_gid: HashMap<String, Vec<Row>> = HashMap::new();
    let mut pet_reward_counts: HashMap<String, i64> = HashMap::new();
    for row in activity_rows {
        let actor = row.text("actor");
        match row.text("row_kind").as_str() {
            "gold_evidence" => {
                if player_ids.contains(&actor) {
                    gold_evidence_by_gid
                        .entry(actor)
                        .or_default()
                        .push(row.text("update_time"));
                }
                continue;
            }
            "transfer_row" => {
                let gid_from = row.text("gid_from");
                let gid_to = row.text("gid_to");
                if player_ids.contains(&gid_from) {
                    transfers_by_gid
                        .entry(gid_from.clone())
                        .or_default()
                        .push(row.clone());
                }
                if gid_to != gid_from && player_ids.contains(&gid_to) {
                    transfers_by_gid
                        .entry(gid_to)
                        .or_default()
                        .push(row.clone());
                }
            }
            _ => {}
        }
        if player_ids.contains(&actor) {
            if row.text("kind") == "pet_reward" {
                *pet_reward_counts.entry(actor).or_default() += 1;
                continue;
            }
            rhythms_by_gid
                .entry(actor.clone())
                .or_default()
                .push(row.clone());
            if !row.text("kind").is_empty() {
                rewards_by_gid.entry(actor).or_default().push(row);
            }
        }
    }

    let mut device_accounts: HashMap<String, HashSet<String>> = HashMap::new();
    let mut ip_accounts: HashMap<String, HashSet<String>> = HashMap::new();
    for player in players {
        let account = player.text("account");
        let mac = player.text("last_login_mac");
        if mac.len() >= 8 {
            device_accounts
                .entry(mac)
                .or_default()
                .insert(account.clone());
        }
        let ip = player.text("last_login_ip");
        if ip.len() >= 7 {
            ip_accounts.entry(ip).or_default().insert(account);
        }
    }

    let mut facts = Vec::with_capacity(players.len());
    for player in players {
        let gid = player.text("gid");
        let transfer_rows = transfers_by_gid.get(&gid).map(Vec::as_slice).unwrap_or(&[]);
        let rhythm_rows = rhythms_by_gid.get(&gid).map(Vec::as_slice).unwrap_or(&[]);
        let reward_flow_rows = rewards_by_gid.get(&gid).map(Vec::as_slice).unwrap_or(&[]);
        let evidence = gold_evidence_by_gid
            .get(&gid)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut gold_jumps = jump_candidates.remove(&gid).unwrap_or_default();
        gold_jumps.retain(|jump| {
            !evidence
                .iter()
                .any(|stamp| *stamp > jump.from_time && *stamp <= jump.to_time)
        });
        gold_jumps.sort_by(|left, right| right.to_time.cmp(&left.to_time));
        let reward_count = reward_flow_rows
            .iter()
            .filter(|row| row.text("kind") == "reward")
            .count() as i64
            + pet_reward_counts.get(&gid).copied().unwrap_or(0);
        facts.push(facts_from_inputs(
            player,
            median,
            &gameplay_caps,
            PlayerFactInputs {
                abnormal_coin: player.int("abnormal_coin"),
                coin_balance: login_by_gid.get(&gid).and_then(|rows| rows.first()),
                transfer_rows,
                rhythm_rows,
                reward_flow_rows,
                shared_device_accounts: device_accounts
                    .get(&player.text("last_login_mac"))
                    .map_or(0, |accounts| accounts.len() as i64),
                shared_ip_accounts: ip_accounts
                    .get(&player.text("last_login_ip"))
                    .map_or(0, |accounts| accounts.len() as i64),
                item_count: player.int("item_count"),
                pet_count: player.int("pet_count"),
                ground_handoffs: player.int("ground_handoffs"),
                gold_jumps,
                reward_count,
            },
        ));
    }
    Ok(facts)
}

/// 单个玩家的完整分析结果。
pub fn player_result(db: &mut GameDatabase, query: Option<&str>) -> Result<Value> {
    let player = find_player(db, query)?;
    let median = median_gold_coin(db)?;
    player_result_for(db, &player, median)
}

fn player_result_for(db: &mut GameDatabase, player: &Row, median: i64) -> Result<Value> {
    let facts = player_facts(db, player, median)?;
    let timeline_rows = timeline(db, player, &facts)?;
    Ok(player_result_from_facts(player, facts, timeline_rows))
}

fn player_result_from_facts(player: &Row, facts: Facts, timeline_rows: Vec<[String; 4]>) -> Value {
    let Assessment {
        score,
        tags,
        reasons,
    } = risk_score(&facts);
    let (status, tone) = status_for(score);
    let summary = if reasons.is_empty() {
        "当前权威日志中未发现可直接定性的异常，仍需结合玩法产出日志复核。".to_string()
    } else {
        format!("{}。", reasons.join("；"))
    };

    json!({
        "id": player.text("gid"),
        "name": player.text("name"),
        "account": player.text("account"),
        "server": player.text("dist"),
        "level": player.int("level"),
        "score": score,
        "status": status,
        "statusTone": tone,
        "tags": tags,
        "summary": summary,
        "metrics": [
            ["金元宝 / 银元宝", format!("{} / {}", number(facts.gold_coin), number(facts.silver_coin))],
            ["当前金钱", number(player.int("cash"))],
            ["持有道具 / 宠物", format!("{} / {}", number(facts.item_count), number(facts.pet_count))],
            ["30 天交易 / 短时扇入", format!("{} / {}", number(facts.transfer_count), number(facts.burst_funnel_source_peers))],
            ["单日跨度 / 事件", format!("{} 小时 {} 分 / {}", facts.max_daily_active_span_minutes / 60, facts.max_daily_active_span_minutes % 60, number(facts.max_daily_active_events))],
            ["动作周期 / 重复率", if facts.mechanical_action.is_empty() { "未形成 / 0%".to_string() } else { format!("{} 秒 / {}%", facts.mechanical_interval_seconds, facts.mechanical_interval_ratio_permille / 10) }],
            ["奖励爆发 / 快速归集", format!("{} / {}", number(facts.reward_burst_events), number(facts.rapid_reward_outflows))],
            ["玩法峰值 / 配置上限", if facts.configured_cap_action.is_empty() { "尚未配置".to_string() } else { format!("{} · 日 {}/{} · 10分 {}/{}", facts.configured_cap_action, facts.configured_cap_daily_events, facts.configured_cap_daily_limit, facts.configured_cap_burst_events, facts.configured_cap_burst_limit) }],
        ],
        "timeline": timeline_rows,
        "evidence": facts,
    })
}

/// 全部角色的分析结果。证据按表批量取回，告警/总览不生成详情页时间线。
pub fn all_player_results(db: &mut GameDatabase) -> Result<Vec<Value>> {
    let players = db.fetch_all(
        &format!(
            "select c.*,coalesce(items.item_count,0) item_count,
               coalesce(pets.pet_count,0) pet_count,
               coalesce(abnormal.abnormal_coin,0) abnormal_coin,
               coalesce(ground.ground_handoffs,0) ground_handoffs
             from (select {PLAYER_COLUMNS} from dl_mdb_1.char_info) c
             left join (
               select owner,coalesce(sum(amount),0) item_count
               from dl_mdb_1.item_info group by owner
             ) items on items.owner=c.gid
             left join (
               select owner,count(*) pet_count from dl_mdb_1.pet_info group by owner
             ) pets on pets.owner=c.gid
             left join (
               select para1 account,count(*) abnormal_coin from dl_ldb_1.important_log
               where type='check_coin' and action='abnormal_coin_num' group by para1
             ) abnormal on abnormal.account=c.account
             left join (
               select gid_from gid,count(distinct transfer_id) ground_handoffs
               from dl_ldb_1.item_transfer_log where action='diuqsq'
                 and gid_to not in ('','(undefined)') and gid_to<>gid_from group by gid_from
             ) ground on ground.gid=c.gid
             order by c.gid"
        ),
        &[],
    )?;
    let facts = bulk_player_facts(db, &players)?;
    Ok(players
        .iter()
        .zip(facts)
        .map(|(player, facts)| player_result_from_facts(player, facts, Vec::new()))
        .collect())
}

// ---------------------------------------------------------------------------
// 告警
// ---------------------------------------------------------------------------

/// 标签 -> 规则名。未映射的标签原样作为规则名。
fn rule_for_tag(tag: &str) -> Option<&'static str> {
    match tag {
        "交易账本缺腿" => Some("交易账本不守恒"),
        "币值校验异常" => Some("服务端币值校验异常"),
        "元宝存量偏离" => Some("元宝存量显著偏离"),
        "同设备交易" => Some("同设备角色互转"),
        "多账号资产归集" => Some("多账号资产归集"),
        "短时资产归集" => Some("短时多账号资产归集"),
        "资产循环回流" => Some("同一资产循环回流"),
        "超长持续活跃" => Some("超长持续活跃"),
        "机械周期行为" => Some("机械化周期行为"),
        "奖励爆发异常" => Some("短时奖励爆发异常"),
        "奖励快速归集" => Some("奖励后快速资产归集"),
        "玩法产出超限" => Some("玩法奖励超过配置上限"),
        "绕过交易转移" => Some("丢弃拾取绕过交易"),
        "元宝快照跳增" => Some("元宝增长来源缺失"),
        "高频流转" => Some("高频资产流转"),
        _ => None,
    }
}

fn severity_for(score: i64) -> &'static str {
    if score >= 70 {
        "严重"
    } else if score >= 45 {
        "高"
    } else {
        "中"
    }
}

fn alert_from_player(player: &Value, today: &str, map_rule: bool) -> Option<Value> {
    let score = player["score"].as_i64().unwrap_or(0);
    if score < 20 {
        return None;
    }
    let tags: Vec<&str> = player["tags"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let first_tag = *tags.first()?;
    let rule = if map_rule {
        // 优先取能映射到规则名的标签。
        let tag = tags
            .iter()
            .find(|tag| rule_for_tag(tag).is_some())
            .copied()
            .unwrap_or(first_tag);
        rule_for_tag(tag).unwrap_or(tag).to_string()
    } else {
        first_tag.to_string()
    };
    let id = player["id"].as_str().unwrap_or_default();
    let suffix: String = {
        let chars: Vec<char> = id.chars().collect();
        chars[chars.len().saturating_sub(4)..].iter().collect()
    };
    Some(json!({
        "id": format!("R-{today}-{suffix}"),
        "time": player["timeline"][0][0],
        "player": format!("{} / {}", player["name"].as_str().unwrap_or_default(), id),
        "actor_id": id,
        "rule": rule,
        "category": "database",
        "severity": severity_for(score),
        "score": score,
        "state": "待研判",
        "evidence": player["evidence"],
    }))
}

fn sort_alerts(mut alerts: Vec<Value>) -> Vec<Value> {
    alerts.sort_by(|left, right| {
        right["score"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&left["score"].as_i64().unwrap_or(0))
    });
    alerts
}

/// 告警队列。分数低于 20 的角色不入队。
pub fn alerts_result(db: &mut GameDatabase) -> Result<Vec<Value>> {
    let today = Local::now().format("%Y%m%d").to_string();
    let players = all_player_results(db)?;
    Ok(sort_alerts(
        players
            .iter()
            .filter_map(|player| alert_from_player(player, &today, true))
            .collect(),
    ))
}

/// 总览页用的告警列表。与 `alerts_result` 的差别是规则名直接取首个标签，
/// 保持与 Python 版 `alerts_result_from_players` 一致。
fn alerts_from_players(players: &[Value], today: &str) -> Vec<Value> {
    sort_alerts(
        players
            .iter()
            .filter_map(|player| alert_from_player(player, today, false))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// 总览
// ---------------------------------------------------------------------------

/// 总览页数据。`started_at` 用于计算这次查询的耗时。
pub fn dashboard_result(db: &mut GameDatabase, started_at: std::time::Instant) -> Result<Value> {
    let today_start = Local::now().format("%Y%m%d000000").to_string();
    let today = Local::now().format("%Y%m%d").to_string();

    let summary_params: Vec<Param> = (0..10).map(|_| Param::Str(today_start.clone())).collect();
    let summary = db
        .fetch_one(
        "select
           (select count(*) from dl_ldb_1.money_log where update_time >= ?) +
           (select count(*) from dl_ldb_1.item_transfer_log where update_time >= ?) +
           (select count(*) from dl_ldb_1.equipment_log where update_time >= ?) +
           (select count(*) from dl_ldb_1.cost_coin_log where update_time >= ?) +
           (select count(*) from dl_ldb_1.apply_log where update_time >= ?) +
           (select count(*) from dl_ldb_1.campaign_log where update_time >= ? and gid<>'') +
           (select count(*) from dl_ldb_1.errand_log where update_time >= ? and gid<>'') +
           (select count(*) from dl_ldb_1.user_log where update_time >= ? and action in ('drop','get','exchange','buy','take_stall_cash','drop_pet')) +
           (select count(*) from dl_ldb_1.pet_log where update_time >= ?) event_count,
          (select count(*) from dl_mdb_1.item_info) +
           (select count(*) from dl_mdb_1.pet_info) asset_count,
          (select count(*) from dl_ldb_1.important_log
           where update_time >= ? and action='abnormal_coin_num') abnormal_today",
            &summary_params,
        )?
        .unwrap_or_default();
    let event_count = summary.int("event_count");
    let asset_count = summary.int("asset_count");
    let abnormal_today = summary.int("abnormal_today");

    let players = all_player_results(db)?;
    let alerts = alerts_from_players(&players, &today);

    let scores: Vec<i64> = players
        .iter()
        .map(|player| player["score"].as_i64().unwrap_or(0))
        .collect();
    let normal = scores.iter().filter(|score| **score < 35).count() as f64;
    let watching = scores
        .iter()
        .filter(|score| **score >= 35 && **score < 70)
        .count() as f64;
    let high = scores.iter().filter(|score| **score >= 70).count() as f64;
    let total_players = scores.len().max(1) as f64;

    let bands = json!([
        [
            "正常",
            python_round(normal * 100.0 / total_players, 1),
            "green"
        ],
        [
            "观察",
            python_round(watching * 100.0 / total_players, 1),
            "gold"
        ],
        [
            "高风险",
            python_round(high * 100.0 / total_players, 1),
            "coral"
        ],
        ["已阻断", 0, "dark"],
    ]);

    let since = now_stamp_minus(ChronoDuration::hours(12));
    let since_params: Vec<Param> = (0..8).map(|_| Param::Str(since.clone())).collect();
    let recent = db.fetch_all(
        "select update_time from dl_ldb_1.money_log where update_time >= ?
         union all select update_time from dl_ldb_1.item_transfer_log where update_time >= ?
         union all select update_time from dl_ldb_1.equipment_log where update_time >= ?
         union all select update_time from dl_ldb_1.cost_coin_log where update_time >= ?
         union all select update_time from dl_ldb_1.campaign_log where update_time >= ? and gid<>''
         union all select update_time from dl_ldb_1.errand_log where update_time >= ? and gid<>''
         union all select update_time from dl_ldb_1.user_log where update_time >= ? and action in ('drop','get','exchange','buy','take_stall_cash','drop_pet')
         union all select update_time from dl_ldb_1.pet_log where update_time >= ?",
        &since_params,
    )?;

    let distribution = hourly_distribution(&recent);

    let latency = ((started_at.elapsed().as_secs_f64() * 1000.0).round() as i64).max(1);
    let coverage = format!("{}/{} 数据表", ASSET_TABLES.len(), ASSET_TABLES.len());
    let risk_players = scores.iter().filter(|score| **score >= 35).count() as i64;

    Ok(json!({
        "updatedAt": Local::now().format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
        "sourceMode": "live",
        "headline": "真实资产账本已连接",
        "description": "当前页面直接读取游戏数据库，展示元宝、奖励、道具掉落/拾取、交易与服务端校验记录。",
        "scope": "全部可分析角色",
        "health": {
            "status": "实时数据已连接",
            "latency": format!("{latency} ms"),
            "coverage": coverage,
            "backlog": alerts.len(),
        },
        "metrics": [
            ["今日资产日志", number(event_count), "来自权威日志"],
            ["风险角色", number(risk_players), format!("共 {} 个角色", players.len())],
            ["可溯源资产", number(asset_count), "当前道具与宠物持有表"],
            ["今日币值异常", number(abnormal_today), "服务端校验"],
        ],
        "distribution": distribution,
        "riskBands": bands,
        "alerts": alerts.iter().take(4).collect::<Vec<_>>(),
    }))
}

/// 把最近 12 小时的事件按小时分桶，再归一化成 0-100 的柱高。
fn hourly_distribution(rows: &[Row]) -> Vec<i64> {
    let mut bins = [0i64; 12];
    let now = Local::now().naive_local();
    for row in rows {
        let stamp = row.text("update_time");
        let Ok(event_time) = NaiveDateTime::parse_from_str(&stamp, TIMESTAMP_FORMAT) else {
            // 解析不了的时间戳直接跳过，对应 Python 的 except ValueError。
            continue;
        };
        let age = (now - event_time).num_seconds() / 3600;
        if (0..12).contains(&age) {
            bins[(11 - age) as usize] += 1;
        }
    }
    let peak = bins.iter().copied().max().unwrap_or(0);
    bins.iter()
        .map(|value| {
            if peak == 0 {
                0
            } else {
                python_round(*value as f64 * 100.0 / peak as f64, 0) as i64
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 资产溯源
// ---------------------------------------------------------------------------

type TraceNode = (String, String, String, String);

fn asset_search_terms(raw: &str) -> (String, String) {
    let query = raw.trim().chars().take(128).collect::<String>();
    let ascii = if query.is_ascii() {
        query.clone()
    } else {
        String::new()
    };
    (query, ascii)
}

/// 先用客户知道的角色、账号或名称发现当前资产，再由 IID 进入完整溯源。
pub fn asset_search_result(db: &mut GameDatabase, query: Option<&str>) -> Result<Value> {
    let (query, ascii) = asset_search_terms(query.unwrap_or_default());
    let iid = if ascii.is_empty() {
        String::new()
    } else {
        risk_core::normalized_iid(&ascii)
    };
    let rows = if query.is_empty() {
        db.fetch_all(
            "select update_time,iid,name,owner,owner_name,env,pos,amount,'item' asset_kind
               from dl_mdb_1.item_info
             union all
             select update_time,iid,name,owner,owner_name,env,pos,1 amount,'pet' asset_kind
               from dl_mdb_1.pet_info
             order by update_time desc limit 21",
            &[],
        )?
    } else {
        let mut params = Vec::with_capacity(12);
        for _ in 0..2 {
            params.extend([
                Param::Str(ascii.clone()),
                Param::Str(ascii.clone()),
                Param::Str(query.clone()),
                Param::Str(query.clone()),
                Param::Str(iid.clone()),
                Param::Str(query.clone()),
            ]);
        }
        db.fetch_all(
            "select update_time,iid,name,owner,owner_name,env,pos,amount,'item' asset_kind
               from dl_mdb_1.item_info
              where owner in (select gid from dl_mdb_1.char_info where gid=? or account=? or name=?)
                 or owner_name=? or locate(?,name)>0 or replace(iid,':','')=?
             union all
             select update_time,iid,name,owner,owner_name,env,pos,1 amount,'pet' asset_kind
               from dl_mdb_1.pet_info
              where owner in (select gid from dl_mdb_1.char_info where gid=? or account=? or name=?)
                 or owner_name=? or locate(?,name)>0 or replace(iid,':','')=?
             order by update_time desc limit 51",
            &params,
        )?
    };
    let truncated = rows.len() > 50;
    Ok(json!({
        "query": query,
        "truncated": truncated,
        "results": rows.iter().take(50).map(|row| json!({
            "id": format!(":{}:", risk_core::normalized_iid(&row.text("iid"))),
            "name": row.text("name"),
            "kind": if row.text("asset_kind") == "pet" { "宠物" } else { "道具" },
            "owner": format!("{} / {}", row.text("owner_name"), row.text("owner")),
            "quantity": row.int("amount"),
            "location": format!("{} / {}", row.text("env"), row.text("pos")),
            "updatedAt": stamp_label(&row.text("update_time")),
        })).collect::<Vec<_>>(),
    }))
}

/// 资产完整路径。
pub fn asset_result(
    db: &mut GameDatabase,
    query: Option<&str>,
    ledger: Option<&rusqlite_alias::Connection>,
) -> Result<Value> {
    let (current, iid) = match query.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => {
            let iid = risk_core::normalized_iid(raw);
            let item = db.fetch_one(
                "select *, 'item' asset_kind from dl_mdb_1.item_info where replace(iid,':','')=? limit 1",
                &[Param::Str(iid.clone())],
            )?;
            let current = match item {
                Some(row) => Some(row),
                None => db.fetch_one(
                    "select update_time,dist,owner,pos,owner_name,name,env,1 amount,iid,'pet' asset_kind
                     from dl_mdb_1.pet_info where replace(iid,':','')=? limit 1",
                    &[Param::Str(iid.clone())],
                )?,
            };
            (current, iid)
        }
        None => {
            let current = db.fetch_one(
                "select *, 'item' asset_kind from dl_mdb_1.item_info order by update_time desc limit 1",
                &[],
            )?;
            let iid = current
                .as_ref()
                .map(|row| risk_core::normalized_iid(&row.text("iid")))
                .unwrap_or_default();
            (current, iid)
        }
    };

    let iid_param = [Param::Str(iid.clone())];
    let transfer_rows = db.fetch_all(
        "select * from dl_ldb_1.item_transfer_log where replace(item_iid,':','')=? order by update_time, id",
        &iid_param,
    )?;
    let equipment_rows = db.fetch_all(
        "select * from dl_ldb_1.equipment_log where replace(item_iid,':','')=? order by update_time, id",
        &iid_param,
    )?;
    let apply_rows = db.fetch_all(
        "select * from dl_ldb_1.apply_log where replace(iid,':','')=? order by update_time, id",
        &iid_param,
    )?;
    let cost_rows = db.fetch_all(
        "select * from dl_ldb_1.cost_coin_log where replace(uid,':','')=? order by update_time, id",
        &iid_param,
    )?;

    let iid_pattern = format!("%{iid}%");
    let activity_rows = db.fetch_all(
        "select update_time,action,gid,bonus_type,bonus_name,bonus_prop,'campaign_log' source_table,id
         from dl_ldb_1.campaign_log where bonus_type in (1,14) and replace(bonus_prop,':','') like ?
         union all
         select update_time,action,gid,bonus_type,bonus_name,bonus_prop,'errand_log' source_table,id
         from dl_ldb_1.errand_log where bonus_type in (1,14) and replace(bonus_prop,':','') like ?
         order by update_time, source_table, id",
        &[
            Param::Str(iid_pattern.clone()),
            Param::Str(iid_pattern),
        ],
    )?;
    let user_rows = db.fetch_all(
        "select update_time,type,action,para1,para2,para3,memo
         from dl_ldb_1.user_log
         where replace(para2,':','')=?
           and action in ('drop','get','exchange','buy','drop_pet')
         order by update_time, id",
        &iid_param,
    )?;
    let pet_rows = db.fetch_all(
        "select update_time,gid,type,action,pet_name,pet_iid,cost_item,item_iid,para1,para2,para3
         from dl_ldb_1.pet_log where replace(pet_iid,':','')=? order by update_time, id",
        &iid_param,
    )?;
    let important_pet_rows = db.fetch_all(
        "select update_time,action,gid_from,gid_to,pet_iid,pet_name
         from dl_ldb_1.important_pet_log where replace(pet_iid,':','')=? order by update_time, id",
        &iid_param,
    )?;

    let snapshot_rows: Vec<LedgerEvent> = match ledger {
        Some(connection) => ledger_events(connection, &iid)?,
        None => Vec::new(),
    };

    let origin_rows: Vec<&Row> = activity_rows
        .iter()
        .filter(|row| is_confirmed_gain(&row.text("action")))
        .collect();
    let user_origin_rows: Vec<&Row> = user_rows
        .iter()
        .filter(|row| row.text("action") == "buy")
        .collect();
    let pet_origin_rows: Vec<&Row> = pet_rows
        .iter()
        .filter(|row| row.text("action") == "jianglcw")
        .collect();

    if current.is_none()
        && transfer_rows.is_empty()
        && equipment_rows.is_empty()
        && apply_rows.is_empty()
        && cost_rows.is_empty()
        && activity_rows.is_empty()
        && user_rows.is_empty()
        && pet_rows.is_empty()
        && important_pet_rows.is_empty()
        && snapshot_rows.is_empty()
    {
        return Err(LookupError("未找到资产流水".to_string()).into());
    }

    let mut nodes: Vec<TraceNode> = Vec::new();

    for row in &snapshot_rows {
        let (action, note) = EventKind::labels(&row.event_type);
        let owner_from = if row.owner_from.is_empty() {
            "-"
        } else {
            &row.owner_from
        };
        let owner_to = if row.owner_to.is_empty() {
            "-"
        } else {
            &row.owner_to
        };
        nodes.push((
            row.event_time.clone(),
            action,
            format!("{owner_from} → {owner_to}"),
            note,
        ));
    }

    for row in &activity_rows {
        let action = row.text("action");
        nodes.push((
            row.text("update_time"),
            if is_confirmed_gain(&action) {
                "游戏奖励发放".to_string()
            } else {
                "游戏资产操作".to_string()
            },
            row.text("gid"),
            format!(
                "{} / {action} / {}",
                row.text("source_table"),
                row.text("bonus_name")
            ),
        ));
    }

    for row in &user_rows {
        let action = row.text("action");
        let label = match action.as_str() {
            "drop" => "玩家丢弃",
            "get" => "玩家拾取",
            "exchange" => "玩家当面交易",
            "buy" => "NPC 商店购买",
            "drop_pet" => "玩家丢弃宠物",
            other => other,
        };
        let owner = if action == "exchange" {
            format!("{} → {}", row.text("para3"), row.text("para1"))
        } else {
            row.text("para1")
        };
        nodes.push((
            row.text("update_time"),
            label.to_string(),
            owner,
            format!("user_log / IID {}", row.text("para2")),
        ));
    }

    for row in &pet_rows {
        let action = row.text("action");
        // 交接报告 §3.3：jianglcw 是奖励获得宠物，yiq 是宠物丢弃。
        let label = match action.as_str() {
            "jianglcw" => "奖励获得宠物".to_string(),
            "yiq" => "宠物丢弃".to_string(),
            "chaojssd" => "宠物培养".to_string(),
            "dianhkq" => "宠物点化开启".to_string(),
            "dianhtslq" => "宠物点化培养".to_string(),
            other => format!("宠物操作 {other}"),
        };
        let kind = row.text("type");
        let mut note = format!("pet_log / {}", if kind.is_empty() { "-" } else { &kind });
        let cost_item = row.text("cost_item");
        if !cost_item.is_empty() {
            note.push_str(&format!(" / 消耗 {cost_item}"));
        }
        nodes.push((row.text("update_time"), label, row.text("gid"), note));
    }

    for row in &important_pet_rows {
        nodes.push((
            row.text("update_time"),
            "重要宠物所有权记录".to_string(),
            format!("{} → {}", row.text("gid_from"), row.text("gid_to")),
            format!("important_pet_log / {}", row.text("action")),
        ));
    }

    for row in &cost_rows {
        nodes.push((
            row.text("update_time"),
            "商城生成".to_string(),
            row.text("gid"),
            format!(
                "购买 {} 件，消耗 {} {}",
                row.text("amount"),
                number(row.int("cost")),
                row.text("cost_type")
            ),
        ));
    }

    for row in &apply_rows {
        nodes.push((
            row.text("update_time"),
            "商城发放".to_string(),
            row.text("gid"),
            format!(
                "商品来源 {}，价格 {}",
                row.text("item_source"),
                number(row.int("item_price"))
            ),
        ));
    }

    for row in &transfer_rows {
        nodes.push((
            row.text("update_time"),
            transfer_trace_action(&transfer_row_from(row)),
            format!("{} → {}", row.text("gid_from"), row.text("gid_to")),
            format!("交易号 {}", row.text("transfer_id")),
        ));
    }

    for row in &equipment_rows {
        nodes.push((
            row.text("update_time"),
            format!("装备操作 {}", row.text("action")),
            row.text("gid"),
            format!("结果 {}", row.int("oper_result")),
        ));
    }

    if let Some(row) = &current {
        nodes.push((
            row.text("update_time"),
            "当前持有".to_string(),
            format!("{} / {}", row.text("owner_name"), row.text("owner")),
            format!("{}位置 {}", row.text("env"), row.text("pos")),
        ));
    }

    // 稳定升序：时间相同的节点保持插入顺序。
    nodes.sort_by(|left, right| left.0.cmp(&right.0));

    let unique_rows = db.fetch_count(
        "select
           (select count(*) from dl_mdb_1.item_info where replace(iid,':','')=?) +
           (select count(*) from dl_mdb_1.pet_info where replace(iid,':','')=?) count",
        &[Param::Str(iid.clone()), Param::Str(iid.clone())],
        "count",
    )?;

    let has_source = !cost_rows.is_empty()
        || !apply_rows.is_empty()
        || !origin_rows.is_empty()
        || !user_origin_rows.is_empty()
        || !pet_origin_rows.is_empty();

    let mut risk = 0i64;
    let mut notes: Vec<&str> = Vec::new();
    if unique_rows > 1 {
        risk += 80;
        notes.push("唯一序列号重复");
    }
    if !has_source {
        risk += 30;
        notes.push("生成来源尚未覆盖");
    }

    // 摆摊必须两条腿齐全，缺腿即账本不守恒。
    let bait_ids: HashSet<String> = transfer_rows
        .iter()
        .filter(|row| row.text("action") == "bait" && !row.text("transfer_id").is_empty())
        .map(|row| row.text("transfer_id"))
        .collect();
    for transfer_id in &bait_ids {
        let legs = db.fetch_all(
            "select item_iid from dl_ldb_1.item_transfer_log where transfer_id=?",
            &[Param::Str(transfer_id.clone())],
        )?;
        let has_item_leg = legs.iter().any(|row| row.truthy("item_iid"));
        let has_cash_leg = legs.iter().any(|row| !row.truthy("item_iid"));
        if !has_item_leg || !has_cash_leg {
            risk += 30;
            notes.push("交易账本缺腿");
            break;
        }
    }
    let risk = risk.min(100);

    let state = if unique_rows > 1 {
        "唯一性冲突"
    } else if !notes.is_empty() {
        "证据不完整"
    } else {
        "链路可闭合"
    };

    let name = resolve_asset_name(
        current.as_ref(),
        &transfer_rows,
        &activity_rows,
        &user_rows,
        &pet_rows,
        &snapshot_rows,
    );

    let source = if !origin_rows.is_empty() || !pet_origin_rows.is_empty() {
        "游戏奖励日志"
    } else if !cost_rows.is_empty() || !apply_rows.is_empty() || !user_origin_rows.is_empty() {
        "商城权威日志"
    } else {
        "现有日志最早节点"
    };

    Ok(json!({
        "id": format!(":{iid}:"),
        "name": name,
        "quantity": current.as_ref().map(|row| row.int("amount")).unwrap_or(0),
        "state": state,
        "risk": risk,
        "owner": current.as_ref().map(|row| format!("{} / {}", row.text("owner_name"), row.text("owner")))
            .unwrap_or_else(|| "已离开当前持有表".to_string()),
        "source": source,
        "nodes": nodes.into_iter()
            .map(|(stamp, action, owner, note)| json!([stamp_label(&stamp), action, owner, note]))
            .collect::<Vec<_>>(),
    }))
}

/// 资产名的回落链，与 Python 的嵌套三元表达式同序。
fn resolve_asset_name(
    current: Option<&Row>,
    transfer_rows: &[Row],
    activity_rows: &[Row],
    user_rows: &[Row],
    pet_rows: &[Row],
    snapshot_rows: &[LedgerEvent],
) -> String {
    if let Some(row) = current {
        return row.text("name");
    }
    if let Some(row) = transfer_rows.last() {
        return row.text("item_name");
    }
    if let Some(row) = activity_rows.last() {
        return row.text("bonus_name");
    }
    let named_user_row = user_rows.iter().rfind(|row| {
        matches!(row.text("action").as_str(), "drop" | "get" | "drop_pet") && row.truthy("para3")
    });
    if let Some(row) = named_user_row {
        return row.text("para3");
    }
    let named_pet_row = pet_rows.iter().rfind(|row| row.truthy("pet_name"));
    if let Some(row) = named_pet_row {
        return row.text("pet_name");
    }
    if let Some(event) = snapshot_rows.last() {
        return event.name.clone();
    }
    "未知资产".to_string()
}

// ---------------------------------------------------------------------------
// 采集与连接测试
// ---------------------------------------------------------------------------

/// 读取当前道具与宠物持有表，供账本比对。
pub fn current_assets(db: &mut GameDatabase) -> Result<Vec<risk_ledger::AssetRow>> {
    let rows = db.fetch_all(
        "select iid,name,owner,owner_name,env,pos,amount from dl_mdb_1.item_info
         union all
         select iid,name,owner,owner_name,env,pos,1 amount from dl_mdb_1.pet_info",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|row| risk_ledger::AssetRow {
            iid: row.text("iid"),
            name: row.text("name"),
            owner: row.text("owner"),
            owner_name: row.text("owner_name"),
            env: row.text("env"),
            pos: row.int("pos"),
            amount: row.int("amount"),
        })
        .collect())
}

/// 连接测试：确认版本可读且七张核心表存在。
pub fn connection_test(db: &mut GameDatabase) -> Result<Value> {
    let version = db
        .fetch_one("select version() version", &[])?
        .map(|row| row.text("version"))
        .unwrap_or_default();
    let main_database = db.main_database().to_string();
    let log_database = db.log_database().to_string();
    let tables = db.fetch_count(
        "select count(*) count from information_schema.tables
         where (table_schema=? and table_name in ('char_info','item_info','pet_info'))
            or (table_schema=? and table_name in ('login_log','item_transfer_log','campaign_log','errand_log'))",
        &[
            Param::Str(main_database.clone()),
            Param::Str(log_database.clone()),
        ],
        "count",
    )?;
    if tables < 7 {
        anyhow::bail!("required RISK tables are missing");
    }
    Ok(json!({
        "ok": true,
        "message": "数据库连接成功，核心表可读",
        "serverVersion": version,
        "mainDatabase": main_database,
        "logDatabase": log_database,
        "verifiedTables": tables,
    }))
}

/// 让上层无需直接依赖 rusqlite 的别名。
pub mod rusqlite_alias {
    pub use rusqlite::Connection;
}

/// 把 `Facts` 序列化成 Python 版 evidence 字典同构的 JSON。
pub fn facts_to_json(facts: &Facts) -> Value {
    serde_json::to_value(facts).unwrap_or(Value::Object(Map::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gameplay_catalog_suggestions_keep_headroom() {
        assert_eq!(suggested_gameplay_limit(0), 0);
        assert_eq!(suggested_gameplay_limit(1), 4);
        assert_eq!(suggested_gameplay_limit(10), 13);
        assert_eq!(suggested_gameplay_limit(100), 120);
        assert_eq!(gameplay_catalog_label("huilcbjl", "", ""), "回合奖励");
        assert_eq!(gameplay_catalog_label("custom", "女娲石", ""), "女娲石奖励");
        assert_eq!(gameplay_catalog_label("custom", "0", ""), "奖励行为 custom");
    }

    #[test]
    fn python_round_uses_half_to_even() {
        // Python: round(0.5) == 0, round(1.5) == 2, round(2.5) == 2
        assert_eq!(python_round(0.5, 0), 0.0);
        assert_eq!(python_round(1.5, 0), 2.0);
        assert_eq!(python_round(2.5, 0), 2.0);
        assert_eq!(python_round(3.5, 0), 4.0);
        // 非 .5 边界走常规四舍五入。
        assert_eq!(python_round(2.4, 0), 2.0);
        assert_eq!(python_round(2.6, 0), 3.0);
    }

    #[test]
    fn python_round_supports_one_decimal() {
        assert_eq!(python_round(96.44, 1), 96.4);
        assert_eq!(python_round(96.46, 1), 96.5);
        assert_eq!(python_round(100.0, 1), 100.0);
        assert_eq!(python_round(0.0, 1), 0.0);
    }

    #[test]
    fn median_matches_python_statistics() {
        assert_eq!(median_int(&mut []), 0);
        assert_eq!(median_int(&mut [5]), 5);
        // 偶数个取中间两个平均后截断：(4+5)/2 = 4.5 -> 4
        assert_eq!(median_int(&mut [1, 4, 5, 9]), 4);
        assert_eq!(median_int(&mut [1, 3, 5]), 3);
        // 未排序输入也要正确。
        assert_eq!(median_int(&mut [9, 1, 5, 4]), 4);
        assert_eq!(median_int(&mut [10, 10]), 10);
    }

    #[test]
    fn latest_snapshot_values_include_deleted_gids_and_latest_ties() {
        let mut values = latest_gold_coin_values([
            ("active".to_string(), "20260731090000".to_string(), 100),
            ("active".to_string(), "20260730090000".to_string(), 1),
            ("deleted".to_string(), "20260731100000".to_string(), 900),
            ("deleted".to_string(), "20260731100000".to_string(), 1100),
            ("deleted".to_string(), "20260730100000".to_string(), 2),
        ]);
        values.sort_unstable();
        assert_eq!(values, [100, 900, 1100]);
        assert_eq!(median_int(&mut values), 900);
    }

    #[test]
    fn asset_search_terms_preserve_names_and_bound_input() {
        assert_eq!(
            asset_search_terms("  北境长歌  "),
            ("北境长歌".to_string(), String::new())
        );
        assert_eq!(
            asset_search_terms(" acc_88241 "),
            ("acc_88241".to_string(), "acc_88241".to_string())
        );
        assert_eq!(asset_search_terms(&"x".repeat(140)).0.len(), 128);
    }

    #[test]
    fn profile_distribution_uses_nearest_rank_without_identities() {
        let result = distribution(&[1, 1, 2, 4, 10]);
        assert_eq!(result["groups"], 5);
        assert_eq!(result["p50"], 2);
        assert_eq!(result["p90"], 10);
        assert_eq!(result["p99"], 10);
        assert_eq!(result["max"], 10);
        assert_eq!(distribution(&[])["max"], 0);
    }

    #[test]
    fn asset_funnel_counts_only_one_way_asset_sources() {
        let mut flows = HashMap::new();
        record_asset_flow(&mut flows, "target", "source-a", "target", true);
        record_asset_flow(&mut flows, "target", "source-a", "target", true);
        record_asset_flow(&mut flows, "target", "source-b", "target", true);
        record_asset_flow(&mut flows, "target", "target", "source-b", true); // 有回流
        record_asset_flow(&mut flows, "target", "source-c", "target", false); // 金币腿
        record_asset_flow(&mut flows, "target", "(undefined)", "target", true);
        record_asset_flow(&mut flows, "target", "target", "target", true);

        assert_eq!(asset_funnel_counts(&flows), (1, 2));
    }

    #[test]
    fn burst_funnel_uses_one_ten_minute_window() {
        let mut events = Vec::new();
        for (stamp, peer) in [
            ("20260101000000", "source-a"),
            ("20260101000100", "source-a"),
            ("20260101000200", "source-b"),
            ("20260101001000", "source-c"),
            ("20260101003000", "source-d"),
        ] {
            record_inbound_asset_event(&mut events, "target", peer, "target", ":A1:", stamp);
        }
        record_inbound_asset_event(
            &mut events,
            "target",
            "source-e",
            "target",
            ":A2:",
            "invalid",
        );

        assert_eq!(burst_funnel_counts(&mut events), (3, 4));
    }

    #[test]
    fn roundtrip_counts_unique_iids_and_peers() {
        let mut directions = AssetDirections::new();
        for (from, to, iid) in [
            ("peer-a", "target", ":A1:"),
            ("target", "peer-a", ":a1:"),
            ("peer-a", "target", ":A2:"),
            ("peer-b", "target", ":A1:"),
            ("target", "peer-b", ":A1:"),
            ("peer-b", "target", ":A3:"),
            ("target", "peer-b", ":A3:"),
            ("(undefined)", "target", ":IGNORED:"),
        ] {
            record_asset_direction(&mut directions, "target", from, to, iid);
        }

        assert_eq!(asset_roundtrip_counts(&directions), (2, 2));
    }

    #[test]
    fn rhythm_requires_dense_activity_on_multiple_long_days() {
        let mut events = Vec::new();
        for day in [1, 2] {
            let start =
                NaiveDateTime::parse_from_str(&format!("202601{day:02}000000"), TIMESTAMP_FORMAT)
                    .unwrap();
            for index in 0..100 {
                events.push(RhythmEvent {
                    at: start + ChronoDuration::minutes(index * 1080 / 99),
                    behavior: format!("event-{day}-{index}"),
                });
            }
        }
        let facts = analyze_rhythm_events(&events);
        assert_eq!(facts.long_active_days, 2);
        assert_eq!(facts.max_daily_span_minutes, 1080);
        assert_eq!(facts.max_daily_events, 100);
    }

    #[test]
    fn rhythm_detects_fixed_intervals_and_deduplicates_same_second() {
        let start = NaiveDateTime::parse_from_str("20260101000000", TIMESTAMP_FORMAT).unwrap();
        let mut events: Vec<RhythmEvent> = (0..=30)
            .map(|index| RhythmEvent {
                at: start + ChronoDuration::seconds(index * 60),
                behavior: "user:start_combat".to_string(),
            })
            .collect();
        events.push(events[0].clone());

        let facts = analyze_rhythm_events(&events);
        assert_eq!(facts.mechanical_action, "user:start_combat");
        assert_eq!(facts.mechanical_events, 31);
        assert_eq!(facts.mechanical_interval_seconds, 60);
        assert_eq!(facts.mechanical_ratio_permille, 1000);
        assert_eq!(facts.mechanical_span_minutes, 30);
    }

    #[test]
    fn reward_flow_detects_burst_and_repeated_concentrated_outflow() {
        let start = NaiveDateTime::parse_from_str("20260101000000", TIMESTAMP_FORMAT).unwrap();
        let mut rewards: Vec<RewardEvent> = (0..10)
            .map(|index| RewardEvent {
                at: start + ChronoDuration::minutes(index),
                action: "huilcbjl".to_string(),
            })
            .collect();
        rewards.push(rewards[0].clone()); // 同动作同秒的重复奖励腿不重复计算
        for day in [0, 1, 2] {
            rewards.push(RewardEvent {
                at: start + ChronoDuration::days(day) + ChronoDuration::hours(12),
                action: "jinn".to_string(),
            });
        }
        let outflows = vec![
            AssetOutflowEvent {
                at: start + ChronoDuration::minutes(1),
                target: "mule".to_string(),
            },
            AssetOutflowEvent {
                at: start + ChronoDuration::minutes(2),
                target: "mule".to_string(),
            },
            AssetOutflowEvent {
                at: start + ChronoDuration::hours(12) + ChronoDuration::minutes(1),
                target: "mule".to_string(),
            },
            AssetOutflowEvent {
                at: start + ChronoDuration::days(1) + ChronoDuration::hours(12),
                target: "mule".to_string(),
            },
            AssetOutflowEvent {
                at: start + ChronoDuration::days(2) + ChronoDuration::hours(12),
                target: "mule".to_string(),
            },
        ];

        let facts = analyze_reward_flow(&rewards, &outflows);
        assert_eq!(facts.burst_action, "huilcbjl");
        assert_eq!(facts.burst_events, 10);
        assert_eq!(facts.rapid_outflows, 5);
        assert_eq!(facts.rapid_outflow_days, 3);
        assert_eq!(facts.target_peers, 1);
    }

    #[test]
    fn one_outflow_matches_at_most_one_reward() {
        let at = NaiveDateTime::parse_from_str("20260101000000", TIMESTAMP_FORMAT).unwrap();
        let rewards = vec![
            RewardEvent {
                at,
                action: "huilcbjl".to_string(),
            },
            RewardEvent {
                at,
                action: "jinn".to_string(),
            },
        ];
        let outflows = vec![AssetOutflowEvent {
            at: at + ChronoDuration::minutes(1),
            target: "mule".to_string(),
        }];
        assert_eq!(analyze_reward_flow(&rewards, &outflows).rapid_outflows, 1);
    }

    #[test]
    fn gameplay_caps_validate_and_measure_daily_and_burst_peaks() {
        assert!(parse_gameplay_caps(
            r#"[{"action":"bad'action","label":"错误","dailyLimit":1,"burst10mLimit":1,"enabled":true}]"#
        )
        .is_err());
        let caps = parse_gameplay_caps(
            r#"[{"action":"custom_reward","label":"自定义奖励","dailyLimit":3,"burst10mLimit":2,"enabled":true}]"#,
        )
        .unwrap();
        let start = NaiveDateTime::parse_from_str("20260101000000", TIMESTAMP_FORMAT).unwrap();
        let mut rewards = vec![
            RewardEvent {
                at: start,
                action: "custom_reward".to_string(),
            },
            RewardEvent {
                at: start + ChronoDuration::minutes(1),
                action: "custom_reward".to_string(),
            },
            RewardEvent {
                at: start + ChronoDuration::minutes(2),
                action: "custom_reward".to_string(),
            },
            RewardEvent {
                at: start + ChronoDuration::hours(12),
                action: "custom_reward".to_string(),
            },
        ];
        rewards.push(rewards[0].clone());
        let facts = analyze_gameplay_caps(&rewards, &caps);
        assert_eq!(facts.action, "custom_reward");
        assert_eq!(facts.daily_events, 4);
        assert_eq!(facts.daily_limit, 3);
        assert_eq!(facts.burst_events, 3);
        assert_eq!(facts.burst_limit, 2);
        assert!(reward_action_sql_list(&caps).contains("'custom_reward'"));
    }

    #[test]
    fn severity_bands_match_python() {
        assert_eq!(severity_for(19), "中");
        assert_eq!(severity_for(44), "中");
        assert_eq!(severity_for(45), "高");
        assert_eq!(severity_for(69), "高");
        assert_eq!(severity_for(70), "严重");
    }

    #[test]
    fn rule_mapping_covers_all_scoring_tags() {
        // 每个会进入告警的标签都应有规则名映射。
        for tag in [
            "交易账本缺腿",
            "币值校验异常",
            "元宝存量偏离",
            "同设备交易",
            "多账号资产归集",
            "短时资产归集",
            "资产循环回流",
            "超长持续活跃",
            "机械周期行为",
            "奖励爆发异常",
            "奖励快速归集",
            "玩法产出超限",
            "绕过交易转移",
            "元宝快照跳增",
            "高频流转",
        ] {
            assert!(rule_for_tag(tag).is_some(), "标签 {tag} 缺少规则映射");
        }
        // 「未见强异常」不该有映射，它也不会进告警队列。
        assert!(rule_for_tag("未见强异常").is_none());
    }

    #[test]
    fn low_score_players_are_not_alerted() {
        let player = json!({
            "id": "1003281", "name": "北境长歌", "score": 19,
            "tags": ["未见强异常"], "timeline": [["07-30 14:21:08", "", "", ""]],
        });
        assert!(alert_from_player(&player, "20260730", true).is_none());
    }

    #[test]
    fn alert_prefers_mapped_rule_tag() {
        let player = json!({
            "id": "1003281", "name": "北境长歌", "score": 79,
            // 首个标签没有映射，应跳到有映射的那个。
            "tags": ["未收录标签", "交易账本缺腿"],
            "timeline": [["07-30 14:21:08", "", "", ""]],
        });
        let alert = alert_from_player(&player, "20260730", true).unwrap();
        assert_eq!(alert["rule"], "交易账本不守恒");
        assert_eq!(alert["id"], "R-20260730-3281");
        assert_eq!(alert["severity"], "严重");
        assert_eq!(alert["player"], "北境长歌 / 1003281");
        assert_eq!(alert["actor_id"], "1003281");
        assert_eq!(alert["category"], "database");
        assert_eq!(alert["state"], "待研判");
    }

    #[test]
    fn overview_alert_uses_first_tag_verbatim() {
        let player = json!({
            "id": "1003281", "name": "北境长歌", "score": 50,
            "tags": ["同设备交易", "交易账本缺腿"],
            "timeline": [["07-30 14:21:08", "", "", ""]],
        });
        let alert = alert_from_player(&player, "20260730", false).unwrap();
        assert_eq!(alert["rule"], "同设备交易");
        assert_eq!(alert["severity"], "高");
    }

    #[test]
    fn alert_id_handles_short_gid() {
        let player = json!({
            "id": "42", "name": "短号", "score": 80,
            "tags": ["高频流转"], "timeline": [["-", "", "", ""]],
        });
        let alert = alert_from_player(&player, "20260730", true).unwrap();
        assert_eq!(alert["id"], "R-20260730-42");
    }

    #[test]
    fn alerts_are_sorted_by_score_descending() {
        let alerts = sort_alerts(vec![
            json!({"score": 30}),
            json!({"score": 90}),
            json!({"score": 60}),
        ]);
        let scores: Vec<i64> = alerts
            .iter()
            .map(|alert| alert["score"].as_i64().unwrap())
            .collect();
        assert_eq!(scores, vec![90, 60, 30]);
    }

    #[test]
    fn lookup_error_message_reaches_display() {
        let error = LookupError("未找到匹配玩家".to_string());
        assert_eq!(error.to_string(), "未找到匹配玩家");
    }
}

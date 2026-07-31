//! 游戏插件本机事件接收器。
//!
//! v1 负责 loopback 接收、合同校验、SQLite/WAL 持久队列、可靠上送和影子决策。

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration as StdDuration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const SCHEMA_VERSION: &str = "1.0";
const DEFAULT_PORT: u16 = 17_870;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_EVENT_BODY_BYTES: usize = 256 * 1024;
const MAX_DECISION_BODY_BYTES: usize = 64 * 1024;
const MAX_UPSTREAM_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_EVENTS: usize = 200;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
const TEN_MINUTES_MS: i64 = 10 * 60 * 1000;
const ONE_HOUR_MS: i64 = 60 * 60 * 1000;
const ACTIVITY_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_GOLD_GAIN_10M: i64 = 1_000_000;
const DEFAULT_ASSET_MOVES_10M: i64 = 5;
const DEFAULT_HIGH_VALUE_GOLD: i64 = 1_000_000;
const DEFAULT_HIGH_VALUE_ASSET_QUANTITY: i64 = 20;
const RAPID_IDENTICAL_ACTION_COUNT: i64 = 20;
const RAPID_IDENTICAL_ACTION_WINDOW_MS: i64 = 10_000;
const MAX_REALTIME_DELTA: u64 = 9_000_000_000_000_000;
const DEFAULT_DELIVERY_BATCH_SIZE: usize = 100;
const DEFAULT_DELIVERY_MAX_ATTEMPTS: i64 = 20;
const DELIVERY_LEASE_MS: i64 = 30_000;
const DELIVERY_IDLE_SLEEP_MS: u64 = 1_000;

const EVENT_TYPES: &[&str] = &[
    "session.started",
    "session.heartbeat",
    "session.ended",
    "state.player_snapshot",
    "ledger.currency_changed",
    "ledger.asset_created",
    "ledger.asset_moved",
    "ledger.asset_changed",
    "ledger.asset_destroyed",
    "ledger.reward_granted",
    "ledger.trade_committed",
    "security.action_attempted",
    "security.validation_failed",
];

const EVENT_STATUSES: &[&str] = &[
    "attempted",
    "succeeded",
    "rejected",
    "failed",
    "rolled_back",
];

const FORBIDDEN_PLUGIN_KEYS: &[&str] = &[
    "tenant_id",
    "server_id",
    "license_key",
    "portal_key",
    "agent_token",
    "database_password",
];

#[derive(Parser, Debug)]
#[command(name = "risk-agent", about = "游戏插件本机风控事件接收器")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 监听 loopback，接收插件事件。
    Serve,
    /// 不需要客户数据的持久队列与合同自检。
    SelfCheck,
}

#[derive(Debug, Clone)]
struct AgentConfig {
    tenant_id: String,
    server_id: String,
    local_token: String,
    port: u16,
    queue_path: PathBuf,
    mode: String,
    gold_gain_10m: i64,
    asset_moves_10m: i64,
    high_value_gold: i64,
    high_value_asset_quantity: i64,
    upstream_url: Option<String>,
    upstream_token: Option<String>,
    delivery_batch_size: usize,
    delivery_max_attempts: i64,
}

impl AgentConfig {
    fn from_env() -> Result<Self> {
        let tenant_id = required_env("PGR_TENANT_ID")?;
        let server_id = required_env("PGR_SERVER_ID")?;
        let local_token = required_env("PGR_LOCAL_TOKEN")?;
        if local_token.len() < 32 {
            bail!("PGR_LOCAL_TOKEN must contain at least 32 bytes");
        }
        let port = env::var("PGR_AGENT_PORT")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| value.parse::<u16>().context("PGR_AGENT_PORT is invalid"))
            .transpose()?
            .unwrap_or(DEFAULT_PORT);
        let queue_path = env::var_os("PGR_QUEUE_DB")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data").join("plugin-events.db"));
        let mode = env::var("PGR_MODE").unwrap_or_else(|_| "shadow".to_string());
        if mode != "shadow" {
            bail!("risk-agent v1 only supports PGR_MODE=shadow");
        }
        let gold_gain_10m = positive_env_i64("PGR_GOLD_GAIN_10M", DEFAULT_GOLD_GAIN_10M)?;
        let asset_moves_10m = positive_env_i64("PGR_ASSET_MOVES_10M", DEFAULT_ASSET_MOVES_10M)?;
        let high_value_gold = positive_env_i64("PGR_HIGH_VALUE_GOLD", DEFAULT_HIGH_VALUE_GOLD)?;
        let high_value_asset_quantity = positive_env_i64(
            "PGR_HIGH_VALUE_ASSET_QUANTITY",
            DEFAULT_HIGH_VALUE_ASSET_QUANTITY,
        )?;
        if asset_moves_10m > 1000 {
            bail!("PGR_ASSET_MOVES_10M must be at most 1000");
        }
        let upstream_url = env::var("PGR_UPSTREAM_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| normalize_upstream_url(&value))
            .transpose()?;
        let upstream_token = env::var("PGR_UPSTREAM_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        if upstream_url.is_some() != upstream_token.is_some() {
            bail!("PGR_UPSTREAM_URL and PGR_UPSTREAM_TOKEN must be configured together");
        }
        if upstream_token
            .as_ref()
            .is_some_and(|value| value.len() < 32)
        {
            bail!("PGR_UPSTREAM_TOKEN must contain at least 32 bytes");
        }
        let delivery_batch_size = positive_env_i64(
            "PGR_DELIVERY_BATCH_SIZE",
            DEFAULT_DELIVERY_BATCH_SIZE as i64,
        )?;
        if delivery_batch_size > MAX_EVENTS as i64 {
            bail!("PGR_DELIVERY_BATCH_SIZE must be at most {MAX_EVENTS}");
        }
        let delivery_max_attempts =
            positive_env_i64("PGR_DELIVERY_MAX_ATTEMPTS", DEFAULT_DELIVERY_MAX_ATTEMPTS)?;
        check_identifier(&tenant_id, "PGR_TENANT_ID")?;
        check_identifier(&server_id, "PGR_SERVER_ID")?;
        Ok(Self {
            tenant_id,
            server_id,
            local_token,
            port,
            queue_path,
            mode,
            gold_gain_10m,
            asset_moves_10m,
            high_value_gold,
            high_value_asset_quantity,
            upstream_url,
            upstream_token,
            delivery_batch_size: delivery_batch_size as usize,
            delivery_max_attempts,
        })
    }
}

fn normalize_upstream_url(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let allow_loopback_http = env::var("PGR_UPSTREAM_ALLOW_HTTP").as_deref() == Ok("1")
        && (value.starts_with("http://127.0.0.1:") || value.starts_with("http://localhost:"));
    if !value.starts_with("https://") && !allow_loopback_http {
        bail!("PGR_UPSTREAM_URL must use HTTPS");
    }
    if value.ends_with("/events:batch") {
        Ok(value.to_string())
    } else if value.ends_with("/sdk/v1") {
        Ok(format!("{value}/events:batch"))
    } else {
        bail!("PGR_UPSTREAM_URL must end with /sdk/v1 or /events:batch");
    }
}

fn positive_env_i64(name: &str, default: i64) -> Result<i64> {
    let value = env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<i64>()
                .with_context(|| format!("{name} is invalid"))
        })
        .transpose()?
        .unwrap_or(default);
    if value <= 0 {
        bail!("{name} must be positive");
    }
    Ok(value)
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    if value.is_empty() {
        bail!("{name} is required");
    }
    Ok(value)
}

fn check_identifier(value: &str, name: &str) -> Result<()> {
    if value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("{name} must match [A-Za-z0-9_.-] and be at most 128 bytes");
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Producer {
    plugin_name: String,
    plugin_version: String,
    game_build: String,
    boot_id: String,
}

impl Producer {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.plugin_name, 1, 64)?;
        check_len(&self.plugin_version, 1, 32)?;
        check_len(&self.game_build, 1, 64)?;
        check_len(&self.boot_id, 8, 128)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchEnvelope {
    schema_version: String,
    producer: Producer,
    sent_at: String,
    events: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Actor {
    player_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    character_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_ip: Option<String>,
}

impl Actor {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.player_id, 1, 128)?;
        check_optional_len(&self.account_id, 128)?;
        check_optional_len(&self.character_id, 128)?;
        check_optional_len(&self.session_id, 128)?;
        check_optional_len(&self.device_fingerprint, 256)?;
        check_optional_len(&self.client_ip, 64)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Position {
    x: f64,
    y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    z: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    action_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    map_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    position: Option<Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

impl EventContext {
    fn is_empty(&self) -> bool {
        self.action_code.is_none()
            && self.reason_code.is_none()
            && self.source_type.is_none()
            && self.source_id.is_none()
            && self.config_version.is_none()
            && self.map_id.is_none()
            && self.position.is_none()
            && self.client_version.is_none()
            && self.request_id.is_none()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CurrencyChange {
    owner_id: String,
    currency: String,
    before: String,
    after: String,
    delta: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    balance_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetChange {
    asset_id: String,
    asset_kind: String,
    operation: String,
    template_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    quantity_before: i64,
    quantity_after: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot_before: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes_before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes_after: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Currencies {
    game_cash: String,
    gold_coin: String,
    silver_coin: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlayerState {
    online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    in_combat: Option<bool>,
    level: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    map_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    position: Option<Position>,
    currencies: Currencies,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inventory_digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidationEvidence {
    rule_code: String,
    severity: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EventData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    currency_changes: Option<Vec<CurrencyChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asset_changes: Option<Vec<AssetChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    player_state: Option<PlayerState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validation: Option<ValidationEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<Map<String, Value>>,
}

impl EventData {
    fn is_empty(&self) -> bool {
        self.currency_changes.as_ref().is_none_or(Vec::is_empty)
            && self.asset_changes.as_ref().is_none_or(Vec::is_empty)
            && self.player_state.is_none()
            && self.validation.is_none()
            && self.metadata.as_ref().is_none_or(Map::is_empty)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PluginEvent {
    event_id: String,
    sequence: u64,
    event_type: String,
    status: String,
    occurred_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_tick: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision_id: Option<String>,
    actor: Actor,
    context: EventContext,
    data: EventData,
}

impl PluginEvent {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.event_id, 16, 128)?;
        if self.sequence > 9_007_199_254_740_991 {
            return Err("sequence_out_of_range");
        }
        if !EVENT_TYPES.contains(&self.event_type.as_str()) {
            return Err("unknown_event_type");
        }
        if !EVENT_STATUSES.contains(&self.status.as_str()) {
            return Err("unknown_event_status");
        }
        parse_time(&self.occurred_at).map_err(|_| "invalid_occurred_at")?;
        self.actor.validate()?;
        if self.context.is_empty() {
            return Err("empty_context");
        }
        if self.data.is_empty() {
            return Err("empty_data");
        }
        if self.event_type.starts_with("ledger.") {
            check_optional_required_len(&self.transaction_id, 128)?;
        }
        check_optional_len(&self.decision_id, 128)?;

        let currency_changes = self.data.currency_changes.as_deref().unwrap_or_default();
        if currency_changes.len() > 16 {
            return Err("too_many_currency_changes");
        }
        for change in currency_changes {
            change.validate()?;
        }

        let asset_changes = self.data.asset_changes.as_deref().unwrap_or_default();
        if asset_changes.len() > 200 {
            return Err("too_many_asset_changes");
        }
        for change in asset_changes {
            change.validate()?;
        }

        if let Some(state) = &self.data.player_state {
            state.validate()?;
        }
        if let Some(validation) = &self.data.validation {
            validation.validate()?;
        }
        if self
            .data
            .metadata
            .as_ref()
            .is_some_and(|value| value.len() > 32)
        {
            return Err("too_many_metadata_fields");
        }

        match self.event_type.as_str() {
            "ledger.currency_changed" if currency_changes.is_empty() => {
                Err("currency_changes_required")
            }
            "ledger.asset_created"
                if !asset_changes
                    .iter()
                    .any(|change| change.operation == "create") =>
            {
                Err("asset_create_required")
            }
            "ledger.asset_moved"
                if !asset_changes
                    .iter()
                    .any(|change| change.operation == "move") =>
            {
                Err("asset_move_required")
            }
            "ledger.asset_changed"
                if !asset_changes
                    .iter()
                    .any(|change| change.operation == "change") =>
            {
                Err("asset_change_required")
            }
            "ledger.asset_destroyed"
                if !asset_changes
                    .iter()
                    .any(|change| change.operation == "destroy") =>
            {
                Err("asset_destroy_required")
            }
            "ledger.reward_granted" | "ledger.trade_committed"
                if currency_changes.is_empty() && asset_changes.is_empty() =>
            {
                Err("ledger_changes_required")
            }
            "state.player_snapshot" if self.data.player_state.is_none() => {
                Err("player_state_required")
            }
            "security.validation_failed" if self.data.validation.is_none() => {
                Err("validation_required")
            }
            _ => Ok(()),
        }
    }
}

impl CurrencyChange {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.owner_id, 1, 128)?;
        if !matches!(
            self.currency.as_str(),
            "game_cash" | "gold_coin" | "silver_coin"
        ) {
            return Err("unknown_currency");
        }
        let before = parse_amount(&self.before)?;
        let after = parse_amount(&self.after)?;
        let delta = parse_amount(&self.delta)?;
        if before < 0 || after < 0 {
            return Err("negative_currency_balance");
        }
        if before.checked_add(delta) != Some(after) {
            return Err("currency_not_balanced");
        }
        check_optional_len(&self.balance_version, 128)?;
        Ok(())
    }
}

impl AssetChange {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.asset_id, 1, 256)?;
        check_len(&self.template_id, 1, 128)?;
        if !matches!(self.asset_kind.as_str(), "item" | "pet") {
            return Err("unknown_asset_kind");
        }
        if !matches!(
            self.operation.as_str(),
            "create" | "move" | "change" | "destroy"
        ) {
            return Err("unknown_asset_operation");
        }
        if self.quantity_before < 0 || self.quantity_after < 0 {
            return Err("negative_asset_quantity");
        }
        if self.slot_before.is_some_and(|value| value < -1)
            || self.slot_after.is_some_and(|value| value < -1)
        {
            return Err("invalid_asset_slot");
        }
        let owner_before = nonempty(self.owner_before.as_deref());
        let owner_after = nonempty(self.owner_after.as_deref());
        match self.operation.as_str() {
            "create"
                if self.quantity_before != 0
                    || self.quantity_after == 0
                    || owner_after.is_none() =>
            {
                return Err("invalid_asset_create_transition");
            }
            "move"
                if self.quantity_before == 0
                    || self.quantity_after == 0
                    || owner_before.is_none()
                    || owner_after.is_none() =>
            {
                return Err("invalid_asset_move_transition");
            }
            "destroy"
                if self.quantity_before == 0
                    || self.quantity_after != 0
                    || owner_before.is_none() =>
            {
                return Err("invalid_asset_destroy_transition");
            }
            _ => {}
        }
        Ok(())
    }
}

impl PlayerState {
    fn validate(&self) -> Result<(), &'static str> {
        if self.level < 0 {
            return Err("negative_player_level");
        }
        for amount in [
            &self.currencies.game_cash,
            &self.currencies.gold_coin,
            &self.currencies.silver_coin,
        ] {
            if parse_amount(amount)? < 0 {
                return Err("negative_currency_balance");
            }
        }
        Ok(())
    }
}

impl ValidationEvidence {
    fn validate(&self) -> Result<(), &'static str> {
        check_len(&self.rule_code, 1, 128)?;
        check_len(&self.message, 1, 512)?;
        if !matches!(
            self.severity.as_str(),
            "info" | "low" | "medium" | "high" | "critical"
        ) {
            return Err("unknown_validation_severity");
        }
        Ok(())
    }
}

fn check_len(value: &str, min: usize, max: usize) -> Result<(), &'static str> {
    if value.len() < min || value.len() > max {
        return Err("invalid_string_length");
    }
    Ok(())
}

fn check_optional_len(value: &Option<String>, max: usize) -> Result<(), &'static str> {
    if value.as_ref().is_some_and(|value| value.len() > max) {
        return Err("invalid_string_length");
    }
    Ok(())
}

fn check_optional_required_len(value: &Option<String>, max: usize) -> Result<(), &'static str> {
    match value {
        Some(value) => check_len(value, 1, max),
        None => Err("transaction_id_required"),
    }
}

fn parse_amount(value: &str) -> Result<i128, &'static str> {
    if value.is_empty()
        || value.starts_with('+')
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && byte != b'-')
        || value.matches('-').count() > usize::from(value.starts_with('-'))
    {
        return Err("invalid_decimal_amount");
    }
    value.parse::<i128>().map_err(|_| "amount_out_of_range")
}

fn parse_rule_amount(value: &str) -> Result<i128> {
    parse_amount(value).map_err(anyhow::Error::msg)
}

fn parse_time(value: &str) -> Result<DateTime<chrono::FixedOffset>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value)
}

fn forbidden_key(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if FORBIDDEN_PLUGIN_KEYS.contains(&key.as_str()) {
                    return Some(key);
                }
                if let Some(found) = forbidden_key(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(forbidden_key),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RejectedEvent {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_id: Option<String>,
    code: String,
    message: String,
    retryable: bool,
}

#[derive(Debug)]
struct ValidatedBatch {
    producer: Producer,
    events: Vec<PluginEvent>,
    rejected: Vec<RejectedEvent>,
}

fn validate_batch(body: &[u8]) -> Result<ValidatedBatch, ApiError> {
    if body.len() > MAX_EVENT_BODY_BYTES {
        return Err(ApiError::payload_too_large());
    }
    let envelope: BatchEnvelope = serde_json::from_slice(body).map_err(|_| {
        ApiError::bad_request("invalid_batch", "batch does not match the v1 contract")
    })?;
    if envelope.schema_version != SCHEMA_VERSION {
        return Err(ApiError::bad_request(
            "unsupported_schema",
            "only schema_version 1.0 is supported",
        ));
    }
    envelope
        .producer
        .validate()
        .map_err(|code| ApiError::bad_request(code, "producer fields are invalid"))?;
    parse_time(&envelope.sent_at).map_err(|_| {
        ApiError::bad_request("invalid_sent_at", "sent_at must be RFC3339 with timezone")
    })?;
    if envelope.events.is_empty() || envelope.events.len() > MAX_EVENTS {
        return Err(ApiError::bad_request(
            "invalid_event_count",
            "events must contain 1 to 200 entries",
        ));
    }

    let mut events = Vec::new();
    let mut rejected = Vec::new();
    let mut previous_sequence = None;
    for (index, raw) in envelope.events.into_iter().enumerate() {
        let event_id = safe_event_id(&raw);
        if let Some(key) = forbidden_key(&raw) {
            rejected.push(RejectedEvent {
                index,
                event_id,
                code: "forbidden_identity".to_string(),
                message: format!("plugin events cannot supply {key}"),
                retryable: false,
            });
            continue;
        }
        let event = match serde_json::from_value::<PluginEvent>(raw) {
            Ok(event) => event,
            Err(_) => {
                rejected.push(RejectedEvent {
                    index,
                    event_id,
                    code: "invalid_event".to_string(),
                    message: "event does not match the v1 contract".to_string(),
                    retryable: false,
                });
                continue;
            }
        };
        if let Err(code) = event.validate() {
            rejected.push(RejectedEvent {
                index,
                event_id: Some(event.event_id.clone()),
                code: code.to_string(),
                message: "event semantic validation failed".to_string(),
                retryable: false,
            });
            continue;
        }
        if previous_sequence.is_some_and(|previous| event.sequence <= previous) {
            rejected.push(RejectedEvent {
                index,
                event_id: Some(event.event_id.clone()),
                code: "sequence_not_increasing".to_string(),
                message: "sequence must be strictly increasing inside one batch".to_string(),
                retryable: false,
            });
            continue;
        }
        previous_sequence = Some(event.sequence);
        events.push(event);
    }
    Ok(ValidatedBatch {
        producer: envelope.producer,
        events,
        rejected,
    })
}

fn safe_event_id(value: &Value) -> Option<String> {
    value
        .get("event_id")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .map(str::to_string)
}

fn connect_queue(path: &Path) -> Result<Connection> {
    let connection =
        Connection::open(path).with_context(|| format!("open queue {}", path.display()))?;
    connection.busy_timeout(StdDuration::from_secs(3))?;
    Ok(connection)
}

fn prepare_queue(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create queue directory {}", parent.display()))?;
    }
    let connection = connect_queue(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.execute_batch(
        r#"
        create table if not exists agent_meta (
          key text primary key,
          value text not null
        );
        create table if not exists event_queue (
          tenant_id text not null,
          server_id text not null,
          event_id text not null,
          plugin_name text not null,
          plugin_version text not null,
          game_build text not null,
          boot_id text not null,
          sequence integer not null,
          event_type text not null,
          event_status text not null,
          occurred_at text not null,
          transaction_id text,
          actor_id text not null,
          payload_json text not null,
          received_at text not null,
          delivery_state text not null default 'pending',
          attempts integer not null default 0,
          next_attempt_at_ms integer not null default 0,
          lease_until_ms integer,
          last_error text,
          delivered_at text,
          primary key (tenant_id, server_id, event_id)
        );
        create index if not exists event_queue_delivery
          on event_queue(tenant_id, server_id, delivery_state, received_at);
        create index if not exists event_queue_actor_time
          on event_queue(tenant_id, server_id, actor_id, occurred_at);
        create table if not exists risk_alerts (
          tenant_id text not null,
          server_id text not null,
          alert_id text not null,
          actor_id text not null,
          event_id text,
          request_id text,
          rule_code text not null,
          category text not null,
          severity text not null,
          score integer not null,
          summary text not null,
          evidence_json text not null,
          occurred_at text not null,
          created_at text not null,
          status text not null default 'open',
          primary key (tenant_id, server_id, alert_id)
        );
        create index if not exists risk_alerts_status_time
          on risk_alerts(tenant_id, server_id, status, created_at desc);
        create index if not exists risk_alerts_actor_time
          on risk_alerts(tenant_id, server_id, actor_id, created_at desc);
        create table if not exists producer_sequence (
          tenant_id text not null,
          server_id text not null,
          plugin_name text not null,
          boot_id text not null,
          last_sequence integer not null,
          updated_at text not null,
          primary key (tenant_id, server_id, plugin_name, boot_id)
        );
        create table if not exists asset_state (
          tenant_id text not null,
          server_id text not null,
          asset_id text not null,
          owner_id text,
          quantity integer not null,
          last_event_id text not null,
          updated_at_ms integer not null,
          primary key (tenant_id, server_id, asset_id)
        );
        create table if not exists asset_activity (
          tenant_id text not null,
          server_id text not null,
          asset_id text not null,
          event_id text not null,
          actor_id text not null,
          operation text not null,
          occurred_at_ms integer not null,
          primary key (tenant_id, server_id, asset_id, event_id)
        );
        create index if not exists asset_activity_window
          on asset_activity(tenant_id, server_id, asset_id, operation, occurred_at_ms);
        create table if not exists currency_activity (
          tenant_id text not null,
          server_id text not null,
          event_id text not null,
          owner_id text not null,
          currency text not null,
          delta integer not null,
          occurred_at_ms integer not null,
          primary key (tenant_id, server_id, event_id, owner_id, currency)
        );
        create index if not exists currency_activity_window
          on currency_activity(tenant_id, server_id, owner_id, currency, occurred_at_ms);
        create table if not exists action_activity (
          tenant_id text not null,
          server_id text not null,
          event_id text not null,
          actor_id text not null,
          action_code text not null,
          occurred_at_ms integer not null,
          primary key (tenant_id, server_id, event_id)
        );
        create index if not exists action_activity_window
          on action_activity(tenant_id, server_id, actor_id, action_code, occurred_at_ms);
        create table if not exists player_snapshot (
          tenant_id text not null,
          server_id text not null,
          actor_id text not null,
          gold_coin integer not null,
          occurred_at_ms integer not null,
          event_id text not null,
          primary key (tenant_id, server_id, actor_id)
        );
        create table if not exists decision_log (
          tenant_id text not null,
          server_id text not null,
          request_id text not null,
          actor_id text not null,
          action_type text not null,
          transaction_id text not null,
          decision text not null,
          risk_score integer not null,
          rule_codes_json text not null,
          request_json text not null,
          response_json text not null,
          created_at text not null,
          primary key (tenant_id, server_id, request_id)
        );
        "#,
    )?;
    ensure_queue_column(
        &connection,
        "next_attempt_at_ms",
        "integer not null default 0",
    )?;
    ensure_queue_column(&connection, "lease_until_ms", "integer")?;
    ensure_queue_column(&connection, "last_error", "text")?;
    ensure_queue_column(&connection, "delivered_at", "text")?;
    Ok(())
}

fn ensure_queue_column(connection: &Connection, name: &str, definition: &str) -> Result<()> {
    let mut statement = connection.prepare("pragma table_info(event_queue)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    if !columns.contains(name) {
        connection.execute(
            &format!("alter table event_queue add column {name} {definition}"),
            [],
        )?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct IngestResponse {
    accepted: usize,
    duplicates: usize,
    rejected: Vec<RejectedEvent>,
    accepted_through_sequence: Option<u64>,
    queue_depth: i64,
    alerts_created: usize,
    rule_codes: Vec<String>,
}

#[derive(Debug, Clone)]
struct RuleHit {
    actor_id: Option<String>,
    rule_code: &'static str,
    category: &'static str,
    severity: &'static str,
    score: i64,
    summary: &'static str,
    evidence: Value,
}

impl RuleHit {
    fn event(
        rule_code: &'static str,
        category: &'static str,
        severity: &'static str,
        score: i64,
        summary: &'static str,
        evidence: Value,
    ) -> Self {
        Self {
            actor_id: None,
            rule_code,
            category,
            severity,
            score,
            summary,
            evidence,
        }
    }

    fn for_actor(mut self, actor_id: &str) -> Self {
        self.actor_id = Some(actor_id.to_string());
        self
    }
}

fn metadata_i64(metadata: Option<&Map<String, Value>>, key: &str) -> Option<i64> {
    let value = metadata?.get(key)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn insert_alert(
    transaction: &rusqlite::Transaction<'_>,
    config: &AgentConfig,
    event_id: Option<&str>,
    request_id: Option<&str>,
    default_actor_id: &str,
    occurred_at: &str,
    created_at: &str,
    hit: &RuleHit,
) -> Result<usize> {
    let actor_id = hit.actor_id.as_deref().unwrap_or(default_actor_id);
    let source_id = event_id.or(request_id).unwrap_or("unknown");
    let source_kind = if event_id.is_some() {
        "event"
    } else {
        "decision"
    };
    let alert_id = format!("{source_kind}:{source_id}:{}:{actor_id}", hit.rule_code);
    let evidence = serde_json::to_string(&hit.evidence)?;
    Ok(transaction.execute(
        "insert or ignore into risk_alerts(
           tenant_id,server_id,alert_id,actor_id,event_id,request_id,rule_code,category,
           severity,score,summary,evidence_json,occurred_at,created_at
         ) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            config.tenant_id,
            config.server_id,
            alert_id,
            actor_id,
            event_id,
            request_id,
            hit.rule_code,
            hit.category,
            hit.severity,
            hit.score,
            hit.summary,
            evidence,
            occurred_at,
            created_at,
        ],
    )?)
}

fn event_rule_hits(
    config: &AgentConfig,
    transaction: &rusqlite::Transaction<'_>,
    producer: &Producer,
    event: &PluginEvent,
    received_at: &str,
) -> Result<Vec<RuleHit>> {
    let occurred_at_ms = parse_time(&event.occurred_at)?.timestamp_millis();
    let mut hits = Vec::new();

    let last_sequence: Option<i64> = transaction
        .query_row(
            "select last_sequence from producer_sequence
             where tenant_id=?1 and server_id=?2 and plugin_name=?3 and boot_id=?4",
            params![
                config.tenant_id,
                config.server_id,
                producer.plugin_name,
                producer.boot_id
            ],
            |row| row.get(0),
        )
        .optional()?;
    let sequence_regressed = last_sequence.is_some_and(|last| event.sequence <= last as u64);
    if sequence_regressed {
        hits.push(RuleHit::event(
            "plugin_sequence_regression",
            "data_quality",
            "high",
            70,
            "Plugin event sequence moved backwards across batches",
            json!({ "last_sequence": last_sequence, "observed_sequence": event.sequence, "boot_id": producer.boot_id }),
        ));
    } else if last_sequence.is_some_and(|last| event.sequence > last as u64 + 1) {
        let expected = last_sequence.unwrap_or_default() as u64 + 1;
        hits.push(RuleHit::event(
            "plugin_sequence_gap",
            "data_quality",
            "medium",
            35,
            "Plugin event sequence contains a cross-batch gap",
            json!({ "expected_sequence": expected, "observed_sequence": event.sequence, "boot_id": producer.boot_id }),
        ));
    }
    transaction.execute(
        "insert into producer_sequence(tenant_id,server_id,plugin_name,boot_id,last_sequence,updated_at)
         values(?1,?2,?3,?4,?5,?6)
         on conflict(tenant_id,server_id,plugin_name,boot_id) do update set
           last_sequence=max(last_sequence,excluded.last_sequence), updated_at=excluded.updated_at",
        params![
            config.tenant_id,
            config.server_id,
            producer.plugin_name,
            producer.boot_id,
            event.sequence as i64,
            received_at,
        ],
    )?;

    if let Some(validation) = &event.data.validation {
        if matches!(validation.severity.as_str(), "high" | "critical") {
            hits.push(RuleHit::event(
                "server_validation_failed",
                "security",
                if validation.severity == "critical" {
                    "critical"
                } else {
                    "high"
                },
                if validation.severity == "critical" { 100 } else { 80 },
                "The game server rejected a high-risk operation",
                json!({ "validation_rule": validation.rule_code, "validation_severity": validation.severity }),
            ));
        }
    }

    if sequence_regressed {
        return Ok(hits);
    }
    if event.status != "succeeded" && event.event_type != "security.action_attempted" {
        return Ok(hits);
    }

    if event.event_type == "security.action_attempted" {
        if let Some(action_code) = event
            .context
            .action_code
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            transaction.execute(
                "insert or ignore into action_activity(
                   tenant_id,server_id,event_id,actor_id,action_code,occurred_at_ms
                 ) values(?1,?2,?3,?4,?5,?6)",
                params![
                    config.tenant_id,
                    config.server_id,
                    event.event_id,
                    event.actor.player_id,
                    action_code,
                    occurred_at_ms
                ],
            )?;
            let (count, first_at, last_at): (i64, i64, i64) = transaction.query_row(
                "select count(*),coalesce(min(occurred_at_ms),0),coalesce(max(occurred_at_ms),0)
                 from action_activity where tenant_id=?1 and server_id=?2 and actor_id=?3
                   and action_code=?4 and occurred_at_ms between ?5 and ?6",
                params![
                    config.tenant_id,
                    config.server_id,
                    event.actor.player_id,
                    action_code,
                    occurred_at_ms - RAPID_IDENTICAL_ACTION_WINDOW_MS,
                    occurred_at_ms
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            // ponytail: 固定窗口适合先抓脚本化洪泛；后续按玩法 action_code 配置基线和冷却时间。
            if count == RAPID_IDENTICAL_ACTION_COUNT {
                hits.push(RuleHit::event(
                    "rapid_identical_action",
                    "behavior",
                    "high",
                    75,
                    "One player repeated the same action too quickly",
                    json!({
                        "action_code": action_code,
                        "count": count,
                        "window_ms": last_at.saturating_sub(first_at),
                        "threshold_count": RAPID_IDENTICAL_ACTION_COUNT,
                        "threshold_window_ms": RAPID_IDENTICAL_ACTION_WINDOW_MS
                    }),
                ));
            }
        }
        return Ok(hits);
    }

    let metadata = event.data.metadata.as_ref();
    if event.event_type == "ledger.reward_granted" {
        let claim_count = metadata_i64(metadata, "daily_claim_count");
        let configured_max = metadata_i64(metadata, "configured_max_count");
        if matches!((claim_count, configured_max), (Some(actual), Some(maximum)) if actual > maximum)
        {
            hits.push(RuleHit::event(
                "reward_claim_limit_exceeded",
                "reward",
                "critical",
                100,
                "Reward claim count exceeds the configured maximum",
                json!({ "daily_claim_count": claim_count, "configured_max_count": configured_max, "source_id": event.context.source_id }),
            ));
        }
        if event.context.source_type.is_none()
            || event.context.source_id.is_none()
            || event.context.config_version.is_none()
        {
            hits.push(RuleHit::event(
                "reward_source_incomplete",
                "data_quality",
                "medium",
                45,
                "Reward event is missing source or configuration evidence",
                json!({
                    "has_source_type": event.context.source_type.is_some(),
                    "has_source_id": event.context.source_id.is_some(),
                    "has_config_version": event.context.config_version.is_some()
                }),
            ));
        }
    }

    if event.event_type == "ledger.trade_committed" {
        if metadata
            .and_then(|value| value.get("same_device"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            hits.push(RuleHit::event(
                "same_device_trade",
                "trade",
                "high",
                70,
                "Trade counterparties share the same device fingerprint",
                json!({ "counterparty_player_id": metadata.and_then(|value| value.get("counterparty_player_id")) }),
            ));
        }
        let mut totals: HashMap<&str, i128> = HashMap::new();
        for change in event.data.currency_changes.as_deref().unwrap_or_default() {
            *totals.entry(&change.currency).or_default() += parse_rule_amount(&change.delta)?;
        }
        let unbalanced: Map<String, Value> = totals
            .into_iter()
            .filter(|(_, total)| *total != 0)
            .map(|(currency, total)| (currency.to_string(), Value::String(total.to_string())))
            .collect();
        if !unbalanced.is_empty() {
            hits.push(RuleHit::event(
                "trade_currency_legs_unbalanced",
                "trade",
                "critical",
                95,
                "Trade currency legs do not balance to zero",
                Value::Object(unbalanced),
            ));
        }
    }

    for change in event.data.asset_changes.as_deref().unwrap_or_default() {
        let previous: Option<(Option<String>, i64, String)> = transaction
            .query_row(
                "select owner_id,quantity,last_event_id from asset_state
                 where tenant_id=?1 and server_id=?2 and asset_id=?3",
                params![config.tenant_id, config.server_id, change.asset_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if change.operation == "create" && previous.is_some() {
            hits.push(RuleHit::event(
                "duplicate_asset_create",
                "asset",
                "critical",
                100,
                "An existing asset ID was created again",
                json!({ "asset_id": change.asset_id, "previous_event_id": previous.as_ref().map(|value| &value.2) }),
            ));
        }
        if change.operation != "create" {
            if let Some((stored_owner, stored_quantity, previous_event_id)) = &previous {
                let reported_owner = nonempty(change.owner_before.as_deref());
                if reported_owner.is_some() && reported_owner != stored_owner.as_deref() {
                    hits.push(RuleHit::event(
                        "asset_owner_chain_mismatch",
                        "asset",
                        "critical",
                        95,
                        "Asset owner does not match the last committed state",
                        json!({
                            "asset_id": change.asset_id,
                            "stored_owner": stored_owner,
                            "reported_owner_before": reported_owner,
                            "stored_quantity": stored_quantity,
                            "previous_event_id": previous_event_id
                        }),
                    ));
                }
            }
        }

        transaction.execute(
            "insert or ignore into asset_activity(
               tenant_id,server_id,asset_id,event_id,actor_id,operation,occurred_at_ms
             ) values(?1,?2,?3,?4,?5,?6,?7)",
            params![
                config.tenant_id,
                config.server_id,
                change.asset_id,
                event.event_id,
                event.actor.player_id,
                change.operation,
                occurred_at_ms,
            ],
        )?;
        if change.operation == "move" {
            let move_count: i64 = transaction.query_row(
                "select count(*) from asset_activity
                 where tenant_id=?1 and server_id=?2 and asset_id=?3 and operation='move'
                   and occurred_at_ms between ?4 and ?5",
                params![
                    config.tenant_id,
                    config.server_id,
                    change.asset_id,
                    occurred_at_ms - TEN_MINUTES_MS,
                    occurred_at_ms,
                ],
                |row| row.get(0),
            )?;
            if move_count >= config.asset_moves_10m {
                hits.push(RuleHit::event(
                    "rapid_asset_transfer",
                    "asset",
                    "high",
                    80,
                    "One asset moved too frequently in ten minutes",
                    json!({ "asset_id": change.asset_id, "move_count_10m": move_count, "threshold": config.asset_moves_10m }),
                ));
            }
        }

        if change.operation == "destroy" {
            transaction.execute(
                "delete from asset_state where tenant_id=?1 and server_id=?2 and asset_id=?3",
                params![config.tenant_id, config.server_id, change.asset_id],
            )?;
        } else {
            let owner = nonempty(change.owner_after.as_deref())
                .or_else(|| nonempty(change.owner_before.as_deref()));
            transaction.execute(
                "insert into asset_state(
                   tenant_id,server_id,asset_id,owner_id,quantity,last_event_id,updated_at_ms
                 ) values(?1,?2,?3,?4,?5,?6,?7)
                 on conflict(tenant_id,server_id,asset_id) do update set
                   owner_id=excluded.owner_id,quantity=excluded.quantity,
                   last_event_id=excluded.last_event_id,updated_at_ms=excluded.updated_at_ms",
                params![
                    config.tenant_id,
                    config.server_id,
                    change.asset_id,
                    owner,
                    change.quantity_after,
                    event.event_id,
                    occurred_at_ms,
                ],
            )?;
        }
    }

    for change in event.data.currency_changes.as_deref().unwrap_or_default() {
        let delta = parse_rule_amount(&change.delta)?;
        let Ok(delta) = i64::try_from(delta) else {
            hits.push(
                RuleHit::event(
                    "currency_delta_storage_overflow",
                    "currency",
                    "critical",
                    100,
                    "Currency delta exceeds the realtime ledger range",
                    json!({ "currency": change.currency, "delta": change.delta }),
                )
                .for_actor(&change.owner_id),
            );
            continue;
        };
        if delta.unsigned_abs() > MAX_REALTIME_DELTA {
            hits.push(
                RuleHit::event(
                    "currency_delta_storage_overflow",
                    "currency",
                    "critical",
                    100,
                    "Currency delta exceeds the safe realtime aggregation range",
                    json!({ "currency": change.currency, "delta": change.delta }),
                )
                .for_actor(&change.owner_id),
            );
            continue;
        }
        transaction.execute(
            "insert or ignore into currency_activity(
               tenant_id,server_id,event_id,owner_id,currency,delta,occurred_at_ms
             ) values(?1,?2,?3,?4,?5,?6,?7)",
            params![
                config.tenant_id,
                config.server_id,
                event.event_id,
                change.owner_id,
                change.currency,
                delta,
                occurred_at_ms,
            ],
        )?;
        if change.currency == "gold_coin" && delta > 0 {
            let gain: i64 = transaction.query_row(
                "select coalesce(sum(delta),0) from currency_activity
                 where tenant_id=?1 and server_id=?2 and owner_id=?3 and currency='gold_coin'
                   and occurred_at_ms between ?4 and ?5",
                params![
                    config.tenant_id,
                    config.server_id,
                    change.owner_id,
                    occurred_at_ms - TEN_MINUTES_MS,
                    occurred_at_ms,
                ],
                |row| row.get(0),
            )?;
            if gain >= config.gold_gain_10m {
                hits.push(
                    RuleHit::event(
                        "rapid_gold_gain",
                        "currency",
                        "high",
                        85,
                        "Gold coin net gain is too high within ten minutes",
                        json!({ "gold_gain_10m": gain, "threshold": config.gold_gain_10m }),
                    )
                    .for_actor(&change.owner_id),
                );
            }
        }
    }

    if event.event_type == "state.player_snapshot" {
        if let Some(state) = &event.data.player_state {
            if let Ok(gold_coin) = parse_amount(&state.currencies.gold_coin)
                .and_then(|value| i64::try_from(value).map_err(|_| "amount_out_of_realtime_range"))
            {
                let previous: Option<(i64, i64, String)> = transaction
                    .query_row(
                        "select gold_coin,occurred_at_ms,event_id from player_snapshot
                         where tenant_id=?1 and server_id=?2 and actor_id=?3",
                        params![config.tenant_id, config.server_id, event.actor.player_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                if let Some((previous_gold, previous_at, previous_event)) = previous {
                    let jump = gold_coin.saturating_sub(previous_gold);
                    if jump >= config.gold_gain_10m {
                        let accounted: i64 = transaction.query_row(
                            "select coalesce(sum(delta),0) from currency_activity
                             where tenant_id=?1 and server_id=?2 and owner_id=?3 and currency='gold_coin'
                               and occurred_at_ms>?4 and occurred_at_ms<=?5",
                            params![
                                config.tenant_id,
                                config.server_id,
                                event.actor.player_id,
                                previous_at,
                                occurred_at_ms,
                            ],
                            |row| row.get(0),
                        )?;
                        if accounted != jump {
                            hits.push(RuleHit::event(
                                "unexplained_gold_snapshot_jump",
                                "currency",
                                "critical",
                                95,
                                "Gold snapshot increase is not explained by realtime ledger events",
                                json!({
                                    "previous_gold": previous_gold,
                                    "current_gold": gold_coin,
                                    "snapshot_jump": jump,
                                    "accounted_delta": accounted,
                                    "previous_event_id": previous_event
                                }),
                            ));
                        }
                    }
                }
                transaction.execute(
                    "insert into player_snapshot(
                       tenant_id,server_id,actor_id,gold_coin,occurred_at_ms,event_id
                     ) values(?1,?2,?3,?4,?5,?6)
                     on conflict(tenant_id,server_id,actor_id) do update set
                       gold_coin=excluded.gold_coin,occurred_at_ms=excluded.occurred_at_ms,
                       event_id=excluded.event_id",
                    params![
                        config.tenant_id,
                        config.server_id,
                        event.actor.player_id,
                        gold_coin,
                        occurred_at_ms,
                        event.event_id,
                    ],
                )?;
            }
        }
    }
    Ok(hits)
}

fn prune_realtime_activity(transaction: &rusqlite::Transaction<'_>, now_ms: i64) -> Result<()> {
    let last_prune = transaction
        .query_row(
            "select value from agent_meta where key='last_realtime_prune_ms'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    if now_ms.saturating_sub(last_prune) < ONE_HOUR_MS {
        return Ok(());
    }
    // ponytail: 24h exceeds the 10m rule window; move retention upstream when delivery exists.
    let cutoff = now_ms.saturating_sub(ACTIVITY_RETENTION_MS);
    transaction.execute(
        "delete from asset_activity where occurred_at_ms<?1",
        params![cutoff],
    )?;
    transaction.execute(
        "delete from currency_activity where occurred_at_ms<?1",
        params![cutoff],
    )?;
    transaction.execute(
        "delete from action_activity where occurred_at_ms<?1",
        params![cutoff],
    )?;
    transaction.execute(
        "insert into agent_meta(key,value) values('last_realtime_prune_ms',?1)
         on conflict(key) do update set value=excluded.value",
        params![now_ms.to_string()],
    )?;
    Ok(())
}

fn ingest(config: &AgentConfig, body: &[u8]) -> Result<IngestResponse, ApiError> {
    let batch = validate_batch(body)?;
    let mut connection = connect_queue(&config.queue_path).map_err(ApiError::queue_unavailable)?;
    let transaction = connection
        .transaction()
        .map_err(ApiError::queue_unavailable)?;
    let received_at = now_rfc3339();
    prune_realtime_activity(&transaction, Utc::now().timestamp_millis())
        .map_err(ApiError::queue_unavailable)?;
    let mut accepted = 0;
    let mut duplicates = 0;
    let mut accepted_through_sequence = None;
    let mut seen_in_batch = HashSet::new();
    let mut alerts_created = 0;
    let mut rule_codes = Vec::new();

    for event in &batch.events {
        if !seen_in_batch.insert(event.event_id.clone()) {
            duplicates += 1;
            continue;
        }
        let payload = serde_json::to_string(event).map_err(ApiError::queue_unavailable)?;
        let inserted = transaction
            .execute(
                "insert or ignore into event_queue(
                   tenant_id,server_id,event_id,plugin_name,plugin_version,game_build,boot_id,
                   sequence,event_type,event_status,occurred_at,transaction_id,actor_id,payload_json,received_at
                 ) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    config.tenant_id,
                    config.server_id,
                    event.event_id,
                    batch.producer.plugin_name,
                    batch.producer.plugin_version,
                    batch.producer.game_build,
                    batch.producer.boot_id,
                    event.sequence as i64,
                    event.event_type,
                    event.status,
                    event.occurred_at,
                    event.transaction_id,
                    event.actor.player_id,
                    payload,
                    received_at,
                ],
            )
            .map_err(ApiError::queue_unavailable)?;
        if inserted == 1 {
            accepted += 1;
            let hits = event_rule_hits(config, &transaction, &batch.producer, event, &received_at)
                .map_err(ApiError::queue_unavailable)?;
            for hit in hits {
                alerts_created += insert_alert(
                    &transaction,
                    config,
                    Some(&event.event_id),
                    None,
                    &event.actor.player_id,
                    &event.occurred_at,
                    &received_at,
                    &hit,
                )
                .map_err(ApiError::queue_unavailable)?;
                if !rule_codes.contains(&hit.rule_code.to_string()) {
                    rule_codes.push(hit.rule_code.to_string());
                }
            }
        } else {
            duplicates += 1;
        }
        accepted_through_sequence = Some(
            accepted_through_sequence
                .map_or(event.sequence, |current: u64| current.max(event.sequence)),
        );
    }

    transaction
        .execute(
            "insert into agent_meta(key,value) values('last_accepted_at',?1)
             on conflict(key) do update set value=excluded.value",
            params![received_at],
        )
        .map_err(ApiError::queue_unavailable)?;
    transaction.commit().map_err(ApiError::queue_unavailable)?;
    let queue_depth = queue_depth(&connection).map_err(ApiError::queue_unavailable)?;
    Ok(IngestResponse {
        accepted,
        duplicates,
        rejected: batch.rejected,
        accepted_through_sequence,
        queue_depth,
        alerts_created,
        rule_codes,
    })
}

fn queue_depth(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row(
        "select count(*) from event_queue where delivery_state in ('pending','retry','leased')",
        [],
        |row| row.get(0),
    )?)
}

#[derive(Debug)]
struct DeliveryEvent {
    tenant_id: String,
    server_id: String,
    event_id: String,
    plugin_name: String,
    plugin_version: String,
    game_build: String,
    boot_id: String,
    payload: Value,
    attempts: i64,
}

fn lease_delivery_batch(config: &AgentConfig) -> Result<Vec<DeliveryEvent>> {
    let now_ms = Utc::now().timestamp_millis();
    let mut connection = connect_queue(&config.queue_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let producer: Option<(String, String, String, String, String, String)> = transaction
        .query_row(
            "select tenant_id,server_id,plugin_name,plugin_version,game_build,boot_id
             from event_queue
             where next_attempt_at_ms<=?1
               and (delivery_state in ('pending','retry')
                    or (delivery_state='leased' and coalesce(lease_until_ms,0)<=?1))
             order by received_at,event_id limit 1",
            params![now_ms],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((tenant_id, server_id, plugin_name, plugin_version, game_build, boot_id)) = producer
    else {
        transaction.commit()?;
        return Ok(Vec::new());
    };

    let mut statement = transaction.prepare(
        "select event_id,payload_json,attempts
         from event_queue
         where tenant_id=?1 and server_id=?2 and plugin_name=?3 and plugin_version=?4
           and game_build=?5 and boot_id=?6 and next_attempt_at_ms<=?7
           and (delivery_state in ('pending','retry')
                or (delivery_state='leased' and coalesce(lease_until_ms,0)<=?7))
         order by received_at,event_id limit ?8",
    )?;
    let rows = statement
        .query_map(
            params![
                tenant_id,
                server_id,
                plugin_name,
                plugin_version,
                game_build,
                boot_id,
                now_ms,
                config.delivery_batch_size as i64,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let mut events = Vec::with_capacity(rows.len());
    for (event_id, payload_json, previous_attempts) in rows {
        let updated = transaction.execute(
            "update event_queue set delivery_state='leased',attempts=attempts+1,
               lease_until_ms=?1,last_error=null
             where tenant_id=?2 and server_id=?3 and event_id=?4
               and (delivery_state in ('pending','retry')
                    or (delivery_state='leased' and coalesce(lease_until_ms,0)<=?5))",
            params![
                now_ms + DELIVERY_LEASE_MS,
                tenant_id,
                server_id,
                event_id,
                now_ms,
            ],
        )?;
        if updated == 1 {
            events.push(DeliveryEvent {
                tenant_id: tenant_id.clone(),
                server_id: server_id.clone(),
                event_id,
                plugin_name: plugin_name.clone(),
                plugin_version: plugin_version.clone(),
                game_build: game_build.clone(),
                boot_id: boot_id.clone(),
                payload: serde_json::from_str(&payload_json)
                    .context("stored event JSON is invalid")?,
                attempts: previous_attempts + 1,
            });
        }
    }
    transaction.commit()?;
    Ok(events)
}

fn retry_delay_ms(attempts: i64, event_id: &str) -> i64 {
    let exponent = attempts.saturating_sub(1).min(6) as u32;
    let base = 5_000i64.saturating_mul(1i64 << exponent).min(300_000);
    let jitter = event_id
        .bytes()
        .fold(0u64, |sum, byte| sum.wrapping_add(byte as u64))
        % 1_000;
    base + jitter as i64
}

fn mark_delivery(
    connection: &Connection,
    event: &DeliveryEvent,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    let now = now_rfc3339();
    let next_attempt = if state == "retry" {
        Utc::now().timestamp_millis() + retry_delay_ms(event.attempts, &event.event_id)
    } else {
        0
    };
    let updated = connection.execute(
        "update event_queue set delivery_state=?1,next_attempt_at_ms=?2,lease_until_ms=null,
           last_error=?3,delivered_at=case when ?1='delivered' then ?4 else delivered_at end
         where tenant_id=?5 and server_id=?6 and event_id=?7 and delivery_state='leased'",
        params![
            state,
            next_attempt,
            error.map(|value| value.chars().take(500).collect::<String>()),
            now,
            event.tenant_id,
            event.server_id,
            event.event_id,
        ],
    )?;
    if updated != 1 {
        bail!("delivery lease was lost for event {}", event.event_id);
    }
    Ok(())
}

fn set_agent_meta(connection: &Connection, key: &str, value: &str) -> Result<()> {
    connection.execute(
        "insert into agent_meta(key,value) values(?1,?2)
         on conflict(key) do update set value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn record_delivery_error(config: &AgentConfig, message: &str) -> Result<()> {
    let connection = connect_queue(&config.queue_path)?;
    set_agent_meta(
        &connection,
        "last_delivery_error",
        &message.chars().take(500).collect::<String>(),
    )?;
    set_agent_meta(&connection, "last_delivery_error_at", &now_rfc3339())
}

fn fail_delivery_batch(
    config: &AgentConfig,
    events: &[DeliveryEvent],
    retryable: bool,
    message: &str,
) -> Result<()> {
    let mut connection = connect_queue(&config.queue_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for event in events {
        let state = if retryable && event.attempts < config.delivery_max_attempts {
            "retry"
        } else {
            "dead"
        };
        mark_delivery(&transaction, event, state, Some(message))?;
    }
    let message = message.chars().take(500).collect::<String>();
    set_agent_meta(&transaction, "last_delivery_error", &message)?;
    set_agent_meta(&transaction, "last_delivery_error_at", &now_rfc3339())?;
    transaction.commit()?;
    Ok(())
}

fn delivery_ack_rejections(
    payload: &Value,
    events: &[DeliveryEvent],
) -> Result<HashMap<String, (bool, String)>> {
    let accepted = payload
        .get("accepted")
        .and_then(Value::as_u64)
        .context("upstream ACK is missing accepted")? as usize;
    let duplicates = payload
        .get("duplicates")
        .and_then(Value::as_u64)
        .context("upstream ACK is missing duplicates")? as usize;
    let rejected = payload
        .get("rejected")
        .and_then(Value::as_array)
        .context("upstream ACK is missing rejected")?;
    if accepted
        .checked_add(duplicates)
        .and_then(|count| count.checked_add(rejected.len()))
        != Some(events.len())
    {
        bail!("upstream ACK count does not match leased batch");
    }

    let leased_ids = events
        .iter()
        .map(|event| event.event_id.as_str())
        .collect::<HashSet<_>>();
    let mut rejected_by_id = HashMap::new();
    for rejection in rejected {
        let event_id = rejection
            .get("event_id")
            .and_then(Value::as_str)
            .context("upstream rejection is missing event_id")?;
        if !leased_ids.contains(event_id) {
            bail!("upstream rejected an event outside the leased batch");
        }
        let retryable = rejection
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let message = rejection
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream rejected event")
            .chars()
            .take(500)
            .collect::<String>();
        if rejected_by_id
            .insert(event_id.to_string(), (retryable, message))
            .is_some()
        {
            bail!("upstream ACK contains a duplicate rejection");
        }
    }
    Ok(rejected_by_id)
}

fn deliver_once(config: &AgentConfig) -> Result<usize> {
    let (Some(url), Some(token)) = (&config.upstream_url, &config.upstream_token) else {
        return Ok(0);
    };
    let events = lease_delivery_batch(config)?;
    if events.is_empty() {
        return Ok(0);
    }
    let first = &events[0];
    let body = json!({
        "schema_version": SCHEMA_VERSION,
        "producer": {
            "plugin_name": first.plugin_name,
            "plugin_version": first.plugin_version,
            "game_build": first.game_build,
            "boot_id": first.boot_id,
        },
        "sent_at": now_rfc3339(),
        "events": events.iter().map(|event| &event.payload).collect::<Vec<_>>(),
    })
    .to_string();
    let response = ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(5))
        .timeout_read(StdDuration::from_secs(15))
        .timeout_write(StdDuration::from_secs(15))
        .build()
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&body);

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(status, _)) => {
            let retryable = matches!(status, 408 | 425 | 429) || status >= 500;
            let message = format!("upstream returned HTTP {status}");
            fail_delivery_batch(config, &events, retryable, &message)?;
            return Ok(events.len());
        }
        Err(error) => {
            let message = format!("upstream request failed: {error}");
            fail_delivery_batch(config, &events, true, &message)?;
            return Ok(events.len());
        }
    };
    let mut response_body = Vec::new();
    if let Err(error) = response
        .into_reader()
        .take((MAX_UPSTREAM_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response_body)
    {
        let message = format!("read upstream response: {error}");
        fail_delivery_batch(config, &events, true, &message)?;
        return Ok(events.len());
    }
    if response_body.len() > MAX_UPSTREAM_RESPONSE_BYTES {
        fail_delivery_batch(config, &events, true, "upstream response is too large")?;
        return Ok(events.len());
    }
    let payload: Value = match serde_json::from_slice(&response_body) {
        Ok(payload) => payload,
        Err(_) => {
            fail_delivery_batch(config, &events, true, "upstream response is not JSON")?;
            return Ok(events.len());
        }
    };
    let rejected_by_id = match delivery_ack_rejections(&payload, &events) {
        Ok(rejected) => rejected,
        Err(error) => {
            fail_delivery_batch(config, &events, true, &error.to_string())?;
            return Ok(events.len());
        }
    };
    let mut connection = connect_queue(&config.queue_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for event in &events {
        if let Some((retryable, message)) = rejected_by_id.get(&event.event_id) {
            let state = if *retryable && event.attempts < config.delivery_max_attempts {
                "retry"
            } else {
                "dead"
            };
            mark_delivery(&transaction, event, state, Some(message))?;
        } else {
            mark_delivery(&transaction, event, "delivered", None)?;
        }
    }
    set_agent_meta(&transaction, "last_delivery_at", &now_rfc3339())?;
    if rejected_by_id.is_empty() {
        set_agent_meta(&transaction, "last_delivery_error", "")?;
    } else {
        let message = format!("upstream rejected {} event(s)", rejected_by_id.len());
        set_agent_meta(&transaction, "last_delivery_error", &message)?;
        set_agent_meta(&transaction, "last_delivery_error_at", &now_rfc3339())?;
    }
    transaction.commit()?;
    Ok(events.len())
}

fn delivery_metrics(connection: &Connection) -> Result<Value> {
    let (pending, leased, dead, delivered, attempts, attempted_events, oldest): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        Option<String>,
    ) = connection.query_row(
        "select
               coalesce(sum(case when delivery_state in ('pending','retry') then 1 else 0 end),0),
               coalesce(sum(case when delivery_state='leased' then 1 else 0 end),0),
               coalesce(sum(case when delivery_state='dead' then 1 else 0 end),0),
               coalesce(sum(case when delivery_state='delivered' then 1 else 0 end),0),
               coalesce(sum(attempts),0),
               coalesce(sum(case when attempts>0 then 1 else 0 end),0),
               min(case when delivery_state in ('pending','retry','leased') then received_at end)
             from event_queue",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let meta = |key: &str| -> Result<Option<String>> {
        Ok(connection
            .query_row(
                "select value from agent_meta where key=?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    };
    Ok(json!({
        "pending": pending,
        "leased": leased,
        "dead": dead,
        "delivered": delivered,
        "delivery_attempts": attempts,
        "retry_attempts": (attempts - attempted_events).max(0),
        "oldest_pending_at": oldest,
        "last_delivery_at": meta("last_delivery_at")?,
        "last_delivery_error": meta("last_delivery_error")?.filter(|value| !value.is_empty()),
        "last_delivery_error_at": meta("last_delivery_error_at")?,
    }))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedChanges {
    #[serde(default)]
    currency_changes: Vec<CurrencyChange>,
    #[serde(default)]
    asset_changes: Vec<AssetChange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest {
    schema_version: String,
    request_id: String,
    occurred_at: String,
    action_type: String,
    transaction_id: String,
    actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    counterparty: Option<Actor>,
    proposed_changes: ProposedChanges,
    timeout_ms: u32,
}

fn push_unique_hit(hits: &mut Vec<RuleHit>, hit: RuleHit) {
    if !hits
        .iter()
        .any(|existing| existing.rule_code == hit.rule_code)
    {
        hits.push(hit);
    }
}

fn decision_rule_hits(config: &AgentConfig, request: &DecisionRequest) -> Result<Vec<RuleHit>> {
    let mut hits = Vec::new();
    let threshold = i128::from(config.high_value_gold);
    for change in &request.proposed_changes.currency_changes {
        let delta = parse_rule_amount(&change.delta)?;
        if change.currency == "gold_coin" && delta.unsigned_abs() >= threshold as u128 {
            push_unique_hit(
                &mut hits,
                RuleHit::event(
                    "high_value_gold_change",
                    "currency",
                    "high",
                    75,
                    "Proposed gold coin change exceeds the realtime review threshold",
                    json!({ "threshold": config.high_value_gold, "owner_id": change.owner_id, "delta": change.delta }),
                ),
            );
        }
    }

    for change in &request.proposed_changes.asset_changes {
        let quantity_delta = i128::from(change.quantity_after) - i128::from(change.quantity_before);
        if quantity_delta.unsigned_abs() >= config.high_value_asset_quantity as u128 {
            push_unique_hit(
                &mut hits,
                RuleHit::event(
                    "large_asset_quantity_change",
                    "asset",
                    "high",
                    70,
                    "Proposed asset quantity change exceeds the realtime review threshold",
                    json!({ "threshold": config.high_value_asset_quantity, "asset_id": change.asset_id, "quantity_delta": quantity_delta.to_string() }),
                ),
            );
        }
        let owner_before = nonempty(change.owner_before.as_deref());
        let owner_after = nonempty(change.owner_after.as_deref());
        if owner_before.is_some() && owner_after.is_some() && owner_before != owner_after {
            push_unique_hit(
                &mut hits,
                RuleHit::event(
                    "cross_player_asset_transfer",
                    "asset",
                    "medium",
                    55,
                    "Proposed operation transfers an asset between players",
                    json!({ "asset_id": change.asset_id, "owner_before": owner_before, "owner_after": owner_after }),
                ),
            );
        }
    }

    if let Some(counterparty) = &request.counterparty {
        let same_device = request
            .actor
            .device_fingerprint
            .as_deref()
            .zip(counterparty.device_fingerprint.as_deref())
            .is_some_and(|(left, right)| !left.is_empty() && left == right);
        if same_device && request.actor.player_id != counterparty.player_id {
            push_unique_hit(
                &mut hits,
                RuleHit::event(
                    "same_device_counterparty",
                    "trade",
                    "high",
                    80,
                    "Operation counterparties share the same device fingerprint",
                    json!({ "counterparty_player_id": counterparty.player_id }),
                ),
            );
        }
    }

    if request.action_type.contains("trade") {
        let mut totals: HashMap<&str, i128> = HashMap::new();
        for change in &request.proposed_changes.currency_changes {
            *totals.entry(&change.currency).or_default() += parse_rule_amount(&change.delta)?;
        }
        let unbalanced: Map<String, Value> = totals
            .into_iter()
            .filter(|(_, total)| *total != 0)
            .map(|(currency, total)| (currency.to_string(), Value::String(total.to_string())))
            .collect();
        if !unbalanced.is_empty() {
            push_unique_hit(
                &mut hits,
                RuleHit::event(
                    "trade_currency_legs_unbalanced",
                    "trade",
                    "critical",
                    95,
                    "Proposed trade currency legs do not balance to zero",
                    Value::Object(unbalanced),
                ),
            );
        }
    }
    Ok(hits)
}

fn decide(config: &AgentConfig, body: &[u8]) -> Result<Value, ApiError> {
    if body.len() > MAX_DECISION_BODY_BYTES {
        return Err(ApiError::payload_too_large());
    }
    let raw: Value = serde_json::from_slice(body).map_err(|_| {
        ApiError::bad_request("invalid_decision_request", "invalid decision request")
    })?;
    if let Some(key) = forbidden_key(&raw) {
        return Err(ApiError::bad_request(
            "forbidden_identity",
            &format!("plugin requests cannot supply {key}"),
        ));
    }
    let request: DecisionRequest = serde_json::from_value(raw).map_err(|_| {
        ApiError::bad_request(
            "invalid_decision_request",
            "request does not match v1 contract",
        )
    })?;
    if request.schema_version != SCHEMA_VERSION
        || request.request_id.is_empty()
        || request.request_id.len() > 128
        || request.action_type.is_empty()
        || request.action_type.len() > 128
        || request.transaction_id.is_empty()
        || request.transaction_id.len() > 128
        || request.timeout_ms == 0
        || request.timeout_ms > 100
        || parse_time(&request.occurred_at).is_err()
    {
        return Err(ApiError::bad_request(
            "invalid_decision_request",
            "decision fields are invalid",
        ));
    }
    request
        .actor
        .validate()
        .map_err(|code| ApiError::bad_request(code, "decision actor is invalid"))?;
    if let Some(counterparty) = &request.counterparty {
        counterparty
            .validate()
            .map_err(|code| ApiError::bad_request(code, "decision counterparty is invalid"))?;
    }
    for change in &request.proposed_changes.currency_changes {
        change
            .validate()
            .map_err(|code| ApiError::bad_request(code, "currency proposal is invalid"))?;
    }
    for change in &request.proposed_changes.asset_changes {
        change
            .validate()
            .map_err(|code| ApiError::bad_request(code, "asset proposal is invalid"))?;
    }
    let mut connection = connect_queue(&config.queue_path).map_err(ApiError::queue_unavailable)?;
    let existing: Option<String> = connection
        .query_row(
            "select response_json from decision_log
             where tenant_id=?1 and server_id=?2 and request_id=?3",
            params![config.tenant_id, config.server_id, request.request_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(ApiError::queue_unavailable)?;
    if let Some(existing) = existing {
        return serde_json::from_str(&existing)
            .map_err(|error| ApiError::queue_unavailable(format!("stored decision: {error}")));
    }

    let hits = decision_rule_hits(config, &request).map_err(ApiError::queue_unavailable)?;
    let risk_score = hits.iter().map(|hit| hit.score).max().unwrap_or(0);
    let rule_codes: Vec<&str> = hits.iter().map(|hit| hit.rule_code).collect();
    let reasons: Vec<&str> = hits.iter().map(|hit| hit.summary).collect();
    let decision = if hits.is_empty() { "allow" } else { "review" };
    let expires_at =
        (Utc::now() + Duration::seconds(2)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let response = json!({
        "decision_id": format!("shadow:{}", request.request_id),
        "mode": config.mode,
        "decision": decision,
        "risk_score": risk_score,
        "rule_codes": rule_codes,
        "reasons": reasons,
        "expires_at": expires_at,
    });
    let created_at = now_rfc3339();
    let request_json = serde_json::to_string(&request).map_err(ApiError::queue_unavailable)?;
    let response_json = serde_json::to_string(&response).map_err(ApiError::queue_unavailable)?;
    let transaction = connection
        .transaction()
        .map_err(ApiError::queue_unavailable)?;
    for hit in &hits {
        insert_alert(
            &transaction,
            config,
            None,
            Some(&request.request_id),
            &request.actor.player_id,
            &request.occurred_at,
            &created_at,
            hit,
        )
        .map_err(ApiError::queue_unavailable)?;
    }
    transaction
        .execute(
            "insert or ignore into decision_log(
               tenant_id,server_id,request_id,actor_id,action_type,transaction_id,decision,
               risk_score,rule_codes_json,request_json,response_json,created_at
             ) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                config.tenant_id,
                config.server_id,
                request.request_id,
                request.actor.player_id,
                request.action_type,
                request.transaction_id,
                decision,
                risk_score,
                serde_json::to_string(&rule_codes).map_err(ApiError::queue_unavailable)?,
                request_json,
                response_json,
                created_at,
            ],
        )
        .map_err(ApiError::queue_unavailable)?;
    transaction.commit().map_err(ApiError::queue_unavailable)?;
    Ok(response)
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Value,
}

impl HttpResponse {
    fn json(status: u16, body: Value) -> Self {
        Self { status, body }
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: String,
    message: String,
}

impl ApiError {
    fn bad_request(code: &str, message: &str) -> Self {
        Self {
            status: 400,
            code: code.to_string(),
            message: message.to_string(),
        }
    }

    fn payload_too_large() -> Self {
        Self {
            status: 413,
            code: "payload_too_large".to_string(),
            message: "request body is too large".to_string(),
        }
    }

    fn queue_unavailable(error: impl std::fmt::Display) -> Self {
        eprintln!("risk-agent queue: {error}");
        Self {
            status: 503,
            code: "queue_unavailable".to_string(),
            message: "persistent queue is unavailable".to_string(),
        }
    }

    fn into_response(self) -> HttpResponse {
        HttpResponse::json(
            self.status,
            json!({ "error": self.message, "code": self.code }),
        )
    }
}

fn open_alert_count(connection: &Connection, config: &AgentConfig) -> Result<i64> {
    Ok(connection.query_row(
        "select count(*) from risk_alerts
         where tenant_id=?1 and server_id=?2 and status='open'",
        params![config.tenant_id, config.server_id],
        |row| row.get(0),
    )?)
}

fn recent_alerts(config: &AgentConfig) -> Result<Value> {
    let connection = connect_queue(&config.queue_path)?;
    // ponytail: fixed latest 100 is enough locally; add cursor paging in the Go control plane.
    let mut statement = connection.prepare(
        "select alert_id,actor_id,event_id,request_id,rule_code,category,severity,score,
                summary,evidence_json,occurred_at,created_at,status
         from risk_alerts
         where tenant_id=?1 and server_id=?2
         order by created_at desc, alert_id desc limit 100",
    )?;
    let rows = statement.query_map(params![config.tenant_id, config.server_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;
    let mut alerts = Vec::new();
    for row in rows {
        let (
            alert_id,
            actor_id,
            event_id,
            request_id,
            rule_code,
            category,
            severity,
            score,
            summary,
            evidence_json,
            occurred_at,
            created_at,
            status,
        ) = row?;
        let evidence = serde_json::from_str(&evidence_json).unwrap_or(Value::Null);
        alerts.push(json!({
            "alert_id": alert_id,
            "actor_id": actor_id,
            "event_id": event_id,
            "request_id": request_id,
            "rule_code": rule_code,
            "category": category,
            "severity": severity,
            "score": score,
            "summary": summary,
            "evidence": evidence,
            "occurred_at": occurred_at,
            "created_at": created_at,
            "status": status,
        }));
    }
    let open = open_alert_count(&connection, config)?;
    let returned = alerts.len();
    Ok(json!({ "alerts": alerts, "returned": returned, "open": open }))
}

fn route(config: &AgentConfig, request: HttpRequest) -> HttpResponse {
    if request.method == "GET" && request.path == "/agent/v1/health" {
        return match health(config) {
            Ok(body) => HttpResponse::json(200, body),
            Err(error) => ApiError::queue_unavailable(error).into_response(),
        };
    }

    let token = request
        .headers
        .get("x-pgr-local-token")
        .map(String::as_bytes)
        .unwrap_or_default();
    if !constant_time_equal(token, config.local_token.as_bytes()) {
        return HttpResponse::json(
            401,
            json!({ "error": "unauthorized", "code": "unauthorized" }),
        );
    }
    let tenant_header = request.headers.get("x-pgr-tenant-id");
    let server_header = request.headers.get("x-pgr-server-id");
    let mut scoped_config = config.clone();
    match (tenant_header, server_header) {
        (Some(tenant_id), Some(server_id)) => {
            if check_identifier(tenant_id, "X-PGR-Tenant-Id").is_err()
                || check_identifier(server_id, "X-PGR-Server-Id").is_err()
            {
                return HttpResponse::json(
                    400,
                    json!({ "error": "invalid platform identity", "code": "invalid_identity" }),
                );
            }
            scoped_config.tenant_id.clone_from(tenant_id);
            scoped_config.server_id.clone_from(server_id);
        }
        (None, None) => {}
        _ => {
            return HttpResponse::json(
                400,
                json!({ "error": "tenant and server headers must be sent together", "code": "invalid_identity" }),
            );
        }
    }
    let config = &scoped_config;
    if request.method == "GET" && request.path == "/agent/v1/alerts" {
        return match recent_alerts(config) {
            Ok(body) => HttpResponse::json(200, body),
            Err(error) => ApiError::queue_unavailable(error).into_response(),
        };
    }
    if request.method != "POST" {
        return HttpResponse::json(404, json!({ "error": "not found", "code": "not_found" }));
    }
    if !request.headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|value| value.trim() == "application/json")
    }) {
        return HttpResponse::json(
            415,
            json!({ "error": "application/json required", "code": "unsupported_media_type" }),
        );
    }

    match request.path.as_str() {
        "/agent/v1/events:batch" => match ingest(config, &request.body) {
            Ok(result) => HttpResponse::json(
                200,
                serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            ),
            Err(error) => error.into_response(),
        },
        "/agent/v1/decisions:check" => match decide(config, &request.body) {
            Ok(result) => HttpResponse::json(200, result),
            Err(error) => error.into_response(),
        },
        "/agent/v1/flush" => {
            let delivery = deliver_once(&scoped_config);
            match delivery.and_then(|processed| {
                let db = connect_queue(&config.queue_path)?;
                Ok((processed, queue_depth(&db)?, delivery_metrics(&db)?))
            }) {
                Ok((processed, depth, metrics)) => HttpResponse::json(
                    200,
                    json!({
                        "ok": true,
                        "queue_depth": depth,
                        "processed": processed,
                        "upstream_configured": config.upstream_url.is_some(),
                        "delivery": metrics,
                        "message": if config.upstream_url.is_some() {
                            "delivery cycle completed"
                        } else {
                            "events are durable locally; upstream delivery is not configured"
                        }
                    }),
                ),
                Err(error) => ApiError::queue_unavailable(error).into_response(),
            }
        }
        _ => HttpResponse::json(404, json!({ "error": "not found", "code": "not_found" })),
    }
}

fn health(config: &AgentConfig) -> Result<Value> {
    let connection = connect_queue(&config.queue_path)?;
    let depth = queue_depth(&connection)?;
    let delivery = delivery_metrics(&connection)?;
    let open_alerts = open_alert_count(&connection, config)?;
    let last_accepted_at: Option<String> = connection
        .query_row(
            "select value from agent_meta where key='last_accepted_at'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "mode": config.mode,
        "bind": format!("127.0.0.1:{}", config.port),
        "server_id": config.server_id,
        "schema_versions": [SCHEMA_VERSION],
        "queue_depth": depth,
        "open_alerts": open_alerts,
        "last_accepted_at": last_accepted_at,
        "delivery": delivery,
        "realtime_rules": [
            "plugin_sequence_gap",
            "plugin_sequence_regression",
            "server_validation_failed",
            "reward_claim_limit_exceeded",
            "reward_source_incomplete",
            "same_device_trade",
            "trade_currency_legs_unbalanced",
            "duplicate_asset_create",
            "asset_owner_chain_mismatch",
            "rapid_asset_transfer",
            "rapid_gold_gain",
            "unexplained_gold_snapshot_jump",
            "high_value_gold_change",
            "large_asset_quantity_change",
            "cross_player_asset_transfer",
            "same_device_counterparty"
            ,"rapid_identical_action"
        ],
        "upstream_configured": config.upstream_url.is_some(),
    }))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, ApiError> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end = loop {
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(ApiError {
                status: 431,
                code: "headers_too_large".to_string(),
                message: "request headers are too large".to_string(),
            });
        }
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0u8; 4096];
        let read = stream
            .read(&mut chunk)
            .map_err(|_| ApiError::bad_request("read_failed", "request read failed"))?;
        if read == 0 {
            return Err(ApiError::bad_request(
                "incomplete_request",
                "request is incomplete",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let header_text = std::str::from_utf8(&buffer[..header_end - 4])
        .map_err(|_| ApiError::bad_request("invalid_headers", "headers must be ASCII/UTF-8"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid_request_line", "request line is missing"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !matches!(method.as_str(), "GET" | "POST")
        || target.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(ApiError::bad_request(
            "invalid_request_line",
            "request line is invalid",
        ));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ApiError::bad_request("invalid_headers", "header is invalid"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || headers.insert(name, value.trim().to_string()).is_some() {
            return Err(ApiError::bad_request(
                "invalid_headers",
                "duplicate or empty header",
            ));
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(ApiError::bad_request(
            "transfer_encoding_unsupported",
            "transfer-encoding is not supported",
        ));
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                ApiError::bad_request("invalid_content_length", "content-length is invalid")
            })
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_EVENT_BODY_BYTES {
        return Err(ApiError::payload_too_large());
    }
    while buffer.len() - header_end < content_length {
        let remaining = content_length - (buffer.len() - header_end);
        let mut chunk = vec![0u8; remaining.min(8192)];
        let read = stream
            .read(&mut chunk)
            .map_err(|_| ApiError::bad_request("read_failed", "request read failed"))?;
        if read == 0 {
            return Err(ApiError::bad_request(
                "incomplete_body",
                "request body is incomplete",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(HttpRequest {
        method,
        path: target.split('?').next().unwrap_or(&target).to_string(),
        headers,
        body: buffer[header_end..header_end + content_length].to_vec(),
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    let body = serde_json::to_vec(&response.body)?;
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response.status,
        reason,
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn handle_connection(mut stream: TcpStream, config: &AgentConfig) -> Result<()> {
    stream.set_read_timeout(Some(StdDuration::from_secs(5)))?;
    stream.set_write_timeout(Some(StdDuration::from_secs(5)))?;
    let response = match read_request(&mut stream) {
        Ok(request) => route(config, request),
        Err(error) => error.into_response(),
    };
    write_response(&mut stream, response)
}

struct ActiveConnectionGuard(Arc<AtomicUsize>);

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve(config: AgentConfig) -> Result<()> {
    prepare_queue(&config.queue_path)?;
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port);
    let listener = TcpListener::bind(address).with_context(|| format!("bind {address}"))?;
    println!(
        "risk-agent listening on http://{} (mode={}, queue={})",
        address,
        config.mode,
        config.queue_path.display()
    );
    let config = Arc::new(config);
    if config.upstream_url.is_some() {
        let delivery_config = Arc::clone(&config);
        thread::Builder::new()
            .name("pgr-delivery".to_string())
            .spawn(move || loop {
                if let Err(error) = deliver_once(&delivery_config) {
                    eprintln!("risk-agent delivery: {error:#}");
                    let _ = record_delivery_error(&delivery_config, &error.to_string());
                }
                thread::sleep(StdDuration::from_millis(DELIVERY_IDLE_SLEEP_MS));
            })?;
    }
    let active_connections = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                let peer = stream.peer_addr().ok();
                if active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_ACTIVE_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    let response = HttpResponse::json(
                        503,
                        json!({ "error": "agent is busy", "code": "too_many_connections" }),
                    );
                    let _ = write_response(&mut stream, response);
                    continue;
                }
                let config = Arc::clone(&config);
                let active_for_thread = Arc::clone(&active_connections);
                if let Err(error) =
                    thread::Builder::new()
                        .name("pgr-http".to_string())
                        .spawn(move || {
                            let _guard = ActiveConnectionGuard(active_for_thread);
                            if let Err(error) = handle_connection(stream, &config) {
                                eprintln!("risk-agent connection {:?}: {}", peer, error);
                            }
                        })
                {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("risk-agent spawn: {error}");
                }
            }
            Err(error) => eprintln!("risk-agent accept: {error}"),
        }
    }
    Ok(())
}

fn self_check() -> Result<usize> {
    let unique = format!(
        "risk-agent-self-check-{}-{}.db",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let path = env::temp_dir().join(unique);
    let config = AgentConfig {
        tenant_id: "tenant-self-check".to_string(),
        server_id: "server-self-check".to_string(),
        local_token: "self-check-token-that-is-longer-than-32-bytes".to_string(),
        port: DEFAULT_PORT,
        queue_path: path.clone(),
        mode: "shadow".to_string(),
        gold_gain_10m: DEFAULT_GOLD_GAIN_10M,
        asset_moves_10m: DEFAULT_ASSET_MOVES_10M,
        high_value_gold: DEFAULT_HIGH_VALUE_GOLD,
        high_value_asset_quantity: DEFAULT_HIGH_VALUE_ASSET_QUANTITY,
        upstream_url: None,
        upstream_token: None,
        delivery_batch_size: DEFAULT_DELIVERY_BATCH_SIZE,
        delivery_max_attempts: DEFAULT_DELIVERY_MAX_ATTEMPTS,
    };
    prepare_queue(&path)?;

    let body = self_check_batch("100", "1000", "1100", "100");
    let first = ingest(&config, body.as_bytes()).map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(first.accepted, 1);
    assert_eq!(first.duplicates, 0);
    assert_eq!(first.queue_depth, 1);

    let second =
        ingest(&config, body.as_bytes()).map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(second.accepted, 0);
    assert_eq!(second.duplicates, 1);
    assert_eq!(second.queue_depth, 1);

    let unbalanced = self_check_batch("101", "1100", "1200", "99");
    let rejected =
        ingest(&config, unbalanced.as_bytes()).map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(rejected.accepted, 0);
    assert_eq!(rejected.rejected.len(), 1);
    assert_eq!(rejected.rejected[0].code, "currency_not_balanced");

    let forbidden = self_check_batch("102", "1100", "1200", "100")
        .replace("\"data\":", "\"tenant_id\":\"spoofed\",\"data\":");
    let rejected =
        ingest(&config, forbidden.as_bytes()).map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(rejected.rejected[0].code, "forbidden_identity");

    let decision_body = json!({
        "schema_version": "1.0",
        "request_id": "self-check-high-value-decision",
        "occurred_at": "2026-07-31T00:01:00+08:00",
        "action_type": "trade.commit",
        "transaction_id": "self-check-trade",
        "timeout_ms": 20,
        "actor": {
            "player_id": "player-1",
            "device_fingerprint": "hmac-sha256:self-check-device"
        },
        "counterparty": {
            "player_id": "player-2",
            "device_fingerprint": "hmac-sha256:self-check-device"
        },
        "proposed_changes": { "currency_changes": [
            { "owner_id": "player-1", "currency": "gold_coin", "before": "0", "after": "2000000", "delta": "2000000" },
            { "owner_id": "player-2", "currency": "gold_coin", "before": "2000000", "after": "0", "delta": "-2000000" }
        ], "asset_changes": [] }
    })
    .to_string();
    let review = decide(&config, decision_body.as_bytes())
        .map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(review["decision"], "review");
    assert_eq!(review["risk_score"], 80);
    let cached = decide(&config, decision_body.as_bytes())
        .map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(cached, review);

    let alerts = recent_alerts(&config)?;
    assert_eq!(alerts["open"], 2);
    let health = health(&config)?;
    assert_eq!(health["queue_depth"], 1);
    assert_eq!(health["open_alerts"], 2);
    assert_eq!(health["upstream_configured"], false);
    assert!(constant_time_equal(b"same", b"same"));
    assert!(!constant_time_equal(b"same", b"different"));

    for extension in ["", "-wal", "-shm"] {
        let target = PathBuf::from(format!("{}{}", path.display(), extension));
        if target.exists() {
            fs::remove_file(target)?;
        }
    }
    Ok(18)
}

fn self_check_batch(event_suffix: &str, before: &str, after: &str, delta: &str) -> String {
    json!({
        "schema_version": "1.0",
        "producer": {
            "plugin_name": "self-check",
            "plugin_version": "1.0.0",
            "game_build": "test",
            "boot_id": "boot-self-check"
        },
        "sent_at": "2026-07-31T00:00:00+08:00",
        "events": [{
            "event_id": format!("event-self-check-{event_suffix}"),
            "sequence": event_suffix.parse::<u64>().unwrap(),
            "event_type": "ledger.currency_changed",
            "status": "succeeded",
            "occurred_at": "2026-07-31T00:00:00+08:00",
            "transaction_id": format!("transaction-{event_suffix}"),
            "actor": { "player_id": "player-1" },
            "context": { "reason_code": "self_check" },
            "data": {
                "currency_changes": [{
                    "owner_id": "player-1",
                    "currency": "gold_coin",
                    "before": before,
                    "after": after,
                    "delta": delta
                }]
            }
        }]
    })
    .to_string()
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve => serve(AgentConfig::from_env()?),
        Command::SelfCheck => {
            let checks = self_check()?;
            println!("{}", json!({ "ok": true, "checks": checks }));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_upstream(response: HttpResponse) -> (SocketAddrV4, thread::JoinHandle<HttpRequest>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            _ => unreachable!(),
        };
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream).unwrap();
            write_response(&mut stream, response).unwrap();
            request
        });
        (address, worker)
    }

    fn test_config(name: &str) -> AgentConfig {
        let unique = format!(
            "risk-agent-{name}-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        AgentConfig {
            tenant_id: "tenant-test".to_string(),
            server_id: "server-test".to_string(),
            local_token: "test-token-that-is-longer-than-32-bytes".to_string(),
            port: DEFAULT_PORT,
            queue_path: env::temp_dir().join(unique),
            mode: "shadow".to_string(),
            gold_gain_10m: DEFAULT_GOLD_GAIN_10M,
            asset_moves_10m: DEFAULT_ASSET_MOVES_10M,
            high_value_gold: DEFAULT_HIGH_VALUE_GOLD,
            high_value_asset_quantity: DEFAULT_HIGH_VALUE_ASSET_QUANTITY,
            upstream_url: None,
            upstream_token: None,
            delivery_batch_size: DEFAULT_DELIVERY_BATCH_SIZE,
            delivery_max_attempts: DEFAULT_DELIVERY_MAX_ATTEMPTS,
        }
    }

    fn cleanup_queue(path: &Path) {
        for extension in ["", "-wal", "-shm"] {
            let target = PathBuf::from(format!("{}{}", path.display(), extension));
            let _ = fs::remove_file(target);
        }
    }

    fn high_value_decision_body(request_id: &str) -> String {
        json!({
            "schema_version": "1.0",
            "request_id": request_id,
            "occurred_at": "2026-07-31T01:30:00+08:00",
            "action_type": "trade.commit",
            "transaction_id": "trade-high-value-1",
            "timeout_ms": 20,
            "actor": {
                "player_id": "player-1",
                "device_fingerprint": "hmac-sha256:same-device"
            },
            "counterparty": {
                "player_id": "player-2",
                "device_fingerprint": "hmac-sha256:same-device"
            },
            "proposed_changes": {
                "currency_changes": [
                    {
                        "owner_id": "player-1", "currency": "gold_coin",
                        "before": "0", "after": "2000000", "delta": "2000000"
                    },
                    {
                        "owner_id": "player-2", "currency": "gold_coin",
                        "before": "2000000", "after": "0", "delta": "-2000000"
                    }
                ],
                "asset_changes": []
            }
        })
        .to_string()
    }

    fn snapshot_batch(sequence: u64, event_id: &str, occurred_at: &str, gold_coin: i64) -> String {
        json!({
            "schema_version": "1.0",
            "producer": {
                "plugin_name": "snapshot-test", "plugin_version": "1.0.0",
                "game_build": "test", "boot_id": "boot-snapshot-test"
            },
            "sent_at": occurred_at,
            "events": [{
                "event_id": event_id,
                "sequence": sequence,
                "event_type": "state.player_snapshot",
                "status": "succeeded",
                "occurred_at": occurred_at,
                "actor": { "player_id": "player-1" },
                "context": { "reason_code": "test_snapshot" },
                "data": { "player_state": {
                    "online": true, "level": 100,
                    "currencies": {
                        "game_cash": "0", "gold_coin": gold_coin.to_string(), "silver_coin": "0"
                    }
                }}
            }]
        })
        .to_string()
    }

    fn rapid_action_batch() -> String {
        let events: Vec<Value> = (0..RAPID_IDENTICAL_ACTION_COUNT)
            .map(|index| json!({
                "event_id": format!("event-rapid-action-{index:04}"),
                "sequence": index + 1,
                "event_type": "security.action_attempted",
                "status": "attempted",
                "occurred_at": format!("2026-07-31T01:00:{:02}.{:03}+08:00", index / 10, (index % 10) * 100),
                "actor": { "player_id": "player-script-1" },
                "context": { "action_code": "npc.dialog.next" },
                "data": { "metadata": { "result": "attempted" } }
            }))
            .collect();
        json!({
            "schema_version": "1.0",
            "producer": {
                "plugin_name": "action-test", "plugin_version": "1.0.0",
                "game_build": "test", "boot_id": "boot-action-test"
            },
            "sent_at": "2026-07-31T01:00:02+08:00",
            "events": events
        })
        .to_string()
    }

    #[test]
    fn amount_parser_is_strict() {
        assert_eq!(parse_amount("-42"), Ok(-42));
        assert!(parse_amount("+42").is_err());
        assert!(parse_amount("1.0").is_err());
        assert!(parse_amount("--1").is_err());
    }

    #[test]
    fn constant_time_comparison_handles_different_lengths() {
        assert!(constant_time_equal(b"secret", b"secret"));
        assert!(!constant_time_equal(b"secret", b"secrex"));
        assert!(!constant_time_equal(b"secret", b"secret-long"));
    }

    #[test]
    fn authenticated_gateway_headers_scope_the_event() {
        let config = test_config("gateway-scope");
        prepare_queue(&config.queue_path).unwrap();
        let mut headers = HashMap::from([
            ("content-type".to_string(), "application/json".to_string()),
            ("x-pgr-local-token".to_string(), config.local_token.clone()),
            ("x-pgr-tenant-id".to_string(), "tenant-remote".to_string()),
            ("x-pgr-server-id".to_string(), "server-remote".to_string()),
        ]);
        let body = self_check_batch("101", "1000", "1100", "100").into_bytes();
        let response = route(
            &config,
            HttpRequest {
                method: "POST".to_string(),
                path: "/agent/v1/events:batch".to_string(),
                headers: headers.clone(),
                body: body.clone(),
            },
        );
        assert_eq!(response.status, 200);
        let db = connect_queue(&config.queue_path).unwrap();
        let scoped: i64 = db
            .query_row(
                "select count(*) from event_queue where tenant_id=?1 and server_id=?2",
                params!["tenant-remote", "server-remote"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scoped, 1);

        headers.remove("x-pgr-server-id");
        let rejected = route(
            &config,
            HttpRequest {
                method: "POST".to_string(),
                path: "/agent/v1/events:batch".to_string(),
                headers,
                body,
            },
        );
        assert_eq!(rejected.status, 400);
        drop(db);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn forbidden_identity_is_found_at_any_depth() {
        let value = json!({ "data": { "metadata": { "server_id": "spoof" } } });
        assert_eq!(forbidden_key(&value), Some("server_id"));
    }

    #[test]
    fn valid_batch_passes_contract_validation() {
        let body = self_check_batch("100", "1000", "1100", "100");
        let batch = validate_batch(body.as_bytes()).unwrap();
        assert_eq!(batch.events.len(), 1);
        assert!(batch.rejected.is_empty());
    }

    #[test]
    fn unbalanced_currency_is_rejected_per_event() {
        let body = self_check_batch("100", "1000", "1100", "99");
        let batch = validate_batch(body.as_bytes()).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.rejected[0].code, "currency_not_balanced");
    }

    #[test]
    fn duplicate_event_insert_is_idempotent() {
        let path = env::temp_dir().join(format!("risk-agent-test-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let config = AgentConfig {
            tenant_id: "tenant-test".to_string(),
            server_id: "server-test".to_string(),
            local_token: "test-token-that-is-longer-than-32-bytes".to_string(),
            port: DEFAULT_PORT,
            queue_path: path.clone(),
            mode: "shadow".to_string(),
            gold_gain_10m: DEFAULT_GOLD_GAIN_10M,
            asset_moves_10m: DEFAULT_ASSET_MOVES_10M,
            high_value_gold: DEFAULT_HIGH_VALUE_GOLD,
            high_value_asset_quantity: DEFAULT_HIGH_VALUE_ASSET_QUANTITY,
            upstream_url: None,
            upstream_token: None,
            delivery_batch_size: DEFAULT_DELIVERY_BATCH_SIZE,
            delivery_max_attempts: DEFAULT_DELIVERY_MAX_ATTEMPTS,
        };
        prepare_queue(&path).unwrap();
        let body = self_check_batch("100", "1000", "1100", "100");
        assert_eq!(ingest(&config, body.as_bytes()).unwrap().accepted, 1);
        let duplicate = ingest(&config, body.as_bytes()).unwrap();
        assert_eq!(duplicate.accepted, 0);
        assert_eq!(duplicate.duplicates, 1);
        drop(config);
        for extension in ["", "-wal", "-shm"] {
            let target = PathBuf::from(format!("{}{}", path.display(), extension));
            let _ = fs::remove_file(target);
        }
    }

    #[test]
    fn delivery_worker_marks_acknowledged_events_delivered() {
        let (address, upstream) = test_upstream(HttpResponse::json(
            200,
            json!({
                "accepted": 1,
                "duplicates": 0,
                "rejected": [],
                "accepted_through_sequence": 100
            }),
        ));

        let mut config = test_config("delivery-ack");
        config.upstream_url = Some(format!("http://{address}/sdk/v1/events:batch"));
        config.upstream_token =
            Some("test-upstream-token-that-is-longer-than-32-bytes".to_string());
        prepare_queue(&config.queue_path).unwrap();
        let body = self_check_batch("100", "1000", "1100", "100");
        assert_eq!(ingest(&config, body.as_bytes()).unwrap().queue_depth, 1);
        assert_eq!(deliver_once(&config).unwrap(), 1);
        let request = upstream.join().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/sdk/v1/events:batch");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer test-upstream-token-that-is-longer-than-32-bytes")
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["events"].as_array().unwrap().len(), 1);

        let connection = connect_queue(&config.queue_path).unwrap();
        assert_eq!(queue_depth(&connection).unwrap(), 0);
        let metrics = delivery_metrics(&connection).unwrap();
        assert_eq!(metrics["delivered"], 1);
        assert_eq!(metrics["dead"], 0);
        drop(connection);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn invalid_delivery_ack_is_retried_instead_of_lost() {
        let (address, upstream) = test_upstream(HttpResponse::json(200, json!({ "ok": true })));
        let mut config = test_config("delivery-invalid-ack");
        config.upstream_url = Some(format!("http://{address}/sdk/v1/events:batch"));
        config.upstream_token =
            Some("test-upstream-token-that-is-longer-than-32-bytes".to_string());
        prepare_queue(&config.queue_path).unwrap();
        let body = self_check_batch("100", "1000", "1100", "100");
        ingest(&config, body.as_bytes()).unwrap();
        assert_eq!(deliver_once(&config).unwrap(), 1);
        upstream.join().unwrap();

        let connection = connect_queue(&config.queue_path).unwrap();
        let state: String = connection
            .query_row("select delivery_state from event_queue", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "retry");
        assert_eq!(delivery_metrics(&connection).unwrap()["delivered"], 0);
        drop(connection);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn permanent_http_error_moves_event_to_dead_letter() {
        let (address, upstream) =
            test_upstream(HttpResponse::json(401, json!({ "error": "unauthorized" })));
        let mut config = test_config("delivery-http-401");
        config.upstream_url = Some(format!("http://{address}/sdk/v1/events:batch"));
        config.upstream_token =
            Some("test-upstream-token-that-is-longer-than-32-bytes".to_string());
        prepare_queue(&config.queue_path).unwrap();
        let body = self_check_batch("100", "1000", "1100", "100");
        ingest(&config, body.as_bytes()).unwrap();
        assert_eq!(deliver_once(&config).unwrap(), 1);
        upstream.join().unwrap();

        let connection = connect_queue(&config.queue_path).unwrap();
        let metrics = delivery_metrics(&connection).unwrap();
        assert_eq!(metrics["dead"], 1);
        assert_eq!(metrics["pending"], 0);
        drop(connection);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn partial_ack_delivers_success_and_dead_letters_permanent_rejection() {
        let (address, upstream) = test_upstream(HttpResponse::json(
            200,
            json!({
                "accepted": 1,
                "duplicates": 0,
                "rejected": [{
                    "event_id": "event-self-check-101",
                    "retryable": false,
                    "message": "unsupported event"
                }]
            }),
        ));
        let mut config = test_config("delivery-partial-ack");
        config.upstream_url = Some(format!("http://{address}/sdk/v1/events:batch"));
        config.upstream_token =
            Some("test-upstream-token-that-is-longer-than-32-bytes".to_string());
        prepare_queue(&config.queue_path).unwrap();
        for sequence in ["100", "101"] {
            let body = self_check_batch(sequence, "1000", "1100", "100");
            ingest(&config, body.as_bytes()).unwrap();
        }
        assert_eq!(deliver_once(&config).unwrap(), 2);
        upstream.join().unwrap();

        let connection = connect_queue(&config.queue_path).unwrap();
        let metrics = delivery_metrics(&connection).unwrap();
        assert_eq!(metrics["delivered"], 1);
        assert_eq!(metrics["dead"], 1);
        assert_eq!(metrics["pending"], 0);
        drop(connection);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn empty_delivery_metrics_and_legacy_queue_migration_work() {
        let config = test_config("delivery-legacy-migration");
        let connection = connect_queue(&config.queue_path).unwrap();
        connection
            .execute_batch(
                "create table event_queue (
                   tenant_id text not null, server_id text not null, event_id text not null,
                   plugin_name text not null, plugin_version text not null, game_build text not null,
                   boot_id text not null, sequence integer not null, event_type text not null,
                   event_status text not null, occurred_at text not null, transaction_id text,
                   actor_id text not null, payload_json text not null, received_at text not null,
                   delivery_state text not null default 'pending', attempts integer not null default 0,
                   primary key (tenant_id, server_id, event_id)
                 );",
            )
            .unwrap();
        drop(connection);

        prepare_queue(&config.queue_path).unwrap();
        let connection = connect_queue(&config.queue_path).unwrap();
        let columns = connection
            .prepare("pragma table_info(event_queue)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<HashSet<_>>>()
            .unwrap();
        for name in [
            "next_attempt_at_ms",
            "lease_until_ms",
            "last_error",
            "delivered_at",
        ] {
            assert!(columns.contains(name));
        }
        let metrics = delivery_metrics(&connection).unwrap();
        assert_eq!(metrics["pending"], 0);
        assert_eq!(metrics["delivery_attempts"], 0);
        drop(connection);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn example_batch_creates_explainable_alerts_once() {
        let config = test_config("example-rules");
        prepare_queue(&config.queue_path).unwrap();
        let body = include_str!("../../../docs/plugin-event-batch.v1.example.json");
        let first = ingest(&config, body.as_bytes()).unwrap();
        assert_eq!(first.accepted, 7);
        assert_eq!(first.alerts_created, 2);
        assert!(first.rule_codes.contains(&"same_device_trade".to_string()));
        assert!(first
            .rule_codes
            .contains(&"server_validation_failed".to_string()));

        let duplicate = ingest(&config, body.as_bytes()).unwrap();
        assert_eq!(duplicate.duplicates, 7);
        assert_eq!(duplicate.alerts_created, 0);
        let alerts = recent_alerts(&config).unwrap();
        assert_eq!(alerts["open"], 2);
        assert_eq!(alerts["returned"], 2);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn high_value_same_device_decision_is_review_and_idempotent() {
        let config = test_config("decision-rules");
        prepare_queue(&config.queue_path).unwrap();
        let body = high_value_decision_body("decision-high-value-1");
        let first = decide(&config, body.as_bytes()).unwrap();
        assert_eq!(first["decision"], "review");
        assert_eq!(first["risk_score"], 80);
        assert!(first["rule_codes"]
            .as_array()
            .unwrap()
            .contains(&json!("high_value_gold_change")));
        assert!(first["rule_codes"]
            .as_array()
            .unwrap()
            .contains(&json!("same_device_counterparty")));
        let second = decide(&config, body.as_bytes()).unwrap();
        assert_eq!(second, first);
        assert_eq!(recent_alerts(&config).unwrap()["open"], 2);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn unexplained_gold_snapshot_jump_is_detected() {
        let config = test_config("snapshot-jump");
        prepare_queue(&config.queue_path).unwrap();
        let first = snapshot_batch(
            1,
            "event-snapshot-test-0001",
            "2026-07-31T01:00:00+08:00",
            100,
        );
        let second = snapshot_batch(
            2,
            "event-snapshot-test-0002",
            "2026-07-31T01:01:00+08:00",
            2_000_100,
        );
        assert_eq!(ingest(&config, first.as_bytes()).unwrap().alerts_created, 0);
        let result = ingest(&config, second.as_bytes()).unwrap();
        assert_eq!(result.alerts_created, 1);
        assert_eq!(result.rule_codes, ["unexplained_gold_snapshot_jump"]);
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn rapid_identical_actions_create_one_explainable_alert() {
        let config = test_config("rapid-actions");
        prepare_queue(&config.queue_path).unwrap();
        let result = ingest(&config, rapid_action_batch().as_bytes()).unwrap();
        assert_eq!(result.accepted, RAPID_IDENTICAL_ACTION_COUNT as usize);
        assert_eq!(result.alerts_created, 1);
        assert_eq!(result.rule_codes, ["rapid_identical_action"]);
        let alerts = recent_alerts(&config).unwrap();
        assert_eq!(
            alerts["alerts"][0]["evidence"]["count"],
            RAPID_IDENTICAL_ACTION_COUNT
        );
        cleanup_queue(&config.queue_path);
    }

    #[test]
    fn cross_batch_sequence_gap_is_detected() {
        let config = test_config("sequence-gap");
        prepare_queue(&config.queue_path).unwrap();
        let first = self_check_batch("100", "0", "1", "1");
        let gap = self_check_batch("102", "1", "2", "1");
        assert_eq!(ingest(&config, first.as_bytes()).unwrap().alerts_created, 0);
        let result = ingest(&config, gap.as_bytes()).unwrap();
        assert_eq!(result.alerts_created, 1);
        assert_eq!(result.rule_codes, ["plugin_sequence_gap"]);
        let late = self_check_batch("101", "2", "3", "1");
        let late_result = ingest(&config, late.as_bytes()).unwrap();
        assert_eq!(late_result.alerts_created, 1);
        assert_eq!(late_result.rule_codes, ["plugin_sequence_regression"]);
        cleanup_queue(&config.queue_path);
    }
}

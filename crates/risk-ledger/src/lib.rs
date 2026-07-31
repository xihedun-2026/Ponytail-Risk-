//! 本地资产快照账本（SQLite）。
//!
//! 对应 `tools/risk_live_data.py` 的 `prepare_ledger` / `apply_snapshot` / `ledger_events`。
//!
//! 重要语义（交接报告 §4 限制 4、README）：这是**轮询快照**账本，不是游戏内的
//! 精确事件流。`first_seen` 只说明「两次扫描之间进入了当前持有表」，
//! 不等同于掉落/生成事件；`missing` 也只说明离开了当前持有表。

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use risk_core::normalized_iid;

/// 当前持有表（`item_info` / `pet_info`）里的一行。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetRow {
    pub iid: String,
    pub name: String,
    pub owner: String,
    pub owner_name: String,
    pub env: String,
    pub pos: i64,
    pub amount: i64,
}

/// 账本事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// 接入时就已存在，不视为生成事件。
    Baseline,
    /// 两次扫描之间进入当前持有表。
    FirstSeen,
    /// 持有人字段发生变化。
    OwnerChanged,
    /// 堆叠数量发生变化。
    AmountChanged,
    /// 离开当前持有表。
    Missing,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Baseline => "baseline",
            EventKind::FirstSeen => "first_seen",
            EventKind::OwnerChanged => "owner_changed",
            EventKind::AmountChanged => "amount_changed",
            EventKind::Missing => "missing",
        }
    }

    /// 前端展示用的 (动作, 说明)，对应 Python 的 `snapshot_labels`。
    pub fn labels(value: &str) -> (String, String) {
        match value {
            "baseline" => ("账本基线".into(), "接入时已存在，不视为生成事件".into()),
            "first_seen" => ("快照首次观察".into(), "两次扫描之间进入当前持有表".into()),
            "owner_changed" => ("快照持有人变化".into(), "持有人字段发生变化".into()),
            "amount_changed" => ("快照数量变化".into(), "堆叠数量发生变化".into()),
            "missing" => (
                "离开当前持有表".into(),
                "可能被使用、丢弃或转入未覆盖容器".into(),
            ),
            other => (other.to_string(), "本地快照事件".into()),
        }
    }
}

/// 一次扫描产生的变更计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub baseline: i64,
    pub first_seen: i64,
    pub owner_changed: i64,
    pub amount_changed: i64,
    pub missing: i64,
}

impl Changes {
    fn bump(&mut self, kind: EventKind) {
        match kind {
            EventKind::Baseline => self.baseline += 1,
            EventKind::FirstSeen => self.first_seen += 1,
            EventKind::OwnerChanged => self.owner_changed += 1,
            EventKind::AmountChanged => self.amount_changed += 1,
            EventKind::Missing => self.missing += 1,
        }
    }
}

/// `apply_snapshot` 的返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub scanned: usize,
    pub changes: Changes,
}

/// 账本里的一条事件记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEvent {
    pub id: i64,
    pub event_time: String,
    pub event_type: String,
    pub iid: String,
    pub name: String,
    pub owner_from: String,
    pub owner_to: String,
    pub amount_before: i64,
    pub amount_after: i64,
    pub evidence: String,
}

/// 建表。可重复调用。
pub fn prepare_ledger(db: &Connection) -> Result<()> {
    db.execute_batch(
        r#"
        create table if not exists ledger_meta (
          key text primary key,
          value text not null
        );
        create table if not exists asset_state (
          iid text primary key,
          name text not null,
          owner text not null,
          owner_name text not null,
          env text not null,
          pos integer not null,
          amount integer not null,
          present integer not null,
          last_seen text not null
        );
        create table if not exists asset_event (
          id integer primary key autoincrement,
          event_time text not null,
          event_type text not null,
          iid text not null,
          name text not null,
          owner_from text not null,
          owner_to text not null,
          amount_before integer not null,
          amount_after integer not null,
          evidence text not null
        );
        create index if not exists asset_event_iid_time on asset_event(iid, event_time);
        "#,
    )?;
    Ok(())
}

/// 上一轮扫描留下的状态。
#[derive(Debug, Clone)]
struct PreviousState {
    owner: String,
    name: String,
    amount: i64,
    present: bool,
}

/// 保持插入顺序的去重集合，对齐 Python dict 推导式的行为
/// （重复 key 覆盖取值，但保留首次出现的位置）。
fn dedupe_preserving_order(rows: &[AssetRow]) -> Vec<(String, AssetRow)> {
    let mut order: Vec<(String, AssetRow)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let key = normalized_iid(&row.iid);
        if key.is_empty() {
            continue;
        }
        match index.get(&key) {
            Some(&position) => order[position].1 = row.clone(),
            None => {
                index.insert(key.clone(), order.len());
                order.push((key, row.clone()));
            }
        }
    }
    order
}

/// 应用一次全量快照，写入差异事件并更新当前状态。
///
/// 首轮（`ledger_meta.initialized` 不存在）产生的全部是 `baseline`，
/// 避免把「接入时就已存在的资产」误报成新生成。
pub fn apply_snapshot(
    db: &Connection,
    rows: &[AssetRow],
    scanned_at: &str,
) -> Result<SnapshotResult> {
    prepare_ledger(db)?;

    let initialized: bool = db
        .query_row(
            "select 1 from ledger_meta where key='initialized'",
            [],
            |_| Ok(()),
        )
        .is_ok();

    let mut previous: Vec<(String, PreviousState)> = Vec::new();
    {
        let mut statement = db.prepare("select iid,name,owner,amount,present from asset_state")?;
        let mapped = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                PreviousState {
                    name: row.get::<_, String>(1)?,
                    owner: row.get::<_, String>(2)?,
                    amount: row.get::<_, i64>(3)?,
                    present: row.get::<_, i64>(4)? != 0,
                },
            ))
        })?;
        for entry in mapped {
            previous.push(entry?);
        }
    }
    let previous_index: HashMap<&str, &PreviousState> = previous
        .iter()
        .map(|(iid, state)| (iid.as_str(), state))
        .collect();

    let current = dedupe_preserving_order(rows);
    let current_keys: HashMap<&str, ()> =
        current.iter().map(|(iid, _)| (iid.as_str(), ())).collect();

    let mut changes = Changes::default();

    for (iid, row) in &current {
        let old = previous_index.get(iid.as_str()).copied();

        match old {
            None => {
                let kind = if initialized {
                    EventKind::FirstSeen
                } else {
                    EventKind::Baseline
                };
                record_event(db, scanned_at, kind, iid, row, "", 0)?;
                changes.bump(kind);
            }
            Some(state) if !state.present => {
                record_event(
                    db,
                    scanned_at,
                    EventKind::FirstSeen,
                    iid,
                    row,
                    &state.owner,
                    state.amount,
                )?;
                changes.bump(EventKind::FirstSeen);
            }
            Some(state) if state.owner != row.owner => {
                record_event(
                    db,
                    scanned_at,
                    EventKind::OwnerChanged,
                    iid,
                    row,
                    &state.owner,
                    state.amount,
                )?;
                changes.bump(EventKind::OwnerChanged);
            }
            Some(_) => {}
        }

        // 数量变化与上面的分支互不排斥：改持有人的同时也可能改数量。
        if let Some(state) = old {
            if state.amount != row.amount {
                record_event(
                    db,
                    scanned_at,
                    EventKind::AmountChanged,
                    iid,
                    row,
                    &state.owner,
                    state.amount,
                )?;
                changes.bump(EventKind::AmountChanged);
            }
        }

        db.execute(
            r#"
            insert into asset_state(iid,name,owner,owner_name,env,pos,amount,present,last_seen)
            values(?1,?2,?3,?4,?5,?6,?7,1,?8)
            on conflict(iid) do update set name=excluded.name,owner=excluded.owner,
              owner_name=excluded.owner_name,env=excluded.env,pos=excluded.pos,
              amount=excluded.amount,present=1,last_seen=excluded.last_seen
            "#,
            params![
                iid,
                row.name,
                row.owner,
                row.owner_name,
                row.env,
                row.pos,
                row.amount,
                scanned_at,
            ],
        )?;
    }

    for (iid, state) in &previous {
        if state.present && !current_keys.contains_key(iid.as_str()) {
            let vanished = AssetRow {
                name: state.name.clone(),
                ..Default::default()
            };
            record_event(
                db,
                scanned_at,
                EventKind::Missing,
                iid,
                &vanished,
                &state.owner,
                state.amount,
            )?;
            changes.bump(EventKind::Missing);
            db.execute(
                "update asset_state set present=0,last_seen=?1 where iid=?2",
                params![scanned_at, iid],
            )?;
        }
    }

    db.execute(
        "insert or replace into ledger_meta(key,value) values('initialized',?1)",
        params![scanned_at],
    )?;

    Ok(SnapshotResult {
        scanned: current.len(),
        changes,
    })
}

fn record_event(
    db: &Connection,
    scanned_at: &str,
    kind: EventKind,
    iid: &str,
    row: &AssetRow,
    owner_from: &str,
    amount_before: i64,
) -> Result<()> {
    let evidence = serde_json::json!({ "env": row.env, "pos": row.pos }).to_string();
    db.execute(
        r#"
        insert into asset_event(event_time,event_type,iid,name,owner_from,owner_to,amount_before,amount_after,evidence)
        values(?1,?2,?3,?4,?5,?6,?7,?8,?9)
        "#,
        params![
            scanned_at,
            kind.as_str(),
            iid,
            row.name,
            owner_from,
            row.owner,
            amount_before,
            row.amount,
            evidence,
        ],
    )?;
    Ok(())
}

/// 读取某个 IID 的全部账本事件，按时间升序。
pub fn ledger_events(db: &Connection, iid: &str) -> Result<Vec<LedgerEvent>> {
    prepare_ledger(db)?;
    let mut statement = db.prepare(
        "select id,event_time,event_type,iid,name,owner_from,owner_to,amount_before,amount_after,evidence
         from asset_event where iid=?1 order by event_time",
    )?;
    let mapped = statement.query_map(params![iid], |row| {
        Ok(LedgerEvent {
            id: row.get(0)?,
            event_time: row.get(1)?,
            event_type: row.get(2)?,
            iid: row.get(3)?,
            name: row.get(4)?,
            owner_from: row.get(5)?,
            owner_to: row.get(6)?,
            amount_before: row.get(7)?,
            amount_after: row.get(8)?,
            evidence: row.get(9)?,
        })
    })?;
    let mut events = Vec::new();
    for entry in mapped {
        events.push(entry?);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_row() -> AssetRow {
        AssetRow {
            iid: ":A1:".to_string(),
            name: "item".to_string(),
            owner: "p1".to_string(),
            owner_name: "one".to_string(),
            env: "bag".to_string(),
            pos: 1,
            amount: 1,
        }
    }

    fn memory_ledger() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        prepare_ledger(&db).unwrap();
        db
    }

    #[test]
    fn snapshot_lifecycle_matches_python_self_check() {
        // 逐条对应 Python self_check 里的 apply_snapshot 断言。
        let db = memory_ledger();

        let first = apply_snapshot(&db, &[base_row()], "2026-01-01T00:00:00").unwrap();
        assert_eq!(first.changes.baseline, 1);
        assert_eq!(first.scanned, 1);

        let changed = vec![
            AssetRow {
                owner: "p2".to_string(),
                owner_name: "two".to_string(),
                amount: 2,
                ..base_row()
            },
            AssetRow {
                iid: ":A2:".to_string(),
                ..base_row()
            },
        ];
        let second = apply_snapshot(&db, &changed, "2026-01-01T00:01:00").unwrap();
        assert_eq!(second.changes.owner_changed, 1);
        assert_eq!(second.changes.amount_changed, 1);
        assert_eq!(second.changes.first_seen, 1);
        assert_eq!(second.changes.baseline, 0);

        let third = apply_snapshot(&db, &[], "2026-01-01T00:02:00").unwrap();
        assert_eq!(third.changes.missing, 2);
        assert_eq!(third.scanned, 0);
    }

    #[test]
    fn first_scan_never_reports_first_seen() {
        // 接入时已存在的资产必须记为 baseline，否则会把存量误报成新生成。
        let db = memory_ledger();
        let rows: Vec<AssetRow> = (0..5)
            .map(|index| AssetRow {
                iid: format!(":A{index}:"),
                ..base_row()
            })
            .collect();
        let result = apply_snapshot(&db, &rows, "2026-01-01T00:00:00").unwrap();
        assert_eq!(result.changes.baseline, 5);
        assert_eq!(result.changes.first_seen, 0);
    }

    #[test]
    fn reappearing_asset_is_first_seen_not_baseline() {
        let db = memory_ledger();
        apply_snapshot(&db, &[base_row()], "t0").unwrap();
        apply_snapshot(&db, &[], "t1").unwrap();
        let back = apply_snapshot(&db, &[base_row()], "t2").unwrap();
        assert_eq!(back.changes.first_seen, 1);
        assert_eq!(back.changes.baseline, 0);
        // 离开时只置 present=0，amount 保留原值，所以原样回归不算数量变化。
        assert_eq!(back.changes.amount_changed, 0);

        // 数量确实变了才记一次。
        apply_snapshot(&db, &[], "t3").unwrap();
        let changed = apply_snapshot(
            &db,
            &[AssetRow {
                amount: 9,
                ..base_row()
            }],
            "t4",
        )
        .unwrap();
        assert_eq!(changed.changes.first_seen, 1);
        assert_eq!(changed.changes.amount_changed, 1);
    }

    #[test]
    fn owner_and_amount_change_together_records_both() {
        let db = memory_ledger();
        apply_snapshot(&db, &[base_row()], "t0").unwrap();
        let result = apply_snapshot(
            &db,
            &[AssetRow {
                owner: "p9".to_string(),
                amount: 7,
                ..base_row()
            }],
            "t1",
        )
        .unwrap();
        assert_eq!(result.changes.owner_changed, 1);
        assert_eq!(result.changes.amount_changed, 1);

        let events = ledger_events(&db, "A1").unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(kinds, vec!["baseline", "owner_changed", "amount_changed"]);
    }

    #[test]
    fn unchanged_asset_produces_no_event() {
        let db = memory_ledger();
        apply_snapshot(&db, &[base_row()], "t0").unwrap();
        let result = apply_snapshot(&db, &[base_row()], "t1").unwrap();
        assert_eq!(result.changes, Changes::default());
        assert_eq!(ledger_events(&db, "A1").unwrap().len(), 1);
    }

    #[test]
    fn iid_is_normalized_before_storage() {
        let db = memory_ledger();
        apply_snapshot(
            &db,
            &[AssetRow {
                iid: "  :6a617f69000102542fd9:  ".to_string(),
                ..base_row()
            }],
            "t0",
        )
        .unwrap();
        let events = ledger_events(&db, "6A617F69000102542FD9").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "baseline");
    }

    #[test]
    fn rows_with_blank_iid_are_skipped() {
        let db = memory_ledger();
        let result = apply_snapshot(
            &db,
            &[
                AssetRow {
                    iid: "::".to_string(),
                    ..base_row()
                },
                AssetRow {
                    iid: "   ".to_string(),
                    ..base_row()
                },
                base_row(),
            ],
            "t0",
        )
        .unwrap();
        assert_eq!(result.scanned, 1);
        assert_eq!(result.changes.baseline, 1);
    }

    #[test]
    fn duplicate_iid_in_one_snapshot_keeps_last_value() {
        let db = memory_ledger();
        let result = apply_snapshot(
            &db,
            &[
                base_row(),
                AssetRow {
                    amount: 42,
                    ..base_row()
                },
            ],
            "t0",
        )
        .unwrap();
        assert_eq!(result.scanned, 1);
        let events = ledger_events(&db, "A1").unwrap();
        assert_eq!(events[0].amount_after, 42);
    }

    #[test]
    fn event_evidence_carries_env_and_pos() {
        let db = memory_ledger();
        apply_snapshot(&db, &[base_row()], "t0").unwrap();
        let events = ledger_events(&db, "A1").unwrap();
        let evidence: serde_json::Value = serde_json::from_str(&events[0].evidence).unwrap();
        assert_eq!(evidence["env"], "bag");
        assert_eq!(evidence["pos"], 1);
    }

    #[test]
    fn missing_event_clears_present_flag_only_once() {
        let db = memory_ledger();
        apply_snapshot(&db, &[base_row()], "t0").unwrap();
        assert_eq!(apply_snapshot(&db, &[], "t1").unwrap().changes.missing, 1);
        // 已经标记为离开的资产不应反复产生 missing 事件。
        assert_eq!(apply_snapshot(&db, &[], "t2").unwrap().changes.missing, 0);
    }

    #[test]
    fn event_kind_labels_match_python() {
        assert_eq!(
            EventKind::labels("baseline"),
            (
                "账本基线".to_string(),
                "接入时已存在，不视为生成事件".to_string()
            )
        );
        assert_eq!(EventKind::labels("missing").0, "离开当前持有表".to_string());
        // 未知类型保持原样，不猜语义。
        assert_eq!(
            EventKind::labels("weird"),
            ("weird".to_string(), "本地快照事件".to_string())
        );
    }
}

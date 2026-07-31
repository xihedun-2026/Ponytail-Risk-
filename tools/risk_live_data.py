#!/usr/bin/env python3
from __future__ import annotations

import argparse
from collections import deque
import json
import os
import re
import sqlite3
import statistics
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

import pymysql


sys.stdout.reconfigure(encoding="utf-8")


def database_identifier(env_name: str, default: str) -> str:
    value = os.environ.get(env_name, default)
    if not re.fullmatch(r"[A-Za-z0-9_]{1,64}", value):
        raise RuntimeError(f"{env_name} is invalid")
    return value


MAIN_DATABASE = database_identifier("GAME_DB_MAIN", "dl_mdb_1")
LOG_DATABASE = database_identifier("GAME_DB_LOG", "dl_ldb_1")


ASSET_TABLES = (
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
)

REWARD_TYPES = {
    1: "道具",
    2: "经验",
    3: "道行",
    7: "元宝",
    14: "宠物",
}

COIN_LABELS = {
    "gold_coin": "金元宝",
    "silver_coin": "银元宝",
}

# ponytail: only actions proven from their call sites are classified as gains/costs.
# Extend these sets after mapping more source call sites; unknown actions stay neutral.
CONFIRMED_GAIN_ACTIONS = {
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
}
CONFIRMED_COST_ACTIONS = {"yuancxl"}
USER_ASSET_ACTIONS = ("drop", "get", "exchange", "buy", "take_stall_cash", "drop_pet")


def gameplay_caps_from_env() -> list[dict]:
    raw = os.environ.get("RISK_GAMEPLAY_CAPS_JSON", "").strip()
    if not raw:
        return []
    try:
        caps = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError("RISK_GAMEPLAY_CAPS_JSON is invalid") from error
    if not isinstance(caps, list) or len(caps) > 100:
        raise RuntimeError("RISK_GAMEPLAY_CAPS_JSON is invalid")
    actions = set()
    expected = {"action", "label", "dailyLimit", "burst10mLimit", "enabled"}
    for cap in caps:
        if not isinstance(cap, dict) or set(cap) != expected:
            raise RuntimeError("gameplay cap entry is invalid")
        action, label = cap["action"], cap["label"]
        daily, burst, enabled = cap["dailyLimit"], cap["burst10mLimit"], cap["enabled"]
        if not isinstance(action, str) or not re.fullmatch(r"[A-Za-z0-9_:-]{1,64}", action) or action in actions:
            raise RuntimeError("gameplay cap action is invalid or duplicated")
        if not isinstance(label, str) or not label.strip() or len(label) > 80 or any(ord(char) < 32 or ord(char) == 127 for char in label):
            raise RuntimeError("gameplay cap label is invalid")
        if type(daily) is not int or not 0 <= daily <= 1_000_000 or type(burst) is not int or not 0 <= burst <= 100_000:
            raise RuntimeError("gameplay cap limit is invalid")
        if type(enabled) is not bool or enabled and not daily and not burst:
            raise RuntimeError("gameplay cap limit is invalid")
        actions.add(action)
    return caps


GAMEPLAY_CAPS = gameplay_caps_from_env()
REWARD_ACTIONS = CONFIRMED_GAIN_ACTIONS | {cap["action"] for cap in GAMEPLAY_CAPS if cap["enabled"]}
REWARD_ACTION_SQL_LIST = ",".join(f"'{action}'" for action in sorted(REWARD_ACTIONS))


def decode_value(value):
    if not isinstance(value, str):
        return value
    try:
        return value.encode("latin1").decode("gbk")
    except (UnicodeEncodeError, UnicodeDecodeError):
        return value


def decode_row(row: dict) -> dict:
    return {key: decode_value(value) for key, value in row.items()}


def database_value(value: str) -> str:
    try:
        return value.encode("gbk").decode("latin1")
    except UnicodeEncodeError:
        return value


def connect():
    password = os.environ.get("GAME_DB_PASSWORD", "")
    if not password:
        raise RuntimeError("GAME_DB_PASSWORD is required")
    return pymysql.connect(
        host=os.environ.get("GAME_DB_HOST", "127.0.0.1"),
        port=int(os.environ.get("GAME_DB_PORT", "3306")),
        user=os.environ.get("GAME_DB_USER", "root"),
        password=password,
        charset="latin1",
        autocommit=True,
        cursorclass=pymysql.cursors.DictCursor,
        read_timeout=8,
        write_timeout=8,
        connect_timeout=5,
    )


def fetch_all(db, sql: str, params=()) -> list[dict]:
    sql = sql.replace("dl_mdb_1", f"`{MAIN_DATABASE}`").replace("dl_ldb_1", f"`{LOG_DATABASE}`")
    with db.cursor() as cursor:
        cursor.execute(sql, params)
        return [decode_row(row) for row in cursor.fetchall()]


def fetch_one(db, sql: str, params=()) -> dict | None:
    rows = fetch_all(db, sql, params)
    return rows[0] if rows else None


def number(value) -> str:
    return f"{int(value or 0):,}"


def stamp_label(value: str) -> str:
    value = str(value or "")
    if len(value) == 14 and value.isdigit():
        return f"{value[4:6]}-{value[6:8]} {value[8:10]}:{value[10:12]}:{value[12:14]}"
    return value or "未知时间"


def risk_score(facts: dict) -> tuple[int, list[str], list[str]]:
    score = 0
    tags = []
    reasons = []
    abnormal = int(facts.get("abnormal_coin", 0))
    if abnormal:
        points = min(40, 20 + abnormal * 2)
        score += points
        tags.append("币值校验异常")
        reasons.append(f"出现 {abnormal} 次服务端币值校验异常")
    gold_coin = int(facts.get("gold_coin", 0))
    median_gold_coin = max(1, int(facts.get("median_gold_coin", 0)))
    if gold_coin >= 100_000_000 and gold_coin >= median_gold_coin * 8:
        score += 25
        tags.append("元宝存量偏离")
        reasons.append("当前元宝显著高于角色群体中位数")
    if int(facts.get("unpaired_transfers", 0)):
        score += 30
        tags.append("交易账本缺腿")
        reasons.append("存在无法同时匹配道具腿和金钱腿的交易")
    if int(facts.get("same_device_peers", 0)):
        score += 12
        tags.append("同设备交易")
        reasons.append("交易双方出现相同设备标识")
    funnel_sources = int(facts.get("funnel_source_peers", 0))
    funnel_rows = int(facts.get("funnel_asset_rows", 0))
    burst_sources = int(facts.get("burst_funnel_source_peers", 0))
    burst_rows = int(facts.get("burst_funnel_asset_rows", 0))
    if burst_sources >= 4 and burst_rows >= 8:
        score += 35
        tags.append("短时资产归集")
        reasons.append(f"10 分钟内有 {burst_sources} 个角色输入资产，共 {burst_rows} 条道具流水")
    elif funnel_sources >= 4 and funnel_rows >= 8:
        score += 25
        tags.append("多账号资产归集")
        reasons.append(f"近 30 天有 {funnel_sources} 个角色单向输入资产，共 {funnel_rows} 条道具流水")
    returned_asset_ids = int(facts.get("returned_asset_ids", 0))
    if returned_asset_ids >= 3:
        returned_asset_peers = int(facts.get("returned_asset_peers", 0))
        score += 20
        tags.append("资产循环回流")
        reasons.append(f"近 30 天有 {returned_asset_ids} 个资产 IID 与 {returned_asset_peers} 个交易对手发生双向回流")
    long_active_days = int(facts.get("long_active_days", 0))
    max_daily_span = int(facts.get("max_daily_active_span_minutes", 0))
    max_daily_events = int(facts.get("max_daily_active_events", 0))
    if long_active_days >= 2 and max_daily_span >= 18 * 60 and max_daily_events >= 100:
        score += 20
        tags.append("超长持续活跃")
        reasons.append(f"近 30 天有 {long_active_days} 天活跃超过 18 小时，单日最高 {max_daily_events} 个行为事件")
    mechanical_events = int(facts.get("mechanical_action_events", 0))
    mechanical_interval = int(facts.get("mechanical_interval_seconds", 0))
    mechanical_ratio = int(facts.get("mechanical_interval_ratio_permille", 0))
    mechanical_span = int(facts.get("mechanical_span_minutes", 0))
    if mechanical_events >= 20 and 1 <= mechanical_interval <= 300 and mechanical_ratio >= 800 and mechanical_span >= 30:
        score += 25
        tags.append("机械周期行为")
        reasons.append(f"行为 {facts.get('mechanical_action', '')} 连续 {mechanical_events} 次，{mechanical_interval} 秒间隔重复率 {mechanical_ratio // 10}%")
    reward_burst_events = int(facts.get("reward_burst_events", 0))
    if reward_burst_events >= 10:
        score += 25
        tags.append("奖励爆发异常")
        reasons.append(f"奖励动作 {facts.get('reward_burst_action', '')} 在 10 分钟内出现 {reward_burst_events} 次去重发放")
    cap_action = str(facts.get("configured_cap_action", ""))
    cap_daily_events = int(facts.get("configured_cap_daily_events", 0))
    cap_daily_limit = int(facts.get("configured_cap_daily_limit", 0))
    cap_burst_events = int(facts.get("configured_cap_burst_events", 0))
    cap_burst_limit = int(facts.get("configured_cap_burst_limit", 0))
    daily_cap_exceeded = cap_daily_limit > 0 and cap_daily_events > cap_daily_limit
    burst_cap_exceeded = cap_burst_limit > 0 and cap_burst_events > cap_burst_limit
    if cap_action and (daily_cap_exceeded or burst_cap_exceeded):
        score += 40
        tags.append("玩法产出超限")
        limits = []
        if daily_cap_exceeded:
            limits.append(f"单日 {cap_daily_events}/{cap_daily_limit}")
        if burst_cap_exceeded:
            limits.append(f"10 分钟 {cap_burst_events}/{cap_burst_limit}")
        reasons.append(f"玩法 {cap_action} 的去重奖励发放超过配置上限：{'，'.join(limits)}")
    rapid_reward_outflows = int(facts.get("rapid_reward_outflows", 0))
    rapid_reward_outflow_days = int(facts.get("rapid_reward_outflow_days", 0))
    reward_outflow_targets = int(facts.get("reward_outflow_target_peers", 0))
    if rapid_reward_outflows >= 5 and rapid_reward_outflow_days >= 3 and 1 <= reward_outflow_targets <= 2:
        score += 20
        tags.append("奖励快速归集")
        reasons.append(f"{rapid_reward_outflows} 次奖励后 10 分钟内转出道具，跨 {rapid_reward_outflow_days} 天集中到 {reward_outflow_targets} 个目标角色")
    if int(facts.get("ground_handoffs", 0)):
        score += 35
        tags.append("绕过交易转移")
        reasons.append("存在角色丢到地面后由另一角色拾取的资产转移")
    unexplained_gold = int(facts.get("unexplained_gold_increase", 0))
    if unexplained_gold:
        jump_count = int(facts.get("unexplained_gold_jumps", 0))
        score += min(40, 25 + jump_count * 3)
        tags.append("元宝快照跳增")
        reasons.append(f"金元宝快照出现 {jump_count} 次跳增，累计 {number(unexplained_gold)}，在已接入来源日志中未找到对应记录")
    transfer_count = int(facts.get("transfer_count", 0))
    if transfer_count >= 20:
        score += 20
        tags.append("高频流转")
        reasons.append("近 30 天资产流转次数较高")
    return min(score, 100), tags or ["未见强异常"], reasons


def status_for(score: int) -> tuple[str, str]:
    if score >= 70:
        return "高风险", "danger"
    if score >= 35:
        return "观察", "warning"
    return "正常", "safe"


def gold_snapshot_jumps(rows: list[dict], minimum: int = 1_000_000) -> list[dict]:
    jumps = []
    previous = None
    for row in rows:
        if previous:
            delta = int(row["gold_coin"] or 0) - int(previous["gold_coin"] or 0)
            if delta >= minimum:
                jumps.append({"from": previous["update_time"], "to": row["update_time"], "amount": delta})
        previous = row
    return jumps


def unexplained_gold_jumps(db, gid: str) -> list[dict]:
    snapshots = list(reversed(fetch_all(
        db,
        "select update_time,gold_coin from dl_ldb_1.login_log where gid=%s order by update_time desc, id desc limit 500",
        (gid,),
    )))
    candidates = sorted(gold_snapshot_jumps(snapshots), key=lambda row: row["amount"], reverse=True)[:8]
    unexplained = []
    # ponytail: check only the eight largest jumps; replace with one aggregated ledger query when a busy server exceeds this review window.
    for row in candidates:
        evidence = fetch_one(
            db,
            f"""
            select
              (select count(*) from dl_ldb_1.campaign_log where gid=%s and update_time>%s and update_time<=%s and bonus_type=7 and action in ({REWARD_ACTION_SQL_LIST})) +
              (select count(*) from dl_ldb_1.errand_log where gid=%s and update_time>%s and update_time<=%s and bonus_type=7 and action in ({REWARD_ACTION_SQL_LIST})) +
              (select count(*) from dl_ldb_1.coin_order_log where gid=%s and update_time>%s and update_time<=%s) +
              (select count(*) from dl_ldb_1.gbuy_action_log where gid=%s and update_time>%s and update_time<=%s) +
              (select count(*) from dl_ldb_1.gift_log where gid=%s and update_time>%s and update_time<=%s) +
              (select count(*) from dl_ldb_1.important_action_log where gid=%s and update_time>%s and update_time<=%s) count
            """,
            (
                gid, row["from"], row["to"], gid, row["from"], row["to"],
                gid, row["from"], row["to"], gid, row["from"], row["to"],
                gid, row["from"], row["to"], gid, row["from"], row["to"],
            ),
        )["count"]
        if not evidence:
            unexplained.append(row)
    return sorted(unexplained, key=lambda row: row["to"], reverse=True)


def find_player(db, query: str | None) -> dict:
    if query:
        query = query.strip()[:128]
        ascii_query = query if query.isascii() else ""
        row = fetch_one(
            db,
            """
            select update_time,dist,gid,name,account,create_time,last_login_time,
                   last_login_ip,last_login_mac,level,cash,balance,char_status
            from dl_mdb_1.char_info
            where gid=%s or account=%s or name=%s
            limit 1
            """,
            (ascii_query, ascii_query, database_value(query)),
        )
    else:
        row = fetch_one(
            db,
            """
            select update_time,dist,gid,name,account,create_time,last_login_time,
                   last_login_ip,last_login_mac,level,cash,balance,char_status
            from dl_mdb_1.char_info order by last_login_time desc limit 1
            """,
        )
    if not row:
        raise LookupError("未找到匹配玩家")
    return row


def burst_funnel_counts(events: list[tuple[datetime, str]]) -> tuple[int, int]:
    events.sort(key=lambda event: event[0])
    left = 0
    peers = {}
    best = (0, 0)
    for right, (at, peer) in enumerate(events):
        peers[peer] = peers.get(peer, 0) + 1
        while at - events[left][0] > timedelta(minutes=10):
            old_peer = events[left][1]
            peers[old_peer] -= 1
            if not peers[old_peer]:
                del peers[old_peer]
            left += 1
        candidate = (len(peers), right - left + 1)
        if candidate > best:
            best = candidate
    return best


def behavior_rhythm(rows: list[dict]) -> dict:
    unique = set()
    by_day = {}
    by_behavior = {}
    for row in rows:
        try:
            at = datetime.strptime(str(row["update_time"]), "%Y%m%d%H%M%S")
        except ValueError:
            continue
        behavior = str(row["behavior"] or "")
        if (behavior, at) in unique:
            continue
        unique.add((behavior, at))
        by_day.setdefault(at.date(), []).append(at)
        by_behavior.setdefault(behavior, []).append(at)

    result = {
        "max_daily_active_span_minutes": 0,
        "max_daily_active_events": 0,
        "long_active_days": 0,
        "mechanical_action": "",
        "mechanical_action_events": 0,
        "mechanical_interval_seconds": 0,
        "mechanical_interval_ratio_permille": 0,
        "mechanical_span_minutes": 0,
    }
    for times in by_day.values():
        times.sort()
        span = int((times[-1] - times[0]).total_seconds() // 60)
        count = len(times)
        result["max_daily_active_span_minutes"] = max(result["max_daily_active_span_minutes"], span)
        result["max_daily_active_events"] = max(result["max_daily_active_events"], count)
        if span >= 18 * 60 and count >= 100:
            result["long_active_days"] += 1

    best = None
    for behavior, times in by_behavior.items():
        times = sorted(set(times))
        if len(times) < 20:
            continue
        deltas = [int((right - left).total_seconds()) for left, right in zip(times, times[1:]) if right > left]
        if not deltas:
            continue
        counts = {}
        for delta in deltas:
            if 1 <= delta <= 300:
                counts[delta] = counts.get(delta, 0) + 1
        if not counts:
            continue
        interval, repeats = max(counts.items(), key=lambda item: (item[1], -item[0]))
        ratio = repeats * 1000 // len(deltas)
        span = int((times[-1] - times[0]).total_seconds() // 60)
        candidate = (ratio, len(times), span, interval, behavior)
        if best is None or candidate > best:
            best = candidate
    if best:
        ratio, count, span, interval, behavior = best
        result.update({
            "mechanical_action": behavior,
            "mechanical_action_events": count,
            "mechanical_interval_seconds": interval,
            "mechanical_interval_ratio_permille": ratio,
            "mechanical_span_minutes": span,
        })
    return result


def reward_flow(rows: list[dict]) -> dict:
    unique_rewards = set()
    outflows = []
    for row in rows:
        try:
            at = datetime.strptime(str(row["update_time"]), "%Y%m%d%H%M%S")
        except ValueError:
            continue
        if row["kind"] == "reward":
            unique_rewards.add((str(row["action"] or ""), at))
        elif row["target"] not in ("", "(undefined)"):
            outflows.append((at, str(row["target"])))

    by_action = {}
    for action, at in unique_rewards:
        by_action.setdefault(action, []).append(at)
    burst_action = ""
    burst_events = 0
    for action, times in by_action.items():
        times.sort()
        left = 0
        for right, at in enumerate(times):
            while at - times[left] > timedelta(minutes=10):
                left += 1
            count = right - left + 1
            if (count, action) > (burst_events, burst_action):
                burst_events, burst_action = count, action

    reward_times = sorted(at for _, at in unique_rewards)
    outflows.sort()
    waiting = deque()
    reward_index = 0
    days = set()
    targets = set()
    rapid_outflows = 0
    for at, target in outflows:
        while reward_index < len(reward_times) and reward_times[reward_index] <= at:
            waiting.append(reward_times[reward_index])
            reward_index += 1
        while waiting and at - waiting[0] > timedelta(minutes=10):
            waiting.popleft()
        if waiting:
            reward = waiting.popleft()
            rapid_outflows += 1
            days.add(reward.date())
            targets.add(target)
    return {
        "reward_burst_action": burst_action,
        "reward_burst_events": burst_events,
        "rapid_reward_outflows": rapid_outflows,
        "rapid_reward_outflow_days": len(days),
        "reward_outflow_target_peers": len(targets),
    }


def configured_gameplay_cap(rows: list[dict]) -> dict:
    unique = set()
    by_action = {}
    for row in rows:
        if row["kind"] != "reward":
            continue
        try:
            at = datetime.strptime(str(row["update_time"]), "%Y%m%d%H%M%S")
        except ValueError:
            continue
        action = str(row["action"] or "")
        if (action, at) in unique:
            continue
        unique.add((action, at))
        by_action.setdefault(action, []).append(at)

    best = None
    for cap in (cap for cap in GAMEPLAY_CAPS if cap["enabled"]):
        times = sorted(by_action.get(cap["action"], []))
        daily = {}
        for at in times:
            daily[at.date()] = daily.get(at.date(), 0) + 1
        daily_events = max(daily.values(), default=0)
        burst_events = 0
        left = 0
        for right, at in enumerate(times):
            while at - times[left] > timedelta(minutes=10):
                left += 1
            burst_events = max(burst_events, right - left + 1)
        daily_ratio = daily_events * 1000 // cap["dailyLimit"] if cap["dailyLimit"] else 0
        burst_ratio = burst_events * 1000 // cap["burst10mLimit"] if cap["burst10mLimit"] else 0
        facts = {
            "configured_cap_action": cap["action"],
            "configured_cap_daily_events": daily_events,
            "configured_cap_daily_limit": cap["dailyLimit"],
            "configured_cap_burst_events": burst_events,
            "configured_cap_burst_limit": cap["burst10mLimit"],
        }
        candidate = ((max(daily_ratio, burst_ratio), daily_events + burst_events, cap["action"]), facts)
        if best is None or candidate[0] > best[0]:
            best = candidate
    return best[1] if best else {
        "configured_cap_action": "",
        "configured_cap_daily_events": 0,
        "configured_cap_daily_limit": 0,
        "configured_cap_burst_events": 0,
        "configured_cap_burst_limit": 0,
    }


def player_facts(db, player: dict, median_gold_coin: int) -> dict:
    gid, account = player["gid"], player["account"]
    since = (datetime.now() - timedelta(days=30)).strftime("%Y%m%d%H%M%S")
    abnormal = fetch_one(
        db,
        """
        select count(*) count from dl_ldb_1.important_log
        where type='check_coin' and action='abnormal_coin_num' and para1=%s
        """,
        (account,),
    )["count"]
    coin_balance = fetch_one(
        db,
        """
        select gold_coin,silver_coin,update_time from dl_ldb_1.login_log
        where gid=%s order by update_time desc limit 1
        """,
        (gid,),
    ) or {"gold_coin": 0, "silver_coin": 0, "update_time": ""}
    transfer_rows = fetch_all(
        db,
        """
        select update_time,transfer_id,action,item_iid,gid_from,gid_to,mac_from,mac_to
        from dl_ldb_1.item_transfer_log
        where update_time >= %s and (gid_from=%s or gid_to=%s)
        """,
        (since, gid, gid),
    )
    grouped = {}
    transfer_ids = set()
    peers = set()
    same_device_peers = set()
    asset_flow_by_peer = {}
    inbound_asset_events = []
    asset_directions = {}
    for row in transfer_rows:
        if row["transfer_id"]:
            transfer_ids.add(row["transfer_id"])
        if row["action"] == "bait" and row["transfer_id"]:
            grouped.setdefault(row["transfer_id"], {"item": False, "coin": False})
            grouped[row["transfer_id"]]["item" if row["item_iid"] else "coin"] = True
        peer = row["gid_to"] if row["gid_from"] == gid else row["gid_from"]
        if peer:
            peers.add(peer)
            if row["mac_from"] and row["mac_from"] == row["mac_to"]:
                same_device_peers.add(peer)
        if row["item_iid"]:
            gid_from, gid_to = row["gid_from"], row["gid_to"]
            iid = normalized_iid(row["item_iid"])
            if gid_to == gid and gid_from not in ("", "(undefined)", gid):
                asset_flow_by_peer.setdefault(gid_from, [0, 0])[0] += 1
                if iid:
                    try:
                        inbound_asset_events.append((datetime.strptime(str(row["update_time"]), "%Y%m%d%H%M%S"), gid_from))
                    except ValueError:
                        pass
                    asset_directions.setdefault((gid_from, iid), [False, False])[0] = True
            elif gid_from == gid and gid_to not in ("", "(undefined)", gid):
                asset_flow_by_peer.setdefault(gid_to, [0, 0])[1] += 1
                if iid:
                    asset_directions.setdefault((gid_to, iid), [False, False])[1] = True
    unpaired = sum(1 for legs in grouped.values() if not all(legs.values()))
    one_way_sources = [flow for flow in asset_flow_by_peer.values() if flow[0] and not flow[1]]
    funnel_source_peers = len(one_way_sources)
    funnel_asset_rows = sum(flow[0] for flow in one_way_sources)
    burst_funnel_source_peers, burst_funnel_asset_rows = burst_funnel_counts(inbound_asset_events)
    returned = [(peer, iid) for (peer, iid), directions in asset_directions.items() if all(directions)]
    returned_asset_ids = len({iid for _, iid in returned})
    returned_asset_peers = len({peer for peer, _ in returned})
    rhythm_rows = fetch_all(
        db,
        """
        select update_time,concat('campaign:',action) behavior
          from dl_ldb_1.campaign_log where update_time >= %s and gid=%s
        union all select update_time,concat('errand:',action)
          from dl_ldb_1.errand_log where update_time >= %s and gid=%s
        union all select update_time,concat('user:',action)
          from dl_ldb_1.user_log where update_time >= %s and para1=%s
        union all select update_time,concat('transfer:',action)
          from dl_ldb_1.item_transfer_log where update_time >= %s and gid_from=%s
        """,
        (since, gid, since, gid, since, gid, since, gid),
    )
    rhythm = behavior_rhythm(rhythm_rows)
    reward_flow_rows = fetch_all(
        db,
        f"""
        select update_time,action,'' target,'reward' kind
          from dl_ldb_1.campaign_log where update_time >= %s and gid=%s
            and action in ({REWARD_ACTION_SQL_LIST}) and bonus_type in (1,2,3,7,14)
        union all select update_time,action,'','reward'
          from dl_ldb_1.errand_log where update_time >= %s and gid=%s
            and action in ({REWARD_ACTION_SQL_LIST}) and bonus_type in (1,2,3,7,14)
        union all select update_time,action,gid_to,'transfer'
          from dl_ldb_1.item_transfer_log where update_time >= %s and gid_from=%s
            and gid_to not in ('','(undefined)') and gid_to<>gid_from and item_iid<>''
        """,
        (since, gid, since, gid, since, gid),
    )
    reward_flow_facts = reward_flow(reward_flow_rows)
    gameplay_cap_facts = configured_gameplay_cap(reward_flow_rows)
    last_login_mac = str(player.get("last_login_mac") or "")
    shared_device_accounts = fetch_one(
        db,
        "select count(distinct account) count from dl_mdb_1.char_info where last_login_mac=%s and last_login_mac<>''",
        (last_login_mac,),
    )["count"] if len(last_login_mac) >= 8 else 0
    last_login_ip = str(player.get("last_login_ip") or "")
    shared_ip_accounts = fetch_one(
        db,
        "select count(distinct account) count from dl_mdb_1.char_info where last_login_ip=%s and last_login_ip<>''",
        (last_login_ip,),
    )["count"] if len(last_login_ip) >= 7 else 0
    item_count = fetch_one(
        db,
        "select coalesce(sum(amount),0) count from dl_mdb_1.item_info where owner=%s",
        (gid,),
    )["count"]
    pet_count = fetch_one(
        db,
        "select count(*) count from dl_mdb_1.pet_info where owner=%s",
        (gid,),
    )["count"]
    ground_handoffs = fetch_one(
        db,
        """
        select count(distinct transfer_id) count
        from dl_ldb_1.item_transfer_log
        where action='diuqsq' and gid_from=%s
          and gid_to not in ('','(undefined)') and gid_to<>gid_from
        """,
        (gid,),
    )["count"]
    gold_jumps = unexplained_gold_jumps(db, gid)
    reward_count = fetch_one(
        db,
        f"""
        select
          (select count(*) from dl_ldb_1.campaign_log where update_time >= %s and gid=%s and action in ({REWARD_ACTION_SQL_LIST}) and bonus_type in (1,2,3,7,14)) +
          (select count(*) from dl_ldb_1.errand_log where update_time >= %s and gid=%s and action in ({REWARD_ACTION_SQL_LIST}) and bonus_type in (1,2,3,7,14)) +
          (select count(*) from dl_ldb_1.pet_log where update_time >= %s and gid=%s and action='jianglcw') count
        """,
        (since, gid, since, gid, since, gid),
    )["count"]
    return {
        "cash": player["cash"],
        "gold_coin": coin_balance["gold_coin"],
        "silver_coin": coin_balance["silver_coin"],
        "coin_observed_at": coin_balance["update_time"],
        "median_gold_coin": median_gold_coin,
        "abnormal_coin": abnormal,
        "transfer_count": len(transfer_ids),
        "unpaired_transfers": unpaired,
        "same_device_peers": len(same_device_peers),
        "funnel_source_peers": funnel_source_peers,
        "funnel_asset_rows": funnel_asset_rows,
        "burst_funnel_source_peers": burst_funnel_source_peers,
        "burst_funnel_asset_rows": burst_funnel_asset_rows,
        "returned_asset_ids": returned_asset_ids,
        "returned_asset_peers": returned_asset_peers,
        **rhythm,
        **reward_flow_facts,
        **gameplay_cap_facts,
        "shared_device_accounts": shared_device_accounts,
        "shared_ip_accounts": shared_ip_accounts,
        "ground_handoffs": ground_handoffs,
        "unexplained_gold_jumps": len(gold_jumps),
        "unexplained_gold_increase": sum(row["amount"] for row in gold_jumps),
        "gold_jumps": gold_jumps,
        "peers": len(peers),
        "item_count": item_count,
        "pet_count": pet_count,
        "reward_count": reward_count,
    }


def reward_change(row: dict) -> str:
    kind = REWARD_TYPES.get(int(row["bonus_type"]), f"类型 {row['bonus_type']}")
    value = row["bonus_name"] or row["bonus_prop"] or "未记录数值"
    if kind == "道具":
        return f"道具 {value}"
    if kind == "宠物":
        return f"宠物 {value}"
    return f"{number(value) if str(value).isdigit() else value} {kind}"


def activity_direction(action: str) -> tuple[str, str]:
    if action in REWARD_ACTIONS:
        return "获得记录", "+"
    if action in CONFIRMED_COST_ACTIONS:
        return "消耗记录", "-"
    return "资产事件", ""


def missing_gid(value) -> bool:
    return not value or value == "(undefined)"


def transfer_timeline_event(row: dict, gid: str):
    amount = number(row["item_amount"] or 0)
    item_name = row["item_name"] or "未知道具"
    iid_note = f"IID {row['item_iid']}" if row["item_iid"] else "堆叠资产"
    if row["action"] == "diuqsq":
        if row["gid_to"] == gid:
            return "拾取资产", f"+{amount} {item_name}", f"地面拾取 / 原持有人 {row['gid_from']} / {iid_note}"
        if row["gid_from"] == gid and missing_gid(row["gid_to"]):
            return "丢弃资产", f"-{amount} {item_name}", f"进入地面或销毁 / {iid_note}"
        return None
    incoming = row["gid_to"] == gid
    action_name = {"bait": "摆摊", "jiaoy": "玩家交易"}.get(row["action"], "道具转移")
    if row["item_iid"]:
        note = "金钱腿与道具腿成对核对" if row["action"] == "bait" else f"原始动作 {row['action']}"
        return f"{action_name}{'收取' if incoming else '转出'}", f"{'+' if incoming else '-'}{amount} {item_name}", note
    return f"{action_name}{'收款' if incoming else '付款'}", f"{'+' if incoming else '-'}{amount} 金钱", "交易流水"


def transfer_trace_action(row: dict) -> str:
    if row["action"] == "diuqsq":
        return "地面拾取" if not missing_gid(row["gid_to"]) else "丢弃到地面"
    return {"bait": "摆摊转移", "jiaoy": "玩家交易"}.get(row["action"], f"资产转移 {row['action']}")


def timeline(db, player: dict, facts: dict | None = None) -> list[list[str]]:
    gid, account = player["gid"], player["account"]
    events = []
    for row in (facts or {}).get("gold_jumps", []):
        events.append((row["to"], "金元宝快照跳增", f"+{number(row['amount'])} 金元宝", f"{stamp_label(row['from'])} 后未找到已接入来源"))
    transfers = fetch_all(
        db,
        """
        select update_time,action,gid_from,gid_to,item_iid,item_name,item_amount,memo
        from dl_ldb_1.item_transfer_log
        where gid_from=%s or gid_to=%s order by update_time desc, id desc limit 40
        """,
        (gid, gid),
    )
    for row in transfers:
        event = transfer_timeline_event(row, gid)
        if event:
            events.append((row["update_time"], *event))
    user_events = fetch_all(
        db,
        """
        select update_time,type,action,para1,para2,para3,memo
        from dl_ldb_1.user_log
        where (para1=%s or (action='exchange' and para3=%s))
          and action in ('buy','take_stall_cash','drop_pet')
        order by update_time desc, id desc limit 20
        """,
        (gid, gid),
    )
    for row in user_events:
        if row["action"] == "take_stall_cash":
            events.append((row["update_time"], "摆摊资金取回", f"+{number(row['para3'])} 金钱", row["memo"] or "摆摊账户"))
        elif row["action"] == "drop_pet":
            events.append((row["update_time"], "丢弃宠物", f"-1 {row['para3']}", f"宠物 IID {row['para2']}"))
        else:
            iid_note = f"IID {row['para2']}" if row["para2"] and row["para2"] != "U" else "堆叠道具"
            events.append((row["update_time"], "NPC 商店购买", "+道具", f"{row['para3']} / {iid_note}"))
    costs = fetch_all(
        db,
        """
        select update_time,item_name,amount,cost,cost_type from dl_ldb_1.cost_coin_log
        where account=%s or gid=%s order by update_time desc, id desc limit 20
        """,
        (account, gid),
    )
    for row in costs:
        coin_name = COIN_LABELS.get(row["cost_type"], row["cost_type"] or "货币")
        events.append((row["update_time"], "商城购买", f"-{number(row['cost'])} {coin_name} / +{row['amount']} {row['item_name']}", row["cost_type"]))
    adjustments = fetch_all(
        db,
        """
        select update_time,type,action,cash,memo from dl_ldb_1.money_log
        where gid=%s and action not in (1,31,32) order by update_time desc, id desc limit 20
        """,
        (gid,),
    )
    for row in adjustments:
        money_actions = {14: "装备修理消耗", 26: "装备养成消耗"}
        prefix = "-" if row["action"] in money_actions else ""
        events.append((row["update_time"], money_actions.get(row["action"], f"金钱事件 #{row['action']}"), f"{prefix}{number(row['cash'])} 金钱", row["memo"] or "服务端记账"))
    rewards = fetch_all(
        db,
        """
        select update_time,action,bonus_type,bonus_name,bonus_prop,'campaign_log' source_table,id
        from dl_ldb_1.campaign_log where gid=%s and bonus_type in (1,2,3,7,14)
        union all
        select update_time,action,bonus_type,bonus_name,bonus_prop,'errand_log' source_table,id
        from dl_ldb_1.errand_log where gid=%s and bonus_type in (1,2,3,7,14)
        order by update_time desc, source_table asc, id desc limit 30
        """,
        (gid, gid),
    )
    for row in rewards:
        source = row["bonus_prop"] if row["bonus_type"] in (2, 3) and row["bonus_prop"] else row["source_table"]
        title, prefix = activity_direction(row["action"])
        events.append((row["update_time"], f"{title} · {row['action']}", f"{prefix}{reward_change(row)}", source))
    pet_rewards = fetch_all(
        db,
        """
        select update_time,pet_name,pet_iid from dl_ldb_1.pet_log
        where gid=%s and action='jianglcw' order by update_time desc, id desc limit 20
        """,
        (gid,),
    )
    for row in pet_rewards:
        events.append((row["update_time"], "奖励获得宠物", f"+宠物 {row['pet_name']}", f"pet_log / IID {row['pet_iid']}"))
    events.sort(key=lambda event: event[0], reverse=True)
    return [[stamp_label(stamp), action, change, note] for stamp, action, change, note in events[:12]] or [["-", "暂无资产事件", "0", "当前日志范围内无记录"]]


def player_result(db, query: str | None = None) -> dict:
    player = find_player(db, query)
    coin_rows = fetch_all(
        db,
        """
        select l.gid,l.gold_coin from dl_ldb_1.login_log l
        inner join (
          select gid,max(update_time) update_time from dl_ldb_1.login_log
          where gid<>'' group by gid
        ) latest on latest.gid=l.gid and latest.update_time=l.update_time
        """,
    )
    median_gold_coin = int(statistics.median([int(row["gold_coin"] or 0) for row in coin_rows])) if coin_rows else 0
    facts = player_facts(db, player, median_gold_coin)
    score, tags, reasons = risk_score(facts)
    status, tone = status_for(score)
    summary = "；".join(reasons) + "。" if reasons else "当前权威日志中未发现可直接定性的异常，仍需结合玩法产出日志复核。"
    return {
        "id": player["gid"],
        "name": player["name"],
        "account": player["account"],
        "server": player["dist"],
        "level": player["level"],
        "score": score,
        "status": status,
        "statusTone": tone,
        "tags": tags,
        "summary": summary,
        "metrics": [
            ["金元宝 / 银元宝", f"{number(facts['gold_coin'])} / {number(facts['silver_coin'])}"],
            ["当前金钱", number(player["cash"])],
            ["持有道具 / 宠物", f"{number(facts['item_count'])} / {number(facts['pet_count'])}"],
            ["30 天交易 / 短时扇入", f"{number(facts['transfer_count'])} / {number(facts['burst_funnel_source_peers'])}"],
            ["单日跨度 / 事件", f"{facts['max_daily_active_span_minutes'] // 60} 小时 {facts['max_daily_active_span_minutes'] % 60} 分 / {number(facts['max_daily_active_events'])}"],
            ["动作周期 / 重复率", "未形成 / 0%" if not facts["mechanical_action"] else f"{facts['mechanical_interval_seconds']} 秒 / {facts['mechanical_interval_ratio_permille'] // 10}%"],
            ["奖励爆发 / 快速归集", f"{number(facts['reward_burst_events'])} / {number(facts['rapid_reward_outflows'])}"],
            ["玩法峰值 / 配置上限", "尚未配置" if not facts["configured_cap_action"] else f"{facts['configured_cap_action']} · 日 {facts['configured_cap_daily_events']}/{facts['configured_cap_daily_limit']} · 10分 {facts['configured_cap_burst_events']}/{facts['configured_cap_burst_limit']}"],
        ],
        "timeline": timeline(db, player, facts),
        "evidence": facts,
    }


def normalized_iid(value: str) -> str:
    return value.strip().strip(":").upper()[:96]


def prepare_ledger(db: sqlite3.Connection) -> None:
    db.executescript(
        """
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
        """
    )


def apply_snapshot(db: sqlite3.Connection, rows: list[dict], scanned_at: str) -> dict:
    prepare_ledger(db)
    initialized = db.execute("select value from ledger_meta where key='initialized'").fetchone() is not None
    columns = ("iid", "name", "owner", "owner_name", "env", "pos", "amount", "present", "last_seen")
    previous = {row[0]: dict(zip(columns, row)) for row in db.execute("select iid,name,owner,owner_name,env,pos,amount,present,last_seen from asset_state")}
    current = {normalized_iid(row["iid"]): row for row in rows if normalized_iid(row["iid"])}
    counts = {"baseline": 0, "first_seen": 0, "owner_changed": 0, "amount_changed": 0, "missing": 0}

    def event(kind: str, iid: str, row: dict, owner_from="", amount_before=0) -> None:
        db.execute(
            """
            insert into asset_event(event_time,event_type,iid,name,owner_from,owner_to,amount_before,amount_after,evidence)
            values(?,?,?,?,?,?,?,?,?)
            """,
            (
                scanned_at,
                kind,
                iid,
                row.get("name", ""),
                owner_from,
                row.get("owner", ""),
                int(amount_before or 0),
                int(row.get("amount", 0) or 0),
                json.dumps({"env": row.get("env", ""), "pos": row.get("pos", 0)}, ensure_ascii=False),
            ),
        )
        counts[kind] += 1

    for iid, row in current.items():
        old = previous.get(iid)
        if not old:
            event("first_seen" if initialized else "baseline", iid, row)
        elif not old["present"]:
            event("first_seen", iid, row, old["owner"], old["amount"])
        elif old["owner"] != row["owner"]:
            event("owner_changed", iid, row, old["owner"], old["amount"])
        if old and int(old["amount"]) != int(row["amount"] or 0):
            event("amount_changed", iid, row, old["owner"], old["amount"])
        db.execute(
            """
            insert into asset_state(iid,name,owner,owner_name,env,pos,amount,present,last_seen)
            values(?,?,?,?,?,?,?,?,?)
            on conflict(iid) do update set name=excluded.name,owner=excluded.owner,
              owner_name=excluded.owner_name,env=excluded.env,pos=excluded.pos,
              amount=excluded.amount,present=1,last_seen=excluded.last_seen
            """,
            (iid, row["name"], row["owner"], row["owner_name"], row["env"], int(row["pos"] or 0), int(row["amount"] or 0), 1, scanned_at),
        )
    for iid, old in previous.items():
        if old["present"] and iid not in current:
            missing_row = {"name": old["name"], "owner": "", "amount": 0, "env": "", "pos": 0}
            event("missing", iid, missing_row, old["owner"], old["amount"])
            db.execute("update asset_state set present=0,last_seen=? where iid=?", (scanned_at, iid))
    db.execute("insert or replace into ledger_meta(key,value) values('initialized',?)", (scanned_at,))
    db.commit()
    return {"scanned": len(current), "changes": counts}


def ledger_path() -> Path:
    path = Path(os.environ.get("RISK_DB_PATH", Path(__file__).resolve().parent.parent / "data" / "risk.db"))
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def collect_once(db) -> dict:
    rows = fetch_all(
        db,
        """
        select iid,name,owner,owner_name,env,pos,amount from dl_mdb_1.item_info
        union all
        select iid,name,owner,owner_name,env,pos,1 amount from dl_mdb_1.pet_info
        """,
    )
    with sqlite3.connect(ledger_path()) as ledger:
        return {"ok": True, **apply_snapshot(ledger, rows, datetime.now().isoformat(timespec="seconds"))}


def ledger_events(iid: str) -> list[dict]:
    path = ledger_path()
    if not path.exists():
        return []
    with sqlite3.connect(path) as ledger:
        prepare_ledger(ledger)
        ledger.row_factory = sqlite3.Row
        return [dict(row) for row in ledger.execute("select * from asset_event where iid=? order by event_time", (iid,))]


def asset_result(db, query: str | None = None) -> dict:
    if query:
        iid = normalized_iid(query)
        current = fetch_one(
            db,
            "select *, 'item' asset_kind from dl_mdb_1.item_info where replace(iid,':','')=%s limit 1",
            (iid,),
        )
        if not current:
            current = fetch_one(
                db,
                """
                select update_time,dist,owner,pos,owner_name,name,env,1 amount,iid,'pet' asset_kind
                from dl_mdb_1.pet_info where replace(iid,':','')=%s limit 1
                """,
                (iid,),
            )
    else:
        current = fetch_one(db, "select *, 'item' asset_kind from dl_mdb_1.item_info order by update_time desc limit 1")
        iid = normalized_iid(current["iid"]) if current else ""
    transfer_rows = fetch_all(
        db,
        "select * from dl_ldb_1.item_transfer_log where replace(item_iid,':','')=%s order by update_time, id",
        (iid,),
    )
    equipment_rows = fetch_all(
        db,
        "select * from dl_ldb_1.equipment_log where replace(item_iid,':','')=%s order by update_time, id",
        (iid,),
    )
    apply_rows = fetch_all(
        db,
        "select * from dl_ldb_1.apply_log where replace(iid,':','')=%s order by update_time, id",
        (iid,),
    )
    cost_rows = fetch_all(
        db,
        "select * from dl_ldb_1.cost_coin_log where replace(uid,':','')=%s order by update_time, id",
        (iid,),
    )
    iid_pattern = f"%{iid}%"
    activity_rows = fetch_all(
        db,
        """
        select update_time,action,gid,bonus_type,bonus_name,bonus_prop,'campaign_log' source_table,id
        from dl_ldb_1.campaign_log where bonus_type in (1,14) and replace(bonus_prop,':','') like %s
        union all
        select update_time,action,gid,bonus_type,bonus_name,bonus_prop,'errand_log' source_table,id
        from dl_ldb_1.errand_log where bonus_type in (1,14) and replace(bonus_prop,':','') like %s
        order by update_time, source_table, id
        """,
        (iid_pattern, iid_pattern),
    )
    user_rows = fetch_all(
        db,
        """
        select update_time,type,action,para1,para2,para3,memo
        from dl_ldb_1.user_log
        where replace(para2,':','')=%s
          and action in ('drop','get','exchange','buy','drop_pet')
        order by update_time, id
        """,
        (iid,),
    )
    pet_rows = fetch_all(
        db,
        """
        select update_time,gid,type,action,pet_name,pet_iid,cost_item,item_iid,para1,para2,para3
        from dl_ldb_1.pet_log where replace(pet_iid,':','')=%s order by update_time, id
        """,
        (iid,),
    )
    important_pet_rows = fetch_all(
        db,
        """
        select update_time,action,gid_from,gid_to,pet_iid,pet_name
        from dl_ldb_1.important_pet_log where replace(pet_iid,':','')=%s order by update_time, id
        """,
        (iid,),
    )
    snapshot_rows = ledger_events(iid)
    origin_rows = [row for row in activity_rows if row["action"] in REWARD_ACTIONS]
    user_origin_rows = [row for row in user_rows if row["action"] == "buy"]
    pet_origin_rows = [row for row in pet_rows if row["action"] == "jianglcw"]
    if not current and not transfer_rows and not equipment_rows and not apply_rows and not cost_rows and not activity_rows and not user_rows and not pet_rows and not important_pet_rows and not snapshot_rows:
        raise LookupError("未找到资产流水")
    nodes = []
    snapshot_labels = {
        "baseline": ("账本基线", "接入时已存在，不视为生成事件"),
        "first_seen": ("快照首次观察", "两次扫描之间进入当前持有表"),
        "owner_changed": ("快照持有人变化", "持有人字段发生变化"),
        "amount_changed": ("快照数量变化", "堆叠数量发生变化"),
        "missing": ("离开当前持有表", "可能被使用、丢弃或转入未覆盖容器"),
    }
    for row in snapshot_rows:
        action, note = snapshot_labels.get(row["event_type"], (row["event_type"], "本地快照事件"))
        owner = f"{row['owner_from'] or '-'} → {row['owner_to'] or '-'}"
        nodes.append((row["event_time"], action, owner, note))
    for row in activity_rows:
        action = "游戏奖励发放" if row["action"] in REWARD_ACTIONS else "游戏资产操作"
        nodes.append((row["update_time"], action, row["gid"], f"{row['source_table']} / {row['action']} / {row['bonus_name']}"))
    user_action_names = {
        "drop": "玩家丢弃",
        "get": "玩家拾取",
        "exchange": "玩家当面交易",
        "buy": "NPC 商店购买",
        "drop_pet": "玩家丢弃宠物",
    }
    for row in user_rows:
        owner = row["para1"]
        if row["action"] == "exchange":
            owner = f"{row['para3']} → {row['para1']}"
        nodes.append((row["update_time"], user_action_names[row["action"]], owner, f"user_log / IID {row['para2']}"))
    pet_action_names = {
        "jianglcw": "奖励获得宠物",
        "yiq": "宠物丢弃",
        "chaojssd": "宠物培养",
        "dianhkq": "宠物点化开启",
        "dianhtslq": "宠物点化培养",
    }
    for row in pet_rows:
        action = pet_action_names.get(row["action"], f"宠物操作 {row['action']}")
        note = f"pet_log / {row['type'] or '-'}"
        if row["cost_item"]:
            note += f" / 消耗 {row['cost_item']}"
        nodes.append((row["update_time"], action, row["gid"], note))
    for row in important_pet_rows:
        nodes.append((row["update_time"], "重要宠物所有权记录", f"{row['gid_from']} → {row['gid_to']}", f"important_pet_log / {row['action']}"))
    for row in cost_rows:
        nodes.append((row["update_time"], "商城生成", row["gid"], f"购买 {row['amount']} 件，消耗 {number(row['cost'])} {row['cost_type']}"))
    for row in apply_rows:
        nodes.append((row["update_time"], "商城发放", row["gid"], f"商品来源 {row['item_source']}，价格 {number(row['item_price'])}"))
    for row in transfer_rows:
        nodes.append((row["update_time"], transfer_trace_action(row), f"{row['gid_from']} → {row['gid_to']}", f"交易号 {row['transfer_id']}"))
    for row in equipment_rows:
        nodes.append((row["update_time"], f"装备操作 {row['action']}", row["gid"], f"结果 {row['oper_result'] or 0}"))
    if current:
        nodes.append((current["update_time"], "当前持有", f"{current['owner_name']} / {current['owner']}", f"{current['env']}位置 {current['pos']}"))
    nodes.sort(key=lambda node: node[0])
    unique_rows = fetch_one(
        db,
        """
        select
          (select count(*) from dl_mdb_1.item_info where replace(iid,':','')=%s) +
          (select count(*) from dl_mdb_1.pet_info where replace(iid,':','')=%s) count
        """,
        (iid, iid),
    )["count"]
    has_source = bool(cost_rows or apply_rows or origin_rows or user_origin_rows or pet_origin_rows)
    risk = 0
    notes = []
    if unique_rows > 1:
        risk += 80
        notes.append("唯一序列号重复")
    if not has_source:
        risk += 30
        notes.append("生成来源尚未覆盖")
    transfer_ids = {row["transfer_id"] for row in transfer_rows if row["action"] == "bait" and row["transfer_id"]}
    for transfer_id in transfer_ids:
        legs = fetch_all(db, "select item_iid from dl_ldb_1.item_transfer_log where transfer_id=%s", (transfer_id,))
        if not any(row["item_iid"] for row in legs) or not any(not row["item_iid"] for row in legs):
            risk += 30
            notes.append("交易账本缺腿")
            break
    risk = min(risk, 100)
    state = "唯一性冲突" if unique_rows > 1 else ("证据不完整" if notes else "链路可闭合")
    user_name_rows = [row for row in user_rows if row["action"] in ("drop", "get", "drop_pet") and row["para3"]]
    pet_name_rows = [row for row in pet_rows if row["pet_name"]]
    name = current["name"] if current else (
        transfer_rows[-1]["item_name"] if transfer_rows else (
            activity_rows[-1]["bonus_name"] if activity_rows else (
                user_name_rows[-1]["para3"] if user_name_rows else (
                    pet_name_rows[-1]["pet_name"] if pet_name_rows else (
                        snapshot_rows[-1]["name"] if snapshot_rows else "未知资产"
                    )
                )
            )
        )
    )
    return {
        "id": f":{iid}:",
        "name": name,
        "quantity": current["amount"] if current else 0,
        "state": state,
        "risk": risk,
        "owner": f"{current['owner_name']} / {current['owner']}" if current else "已离开当前持有表",
        "source": "游戏奖励日志" if origin_rows or pet_origin_rows else ("商城权威日志" if cost_rows or apply_rows or user_origin_rows else "现有日志最早节点"),
        "nodes": [[stamp_label(stamp), action, owner, note] for stamp, action, owner, note in nodes],
    }


def all_player_results(db) -> list[dict]:
    rows = fetch_all(db, "select gid from dl_mdb_1.char_info order by gid")
    return [player_result(db, row["gid"]) for row in rows]


def alerts_result(db) -> list[dict]:
    alerts = []
    today = datetime.now().strftime("%Y%m%d")
    rule_by_tag = {
        "交易账本缺腿": "交易账本不守恒",
        "币值校验异常": "服务端币值校验异常",
        "元宝存量偏离": "元宝存量显著偏离",
        "同设备交易": "同设备角色互转",
        "绕过交易转移": "丢弃拾取绕过交易",
        "元宝快照跳增": "元宝增长来源缺失",
        "高频流转": "高频资产流转",
    }
    for player in all_player_results(db):
        if player["score"] < 20:
            continue
        severity = "严重" if player["score"] >= 70 else ("高" if player["score"] >= 45 else "中")
        tag = next((item for item in player["tags"] if item in rule_by_tag), player["tags"][0])
        alerts.append({
            "id": f"R-{today}-{player['id'][-4:]}",
            "time": player["timeline"][0][0],
            "player": f"{player['name']} / {player['id']}",
            "rule": rule_by_tag.get(tag, tag),
            "severity": severity,
            "score": player["score"],
            "state": "待研判",
        })
    return sorted(alerts, key=lambda item: item["score"], reverse=True)


def dashboard_result(db, started_at: float) -> dict:
    today = datetime.now().strftime("%Y%m%d000000")
    counts = fetch_one(
        db,
        """
        select
          (select count(*) from dl_ldb_1.money_log where update_time >= %s) +
          (select count(*) from dl_ldb_1.item_transfer_log where update_time >= %s) +
          (select count(*) from dl_ldb_1.equipment_log where update_time >= %s) +
          (select count(*) from dl_ldb_1.cost_coin_log where update_time >= %s) +
          (select count(*) from dl_ldb_1.apply_log where update_time >= %s) +
          (select count(*) from dl_ldb_1.campaign_log where update_time >= %s and gid<>'') +
          (select count(*) from dl_ldb_1.errand_log where update_time >= %s and gid<>'') +
          (select count(*) from dl_ldb_1.user_log where update_time >= %s and action in ('drop','get','exchange','buy','take_stall_cash','drop_pet')) +
          (select count(*) from dl_ldb_1.pet_log where update_time >= %s) total
        """,
        (today, today, today, today, today, today, today, today, today),
    )["total"]
    players = all_player_results(db)
    alerts = alerts_result_from_players(players)
    asset_count = fetch_one(
        db,
        "select (select count(*) from dl_mdb_1.item_info) + (select count(*) from dl_mdb_1.pet_info) count",
    )["count"]
    abnormal_today = fetch_one(
        db,
        "select count(*) count from dl_ldb_1.important_log where update_time >= %s and action='abnormal_coin_num'",
        (today,),
    )["count"]
    risk_counts = {
        "正常": sum(1 for player in players if player["score"] < 35),
        "观察": sum(1 for player in players if 35 <= player["score"] < 70),
        "高风险": sum(1 for player in players if player["score"] >= 70),
    }
    total_players = max(1, len(players))
    bands = [
        ["正常", round(risk_counts["正常"] * 100 / total_players, 1), "green"],
        ["观察", round(risk_counts["观察"] * 100 / total_players, 1), "gold"],
        ["高风险", round(risk_counts["高风险"] * 100 / total_players, 1), "coral"],
        ["已阻断", 0, "dark"],
    ]
    since = (datetime.now() - timedelta(hours=12)).strftime("%Y%m%d%H%M%S")
    recent = fetch_all(
        db,
        """
        select update_time from dl_ldb_1.money_log where update_time >= %s
        union all select update_time from dl_ldb_1.item_transfer_log where update_time >= %s
        union all select update_time from dl_ldb_1.equipment_log where update_time >= %s
        union all select update_time from dl_ldb_1.cost_coin_log where update_time >= %s
        union all select update_time from dl_ldb_1.campaign_log where update_time >= %s and gid<>''
        union all select update_time from dl_ldb_1.errand_log where update_time >= %s and gid<>''
        union all select update_time from dl_ldb_1.user_log where update_time >= %s and action in ('drop','get','exchange','buy','take_stall_cash','drop_pet')
        union all select update_time from dl_ldb_1.pet_log where update_time >= %s
        """,
        (since, since, since, since, since, since, since, since),
    )
    bins = [0] * 12
    now = datetime.now()
    for row in recent:
        try:
            event_time = datetime.strptime(str(row["update_time"]), "%Y%m%d%H%M%S")
            age = int((now - event_time).total_seconds() // 3600)
            if 0 <= age < 12:
                bins[11 - age] += 1
        except ValueError:
            pass
    peak = max(bins) if bins else 0
    distribution = [round(value * 100 / peak) if peak else 0 for value in bins]
    latency = max(1, round((time.perf_counter() - started_at) * 1000))
    return {
        "updatedAt": datetime.now().isoformat(),
        "sourceMode": "live",
        "headline": "真实资产账本已连接",
        "description": "当前页面直接读取游戏数据库，展示元宝、奖励、道具掉落/拾取、交易与服务端校验记录。",
        "scope": "全部可分析角色",
        "health": {"status": "实时数据已连接", "latency": f"{latency} ms", "coverage": f"{len(ASSET_TABLES)}/{len(ASSET_TABLES)} 数据表", "backlog": len(alerts)},
        "metrics": [
            ["今日资产日志", number(counts), "来自权威日志"],
            ["风险角色", number(sum(1 for player in players if player["score"] >= 35)), f"共 {len(players)} 个角色"],
            ["可溯源资产", number(asset_count), "当前道具与宠物持有表"],
            ["今日币值异常", number(abnormal_today), "服务端校验"],
        ],
        "distribution": distribution,
        "riskBands": bands,
        "alerts": alerts[:4],
    }


def alerts_result_from_players(players: list[dict]) -> list[dict]:
    today = datetime.now().strftime("%Y%m%d")
    result = []
    for player in players:
        if player["score"] < 20:
            continue
        severity = "严重" if player["score"] >= 70 else ("高" if player["score"] >= 45 else "中")
        result.append({
            "id": f"R-{today}-{player['id'][-4:]}",
            "time": player["timeline"][0][0],
            "player": f"{player['name']} / {player['id']}",
            "rule": player["tags"][0],
            "severity": severity,
            "score": player["score"],
            "state": "待研判",
        })
    return sorted(result, key=lambda item: item["score"], reverse=True)


def self_check() -> dict:
    normal, _, _ = risk_score({"gold_coin": 1000, "median_gold_coin": 1000})
    suspicious, tags, _ = risk_score({"gold_coin": 900_000_000, "median_gold_coin": 10_000_000, "abnormal_coin": 2, "unpaired_transfers": 1})
    assert normal == 0
    assert suspicious >= 70
    assert "交易账本缺腿" in tags
    assert normalized_iid(":6a617f69000102542fd9:") == "6A617F69000102542FD9"
    assert reward_change({"bonus_type": 7, "bonus_name": "9582200", "bonus_prop": ""}) == "9,582,200 元宝"
    assert activity_direction("huilcbjl") == ("获得记录", "+")
    assert activity_direction("yuancxl") == ("消耗记录", "-")
    assert activity_direction("shouszh") == ("资产事件", "")
    handoff_score, handoff_tags, _ = risk_score({"ground_handoffs": 1})
    assert handoff_score == 35 and "绕过交易转移" in handoff_tags
    jumps = gold_snapshot_jumps([
        {"update_time": "20260101000000", "gold_coin": 10},
        {"update_time": "20260101000100", "gold_coin": 1_000_009},
        {"update_time": "20260101000200", "gold_coin": 1_000_010},
    ])
    assert jumps == []
    assert gold_snapshot_jumps([
        {"update_time": "20260101000000", "gold_coin": 10},
        {"update_time": "20260101000100", "gold_coin": 1_000_010},
    ])[0]["amount"] == 1_000_000
    drop_row = {"action": "diuqsq", "item_amount": 1, "item_name": "测试道具", "item_iid": ":A1:", "gid_from": "p1", "gid_to": "(undefined)"}
    assert transfer_timeline_event(drop_row, "p1")[0:2] == ("丢弃资产", "-1 测试道具")
    pickup_row = {**drop_row, "gid_to": "p2"}
    assert transfer_timeline_event(pickup_row, "p2")[0:2] == ("拾取资产", "+1 测试道具")
    assert transfer_timeline_event(pickup_row, "p1") is None
    assert transfer_trace_action(pickup_row) == "地面拾取"
    with sqlite3.connect(":memory:") as ledger:
        base = [{"iid": ":A1:", "name": "item", "owner": "p1", "owner_name": "one", "env": "bag", "pos": 1, "amount": 1}]
        assert apply_snapshot(ledger, base, "2026-01-01T00:00:00")["changes"]["baseline"] == 1
        changed = [{**base[0], "owner": "p2", "owner_name": "two", "amount": 2}, {**base[0], "iid": ":A2:"}]
        result = apply_snapshot(ledger, changed, "2026-01-01T00:01:00")
        assert result["changes"]["owner_changed"] == 1
        assert result["changes"]["amount_changed"] == 1
        assert result["changes"]["first_seen"] == 1
        assert apply_snapshot(ledger, [], "2026-01-01T00:02:00")["changes"]["missing"] == 2
    assert COIN_LABELS["gold_coin"] == "金元宝" and COIN_LABELS["silver_coin"] == "银元宝"
    return {"ok": True, "checks": 20}


def connection_test(db) -> dict:
    version = fetch_one(db, "select version() version")["version"]
    tables = fetch_one(
        db,
        """
        select count(*) count from information_schema.tables
        where (table_schema=%s and table_name in ('char_info','item_info','pet_info'))
           or (table_schema=%s and table_name in ('login_log','item_transfer_log','campaign_log','errand_log'))
        """,
        (MAIN_DATABASE, LOG_DATABASE),
    )["count"]
    if int(tables) < 7:
        raise RuntimeError("required RISK tables are missing")
    return {
        "ok": True,
        "message": "数据库连接成功，核心表可读",
        "serverVersion": version,
        "mainDatabase": MAIN_DATABASE,
        "logDatabase": LOG_DATABASE,
        "verifiedTables": int(tables),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=("dashboard", "player", "asset", "alerts", "collect-once", "connection-test", "self-check"))
    parser.add_argument("query", nargs="?")
    args = parser.parse_args()
    if args.operation == "self-check":
        print(json.dumps(self_check(), ensure_ascii=False))
        return 0
    started_at = time.perf_counter()
    db = connect()
    try:
        if args.operation == "dashboard":
            result = dashboard_result(db, started_at)
        elif args.operation == "player":
            result = player_result(db, args.query)
        elif args.operation == "asset":
            result = asset_result(db, args.query)
        elif args.operation == "alerts":
            result = alerts_result(db)
        elif args.operation == "connection-test":
            result = connection_test(db)
        else:
            result = collect_once(db)
        print(json.dumps(result, ensure_ascii=False, default=str))
        return 0
    except LookupError as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=False))
        return 2
    finally:
        db.close()


if __name__ == "__main__":
    raise SystemExit(main())

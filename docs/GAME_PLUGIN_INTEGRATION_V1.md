# 游戏插件实时风控对接规范 v1

状态：v1 接口合同、Rust 本机接收器、支持本机/远程 HTTPS 的跨平台 C ABI SDK、区服级远程凭据和平台网关已实现。
适用对象：游戏服务端插件、网关插件、Rust Agent 和风控平台开发者。
配套文件：`plugin-event-batch.v1.schema.json`、`plugin-event-batch.v1.example.json`、根目录 `plugin_contract_check.mjs`。

## 1. 为什么数据库取数不够

数据库适合历史核对，但不能完整回答以下问题：

- 一次操作是玩家主动触发、系统奖励、GM 指令还是插件补偿。
- 操作在校验前失败、校验后回滚，还是已经提交成功。
- 一笔交易的金钱腿、道具腿和双方角色是否属于同一事务。
- 道具生成时使用了哪一版掉落表、活动配置和奖励上限。
- 玩家当时的地图、会话、设备、延迟、战斗和在线状态。
- 高价值操作发生前能否先做低延迟检查。

因此数据源优先级固定为：

1. 游戏逻辑提交点产生的权威插件事件。
2. 游戏数据库中的权威流水。
3. 当前状态快照和差异账本。
4. 统计推断与模型分数。

数据库适配器继续保留，用于补历史、校准状态和检测插件漏报；插件事件负责实时性与操作上下文。两者不互相替代。

## 2. 推荐拓扑

```text
游戏进程 / 网关插件
  -> Risk SDK（C ABI）
  -> 同机 Rust Agent，或平台 HTTPS 网关
       -> 校验事件格式
       -> 注入 tenant_id / server_id
       -> SQLite/WAL 持久队列
       -> 本地实时规则
       -> mTLS/TLS 批量上报
  -> Go Control API / Rust Risk Engine

游戏数据库（只读）
  -> Rust Agent 增量补采与对账
```

插件与 Agent 不在同一台机器时，使用远程模式：

```text
游戏服插件
  -> HTTPS /sdk/v1/events:batch（区服 SDK 凭据）
  -> 平台网关（验证摘要、限频、注入 tenant_id/server_id）
  -> 平台本机 Rust Agent（仍只监听 127.0.0.1）
```

关键边界：

- 游戏插件不保存授权卡密、租户编号、平台数据库密码或云端 Token。
- `tenant_id`、`server_id` 由 Agent 配置注入，不接受客户端或事件正文覆盖。
- 同机插件只连接本机命名管道、Unix Socket 或 `127.0.0.1`；跨机插件只连接平台 HTTPS 网关，不能直接访问 Agent 端口。
- SDK 负责有限内存重试与 TLS；Agent 负责本机 SQLite/WAL 持久队列。跨机直连网关时，插件必须处理 SDK 队列满和进程退出前 `pgr_flush` 失败，当前 SDK 不承诺跨进程断电续传。
- 第一阶段为影子模式，只记录和告警，不自动封号、扣款或销毁道具。

### 2.1 远程 SDK 凭据

在后台「插件接入」页为每个区服分别生成凭据。密钥只在生成或轮换时显示一次，平台磁盘只保存 SHA-256 摘要。不要把后台登录卡密当作 SDK 凭据。

```http
POST /sdk/v1/events:batch
Authorization: Bearer <PGR_SDK_KEY>
Content-Type: application/json
```

同步检查使用 `POST /sdk/v1/decisions:check`，请求合同与本机 Agent 相同。生产入口必须使用 HTTPS；反向代理部署时设置 `RISK_BEHIND_TLS_PROXY=1`，并只信任受控代理写入的 `X-Forwarded-Proto: https`。`RISK_SDK_ALLOW_INSECURE=1` 只供封闭测试网络使用。

轮换流程：先生成新密钥并更新插件配置，再验证新密钥成功上报；后台轮换操作会立即吊销旧密钥。不同区服不得共享同一密钥。

## 3. 插件需要保留的最小接口

优先给客户提供一个无运行时依赖的动态库。插件调用以下七个 C ABI 函数：

```c
#include <stddef.h>
#include <stdint.h>

#define PGR_ABI_VERSION 1u

typedef struct pgr_config_v1 {
    uint32_t abi_version;
    const char *endpoint_utf8;    /* http://127.0.0.1:<port> 或 https://<平台域名>/sdk/v1 */
    const char *local_token_utf8; /* 本机 Agent Token 或后台生成的区服 SDK 密钥 */
    uint32_t emit_timeout_ms;  /* 异步入队通常 1 ms */
    uint32_t check_timeout_ms; /* 同步决策默认 20 ms */
    uint32_t queue_capacity;   /* 0 使用默认值 256，最大 16384 */
} pgr_config_v1;

int32_t pgr_init(const pgr_config_v1 *config);
int32_t pgr_emit_json(const char *json_utf8, size_t json_len);
int32_t pgr_check_json(
    const char *request_utf8,
    size_t request_len,
    char *response_utf8,
    size_t *response_capacity
);
int32_t pgr_pull_actions(
    const char *request_utf8, size_t request_len,
    char *response_utf8, size_t *response_capacity
);
int32_t pgr_ack_action(
    const char *request_utf8, size_t request_len,
    char *response_utf8, size_t *response_capacity
);
int32_t pgr_flush(uint32_t timeout_ms);
void pgr_shutdown(void);
```

约束：

- ABI 使用 `cdecl`，所有字符串是 UTF-8，接口线程安全。
- `pgr_emit_json` 的参数是一个完整 `plugin-event-batch.v1` 批次，不是单条事件；它只放入 SDK 有界内存队列，不等待云端。
- `pgr_check_json` 只用于少量高价值操作，不用于移动、心跳等高频行为。
- `pgr_pull_actions` 与 `pgr_ack_action` 用于远程平台命令通道；本机 Agent 模式暂不提供该队列。
- 命令按 `action.id` 至少投递一次。插件必须持久化已执行 ID，重复拉到同一 ID 时不得重复扣除、封号或销毁资产。
- `pgr_flush` 只在区服优雅停机时调用，最长建议 1000 ms。
- `emit_timeout_ms` 允许 `0-100` ms，`check_timeout_ms` 允许 `1-5000` ms，`pgr_flush` 最多等待 60 秒；异常配置直接返回 `-1`。
- Agent 暂时不可用时，SDK 后台重试同一批次；有界队列满时 `pgr_emit_json` 返回 `-2`，插件必须限量重试，不能静默丢弃账本事件。
- Agent 返回 HTTP 200 但 ACK 含拒绝事件时，`pgr_flush` 返回 `-5`，不能误报为已持久化成功。
- 影子模式下同步决策超时必须放行并记录本地计数。
- SDK 不在游戏主线程写磁盘；异步上报由后台线程建立本机或 HTTPS 连接，同步检查只用于少量高价值操作。

建议返回码：

| 返回值 | 含义 | 插件动作 |
|---|---|---|
| `0` | 已进入 SDK 队列，或调用成功 | 继续游戏逻辑 |
| `-1` | 参数或 JSON 无效 | 丢弃该事件并记录限频错误 |
| `-2` | SDK 队列满、Agent 暂不可用或响应协议无效 | 影子模式放行；有限内存重试 |
| `-3` | 超时 | 同步检查按当前模式降级 |
| `-4` | 响应缓冲区不足 | 按 `response_capacity` 返回值扩容 |
| `-5` | Agent 拒绝事件或请求 | 修复合同/数据，不要原样无限重试 |
| `-6` | SDK 未初始化或重复初始化 | 修复插件生命周期 |
| `-7` | SDK 内部错误 | 影子模式放行并告警 |

如果客户暂时不能加载 SDK，可使用第 4 节本机 HTTP 接口；事件合同完全相同。

## 4. 本机 Agent 接口

当前实现位于 `crates/risk-agent` 和 `crates/risk-sdk`，已经具备：

- loopback HTTP 接收器和本机 Token 认证。
- 事件级合同校验、币值守恒校验和身份字段防伪造。
- SQLite/WAL 持久队列与 `(tenant_id, server_id, event_id)` 幂等约束。
- 健康检查、批量 ACK、影子决策和队列状态接口。
- Windows DLL、Linux SO、公开 C 头文件、有界异步队列、ACK 校验和同步决策调用。
- SDK 接受显式 `http://127.0.0.1:<port>`，或严格的 `https://<平台域名>/sdk/v1`；拒绝明文局域网/公网地址、URL 用户信息、查询参数和片段。
- 状态化实时规则、持久化告警、幂等决策和本机告警查询接口。

当前尚未实现：Agent 到平台的 TLS/mTLS 投递器、命名管道/Unix Socket 和自动 `enforce` 决策。本机模式的 `flush` 只确认事件已在 Agent 本地持久化；远程模式的 `flush` 确认平台网关已返回 Agent 的持久化 ACK。人工处置命令已经通过远程 SDK 通道实现。

### 3.1 人工处置命令

插件每 1-3 秒调用一次 `pgr_pull_actions`：

```json
{"limit":10}
```

平台返回 `asset.freeze`、`session.kick`、`account.suspend`、`account.ban` 或 `currency.deduct`。每条命令包含平台生成的 `id`、绑定的 `tenantId/serverId`、`target`、`reason` 和 `requestedAt`。租户和区服来自 SDK 密钥，插件必须再次核对本机区服配置。

执行完成后调用 `pgr_ack_action`：

```json
{
  "actionId": "act_...",
  "status": "applied",
  "executionRef": "game-command-log-781",
  "message": "执行成功"
}
```

`status` 只能是 `applied`、`failed` 或 `rejected`。正确顺序是：持久化 `action.id` -> 执行游戏服权威命令 -> 记录结果 -> ACK。网络超时后应使用相同 ID 重试 ACK，不能重新执行游戏逻辑。响应缓冲区建议直接分配 64 KiB；拉取接口采用至少一次投递，缓冲区扩容重试仍会返回同一未 ACK 命令。

默认仅监听本机，不对局域网或公网开放：

- 当前实现：`http://127.0.0.1:17870`
- 跨机实现：`https://<平台域名>/sdk/v1`
- 规划中的 Windows 命名管道：`\\.\pipe\ponytail-risk-v1`
- 规划中的 Linux Unix Socket：`/run/ponytail-risk/agent.sock`

### 4.1 批量上报事件

```http
POST /agent/v1/events:batch
Content-Type: application/json
X-PGR-Local-Token: <仅本机配置文件可读的随机值>
```

请求正文必须通过 `plugin-event-batch.v1.schema.json`。正常响应：

```json
{
  "accepted": 6,
  "duplicates": 0,
  "rejected": [],
  "accepted_through_sequence": 106,
  "alerts_created": 2,
  "rule_codes": ["same_device_trade", "server_validation_failed"]
}
```

规则：

- 同一 `event_id` 重传必须幂等，不重复记账或重复告警。
- 只有事件进入 Agent 持久队列后才能返回已接收。
- 部分失败必须返回具体 `event_id`、错误码和可否重试。
- Agent 发现 `sequence` 缺口时接收后续事件，但必须报告缺口并触发对账。

### 4.2 高价值操作同步检查

```http
POST /agent/v1/decisions:check
Content-Type: application/json
```

请求示例：

```json
{
  "schema_version": "1.0",
  "request_id": "019fd684-55ce-7b63-a648-4fead2410001",
  "occurred_at": "2026-07-31T00:10:22.381+08:00",
  "action_type": "trade.commit",
  "transaction_id": "trade-9000281",
  "actor": {
    "player_id": "10001",
    "account_id": "account-11",
    "session_id": "session-781"
  },
  "counterparty": {
    "player_id": "10002",
    "device_fingerprint": "hmac-sha256:counterparty-device-digest"
  },
  "proposed_changes": {
    "currency_changes": [],
    "asset_changes": []
  },
  "timeout_ms": 20
}
```

响应示例：

```json
{
  "decision_id": "decision-7a22",
  "mode": "shadow",
  "decision": "review",
  "risk_score": 80,
  "rule_codes": ["same_device_counterparty"],
  "reasons": ["Operation counterparties share the same device fingerprint"],
  "expires_at": "2026-07-31T00:10:24.381+08:00"
}
```

决策语义：

- `allow`：未命中规则。
- `review`：允许完成，但资产进入人工复核队列。
- `deny`：只允许确定性规则在 `enforce` 模式返回。
- `shadow` 模式下插件始终继续原业务，最终事件携带 `decision_id`。
- 超时、Agent 离线或响应无法验证时默认 `fail-open`；是否改成 `fail-closed` 必须按区服和动作单独配置。
- 永久封号不属于该同步接口的自动动作。

同一个 `request_id` 的首次决策会写入 `decision_log`；SDK 因响应缓冲区扩容而重试时，Agent 返回完全相同的已存决策，不重复生成告警。

### 4.3 健康检查

```http
GET /agent/v1/health
```

至少返回 Agent 版本、队列深度、最后成功上报时间、当前模式和支持的 schema 版本。不得返回密钥。

### 4.4 实时告警查询

```http
GET /agent/v1/alerts
X-PGR-Local-Token: <仅本机配置文件可读的随机值>
```

返回当前租户/区服最近 100 条持久告警，包括 `actor_id`、`event_id/request_id`、`rule_code`、分类、严重度、风险分、证据和时间。该接口仅供本机控制层读取，不对局域网或公网开放。

### 4.5 当前实时规则

| 规则码 | 证据 | 默认等级 |
|---|---|---|
| `duplicate_asset_create` | 已存在的 `asset_id` 再次创建 | 严重 |
| `asset_owner_chain_mismatch` | `owner_before` 与上次提交状态不同 | 严重 |
| `rapid_asset_transfer` | 同一资产 10 分钟移动达到阈值 | 高 |
| `rapid_gold_gain` | 同一角色 10 分钟金元宝净获得达到阈值 | 高 |
| `unexplained_gold_snapshot_jump` | 快照跳增与区间币值流水不一致 | 严重 |
| `reward_claim_limit_exceeded` | `daily_claim_count > configured_max_count` | 严重 |
| `reward_source_incomplete` | 奖励缺少来源或配置版本 | 中 |
| `same_device_trade` | 已提交交易的 `metadata.same_device=true` | 高 |
| `trade_currency_legs_unbalanced` | 交易各币种双方腿合计不为零 | 严重 |
| `server_validation_failed` | 服务端上报 high/critical 校验失败 | 高/严重 |
| `plugin_sequence_gap` | 同一 producer/boot 跨批次序号缺口 | 中 |
| `plugin_sequence_regression` | 同一 producer/boot 出现迟到或倒退序号；事件保留但不更新实时状态 | 高 |
| `rapid_identical_action` | 同角色同 `action_code` 在 10 秒内尝试 20 次，记录次数与实际时间跨度 | 高 |
| `high_value_gold_change` | 同步请求元宝变化达到阈值 | 高 |
| `large_asset_quantity_change` | 同步请求资产数量变化达到阈值 | 高 |
| `cross_player_asset_transfer` | 同步请求资产持有人发生变化 | 中 |
| `same_device_counterparty` | 同步请求双方设备摘要相同 | 高 |

默认环境变量：

```text
PGR_GOLD_GAIN_10M=1000000
PGR_ASSET_MOVES_10M=5
PGR_HIGH_VALUE_GOLD=1000000
PGR_HIGH_VALUE_ASSET_QUANTITY=20
```

这些阈值只是保守初值，不是所有游戏通用结论。`rapid_identical_action` 也必须按不同玩法动作的正常冷却校准；生产区服先运行至少一周 shadow，再按币种产出、活动周期、交易量和误报分布分别调整。

## 5. 标准事件信封

一个批次包含生产者信息和有序事件：

```json
{
  "schema_version": "1.0",
  "producer": {
    "plugin_name": "example-game-plugin",
    "plugin_version": "1.3.0",
    "game_build": "20260731.1",
    "boot_id": "boot-019fd684"
  },
  "sent_at": "2026-07-31T00:10:23.000+08:00",
  "events": []
}
```

每条事件的公共字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| `event_id` | 是 | UUIDv7/ULID 或等价稳定 ID；重试不能改变 |
| `sequence` | 是 | 同一 `boot_id` 内单调递增的无符号整数 |
| `event_type` | 是 | 第 6 节列出的稳定枚举 |
| `status` | 是 | `attempted/succeeded/rejected/failed/rolled_back` |
| `occurred_at` | 是 | 带时区 RFC3339 时间 |
| `server_tick` | 否 | 游戏进程单调 tick，用于系统时钟漂移时排序 |
| `transaction_id` | 账本事件必填 | 同一业务事务的所有腿使用同一个值 |
| `decision_id` | 否 | 对应同步检查响应 |
| `actor` | 是 | 玩家、账号、角色、会话和设备上下文 |
| `context` | 是 | 原因码、来源、地图、配置版本等 |
| `data` | 是 | 统一的币值、资产、状态和校验证据容器 |

金额全部使用十进制字符串，禁止 JSON 浮点数，避免 C++/JavaScript/数据库之间精度不一致。

## 6. 第一批事件类型

| `event_type` | 产生位置 | 必须携带 |
|---|---|---|
| `session.started` | 登录鉴权成功并创建角色会话后 | `actor.session_id`、设备摘要、客户端 IP |
| `session.heartbeat` | 每 10-30 秒 | 会话、地图和轻量 `player_state` |
| `session.ended` | 正常退出或连接断开 | 会话、结束原因 |
| `state.player_snapshot` | 登录后、切图后、每 30 秒 | 三类币值、等级、地图、在线状态 |
| `ledger.currency_changed` | 币值事务提交后 | before/after/delta、币种、原因码 |
| `ledger.asset_created` | 道具或宠物分配唯一 ID 后 | 资产 ID、模板、数量、首个持有人 |
| `ledger.asset_moved` | 交易、邮件、仓库、摆摊、丢弃/拾取提交后 | before/after 所有人与容器 |
| `ledger.asset_changed` | 堆叠、拆分、合并、强化等提交后 | 数量和关键属性 before/after |
| `ledger.asset_destroyed` | 使用、过期、回收或删除提交后 | 最后持有人、销毁原因 |
| `ledger.reward_granted` | 任务/活动/礼包结算提交后 | 来源 ID、配置版本、所有实际变化 |
| `ledger.trade_committed` | 双方交易或摆摊事务提交后 | 双方与全部币值/资产腿 |
| `security.action_attempted` | 关键操作被业务校验拒绝时 | 动作码、请求参数摘要、拒绝原因 |
| `security.validation_failed` | 服务端守恒、上限或唯一性校验失败时 | 规则码、期望值、观察值、证据 |

不要把所有行为塞进 `state.player_snapshot`。快照只能说明“现在是什么”，不能证明“为什么变成这样”。币值、道具和交易必须在修改成功的提交点产生独立事件。

## 7. 必须埋点的位置

### 7.1 币值

所有加减金钱、金元宝、银元宝的公共函数必须统一调用埋点，不能只在商城或任务里零散添加。

每次成功修改至少提供：

- `currency`：`game_cash/gold_coin/silver_coin`。
- `before`、`after`、`delta`，且 `after - before == delta`。
- 业务 `reason_code`，例如 `quest_reward`、`mall_purchase`、`gm_grant`。
- `source_type`、`source_id` 和 `transaction_id`。
- 修改所使用的配置版本。

校验失败也要上报，但 `status` 必须是 `rejected`，不能伪装成成功流水。

### 7.2 道具和宠物

每个可流转资产必须有稳定 `asset_id`。优先使用游戏已有 IID；如果没有，插件必须在创建时生成并持久化到对象扩展字段。

没有稳定 ID 时只能追踪“某模板的一批数量”，不能声称追踪到同一件道具。

必须覆盖：

- 生成、奖励发放、拾取。
- 包裹、仓库、邮件、摆摊、交易和地面之间移动。
- 堆叠、拆分、合并和数量变化。
- 使用、过期、回收、删除。
- 宠物获得、放生和所有权变化。

### 7.3 奖励与活动

奖励事件需要同时记录“规则允许什么”和“实际发了什么”：

- 活动/任务/礼包 ID。
- 配置版本和奖励表版本。
- 角色完成次数、当日次数和幂等领取号。
- 期望上限与实际币值/资产变化。
- 补偿、重放或 GM 发放必须使用不同 `reason_code`。

### 7.4 交易

玩家交易、摆摊、邮件转移必须在数据库或游戏对象事务提交成功后上报。双方金钱腿和资产腿共用一个 `transaction_id`。

禁止在“点击确认”时直接上报成功；点击只产生 `attempted`，最终提交才产生 `succeeded`。回滚必须产生 `rolled_back`。

### 7.5 玩家状态

插件实时状态可以提供：

- 角色等级、门派、地图、坐标、在线/战斗状态。
- 会话 ID、登录时间、客户端版本。
- 设备摘要、同设备账号线索和网络地址。
- 当前三类币值和背包摘要。

状态快照建议 10-30 秒一次，切图、登录和高价值操作后可立即补一帧。不要每个游戏 tick 上报。

## 8. 顺序、重试和背压

- `event_id` 在业务提交成功时生成并随本地重试持久化。
- `sequence` 只在当前 `boot_id` 内递增，进程重启必须更换 `boot_id`。
- 每批最多 200 条或 256 KiB，先达到哪个条件就发送。
- 正常聚合窗口建议 50-100 ms；高价值事件立即刷新。
- SDK 使用有界内存队列，默认 256 个批次、最大 16,384 个批次；队列满返回 `-2`，当前实现不在 SDK 内静默丢弃或重排事件。
- 可靠磁盘队列在 Agent，不在游戏进程。
- Agent 只有收到平台持久化 ACK 后才能删除本地事件。
- 数据库补采事件必须有不同的 `producer`，并使用确定性 ID，避免与插件事件重复记账。

## 9. 安全要求

- 本机 HTTP 回退必须只绑定 `127.0.0.1`，并使用权限为 `600`/ACL 限制的本地 Token。
- 插件事件中禁止出现授权卡密、数据库密码、Session Token、完整硬件序列号和明文支付信息。
- 设备信息使用稳定不可逆摘要；原始硬件字段不得上传平台。
- Agent 到平台使用短期 Agent Token 和 TLS/mTLS；Token 只绑定一个租户和区服，可撤销。
- 平台忽略正文中的租户/区服字段，由认证上下文注入。
- 日志必须擦除 Token、密码和原始事件正文中的敏感字段。
- 同步 `deny` 只用于重复资产 ID、奖励配置越界等确定性规则；统计分数不得直接永久封号。

## 10. 插件开发验收

交付插件前至少通过以下测试：

1. 同一事件重复提交 10 次，平台只生成一条账本记录和一条告警。
2. 人为断开 Agent 60 秒后恢复，事件按原 ID 补传且顺序可解释。
3. 交易回滚不会留下成功的资产或币值变化。
4. 同一交易的双方与所有资产腿拥有相同 `transaction_id`。
5. 元宝 `before + delta == after`，超大整数不发生精度变化。
6. Agent 不可用时游戏主线程延迟不增加超过约定预算。
7. 影子模式下任何风控超时都不阻断玩家。
8. `enforce` 模式只阻断白名单中的确定性规则。
9. 插件重启产生新 `boot_id`，旧批次重传仍保持幂等。
10. 插件事件与数据库流水对账能够发现漏报，而不是重复计数。
11. A 区服事件无法写入或查询 B 区服。
12. 中文角色名、道具名以 UTF-8 上报，不出现乱码或截断。
13. 同一 `action.id` 连续拉取 10 次，封号或扣除逻辑只执行一次，重复 ACK 返回同一终态。
14. A 区服密钥无法拉取或 ACK B 区服命令，插件执行日志可用 `executionRef` 回查。

运行合同自检：

```powershell
node .\plugin_contract_check.mjs
cargo run -p risk-agent -- self-check
powershell -NoProfile -ExecutionPolicy Bypass -File .\agent_http_check.ps1
cargo test -p risk-sdk
powershell -NoProfile -ExecutionPolicy Bypass -File .\sdk_c_abi_check.ps1
```

## 11. 推荐实施顺序

第一阶段只做异步影子事件：

1. `session.started/ended`。
2. `ledger.currency_changed`。
3. `ledger.asset_created/moved/destroyed`。
4. `ledger.reward_granted/trade_committed`。
5. `security.validation_failed`。
6. `state.player_snapshot` 校准。

第二阶段实现 Agent 持久队列、平台幂等写入和数据库对账。

第三阶段只为奖励结算、玩家交易、摆摊和高价值资产移动增加同步检查。先运行至少一周 `shadow`，确认误报、延迟和降级路径后，再按规则逐项开启 `enforce`。

当前项目的数据库轮询继续运行，直到插件事件覆盖率、账本守恒率和断线补传测试全部达标。

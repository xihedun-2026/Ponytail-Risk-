import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));
const schema = JSON.parse(readFileSync(join(root, "docs", "plugin-event-batch.v1.schema.json"), "utf8"));
const batch = JSON.parse(readFileSync(join(root, "docs", "plugin-event-batch.v1.example.json"), "utf8"));

const definition = (name) => schema.$defs[name];

function checkShape(value, shape, path) {
  assert.ok(value && typeof value === "object" && !Array.isArray(value), `${path} 必须是对象`);
  for (const key of shape.required || []) assert.ok(Object.hasOwn(value, key), `${path}.${key} 缺失`);
  if (shape.additionalProperties === false) {
    for (const key of Object.keys(value)) {
      assert.ok(Object.hasOwn(shape.properties, key), `${path}.${key} 不在 v1 合同中`);
    }
  }
}

function checkDateTime(value, path) {
  assert.match(value, /^\d{4}-\d{2}-\d{2}T.*(?:Z|[+-]\d{2}:\d{2})$/, `${path} 必须携带时区`);
  assert.ok(Number.isFinite(Date.parse(value)), `${path} 不是有效时间`);
}

const forbiddenKeys = new Set([
  "tenant_id",
  "server_id",
  "license_key",
  "portal_key",
  "agent_token",
  "database_password",
]);

function checkForbiddenKeys(value, path = "batch") {
  if (!value || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value)) {
    assert.ok(!forbiddenKeys.has(key), `${path}.${key} 不允许由插件上报`);
    checkForbiddenKeys(child, `${path}.${key}`);
  }
}

checkShape(batch, schema, "batch");
checkShape(batch.producer, definition("producer"), "batch.producer");
assert.equal(batch.schema_version, "1.0");
checkDateTime(batch.sent_at, "batch.sent_at");
assert.ok(batch.events.length > 0 && batch.events.length <= schema.properties.events.maxItems);
assert.ok(Buffer.byteLength(JSON.stringify(batch), "utf8") <= 256 * 1024, "示例批次超过 256 KiB");
checkForbiddenKeys(batch);

const eventShape = definition("event");
const allowedTypes = new Set(eventShape.properties.event_type.enum);
const allowedStatuses = new Set(eventShape.properties.status.enum);
const eventIds = new Set();
const seenTypes = new Set();
let previousSequence = -1;

for (const [index, event] of batch.events.entries()) {
  const path = `batch.events[${index}]`;
  checkShape(event, eventShape, path);
  checkShape(event.actor, definition("actor"), `${path}.actor`);
  checkShape(event.context, definition("context"), `${path}.context`);
  checkShape(event.data, definition("data"), `${path}.data`);

  assert.ok(!eventIds.has(event.event_id), `${path}.event_id 重复`);
  eventIds.add(event.event_id);
  assert.ok(Number.isSafeInteger(event.sequence), `${path}.sequence 必须是安全整数`);
  assert.ok(event.sequence > previousSequence, `${path}.sequence 必须严格递增`);
  previousSequence = event.sequence;
  assert.ok(allowedTypes.has(event.event_type), `${path}.event_type 未登记`);
  assert.ok(allowedStatuses.has(event.status), `${path}.status 未登记`);
  checkDateTime(event.occurred_at, `${path}.occurred_at`);
  seenTypes.add(event.event_type);

  if (event.event_type.startsWith("ledger.")) {
    assert.ok(event.transaction_id, `${path} 账本事件缺少 transaction_id`);
  }
  if (event.event_type === "ledger.currency_changed") {
    assert.ok(event.data.currency_changes?.length, `${path} 缺少 currency_changes`);
  }
  if (["ledger.asset_created", "ledger.asset_moved", "ledger.asset_changed", "ledger.asset_destroyed"].includes(event.event_type)) {
    assert.ok(event.data.asset_changes?.length, `${path} 缺少 asset_changes`);
  }
  if (event.event_type === "state.player_snapshot") {
    assert.ok(event.data.player_state, `${path} 缺少 player_state`);
  }
  if (event.event_type === "security.validation_failed") {
    assert.ok(event.data.validation, `${path} 缺少 validation`);
  }

  for (const [currencyIndex, change] of (event.data.currency_changes || []).entries()) {
    const currencyPath = `${path}.data.currency_changes[${currencyIndex}]`;
    checkShape(change, definition("currency_change"), currencyPath);
    assert.match(change.before, /^-?[0-9]+$/);
    assert.match(change.after, /^-?[0-9]+$/);
    assert.match(change.delta, /^-?[0-9]+$/);
    assert.equal(BigInt(change.before) + BigInt(change.delta), BigInt(change.after), `${currencyPath} 币值不守恒`);
    assert.ok(BigInt(change.before) >= 0n && BigInt(change.after) >= 0n, `${currencyPath} 余额不能为负数`);
  }

  for (const [assetIndex, change] of (event.data.asset_changes || []).entries()) {
    const assetPath = `${path}.data.asset_changes[${assetIndex}]`;
    checkShape(change, definition("asset_change"), assetPath);
    assert.ok(change.asset_id.length > 0, `${assetPath}.asset_id 为空`);
    assert.ok(Number.isSafeInteger(change.quantity_before) && change.quantity_before >= 0);
    assert.ok(Number.isSafeInteger(change.quantity_after) && change.quantity_after >= 0);
    if (change.operation === "create") {
      assert.equal(change.quantity_before, 0, `${assetPath} 创建前数量必须为 0`);
      assert.ok(change.quantity_after > 0 && change.owner_after, `${assetPath} 创建结果无效`);
    }
    if (change.operation === "move") {
      assert.ok(change.quantity_before > 0 && change.quantity_after > 0, `${assetPath} 移动数量无效`);
      assert.ok(change.owner_before && change.owner_after, `${assetPath} 移动缺少持有人`);
    }
    if (change.operation === "destroy") {
      assert.ok(change.quantity_before > 0 && change.quantity_after === 0 && change.owner_before, `${assetPath} 销毁状态无效`);
    }
  }

  if (event.data.player_state) {
    const statePath = `${path}.data.player_state`;
    checkShape(event.data.player_state, definition("player_state"), statePath);
    for (const [currency, amount] of Object.entries(event.data.player_state.currencies)) {
      assert.match(amount, /^-?[0-9]+$/, `${statePath}.currencies.${currency} 必须是十进制字符串`);
      assert.ok(BigInt(amount) >= 0n, `${statePath}.currencies.${currency} 不能为负数`);
    }
  }

  if (event.data.validation) {
    checkShape(event.data.validation, definition("validation"), `${path}.data.validation`);
  }
}

for (const requiredType of [
  "session.started",
  "state.player_snapshot",
  "ledger.reward_granted",
  "ledger.currency_changed",
  "ledger.asset_moved",
  "ledger.trade_committed",
  "security.validation_failed",
]) {
  assert.ok(seenTypes.has(requiredType), `示例缺少 ${requiredType}`);
}

const trade = batch.events.find((event) => event.event_type === "ledger.trade_committed");
const tradeCashTotal = trade.data.currency_changes
  .filter((change) => change.currency === "game_cash")
  .reduce((total, change) => total + BigInt(change.delta), 0n);
assert.equal(tradeCashTotal, 0n, "示例玩家交易的金钱双方腿不守恒");
assert.equal(new Set(trade.data.currency_changes.map((change) => change.owner_id)).size, 2, "示例交易必须包含双方");

console.log(`plugin contract check ok: ${batch.events.length} events, ${seenTypes.size} event types`);

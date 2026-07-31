import { createCipheriv, createDecipheriv, createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { execFile } from "node:child_process";
import { createReadStream, existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { isIP } from "node:net";
import { promisify } from "node:util";
import { compareRuleReplay } from "./rule_replay.mjs";

// import.meta.dirname 要 Node 20.11+，但我们声明支持 18+；这样算等价且不挑版本。
const projectRoot = fileURLToPath(new URL(".", import.meta.url));
const port = Number(process.env.RISK_PORT || 4173);
const host = process.env.RISK_HOST || "127.0.0.1";
const publicRoot = resolve(projectRoot, "public");
const dataRoot = resolve(projectRoot, "data");
const agentPort = Number(process.env.PGR_AGENT_PORT || 17870);
const connectionConfigPath = resolve(process.env.RISK_DB_CONFIG_PATH || join(dataRoot, "database-connection.enc.json"));
const aiConfigPath = resolve(process.env.RISK_AI_CONFIG_PATH || join(dataRoot, "ai-provider.enc.json"));
const aiReviewsPath = resolve(process.env.RISK_AI_REVIEWS_PATH || join(dataRoot, "ai-reviews.json"));
const sdkCredentialsPath = resolve(process.env.RISK_SDK_KEYS_PATH || join(dataRoot, "sdk-credentials.json"));
const caseActionsPath = resolve(process.env.RISK_CASE_ACTIONS_PATH || join(dataRoot, "case-actions.json"));
const gameplayCapsPath = resolve(process.env.RISK_GAMEPLAY_CAPS_PATH || join(dataRoot, "gameplay-caps.json"));
const execFileAsync = promisify(execFile);
const production = process.env.NODE_ENV === "production";
const configuredPortalKey = process.env.RISK_PORTAL_KEY || "";
if (production && (configuredPortalKey.length < 16 || configuredPortalKey === "PONYTAIL-DEMO-2026")) {
  throw new Error("RISK_PORTAL_KEY must be a unique value of at least 16 characters in production");
}
const portalKey = configuredPortalKey || "PONYTAIL-DEMO-2026";
const agentLocalToken = process.env.PGR_AGENT_LOCAL_TOKEN || "";
const allowInsecureSdk = process.env.RISK_SDK_ALLOW_INSECURE === "1";
const behindTlsProxy = process.env.RISK_BEHIND_TLS_PROXY === "1";
const trustedProxyIps = new Set(
  String(process.env.RISK_TRUSTED_PROXY_IPS || "127.0.0.1,::1,::ffff:127.0.0.1")
    .split(",")
    .map((value) => value.trim())
    .filter((value) => isIP(value)),
);
const expectedKeyHash = createHash("sha256").update(portalKey).digest();
const masterKeyHex = process.env.RISK_CONFIG_MASTER_KEY || "";
if (masterKeyHex && !/^[a-fA-F0-9]{64}$/.test(masterKeyHex)) throw new Error("RISK_CONFIG_MASTER_KEY must be 32 bytes encoded as 64 hex characters");
if (production && !masterKeyHex) throw new Error("RISK_CONFIG_MASTER_KEY is required in production");
const configMasterKey = masterKeyHex ? Buffer.from(masterKeyHex, "hex") : null;
const connectionEncryptionKey = configMasterKey ? createHmac("sha256", configMasterKey).update("ponytail-risk-database-v2").digest() : null;
const aiEncryptionKey = configMasterKey ? createHmac("sha256", configMasterKey).update("ponytail-risk-ai-v2").digest() : null;
const legacyConnectionEncryptionKey = createHash("sha256").update(`ponytail-risk-db:${portalKey}`).digest();
const legacyAiEncryptionKey = createHash("sha256").update(`ponytail-risk-ai:${portalKey}`).digest();
const defaultDatabaseConfig = {
  enabled: process.env.WDSF_LIVE === "1",
  host: process.env.WDSF_HOST || "127.0.0.1",
  port: Number(process.env.WDSF_DB_PORT || 3306),
  user: process.env.WDSF_DB_USER || "",
  password: process.env.WDSF_DB_PASSWORD || "",
  mainDatabase: process.env.WDSF_MDB || "dl_mdb_1",
  logDatabase: process.env.WDSF_LDB || "dl_ldb_1",
};
const defaultAiConfig = {
  enabled: false,
  provider: "groq",
  model: "qwen/qwen3.6-27b",
  apiKey: "",
};
let databaseConfigStored = existsSync(connectionConfigPath);
let databaseConfig = loadDatabaseConfig();
let aiConfigStored = existsSync(aiConfigPath);
let aiConfig = loadAiConfig();
let aiReviews = loadAiReviews();
const sessions = new Map();
const attempts = new Map();
const sdkRateLimits = new Map();
let collectorRunning = false;
let aiReviewWorkerRunning = false;
let sdkCredentials = loadSdkCredentials();
let caseActions = loadCaseActions();
let gameplayCapsState = loadGameplayCapsState();
let gameplayCaps = gameplayCapsState.caps;
if (configMasterKey && databaseConfigStored && storedConfigVersion(connectionConfigPath) === 1) saveDatabaseConfig(databaseConfig);
if (configMasterKey && aiConfigStored && storedConfigVersion(aiConfigPath) === 1) saveAiConfig(aiConfig);

const integrationArtifacts = new Map([
  ["windows-sdk", { path: resolve(projectRoot, "dist", "risk-sdk", "ponytail-risk-sdk-windows-x86_64.zip"), filename: "ponytail-risk-sdk-windows-x86_64.zip", label: "Windows x64 SDK", type: "application/zip" }],
  ["linux-sdk", { path: resolve(projectRoot, "dist", "risk-sdk", "ponytail-risk-sdk-linux-x86_64.zip"), filename: "ponytail-risk-sdk-linux-x86_64.zip", label: "Linux x86_64 SDK", type: "application/zip" }],
  ["integration-guide", { path: resolve(projectRoot, "docs", "GAME_PLUGIN_INTEGRATION_V1.md"), filename: "GAME_PLUGIN_INTEGRATION_V1.md", label: "完整接入文档", type: "text/markdown; charset=utf-8" }],
  ["event-schema", { path: resolve(projectRoot, "docs", "plugin-event-batch.v1.schema.json"), filename: "plugin-event-batch.v1.schema.json", label: "事件 Schema", type: "application/schema+json; charset=utf-8" }],
  ["event-example", { path: resolve(projectRoot, "docs", "plugin-event-batch.v1.example.json"), filename: "plugin-event-batch.v1.example.json", label: "事件示例", type: "application/json; charset=utf-8" }],
]);

const players = [
  {
    id: "1003281",
    name: "北境长歌",
    account: "acc_88241",
    server: "问道一区",
    level: 142,
    score: 87,
    status: "高风险",
    statusTone: "danger",
    tags: ["小号归集", "收益超速", "同设备群"],
    summary: "16 个新角色在 42 分钟内向该角色集中转入高价值道具，路径跨越摆摊与邮件。",
    metrics: [
      ["今日元宝净流入", "+286,400"],
      ["高价值道具", "47 件"],
      ["关联账号", "23 个"],
      ["异常路径", "6 条"],
    ],
    timeline: [
      ["14:21:08", "任务奖励", "+2,000 元宝", "合法"],
      ["14:24:31", "摆摊收购", "+12 件玄天令", "可疑低价"],
      ["14:28:02", "邮件收取", "+18 件玄天令", "关联小号"],
      ["14:31:19", "跨服交易", "-24 件玄天令", "资产扩散"],
    ],
  },
  {
    id: "1007742",
    name: "山海一梦",
    account: "acc_19207",
    server: "问道二区",
    level: 131,
    score: 38,
    status: "观察",
    statusTone: "warning",
    tags: ["产出偏高", "夜间活跃"],
    summary: "副本效率高于同层级玩家 2.1 倍，但奖励来源与副本次数可以闭合。",
    metrics: [
      ["今日元宝净流入", "+38,200"],
      ["高价值道具", "6 件"],
      ["关联账号", "2 个"],
      ["异常路径", "1 条"],
    ],
    timeline: [
      ["02:11:42", "副本掉落", "+1 件雷极弧光", "合法"],
      ["02:32:10", "商店出售", "+8,600 元宝", "合法"],
      ["03:08:55", "副本掉落", "+1 件雷极弧光", "效率偏高"],
    ],
  },
];

const assets = [
  {
    id: "ITEM-9F2A-771C",
    name: "玄天令",
    quantity: 24,
    state: "暂存中",
    risk: 91,
    owner: "北境长歌 / 1003281",
    source: "异常活动结算",
    nodes: [
      ["14:02:11", "系统生成", "活动奖励 ACT-77", "可疑：配置上限 2，实际发放 24"],
      ["14:03:20", "角色获得", "青竹小号07 / 1009917", "同设备账号"],
      ["14:18:44", "摆摊转移", "北境长歌 / 1003281", "成交价低于中位数 96%"],
      ["14:31:19", "跨服挂单", "订单 XT-44281", "已被风控暂存"],
    ],
  },
  {
    id: "ITEM-31BC-20D9",
    name: "雷极弧光",
    quantity: 1,
    state: "流通中",
    risk: 12,
    owner: "山海一梦 / 1007742",
    source: "副本掉落",
    nodes: [
      ["02:11:42", "系统生成", "副本 BOSS-184", "掉落表与次数一致"],
      ["02:11:44", "角色拾取", "山海一梦 / 1007742", "合法"],
      ["02:29:03", "仓库存入", "个人仓库", "合法"],
    ],
  },
];

const alerts = [
  { id: "R-20260730-0081", time: "14:31", player: "北境长歌", rule: "高价值资产异常归集", severity: "严重", score: 91, state: "待研判" },
  { id: "R-20260730-0079", time: "14:24", player: "青竹小号07", rule: "配置外奖励数量", severity: "严重", score: 96, state: "已暂存" },
  { id: "R-20260730-0068", time: "13:58", player: "夜雨声烦", rule: "元宝收益速率异常", severity: "高", score: 78, state: "观察中" },
  { id: "R-20260730-0051", time: "12:40", player: "山海一梦", rule: "同群体效率偏离", severity: "中", score: 38, state: "已复核" },
  { id: "R-20260730-0042", time: "11:12", player: "临江仙", rule: "重复邮件领取", severity: "高", score: 84, state: "已阻断" },
];

const rules = [
  { id: "ledger-invariants", name: "账本守恒与唯一资产", desc: "识别资产重复、所有权链断裂和交易币值缺腿。", enabled: true, level: "确定性" },
  { id: "configured-reward-cap", name: "玩法产出上限", desc: "按已发布版本检查单日与 10 分钟奖励次数。", enabled: true, level: "确定性" },
  { id: "currency-evidence", name: "元宝来源证据", desc: "结合存量偏离、快照跳增来源和服务端币值校验评分。", enabled: true, level: "行为" },
  { id: "asset-network", name: "资产归集与循环回流", desc: "识别多账号扇入、奖励后快速归集和同一资产循环流转。", enabled: true, level: "图谱" },
  { id: "behavior-rhythm", name: "持续活跃与机械周期", desc: "把超长活跃和固定间隔重复作为可解释的辅助信号。", enabled: true, level: "辅助" },
];

function hash(value) {
  return createHash("sha256").update(value).digest();
}

function safeEqual(left, right) {
  return left.length === right.length && timingSafeEqual(left, right);
}

function storedConfigVersion(path) {
  try {
    return Number(JSON.parse(readFileSync(path, "utf8")).version || 1);
  } catch {
    return 0;
  }
}

function configCipherMaterial(payload, currentKey, legacyKey, purpose) {
  const version = Number(payload.version || 1);
  if (version === 2) {
    if (!currentKey) throw new Error(`${purpose} config requires RISK_CONFIG_MASTER_KEY`);
    return { version, key: currentKey, aad: `ponytail-risk-${purpose}-v2` };
  }
  if (version === 1) return { version, key: legacyKey, aad: `ponytail-risk-${purpose}-v1` };
  throw new Error(`${purpose} config version is unsupported`);
}

function currentCipherMaterial(currentKey, legacyKey, purpose) {
  return currentKey
    ? { version: 2, key: currentKey, aad: `ponytail-risk-${purpose}-v2` }
    : { version: 1, key: legacyKey, aad: `ponytail-risk-${purpose}-v1` };
}

function decryptDatabaseConfig(payload) {
  const iv = Buffer.from(payload.iv, "base64");
  const tag = Buffer.from(payload.tag, "base64");
  const material = configCipherMaterial(payload, connectionEncryptionKey, legacyConnectionEncryptionKey, "database");
  const decipher = createDecipheriv("aes-256-gcm", material.key, iv);
  decipher.setAAD(Buffer.from(material.aad));
  decipher.setAuthTag(tag);
  return JSON.parse(Buffer.concat([decipher.update(Buffer.from(payload.data, "base64")), decipher.final()]).toString("utf8"));
}

function loadDatabaseConfig() {
  if (!existsSync(connectionConfigPath)) return { ...defaultDatabaseConfig };
  try {
    return normalizeDatabaseConfig(decryptDatabaseConfig(JSON.parse(readFileSync(connectionConfigPath, "utf8"))), defaultDatabaseConfig);
  } catch (error) {
    console.error(`database config: ${error.message}`);
    databaseConfigStored = false;
    return { ...defaultDatabaseConfig };
  }
}

function saveDatabaseConfig(config) {
  mkdirSync(dirname(connectionConfigPath), { recursive: true });
  const iv = randomBytes(12);
  const material = currentCipherMaterial(connectionEncryptionKey, legacyConnectionEncryptionKey, "database");
  const cipher = createCipheriv("aes-256-gcm", material.key, iv);
  cipher.setAAD(Buffer.from(material.aad));
  const encrypted = Buffer.concat([cipher.update(JSON.stringify(config), "utf8"), cipher.final()]);
  const payload = JSON.stringify({ version: material.version, iv: iv.toString("base64"), tag: cipher.getAuthTag().toString("base64"), data: encrypted.toString("base64") });
  const temporary = `${connectionConfigPath}.tmp`;
  writeFileSync(temporary, payload, { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, connectionConfigPath);
  databaseConfigStored = true;
}

function decryptAiConfig(payload) {
  const iv = Buffer.from(payload.iv, "base64");
  const tag = Buffer.from(payload.tag, "base64");
  const material = configCipherMaterial(payload, aiEncryptionKey, legacyAiEncryptionKey, "ai");
  const decipher = createDecipheriv("aes-256-gcm", material.key, iv);
  decipher.setAAD(Buffer.from(material.aad));
  decipher.setAuthTag(tag);
  return JSON.parse(Buffer.concat([decipher.update(Buffer.from(payload.data, "base64")), decipher.final()]).toString("utf8"));
}

function loadAiConfig() {
  if (!existsSync(aiConfigPath)) return { ...defaultAiConfig };
  try {
    return normalizeAiConfig(decryptAiConfig(JSON.parse(readFileSync(aiConfigPath, "utf8"))), defaultAiConfig);
  } catch (error) {
    console.error(`ai config: ${error.message}`);
    aiConfigStored = false;
    return { ...defaultAiConfig };
  }
}

function saveAiConfig(config) {
  mkdirSync(dirname(aiConfigPath), { recursive: true });
  const iv = randomBytes(12);
  const material = currentCipherMaterial(aiEncryptionKey, legacyAiEncryptionKey, "ai");
  const cipher = createCipheriv("aes-256-gcm", material.key, iv);
  cipher.setAAD(Buffer.from(material.aad));
  const encrypted = Buffer.concat([cipher.update(JSON.stringify(config), "utf8"), cipher.final()]);
  const payload = JSON.stringify({ version: material.version, iv: iv.toString("base64"), tag: cipher.getAuthTag().toString("base64"), data: encrypted.toString("base64") });
  const temporary = `${aiConfigPath}.tmp`;
  writeFileSync(temporary, payload, { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, aiConfigPath);
  aiConfigStored = true;
}

function normalizeGameplayCaps(input) {
  const entries = Array.isArray(input) ? input : input?.caps;
  if (!Array.isArray(entries) || entries.length > 100) throw new Error("玩法上限配置无效");
  const actions = new Set();
  return entries.map((entry) => {
    if (!entry || typeof entry !== "object") throw new Error("玩法上限条目无效");
    const action = String(entry.action || "").trim();
    const label = String(entry.label || action).trim();
    const dailyLimit = Number(entry.dailyLimit);
    const burst10mLimit = Number(entry.burst10mLimit);
    const enabled = entry.enabled;
    if (!/^[A-Za-z0-9_:-]{1,64}$/.test(action) || actions.has(action)) throw new Error("玩法 action 无效或重复");
    if (!label || label.length > 80 || /[\u0000-\u001f\u007f]/.test(label)) throw new Error("玩法名称无效");
    if (!Number.isInteger(dailyLimit) || dailyLimit < 0 || dailyLimit > 1_000_000) throw new Error("单日上限无效");
    if (!Number.isInteger(burst10mLimit) || burst10mLimit < 0 || burst10mLimit > 100_000) throw new Error("10 分钟上限无效");
    if (typeof enabled !== "boolean" || (enabled && dailyLimit === 0 && burst10mLimit === 0)) throw new Error("启用的玩法至少需要一个上限");
    actions.add(action);
    return { action, label, dailyLimit, burst10mLimit, enabled };
  });
}

function gameplayCapsVersionId(caps) {
  return `caps_${createHash("sha256").update(JSON.stringify(caps)).digest("hex").slice(0, 16)}`;
}

function loadGameplayCapsState() {
  if (!existsSync(gameplayCapsPath)) return { caps: [], currentVersion: null, versions: [] };
  try {
    const parsed = JSON.parse(readFileSync(gameplayCapsPath, "utf8"));
    if (Array.isArray(parsed)) {
      const caps = normalizeGameplayCaps(parsed);
      const id = gameplayCapsVersionId(caps);
      return { caps, currentVersion: id, versions: [{ id, createdAt: statSync(gameplayCapsPath).mtime.toISOString(), caps }] };
    }
    if (parsed?.schemaVersion !== 1 || !Array.isArray(parsed.versions) || parsed.versions.length > 100) throw new Error("版本历史无效");
    const versions = parsed.versions.map((version) => {
      const caps = normalizeGameplayCaps(version.caps);
      const id = gameplayCapsVersionId(caps);
      if (version.id !== id || typeof version.createdAt !== "string" || !Number.isFinite(Date.parse(version.createdAt))) throw new Error("规则版本无效");
      return { id, createdAt: version.createdAt, caps };
    });
    if (new Set(versions.map((version) => version.id)).size !== versions.length) throw new Error("规则版本重复");
    const current = versions.find((version) => version.id === parsed.currentVersion);
    if (!current && parsed.currentVersion !== null) throw new Error("当前规则版本不存在");
    return { caps: current?.caps || [], currentVersion: current?.id || null, versions };
  } catch (error) {
    console.error(`gameplay caps: ${error.message}`);
    return { caps: [], currentVersion: null, versions: [] };
  }
}

function publicGameplayCapsState() {
  return {
    caps: gameplayCaps,
    currentVersion: gameplayCapsState.currentVersion,
    versions: gameplayCapsState.versions.map((version) => ({
      id: version.id,
      createdAt: version.createdAt,
      capCount: version.caps.length,
      enabledCount: version.caps.filter((cap) => cap.enabled).length,
    })),
  };
}

function publishGameplayCaps(caps) {
  const id = gameplayCapsVersionId(caps);
  const existing = gameplayCapsState.versions.find((version) => version.id === id);
  const versions = existing
    ? gameplayCapsState.versions
    : [...gameplayCapsState.versions, { id, createdAt: new Date().toISOString(), caps }].slice(-100);
  const state = { schemaVersion: 1, currentVersion: id, versions };
  mkdirSync(dirname(gameplayCapsPath), { recursive: true });
  const temporary = `${gameplayCapsPath}.tmp`;
  writeFileSync(temporary, JSON.stringify(state, null, 2), { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, gameplayCapsPath);
  gameplayCapsState = { caps, currentVersion: id, versions };
  gameplayCaps = caps;
  return { created: !existing, ...publicGameplayCapsState() };
}

function normalizeAiConfig(input, fallback = aiConfig) {
  if (!input || typeof input !== "object") throw new Error("AI 配置无效");
  const enabled = input.enabled === undefined ? Boolean(fallback.enabled) : input.enabled;
  const provider = String(input.provider ?? fallback.provider ?? "groq").trim().toLowerCase();
  const defaultModel = provider === "ollama" ? "qwen3:4b" : "qwen/qwen3.6-27b";
  const model = String(input.model ?? fallback.model ?? defaultModel).trim();
  const apiKeyInput = typeof input.apiKey === "string" ? input.apiKey.trim() : "";
  const apiKey = apiKeyInput || fallback.apiKey || "";
  if (typeof enabled !== "boolean" || !["groq", "ollama"].includes(provider)) throw new Error("AI Provider 无效");
  if (!/^[A-Za-z0-9_.:/-]{1,128}$/.test(model)) throw new Error("AI 模型名称无效");
  if (apiKey.length > 512) throw new Error("AI API Key 过长");
  if (enabled && provider === "groq" && apiKey.length < 16) throw new Error("请填写有效的 Groq API Key");
  return { enabled, provider, model, apiKey: provider === "ollama" ? "" : apiKey };
}

function publicAiConfig() {
  return {
    enabled: aiConfig.enabled,
    provider: aiConfig.provider,
    model: aiConfig.model,
    apiKeyConfigured: Boolean(aiConfig.apiKey),
    persisted: aiConfigStored,
    automatic: true,
    completedReviews: aiReviews.length,
  };
}

function aiProviderEndpoint(config) {
  return config.provider === "ollama"
    ? process.env.RISK_AI_OLLAMA_ENDPOINT || "http://127.0.0.1:11434/api/chat"
    : process.env.RISK_AI_GROQ_ENDPOINT || "https://api.groq.com/openai/v1/chat/completions";
}

function textField(value, field, maximum = 1000) {
  if (typeof value !== "string" || !value.trim() || value.length > maximum) throw new Error(`AI 响应字段 ${field} 无效`);
  return value.trim();
}

function validateAiAnalysis(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("AI 响应不是 JSON 对象");
  const riskLevel = String(value.risk_level || "");
  if (!["normal", "watch", "high"].includes(riskLevel)) throw new Error("AI 响应风险等级无效");
  const confidence = Number(value.confidence);
  if (!Number.isInteger(confidence) || confidence < 0 || confidence > 100) throw new Error("AI 响应置信度无效");
  if (!Array.isArray(value.findings) || value.findings.length > 8) throw new Error("AI 响应发现列表无效");
  if (!Array.isArray(value.suggested_actions) || value.suggested_actions.length > 6) throw new Error("AI 响应建议列表无效");
  return {
    summary: textField(value.summary, "summary"),
    riskLevel,
    confidence,
    findings: value.findings.map((item) => {
      if (!item || typeof item !== "object" || !["low", "medium", "high"].includes(item.severity)) throw new Error("AI 响应发现项无效");
      return { title: textField(item.title, "findings.title", 160), evidence: textField(item.evidence, "findings.evidence", 400), severity: item.severity };
    }),
    suggestedActions: value.suggested_actions.map((item) => textField(item, "suggested_actions", 300)),
  };
}

async function callAi(config, messages) {
  const endpoint = aiProviderEndpoint(config);
  const headers = { "Content-Type": "application/json" };
  let body;
  if (config.provider === "ollama") {
    body = { model: config.model, messages, stream: false, format: "json", options: { temperature: 0.1 } };
  } else {
    headers.Authorization = `Bearer ${config.apiKey}`;
    body = { model: config.model, messages, temperature: 0.1, response_format: { type: "json_object" } };
  }
  let response;
  try {
    response = await fetch(endpoint, { method: "POST", headers, body: JSON.stringify(body), signal: AbortSignal.timeout(45_000) });
  } catch (error) {
    const wrapped = new Error(error.name === "TimeoutError" ? "AI 服务响应超时" : "AI 服务不可达");
    wrapped.statusCode = 503;
    throw wrapped;
  }
  if (!response.ok) {
    const error = new Error(`AI 服务返回 HTTP ${response.status}`);
    error.statusCode = 503;
    throw error;
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  if (bytes.length > 512 * 1024) {
    const error = new Error("AI 响应超过大小限制");
    error.statusCode = 503;
    throw error;
  }
  let payload;
  try {
    payload = JSON.parse(bytes.toString("utf8"));
    const content = config.provider === "ollama" ? payload.message?.content : payload.choices?.[0]?.message?.content;
    return validateAiAnalysis(JSON.parse(content));
  } catch (error) {
    const wrapped = new Error(error.message.startsWith("AI 响应") ? error.message : "AI 服务返回了无效 JSON");
    wrapped.statusCode = 502;
    throw wrapped;
  }
}

function loadAiReviews() {
  if (!existsSync(aiReviewsPath)) return [];
  try {
    const parsed = JSON.parse(readFileSync(aiReviewsPath, "utf8"));
    if (!Array.isArray(parsed)) throw new Error("root must be an array");
    return parsed.filter((item) => item && typeof item.alertId === "string" && typeof item.evidenceHash === "string").slice(-1000);
  } catch (error) {
    console.error(`ai reviews: ${error.message}`);
    return [];
  }
}

function saveAiReviews() {
  mkdirSync(dataRoot, { recursive: true });
  const temporary = `${aiReviewsPath}.tmp`;
  writeFileSync(temporary, JSON.stringify(aiReviews.slice(-1000), null, 2), { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, aiReviewsPath);
}

function collectNumericEvidence(value, prefix = "evidence", output = {}) {
  if (Object.keys(output).length >= 40 || value === null || value === undefined) return output;
  if (typeof value === "number" && Number.isFinite(value)) output[prefix] = value;
  else if (typeof value === "boolean") output[prefix] = value;
  else if (Array.isArray(value)) value.slice(0, 12).forEach((item, index) => collectNumericEvidence(item, `${prefix}.${index}`, output));
  else if (typeof value === "object") Object.entries(value).slice(0, 24).forEach(([key, item]) => collectNumericEvidence(item, `${prefix}.${String(key).replace(/[^A-Za-z0-9_.-]/g, "_").slice(0, 40)}`, output));
  return output;
}

function aiReference(value) {
  return createHash("sha256").update(String(value || "unknown")).digest("hex").slice(0, 20);
}

function aiEvidenceText(value, secrets = [], maximum = 400) {
  let result = String(value ?? "").trim().slice(0, maximum);
  for (const secret of secrets.filter(Boolean)) result = result.split(String(secret)).join("[redacted]");
  return result;
}

async function analyzeEvidenceWithAi(scope, task, evidence, config = aiConfig) {
  const messages = [
    {
      role: "system",
      content: "你是游戏行为风控研判助手。只根据证据做辅助研判，不得建议永久封号、扣款或销毁资产。必须返回 JSON：summary 字符串，risk_level 为 normal/watch/high，confidence 为 0-100 整数，findings 为最多 8 个 {title,evidence,severity}，severity 为 low/medium/high，suggested_actions 为最多 6 个字符串。",
    },
    { role: "user", content: JSON.stringify({ task, evidence }) },
  ];
  return {
    ...(await callAi(config, messages)),
    scope,
    evidenceHash: createHash("sha256").update(JSON.stringify(evidence)).digest("hex"),
    provider: config.provider,
    model: config.model,
    generatedAt: new Date().toISOString(),
    advisoryOnly: true,
  };
}

function alertAiEvidence(alert) {
  const alertId = String(alert.alert_id || alert.id || "unknown").slice(0, 160);
  const actorId = String(alert.actor_id || alert.player || "unknown");
  return {
    alertId,
    actorRef: createHash("sha256").update(actorId).digest("hex").slice(0, 20),
    evidence: {
      alert_ref: createHash("sha256").update(alertId).digest("hex").slice(0, 20),
      actor_ref: createHash("sha256").update(actorId).digest("hex").slice(0, 20),
      rule_code: String(alert.rule_code || alert.rule || "unknown").slice(0, 100),
      category: String(alert.category || "unknown").slice(0, 80),
      severity: String(alert.severity || "unknown").slice(0, 32),
      deterministic_score: Number(alert.score || 0),
      numeric_facts: collectNumericEvidence(alert.evidence),
    },
  };
}

async function analyzeAlertWithAi(alert, config = aiConfig) {
  const scoped = alertAiEvidence(alert);
  return {
    ...(await analyzeEvidenceWithAi("alert", "分析该规则告警是否具有复核价值，并指出还缺少哪些证据", scoped.evidence, config)),
    alertId: scoped.alertId,
    actorRef: scoped.actorRef,
  };
}

function playerAiEvidence(player) {
  const playerId = String(player.id || player.gid || "unknown");
  const secrets = [playerId, player.name, player.account, player.server];
  return {
    playerRef: aiReference(playerId),
    evidence: {
      player_ref: aiReference(playerId),
      deterministic_score: Number(player.score || 0),
      status: aiEvidenceText(player.status, secrets, 40),
      tags: Array.isArray(player.tags) ? player.tags.slice(0, 12).map((tag) => aiEvidenceText(tag, secrets, 80)) : [],
      metrics: Array.isArray(player.metrics) ? player.metrics.slice(0, 16).map(([label, value]) => ({ label: aiEvidenceText(label, secrets, 80), value: aiEvidenceText(value, secrets, 120) })) : [],
      timeline: Array.isArray(player.timeline) ? player.timeline.slice(0, 30).map(([time, action, change, judgment]) => ({
        time: aiEvidenceText(time, secrets, 40),
        action: aiEvidenceText(action, secrets, 100),
        change: aiEvidenceText(change, secrets, 160),
        judgment: aiEvidenceText(judgment, secrets, 120),
      })) : [],
    },
  };
}

async function analyzePlayerWithAi(player, config = aiConfig) {
  const scoped = playerAiEvidence(player);
  return {
    ...(await analyzeEvidenceWithAi("player", "结合确定性评分、群体偏离指标与关键行为时间线，研判该玩家是否需要人工复核，并明确缺失证据", scoped.evidence, config)),
    playerRef: scoped.playerRef,
  };
}

function assetAiEvidence(asset) {
  const assetId = String(asset.id || "unknown");
  const ownerParts = String(asset.owner || "").split(/\s*\/\s*/).filter(Boolean);
  const secrets = [assetId, asset.owner, ...ownerParts];
  return {
    assetRef: aiReference(assetId),
    evidence: {
      asset_ref: aiReference(assetId),
      name: aiEvidenceText(asset.name, secrets, 120),
      quantity: Number(asset.quantity || 0),
      state: aiEvidenceText(asset.state, secrets, 80),
      deterministic_score: Number(asset.risk || 0),
      source: aiEvidenceText(asset.source, secrets, 160),
      path: Array.isArray(asset.nodes) ? asset.nodes.slice(0, 40).map(([time, action, owner, note]) => ({
        time: aiEvidenceText(time, secrets, 40),
        action: aiEvidenceText(action, secrets, 100),
        owner_ref: aiReference(owner),
        note: aiEvidenceText(note, secrets, 240),
      })) : [],
    },
  };
}

async function analyzeAssetWithAi(asset, config = aiConfig) {
  const scoped = assetAiEvidence(asset);
  return {
    ...(await analyzeEvidenceWithAi("asset", "检查资产生成、持有、转移与当前状态是否闭合，指出可疑节点、证据缺口和建议复核动作", scoped.evidence, config)),
    assetRef: scoped.assetRef,
  };
}

async function agentAlerts() {
  if (agentLocalToken.length < 32) return [];
  const response = await fetch(`http://127.0.0.1:${agentPort}/agent/v1/alerts`, {
    headers: { "X-PGR-Local-Token": agentLocalToken },
    signal: AbortSignal.timeout(3000),
  });
  if (!response.ok) throw new Error(`agent alerts HTTP ${response.status}`);
  const result = await response.json();
  return Array.isArray(result.alerts) ? result.alerts : [];
}

function isReviewableAlert(alert) {
  if (alert.status && !["open", "pending"].includes(alert.status)) return false;
  return !["已复核", "已阻断", "已关闭"].includes(alert.state);
}

async function aiReviewAlerts() {
  const sources = [];
  try {
    sources.push(...await agentAlerts());
  } catch (error) {
    console.error(`ai review agent source: ${error.message}`);
  }
  try {
    const databaseAlerts = databaseConfig.enabled ? await liveData("alerts") : alerts;
    if (Array.isArray(databaseAlerts)) sources.push(...databaseAlerts);
  } catch (error) {
    console.error(`ai review database source: ${error.message}`);
  }
  const unique = new Map();
  for (const alert of sources) {
    if (!alert || typeof alert !== "object" || !isReviewableAlert(alert)) continue;
    const id = String(alert.alert_id || alert.id || "").slice(0, 160);
    if (id && !unique.has(id)) unique.set(id, alert);
  }
  return [...unique.values()];
}

async function runAiReviewWorker() {
  if (!aiConfig.enabled || aiReviewWorkerRunning) return;
  aiReviewWorkerRunning = true;
  try {
    const maximumPerHour = Math.max(1, Math.min(1000, Number(process.env.RISK_AI_MAX_REVIEWS_PER_HOUR || 30)));
    const recentCount = aiReviews.filter((review) => Date.now() - Date.parse(review.generatedAt) < 60 * 60 * 1000).length;
    let remaining = Math.max(0, maximumPerHour - recentCount);
    if (remaining === 0) return;
    const source = await aiReviewAlerts();
    for (const alert of source) {
      if (remaining === 0) break;
      const scoped = alertAiEvidence(alert);
      const evidenceHash = createHash("sha256").update(JSON.stringify(scoped.evidence)).digest("hex");
      if (aiReviews.some((review) => review.alertId === scoped.alertId && review.evidenceHash === evidenceHash)) continue;
      const review = await analyzeAlertWithAi(alert);
      aiReviews = aiReviews.filter((item) => item.alertId !== review.alertId);
      aiReviews.push(review);
      saveAiReviews();
      remaining -= 1;
    }
  } catch (error) {
    console.error(`ai review worker: ${error.message}`);
  } finally {
    aiReviewWorkerRunning = false;
  }
}

function loadSdkCredentials() {
  if (!existsSync(sdkCredentialsPath)) return [];
  try {
    const parsed = JSON.parse(readFileSync(sdkCredentialsPath, "utf8"));
    if (!Array.isArray(parsed)) throw new Error("root must be an array");
    return parsed.filter((item) => item && typeof item.id === "string" && /^[a-f0-9]{64}$/.test(item.keyHash || ""));
  } catch (error) {
    console.error(`sdk credentials: ${error.message}`);
    return [];
  }
}

function saveSdkCredentials() {
  mkdirSync(dataRoot, { recursive: true });
  const temporary = `${sdkCredentialsPath}.tmp`;
  writeFileSync(temporary, JSON.stringify(sdkCredentials, null, 2), { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, sdkCredentialsPath);
}

function loadCaseActions() {
  if (!existsSync(caseActionsPath)) return { cases: [], actions: [] };
  try {
    const parsed = JSON.parse(readFileSync(caseActionsPath, "utf8"));
    if (!Array.isArray(parsed.cases) || !Array.isArray(parsed.actions)) throw new Error("invalid root");
    return parsed;
  } catch (error) {
    console.error(`case actions: ${error.message}`);
    return { cases: [], actions: [] };
  }
}

function saveCaseActions() {
  mkdirSync(dirname(caseActionsPath), { recursive: true });
  const temporary = `${caseActionsPath}.tmp`;
  writeFileSync(temporary, JSON.stringify(caseActions, null, 2), { encoding: "utf8", mode: 0o600 });
  renameSync(temporary, caseActionsPath);
}

function cleanText(value, field, maximum = 500) {
  const result = String(value || "").trim();
  if (!result || result.length > maximum || /[\u0000-\u0009\u000b\u000c\u000e-\u001f\u007f]/.test(result)) throw new Error(`${field}无效`);
  return result;
}

function assetQuery(url) {
  const result = String(url.searchParams.get("q") || "").trim();
  if ([...result].length > 128 || /[\u0000-\u001f\u007f]/.test(result)) {
    const error = new Error("资产查询条件无效");
    error.statusCode = 400;
    throw error;
  }
  return result;
}

function demoAssetSearch(query) {
  const needle = query.toLocaleLowerCase("zh-CN");
  const results = assets
    .filter((item) => !needle || [item.id, item.name, item.owner, item.source].some((value) => String(value).toLocaleLowerCase("zh-CN").includes(needle)))
    .slice(0, 50)
    .map((item) => ({ id: item.id, name: item.name, kind: "道具", owner: item.owner, quantity: item.quantity, location: item.state, updatedAt: item.nodes.at(-1)?.[0] || "" }));
  return { query, truncated: false, results };
}

function alertIdentity(alert) {
  return String(alert?.id || alert?.alert_id || "").slice(0, 160);
}

function alertActorId(alert) {
  if (alert?.actor_id) return String(alert.actor_id);
  const player = players.find((item) => item.name === alert?.player || item.id === String(alert?.player || ""));
  return player?.id || "";
}

async function alertSource() {
  return databaseConfig.enabled ? await liveData("alerts") : alerts;
}

async function alertById(id) {
  const safeId = cleanText(id, "告警编号", 160);
  const alert = (await alertSource()).find((item) => alertIdentity(item) === safeId);
  if (!alert) {
    const error = new Error("告警不存在");
    error.statusCode = 404;
    throw error;
  }
  return alert;
}

function caseFor(alertId, create = false) {
  let item = caseActions.cases.find((candidate) => candidate.alertId === alertId);
  if (!item && create) {
    const now = new Date().toISOString();
    item = { id: `case_${randomBytes(10).toString("hex")}`, alertId, status: "open", decision: null, note: "", createdAt: now, updatedAt: now, history: [] };
    caseActions.cases.push(item);
  }
  return item || { alertId, status: "open", decision: null, note: "", history: [], createdAt: null, updatedAt: null };
}

function publicAction(action) {
  return {
    id: action.id,
    alertId: action.alertId,
    tenantId: action.tenantId,
    serverId: action.serverId,
    type: action.type,
    target: action.target,
    reason: action.reason,
    status: action.status,
    requestedAt: action.requestedAt,
    leasedAt: action.leasedAt || null,
    completedAt: action.completedAt || null,
    executionRef: action.executionRef || null,
    message: action.message || null,
  };
}

function publicCase(item) {
  return { id: item.id || null, alertId: item.alertId, status: item.status, decision: item.decision, note: item.note, createdAt: item.createdAt, updatedAt: item.updatedAt, history: item.history || [] };
}

async function alertDetail(id) {
  const alert = await alertById(id);
  const actorId = alertActorId(alert);
  let player = null;
  if (actorId || alert.player) {
    try { player = await playerForQuery(actorId || alert.player); } catch { player = { id: actorId, name: alert.player || actorId }; }
  }
  const relatedAssets = databaseConfig.enabled
    ? []
    : assets.filter((asset) => !actorId || asset.owner.includes(actorId));
  const review = aiReviews.find((item) => item.alertId === alertIdentity(alert)) || null;
  return {
    alert: { ...alert, id: alertIdentity(alert), actorId },
    player,
    assets: relatedAssets,
    aiReview: review,
    case: publicCase(caseFor(alertIdentity(alert))),
    actions: caseActions.actions.filter((item) => item.alertId === alertIdentity(alert)).map(publicAction).reverse(),
    credentials: sdkCredentials.filter((item) => !item.revokedAt).map(publicSdkCredential),
  };
}

function recordDecision(alertId, input) {
  const decision = String(input.decision || "");
  if (!new Set(["watch", "dismiss", "escalate"]).has(decision)) throw new Error("研判决定无效");
  const note = cleanText(input.note, "研判说明", 1000);
  const item = caseFor(alertId, true);
  const now = new Date().toISOString();
  item.decision = decision;
  item.status = decision;
  item.note = note;
  item.updatedAt = now;
  item.history.push({ at: now, event: "decision", decision, note });
  saveCaseActions();
  return item;
}

function normalizeActionTarget(type, input, actorId) {
  const target = input && typeof input === "object" ? input : {};
  if (type === "asset.freeze") return { assetId: cleanText(target.assetId, "资产编号", 160) };
  const requestedActor = cleanText(target.actorId, "角色编号", 128);
  if (actorId && requestedActor !== actorId) throw new Error("处置角色与告警角色不一致");
  if (type === "session.kick") return { actorId: requestedActor, sessionId: String(target.sessionId || "").trim().slice(0, 160) || null };
  if (type === "account.suspend") {
    const durationMinutes = Number(target.durationMinutes);
    if (!Number.isInteger(durationMinutes) || durationMinutes < 5 || durationMinutes > 525600) throw new Error("封停时长必须为 5 到 525600 分钟");
    return { actorId: requestedActor, durationMinutes };
  }
  if (type === "account.ban") return { actorId: requestedActor };
  if (type === "currency.deduct") {
    const amount = Number(target.amount);
    const currency = cleanText(target.currency, "货币类型", 32);
    if (!Number.isSafeInteger(amount) || amount < 1) throw new Error("扣除数量必须为正整数");
    return { actorId: requestedActor, currency, amount };
  }
  throw new Error("处置类型无效");
}

function queueAction(alert, input) {
  const credential = sdkCredentials.find((item) => item.id === input.credentialId && !item.revokedAt);
  if (!credential) throw new Error("请选择有效的区服插件凭据");
  const type = String(input.type || "");
  const target = normalizeActionTarget(type, input.target, alertActorId(alert));
  const confirmationTarget = type === "asset.freeze" ? target.assetId : target.actorId;
  if (String(input.confirmation || "").trim() !== confirmationTarget) throw new Error(`请输入 ${confirmationTarget} 确认处置对象`);
  if (new Set(["account.ban", "currency.deduct"]).has(type) && input.acknowledgeIrreversible !== true) throw new Error("必须确认不可逆操作风险");
  const reason = cleanText(input.reason, "处置原因", 1000);
  if (reason.length < 8) throw new Error("处置原因至少填写 8 个字符");
  const item = caseFor(alertIdentity(alert), true);
  const now = new Date().toISOString();
  const action = {
    id: `act_${randomBytes(12).toString("hex")}`,
    alertId: alertIdentity(alert),
    caseId: item.id,
    credentialId: credential.id,
    tenantId: credential.tenantId,
    serverId: credential.serverId,
    type,
    target,
    reason,
    status: "pending",
    requestedAt: now,
    attemptCount: 0,
  };
  caseActions.actions.push(action);
  item.status = "action_pending";
  item.updatedAt = now;
  item.history.push({ at: now, event: "action_queued", actionId: action.id, type });
  saveCaseActions();
  return action;
}

function pullActions(credential, input) {
  const limit = Math.max(1, Math.min(20, Number.isInteger(Number(input.limit)) ? Number(input.limit) : 10));
  const now = Date.now();
  // ponytail: 单节点采用 at-least-once 租约；插件必须按 action.id 幂等，扩容时换共享队列。
  const selected = caseActions.actions.filter((item) => item.credentialId === credential.id && (item.status === "pending" || item.status === "leased")).slice(0, limit);
  for (const item of selected) {
    const alreadyLeased = item.status === "leased" && Date.parse(item.leaseUntil || 0) > now;
    item.status = "leased";
    if (!alreadyLeased) {
      item.leasedAt = new Date(now).toISOString();
      item.leaseUntil = new Date(now + 30_000).toISOString();
      item.attemptCount += 1;
    }
  }
  if (selected.length) saveCaseActions();
  return selected.map(publicAction);
}

function acknowledgeAction(credential, input) {
  const actionId = cleanText(input.actionId, "命令编号", 160);
  const status = String(input.status || "");
  if (!new Set(["applied", "failed", "rejected"]).has(status)) throw new Error("回执状态无效");
  const action = caseActions.actions.find((item) => item.id === actionId && item.credentialId === credential.id);
  if (!action) {
    const error = new Error("命令不存在或不属于当前区服");
    error.statusCode = 404;
    throw error;
  }
  if (new Set(["applied", "failed", "rejected"]).has(action.status)) {
    if (action.status !== status) {
      const error = new Error("命令已有不同终态");
      error.statusCode = 409;
      throw error;
    }
    return action;
  }
  const now = new Date().toISOString();
  action.status = status;
  action.completedAt = now;
  action.executionRef = String(input.executionRef || "").trim().slice(0, 160) || null;
  action.message = String(input.message || "").trim().slice(0, 500) || null;
  const item = caseFor(action.alertId, true);
  item.status = status === "applied" ? "action_applied" : `action_${status}`;
  item.updatedAt = now;
  item.history.push({ at: now, event: "action_ack", actionId, status, executionRef: action.executionRef, message: action.message });
  saveCaseActions();
  return action;
}

function sdkIdentifier(value, field) {
  const result = String(value || "").trim();
  if (!/^[A-Za-z0-9_.-]{1,128}$/.test(result)) throw new Error(`${field} 只能包含字母、数字、点、下划线和连字符`);
  return result;
}

function publicSdkCredential(item) {
  return {
    id: item.id,
    name: item.name,
    tenantId: item.tenantId,
    serverId: item.serverId,
    prefix: item.prefix,
    status: item.revokedAt ? "revoked" : "active",
    createdAt: item.createdAt,
    revokedAt: item.revokedAt || null,
    lastUsedAt: item.lastUsedAt || null,
  };
}

function createSdkCredential(input, replacementFor = null) {
  const tenantId = sdkIdentifier(input.tenantId, "租户编号");
  const serverId = sdkIdentifier(input.serverId, "区服编号");
  const name = String(input.name || `${serverId} 插件`).trim().slice(0, 80);
  if (!name) throw new Error("凭据名称不能为空");
  const secret = `pgr_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  const credential = {
    id: `sdk_${randomBytes(10).toString("hex")}`,
    name,
    tenantId,
    serverId,
    prefix: secret.slice(0, 12),
    keyHash: createHash("sha256").update(secret).digest("hex"),
    createdAt: now,
    revokedAt: null,
    lastUsedAt: null,
    replacementFor,
  };
  sdkCredentials.push(credential);
  return { credential, secret };
}

function sdkCredentialFor(req) {
  const authorization = req.headers.authorization || "";
  const match = /^Bearer ([A-Za-z0-9_-]{32,128})$/.exec(authorization);
  if (!match) return null;
  const candidateHash = hash(match[1]);
  // ponytail: O(n) 验证适合单机千把以内凭据；升级到 Go 控制面时改成摘要索引。
  return sdkCredentials.find((item) => !item.revokedAt && safeEqual(candidateHash, Buffer.from(item.keyHash, "hex"))) || null;
}

function requestFromTrustedProxy(req) {
  return trustedProxyIps.has(req.socket.remoteAddress || "");
}

function requestIsSecure(req) {
  if (req.socket.encrypted) return true;
  const forwardedProto = String(req.headers["x-forwarded-proto"] || "")
    .split(",", 1)[0]
    .trim()
    .toLowerCase();
  return behindTlsProxy && requestFromTrustedProxy(req) && forwardedProto === "https";
}

function clientIp(req) {
  if (behindTlsProxy && requestFromTrustedProxy(req)) {
    const forwarded = String(req.headers["x-forwarded-for"] || "").split(",", 1)[0].trim();
    if (isIP(forwarded)) return forwarded;
  }
  return req.socket.remoteAddress || "unknown";
}

function sdkTransportSecure(req) {
  if (allowInsecureSdk) return true;
  return requestIsSecure(req);
}

function sdkRateLimitOk(credential) {
  const now = Date.now();
  const recent = (sdkRateLimits.get(credential.id) || []).filter((stamp) => now - stamp < 60_000);
  if (recent.length >= 600) return false;
  recent.push(now);
  sdkRateLimits.set(credential.id, recent);
  return true;
}

function normalizeDatabaseConfig(input, fallback = databaseConfig) {
  if (!input || typeof input !== "object") throw new Error("数据库配置无效");
  const enabled = input.enabled === undefined ? Boolean(fallback.enabled) : input.enabled;
  const host = String(input.host ?? fallback.host ?? "").trim();
  const portValue = Number(input.port ?? fallback.port ?? 3306);
  const user = String(input.user ?? fallback.user ?? "").trim();
  const passwordInput = typeof input.password === "string" ? input.password : "";
  const password = passwordInput || fallback.password || "";
  const mainDatabase = String(input.mainDatabase ?? fallback.mainDatabase ?? "").trim();
  const logDatabase = String(input.logDatabase ?? fallback.logDatabase ?? "").trim();
  if (typeof enabled !== "boolean" || !host || host.length > 255) throw new Error("服务器地址无效");
  if (!Number.isInteger(portValue) || portValue < 1 || portValue > 65535) throw new Error("数据库端口无效");
  if (!user || user.length > 128) throw new Error("数据库账号无效");
  if (password.length > 512) throw new Error("数据库密码过长");
  if (!/^[A-Za-z0-9_]{1,64}$/.test(mainDatabase) || !/^[A-Za-z0-9_]{1,64}$/.test(logDatabase)) throw new Error("数据库名称无效");
  if (enabled && !password) throw new Error("请填写数据库密码");
  return { enabled, host, port: portValue, user, password, mainDatabase, logDatabase };
}

function publicDatabaseConfig() {
  return {
    enabled: databaseConfig.enabled,
    host: databaseConfig.host,
    port: databaseConfig.port,
    user: databaseConfig.user,
    mainDatabase: databaseConfig.mainDatabase,
    logDatabase: databaseConfig.logDatabase,
    passwordConfigured: Boolean(databaseConfig.password),
    persisted: databaseConfigStored,
  };
}

function databaseEnvironment(config = databaseConfig, caps = gameplayCaps) {
  return {
    ...process.env,
    WDSF_LIVE: config.enabled ? "1" : "0",
    WDSF_HOST: config.host,
    WDSF_DB_PORT: String(config.port),
    WDSF_DB_USER: config.user,
    WDSF_DB_PASSWORD: config.password,
    WDSF_MDB: config.mainDatabase,
    WDSF_LDB: config.logDatabase,
    RISK_GAMEPLAY_CAPS_JSON: JSON.stringify(caps.filter((cap) => cap.enabled)),
  };
}

function cookies(req) {
  return Object.fromEntries(
    (req.headers.cookie || "")
      .split(";")
      .map((part) => part.trim().split("="))
      .filter(([key, value]) => key && value),
  );
}

function sessionFor(req) {
  const token = cookies(req).pg_session;
  const expiresAt = token && sessions.get(token);
  if (!expiresAt || expiresAt < Date.now()) {
    if (token) sessions.delete(token);
    return null;
  }
  return token;
}

function pruneLoginState(now = Date.now()) {
  for (const [token, expiresAt] of sessions) if (expiresAt < now) sessions.delete(token);
  for (const [ip, stamps] of attempts) {
    const recent = stamps.filter((stamp) => now - stamp < 60_000);
    if (recent.length) attempts.set(ip, recent);
    else attempts.delete(ip);
  }
  // ponytail: single-process portal cap; use an external session store before horizontal scaling.
  while (sessions.size > 2048) sessions.delete(sessions.keys().next().value);
  while (attempts.size > 4096) attempts.delete(attempts.keys().next().value);
}

function json(res, status, body, headers = {}) {
  res.writeHead(status, { "Content-Type": "application/json; charset=utf-8", ...headers });
  res.end(JSON.stringify(body));
}

async function readBody(req, maximum = 16_384) {
  const chunks = [];
  let length = 0;
  for await (const chunk of req) {
    length += chunk.length;
    if (length > maximum) throw new Error("body_too_large");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

async function readJson(req, maximum) {
  const body = await readBody(req, maximum);
  return body.length ? JSON.parse(body.toString("utf8")) : {};
}

function dashboard() {
  return {
    updatedAt: new Date().toISOString(),
    sourceMode: "demo",
    headline: "资产流水稳定，风险正在收敛",
    description: "当前为演示数据；开启实时模式后将读取游戏数据库中的权威资产日志。",
    scope: "全部在线玩家",
    health: { status: "检测运行中", latency: "86 ms", coverage: "99.97%", backlog: 12 },
    metrics: [
      ["今日分析事件", "2,846,192", "+12.8%"],
      ["高风险玩家", "47", "-8"],
      ["暂存资产", "¥ 286,400", "+31.2%"],
      ["规则命中", "1,284", "+6.4%"],
    ],
    distribution: [52, 68, 61, 84, 72, 90, 77, 66, 88, 73, 58, 79],
    riskBands: [
      ["正常", 96.4, "green"],
      ["观察", 2.7, "gold"],
      ["高风险", 0.7, "coral"],
      ["已阻断", 0.2, "dark"],
    ],
    alerts: alerts.slice(0, 4),
  };
}

// 数据层已从 Python 迁移到 Rust（crates/wdsf-engine）。
// 二进制路径可用 WDSF_ENGINE 覆盖；默认找 cargo 的 release 产物，其次 debug。
function engineBinary() {
  if (process.env.WDSF_ENGINE) return process.env.WDSF_ENGINE;
  const name = process.platform === "win32" ? "wdsf-live-data.exe" : "wdsf-live-data";
  for (const profile of ["release", "debug"]) {
    const candidate = join(projectRoot, "target", profile, name);
    if (existsSync(candidate)) return candidate;
  }
  return name; // 交给 PATH 解析
}

async function liveData(operation, query = "", config = databaseConfig, caps = gameplayCaps) {
  const args = [operation];
  if (query) args.push(query.slice(0, 128));
  // 总览与告警要扫描全部角色，真实服上可能几十秒；12 秒会稳定超时。
  // 可用 RISK_ENGINE_TIMEOUT_MS 调整。
  const timeoutMs = Number(process.env.RISK_ENGINE_TIMEOUT_MS || 180_000);
  const startedAt = Date.now();
  try {
    const { stdout, stderr } = await execFileAsync(engineBinary(), args, {
      cwd: projectRoot,
      env: databaseEnvironment(config, caps),
      timeout: timeoutMs,
      windowsHide: true,
      maxBuffer: 2 * 1024 * 1024,
    });
    if (process.env.WDSF_QUERY_TRACE === "1" && stderr.trim()) console.error(stderr.trim());
    const result = JSON.parse(stdout);
    if (result.error) {
      const error = new Error(result.error);
      error.statusCode = 404;
      throw error;
    }
    return result;
  } catch (error) {
    if (error.statusCode) throw error;
    try {
      const result = JSON.parse(error.stdout || "{}");
      if (result.error) {
        const notFound = new Error(result.error);
        notFound.statusCode = 404;
        throw notFound;
      }
    } catch (parseError) {
      if (parseError.statusCode) throw parseError;
    }
    // 把引擎的真实报错透出来，否则前端只能看到"不可用"，没法自查。
    // 引擎从不打印密码；这里再做一次兜底擦除，防止任何路径把凭据带进响应。
    const redact = (text) => {
      let output = String(text || "").trim();
      const secret = config.password;
      if (secret) output = output.split(secret).join("***");
      return output.replace(/\s+/g, " ").slice(0, 400);
    };
    // execFile 在超时/被信号杀掉时 stderr 是空的，只留一句 "Command failed: <cmd>"，
    // 看不出到底是超时、被系统杀掉，还是引擎自己非零退出。这里把原因翻译出来。
    const seconds = Math.round((Date.now() - startedAt) / 1000);
    let reason = "";
    if (error.killed && error.signal === "SIGTERM") {
      reason = `引擎超过 ${Math.round(timeoutMs / 1000)} 秒未返回，已被超时终止（${seconds} 秒；调大 RISK_ENGINE_TIMEOUT_MS 可放宽）`;
    } else if (error.signal) {
      reason = `引擎被信号 ${error.signal} 终止（${seconds} 秒；二进制架构不符或被系统安全策略拦截时会这样）`;
    } else if (error.code === "ENOENT") {
      reason = `找不到引擎二进制 ${engineBinary()}，请先 cargo build --release -p wdsf-engine`;
    } else if (error.code === "ENOEXEC" || error.code === "EBADARCH") {
      reason = `引擎二进制不是本机架构（${error.code}），请在本机重新 cargo build --release -p wdsf-engine`;
    } else if (typeof error.code === "number" && error.code !== 0) {
      reason = `引擎退出码 ${error.code}（${seconds} 秒）`;
    }
    const detail = [redact(error.stderr), reason].filter(Boolean).join("；") || redact(error.message);
    console.error(`live data ${operation}: ${detail}`);
    const wrapped = new Error(detail ? `实时数据源不可用：${detail}` : "实时数据源不可用");
    wrapped.statusCode = 503;
    wrapped.cause = error;
    throw wrapped;
  }
}

async function runCollector() {
  if (!databaseConfig.enabled || collectorRunning) return;
  collectorRunning = true;
  try {
    await liveData("collect-once");
  } catch (error) {
    console.error(`risk collector: ${error.message}`);
  } finally {
    collectorRunning = false;
  }
}

function contentType(pathname) {
  return {
    ".html": "text/html; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".png": "image/png",
    ".svg": "image/svg+xml",
    ".ico": "image/x-icon",
  }[extname(pathname)] || "application/octet-stream";
}

function serveFile(res, pathname) {
  const route = pathname === "/" ? "/index.html" : pathname === "/app" ? "/app.html" : pathname;
  const absolute = resolve(publicRoot, `.${normalize(route)}`);
  if (!absolute.startsWith(`${publicRoot}${sep}`) || !existsSync(absolute) || !statSync(absolute).isFile()) return false;
  res.writeHead(200, { "Content-Type": contentType(absolute), "Cache-Control": "no-cache" });
  createReadStream(absolute).pipe(res);
  return true;
}

function artifactInfo(id, artifact) {
  if (!existsSync(artifact.path) || !statSync(artifact.path).isFile()) {
    return { id, label: artifact.label, filename: artifact.filename, available: false };
  }
  const body = readFileSync(artifact.path);
  return {
    id,
    label: artifact.label,
    filename: artifact.filename,
    available: true,
    size: body.length,
    sha256: createHash("sha256").update(body).digest("hex"),
    url: `/api/integration/downloads/${id}`,
  };
}

function serveIntegrationArtifact(res, id) {
  const artifact = integrationArtifacts.get(id);
  if (!artifact || !existsSync(artifact.path) || !statSync(artifact.path).isFile()) {
    return json(res, 404, { error: "接入资料不存在" });
  }
  const size = statSync(artifact.path).size;
  res.writeHead(200, {
    "Content-Type": artifact.type,
    "Content-Length": size,
    "Content-Disposition": `attachment; filename="${artifact.filename}"`,
    "Cache-Control": "private, no-store",
  });
  createReadStream(artifact.path).pipe(res);
}

async function agentHealth() {
  if (!Number.isInteger(agentPort) || agentPort < 1 || agentPort > 65535) {
    return { connected: false, endpoint: "127.0.0.1", error: "PGR_AGENT_PORT 配置无效" };
  }
  const endpoint = `http://127.0.0.1:${agentPort}`;
  try {
    const response = await fetch(`${endpoint}/agent/v1/health`, { signal: AbortSignal.timeout(1200) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const health = await response.json();
    if (!health || health.ok !== true) throw new Error("响应格式无效");
    return { connected: true, endpoint, ...health };
  } catch (error) {
    return { connected: false, endpoint, error: error.name === "TimeoutError" ? "Agent 响应超时" : "Agent 未运行或不可达" };
  }
}

async function integrationOverview() {
  return {
    agent: await agentHealth(),
    artifacts: [...integrationArtifacts].map(([id, artifact]) => artifactInfo(id, artifact)),
    sdkCredentials: sdkCredentials.map(publicSdkCredential).sort((left, right) => right.createdAt.localeCompare(left.createdAt)),
    remoteGateway: {
      path: "/sdk/v1",
      requiresHttps: !allowInsecureSdk,
      configured: agentLocalToken.length >= 32,
    },
    contract: { schemaVersion: "1.0", abiVersion: 1, interfaceCount: 7, eventTypeCount: 13, realtimeRuleCount: 17 },
  };
}

async function playerForQuery(query = "") {
  const normalized = String(query || "").trim().slice(0, 128);
  if (databaseConfig.enabled) return await liveData("player", normalized);
  const search = normalized.toLowerCase() || players[0].id;
  const player = players.find((item) => [item.id, item.name.toLowerCase(), item.account].includes(search));
  if (player) return player;
  const error = new Error("未找到匹配玩家");
  error.statusCode = 404;
  throw error;
}

async function assetForQuery(query = "") {
  const normalized = String(query || "").trim().slice(0, 128);
  if (databaseConfig.enabled) return await liveData("asset", normalized);
  const assetId = normalized.toUpperCase() || assets[0].id;
  const asset = assets.find((item) => item.id === assetId);
  if (asset) return asset;
  const error = new Error("未找到资产流水");
  error.statusCode = 404;
  throw error;
}

async function forwardSdkRequest(req, res, pathname) {
  if (!sdkTransportSecure(req)) return json(res, 426, { error: "https_required", code: "https_required" });
  const credential = sdkCredentialFor(req);
  if (!credential) return json(res, 401, { error: "unauthorized", code: "unauthorized" });
  if (!sdkRateLimitOk(credential)) return json(res, 429, { error: "rate_limited", code: "rate_limited" });
  if (req.method !== "POST") return json(res, 404, { error: "not_found", code: "not_found" });
  if (req.headers["content-type"]?.split(";")[0].trim().toLowerCase() !== "application/json") {
    return json(res, 415, { error: "application/json required", code: "unsupported_media_type" });
  }
  if (pathname === "/sdk/v1/actions:pull") {
    const actions = pullActions(credential, await readJson(req, 16 * 1024));
    credential.lastUsedAt = new Date().toISOString();
    return json(res, 200, { schemaVersion: "1.0", leaseSeconds: 30, actions });
  }
  if (pathname === "/sdk/v1/actions:ack") {
    const action = acknowledgeAction(credential, await readJson(req, 16 * 1024));
    credential.lastUsedAt = new Date().toISOString();
    return json(res, 200, { ok: true, action: publicAction(action) });
  }
  if (!agentLocalToken || agentLocalToken.length < 32) return json(res, 503, { error: "gateway_not_configured", code: "gateway_not_configured" });
  const agentPath = pathname === "/sdk/v1/events:batch"
    ? "/agent/v1/events:batch"
    : pathname === "/sdk/v1/decisions:check"
      ? "/agent/v1/decisions:check"
      : null;
  if (!agentPath) return json(res, 404, { error: "not_found", code: "not_found" });
  const body = await readBody(req, pathname.endsWith("events:batch") ? 256 * 1024 : 64 * 1024);
  let response;
  try {
    response = await fetch(`http://127.0.0.1:${agentPort}${agentPath}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-PGR-Local-Token": agentLocalToken,
        "X-PGR-Tenant-Id": credential.tenantId,
        "X-PGR-Server-Id": credential.serverId,
      },
      body,
      signal: AbortSignal.timeout(5000),
    });
  } catch {
    return json(res, 503, { error: "risk_agent_unavailable", code: "risk_agent_unavailable" });
  }
  credential.lastUsedAt = new Date().toISOString();
  const responseBody = Buffer.from(await response.arrayBuffer());
  res.writeHead(response.status, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" });
  res.end(responseBody);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);
  res.setHeader("X-Content-Type-Options", "nosniff");
  res.setHeader("Referrer-Policy", "no-referrer");
  res.setHeader("X-Frame-Options", "DENY");
  res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
  res.setHeader("Permissions-Policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()");
  if (requestIsSecure(req)) res.setHeader("Strict-Transport-Security", "max-age=31536000");
  res.setHeader(
    "Content-Security-Policy",
    "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
  );

  try {
    if (url.pathname.startsWith("/sdk/v1/")) return await forwardSdkRequest(req, res, url.pathname);

    if (req.method === "POST" && url.pathname === "/api/login") {
      pruneLoginState();
      const ip = clientIp(req);
      const recent = (attempts.get(ip) || []).filter((stamp) => Date.now() - stamp < 60_000);
      if (recent.length >= 8) return json(res, 429, { error: "尝试过于频繁，请稍后再试" });
      const { key = "" } = await readJson(req);
      recent.push(Date.now());
      attempts.set(ip, recent);
      if (typeof key !== "string" || !safeEqual(hash(key.trim()), expectedKeyHash)) {
        return json(res, 401, { error: "卡密无效或已失效" });
      }
      attempts.delete(ip);
      const token = randomBytes(32).toString("hex");
      sessions.set(token, Date.now() + 12 * 60 * 60 * 1000);
      const secure = requestIsSecure(req) ? "; Secure" : "";
      return json(res, 200, { ok: true }, { "Set-Cookie": `pg_session=${token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200${secure}` });
    }

    if (req.method === "POST" && url.pathname === "/api/logout") {
      const token = sessionFor(req);
      if (token) sessions.delete(token);
      const secure = requestIsSecure(req) ? "; Secure" : "";
      return json(res, 200, { ok: true }, { "Set-Cookie": `pg_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0${secure}` });
    }

    if (url.pathname.startsWith("/api/")) {
      if (!sessionFor(req)) return json(res, 401, { error: "unauthorized" });
      if (req.method === "GET" && url.pathname === "/api/session") return json(res, 200, { authenticated: true });
      if (req.method === "GET" && url.pathname === "/api/integration") return json(res, 200, await integrationOverview());
      if (req.method === "GET" && url.pathname === "/api/sdk-keys") {
        return json(res, 200, { credentials: sdkCredentials.map(publicSdkCredential) });
      }
      if (req.method === "POST" && url.pathname === "/api/sdk-keys") {
        const created = createSdkCredential(await readJson(req));
        saveSdkCredentials();
        return json(res, 201, { credential: publicSdkCredential(created.credential), secret: created.secret });
      }
      const sdkAction = /^\/api\/sdk-keys\/([A-Za-z0-9_-]+)\/(revoke|rotate)$/.exec(url.pathname);
      if (req.method === "POST" && sdkAction) {
        const current = sdkCredentials.find((item) => item.id === sdkAction[1]);
        if (!current) return json(res, 404, { error: "SDK 凭据不存在" });
        if (current.revokedAt) return json(res, 409, { error: "SDK 凭据已经吊销" });
        current.revokedAt = new Date().toISOString();
        if (sdkAction[2] === "revoke") {
          saveSdkCredentials();
          return json(res, 200, { credential: publicSdkCredential(current) });
        }
        const replacement = createSdkCredential(current, current.id);
        saveSdkCredentials();
        return json(res, 201, { credential: publicSdkCredential(replacement.credential), secret: replacement.secret });
      }
      if (req.method === "GET" && url.pathname.startsWith("/api/integration/downloads/")) {
        return serveIntegrationArtifact(res, url.pathname.slice("/api/integration/downloads/".length));
      }
      if (req.method === "GET" && url.pathname === "/api/settings/ai") {
        return json(res, 200, publicAiConfig());
      }
      if (req.method === "POST" && url.pathname === "/api/settings/ai/test") {
        const candidate = normalizeAiConfig({ ...(await readJson(req)), enabled: true });
        const result = await analyzeAlertWithAi({ alert_id: "provider-test", rule_code: "provider_connection_test", severity: "low", score: 0, evidence: { sample_count: 1 } }, candidate);
        return json(res, 200, { ok: true, provider: result.provider, model: result.model });
      }
      if (req.method === "POST" && url.pathname === "/api/settings/ai") {
        const candidate = normalizeAiConfig(await readJson(req));
        let test = null;
        if (candidate.enabled) {
          const checked = await analyzeAlertWithAi({ alert_id: "provider-save-test", rule_code: "provider_connection_test", severity: "low", score: 0, evidence: { sample_count: 1 } }, candidate);
          test = { ok: true, provider: checked.provider, model: checked.model };
        }
        saveAiConfig(candidate);
        aiConfig = candidate;
        if (candidate.enabled) void runAiReviewWorker();
        return json(res, 200, { ok: true, config: publicAiConfig(), test });
      }
      if (req.method === "GET" && url.pathname === "/api/ai/reviews") {
        return json(res, 200, { enabled: aiConfig.enabled, provider: aiConfig.provider, model: aiConfig.model, automatic: true, running: aiReviewWorkerRunning, reviews: aiReviews.slice().reverse() });
      }
      if (req.method === "POST" && url.pathname === "/api/ai/player") {
        if (!aiConfig.enabled) return json(res, 409, { error: "请先在规则与设置中启用 AI" });
        const query = cleanText((await readJson(req)).q, "玩家查询条件", 128);
        return json(res, 200, await analyzePlayerWithAi(await playerForQuery(query)));
      }
      if (req.method === "POST" && url.pathname === "/api/ai/asset") {
        if (!aiConfig.enabled) return json(res, 409, { error: "请先在规则与设置中启用 AI" });
        const query = cleanText((await readJson(req)).q, "资产查询条件", 128);
        return json(res, 200, await analyzeAssetWithAi(await assetForQuery(query)));
      }
      if (req.method === "GET" && url.pathname === "/api/settings/database") {
        return json(res, 200, publicDatabaseConfig());
      }
      if (req.method === "POST" && url.pathname === "/api/settings/database/test") {
        const candidate = normalizeDatabaseConfig({ ...(await readJson(req)), enabled: true });
        const result = await liveData("connection-test", "", candidate);
        return json(res, 200, result);
      }
      if (req.method === "POST" && url.pathname === "/api/settings/database") {
        const candidate = normalizeDatabaseConfig(await readJson(req));
        let test = null;
        if (candidate.enabled) test = await liveData("connection-test", "", candidate);
        saveDatabaseConfig(candidate);
        databaseConfig = candidate;
        if (candidate.enabled) void runCollector();
        return json(res, 200, { ok: true, config: publicDatabaseConfig(), test });
      }
      if (req.method === "GET" && url.pathname === "/api/dashboard") {
        return json(res, 200, databaseConfig.enabled ? await liveData("dashboard") : dashboard());
      }
      if (req.method === "GET" && url.pathname === "/api/player") {
        return json(res, 200, await playerForQuery(url.searchParams.get("q") || ""));
      }
      if (req.method === "GET" && url.pathname === "/api/assets") {
        const query = assetQuery(url);
        return json(res, 200, databaseConfig.enabled ? await liveData("asset-search", query) : demoAssetSearch(query));
      }
      if (req.method === "GET" && url.pathname === "/api/asset") {
        const query = assetQuery(url);
        return json(res, 200, await assetForQuery(query));
      }
      if (req.method === "GET" && url.pathname === "/api/alerts") {
        const severity = url.searchParams.get("severity") || "全部";
        const source = databaseConfig.enabled ? await liveData("alerts") : alerts;
        return json(res, 200, severity === "全部" ? source : source.filter((item) => item.severity === severity));
      }
      const alertRoute = /^\/api\/alerts\/([^/]+)(?:\/(decision|actions))?$/.exec(url.pathname);
      if (alertRoute) {
        const alertId = decodeURIComponent(alertRoute[1]);
        const alert = await alertById(alertId);
        if (req.method === "GET" && !alertRoute[2]) return json(res, 200, await alertDetail(alertId));
        if (req.method === "POST" && alertRoute[2] === "decision") {
          return json(res, 200, { case: publicCase(recordDecision(alertIdentity(alert), await readJson(req))) });
        }
        if (req.method === "POST" && alertRoute[2] === "actions") {
          return json(res, 201, { action: publicAction(queueAction(alert, await readJson(req))) });
        }
      }
      if (req.method === "GET" && url.pathname === "/api/rules") return json(res, 200, rules.map((rule) => ({ ...rule, mutable: false })));
      if (req.method === "GET" && url.pathname === "/api/settings/gameplay-caps") return json(res, 200, publicGameplayCapsState());
      if (req.method === "GET" && url.pathname === "/api/settings/gameplay-catalog") {
        if (!databaseConfig.enabled) return json(res, 200, { connected: false, windowDays: 30, actions: [] });
        return json(res, 200, await liveData("gameplay-catalog"));
      }
      if (req.method === "POST" && url.pathname === "/api/settings/gameplay-caps") {
        let caps;
        try {
          caps = normalizeGameplayCaps(await readJson(req));
        } catch (error) {
          error.statusCode = 400;
          throw error;
        }
        return json(res, 200, { ok: true, ...publishGameplayCaps(caps) });
      }
      if (req.method === "POST" && url.pathname === "/api/settings/gameplay-caps/replay") {
        if (!databaseConfig.enabled) return json(res, 409, { error: "请先连接游戏数据库" });
        let candidateCaps;
        try {
          candidateCaps = normalizeGameplayCaps(await readJson(req));
        } catch (error) {
          error.statusCode = 400;
          throw error;
        }
        const [baseline, candidate] = await Promise.all([
          liveData("alerts", "", databaseConfig, gameplayCaps),
          liveData("alerts", "", databaseConfig, candidateCaps),
        ]);
        return json(res, 200, {
          baselineVersion: gameplayCapsState.currentVersion,
          candidateVersion: gameplayCapsVersionId(candidateCaps),
          ...compareRuleReplay(baseline, candidate),
        });
      }
      if (req.method === "POST" && url.pathname === "/api/rules") {
        await readBody(req, 1024);
        return json(res, 405, { error: "内置规则目录只读；可配置规则请使用玩法上限版本" }, { Allow: "GET" });
      }
      return json(res, 404, { error: "not_found" });
    }

    if (req.method === "GET" && serveFile(res, url.pathname)) return;
    json(res, 404, { error: "not_found" });
  } catch (error) {
    const status = error.statusCode || (error.message === "body_too_large" ? 413 : 400);
    json(res, status, { error: error.statusCode ? error.message : "请求格式无效" });
  }
});

server.listen(port, host, () => {
  console.log(`Ponytail Risk Web listening on http://${host}:${port} (${databaseConfig.enabled ? "live" : "demo"})`);
  if (databaseConfig.enabled) runCollector();
  if (aiConfig.enabled) runAiReviewWorker();
  setInterval(runCollector, Math.max(10_000, Number(process.env.RISK_COLLECT_INTERVAL_MS || 30_000))).unref();
  setInterval(runAiReviewWorker, Math.max(15_000, Number(process.env.RISK_AI_REVIEW_INTERVAL_MS || 60_000))).unref();
});

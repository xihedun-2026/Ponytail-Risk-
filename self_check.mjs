import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createCipheriv, createHash, randomBytes } from "node:crypto";
import { existsSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { compareRuleReplay } from "./rule_replay.mjs";

// import.meta.dirname 要 Node 20.11+；这样算等价且不挑版本。
const projectRoot = fileURLToPath(new URL(".", import.meta.url));

const port = 4174;
const base = `http://127.0.0.1:${port}`;
const configPath = join(tmpdir(), `ponytail-risk-self-check-${process.pid}.json`);
const aiConfigPath = join(tmpdir(), `ponytail-risk-ai-config-${process.pid}.json`);
const aiReviewsPath = join(tmpdir(), `ponytail-risk-ai-reviews-${process.pid}.json`);
const sdkKeysPath = join(tmpdir(), `ponytail-risk-sdk-keys-${process.pid}.json`);
const caseActionsPath = join(tmpdir(), `ponytail-risk-case-actions-${process.pid}.json`);
const gameplayCapsPath = join(tmpdir(), `ponytail-risk-gameplay-caps-${process.pid}.json`);
const configMasterKey = "11".repeat(32);

function writeLegacyEncryptedConfig(path, purpose, value) {
  const key = createHash("sha256").update(`ponytail-risk-${purpose}:self-check-key`).digest();
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  cipher.setAAD(Buffer.from(`ponytail-risk-${purpose === "db" ? "database" : purpose}-v1`));
  const data = Buffer.concat([cipher.update(JSON.stringify(value), "utf8"), cipher.final()]);
  writeFileSync(path, JSON.stringify({ version: 1, iv: iv.toString("base64"), tag: cipher.getAuthTag().toString("base64"), data: data.toString("base64") }));
}

writeLegacyEncryptedConfig(configPath, "db", { enabled: false, host: "127.0.0.1", port: 3306, user: "legacy-reader", password: "", mainDatabase: "main_db", logDatabase: "log_db" });
writeLegacyEncryptedConfig(aiConfigPath, "ai", { enabled: false, provider: "groq", model: "qwen/qwen3.6-27b", apiKey: "" });
writeFileSync(gameplayCapsPath, JSON.stringify([{ action: "huilcbjl", label: "回合奖励", dailyLimit: 80, burst10mLimit: 8, enabled: true }]));
const replayCheck = compareRuleReplay(
  [{ id: "a", player: "A", score: 40, rule: "old" }, { id: "b", player: "B", score: 80, rule: "same" }],
  [{ id: "a", player: "A", score: 50, rule: "old" }, { id: "c", player: "C", score: 90, rule: "new" }],
);
assert.deepEqual(replayCheck.delta, { total: 0, added: 1, removed: 1, scoreChanged: 1 });
assert.equal(replayCheck.changes.length, 3);
const aiRequests = [];
const aiMock = createServer(async (req, res) => {
  let body = "";
  for await (const chunk of req) body += chunk;
  aiRequests.push({ authorization: req.headers.authorization, body });
  const content = JSON.stringify({
    summary: "规则证据具有复核价值，暂不建议自动处罚。",
    risk_level: "watch",
    confidence: 82,
    findings: [{ title: "数值异常", evidence: "确定性规则分数与计数需要人工核验", severity: "medium" }],
    suggested_actions: ["核对原始资产流水"],
  });
  res.writeHead(200, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ choices: [{ message: { content } }] }));
});
const agentMock = createServer(async (req, res) => {
  for await (const _chunk of req) {}
  res.setHeader("Content-Type", "application/json");
  if (req.method === "GET" && req.url === "/agent/v1/health") return res.end(JSON.stringify({ ok: true, mode: "shadow", bind: "127.0.0.1:17879", schema_versions: ["1.0"], queue_depth: 0, open_alerts: 1, realtime_rules: [] }));
  if (req.method === "GET" && req.url === "/agent/v1/alerts") return res.end(JSON.stringify({ alerts: [{ alert_id: "alert-auto-1", actor_id: "secret-player-name", rule_code: "rapid_gold_gain", category: "currency", severity: "high", score: 88, status: "open", evidence: { gain: 2000000, window_seconds: 600, client_ip: "10.0.0.8" } }] }));
  res.statusCode = 400;
  res.end(JSON.stringify({ error: "invalid batch", code: "invalid_batch" }));
});
await new Promise((resolve) => aiMock.listen(4176, "127.0.0.1", resolve));
await new Promise((resolve) => agentMock.listen(17879, "127.0.0.1", resolve));
const child = spawn(process.execPath, ["server.mjs"], {
  cwd: projectRoot,
  env: {
    ...process.env,
    RISK_PORT: String(port),
    RISK_PORTAL_KEY: "self-check-key",
    RISK_CONFIG_MASTER_KEY: configMasterKey,
    RISK_BEHIND_TLS_PROXY: "1",
    RISK_TRUSTED_PROXY_IPS: "127.0.0.1,::1,::ffff:127.0.0.1",
    RISK_DB_CONFIG_PATH: configPath,
    RISK_AI_CONFIG_PATH: aiConfigPath,
    RISK_AI_REVIEWS_PATH: aiReviewsPath,
    RISK_AI_GROQ_ENDPOINT: "http://127.0.0.1:4176/openai/v1/chat/completions",
    RISK_SDK_KEYS_PATH: sdkKeysPath,
    RISK_CASE_ACTIONS_PATH: caseActionsPath,
    RISK_GAMEPLAY_CAPS_PATH: gameplayCapsPath,
    RISK_SDK_ALLOW_INSECURE: "1",
    PGR_AGENT_LOCAL_TOKEN: "self-check-agent-token-that-is-long-enough",
    PGR_AGENT_PORT: "17879",
    WDSF_LIVE: "0",
  },
  stdio: ["ignore", "pipe", "pipe"],
  windowsHide: true,
});

async function waitForServer() {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    try {
      const response = await fetch(`${base}/`);
      if (response.ok) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("server did not start");
}

try {
  await waitForServer();

  const rejected = await fetch(`${base}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key: "wrong-key" }),
  });
  assert.equal(rejected.status, 401);

  const login = await fetch(`${base}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Forwarded-Proto": "https" },
    body: JSON.stringify({ key: "self-check-key" }),
  });
  assert.equal(login.status, 200);
  const setCookie = login.headers.get("set-cookie") || "";
  assert.match(setCookie, /; Secure(?:;|$)/);
  const cookie = setCookie.split(";")[0];
  assert.ok(cookie?.startsWith("pg_session="));
  const headers = { Cookie: cookie };
  assert.equal(JSON.parse(readFileSync(configPath, "utf8")).version, 2);
  assert.equal(JSON.parse(readFileSync(aiConfigPath, "utf8")).version, 2);

  const dashboard = await fetch(`${base}/api/dashboard`, { headers });
  assert.equal(dashboard.status, 200);
  const dashboardData = await dashboard.json();
  assert.equal(dashboardData.metrics.length, 4);
  assert.equal(typeof dashboardData.health.coverage, "string");
  assert.equal(Number.isFinite(Number(dashboardData.health.backlog)), true);
  const appSource = readFileSync(join(projectRoot, "public", "app.js"), "utf8");
  const appHtmlSource = readFileSync(join(projectRoot, "public", "app.html"), "utf8");
  const styleSource = readFileSync(join(projectRoot, "public", "styles.css"), "utf8");
  const serverSource = readFileSync(join(projectRoot, "server.mjs"), "utf8");
  const linuxInstallerSource = readFileSync(join(projectRoot, "deploy", "install.sh"), "utf8");
  const linuxBuilderSource = readFileSync(join(projectRoot, "deploy", "build-linux-bundle.sh"), "utf8");
  assert.match(appSource, /class="health-summary"/);
  assert.match(appSource, /progressRail\("数据表覆盖"/);
  assert.match(appSource, /IntersectionObserver/);
  assert.doesNotMatch(appSource, /style="--/);
  assert.match(styleSource, /\.progress-value/);
  assert.match(styleSource, /\.progress-width-100\{width:100%\}/);
  assert.match(styleSource, /@keyframes progress-flow/);
  assert.match(styleSource, /\.progress-value::after/);
  assert.match(styleSource, /transform:translateX\(-120%\)/);
  assert.match(styleSource, /transform:translateX\(400%\)/);
  assert.match(styleSource, /92%, 100% \{ opacity:0; transform:translateX\(400%\); \}/);
  assert.match(appHtmlSource, /\/vendor\/lucide-0\.468\.0\.min\.js/);
  assert.doesNotMatch(appHtmlSource, /unpkg\.com/);
  assert.match(appHtmlSource, /id="disclaimer-dialog"/);
  assert.match(appHtmlSource, /www\.xihedun\.com/);
  assert.match(appSource, /showDisclaimer\(\)/);
  assert.match(appSource, /addEventListener\("cancel", \(event\) => event\.preventDefault\(\)\)/);
  assert.match(styleSource, /\.disclaimer-dialog::backdrop/);
  assert.match(styleSource, /\.side-publisher/);
  assert.doesNotMatch(serverSource, /script-src 'self' https:\/\/unpkg\.com/);
  assert.match(serverSource, /RISK_CONFIG_MASTER_KEY/);
  assert.match(serverSource, /RISK_TRUSTED_PROXY_IPS/);
  assert.match(serverSource, /absolute\.startsWith\(`\$\{publicRoot\}\$\{sep\}`\)/);
  assert.match(linuxInstallerSource, /Release bundle SHA-256 mismatch/);
  assert.match(linuxInstallerSource, /Release bundle signature verification failed/);
  assert.match(linuxInstallerSource, /bundle_file="\$2"; bundle_url=""/);
  assert.match(linuxInstallerSource, /Release bundle contains an unsafe path/);
  assert.match(linuxInstallerSource, /rollback\(\)/);
  assert.match(linuxInstallerSource, /systemctl is-active --quiet/);
  assert.match(linuxInstallerSource, /Rolled back and healthy/);
  assert.match(linuxInstallerSource, /--check-only/);
  assert.match(linuxBuilderSource, /cargo build --locked --release -p wdsf-engine -p wdsf-probe -p risk-agent -p risk-sdk/);
  assert.match(linuxBuilderSource, /PGR_RELEASE_SIGNING_KEY/);
  assert.match(linuxBuilderSource, /PGR_NODE_SHA256/);
  assert.match(linuxBuilderSource, /openssl dgst -sha256 -sign/);
  assert.doesNotMatch(linuxBuilderSource, /cp -a public docs deploy/);
  assert.match(linuxBuilderSource, /cp public\/app\.html public\/app\.js public\/home\.js public\/index\.html public\/styles\.css/);
  assert.doesNotMatch(linuxBuilderSource, /curl -fsSL %s\/%s \| sudo bash/);

  const anonymousDownload = await fetch(`${base}/api/integration/downloads/integration-guide`);
  assert.equal(anonymousDownload.status, 401);

  const integration = await fetch(`${base}/api/integration`, { headers });
  const integrationData = await integration.json();
  assert.equal(integration.status, 200);
  assert.equal(integrationData.contract.interfaceCount, 7);
  assert.equal(integrationData.contract.realtimeRuleCount, 17);
  assert.equal(integrationData.artifacts.length, 5);
  const artifactPaths = new Map([
    ["windows-sdk", join(projectRoot, "dist", "risk-sdk", "ponytail-risk-sdk-windows-x86_64.zip")],
    ["linux-sdk", join(projectRoot, "dist", "risk-sdk", "ponytail-risk-sdk-linux-x86_64.zip")],
    ["integration-guide", join(projectRoot, "docs", "GAME_PLUGIN_INTEGRATION_V1.md")],
    ["event-schema", join(projectRoot, "docs", "plugin-event-batch.v1.schema.json")],
    ["event-example", join(projectRoot, "docs", "plugin-event-batch.v1.example.json")],
  ]);
  for (const artifact of integrationData.artifacts) {
    const availableOnDisk = existsSync(artifactPaths.get(artifact.id));
    assert.equal(artifact.available, availableOnDisk);
    if (availableOnDisk) assert.match(artifact.sha256, /^[a-f0-9]{64}$/);
  }

  const createdKey = await fetch(`${base}/api/sdk-keys`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ name: "一区插件", tenantId: "tenant-1", serverId: "server-1" }),
  });
  const createdKeyData = await createdKey.json();
  assert.equal(createdKey.status, 201);
  assert.match(createdKeyData.secret, /^pgr_[A-Za-z0-9_-]{40,}$/);
  assert.equal(readFileSync(sdkKeysPath, "utf8").includes(createdKeyData.secret), false);

  const listedKeys = await fetch(`${base}/api/sdk-keys`, { headers });
  const listedKeyData = await listedKeys.json();
  assert.equal(listedKeyData.credentials.length, 1);
  assert.equal("secret" in listedKeyData.credentials[0], false);

  const wrongSdkKey = await fetch(`${base}/sdk/v1/events:batch`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: "Bearer pgr_this_key_is_definitely_wrong_123456789" },
    body: "{}",
  });
  assert.equal(wrongSdkKey.status, 401);

  const authorizedSdkKey = await fetch(`${base}/sdk/v1/events:batch`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${createdKeyData.secret}` },
    body: "{}",
  });
  assert.equal(authorizedSdkKey.status, 400);

  const rotatedKey = await fetch(`${base}/api/sdk-keys/${createdKeyData.credential.id}/rotate`, { method: "POST", headers });
  const rotatedKeyData = await rotatedKey.json();
  assert.equal(rotatedKey.status, 201);
  assert.notEqual(rotatedKeyData.secret, createdKeyData.secret);

  const oldSdkKey = await fetch(`${base}/sdk/v1/events:batch`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${createdKeyData.secret}` },
    body: "{}",
  });
  assert.equal(oldSdkKey.status, 401);

  const alertDetail = await fetch(`${base}/api/alerts/R-20260730-0081`, { headers });
  const alertDetailData = await alertDetail.json();
  assert.equal(alertDetail.status, 200);
  assert.equal(alertDetailData.alert.id, "R-20260730-0081");
  assert.equal(alertDetailData.player.id, "1003281");

  const watchedCase = await fetch(`${base}/api/alerts/R-20260730-0081/decision`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ decision: "watch", note: "保留观察并补充同设备交易证据" }),
  });
  assert.equal(watchedCase.status, 200);
  assert.equal((await watchedCase.json()).case.status, "watch");

  const invalidBan = await fetch(`${base}/api/alerts/R-20260730-0081/actions`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ credentialId: rotatedKeyData.credential.id, type: "account.ban", target: { actorId: "1003281" }, reason: "确定性证据已由人工复核", confirmation: "wrong" }),
  });
  assert.equal(invalidBan.status, 400);

  const queuedAction = await fetch(`${base}/api/alerts/R-20260730-0081/actions`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ credentialId: rotatedKeyData.credential.id, type: "account.suspend", target: { actorId: "1003281", durationMinutes: 60 }, reason: "确定性证据已由人工复核", confirmation: "1003281" }),
  });
  const queuedActionData = await queuedAction.json();
  assert.equal(queuedAction.status, 201);
  assert.equal(queuedActionData.action.status, "pending");
  assert.equal(readFileSync(caseActionsPath, "utf8").includes(rotatedKeyData.secret), false);

  const pulledActions = await fetch(`${base}/sdk/v1/actions:pull`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${rotatedKeyData.secret}` },
    body: JSON.stringify({ limit: 5 }),
  });
  const pulledActionsData = await pulledActions.json();
  assert.equal(pulledActions.status, 200);
  assert.equal(pulledActionsData.actions.length, 1);
  assert.equal(pulledActionsData.actions[0].id, queuedActionData.action.id);
  assert.equal(pulledActionsData.actions[0].tenantId, "tenant-1");
  assert.equal(pulledActionsData.actions[0].serverId, "server-1");

  const actionAck = await fetch(`${base}/sdk/v1/actions:ack`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${rotatedKeyData.secret}` },
    body: JSON.stringify({ actionId: queuedActionData.action.id, status: "applied", executionRef: "plugin-job-1", message: "执行成功" }),
  });
  assert.equal(actionAck.status, 200);
  assert.equal((await actionAck.json()).action.status, "applied");

  const appliedDetail = await fetch(`${base}/api/alerts/R-20260730-0081`, { headers });
  const appliedDetailData = await appliedDetail.json();
  assert.equal(appliedDetailData.case.status, "action_applied");
  assert.equal(appliedDetailData.actions[0].status, "applied");

  const guide = await fetch(`${base}/api/integration/downloads/integration-guide`, { headers });
  assert.equal(guide.status, 200);
  assert.match(guide.headers.get("content-disposition") || "", /GAME_PLUGIN_INTEGRATION_V1\.md/);
  assert.match(await guide.text(), /游戏插件实时风控对接规范 v1/);

  const player = await fetch(`${base}/api/player?q=1003281`, { headers });
  assert.equal((await player.json()).name, "北境长歌");

  const initialAi = await fetch(`${base}/api/settings/ai`, { headers });
  const initialAiData = await initialAi.json();
  assert.equal(initialAiData.enabled, false);
  assert.equal("apiKey" in initialAiData, false);

  const testedAi = await fetch(`${base}/api/settings/ai/test`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ provider: "groq", model: "qwen/qwen3.6-27b", apiKey: "self-check-ai-key" }),
  });
  assert.equal(testedAi.status, 200);

  const savedAi = await fetch(`${base}/api/settings/ai`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ enabled: true, provider: "groq", model: "qwen/qwen3.6-27b", apiKey: "self-check-ai-key" }),
  });
  assert.equal(savedAi.status, 200);
  assert.equal(readFileSync(aiConfigPath, "utf8").includes("self-check-ai-key"), false);

  let automaticReviews = [];
  for (let attempt = 0; attempt < 30; attempt += 1) {
    const response = await fetch(`${base}/api/ai/reviews`, { headers });
    automaticReviews = (await response.json()).reviews;
    if (automaticReviews.length) break;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  const agentReview = automaticReviews.find((review) => review.alertId === "alert-auto-1");
  assert.equal(agentReview?.advisoryOnly, true);
  assert.equal(automaticReviews.some((review) => review.alertId === "R-20260730-0081"), true);
  assert.equal(aiRequests.every((request) => request.authorization === "Bearer self-check-ai-key"), true);

  const playerAi = await fetch(`${base}/api/ai/player`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ q: "1003281" }),
  });
  const playerAiData = await playerAi.json();
  assert.equal(playerAi.status, 200);
  assert.equal(playerAiData.scope, "player");
  assert.equal(playerAiData.advisoryOnly, true);

  const asset = await fetch(`${base}/api/asset?q=ITEM-9F2A-771C`, { headers });
  assert.equal((await asset.json()).nodes.length, 4);
  const assetAi = await fetch(`${base}/api/ai/asset`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ q: "ITEM-9F2A-771C" }),
  });
  const assetAiData = await assetAi.json();
  assert.equal(assetAi.status, 200);
  assert.equal(assetAiData.scope, "asset");
  assert.equal(assetAiData.advisoryOnly, true);
  const invalidAiPlayer = await fetch(`${base}/api/ai/player`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ q: "" }),
  });
  assert.equal(invalidAiPlayer.status, 400);

  const allAiBodies = aiRequests.map((request) => request.body).join("\n");
  assert.equal(allAiBodies.includes("secret-player-name"), false);
  assert.equal(allAiBodies.includes("10.0.0.8"), false);
  assert.equal(allAiBodies.includes("北境长歌"), false);
  assert.equal(allAiBodies.includes("acc_88241"), false);
  assert.equal(allAiBodies.includes("ITEM-9F2A-771C"), false);
  assert.equal(allAiBodies.includes("北境长歌 / 1003281"), false);

  const assetsByPlayer = await fetch(`${base}/api/assets?q=${encodeURIComponent("北境长歌")}`, { headers });
  const assetsByPlayerData = await assetsByPlayer.json();
  assert.equal(assetsByPlayer.status, 200);
  assert.equal(assetsByPlayerData.results[0].id, "ITEM-9F2A-771C");
  const assetsByName = await fetch(`${base}/api/assets?q=${encodeURIComponent("雷极弧光")}`, { headers });
  assert.equal((await assetsByName.json()).results[0].owner.includes("山海一梦"), true);
  const invalidAssetQuery = await fetch(`${base}/api/assets?q=${"x".repeat(129)}`, { headers });
  assert.equal(invalidAssetQuery.status, 400);

  const databaseSettings = await fetch(`${base}/api/settings/database`, { headers });
  const initialSettings = await databaseSettings.json();
  assert.equal(databaseSettings.status, 200);
  assert.equal("password" in initialSettings, false);

  const invalidDatabase = await fetch(`${base}/api/settings/database`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ enabled: false, host: "127.0.0.1", port: 3306, user: "reader", password: "secret", mainDatabase: "bad-name", logDatabase: "logs" }),
  });
  assert.equal(invalidDatabase.status, 400);

  const savedDatabase = await fetch(`${base}/api/settings/database`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ enabled: false, host: "127.0.0.1", port: 3306, user: "reader", password: "secret", mainDatabase: "main_db", logDatabase: "log_db" }),
  });
  const savedSettings = await savedDatabase.json();
  assert.equal(savedDatabase.status, 200);
  assert.equal(savedSettings.config.passwordConfigured, true);
  const storedDatabaseConfig = JSON.parse(readFileSync(configPath, "utf8"));
  assert.equal(storedDatabaseConfig.version, 2);
  assert.equal(JSON.stringify(storedDatabaseConfig).includes("secret"), false);

  const rule = await fetch(`${base}/api/rules`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ id: "night-activity", enabled: true }),
  });
  assert.equal(rule.status, 405);
  assert.equal(rule.headers.get("allow"), "GET");
  const ruleCatalog = await fetch(`${base}/api/rules`, { headers });
  assert.equal((await ruleCatalog.json()).every((item) => item.mutable === false), true);

  const rejectedCaps = await fetch(`${base}/api/settings/gameplay-caps`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ caps: [{ action: "bad'action", label: "错误", dailyLimit: 1, burst10mLimit: 1, enabled: true }] }),
  });
  assert.equal(rejectedCaps.status, 400);

  const savedCaps = await fetch(`${base}/api/settings/gameplay-caps`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ caps: [{ action: "huilcbjl", label: "回合奖励", dailyLimit: 100, burst10mLimit: 10, enabled: true }] }),
  });
  assert.equal(savedCaps.status, 200);
  const savedCapsData = await savedCaps.json();
  assert.equal(savedCapsData.caps[0].dailyLimit, 100);
  assert.equal(savedCapsData.created, true);
  assert.match(savedCapsData.currentVersion, /^caps_[a-f0-9]{16}$/);
  assert.equal(savedCapsData.versions.length, 2);
  const storedCaps = JSON.parse(readFileSync(gameplayCapsPath, "utf8"));
  assert.equal(storedCaps.schemaVersion, 1);
  assert.equal(storedCaps.currentVersion, savedCapsData.currentVersion);
  assert.equal(storedCaps.versions[0].caps[0].dailyLimit, 80);

  const repeatedCaps = await fetch(`${base}/api/settings/gameplay-caps`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ caps: savedCapsData.caps }),
  });
  const repeatedCapsData = await repeatedCaps.json();
  assert.equal(repeatedCapsData.created, false);
  assert.equal(repeatedCapsData.versions.length, 2);

  const listedCaps = await fetch(`${base}/api/settings/gameplay-caps`, { headers });
  const listedCapsData = await listedCaps.json();
  assert.deepEqual(listedCapsData.caps, [{ action: "huilcbjl", label: "回合奖励", dailyLimit: 100, burst10mLimit: 10, enabled: true }]);
  assert.equal(listedCapsData.versions.length, 2);

  const unavailableReplay = await fetch(`${base}/api/settings/gameplay-caps/replay`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ caps: listedCapsData.caps }),
  });
  assert.equal(unavailableReplay.status, 409);

  const gameplayCatalog = await fetch(`${base}/api/settings/gameplay-catalog`, { headers });
  const gameplayCatalogData = await gameplayCatalog.json();
  assert.equal(gameplayCatalog.status, 200);
  assert.equal(gameplayCatalogData.connected, false);
  assert.deepEqual(gameplayCatalogData.actions, []);

  console.log("web self_check ok");
} finally {
  child.kill();
  await new Promise((resolve) => aiMock.close(resolve));
  await new Promise((resolve) => agentMock.close(resolve));
  if (existsSync(configPath)) unlinkSync(configPath);
  if (existsSync(aiConfigPath)) unlinkSync(aiConfigPath);
  if (existsSync(aiReviewsPath)) unlinkSync(aiReviewsPath);
  if (existsSync(sdkKeysPath)) unlinkSync(sdkKeysPath);
  if (existsSync(caseActionsPath)) unlinkSync(caseActionsPath);
  if (existsSync(gameplayCapsPath)) unlinkSync(gameplayCapsPath);
}

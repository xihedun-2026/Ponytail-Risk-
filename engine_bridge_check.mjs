// 验证 Node 控制层确实在调用 Rust 引擎二进制，且引擎的三种结果被正确翻译：
//   正常输出 -> 200 直通
//   {"error":...} + 退出码 2 -> 404
//   异常退出 -> 503
// 用一个替身脚本顶替真引擎，因此不需要数据库也能跑。
//
//   node engine_bridge_check.mjs
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// import.meta.dirname 要 Node 20.11+；这样算等价且不挑版本。
const projectRoot = fileURLToPath(new URL(".", import.meta.url));

const port = Number(process.env.BRIDGE_CHECK_PORT || 4179);
const base = `http://127.0.0.1:${port}`;
const workDir = mkdtempSync(join(tmpdir(), "wdsf-bridge-"));
const argvLog = join(workDir, "argv.log");
const fakeEngine = join(workDir, "fake-engine.cjs");

writeFileSync(
  fakeEngine,
  `const { appendFileSync } = require("node:fs");
const { basename } = require("node:path");
const operation = basename(process.argv[1] || "");
if (["dashboard", "player", "asset-search", "alerts"].includes(operation)) {
  appendFileSync(${JSON.stringify(argvLog)}, \`ARGS \${[operation, ...process.argv.slice(2)].join(" ")}\\n\`);
  appendFileSync(${JSON.stringify(argvLog)}, \`ENV \${process.env.WDSF_HOST}|\${process.env.WDSF_MDB}|\${process.env.WDSF_DB_USER}\\n\`);
  if (operation === "player") {
    console.log('{"error":"未找到匹配玩家"}');
    process.exit(2);
  }
  if (operation === "dashboard") {
    console.log('{"headline":"来自 Rust 引擎"}');
    process.exit(0);
  }
  if (operation === "asset-search") {
    console.log('{"query":"北境长歌","truncated":false,"results":[{"id":":A1:","name":"测试道具"}]}');
    process.exit(0);
  }
  console.error("boom");
  process.exit(1);
}
`,
  { mode: 0o600 },
);

const child = spawn(process.execPath, ["server.mjs"], {
  cwd: projectRoot,
  env: {
    ...process.env,
    RISK_PORT: String(port),
    RISK_PORTAL_KEY: "bridge-check-key",
    RISK_DB_CONFIG_PATH: join(workDir, "cfg.json"),
    WDSF_LIVE: "1",
    WDSF_HOST: "10.9.8.7",
    WDSF_DB_USER: "reader",
    WDSF_DB_PASSWORD: "pw",
    WDSF_MDB: "main_db",
    WDSF_LDB: "log_db",
    WDSF_ENGINE: process.execPath,
    NODE_OPTIONS: `${process.env.NODE_OPTIONS || ""} --require=${fakeEngine}`.trim(),
    // 拉长采集间隔，避免后台采集干扰断言。
    RISK_COLLECT_INTERVAL_MS: "600000",
  },
  stdio: ["ignore", "pipe", "pipe"],
  windowsHide: true,
});

async function waitForServer() {
  for (let attempt = 0; attempt < 60; attempt += 1) {
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

  const login = await fetch(`${base}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key: "bridge-check-key" }),
  });
  assert.equal(login.status, 200);
  const headers = { Cookie: login.headers.get("set-cookie").split(";")[0] };

  const dashboard = await fetch(`${base}/api/dashboard`, { headers });
  assert.equal(dashboard.status, 200);
  assert.equal((await dashboard.json()).headline, "来自 Rust 引擎");

  const player = await fetch(`${base}/api/player?q=nobody`, { headers });
  assert.equal(player.status, 404, `查不到玩家应返回 404，实际 ${player.status}`);
  assert.equal((await player.json()).error, "未找到匹配玩家");

  const assets = await fetch(`${base}/api/assets?q=${encodeURIComponent("北境长歌")}`, { headers });
  assert.equal(assets.status, 200);
  assert.equal((await assets.json()).results[0].id, ":A1:");

  const alerts = await fetch(`${base}/api/alerts`, { headers });
  assert.equal(alerts.status, 503, `引擎异常应返回 503，实际 ${alerts.status}`);

  const log = readFileSync(argvLog, "utf8");
  assert.ok(log.includes("ARGS dashboard"), "未看到 dashboard 调用");
  assert.ok(log.includes("ARGS player nobody"), "query 未作为位置参数传入");
  assert.ok(log.includes("ARGS asset-search 北境长歌"), "资产发现条件未传入 Rust 引擎");
  assert.ok(log.includes("ENV 10.9.8.7|main_db|reader"), `数据库环境未正确注入：${log}`);
  assert.ok(!log.includes(".py"), "数据层仍在调用 Python 脚本");

  console.log("engine bridge check ok");
} finally {
  child.kill();
}

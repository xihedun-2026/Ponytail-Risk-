const loginScreen = document.querySelector("#login-screen");
const consoleShell = document.querySelector("#console-shell");
const loginForm = document.querySelector("#login-form");
const loginError = document.querySelector("#login-error");
const content = document.querySelector("#console-content");
const breadcrumb = document.querySelector("#breadcrumb");
const disclaimerDialog = document.querySelector("#disclaimer-dialog");
const disclaimerAccept = document.querySelector("#disclaimer-accept");
let currentView = "overview";
let viewContext = {};
let pendingSdkCredential = null;

const viewNames = {
  overview: "总览",
  player: "玩家分析",
  asset: "资产溯源",
  alerts: "告警研判",
  rules: "规则与设置",
  integration: "插件接入",
};

const escapeHtml = (value) =>
  String(value).replace(/[&<>'"]/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[character]);
const clampPercent = (value) => Math.max(0, Math.min(100, Number(value) || 0));
const aiRiskNames = { normal: "正常", watch: "观察", high: "高风险" };

function aiAnalysisMarkup(analysis) {
  return `<article class="entity-ai-review">
    <div class="ai-review-head"><div><span>${escapeHtml(analysis.provider)} · ${escapeHtml(analysis.model)}</span><strong>${escapeHtml(aiRiskNames[analysis.riskLevel] || analysis.riskLevel)} · ${escapeHtml(analysis.confidence)}%</strong></div><time>${escapeHtml(new Date(analysis.generatedAt).toLocaleString("zh-CN", { hour12: false }))}</time></div>
    <p>${escapeHtml(analysis.summary)}</p>
    <div class="ai-findings">${analysis.findings.map((finding) => `<span class="${escapeHtml(finding.severity)}"><b>${escapeHtml(finding.title)}</b>${escapeHtml(finding.evidence)}</span>`).join("")}</div>
    ${analysis.suggestedActions.length ? `<ul>${analysis.suggestedActions.map((action) => `<li>${escapeHtml(action)}</li>`).join("")}</ul>` : ""}
    <small>AI 结果仅作辅助证据，不会自动执行处罚</small>
  </article>`;
}

function entityAiPanel(kind, query) {
  const label = kind === "player" ? "玩家行为 AI 研判" : "资产链路 AI 研判";
  const description = kind === "player" ? "结合当前评分、群体偏离与关键行为时间线进行复核。" : "检查生成、持有、转移和当前状态是否闭合。";
  return `<section class="entity-ai-panel">
    <div class="panel-title"><div><span>AI 辅助研判</span><h2>${label}</h2></div><button class="icon-text-button" type="button" data-ai-request="${kind}" data-ai-query="${escapeHtml(query)}"><i data-lucide="sparkles"></i><span>开始 AI 研判</span></button></div>
    <div class="entity-ai-result" data-ai-result="${kind}"><p class="ai-empty">${description}</p></div>
  </section>`;
}

function bindEntityAi(kind, query) {
  const button = document.querySelector(`[data-ai-request="${kind}"]`);
  const target = document.querySelector(`[data-ai-result="${kind}"]`);
  if (!button || !target) return;
  button.addEventListener("click", async () => {
    button.disabled = true;
    target.innerHTML = '<div class="loading-state">正在整理确定性证据并请求模型…</div>';
    try {
      target.innerHTML = aiAnalysisMarkup(await api(`/api/ai/${kind}`, { method: "POST", body: JSON.stringify({ q: query }) }));
    } catch (error) {
      target.innerHTML = `<p class="empty-state">${escapeHtml(error.message)}</p>`;
    } finally {
      button.disabled = false;
    }
  });
}

function progressRail(label, value, tone = "green", extraClass = "") {
  const percent = clampPercent(value);
  const safeTone = ["green", "gold", "coral", "dark", "blue"].includes(tone) ? tone : "green";
  const safeClass = String(extraClass).replace(/[^A-Za-z0-9_-]/g, "");
  // ponytail: CSP 禁止动态内联宽度；1% 宽度类的最大视觉误差为 0.5%。
  const visualPercent = Math.round(percent);
  return `<div class="progress-rail ${safeClass}" role="progressbar" aria-label="${escapeHtml(label)}" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${percent.toFixed(1)}"><span class="progress-value ${safeTone} progress-width-${visualPercent}"></span></div>`;
}

function activateProgressAnimations() {
  const sections = [...content.querySelectorAll(".health-summary, .overview-grid")];
  if (!("IntersectionObserver" in window)) {
    sections.forEach((section) => section.classList.add("progress-animate"));
    return;
  }
  const observer = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) return;
      entry.target.classList.add("progress-animate");
      observer.unobserve(entry.target);
    });
  }, { threshold: 0.18, rootMargin: "0px 0px -8% 0px" });
  sections.forEach((section) => observer.observe(section));
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { "Content-Type": "application/json", ...(options.headers || {}) },
  });
  if (response.status === 401) {
    showLogin();
    throw new Error("登录已失效");
  }
  const result = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(result.error || "请求失败");
  return result;
}

function showLogin() {
  loginScreen.classList.remove("hidden");
  consoleShell.classList.add("hidden");
}

function showConsole() {
  loginScreen.classList.add("hidden");
  consoleShell.classList.remove("hidden");
}

function showDisclaimer() {
  if (disclaimerDialog.open) return;
  if (typeof disclaimerDialog.showModal === "function") disclaimerDialog.showModal();
  else disclaimerDialog.setAttribute("open", "");
}

function activateNav(view) {
  document.querySelectorAll("[data-view]").forEach((button) => button.classList.toggle("active", button.dataset.view === view));
  breadcrumb.textContent = `风控中心 / ${viewNames[view]}`;
}

function severityClass(value) {
  return value === "严重" ? "danger" : value === "高" ? "warning" : "neutral";
}

function alertRows(alerts) {
  return alerts
    .map(
      (alert) => `
      <tr class="clickable-row" data-alert-id="${escapeHtml(alert.id || alert.alert_id)}" tabindex="0">
        <td><button class="row-link" type="button" data-alert-id="${escapeHtml(alert.id || alert.alert_id)}"><strong>${escapeHtml(alert.id || alert.alert_id)}</strong><small>${escapeHtml(alert.time || alert.occurred_at || "-")}</small></button></td>
        <td>${escapeHtml(alert.player)}</td>
        <td>${escapeHtml(alert.rule)}</td>
        <td><span class="badge ${severityClass(alert.severity)}">${escapeHtml(alert.severity)}</span></td>
        <td><strong>${escapeHtml(alert.score)}</strong></td>
        <td>${escapeHtml(alert.state)}</td>
      </tr>`,
    )
    .join("");
}

async function renderOverview() {
  const data = await api("/api/dashboard");
  const coverageText = String(data.health.coverage || "-");
  const coverageMatch = /([\d.]+)\s*\/\s*([\d.]+)/.exec(coverageText);
  const coveragePercent = clampPercent(coverageMatch
    ? (Number(coverageMatch[1]) / Math.max(Number(coverageMatch[2]), 1)) * 100
    : Number.parseFloat(coverageText));
  const latencyMs = Math.max(0, Number.parseFloat(data.health.latency) || 0);
  const backlog = Math.max(0, Number(data.health.backlog) || 0);
  // ponytail: 500 ms 和 50 条只作为总览水位线；需要不同容量时再下沉为租户配置。
  const latencyPercent = clampPercent(latencyMs / 5);
  const backlogPercent = clampPercent(backlog * 2);
  const elevatedRiskPercent = clampPercent(data.riskBands.filter(([, , tone]) => tone !== "green").reduce((sum, [, value]) => sum + Number(value), 0));
  const updatedAt = new Date(data.updatedAt);
  const updatedLabel = Number.isNaN(updatedAt.getTime()) ? "-" : updatedAt.toLocaleTimeString("zh-CN", { hour12: false });
  content.innerHTML = `
    <section class="status-hero">
      <div>
        <span class="live-status"><i></i>${escapeHtml(data.health.status)}</span>
        <h1>${escapeHtml(data.headline || "资产流水稳定，风险正在收敛")}</h1>
        <p>${escapeHtml(data.description || "权威事件已进入风控分析流程。")}</p>
        <div class="hero-actions"><button class="primary-button" data-go="alerts">查看告警 <span>→</span></button><button class="dark-ghost" data-go="player">分析玩家</button></div>
      </div>
      <div class="radar-mark"><span>AI</span><i></i><i></i><i></i></div>
    </section>

    <section class="compliance-strip">
      <div class="strip-icon">!</div><div><span>处置原则 · 强证据优先</span><strong>模型分数不会直接触发永久封号</strong><p>确定性规则可实时阻断；统计与图谱异常先暂存资产，再由案件证据复核。</p></div>
      <div class="rule-pills"><span>账本守恒</span><span>唯一资产</span><span>配置一致</span><span>人工复核</span></div>
    </section>

    <section class="metric-grid">
      ${data.metrics
        .map(
          ([label, value, delta], index) => `<button class="metric-card" type="button" data-go="${["alerts", "player", "asset", "alerts"][index]}"><span class="metric-icon tone-${index + 1}">${["↗", "◎", "⇅", "▤"][index]}</span><p>${escapeHtml(label)}</p><strong>${escapeHtml(value)}</strong><small>${escapeHtml(delta)}</small><span class="metric-open">查看 →</span></button>`,
        )
        .join("")}
    </section>

    <section class="health-summary">
      <article class="health-usage">
        <div class="health-title">
          <div><span>运行水位</span><h2>实时检测链路</h2></div>
          <div class="health-donut"><svg viewBox="0 0 44 44" aria-hidden="true"><circle class="health-donut-track" cx="22" cy="22" r="18"></circle><circle class="health-donut-value" cx="22" cy="22" r="18" pathLength="100" stroke-dasharray="${backlogPercent} 100"></circle></svg><strong>${backlogPercent.toFixed(0)}%</strong><small>队列占用</small></div>
        </div>
        <div class="health-row"><div><span>数据表覆盖</span><strong>${escapeHtml(coverageText)}</strong></div>${progressRail("数据表覆盖", coveragePercent, "green", "health-progress")}</div>
        <div class="health-row"><div><span>待研判队列 <strong>${backlogPercent.toFixed(0)}%</strong></span><span>${backlog} / 50 条</span></div>${progressRail("待研判队列占用", backlogPercent, backlogPercent >= 70 ? "coral" : "gold", "health-progress")}</div>
        <p class="health-note"><strong>${backlogPercent >= 70 ? "队列接近观察线" : "当前处理余量充足"}</strong><span>达到 50 条进入积压预警，不会丢失已经接收的事件。</span></p>
        <p class="health-note warm"><span>接近队列上限时先处理确定性规则与高价值资产，统计异常会保留证据并延后研判。</span></p>
        <p class="health-footnote">当前租户共享数据库覆盖、Rust 引擎时延与告警研判队列水位。</p>
      </article>
      <article class="health-service">
        <div class="health-service-head"><span>服务信息</span><h2>${data.sourceMode === "live" ? "真实数据模式" : "演示数据模式"}</h2><span class="source-badge">${data.sourceMode === "live" ? "RUST ENGINE" : "DEMO"}</span></div>
        <dl class="health-meta"><div><dt>检测状态</dt><dd>${escapeHtml(data.health.status)}</dd></div><div><dt>最近刷新</dt><dd>${escapeHtml(updatedLabel)}</dd></div><div><dt>覆盖范围</dt><dd>${escapeHtml(coverageText)}</dd></div><div><dt>查询耗时</dt><dd>${escapeHtml(data.health.latency)}</dd></div></dl>
        <span class="service-rail-title">检测资源</span>
        <div class="service-rails">
          <div><span>数据库</span><strong>${coveragePercent.toFixed(0)}%</strong>${progressRail("数据库覆盖率", coveragePercent, "green", "service-progress")}</div>
          <div><span>Rust 引擎</span><strong>${escapeHtml(data.health.latency)}</strong>${progressRail("Rust 引擎时延水位", latencyPercent, "blue", "service-progress")}</div>
          <div><span>告警队列</span><strong>${backlog} 条</strong>${progressRail("告警队列水位", backlogPercent, backlogPercent >= 70 ? "coral" : "gold", "service-progress")}</div>
        </div>
        <button class="health-link" type="button" data-go="integration">查看接入状态 →</button>
      </article>
    </section>

    <section class="overview-grid">
      <article class="chart-panel">
        <div class="panel-title"><div><span>风险趋势</span><h2>过去 12 小时</h2></div><span class="refresh-time">实时刷新</span></div>
        <div class="bar-chart">${data.distribution.map((height, index) => `<progress class="bar-column${index % 6 === 5 ? " coral" : index % 4 === 1 ? " soft" : ""}" aria-label="第 ${index + 1} 个时段风险事件强度" max="100" value="${clampPercent(height)}">${clampPercent(height)}%</progress>`).join("")}</div>
        <div class="chart-labels"><span>03:00</span><span>06:00</span><span>09:00</span><span>12:00</span><span>现在</span></div>
      </article>
      <article class="risk-panel">
        <div class="panel-title"><div><span>风险分布</span><h2>${escapeHtml(data.scope || "全部在线玩家")}</h2></div><div class="risk-ring" role="img" aria-label="异常角色占比 ${elevatedRiskPercent.toFixed(1)}%"><svg viewBox="0 0 44 44" aria-hidden="true"><circle class="risk-ring-track" cx="22" cy="22" r="18"></circle><circle class="risk-ring-value" cx="22" cy="22" r="18" pathLength="100" stroke-dasharray="${elevatedRiskPercent} 100"></circle></svg><strong>${elevatedRiskPercent.toFixed(1)}</strong><small>%</small></div></div>
        <div class="risk-bands">${data.riskBands.map(([label, value, tone]) => `<div><span>${escapeHtml(label)}</span>${progressRail(`${label}占比`, value, tone, "risk-progress")}<strong>${escapeHtml(value)}%</strong></div>`).join("")}</div>
      </article>
    </section>

    <section class="table-panel">
      <div class="panel-title"><div><span>最新告警</span><h2>等待研判的高价值事件</h2></div><button class="ghost-button" data-go="alerts">查看全部</button></div>
      <div class="table-wrap"><table><thead><tr><th>事件</th><th>玩家</th><th>命中规则</th><th>等级</th><th>评分</th><th>状态</th></tr></thead><tbody>${alertRows(data.alerts)}</tbody></table></div>
    </section>`;
  activateProgressAnimations();
  bindGoButtons();
  bindAlertRows();
}

function playerResult(player) {
  return `
    <section class="player-hero">
      <div><span class="badge ${player.statusTone}">${escapeHtml(player.status)}</span><h2>${escapeHtml(player.name)}</h2><p>${escapeHtml(player.server)} · 角色 ${escapeHtml(player.id)} · ${escapeHtml(player.level)} 级</p><button class="icon-text-button player-assets-button" type="button" data-find-assets="${escapeHtml(player.id)}"><i data-lucide="package-search"></i><span>查看该玩家资产</span></button></div>
      <div class="score-dial"><strong>${escapeHtml(player.score)}</strong><span>风险评分</span></div>
    </section>
    <section class="tag-line">${player.tags.map((tag) => `<span>${escapeHtml(tag)}</span>`).join("")}</section>
    <p class="player-summary">${escapeHtml(player.summary)}</p>
    ${entityAiPanel("player", player.id)}
    <section class="metric-grid compact">${player.metrics.map(([label, value]) => `<article><p>${escapeHtml(label)}</p><strong>${escapeHtml(value)}</strong></article>`).join("")}</section>
    <section class="table-panel"><div class="panel-title"><div><span>关键行为</span><h2>资产与交易时间线</h2></div></div>
    <div class="table-wrap"><table><thead><tr><th>时间</th><th>行为</th><th>资产变化</th><th>研判</th></tr></thead><tbody>${player.timeline.map((row) => `<tr>${row.map((cell, index) => `<td${index === 3 ? ' class="evidence"' : ""}>${escapeHtml(cell)}</td>`).join("")}</tr>`).join("")}</tbody></table></div></section>`;
}

function bindPlayerActions(player) {
  document.querySelector("[data-find-assets]")?.addEventListener("click", (event) => setView("asset", { search: event.currentTarget.dataset.findAssets }));
  bindEntityAi("player", player.id);
}

async function renderPlayer() {
  const player = await api(`/api/player${viewContext.q ? `?q=${encodeURIComponent(viewContext.q)}` : ""}`);
  content.innerHTML = `
    <section class="page-heading"><span>玩家分析</span><h1>当前玩家的数据是否符合正常路径？</h1><p>输入角色 ID、角色名或账号，查看风险评分、同群体偏离与关键事件。</p></section>
    <form class="search-strip" id="player-search"><input name="q" value="${escapeHtml(player.id)}" placeholder="角色 ID / 角色名 / 账号" /><button class="primary-button" type="submit">开始分析</button></form>
    <div id="player-result">${playerResult(player)}</div>`;
  bindPlayerActions(player);
  document.querySelector("#player-search").addEventListener("submit", async (event) => {
    event.preventDefault();
    const query = new FormData(event.currentTarget).get("q").trim();
    const target = document.querySelector("#player-result");
    try {
      const nextPlayer = await api(`/api/player?q=${encodeURIComponent(query)}`);
      target.innerHTML = playerResult(nextPlayer);
      bindPlayerActions(nextPlayer);
      window.lucide?.createIcons();
    } catch (error) {
      target.innerHTML = `<p class="empty-state">${escapeHtml(error.message)}</p>`;
    }
  });
}

function assetCandidates(search) {
  const rows = search.results.map((item) => `<tr>
    <td><span class="badge neutral">${escapeHtml(item.kind)}</span></td>
    <td><strong>${escapeHtml(item.name)}</strong><small>${escapeHtml(item.id)}</small></td>
    <td>${escapeHtml(item.owner)}</td>
    <td>${escapeHtml(item.quantity)}</td>
    <td>${escapeHtml(item.updatedAt)}</td>
    <td><button class="icon-text-button" type="button" data-trace-asset="${escapeHtml(item.id)}"><i data-lucide="route"></i><span>回放链路</span></button></td>
  </tr>`).join("");
  return `<section class="asset-discovery">
    <div class="panel-title"><div><span>${search.query ? "查找结果" : "最近更新"}</span><h2>${search.results.length} 项可溯源资产</h2></div>${search.truncated ? "<small>仅显示前 50 项，请缩小查询范围</small>" : ""}</div>
    ${rows ? `<div class="table-wrap"><table><thead><tr><th>类型</th><th>资产</th><th>当前持有</th><th>数量</th><th>最近更新</th><th></th></tr></thead><tbody>${rows}</tbody></table></div>` : '<p class="empty-state">没有找到当前资产。可换用角色 ID、角色名、账号或道具名。</p>'}
  </section>`;
}

function assetResult(asset, relatedAlert = null) {
  const ownerId = String(asset.owner || "").match(/(?:\/\s*)?([A-Za-z0-9_.-]{4,})\s*$/)?.[1] || "";
  return `
    <section class="asset-summary">
      <div><span class="badge ${asset.risk > 70 ? "danger" : "safe"}">${escapeHtml(asset.state)}</span><h2>${escapeHtml(asset.name)} <small>${escapeHtml(asset.id)}</small></h2><p>当前持有：${escapeHtml(asset.owner)} · 数量 ${escapeHtml(asset.quantity)}</p></div>
      <div><span>来源</span><strong>${escapeHtml(asset.source)}</strong><span>风险评分</span><strong>${escapeHtml(asset.risk)}</strong></div>
    </section>
    <section class="asset-actions" aria-label="资产操作">
      <div><strong>下一步操作</strong><span>先核对持有人与完整证据，再提交区服插件执行。</span></div>
      ${ownerId ? `<button class="ghost-button" type="button" data-owner-id="${escapeHtml(ownerId)}">查看持有人</button>` : ""}
      ${relatedAlert ? `<button class="primary-button" type="button" data-action-alert="${escapeHtml(relatedAlert.id || relatedAlert.alert_id)}" data-asset-id="${escapeHtml(asset.id)}">进入案件处置</button>` : `<span class="action-unavailable">暂无关联告警，不能直接处置</span>`}
    </section>
    ${entityAiPanel("asset", asset.id)}
    <section class="trace-panel">
      <div class="panel-title"><div><span>完整路径</span><h2>生成 → 持有 → 转移 → 当前状态</h2></div><button class="ghost-button" id="export-asset" type="button">导出证据</button></div>
      <div class="trace-timeline">${asset.nodes.map(([time, action, owner, note], index) => `<article class="${index === asset.nodes.length - 1 ? "current" : ""}"><i></i><time>${escapeHtml(time)}</time><div><strong>${escapeHtml(action)}</strong><span>${escapeHtml(owner)}</span><p>${escapeHtml(note)}</p></div></article>`).join("")}</div>
    </section>`;
}

async function renderAsset() {
  const searchQuery = viewContext.search || "";
  const [asset, alerts, discovered] = await Promise.all([
    api(`/api/asset${viewContext.q ? `?q=${encodeURIComponent(viewContext.q)}` : ""}`),
    api("/api/alerts"),
    api(`/api/assets${searchQuery ? `?q=${encodeURIComponent(searchQuery)}` : ""}`),
  ]);
  const relatedFor = (item) => {
    const ownerId = String(item.owner || "").match(/(?:\/\s*)?([A-Za-z0-9_.-]{4,})\s*$/)?.[1] || "";
    const ownerName = String(item.owner || "").split("/")[0].trim();
    return alerts.find((alert) => String(alert.actor_id || "") === ownerId || alert.player === ownerName) || null;
  };
  content.innerHTML = `
    <section class="page-heading"><span>资产溯源</span><h1>先找到资产，再看它从哪里来</h1><p>输入客户已知的角色、账号或道具名；从候选资产中点击序列号，即可回放完整生成、交易与消耗链路。</p></section>
    <form class="search-strip" id="asset-search"><input name="q" value="${escapeHtml(searchQuery)}" placeholder="角色 ID / 角色名 / 账号 / 道具名 / 资产序列号" /><button class="primary-button" type="submit">查找资产</button></form>
    <div id="asset-candidates">${assetCandidates(discovered)}</div>
    <div id="asset-result">${assetResult(asset, relatedFor(asset))}</div>`;
  bindAssetActions(asset);
  const bindCandidates = () => document.querySelectorAll("[data-trace-asset]").forEach((button) => button.addEventListener("click", async () => {
    const target = document.querySelector("#asset-result");
    target.innerHTML = '<div class="loading-state">正在回放资产链路…</div>';
    try {
      const nextAsset = await api(`/api/asset?q=${encodeURIComponent(button.dataset.traceAsset)}`);
      target.innerHTML = assetResult(nextAsset, relatedFor(nextAsset));
      bindAssetActions(nextAsset);
      window.lucide?.createIcons();
    } catch (error) {
      target.innerHTML = `<p class="empty-state">${escapeHtml(error.message)}</p>`;
    }
  }));
  bindCandidates();
  document.querySelector("#asset-search").addEventListener("submit", async (event) => {
    event.preventDefault();
    const query = new FormData(event.currentTarget).get("q").trim();
    const target = document.querySelector("#asset-candidates");
    target.innerHTML = '<div class="loading-state">正在查找当前资产…</div>';
    try {
      target.innerHTML = assetCandidates(await api(`/api/assets?q=${encodeURIComponent(query)}`));
      bindCandidates();
      window.lucide?.createIcons();
    } catch (error) {
      target.innerHTML = `<p class="empty-state">${escapeHtml(error.message)}</p>`;
    }
  });
}

function bindAssetActions(asset) {
  bindAssetExport(asset);
  bindEntityAi("asset", asset.id);
  document.querySelector("[data-owner-id]")?.addEventListener("click", (event) => setView("player", { q: event.currentTarget.dataset.ownerId }));
  document.querySelector("[data-action-alert]")?.addEventListener("click", (event) => setView("alerts", { alertId: event.currentTarget.dataset.actionAlert, actionType: "asset.freeze", assetId: event.currentTarget.dataset.assetId }));
}

function bindAssetExport(asset) {
  document.querySelector("#export-asset")?.addEventListener("click", () => {
    const blob = new Blob([JSON.stringify(asset, null, 2)], { type: "application/json;charset=utf-8" });
    const link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = `asset-${asset.id.replace(/[^A-Za-z0-9_-]/g, "")}.json`;
    link.click();
    URL.revokeObjectURL(link.href);
  });
}

const actionNames = {
  "asset.freeze": "冻结资产",
  "session.kick": "踢下线",
  "account.suspend": "临时封停",
  "account.ban": "永久封号",
  "currency.deduct": "扣除货币",
};

const actionStatuses = {
  pending: "等待插件领取",
  leased: "插件执行中",
  applied: "执行成功",
  failed: "执行失败",
  rejected: "插件拒绝",
};

const caseStatuses = { open: "待研判", watch: "观察中", dismiss: "已排除", escalate: "已升级", action_pending: "等待插件执行", action_applied: "处置已执行", action_failed: "处置失败", action_rejected: "插件拒绝" };

function caseDetailMarkup(detail) {
  const alert = detail.alert;
  const actorId = alert.actorId || detail.player?.id || "";
  const requestedType = viewContext.actionType || "account.suspend";
  const defaultAsset = viewContext.assetId || detail.assets?.[0]?.id || "";
  const credentialOptions = detail.credentials.map((item) => `<option value="${escapeHtml(item.id)}">${escapeHtml(item.name)} · ${escapeHtml(item.tenantId)}/${escapeHtml(item.serverId)}</option>`).join("");
  return `
    <section class="case-workbench" id="case-workbench">
      <div class="case-heading">
        <div><span>案件 ${escapeHtml(alert.id)}</span><h2>${escapeHtml(alert.rule || alert.rule_code || "风险事件")}</h2><p>${escapeHtml(alert.player || detail.player?.name || actorId || "未知玩家")} · 风险分 ${escapeHtml(alert.score ?? "-")} · ${escapeHtml(alert.severity || "-")}</p></div>
        <button class="icon-text-button" id="close-case" type="button" title="关闭案件详情"><span>返回队列</span></button>
      </div>
      <div class="case-layout">
        <div class="case-evidence">
          <section><span class="case-kicker">关键对象</span><h3>${escapeHtml(detail.player?.name || alert.player || actorId || "未知玩家")}</h3><p>角色 ${escapeHtml(actorId || "未提供")} ${detail.player?.account ? `· 账号 ${escapeHtml(detail.player.account)}` : ""}</p>${actorId ? `<button class="ghost-button" type="button" data-player-id="${escapeHtml(actorId)}">查看完整玩家行为</button>` : ""}</section>
          <section><span class="case-kicker">规则证据</span><pre><code>${escapeHtml(JSON.stringify(alert.evidence || { rule: alert.rule, score: alert.score, state: alert.state }, null, 2))}</code></pre></section>
          <section><span class="case-kicker">AI 辅助研判</span>${detail.aiReview ? `<h3>${escapeHtml(detail.aiReview.summary)}</h3><p>置信度 ${escapeHtml(detail.aiReview.confidence)}%，仅作辅助证据。</p>` : `<p>尚无 AI 结果。确定性规则和原始流水仍可人工研判。</p>`}</section>
          ${detail.assets?.length ? `<section><span class="case-kicker">关联资产</span><div class="related-assets">${detail.assets.map((asset) => `<button type="button" data-asset-id="${escapeHtml(asset.id)}"><strong>${escapeHtml(asset.name)}</strong><span>${escapeHtml(asset.id)} · 风险 ${escapeHtml(asset.risk)}</span></button>`).join("")}</div></section>` : ""}
        </div>
        <aside class="case-controls">
          <form id="decision-form">
            <span class="case-kicker">人工研判</span><h3>记录案件结论</h3>
            <label><span>决定</span><select name="decision"><option value="watch">继续观察</option><option value="dismiss">排除告警</option><option value="escalate">升级处置</option></select></label>
            <label><span>研判说明</span><textarea name="note" required maxlength="1000" placeholder="写清支持该决定的证据"></textarea></label>
            <button class="ghost-button" type="submit">保存研判</button><p class="form-status" role="status"></p>
          </form>
          <form id="action-form">
            <span class="case-kicker">区服处置</span><h3>提交插件执行</h3>
            <label><span>执行区服</span><select name="credentialId" required ${credentialOptions ? "" : "disabled"}>${credentialOptions || `<option>请先生成 SDK 凭据</option>`}</select></label>
            ${credentialOptions ? "" : `<button class="ghost-button" type="button" data-go="integration">去插件接入生成凭据</button>`}
            <label><span>操作</span><select name="type">${Object.entries(actionNames).map(([value, label]) => `<option value="${value}" ${value === requestedType ? "selected" : ""}>${label}</option>`).join("")}</select></label>
            <div class="action-target-fields">
              <label data-field="actor"><span>角色编号</span><input name="actorId" value="${escapeHtml(actorId)}" readonly /></label>
              <label data-field="asset"><span>资产编号</span><input name="assetId" value="${escapeHtml(defaultAsset)}" /></label>
              <label data-field="duration"><span>封停分钟</span><input name="durationMinutes" type="number" min="5" max="525600" value="60" /></label>
              <label data-field="currency"><span>货币类型</span><select name="currency"><option value="yuanbao">元宝</option><option value="gold">金币</option><option value="silver">银两</option></select></label>
              <label data-field="amount"><span>扣除数量</span><input name="amount" type="number" min="1" value="1" /></label>
            </div>
            <label><span>处置原因</span><textarea name="reason" required minlength="8" maxlength="1000" placeholder="至少 8 个字符，写清确定性证据"></textarea></label>
            <label><span>输入处置对象编号确认</span><input name="confirmation" required autocomplete="off" /></label>
            <label class="irreversible-check"><input name="acknowledgeIrreversible" type="checkbox" /><span>我已核对不可逆操作风险</span></label>
            <button class="primary-button" type="submit" ${credentialOptions ? "" : "disabled"}>提交执行命令</button>
            <p class="command-note">提交后状态为“等待插件领取”；只有游戏服插件回执才会显示执行成功。</p><p class="form-status" role="status"></p>
          </form>
        </aside>
      </div>
      <section class="action-history"><span class="case-kicker">处置记录</span><h3>${escapeHtml(caseStatuses[detail.case.status] || detail.case.status || "待研判")}</h3>${detail.actions.length ? `<div>${detail.actions.map((action) => `<article><strong>${escapeHtml(actionNames[action.type] || action.type)}</strong><span>${escapeHtml(actionStatuses[action.status] || action.status)} · ${escapeHtml(action.serverId)}</span><small>${escapeHtml(action.message || action.requestedAt)}</small></article>`).join("")}</div>` : `<p>尚未提交区服命令。</p>`}</section>
    </section>`;
}

function bindCaseWorkbench(detail) {
  bindGoButtons();
  document.querySelector("#close-case")?.addEventListener("click", () => setView("alerts"));
  document.querySelector("[data-player-id]")?.addEventListener("click", (event) => setView("player", { q: event.currentTarget.dataset.playerId }));
  document.querySelectorAll(".related-assets [data-asset-id]").forEach((button) => button.addEventListener("click", () => setView("asset", { q: button.dataset.assetId })));
  const actionForm = document.querySelector("#action-form");
  const syncActionFields = () => {
    if (!actionForm) return;
    const type = actionForm.elements.type.value;
    actionForm.querySelectorAll("[data-field]").forEach((field) => { field.hidden = true; });
    const fields = type === "asset.freeze" ? ["asset"] : type === "account.suspend" ? ["actor", "duration"] : type === "currency.deduct" ? ["actor", "currency", "amount"] : ["actor"];
    fields.forEach((name) => { const field = actionForm.querySelector(`[data-field="${name}"]`); if (field) field.hidden = false; });
    actionForm.querySelector(".irreversible-check").hidden = !["account.ban", "currency.deduct"].includes(type);
  };
  actionForm?.elements.type.addEventListener("change", syncActionFields);
  syncActionFields();
  document.querySelector("#decision-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const status = form.querySelector(".form-status");
    status.textContent = "正在保存…";
    try {
      await api(`/api/alerts/${encodeURIComponent(detail.alert.id)}/decision`, { method: "POST", body: JSON.stringify({ decision: form.elements.decision.value, note: form.elements.note.value }) });
      status.textContent = "研判已保存";
      status.className = "form-status success";
    } catch (error) {
      status.textContent = error.message;
      status.className = "form-status error";
    }
  });
  actionForm?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const type = form.elements.type.value;
    const target = type === "asset.freeze"
      ? { assetId: form.elements.assetId.value.trim() }
      : type === "account.suspend"
        ? { actorId: form.elements.actorId.value, durationMinutes: Number(form.elements.durationMinutes.value) }
        : type === "currency.deduct"
          ? { actorId: form.elements.actorId.value, currency: form.elements.currency.value, amount: Number(form.elements.amount.value) }
          : { actorId: form.elements.actorId.value };
    const status = form.querySelector(".form-status");
    status.textContent = "正在提交…";
    try {
      await api(`/api/alerts/${encodeURIComponent(detail.alert.id)}/actions`, { method: "POST", body: JSON.stringify({ credentialId: form.elements.credentialId.value, type, target, reason: form.elements.reason.value, confirmation: form.elements.confirmation.value, acknowledgeIrreversible: form.elements.acknowledgeIrreversible.checked }) });
      await setView("alerts", { alertId: detail.alert.id });
    } catch (error) {
      status.textContent = error.message;
      status.className = "form-status error";
    }
  });
}

async function renderAlerts(severity = "全部") {
  const [alerts, ai, detail] = await Promise.all([api(`/api/alerts?severity=${encodeURIComponent(severity)}`), api("/api/ai/reviews"), viewContext.alertId ? api(`/api/alerts/${encodeURIComponent(viewContext.alertId)}`) : null]);
  content.innerHTML = `
    <section class="page-heading dark-heading"><span>告警研判</span><h1>先看证据，再决定如何处置</h1><p>严重事件优先展示；每个案件均保留原始流水、命中规则和资产路径。</p>
      <div class="filter-row">${["全部", "严重", "高", "中"].map((item) => `<button class="filter-button ${item === severity ? "active" : ""}" data-severity="${item}" type="button">${item}</button>`).join("")}</div>
    </section>
    ${detail ? caseDetailMarkup(detail) : ""}
    <section class="ai-review-section">
      <div class="panel-title"><div><span>AI 自动研判</span><h2>${ai.enabled ? `${escapeHtml(ai.provider)} · ${escapeHtml(ai.model)}` : "尚未启用"}</h2></div><span class="ai-worker-state ${ai.enabled ? "active" : ""}"><i></i>${ai.running ? "正在处理" : ai.enabled ? "等待新告警" : "未运行"}</span></div>
      <div class="ai-review-list">
        ${ai.reviews.length ? ai.reviews.slice(0, 12).map((review) => `<article class="clickable-ai-review" data-alert-id="${escapeHtml(review.alertId)}" tabindex="0"><div class="ai-review-head"><div><span>${escapeHtml(review.alertId)}</span><strong>${escapeHtml(aiRiskNames[review.riskLevel] || review.riskLevel)} · ${escapeHtml(review.confidence)}%</strong></div><time>${escapeHtml(new Date(review.generatedAt).toLocaleString("zh-CN", { hour12: false }))}</time></div><p>${escapeHtml(review.summary)}</p><div class="ai-findings">${review.findings.map((finding) => `<span class="${escapeHtml(finding.severity)}"><b>${escapeHtml(finding.title)}</b>${escapeHtml(finding.evidence)}</span>`).join("")}</div>${review.suggestedActions.length ? `<ul>${review.suggestedActions.map((action) => `<li>${escapeHtml(action)}</li>`).join("")}</ul>` : ""}<small>点击进入案件 · 不触发永久处罚</small></article>`).join("") : `<p class="ai-empty">${ai.enabled ? "等待实时规则产生新告警" : "在规则与设置中启用 AI Provider 后自动运行"}</p>`}
      </div>
    </section>
    <section class="table-panel alerts-table"><div class="panel-title"><div><span>案件队列</span><h2>${escapeHtml(severity)}告警 · ${alerts.length} 条</h2></div></div>
      <div class="table-wrap"><table><thead><tr><th>事件</th><th>玩家</th><th>命中规则</th><th>等级</th><th>评分</th><th>状态</th></tr></thead><tbody>${alertRows(alerts)}</tbody></table></div>
    </section>`;
  document.querySelectorAll("[data-severity]").forEach((button) => button.addEventListener("click", () => renderAlerts(button.dataset.severity)));
  bindAlertRows();
  if (detail) bindCaseWorkbench(detail);
}

async function renderRules() {
  const catalogRequest = api("/api/settings/gameplay-catalog").catch((error) => ({ connected: false, actions: [], error: error.message }));
  const [rules, database, ai, gameplay, gameplayCatalog] = await Promise.all([api("/api/rules"), api("/api/settings/database"), api("/api/settings/ai"), api("/api/settings/gameplay-caps"), catalogRequest]);
  const versioningSupported = Array.isArray(gameplay.versions);
  let nextCapIndex = gameplay.caps.length;
  const capRow = (cap, index) => `<div class="gameplay-cap-row" data-cap-row>
    <label><span>数据库代码</span><input class="cap-action-code" name="action-${index}" data-cap-action required maxlength="64" pattern="[A-Za-z0-9_:-]+" value="${escapeHtml(cap.action || "")}" placeholder="仅高级接入时填写" ${cap.custom ? "" : "readonly"} /></label>
    <label><span>显示名称</span><input name="label-${index}" data-cap-label required maxlength="80" value="${escapeHtml(cap.label || "")}" placeholder="例如 回合奖励" /></label>
    <label><span>每角色单日上限</span><input name="daily-${index}" data-cap-daily required type="number" min="0" max="1000000" step="1" value="${escapeHtml(cap.dailyLimit ?? 0)}" /></label>
    <label><span>每角色 10 分钟上限</span><input name="burst-${index}" data-cap-burst required type="number" min="0" max="100000" step="1" value="${escapeHtml(cap.burst10mLimit ?? 0)}" /></label>
    <label class="cap-enabled"><span>检测</span><input name="enabled-${index}" data-cap-enabled type="checkbox" ${cap.enabled ? "checked" : ""} aria-label="启用该玩法上限" /></label>
    <button class="square-icon-button danger-button" type="button" data-cap-remove title="删除玩法" aria-label="删除玩法"><i data-lucide="trash-2"></i></button>
  </div>`;
  const catalogRows = gameplayCatalog.actions.map((item) => `<div class="gameplay-catalog-row">
    <div class="catalog-identity"><span class="badge ${item.confirmedGain ? "safe" : "neutral"}">${item.confirmedGain ? "已确认获得" : "需确认语义"}</span><h3>${escapeHtml(item.label)}</h3><code>${escapeHtml(item.action)}</code><p>${item.sampleReward ? `日志样例：${escapeHtml(item.sampleReward)}` : "日志未记录奖励名称"}</p></div>
    <dl><div><dt>30 天事件</dt><dd>${Number(item.events).toLocaleString("zh-CN")}</dd></div><div><dt>涉及角色</dt><dd>${Number(item.players).toLocaleString("zh-CN")}</dd></div><div><dt>单日峰值</dt><dd>${Number(item.dailyPeak).toLocaleString("zh-CN")}</dd></div><div><dt>10 分钟分桶</dt><dd>${Number(item.burst10mBucketPeak).toLocaleString("zh-CN")}</dd></div></dl>
    <button class="icon-text-button" type="button" data-catalog-add="${escapeHtml(item.action)}"><i data-lucide="plus"></i><span>加入检测</span></button>
  </div>`).join("");
  content.innerHTML = `
    <section class="page-heading"><span>规则与设置</span><h1>确定性规则优先，辅助信号只做加权</h1><p>内置规则目录只读；玩法经济规则按不可变版本发布，可先用最近 30 天数据回放。</p></section>
    <section class="policy-panel"><span>当前处置模式</span><h2>自动检测，人工处置</h2><p>规则与 AI 自动生成告警；封号、扣除等高风险动作必须人工确认，并等待区服插件回执。</p><div class="mode-pills"><span>自动告警：开启</span><span>人工确认：开启</span><span>插件回执：强制</span></div></section>
    <form class="database-panel" id="database-settings">
      <div class="panel-title"><div><span>实时数据源</span><h2>游戏数据库连接</h2></div><label class="connection-toggle"><input name="enabled" type="checkbox" ${database.enabled ? "checked" : ""} /><span>启用实时分析</span></label></div>
      <p class="database-note">保存前自动验证账号权限和核心表。密码不会返回浏览器，配置使用控制台卡密派生密钥加密保存。</p>
      <div class="database-fields">
        <label><span>服务器地址</span><input name="host" required maxlength="255" value="${escapeHtml(database.host)}" placeholder="192.168.1.10" /></label>
        <label><span>端口</span><input name="port" required type="number" min="1" max="65535" value="${escapeHtml(database.port)}" /></label>
        <label><span>数据库账号</span><input name="user" required maxlength="128" value="${escapeHtml(database.user)}" autocomplete="username" /></label>
        <label><span>数据库密码</span><input name="password" type="password" maxlength="512" autocomplete="new-password" placeholder="${database.passwordConfigured ? "已配置，留空保持不变" : "请输入只读账号密码"}" /></label>
        <label><span>角色数据库</span><input name="mainDatabase" required maxlength="64" value="${escapeHtml(database.mainDatabase)}" /></label>
        <label><span>日志数据库</span><input name="logDatabase" required maxlength="64" value="${escapeHtml(database.logDatabase)}" /></label>
      </div>
      <div class="database-actions"><button class="ghost-button" id="test-database" type="button">测试连接</button><button class="primary-button" type="submit">测试并保存</button><p id="database-status" role="status">${database.persisted ? "已加载加密配置" : "当前使用进程环境配置"}</p></div>
    </form>
    <form class="database-panel ai-settings-panel" id="ai-settings">
      <div class="panel-title"><div><span>AI 自动研判</span><h2>Provider 与模型</h2></div><label class="connection-toggle"><input name="enabled" type="checkbox" ${ai.enabled ? "checked" : ""} /><span>自动研判新告警</span></label></div>
      <p class="database-note">Worker 单并发处理 Rust 规则告警；云端只发送脱敏编号和数值证据，结果仅用于辅助复核。</p>
      <div class="ai-fields">
        <label><span>Provider</span><select name="provider"><option value="groq" ${ai.provider === "groq" ? "selected" : ""}>Groq 免费 API</option><option value="ollama" ${ai.provider === "ollama" ? "selected" : ""}>Ollama 本机模型</option></select></label>
        <label><span>模型</span><input name="model" required maxlength="128" value="${escapeHtml(ai.model)}" /></label>
        <label><span>API Key</span><input name="apiKey" type="password" maxlength="512" autocomplete="new-password" placeholder="${ai.apiKeyConfigured ? "已配置，留空保持不变" : "gsk_..."}" /></label>
      </div>
      <div class="database-actions"><button class="ghost-button" id="test-ai" type="button">测试 AI</button><button class="primary-button" type="submit">测试并保存</button><p id="ai-status" role="status">${ai.persisted ? `已加载配置 · 已完成 ${ai.completedReviews} 条` : "尚未配置"}</p></div>
    </form>
    <form class="database-panel gameplay-cap-panel" id="gameplay-cap-settings">
      <div class="panel-title"><div><span>玩法经济规则</span><h2>奖励产出上限</h2></div><button class="square-icon-button" id="add-gameplay-cap" type="button" title="高级：手动填写数据库 action" aria-label="高级：手动填写数据库 action"><i data-lucide="code-2"></i></button></div>
      <p class="database-note">从服务器最近 30 天的奖励日志中选择即可。建议值按历史单角色峰值增加 20% 余量，只用于开始影子观察；填 0 表示不检查该时间窗口。</p>
      <section class="gameplay-catalog" aria-labelledby="gameplay-catalog-title"><div class="catalog-heading"><div><span>服务器自动发现</span><h3 id="gameplay-catalog-title">可配置的奖励行为</h3></div><p>${gameplayCatalog.connected ? `发现 ${gameplayCatalog.actions.length} 种行为` : database.enabled ? escapeHtml(gameplayCatalog.error || "暂时无法读取行为目录") : "先启用上方游戏数据库连接"}</p></div>${catalogRows || `<p class="catalog-empty">${gameplayCatalog.connected ? "最近 30 天没有奖励记录" : "连接数据库后，这里会自动出现玩法和建议上限，不需要手写代码。"}</p>`}</section>
      <div class="configured-caps-title"><div><span>已经加入</span><h3>当前检测规则</h3></div><p>${gameplay.currentVersion ? `版本 ${escapeHtml(gameplay.currentVersion)}` : versioningSupported ? "尚无已发布版本" : "当前服务重启后启用版本历史"}</p></div>
      <div class="gameplay-cap-head" aria-hidden="true"><span>数据库代码</span><span>显示名称</span><span>每角色单日上限</span><span>每角色 10 分钟上限</span><span>检测</span><span></span></div>
      <div class="gameplay-cap-rows" id="gameplay-cap-rows">${gameplay.caps.map(capRow).join("") || '<p class="cap-empty">尚未加入检测规则，请从上方服务器行为中选择。</p>'}</div>
      <div class="database-actions"><button class="ghost-button" id="replay-gameplay-caps" type="button" ${versioningSupported ? "" : "hidden"}><i data-lucide="history"></i>历史回放</button><button class="primary-button" type="submit">保存并立即生效</button><p id="gameplay-cap-status" role="status">已配置 ${gameplay.caps.length} 个玩法${versioningSupported ? ` · ${gameplay.versions.length} 个版本` : ""}</p></div>
    </form>
    <section class="rules-list">
      ${rules.map((rule) => `<article><div><span class="badge neutral">${escapeHtml(rule.level)}</span><h3>${escapeHtml(rule.name)}</h3><p>${escapeHtml(rule.desc)}</p></div><span class="rule-runtime ${rule.enabled ? "active" : "evidence"}"><i></i><b>${rule.enabled ? "内置生效" : "辅助证据"}</b></span></article>`).join("")}
    </section>
    <section class="settings-grid"><article><span>数据保留</span><h3>沿用游戏服</h3><p>当前只读现有日志，不改变清理周期。</p></article><article><span>事件覆盖</span><h3>16 张权威表</h3><p>金银元宝、金钱、道具、宠物、摆摊、商城与币值校验已接入。</p></article><article><span>处置出口</span><h3>区服命令队列</h3><p>人工确认后由绑定区服的插件拉取执行并回传结果。</p></article></section>`;
  const databaseForm = document.querySelector("#database-settings");
  const databaseStatus = document.querySelector("#database-status");
  const databasePayload = () => {
    const form = new FormData(databaseForm);
    return {
      enabled: databaseForm.elements.enabled.checked,
      host: String(form.get("host") || "").trim(),
      port: Number(form.get("port")),
      user: String(form.get("user") || "").trim(),
      password: String(form.get("password") || ""),
      mainDatabase: String(form.get("mainDatabase") || "").trim(),
      logDatabase: String(form.get("logDatabase") || "").trim(),
    };
  };
  document.querySelector("#test-database").addEventListener("click", async () => {
    databaseStatus.className = "working";
    databaseStatus.textContent = "正在连接并检查核心表…";
    try {
      const result = await api("/api/settings/database/test", { method: "POST", body: JSON.stringify(databasePayload()) });
      databaseStatus.className = "success";
      databaseStatus.textContent = `${result.message} · ${result.verifiedTables} 张核心表 · MySQL ${result.serverVersion}`;
    } catch (error) {
      databaseStatus.className = "error";
      databaseStatus.textContent = error.message;
    }
  });
  databaseForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    databaseStatus.className = "working";
    databaseStatus.textContent = "正在验证并保存加密配置…";
    try {
      const result = await api("/api/settings/database", { method: "POST", body: JSON.stringify(databasePayload()) });
      databaseForm.elements.password.value = "";
      databaseForm.elements.password.placeholder = result.config.passwordConfigured ? "已配置，留空保持不变" : "请输入只读账号密码";
      databaseStatus.className = "success";
      databaseStatus.textContent = result.test ? `${result.test.message}，配置已立即生效` : "配置已保存，当前使用演示模式";
    } catch (error) {
      databaseStatus.className = "error";
      databaseStatus.textContent = error.message;
    }
  });
  const aiForm = document.querySelector("#ai-settings");
  const aiStatus = document.querySelector("#ai-status");
  const aiPayload = () => {
    const form = new FormData(aiForm);
    return {
      enabled: aiForm.elements.enabled.checked,
      provider: String(form.get("provider") || "groq"),
      model: String(form.get("model") || "").trim(),
      apiKey: String(form.get("apiKey") || "").trim(),
    };
  };
  const syncAiProvider = () => {
    const ollama = aiForm.elements.provider.value === "ollama";
    aiForm.elements.apiKey.disabled = ollama;
    if (ollama && aiForm.elements.model.value === "qwen/qwen3.6-27b") aiForm.elements.model.value = "qwen3:4b";
    if (!ollama && aiForm.elements.model.value === "qwen3:4b") aiForm.elements.model.value = "qwen/qwen3.6-27b";
  };
  aiForm.elements.provider.addEventListener("change", syncAiProvider);
  syncAiProvider();
  document.querySelector("#test-ai").addEventListener("click", async () => {
    aiStatus.className = "working";
    aiStatus.textContent = "正在请求模型并校验 JSON…";
    try {
      const result = await api("/api/settings/ai/test", { method: "POST", body: JSON.stringify(aiPayload()) });
      aiStatus.className = "success";
      aiStatus.textContent = `${result.provider} / ${result.model} 响应正常`;
    } catch (error) {
      aiStatus.className = "error";
      aiStatus.textContent = error.message;
    }
  });
  aiForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    aiStatus.className = "working";
    aiStatus.textContent = "正在验证并保存加密配置…";
    try {
      const result = await api("/api/settings/ai", { method: "POST", body: JSON.stringify(aiPayload()) });
      aiForm.elements.apiKey.value = "";
      aiForm.elements.apiKey.placeholder = result.config.apiKeyConfigured ? "已配置，留空保持不变" : "gsk_...";
      aiStatus.className = "success";
      aiStatus.textContent = result.config.enabled ? "配置已保存，自动研判 Worker 已启动" : "配置已保存，自动研判已停用";
    } catch (error) {
      aiStatus.className = "error";
      aiStatus.textContent = error.message;
    }
  });
  const capForm = document.querySelector("#gameplay-cap-settings");
  const capRows = document.querySelector("#gameplay-cap-rows");
  const capStatus = document.querySelector("#gameplay-cap-status");
  const syncCatalogButtons = () => {
    const configured = new Set([...capRows.querySelectorAll("[data-cap-action]")].map((input) => input.value.trim()));
    document.querySelectorAll("[data-catalog-add]").forEach((button) => {
      const added = configured.has(button.dataset.catalogAdd);
      button.disabled = added;
      button.querySelector("span").textContent = added ? "已加入" : "加入检测";
    });
  };
  const bindCapRemovers = () => {
    capRows.querySelectorAll("[data-cap-remove]").forEach((button) => {
      if (button.dataset.bound) return;
      button.dataset.bound = "1";
      button.addEventListener("click", () => {
        button.closest("[data-cap-row]").remove();
        if (!capRows.querySelector("[data-cap-row]")) capRows.innerHTML = '<p class="cap-empty">尚未加入检测规则，请从上方服务器行为中选择。</p>';
        syncCatalogButtons();
      });
    });
  };
  document.querySelectorAll("[data-catalog-add]").forEach((button) => button.addEventListener("click", () => {
    const item = gameplayCatalog.actions.find((candidate) => candidate.action === button.dataset.catalogAdd);
    if (!item) return;
    capRows.querySelector(".cap-empty")?.remove();
    capRows.insertAdjacentHTML("beforeend", capRow({ action: item.action, label: item.label, dailyLimit: item.suggestedDailyLimit, burst10mLimit: item.suggestedBurst10mLimit, enabled: true }, nextCapIndex++));
    bindCapRemovers();
    syncCatalogButtons();
    window.lucide?.createIcons();
    capStatus.className = "working";
    capStatus.textContent = `已加入 ${item.label}，确认建议值后点击保存`;
    capRows.lastElementChild?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }));
  document.querySelector("#add-gameplay-cap").addEventListener("click", () => {
    capRows.querySelector(".cap-empty")?.remove();
    capRows.insertAdjacentHTML("beforeend", capRow({ action: "", label: "", dailyLimit: 0, burst10mLimit: 0, enabled: false, custom: true }, nextCapIndex++));
    bindCapRemovers();
    window.lucide?.createIcons();
    capRows.lastElementChild?.querySelector("[data-cap-action]")?.focus();
    capStatus.className = "working";
    capStatus.textContent = "高级模式：仅填写插件或源码已经确认语义的 action";
  });
  bindCapRemovers();
  syncCatalogButtons();
  const readCaps = () => [...capRows.querySelectorAll("[data-cap-row]")].map((row) => ({
    action: row.querySelector("[data-cap-action]").value.trim(),
    label: row.querySelector("[data-cap-label]").value.trim(),
    dailyLimit: Number(row.querySelector("[data-cap-daily]").value),
    burst10mLimit: Number(row.querySelector("[data-cap-burst]").value),
    enabled: row.querySelector("[data-cap-enabled]").checked,
  }));
  capForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const caps = readCaps();
    capStatus.className = "working";
    capStatus.textContent = "正在校验并保存…";
    try {
      const result = await api("/api/settings/gameplay-caps", { method: "POST", body: JSON.stringify({ caps }) });
      capStatus.className = "success";
      capStatus.textContent = result.currentVersion
        ? `${result.created ? "已发布" : "已切换到"} ${result.currentVersion} · ${result.caps.length} 个玩法`
        : `已保存 ${result.caps.length} 个玩法，下一次分析立即生效`;
    } catch (error) {
      capStatus.className = "error";
      capStatus.textContent = error.message;
    }
  });
  const replayButton = document.querySelector("#replay-gameplay-caps");
  if (versioningSupported) replayButton.addEventListener("click", async () => {
    replayButton.disabled = true;
    capStatus.className = "working";
    capStatus.textContent = "正在用最近 30 天数据回放当前版本与候选配置…";
    try {
      const result = await api("/api/settings/gameplay-caps/replay", { method: "POST", body: JSON.stringify({ caps: readCaps() }) });
      capStatus.className = "success";
      capStatus.textContent = `回放完成：告警 ${result.baseline.total} → ${result.candidate.total}，新增 ${result.delta.added}，移除 ${result.delta.removed}，评分变化 ${result.delta.scoreChanged}`;
    } catch (error) {
      capStatus.className = "error";
      capStatus.textContent = error.message;
    } finally {
      replayButton.disabled = false;
    }
  });
}

function formatBytes(value) {
  if (!Number.isFinite(value)) return "文件不可用";
  return value >= 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${Math.ceil(value / 1024)} KB`;
}

async function copyText(value) {
  if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(value);
  const temporary = document.createElement("textarea");
  temporary.value = value;
  temporary.setAttribute("readonly", "");
  temporary.style.position = "fixed";
  temporary.style.opacity = "0";
  document.body.appendChild(temporary);
  temporary.select();
  const copied = document.execCommand("copy");
  temporary.remove();
  if (!copied) throw new Error("复制失败");
}

function artifactLink(artifact, icon = "download") {
  if (!artifact?.available) return `<span class="download-button disabled">文件不可用</span>`;
  return `<a class="download-button" href="${escapeHtml(artifact.url)}" download><i data-lucide="${icon}"></i>下载</a>`;
}

function bindIntegrationActions() {
  document.querySelectorAll("[data-copy-target]").forEach((button) => {
    button.addEventListener("click", async () => {
      const source = document.querySelector(`#${button.dataset.copyTarget}`);
      if (!source) return;
      try {
        await copyText(source.textContent);
        button.querySelector("span").textContent = "已复制";
        setTimeout(() => {
          const label = button.querySelector("span");
          if (label) label.textContent = "复制";
        }, 1500);
      } catch {
        button.querySelector("span").textContent = "复制失败";
      }
    });
  });
  document.querySelector("#sdk-key-form")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const submit = event.currentTarget.querySelector("button[type='submit']");
    submit.disabled = true;
    try {
      const fields = new FormData(event.currentTarget);
      pendingSdkCredential = await api("/api/sdk-keys", {
        method: "POST",
        body: JSON.stringify({ name: fields.get("name"), tenantId: fields.get("tenantId"), serverId: fields.get("serverId") }),
      });
      await renderIntegration();
    } catch (error) {
      submit.disabled = false;
      document.querySelector("#sdk-key-error").textContent = error.message;
    }
  });
  document.querySelectorAll("[data-sdk-action]").forEach((button) => {
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        const result = await api(`/api/sdk-keys/${button.dataset.sdkId}/${button.dataset.sdkAction}`, { method: "POST" });
        pendingSdkCredential = result.secret ? result : null;
        await renderIntegration();
      } catch (error) {
        button.disabled = false;
        document.querySelector("#sdk-key-error").textContent = error.message;
      }
    });
  });
  window.lucide?.createIcons();
}

async function renderIntegration() {
  const data = await api("/api/integration");
  const agent = data.agent;
  const artifacts = Object.fromEntries(data.artifacts.map((artifact) => [artifact.id, artifact]));
  const credentials = data.sdkCredentials || [];
  const agentStatus = agent.connected ? "Agent 已连接" : "Agent 未连接";
  const sdkCode = `pgr_init(&config);\npgr_emit_json(batch_json, batch_len);\npgr_check_json(request_json, request_len, response, &response_len);\npgr_pull_actions(pull_json, pull_len, response, &response_len);\npgr_ack_action(ack_json, ack_len, response, &response_len);\npgr_flush(1000);\npgr_shutdown();`;
  const envCode = `PGR_TENANT_ID=<tenant-id>\nPGR_SERVER_ID=<server-id>\nPGR_LOCAL_TOKEN=<至少 32 字节随机 Token>\nPGR_AGENT_PORT=17870\nPGR_MODE=shadow`;
  const verifyCode = `curl http://127.0.0.1:17870/agent/v1/health\n./risk_sdk_c_example 127.0.0.1:17870`;
  const remoteEndpoint = location.protocol === "https:" ? `${location.origin}/sdk/v1` : "https://<你的风控域名>/sdk/v1";
  const remoteCode = pendingSdkCredential
    ? `PGR_ENDPOINT=${remoteEndpoint}\nPGR_SDK_KEY=${pendingSdkCredential.secret}`
    : "";
  content.innerHTML = `
    <section class="page-heading integration-heading"><span>插件接入</span><h1>游戏服权威事件接入</h1><p>同机插件可走本机 Rust Agent；跨机器插件使用区服 SDK 凭据调用平台 HTTPS 网关。数据库继续负责历史对账与漏报校准。</p></section>

    <section class="integration-status ${agent.connected ? "connected" : "offline"}">
      <div><span class="integration-live"><i></i>${agentStatus}</span><h2>${agent.connected ? "实时风控链路正在运行" : "接入资料已就绪，等待启动 Agent"}</h2><p>${escapeHtml(agent.connected ? `本机 ${agent.bind || agent.endpoint} · schema ${agent.schema_versions?.join(", ") || data.contract.schemaVersion}` : `${agent.endpoint} · ${agent.error}`)}</p></div>
      <dl>
        <div><dt>运行模式</dt><dd>${escapeHtml(agent.mode || "shadow")}</dd></div>
        <div><dt>持久队列</dt><dd>${escapeHtml(agent.queue_depth ?? "-")}</dd></div>
        <div><dt>待处理告警</dt><dd>${escapeHtml(agent.open_alerts ?? "-")}</dd></div>
        <div><dt>实时规则</dt><dd>${escapeHtml(agent.realtime_rules?.length ?? data.contract.realtimeRuleCount)}</dd></div>
      </dl>
    </section>

    <section class="integration-steps" aria-label="接入步骤">
      <article><span>01</span><i data-lucide="monitor-down"></i><div><h3>下载对应 SDK</h3><p>Windows DLL 或 Linux SO，均附 C 头文件与调用示例。</p></div></article>
      <article><span>02</span><i data-lucide="server"></i><div><h3>选择接入模式</h3><p>同机走本地 Agent；跨机生成区服 SDK 凭据并走 HTTPS。</p></div></article>
      <article><span>03</span><i data-lucide="shield-check"></i><div><h3>在提交点上报</h3><p>先接资产账本和校验失败，再逐步补齐状态快照。</p></div></article>
    </section>

    <section class="integration-panel remote-sdk-panel">
      <div class="panel-title"><div><span>远程 SDK</span><h2>为每个区服生成独立凭据</h2></div><p>密钥只显示一次；轮换后旧密钥立即失效。</p></div>
      <form id="sdk-key-form" class="sdk-key-form">
        <label><span>凭据名称</span><input name="name" maxlength="80" placeholder="一区主插件" required></label>
        <label><span>租户编号</span><input name="tenantId" maxlength="128" pattern="[A-Za-z0-9_.-]+" placeholder="tenant-001" required></label>
        <label><span>区服编号</span><input name="serverId" maxlength="128" pattern="[A-Za-z0-9_.-]+" placeholder="server-001" required></label>
        <button class="primary-button" type="submit"><i data-lucide="key-round"></i>生成凭据</button>
      </form>
      <p id="sdk-key-error" class="form-error" aria-live="polite"></p>
      ${location.protocol !== "https:" ? `<p class="gateway-warning"><i data-lucide="triangle-alert"></i><span>当前控制台还是 HTTP。远程插件正式接入前，必须先为平台配置域名和 TLS 证书。</span></p>` : ""}
      ${pendingSdkCredential ? `<div class="credential-secret"><div><strong>请立即保存，关闭页面后无法再次查看</strong><span>${escapeHtml(pendingSdkCredential.credential.tenantId)} / ${escapeHtml(pendingSdkCredential.credential.serverId)}</span></div><pre id="remote-sdk-code"><code>${escapeHtml(remoteCode)}</code></pre><button class="icon-text-button" data-copy-target="remote-sdk-code" type="button"><i data-lucide="copy"></i><span>复制</span></button></div>` : ""}
      <div class="sdk-key-list">
        ${credentials.length ? credentials.map((credential) => `<article><div><strong>${escapeHtml(credential.name)}</strong><span>${escapeHtml(credential.tenantId)} / ${escapeHtml(credential.serverId)} · ${escapeHtml(credential.prefix)}…</span></div><span class="credential-status ${credential.status}">${credential.status === "active" ? "使用中" : "已吊销"}</span>${credential.status === "active" ? `<button class="icon-text-button" type="button" data-sdk-action="rotate" data-sdk-id="${escapeHtml(credential.id)}"><i data-lucide="refresh-cw"></i><span>轮换</span></button><button class="icon-text-button danger-button" type="button" data-sdk-action="revoke" data-sdk-id="${escapeHtml(credential.id)}"><i data-lucide="ban"></i><span>吊销</span></button>` : ""}</article>`).join("") : `<p class="integration-note">还没有远程凭据。本机 Agent 模式不需要生成。</p>`}
      </div>
      <p class="security-note"><i data-lucide="shield-check"></i><span>公网入口必须使用 HTTPS。平台只保存密钥摘要，并从认证上下文注入租户和区服；事件正文不能覆盖身份。</span></p>
    </section>

    <section class="integration-layout">
      <div class="integration-main">
        <section class="integration-panel">
          <div class="panel-title"><div><span>SDK 下载</span><h2>选择游戏服运行平台</h2></div></div>
          <div class="download-list">
            ${[artifacts["windows-sdk"], artifacts["linux-sdk"]].map((artifact) => `<article><i data-lucide="package"></i><div><strong>${escapeHtml(artifact.label)}</strong><span>${formatBytes(artifact.size)} · ${artifact.available ? `SHA256 ${escapeHtml(artifact.sha256.slice(0, 12))}…` : "尚未生成"}</span></div>${artifactLink(artifact)}</article>`).join("")}
          </div>
        </section>

        <section class="integration-panel">
          <div class="code-title"><div><span>插件接口</span><h2>7 个 C ABI 调用</h2></div><button class="icon-text-button" data-copy-target="sdk-interface-code" type="button" title="复制接口示例"><i data-lucide="copy"></i><span>复制</span></button></div>
          <pre id="sdk-interface-code"><code>${escapeHtml(sdkCode)}</code></pre>
          <p class="integration-note">所有字符串使用 UTF-8；事件异步上报，区服插件按 <code>action.id</code> 幂等执行命令并回传终态。</p>
        </section>

        <section class="integration-panel">
          <div class="panel-title"><div><span>必须上报</span><h2>第一阶段事件</h2></div></div>
          <div class="event-grid">
            <article><code>ledger.currency_changed</code><p>金元宝、银元宝和游戏币的增减原因与事务号。</p></article>
            <article><code>ledger.asset_created</code><p>道具或宠物生成来源、配置版本和唯一资产号。</p></article>
            <article><code>ledger.asset_moved</code><p>交易、邮件、摆摊、仓库的前后持有人。</p></article>
            <article><code>ledger.reward_granted</code><p>活动、任务、副本奖励及领取次数上限。</p></article>
            <article><code>ledger.trade_committed</code><p>交易双方所有币值腿和资产腿必须闭合。</p></article>
            <article><code>security.validation_failed</code><p>服务端拒绝、重复领取、越权和配置校验失败。</p></article>
            <article><code>state.player_snapshot</code><p>关键时点等级、地图、在线和资产余额快照。</p></article>
          </div>
        </section>
      </div>

      <aside class="integration-side">
        <section class="integration-panel">
          <div class="code-title"><div><span>Agent 配置</span><h2>本机环境变量</h2></div><button class="icon-text-button" data-copy-target="agent-env-code" type="button" title="复制 Agent 配置"><i data-lucide="copy"></i><span>复制</span></button></div>
          <pre id="agent-env-code"><code>${escapeHtml(envCode)}</code></pre>
          <p class="security-note"><i data-lucide="lock-keyhole"></i><span>这是同机模式：Agent 仍只监听 <code>127.0.0.1</code>。不同机器请使用上方 HTTPS SDK 凭据。</span></p>
        </section>

        <section class="integration-panel">
          <div class="panel-title"><div><span>接入资料</span><h2>合同与示例</h2></div></div>
          <div class="document-list">
            ${[artifacts["integration-guide"], artifacts["event-schema"], artifacts["event-example"]].map((artifact, index) => `<article><i data-lucide="${index === 0 ? "file-text" : "file-json"}"></i><div><strong>${escapeHtml(artifact.label)}</strong><span>${formatBytes(artifact.size)}</span></div>${artifactLink(artifact, index === 0 ? "file-down" : "download")}</article>`).join("")}
          </div>
        </section>

        <section class="integration-panel">
          <div class="code-title"><div><span>上线自检</span><h2>两条命令</h2></div><button class="icon-text-button" data-copy-target="verify-code" type="button" title="复制自检命令"><i data-lucide="copy"></i><span>复制</span></button></div>
          <pre id="verify-code"><code>${escapeHtml(verifyCode)}</code></pre>
          <p class="integration-note">先确认健康接口，再用 SDK 示例发送事件；影子模式下不处罚真实玩家。</p>
        </section>
      </aside>
    </section>`;
  bindIntegrationActions();
}

const renderers = { overview: renderOverview, player: renderPlayer, asset: renderAsset, alerts: renderAlerts, rules: renderRules, integration: renderIntegration };

async function setView(view, context = {}) {
  if (view !== "integration") pendingSdkCredential = null;
  currentView = view;
  viewContext = context;
  activateNav(view);
  content.innerHTML = '<div class="loading-state">正在读取风控数据…</div>';
  try {
    await renderers[view]();
    window.lucide?.createIcons();
    content.focus({ preventScroll: true });
  } catch (error) {
    content.innerHTML = `<p class="empty-state">${escapeHtml(error.message)}</p>`;
  }
}

function bindGoButtons() {
  document.querySelectorAll("[data-go]").forEach((button) => button.addEventListener("click", () => setView(button.dataset.go)));
}

function bindAlertRows() {
  document.querySelectorAll("[data-alert-id]").forEach((element) => {
    const open = () => setView("alerts", { alertId: element.dataset.alertId });
    element.addEventListener("click", (event) => {
      if (event.currentTarget.matches("tr") && event.target.closest("[data-alert-id]") !== event.currentTarget) return;
      open();
    });
    element.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); }
    });
  });
}

loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  loginError.textContent = "";
  const key = document.querySelector("#portal-key").value;
  try {
    await api("/api/login", { method: "POST", body: JSON.stringify({ key }) });
    showConsole();
    await setView("overview");
    showDisclaimer();
  } catch (error) {
    loginError.textContent = error.message;
  }
});

disclaimerDialog.addEventListener("cancel", (event) => event.preventDefault());
disclaimerAccept.addEventListener("click", () => disclaimerDialog.close());

document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => setView(button.dataset.view)));
document.querySelector("#refresh-button").addEventListener("click", () => setView(currentView, viewContext));
document.querySelector("#logout-button").addEventListener("click", async () => {
  await api("/api/logout", { method: "POST" }).catch(() => {});
  showLogin();
});

api("/api/session")
  .then(() => {
    showConsole();
    return setView("overview");
  })
  .catch(() => showLogin());

const alertScore = (alert) => Number.isFinite(Number(alert?.score)) ? Number(alert.score) : 0;

const alertSummary = (alerts) => ({
  total: alerts.length,
  highRisk: alerts.filter((alert) => alertScore(alert) >= 70).length,
  scoreTotal: alerts.reduce((total, alert) => total + alertScore(alert), 0),
});

export function compareRuleReplay(baseline, candidate) {
  if (!Array.isArray(baseline) || !Array.isArray(candidate)) throw new Error("alerts must be arrays");
  const byId = (alerts) => new Map(alerts.filter((alert) => alert?.id).map((alert) => [String(alert.id), alert]));
  const before = byId(baseline);
  const after = byId(candidate);
  const ids = new Set([...before.keys(), ...after.keys()]);
  const changes = [];
  let added = 0;
  let removed = 0;
  let scoreChanged = 0;

  for (const id of ids) {
    const oldAlert = before.get(id);
    const newAlert = after.get(id);
    if (!oldAlert) added += 1;
    else if (!newAlert) removed += 1;
    else if (alertScore(oldAlert) !== alertScore(newAlert) || oldAlert.rule !== newAlert.rule) scoreChanged += 1;
    else continue;
    changes.push({
      id,
      player: String(newAlert?.player || oldAlert?.player || ""),
      beforeScore: oldAlert ? alertScore(oldAlert) : null,
      afterScore: newAlert ? alertScore(newAlert) : null,
      beforeRule: oldAlert?.rule || null,
      afterRule: newAlert?.rule || null,
    });
  }
  changes.sort((left, right) => {
    const leftDelta = Math.abs((left.afterScore ?? 0) - (left.beforeScore ?? 0));
    const rightDelta = Math.abs((right.afterScore ?? 0) - (right.beforeScore ?? 0));
    return rightDelta - leftDelta || left.id.localeCompare(right.id);
  });
  return {
    baseline: alertSummary(baseline),
    candidate: alertSummary(candidate),
    delta: { total: candidate.length - baseline.length, added, removed, scoreChanged },
    changes: changes.slice(0, 50),
  };
}

$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$binary = Join-Path $projectRoot "target\debug\risk-agent.exe"
$example = Join-Path $projectRoot "docs\plugin-event-batch.v1.example.json"
$port = 17871

if (-not (Test-Path -LiteralPath $binary)) {
    throw "risk-agent debug binary missing: $binary"
}
if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) {
    throw "port $port is already in use"
}

$tokenBytes = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
try {
    $rng.GetBytes($tokenBytes)
} finally {
    $rng.Dispose()
}
$token = [Convert]::ToBase64String($tokenBytes)
$tag = [Guid]::NewGuid().ToString("N")
$queue = Join-Path $env:TEMP "risk-agent-http-smoke-$tag.db"
$stdout = Join-Path $env:TEMP "risk-agent-http-smoke-$tag.out"
$stderr = Join-Path $env:TEMP "risk-agent-http-smoke-$tag.err"

$env:PGR_TENANT_ID = "tenant-http-smoke"
$env:PGR_SERVER_ID = "server-http-smoke"
$env:PGR_LOCAL_TOKEN = $token
$env:PGR_AGENT_PORT = [string]$port
$env:PGR_QUEUE_DB = $queue
$env:PGR_MODE = "shadow"

$process = $null
try {
    $process = Start-Process -FilePath $binary -ArgumentList "serve" -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $base = "http://127.0.0.1:$port"
    $ready = $false
    for ($attempt = 0; $attempt -lt 50; $attempt += 1) {
        try {
            $health = Invoke-RestMethod -Uri "$base/agent/v1/health" -Method Get -TimeoutSec 2
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) {
        throw "risk-agent did not become ready"
    }

    $unauthorized = 0
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/agent/v1/flush" -Method Post -ContentType "application/json" -Body "{}" -TimeoutSec 3 | Out-Null
    } catch {
        $unauthorized = [int]$_.Exception.Response.StatusCode
    }

    $headers = @{ "X-PGR-Local-Token" = $token }
    $batch = Get-Content -LiteralPath $example -Raw -Encoding utf8
    $batchBytes = [Text.Encoding]::UTF8.GetBytes($batch)
    $first = Invoke-RestMethod -Uri "$base/agent/v1/events:batch" -Method Post -Headers $headers -ContentType "application/json; charset=utf-8" -Body $batchBytes -TimeoutSec 10
    $second = Invoke-RestMethod -Uri "$base/agent/v1/events:batch" -Method Post -Headers $headers -ContentType "application/json; charset=utf-8" -Body $batchBytes -TimeoutSec 10

    $decisionBody = @{
        schema_version = "1.0"
        request_id = "decision-http-smoke-0001"
        occurred_at = "2026-07-31T00:10:22.381+08:00"
        action_type = "trade.commit"
        transaction_id = "trade-http-smoke"
        timeout_ms = 20
        actor = @{ player_id = "10001"; account_id = "account-11"; session_id = "session-781" }
        proposed_changes = @{ currency_changes = @(); asset_changes = @() }
    } | ConvertTo-Json -Depth 8 -Compress
    $decision = Invoke-RestMethod -Uri "$base/agent/v1/decisions:check" -Method Post -Headers $headers -ContentType "application/json" -Body $decisionBody -TimeoutSec 5
    $reviewBody = @{
        schema_version = "1.0"
        request_id = "decision-http-smoke-high-value-0001"
        occurred_at = "2026-07-31T00:10:23.381+08:00"
        action_type = "trade.commit"
        transaction_id = "trade-http-smoke-high-value"
        timeout_ms = 20
        actor = @{ player_id = "10001"; device_fingerprint = "hmac-sha256:http-smoke-device" }
        counterparty = @{ player_id = "10002"; device_fingerprint = "hmac-sha256:http-smoke-device" }
        proposed_changes = @{
            currency_changes = @(
                @{ owner_id = "10001"; currency = "gold_coin"; before = "0"; after = "2000000"; delta = "2000000" },
                @{ owner_id = "10002"; currency = "gold_coin"; before = "2000000"; after = "0"; delta = "-2000000" }
            )
            asset_changes = @()
        }
    } | ConvertTo-Json -Depth 8 -Compress
    $review = Invoke-RestMethod -Uri "$base/agent/v1/decisions:check" -Method Post -Headers $headers -ContentType "application/json" -Body $reviewBody -TimeoutSec 5
    $alerts = Invoke-RestMethod -Uri "$base/agent/v1/alerts" -Method Get -Headers $headers -TimeoutSec 5
    $finalHealth = Invoke-RestMethod -Uri "$base/agent/v1/health" -Method Get -TimeoutSec 2
    $flush = Invoke-RestMethod -Uri "$base/agent/v1/flush" -Method Post -Headers $headers -ContentType "application/json" -Body "{}" -TimeoutSec 5

    if ($unauthorized -ne 401) { throw "unauthorized request returned $unauthorized" }
    if ($first.accepted -ne 7 -or $first.rejected.Count -ne 0 -or $first.queue_depth -ne 7 -or $first.alerts_created -ne 2) { throw "first ingest assertion failed" }
    if ($second.accepted -ne 0 -or $second.duplicates -ne 7 -or $second.queue_depth -ne 7 -or $second.alerts_created -ne 0) { throw "idempotent retry assertion failed" }
    if ($decision.decision -ne "allow" -or $decision.mode -ne "shadow") { throw "shadow decision assertion failed" }
    if ($review.decision -ne "review" -or $review.risk_score -ne 80 -or $review.rule_codes.Count -ne 2) { throw "high-value review assertion failed" }
    if ($alerts.open -ne 4 -or $alerts.returned -ne 4) { throw "realtime alerts assertion failed" }
    if ($finalHealth.open_alerts -ne 4 -or $finalHealth.realtime_rules.Count -lt 15) { throw "realtime health assertion failed" }
    if ($flush.upstream_configured -ne $false -or $flush.queue_depth -ne 7) { throw "flush status assertion failed" }

    Write-Output "agent HTTP check ok"
    Write-Output "health=ok bind=$($health.bind) mode=$($health.mode) upstream=$($health.upstream_configured)"
    Write-Output "auth=401 first=7 duplicate=7 queue=7 alerts=4 decision=allow+review/shadow"
} finally {
    if ($process -and -not $process.HasExited) {
        $process.Kill()
        $process.WaitForExit()
    }
    Remove-Item Env:PGR_TENANT_ID, Env:PGR_SERVER_ID, Env:PGR_LOCAL_TOKEN, Env:PGR_AGENT_PORT, Env:PGR_QUEUE_DB, Env:PGR_MODE -ErrorAction SilentlyContinue
    foreach ($path in @($queue, "$queue-wal", "$queue-shm", $stdout, $stderr)) {
        $resolvedTemp = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
        $resolvedPath = [IO.Path]::GetFullPath($path)
        if (-not $resolvedPath.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to clean non-temp path: $resolvedPath"
        }
        if ([IO.File]::Exists($resolvedPath)) {
            [IO.File]::Delete($resolvedPath)
        }
    }
}

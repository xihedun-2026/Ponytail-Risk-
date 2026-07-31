$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$agentBinary = Join-Path $projectRoot "target\debug\risk-agent.exe"
$sdkLibrary = Join-Path $projectRoot "target\release\risk_sdk.dll"
$source = Join-Path $projectRoot "crates\risk-sdk\tests\c_abi_check.c"
$example = Join-Path $projectRoot "docs\plugin-event-batch.v1.example.json"
$outputDirectory = Join-Path $projectRoot "target\c-abi-check"
$caller = Join-Path $outputDirectory "pgr_c_abi_check.exe"
$vswhere = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe"
$port = 17872

foreach ($path in @($agentBinary, $sdkLibrary, $source, $example, $vswhere)) {
    if (-not (Test-Path -LiteralPath $path)) { throw "required file missing: $path" }
}
if (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue) {
    throw "port $port is already in use"
}

$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw "Visual C++ build tools not found" }
$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$compile = 'call "' + $vcvars + '" >nul && cl.exe /nologo /TC /std:c11 /utf-8 /W4 /WX /I"' +
    (Join-Path $projectRoot "crates\risk-sdk\include") + '" "' + $source + '" /Fe:"' + $caller + '"'
& cmd.exe /d /c $compile
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $caller)) {
    throw "C ABI caller compilation failed"
}

$tokenBytes = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
try { $rng.GetBytes($tokenBytes) } finally { $rng.Dispose() }
$token = [Convert]::ToBase64String($tokenBytes)
$tag = [Guid]::NewGuid().ToString("N")
$queue = Join-Path $env:TEMP "risk-sdk-c-abi-$tag.db"
$stdout = Join-Path $env:TEMP "risk-sdk-c-abi-$tag.out"
$stderr = Join-Path $env:TEMP "risk-sdk-c-abi-$tag.err"

$env:PGR_TENANT_ID = "tenant-c-abi"
$env:PGR_SERVER_ID = "server-c-abi"
$env:PGR_LOCAL_TOKEN = $token
$env:PGR_AGENT_PORT = [string]$port
$env:PGR_QUEUE_DB = $queue
$env:PGR_MODE = "shadow"
$env:PGR_TEST_LOCAL_TOKEN = $token

$process = $null
try {
    $process = Start-Process -FilePath $agentBinary -ArgumentList "serve" -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $base = "http://127.0.0.1:$port"
    $ready = $false
    for ($attempt = 0; $attempt -lt 50; $attempt += 1) {
        try {
            Invoke-RestMethod -Uri "$base/agent/v1/health" -Method Get -TimeoutSec 2 | Out-Null
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) { throw "risk-agent did not become ready" }

    & $caller $sdkLibrary $base $example
    if ($LASTEXITCODE -ne 0) { throw "external C ABI check failed with exit code $LASTEXITCODE" }

    $headers = @{ "X-PGR-Local-Token" = $token }
    $health = Invoke-RestMethod -Uri "$base/agent/v1/health" -Method Get -TimeoutSec 2
    if ($health.queue_depth -ne 7) { throw "expected queue depth 7, got $($health.queue_depth)" }
    $flush = Invoke-RestMethod -Uri "$base/agent/v1/flush" -Method Post -Headers $headers -ContentType "application/json" -Body "{}" -TimeoutSec 3
    if ($flush.queue_depth -ne 7) { throw "agent flush queue assertion failed" }
    Write-Output "external DLL verification ok: exports=7 queue=7"
} finally {
    if ($process -and -not $process.HasExited) {
        $process.Kill()
        $process.WaitForExit()
    }
    Remove-Item Env:PGR_TENANT_ID, Env:PGR_SERVER_ID, Env:PGR_LOCAL_TOKEN, Env:PGR_AGENT_PORT, Env:PGR_QUEUE_DB, Env:PGR_MODE, Env:PGR_TEST_LOCAL_TOKEN -ErrorAction SilentlyContinue
    $resolvedTemp = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    foreach ($path in @($queue, "$queue-wal", "$queue-shm", $stdout, $stderr)) {
        $resolvedPath = [IO.Path]::GetFullPath($path)
        if (-not $resolvedPath.StartsWith($resolvedTemp, [StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to clean non-temp path: $resolvedPath"
        }
        if ([IO.File]::Exists($resolvedPath)) { [IO.File]::Delete($resolvedPath) }
    }
}

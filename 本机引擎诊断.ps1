# 在你自己的 Windows 电脑上跑：
#   powershell -ExecutionPolicy Bypass -File .\本机引擎诊断.ps1
#
# 用来分清「总览一直转圈 / 实时数据源不可用」到底是哪一种：
#   A. 没有本机编译的 wdsf-live-data.exe            → 跑 start-local.ps1 编译
#   B. 引擎起不来（架构不符 / 被安全软件拦截）      → 重新编译或放行
#   C. 引擎能跑，但 dashboard 太慢撞上 180 秒超时    → 需要批量查询优化
# 只做只读查询，不改数据库、不打印密码。

if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
  $PSNativeCommandUseErrorActionPreference = $false
}
Set-Location -LiteralPath $PSScriptRoot
$Engine = Join-Path $PSScriptRoot "target\release\wdsf-live-data.exe"

Write-Host "=== 1. 引擎二进制 ==="
if (-not (Test-Path -LiteralPath $Engine)) {
  Write-Host "没有 $Engine —— 还没在本机编译过。先跑：powershell -ExecutionPolicy Bypass -File .\start-local.ps1"
  exit 1
}
Get-Item -LiteralPath $Engine | Format-List Name, Length, LastWriteTime | Out-String | Write-Host
Write-Host "本机架构：$env:PROCESSOR_ARCHITECTURE"

Write-Host "`n=== 2. 能不能起来（self-check，不连数据库）==="
& $Engine self-check *> $null
if ($LASTEXITCODE -eq 0) {
  Write-Host "OK：引擎可以在本机执行"
} else {
  Write-Host "失败：退出码 $LASTEXITCODE"
  Write-Host "→ 多半是别的系统编译的二进制，或被杀毒软件拦了。解决：删掉 target\release\wdsf-live-data.exe 后重跑 start-local.ps1。"
  exit 1
}

Write-Host "`n=== 3. 载入配置 ==="
$envFile = if (Test-Path -LiteralPath ".env.local") { ".env.local" }
           elseif (Test-Path -LiteralPath "本地配置-复制成.env.local.txt") { "本地配置-复制成.env.local.txt" }
           else { $null }
if ($envFile) {
  foreach ($line in Get-Content -LiteralPath $envFile -Encoding UTF8) {
    $text = $line.Trim()
    if ($text -eq "" -or $text.StartsWith("#")) { continue }
    $split = $text.IndexOf("=")
    if ($split -lt 1) { continue }
    [Environment]::SetEnvironmentVariable($text.Substring(0, $split).Trim(), $text.Substring($split + 1).Trim().Trim('"').Trim("'"), "Process")
  }
  Write-Host "已载入 $envFile"
} else {
  Write-Host "没找到配置文件，下面两步会走演示数据。"
}

function Invoke-Timed($label, $op) {
  $out = [System.IO.Path]::GetTempFileName()
  $err = [System.IO.Path]::GetTempFileName()
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  & $Engine $op 1> $out 2> $err
  $code = $LASTEXITCODE
  $sw.Stop()
  Write-Host "$label：退出码 $code，耗时 $([int]$sw.Elapsed.TotalSeconds) 秒"
  if ($code -ne 0) {
    Write-Host "  stderr: $((Get-Content -LiteralPath $err -Raw) -replace '\s+', ' ')"
  } else {
    $text = Get-Content -LiteralPath $out -Raw
    if ($text.Length -gt 160) { $text = $text.Substring(0, 160) }
    Write-Host "  输出前 160 字节: $text"
  }
  Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue
}

Write-Host "`n=== 4. 连接测试（应该 1 秒内）==="
Invoke-Timed "connection-test" "connection-test"

Write-Host "`n=== 5. 总览取数（云端同一条命令是 47 秒；本机若 >180 秒就是超时的原因）==="
Write-Host "别中断，慢慢等，最长可能几分钟…"
Invoke-Timed "dashboard" "dashboard"

Write-Host "`n=== 结论参考 ==="
Write-Host "· 第 5 步成功但耗时接近或超过 180 秒 → 就是超时。临时办法：启动前 `$env:RISK_ENGINE_TIMEOUT_MS = '600000'；根治办法是把逐角色查询改成批量查询。"
Write-Host "· 第 5 步很快成功 → 问题不在引擎，把 node server.mjs 那个窗口里 live data dashboard: 那行贴出来。"
Write-Host "· 第 2 步就失败 → 二进制问题，按上面提示重新编译。"

# 一键启动（Windows）：缺什么自动装，装完编译引擎、预检数据库、启动控制台。
#
#   双击 一键启动.bat
#   或在 PowerShell 里： powershell -ExecutionPolicy Bypass -File .\start-local.ps1
#
# 会自动装（已装的会跳过）：
#   · Node.js LTS                    —— 需要管理员
#   · Visual Studio C++ 生成工具     —— 需要管理员，SQLite 要用它编译，约 2-4 GB
#   · Rust 稳定版                    —— 装在你自己的用户目录，不需要管理员
# 需要管理员的部分会单独弹一次 UAC 授权窗口，装完自动回到普通权限继续跑服务。
#
# 已经装好的会自动认出来，装在 D / E 盘也认得（vswhere 位置固定，跟 VS 装在哪个盘无关）。
#
# 参数：
#   -NoInstall       只检查环境，什么都不装（缺什么会直接告诉你）
#   -VsInstallPath   需要新装 C++ 生成工具、又不想占 C 盘时指定，例如 -VsInstallPath "E:\BuildTools"
#   -InstallOnly     内部使用：提权子进程只做需要管理员的那两项安装

[CmdletBinding()]
param(
  [switch]$InstallOnly,
  [switch]$NoInstall,
  # 需要装 C++ 生成工具、又不想占 C 盘时用，例如： -VsInstallPath "E:\BuildTools"
  # 已经装好的（不管在哪个盘）会被自动认出来，用不上这个参数。
  [string]$VsInstallPath = ""
)

$ErrorActionPreference = "Stop"
# PowerShell 7.4+ 默认把原生命令的非零退出也当成终止错误，那样 winget / cargo 一失败就直接中断，
# 走不到下面的兜底分支。这里关掉，改成我们自己看 $LASTEXITCODE。
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
  $PSNativeCommandUseErrorActionPreference = $false
}
Set-Location -LiteralPath $PSScriptRoot

function Info($m) { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "==> $m" -ForegroundColor Yellow }
function Fail($m) { Write-Host "==> $m" -ForegroundColor Red; exit 1 }

if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
  $nodeArch = "arm64"
  $rustupUrl = "https://win.rustup.rs/aarch64"
} else {
  $nodeArch = "x64"
  $rustupUrl = "https://win.rustup.rs/x86_64"
}

function Test-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  return (New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
}

# 安装程序改的是注册表里的 PATH，当前窗口的 $env:PATH 不会自动跟着变，
# 所以每次装完都要重新从注册表读一遍，否则刚装好的 node/cargo 仍然"找不到"。
function Update-SessionPath {
  $parts = @(
    [Environment]::GetEnvironmentVariable("Path", "Machine"),
    [Environment]::GetEnvironmentVariable("Path", "User"),
    (Join-Path $env:USERPROFILE ".cargo\bin")
  )
  $env:PATH = ($parts | Where-Object { $_ }) -join ";"
}

function Test-NodeReady {
  if (-not (Get-Command node -ErrorAction SilentlyContinue)) { return $false }
  try {
    return ([int](& node -p "process.versions.node.split('.')[0]") -ge 18)
  } catch { return $false }
}

function Test-CargoReady {
  return [bool](Get-Command cargo -ErrorAction SilentlyContinue)
}

# VS 装在哪个盘都行：vswhere.exe 本身永远在 C 盘那个固定位置，装到 E 盘也照样能报出真实路径。
# 找到的位置记在 $script:MsvcWhere 里，检查阶段会打印出来，省得怀疑"到底认没认出来"。
$script:MsvcWhere = ""
$script:MsvcSource = ""

function Test-MsvcReady {
  $onPath = Get-Command cl.exe -ErrorAction SilentlyContinue
  if ($onPath) { $script:MsvcWhere = $onPath.Source; $script:MsvcSource = "PATH"; return $true }

  # vswhere 是 VS 安装器自带的，位置固定，两个 Program Files 都试一下
  foreach ($base in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
    if (-not $base) { continue }
    $vswhere = Join-Path $base "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere)) { continue }
    foreach ($component in @("Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                             "Microsoft.VisualStudio.Component.VC.Tools.ARM64")) {
      $found = & $vswhere -products * -requires $component -property installationPath 2>$null
      if ($found) { $script:MsvcWhere = ($found | Select-Object -First 1); $script:MsvcSource = "vswhere"; return $true }
    }
  }

  # 兜底：vswhere 不在（绿色包、从别的机器搬过来的装法）时，在各固定盘按 VS 的标准目录结构找 cl.exe。
  # 每一层都有明确通配，不会变成全盘扫描。
  foreach ($drive in [System.IO.DriveInfo]::GetDrives()) {
    if ($drive.DriveType -ne [System.IO.DriveType]::Fixed -or -not $drive.IsReady) { continue }
    $root = $drive.Name.TrimEnd("\")
    $patterns = @(
      "$root\Program Files\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Host*\*\cl.exe",
      "$root\Program Files (x86)\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Host*\*\cl.exe",
      "$root\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Host*\*\cl.exe",
      "$root\VS*\VC\Tools\MSVC\*\bin\Host*\*\cl.exe",
      "$root\BuildTools\VC\Tools\MSVC\*\bin\Host*\*\cl.exe"
    )
    foreach ($pattern in $patterns) {
      $hit = Get-ChildItem -Path $pattern -ErrorAction SilentlyContinue | Select-Object -First 1
      if ($hit) { $script:MsvcWhere = $hit.FullName; $script:MsvcSource = "扫描"; return $true }
    }
  }
  return $false
}

function Test-Winget {
  return [bool](Get-Command winget -ErrorAction SilentlyContinue)
}

function Install-Node {
  Info "安装 Node.js LTS…"
  if (Test-Winget) {
    & winget install --id OpenJS.NodeJS.LTS -e --silent --accept-package-agreements --accept-source-agreements
    Update-SessionPath
    if (Test-NodeReady) { Info "Node 装好了"; return }
    Warn "winget 装 Node 没成功（退出码 $LASTEXITCODE），改用官网安装包。"
  }
  Info "从 nodejs.org 取最新 LTS 版本号…"
  $index = Invoke-RestMethod -Uri "https://nodejs.org/dist/index.json" -UseBasicParsing
  $lts = $index | Where-Object { $_.lts } | Select-Object -First 1
  if (-not $lts) { Fail "拿不到 Node 版本列表。请手动去 https://nodejs.org/ 装 LTS 后重跑本脚本。" }
  $msi = Join-Path $env:TEMP "node-$($lts.version)-$nodeArch.msi"
  $url = "https://nodejs.org/dist/$($lts.version)/node-$($lts.version)-$nodeArch.msi"
  Info "下载 $url"
  Invoke-WebRequest -Uri $url -OutFile $msi -UseBasicParsing
  Info "静默安装中…"
  $proc = Start-Process -FilePath "msiexec.exe" -ArgumentList @("/i", "`"$msi`"", "/quiet", "/norestart") -Wait -PassThru
  # 3010 = 装好了但要求重启，对 Node 来说不影响使用
  if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
    Fail "Node 安装失败（msiexec 退出码 $($proc.ExitCode)）。手动装：https://nodejs.org/"
  }
  Update-SessionPath
  if (-not (Test-NodeReady)) { Fail "Node 装完仍然找不到，重开一个 PowerShell 窗口再跑本脚本试试。" }
  Info "Node 装好了"
}

function Install-Rust {
  Info "安装 Rust（装到 $env:USERPROFILE\.cargo，不需要管理员）…"
  $init = Join-Path $env:TEMP "rustup-init.exe"
  Info "下载 $rustupUrl"
  Invoke-WebRequest -Uri $rustupUrl -OutFile $init -UseBasicParsing
  $proc = Start-Process -FilePath $init -ArgumentList @("-y", "--profile", "minimal", "--default-toolchain", "stable") -Wait -PassThru -NoNewWindow
  if ($proc.ExitCode -ne 0) {
    Fail "Rust 安装失败（退出码 $($proc.ExitCode)）。手动装：https://win.rustup.rs/ 下载 rustup-init.exe 双击运行。"
  }
  Update-SessionPath
  if (-not (Test-CargoReady)) { Fail "Rust 装完仍然找不到 cargo。重开一个 PowerShell 窗口再跑本脚本。" }
  Info "Rust 装好了"
}

function Install-BuildTools {
  Warn "安装 Visual Studio C++ 生成工具：约 2-4 GB，网速一般要 10-30 分钟，中途没有进度条，别关窗口。"
  Warn "（风控引擎里的 SQLite 是现编的 C 代码，需要它提供 cl.exe / link.exe。）"
  # 装到别的盘（C 盘紧张时）：-VsInstallPath "E:\BuildTools"
  $vsArgs = @("--quiet", "--wait", "--norestart", "--nocache",
              "--add", "Microsoft.VisualStudio.Workload.VCTools", "--includeRecommended")
  if ($VsInstallPath) {
    Info "安装目录指定为 $VsInstallPath"
    $vsArgs += @("--installPath", $VsInstallPath)
  }
  $override = ($vsArgs -join " ")
  if ($VsInstallPath) { $override = $override -replace [regex]::Escape($VsInstallPath), "`"$VsInstallPath`"" }
  if (Test-Winget) {
    & winget install --id Microsoft.VisualStudio.2022.BuildTools -e --silent `
      --accept-package-agreements --accept-source-agreements --override $override
    if (Test-MsvcReady) { Info "C++ 生成工具装好了"; return }
    Warn "winget 装生成工具没成功（退出码 $LASTEXITCODE），改用微软官方引导程序。"
  }
  $bootstrapper = Join-Path $env:TEMP "vs_BuildTools.exe"
  Info "下载 https://aka.ms/vs/17/release/vs_BuildTools.exe"
  Invoke-WebRequest -Uri "https://aka.ms/vs/17/release/vs_BuildTools.exe" -OutFile $bootstrapper -UseBasicParsing
  Info "安装中（可能要十几分钟）…"
  $proc = Start-Process -FilePath $bootstrapper -ArgumentList $vsArgs -Wait -PassThru
  if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
    Fail "C++ 生成工具安装失败（退出码 $($proc.ExitCode)）。手动装：https://visualstudio.microsoft.com/visual-cpp-build-tools/ ，勾选「使用 C++ 的桌面开发」。"
  }
  if (-not (Test-MsvcReady)) { Fail "生成工具装完仍检测不到。重启电脑后再跑一次本脚本。" }
  Info "C++ 生成工具装好了"
}

# ---------- 提权子进程模式：只做需要管理员的两件事 ----------
# Rust 故意不在这里装：提权进程的 %USERPROFILE% 是管理员账户，
# rustup 会装进管理员的 .cargo，回到普通权限后照样找不到 cargo。
if ($InstallOnly) {
  $log = Join-Path $PSScriptRoot "install-log.txt"
  try { Start-Transcript -LiteralPath $log -Force | Out-Null } catch { }
  try {
    Update-SessionPath
    if (-not (Test-NodeReady)) { Install-Node }
    if (-not (Test-MsvcReady)) { Install-BuildTools }
    Info "需要管理员的安装都完成了。"
  } catch {
    Write-Host "==> 安装出错：$($_.Exception.Message)" -ForegroundColor Red
    try { Stop-Transcript | Out-Null } catch { }
    exit 1
  }
  try { Stop-Transcript | Out-Null } catch { }
  exit 0
}

# ---------- 1. 环境检查与自动安装 ----------
Update-SessionPath
Info "检查运行环境…"
$hasNode = Test-NodeReady
$hasMsvc = Test-MsvcReady
$hasCargo = Test-CargoReady
Info ("Node.js：" + $(if ($hasNode) { "已就绪 $(node -v)" } else { "缺" }))
if ($hasMsvc) {
  # 明确打印在哪儿找到的：装在 D/E 盘也能认出来，不必怀疑
  Info "C++ 生成工具：已就绪（$script:MsvcSource 认出）$script:MsvcWhere"
  if ($script:MsvcSource -eq "扫描") {
    Warn "这份是靠目录扫描找到的，vswhere 不在标准位置；cargo 有可能仍然定位不到它。"
    Warn "编译若报 link.exe / cl.exe 找不到，就在「x64 Native Tools Command Prompt for VS」里跑本脚本。"
  }
} else {
  Info "C++ 生成工具：缺"
}
Info ("Rust：" + $(if ($hasCargo) { "已就绪" } else { "缺" }))

if ($NoInstall) {
  if (-not ($hasNode -and $hasMsvc -and $hasCargo)) { Fail "环境不全，而且指定了 -NoInstall。去掉这个参数会自动安装。" }
} else {
  if (-not $hasNode -or -not $hasMsvc) {
    if (Test-Admin) {
      if (-not $hasNode) { Install-Node }
      if (-not $hasMsvc) { Install-BuildTools }
    } else {
      Warn "Node / C++ 生成工具需要管理员权限安装，马上会弹一个 UAC 授权窗口，请点「是」。"
      Warn "安装在那个窗口里进行，日志同时写到 install-log.txt；装完会自动回到这里继续。"
      $host_exe = (Get-Process -Id $PID).Path
      $childArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"", "-InstallOnly")
      if ($VsInstallPath) { $childArgs += @("-VsInstallPath", "`"$VsInstallPath`"") }
      $proc = Start-Process -FilePath $host_exe -Verb RunAs -Wait -PassThru -ArgumentList $childArgs
      Update-SessionPath
      if ($proc.ExitCode -ne 0) {
        Fail "安装没成功（退出码 $($proc.ExitCode)）。看看同目录下的 install-log.txt，或按 README「本机快速启动（Windows）」手动装。"
      }
    }
    Update-SessionPath
    if (-not (Test-NodeReady)) { Fail "Node 仍然不可用。重开一个 PowerShell 窗口再跑本脚本；还不行就手动装 https://nodejs.org/ 。" }
    if (-not (Test-MsvcReady)) { Fail "C++ 生成工具仍然检测不到。重启电脑后再跑一次本脚本。" }
  }
  if (-not (Test-CargoReady)) { Install-Rust }
}
Info "Node $(node -v)"

# ---------- 2. Rust 引擎 ----------
# Windows 上引擎叫 risk-live-data.exe；互通文件夹里可能躺着 macOS / Linux 编译的同名文件，
# 那些在这里跑不起来，所以不看文件在不在，直接试运行一次自检来判断。
$Engine = Join-Path $PSScriptRoot "target\release\risk-live-data.exe"

function Test-Engine {
  if (-not (Test-Path -LiteralPath $Engine)) { return $false }
  try {
    & $Engine self-check *> $null
    return ($LASTEXITCODE -eq 0)
  } catch { return $false }
}

if (-not (Test-Engine)) {
  if (Test-Path -LiteralPath $Engine) {
    Warn "现有引擎在本机跑不起来（多半是别的系统/架构编译的），重新构建。"
    Remove-Item -LiteralPath $Engine -Force
  }
  Info "正在构建 Rust 引擎（首次约 2-5 分钟，之后秒开）…"
  # 只构建引擎；risk-probe 依赖 russh/tokio，本地跑网页用不到。
  & cargo build --release -p risk-engine
  if ($LASTEXITCODE -ne 0) {
    Fail @"
编译失败。最常见的原因是 C++ 生成工具装了但当前窗口没刷新到（报错里有 link.exe / cl.exe / MSVC 字样）。
先【重开一个 PowerShell 窗口】再跑一次本脚本；还不行就重启电脑后再试。
"@
  }
}
if (-not (Test-Engine)) { Fail "引擎构建后仍无法运行：$Engine" }
Info "引擎就绪"

# ---------- 3. 配置 ----------
# 数据库连接也可以在网页「规则与设置」页填写并加密保存，所以这里只需要一个登录卡密。
# .env.local 是可选的：有就用，没有就用默认卡密，绝不因此卡住。
if (-not (Test-Path -LiteralPath ".env.local") -and (Test-Path -LiteralPath ".env.example")) {
  Copy-Item -LiteralPath ".env.example" -Destination ".env.local"
  Info "已把 .env.example 复制成 .env.local；首次使用请修改本地登录卡密"
}
if (Test-Path -LiteralPath ".env.local") {
  foreach ($line in Get-Content -LiteralPath ".env.local" -Encoding UTF8) {
    $text = $line.Trim()
    if ($text -eq "" -or $text.StartsWith("#")) { continue }
    $split = $text.IndexOf("=")
    if ($split -lt 1) { continue }
    $name = $text.Substring(0, $split).Trim()
    $value = $text.Substring($split + 1).Trim().Trim('"').Trim("'")
    [Environment]::SetEnvironmentVariable($name, $value, "Process")
  }
  Info "已载入 .env.local"
} else {
  Warn "没有 .env.local，先以演示数据启动。登录后进「规则与设置」页填数据库连接即可切到真实数据。"
}
if (-not $env:RISK_PORTAL_KEY) { $env:RISK_PORTAL_KEY = "PONYTAIL-LOCAL-2026" }
if (-not $env:GAME_DB_LIVE) { $env:GAME_DB_LIVE = "0" }
if (-not $env:RISK_PORT) { $env:RISK_PORT = "4173" }

# ---------- 4. 数据源 ----------
if ($env:GAME_DB_LIVE -eq "1" -and $env:GAME_DB_PASSWORD) {
  Info "检查数据库连通性…"
  $errFile = [System.IO.Path]::GetTempFileName()
  & $Engine connection-test 1> $null 2> $errFile
  if ($LASTEXITCODE -ne 0) {
    Warn "数据库连不上，先以演示数据启动。错误："
    Get-Content -LiteralPath $errFile | ForEach-Object { Write-Host "    $_" }
    $env:GAME_DB_LIVE = "0"
  } else {
    Info "数据库连接正常，核心表可读。"
  }
  Remove-Item -LiteralPath $errFile -Force -ErrorAction SilentlyContinue
} elseif (Test-Path -LiteralPath "data\database-connection.enc.json") {
  Info "已存在加密的数据库配置，服务会自动载入。"
} else {
  Info "还没配数据库。登录后进「规则与设置」页填连接信息，点「测试并保存」即可切到真实数据。"
}

# ---------- 5. 启动 ----------
$url = "http://127.0.0.1:$($env:RISK_PORT)/"
Info "控制台地址： $url"
Info "登录卡密：   $($env:RISK_PORTAL_KEY)"
Info "按 Ctrl+C 停止。"

# 服务起来后自动开浏览器（失败也不影响）
Start-Job -ScriptBlock {
  param($u)
  for ($i = 0; $i -lt 30; $i++) {
    try {
      Invoke-WebRequest -Uri $u -UseBasicParsing -TimeoutSec 2 | Out-Null
      Start-Process $u
      break
    } catch { Start-Sleep -Seconds 1 }
  }
} -ArgumentList $url | Out-Null

& node server.mjs

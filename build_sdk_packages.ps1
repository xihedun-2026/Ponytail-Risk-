param(
    [string]$LinuxLibrary = "dist\risk-sdk\linux-x86_64\librisk_sdk.so"
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$dist = Join-Path $root "dist\risk-sdk"
$header = Join-Path $root "crates\risk-sdk\include\ponytail_risk_sdk.h"
$check = Join-Path $root "crates\risk-sdk\tests\c_abi_check.c"
$windowsLibrary = Join-Path $root "target\release\risk_sdk.dll"
$windowsImport = Join-Path $root "target\release\risk_sdk.dll.lib"
$linuxLibraryPath = if ([IO.Path]::IsPathRooted($LinuxLibrary)) { $LinuxLibrary } else { Join-Path $root $LinuxLibrary }

foreach ($path in @($header, $check, $windowsLibrary, $windowsImport, $linuxLibraryPath)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing SDK artifact: $path" }
}

$windowsDir = Join-Path $dist "windows-x86_64"
$linuxDir = Join-Path $dist "linux-x86_64"
New-Item -ItemType Directory -Force -Path $windowsDir, $linuxDir | Out-Null

Copy-Item -LiteralPath $header -Destination (Join-Path $windowsDir "ponytail_risk_sdk.h") -Force
Copy-Item -LiteralPath $check -Destination (Join-Path $windowsDir "c_abi_check.c") -Force
Copy-Item -LiteralPath $windowsLibrary -Destination (Join-Path $windowsDir "risk_sdk.dll") -Force
Copy-Item -LiteralPath $windowsImport -Destination (Join-Path $windowsDir "risk_sdk.dll.lib") -Force
Copy-Item -LiteralPath $header -Destination (Join-Path $linuxDir "ponytail_risk_sdk.h") -Force
Copy-Item -LiteralPath $check -Destination (Join-Path $linuxDir "c_abi_check.c") -Force
if ((Resolve-Path -LiteralPath $linuxLibraryPath).Path -ne (Resolve-Path -LiteralPath (Join-Path $linuxDir "librisk_sdk.so") -ErrorAction SilentlyContinue).Path) {
    Copy-Item -LiteralPath $linuxLibraryPath -Destination (Join-Path $linuxDir "librisk_sdk.so") -Force
}

$windowsZip = Join-Path $dist "ponytail-risk-sdk-windows-x86_64.zip"
$linuxZip = Join-Path $dist "ponytail-risk-sdk-linux-x86_64.zip"
Compress-Archive -Path (Join-Path $windowsDir "*") -DestinationPath $windowsZip -Force
Compress-Archive -Path (Join-Path $linuxDir "*") -DestinationPath $linuxZip -Force

$checksumLines = Get-FileHash -Algorithm SHA256 -LiteralPath $windowsZip, $linuxZip | ForEach-Object {
    "{0}  {1}" -f $_.Hash.ToLowerInvariant(), (Split-Path -Leaf $_.Path)
}
[IO.File]::WriteAllText(
    (Join-Path $dist "SHA256SUMS.txt"),
    ($checksumLines -join "`n") + "`n",
    [Text.Encoding]::ASCII
)

Write-Host "SDK packages built:"
Get-Item -LiteralPath $windowsZip, $linuxZip | Select-Object FullName, Length, LastWriteTime

@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo.
echo   Ponytail Risk - starting...
echo   First run may install Node / Rust / C++ build tools (UAC prompt will appear).
echo.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-local.ps1" %*
if errorlevel 1 (
  echo.
  echo   Startup failed - read the messages above, or open install-log.txt
)
echo.
pause

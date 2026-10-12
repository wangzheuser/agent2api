@echo off
setlocal

cd /d "%~dp0"
set "AGENT2API_PROXY_HOME=%~dp0.dev-data"
set "AGENT2API_PROXY_PORT=3066"

where npm >nul 2>nul
if errorlevel 1 (
  echo npm was not found in PATH.
  exit /b 1
)

echo Starting Agent2API in native Windows development mode...
echo Data: %AGENT2API_PROXY_HOME%
echo Port: %AGENT2API_PROXY_PORT%
call npm run tauri:dev
exit /b %errorlevel%

@echo off
setlocal

cd /d "%~dp0"
set "AGENT2API_PROXY_HOME=%~dp0.dev-data"
set "AGENT2API_PROXY_PORT=3066"

rem Pin the build cache to the project root target/ so an external
rem CARGO_TARGET_DIR cannot send it somewhere else (e.g. a small C: drive).
set "CARGO_TARGET_DIR=%~dp0target"

rem Turn off incremental compilation and debug symbols. The dev profile
rem defaults to debuginfo=2 plus an incremental cache, which on this
rem mechanical disk writes tens of GB of intermediates and makes rustc
rem stall for a long time while compiling this crate. With them off the
rem artifacts are an order of magnitude smaller and the build completes.
set "CARGO_INCREMENTAL=0"
set "CARGO_PROFILE_DEV_DEBUG=0"

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

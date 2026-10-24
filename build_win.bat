@echo off
setlocal EnableExtensions

cd /d "%~dp0"

where npm >nul 2>nul
if errorlevel 1 (
  echo npm was not found in PATH.
  exit /b 1
)

if not exist "package.json" (
  echo This script must be run from the Agent2API project root.
  exit /b 1
)

echo Building the Windows NSIS installer...
call npm run tauri:build
if errorlevel 1 (
  echo Build failed.
  exit /b %errorlevel%
)

set "OUTPUT_DIR=%~dp0target\release\bundle\nsis"
if not exist "%OUTPUT_DIR%" (
  echo Build completed, but the NSIS output directory was not found:
  echo %OUTPUT_DIR%
  exit /b 1
)

for /f "delims=" %%F in ('dir /b /a:-d /o:-d "%OUTPUT_DIR%\*.exe" 2^>nul') do (
  echo Installer: %OUTPUT_DIR%\%%F
  exit /b 0
)

echo Build completed, but no EXE was found in:
echo %OUTPUT_DIR%
exit /b 1

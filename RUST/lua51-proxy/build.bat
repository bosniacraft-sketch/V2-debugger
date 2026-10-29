@echo off
cd /d "%~dp0"
rem Builds the 32-bit lua51.dll proxy for Victoria 2.
cargo build --release --target i686-pc-windows-msvc
if errorlevel 1 (
  echo.
  echo BUILD FAILED - see the error text above.
  pause
  exit /b 1
)
echo.
echo Built: %~dp0target\i686-pc-windows-msvc\release\lua51.dll
pause

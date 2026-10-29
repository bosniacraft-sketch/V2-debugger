@echo off
cd /d "%~dp0"
rem Builds the 32-bit lua51.dll proxy for Victoria 2 with Visual Studio (2019 or newer).
cmake -B build -A Win32
if errorlevel 1 goto failed
cmake --build build --config Release
if errorlevel 1 goto failed
echo.
echo Built: %~dp0build\Release\lua51.dll
pause
exit /b 0
:failed
echo.
echo BUILD FAILED - see the error text above.
pause
exit /b 1

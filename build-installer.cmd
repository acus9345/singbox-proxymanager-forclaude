@echo off
rem Builds both apps, then the one-file installer that embeds them.
rem Output: target\release\singbox-watch-setup.exe
cd /d "%~dp0"
cargo build --release || exit /b 1
cargo build --release -p singbox-watch-setup || exit /b 1
echo.
echo [ok] installer: %~dp0target\release\singbox-watch-setup.exe

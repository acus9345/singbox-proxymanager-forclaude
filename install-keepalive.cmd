@echo off
rem Creates the per-minute hidden keep-alive task (runs keep-alive.cmd next
rem to this file). Run once from the deployment folder; re-run freely.
schtasks /Create /TN "singbox-apps-keepalive" /TR "wscript.exe \"%~dp0hidden.vbs\"" /SC MINUTE /MO 1 /F
echo.
echo [ok] keep-alive task installed.
echo      Disable it during maintenance: schtasks /Change /TN singbox-apps-keepalive /DISABLE
pause

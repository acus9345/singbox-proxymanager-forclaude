@echo off
rem Resurrects proxy-tray, singbox-watch and (when the proxy is meant to be
rem ON) the tunnel task itself. Launched hidden every minute by the scheduled
rem task created by install-keepalive.cmd. Edit TASK to match your setup.
set TASK=sing-box-tun

tasklist /FI "IMAGENAME eq proxy-tray.exe" 2>nul | find /I "proxy-tray.exe" >nul || explorer.exe "%~dp0proxy-tray.exe"
tasklist /FI "IMAGENAME eq singbox-watch.exe" 2>nul | find /I "singbox-watch.exe" >nul || explorer.exe "%~dp0singbox-watch.exe"

rem Only resurrect the tunnel when the proxy is NOT deliberately off.
set STATE=%~dp0proxy-state.txt
if not exist "%STATE%" set STATE=%USERPROFILE%\.singbox-watch\proxy-state.txt
find /I "off" "%STATE%" >nul 2>&1
if errorlevel 1 (
  tasklist /FI "IMAGENAME eq sing-box.exe" 2>nul | find /I "sing-box.exe" >nul || schtasks /Run /TN "%TASK%" >nul 2>&1
)

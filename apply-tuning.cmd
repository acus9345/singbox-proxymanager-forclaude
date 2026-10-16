@echo off
rem Optional tuning script, launched elevated (UAC) by proxy-tray's
rem "Apply tuning" menu item and by defaults enforcement.
rem Edit TASK to match your tunnel's scheduled task name.
set TASK=sing-box-tun

echo === BBR2 congestion control (steadier throughput on long, lossy links) ===
netsh int tcp set supplemental Template=Internet CongestionProvider=BBR2

echo === WebRTC local-IP-leak policy, machine-wide (Chrome / Edge / Brave) ===
reg add "HKLM\SOFTWARE\Policies\Microsoft\Edge" /v WebRtcIPHandling /t REG_SZ /d disable_non_proxied_udp /f
reg add "HKLM\SOFTWARE\Policies\BraveSoftware\Brave" /v WebRtcIPHandling /t REG_SZ /d disable_non_proxied_udp /f
reg add "HKLM\SOFTWARE\Policies\Google\Chrome" /v WebRtcIPHandling /t REG_SZ /d disable_non_proxied_udp /f

echo === Restarting the tunnel so new connections use the new rules ===
schtasks /End /TN "%TASK%" >nul 2>&1
taskkill /F /IM sing-box.exe >nul 2>&1
schtasks /Run /TN "%TASK%"
echo Done.

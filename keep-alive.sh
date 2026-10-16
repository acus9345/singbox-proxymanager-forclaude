#!/bin/sh
# Resurrects the tray app and (when the proxy is meant to be ON) the tunnel
# service. Launched every 60s by com.singbox.watch.keepalive.plist.
# Edit SERVICE to match your sing-box launchd label.
SERVICE="com.singbox.tun"
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
STATE="$APP_DIR/proxy-state.txt"
[ -f "$STATE" ] || STATE="$HOME/.singbox-watch/proxy-state.txt"

pgrep -f "singbox-watch-mac" >/dev/null 2>&1 || (cd "$APP_DIR" && ./singbox-watch-mac >/dev/null 2>&1 &)

# only resurrect the tunnel when the proxy is NOT deliberately off
if ! grep -qi "off" "$STATE" 2>/dev/null; then
  if ! pgrep -x sing-box >/dev/null 2>&1; then
    launchctl kickstart -k "system/$SERVICE" >/dev/null 2>&1
  fi
fi

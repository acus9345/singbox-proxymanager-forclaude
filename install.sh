#!/bin/sh
# One-shot installer: installs the keep-alive LaunchAgent (60 s) and the
# tray-app LaunchAgent (RunAtLoad + KeepAlive). Re-run freely.
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
AGENTS="$HOME/Library/LaunchAgents"
mkdir -p "$AGENTS"

# keep-alive every minute, hidden
cat > "$AGENTS/com.singbox.watch.keepalive.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.singbox.watch.keepalive</string>
  <key>ProgramArguments</key>
  <array><string>/bin/sh</string><string>$DIR/keep-alive.sh</string></array>
  <key>StartInterval</key><integer>60</integer>
</dict>
</plist>
EOF

# tray app at login (the binary also has its own --install)
"$DIR/singbox-watch-mac" --install

launchctl bootstrap "gui/$(id -u)" "$AGENTS/com.singbox.watch.keepalive.plist" 2>/dev/null || \
launchctl load "$AGENTS/com.singbox.watch.keepalive.plist" 2>/dev/null || true

echo "[ok] keep-alive installed (every 60 s) + tray LaunchAgent written."
echo "    Start the tray now:  $DIR/singbox-watch-mac"

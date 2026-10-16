# singbox-watch (macOS edition)

macOS port of singbox-watch — a menu-bar controller, kill-switch and
watchdog for a [sing-box](https://sing-box.sagernet.org) tunnel with a
**static exit IP**.

> **Status: source-first release.** This port was written against the
> documented APIs of `tao` + `tray-icon` and follows the battle-tested
> Windows edition closely, but it has **not yet been compiled or run on
> macOS** — the first `cargo build --release` on a Mac is the remaining
> validation step, and minor crate-version adjustments may be needed.
> The behavior spec below is the contract.

## What it does (parity with the Windows edition)

- **Menu-bar switch** — green icon when the public exit IP matches the
  pinned IP in `expected-exit-ip.txt`, red when it doesn't; tooltip shows
  the live exit IP and an optional renewal countdown. Left-click toggles.
- **Toggle-off kills first** — matched applications are closed before the
  tunnel stops (`ps` + `kill -9`, sub-second), so nothing slips out through
  the raw connection during the transition.
- **Keep-closed policy (100 ms detection)** — while the proxy is off,
  matched apps are closed within a tenth of a second of appearing, and one
  dialog asks per off-episode: *keep them closed?* Unanswered = keep closed
  (safe default). **No** switches to auto-enable behavior.
- **Auto-enable guard** — matched apps are suspended (`SIGSTOP`), the tunnel
  is started, and they are resumed (`SIGCONT`) only after the exit IP is
  verified. If the tunnel can't come up, they stay suspended: fail closed.
- **Doctor** — `--doctor` reports exit IP vs pinned IP, sing-box process,
  launchd service, keep-alive agent, per-browser WebRTC policy, renewal
  countdown.
- **Defaults enforcement** — Chromium WebRTC leak policy
  (`WebRtcIPHandling=disable_non_proxied_udp`) verified after every enable;
  drift is auto-fixed via `defaults write` (no admin needed).
- **Keep-alive** — a 60 s LaunchAgent resurrects the tray app and (when the
  proxy is meant to be on) the tunnel service.
- macOS notifications announce state changes (in place of the Windows
  always-on-top banner).

## macOS-specific notes

- **Privileges**: sing-box TUN needs root, so start/stop goes through
  `launchctl` invoked via `osascript … with administrator privileges` —
  expect a password prompt on each toggle (the macOS equivalent of UAC).
- **Tunnel service**: point `SINGBOX_WATCH_SERVICE` (env) at your launchd
  label; default `com.singbox.tun`.
- **Kill matching**: processes whose executable is `claude`, or whose command
  line contains `claude-code` (override: `SINGBOX_WATCH_KILL_MATCH`), minus
  exclusions (`SINGBOX_WATCH_KILL_EXCLUDE`, comma-separated).
- **No DNS-cache watcher** on macOS (no public equivalent API); the
  auto-enable guard relies on process detection.
- **No BBR2**: macOS does not expose user-configurable TCP congestion
  control; that check from the Windows doctor is intentionally absent.

## Setup

```sh
cargo build --release
mkdir -p ~/singbox-watch && cp target/release/singbox-watch-mac ~/singbox-watch/
cp keep-alive.sh install.sh ~/singbox-watch/
chmod +x ~/singbox-watch/*.sh ~/singbox-watch/singbox-watch-mac

# pin your exit IP
echo "203.0.113.7" > ~/singbox-watch/expected-exit-ip.txt

# optional renewal countdown
echo "2026-11-11" > ~/singbox-watch/renew-deadline.txt

# autostart + keep-alive
~/singbox-watch/install.sh
~/singbox-watch/singbox-watch-mac            # start it now
```

## CLI

```
singbox-watch-mac                run (menu-bar icon)
singbox-watch-mac --doctor       health report, exit 0 = all good
singbox-watch-mac --state-once   print tunnel state and exit
singbox-watch-mac --kill-list    dry run: list kill targets
singbox-watch-mac --kill-now     kill matched processes immediately
singbox-watch-mac --install / --uninstall    LaunchAgent autostart
```

## Files

| file | purpose |
|---|---|
| `expected-exit-ip.txt` | pinned public IPv4 — the ground truth |
| `proxy-state.txt` | written by the app: `on` / `off` intent |
| `renew-deadline.txt` | optional `YYYY-MM-DD` proxy expiry for the tooltip countdown |
| `singbox-watch.log` | everything the app does |

Files are looked up next to the binary first, then `~/.singbox-watch/`.

## License

MIT — see [LICENSE](LICENSE).

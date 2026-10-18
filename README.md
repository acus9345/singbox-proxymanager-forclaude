# singbox-watch

A Windows tray controller, watchdog and fail-safe toolkit for a
[sing-box](https://sing-box.sagernet.org) tunnel with a **static exit IP**.
BETTER PUT IT BESIDES SINGBOX.

If your machine routes all traffic through a SOCKS5/TUN tunnel and you want
to be certain it never silently falls back to the raw connection, this is
that toolkit. It was built the hard way — every feature corresponds to a
failure that actually happened.

Two small binaries (Rust, winapi only, no GUI framework):

| binary | what it does |
|---|---|
| `proxy-tray.exe` | tray on/off switch, auto-enable guard, sub-second app kill-switch, health check (`--doctor`), keep-alive trigger, defaults enforcement |
| `singbox-watch.exe` | always-on-top warning banner when the exit IP stops matching the pinned IP |

## Features
- **to use some of the features go to show hidden icons button on ur task bar and right click our app icon**
- **Tray switch** — green icon when the exit IP matches the pinned IP, red
  when it doesn't. Left-click toggles the tunnel (starts/stops a scheduled
  task; a UAC prompt appears when elevation is needed). The tooltip always
  shows the live exit IP and an optional renewal countdown.
- **Fail-closed app kill-switch** — turning the proxy off first kills every
  matched application process using a native path (process snapshot +
  `TerminateProcess`, command lines read straight from each process's PEB —
  no PowerShell in the hot path). Measured: under 100 ms.
- **Always-on-top banner** — red "TUNNEL BYPASSED" banner when the exit IP
  changes unexpectedly; amber for intentional off / no connectivity.
  Left-click snoozes, right-click quits.
- **Keep-closed policy (100 ms detection)** — while the proxy is off, matched
  applications are closed within a tenth of a second of appearing, and a
  topmost Yes/No dialog asks (once per off-episode): *keep them closed?*
  Unanswered = keep closed (the safe default). **Yes** keeps closing them on
  sight; **No** switches to the auto-enable behavior below.
- **Auto-enable guard** — if a matched app starts (or a fresh
  DNS lookup for a watched domain occurs) while the tunnel is down, the
  app processes are suspended, the tunnel is started, and they are resumed
  only after the exit IP is verified. If the tunnel can't come up, they stay
  suspended: fail closed, never leak.
- **Keep-alive** — a hidden per-minute scheduled task resurrects the tray
  app, the banner watcher, and the tunnel itself (only when the proxy is
  meant to be on). A crashed tunnel self-heals in under a minute.
- **Doctor** — `proxy-tray.exe --doctor` prints a full health report: exit
  IP vs pinned IP, processes, keep-alive task, autostart entries, TCP
  congestion control, per-browser WebRTC leak policy, renewal countdown.
- **Defaults enforcement** — every proxy enable verifies BBR2 congestion
  control and the WebRTC leak policy; drift triggers the tuning script
  (UAC, max once per 30 minutes).
- **Tuning script** — optional `apply-tuning.cmd` switches Windows to BBR2,
  applies the machine-wide WebRTC `disable_non_proxied_udp` policy for
  Chrome/Edge/Brave, and restarts the tunnel.

## Requirements

- mac i guess
- [Rust](https://rustup.rs) (to build) — `cargo build --release`
- sing-box installed and runnable via a **scheduled task** (default name
  `sing-box-tun`), e.g.:

  ```
  schtasks /Create /TN sing-box-tun /TR "wscript.exe C:\path\to\run-hidden.vbs" /SC ONLOGON /RL HIGHEST /F
  ```

## Setup

1. Build and lay out the deployment folder:

   ```
   cargo build --release
   mkdir %USERPROFILE%\singbox-watch
   copy target\release\*.exe %USERPROFILE%\singbox-watch\
   copy apply-tuning.cmd keep-alive.cmd hidden.vbs install-keepalive.cmd %USERPROFILE%\singbox-watch\
   ```

2. Pin your exit IP — create `expected-exit-ip.txt` in that folder
   containing exactly your tunnel's public IPv4. This file is the ground
   truth every check compares against.

3. Optional files (same folder):

   | file | effect |
   |---|---|
   | `renew-deadline.txt` | `YYYY-MM-DD` proxy expiry — tooltip countdown, turns into a "RENEW NOW" nag in the last 5 days |
   | `apply-tuning.cmd` | enables the tuning menu item + defaults enforcement repair |

4. Install autostart + keep-alive (from the deployment folder):

   ```
   proxy-tray.exe --install       % rem starts with Windows
   singbox-watch.exe --install
   install-keepalive.cmd          % rem per-minute resurrection task
   ```

5. Right-click the tray icon → pin it to the taskbar so it's always visible.

## Configuration

Everything is file/env based — no config file to maintain:

| env var | default | meaning |
|---|---|---|
| `SINGBOX_WATCH_TASK` | `sing-box-tun` | scheduled task used to start/stop the tunnel |
| `SINGBOX_WATCH_KILL_EXCLUDE` | *(empty)* | comma-separated substrings; node.exe command lines containing any of them are never killed (protect your own agent/editor runtimes) |

State files (`expected-exit-ip.txt`, `proxy-state.txt`) are looked up next
to the exe first, then `%USERPROFILE%\.singbox-watch\`. Logs and temp files
land next to the exe (`proxy-tray.log`).

### What gets killed

By design this is a kill-switch for tools that must never talk to the
network from the raw connection:

- `claude.exe` (any program with that image name), and
- `node.exe` whose command line contains `claude-code`, minus exclusions.

If you use it for other apps, adjust `list_claude_targets()` /
`kill_claude_blocking()` in `src/bin/proxy-tray.rs` — the matching logic is
in one place.

## CLI

```
proxy-tray.exe                run (tray icon)
proxy-tray.exe --doctor       full health report, exit 0 = all good
proxy-tray.exe --state-once   print tunnel state and exit
proxy-tray.exe --kill-list    dry run: list everything the kill-switch would target
proxy-tray.exe --kill-now     kill matched processes immediately
proxy-tray.exe --install / --uninstall   login autostart
singbox-watch.exe             run (banner watcher)
singbox-watch.exe --check-once / --test-banner / --ip <addr> / --install / --uninstall
```

## Design notes / gotchas (all learned the hard way)

- **Pin the icon once**: the tray icon is updated with `NIM_MODIFY` on state
  changes specifically because delete+re-add resets Windows taskbar pinning.
- **GUI subsystem**: both binaries are built with `#![windows_subsystem =
  "windows"]` — a console window is closable, and closing it kills the
  process. (That took an incident to learn.)
- **Don't launch these from an editor/agent shell**: child processes share
  that shell's job object and die with it. Launch via `explorer.exe <path>`
  or the Run-key autostart.
- **Zombie tray icons**: after a UAC secure-desktop switch, Windows can
  keep a dead icon entry that fails every update with `E_FAIL`. Recovery is
  delete-then-re-add, and any failure re-arms the 5-second retry.
- **Fail-closed over convenience**: when the tunnel can't be verified after
  an auto-enable, matched processes stay suspended instead of being resumed
  into a leaking network.
- The DNS-cache watcher (auto-enable on fresh `claude.ai`/`anthropic.com`
  lookups) is best-effort: browsers with DoH/"secure DNS" bypass the system
  resolver.

## Limitations

- Windows-only by design (winapi, tray, scheduled tasks, registry).
- Assumes a single static exit IP; multi-exit setups need their own logic.
- The elevated stop/start paths rely on `schtasks`; a task running at
  HIGHEST integrity needs a UAC prompt to be stopped from a standard context.

## License

MIT — see [LICENSE](LICENSE).

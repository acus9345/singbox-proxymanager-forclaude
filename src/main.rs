// singbox-watch-mac — menu-bar switch, kill-switch and watchdog for a
// sing-box tunnel with a static exit IP. macOS port of the Windows tool.
//
// STATUS: written and reviewed off-platform — the FIRST BUILD must happen on
// a Mac (`cargo build --release`); minor crate-API adjustments may be needed.
//
// Behaviour parity with the Windows edition:
//   - menu-bar icon: green = exit IP matches expected-exit-ip.txt, red = off
//   - left-click toggles the tunnel (launchd service; admin prompt on mac)
//   - toggle-off kills matched apps FIRST (ps/kill, sub-second)
//   - keep-closed policy: 100 ms detection while off, kill + ONE dialog per
//     off-episode ("keep closed?" — unanswered/default = keep closed)
//   - "allow" mode: matched apps suspend (SIGSTOP) -> tunnel start -> verify
//     exit IP -> resume (SIGCONT); stay stopped if the tunnel can't come up
//   - doctor / state-once / kill-list / kill-now CLI flags
//   - notifications (osascript) on state changes instead of a topmost banner
//
// Files: expected-exit-ip.txt, proxy-state.txt, renew-deadline.txt (optional,
// YYYY-MM-DD), singbox-watch.log — next to the binary, or ~/.singbox-watch/.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tao::event::Event::{NewEvents, UserEvent};
use tao::event::StartCause;
use tao::event_loop::{ControlFlow, EventLoop};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

// launchd service label that runs sing-box (edit or export SINGBOX_WATCH_SERVICE)
fn service_label() -> String {
    env::var("SINGBOX_WATCH_SERVICE").unwrap_or_else(|_| "com.singbox.tun".to_string())
}

fn base_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn state_root() -> PathBuf {
    let b = base_dir();
    if b.join("expected-exit-ip.txt").exists() {
        return b;
    }
    if let Ok(home) = env::var("HOME") {
        let h = PathBuf::from(home).join(".singbox-watch");
        let _ = std::fs::create_dir_all(&h);
        return h;
    }
    b
}

fn expected_file() -> PathBuf {
    state_root().join("expected-exit-ip.txt")
}

fn state_file() -> PathBuf {
    state_root().join("proxy-state.txt")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn log_line(msg: &str) {
    use std::io::Write;
    let path = base_dir().join("singbox-watch.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {msg}", now());
    }
}

// ---------- exit IP (plain HTTP, works through the tunnel and when bypassed) ----------

fn valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.trim().split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
        })
}

fn http_get(host: &str, path: &str) -> Option<String> {
    let mut s = TcpStream::connect((host, 80)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(6))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(6))).ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: singbox-watch/1.0\r\nAccept: */*\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.split("\r\n\r\n").nth(1).map(|s| s.to_string())
}

fn fetch_public_ip() -> Option<String> {
    let body = http_get("ip-api.com", "/line?fields=query")?;
    let ip = body.trim();
    if valid_ipv4(ip) {
        Some(ip.to_string())
    } else {
        None
    }
}

fn read_expected_ip() -> Option<String> {
    std::fs::read_to_string(expected_file())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| valid_ipv4(t))
}

fn proxy_state_off() -> bool {
    std::fs::read_to_string(state_file())
        .map(|t| t.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

fn set_proxy_state(on: bool) {
    let _ = std::fs::write(state_file(), if on { "on" } else { "off" });
}

// ---------- process matching / kill / suspend (ps + signals) ----------

fn sh(cmd: &str) -> Option<String> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

fn sh_ok(cmd: &str) -> bool {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kill_exclusions() -> Vec<String> {
    env::var("SINGBOX_WATCH_KILL_EXCLUDE")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// (pid, command line) of kill targets: image "claude" by name, or any
/// process whose command line contains the match string minus exclusions.
fn matched_processes() -> Vec<(i32, String)> {
    let mut out = Vec::new();
    let excl = kill_exclusions();
    let match_str = env::var("SINGBOX_WATCH_KILL_MATCH")
        .unwrap_or_else(|_| "claude-code".to_string())
        .to_lowercase();
    if let Some(ps) = sh("ps -axo pid=,command=") {
        for line in ps.lines() {
            let line = line.trim_start();
            let Some((pid_s, rest)) = line.split_once(' ') else { continue };
            let Ok(pid) = pid_s.trim().parse::<i32>() else { continue };
            if pid == std::process::id() as i32 {
                continue;
            }
            let cmd = rest.trim();
            let argv0 = cmd.split_whitespace().next().unwrap_or("");
            let image = argv0.rsplit('/').next().unwrap_or(argv0);
            let lower = cmd.to_lowercase();
            let hit = image == "claude"
                || (lower.contains(&match_str) && !excl.iter().any(|x| lower.contains(x)));
            if hit {
                out.push((pid, cmd.to_string()));
            }
        }
    }
    out
}

fn kill_matched() -> usize {
    let targets = matched_processes();
    for (pid, _) in &targets {
        let _ = Command::new("/bin/kill").arg("-9").arg(pid.to_string()).status();
    }
    targets.len()
}

fn signal_all(pids: &[i32], sig: &str) {
    for pid in pids {
        let _ = Command::new("/bin/kill").arg(sig).arg(pid.to_string()).status();
    }
}

// ---------- tunnel control (launchd; TUN needs root → admin prompt) ----------

fn admin_shell(cmd: &str) -> bool {
    let escaped = cmd.replace('\\', "\\\\").replace('\"', "\\\"");
    sh_ok(&format!(
        "osascript -e 'do shell script \"{escaped}\" with administrator privileges' >/dev/null 2>&1"
    ))
}

fn service_running() -> bool {
    sh_ok(&format!(
        "launchctl print system/{} >/dev/null 2>&1 || launchctl list {} >/dev/null 2>&1",
        service_label(),
        service_label()
    ))
}

fn start_tunnel() {
    log_line("start: launchd kickstart (admin prompt)");
    let label = service_label();
    let ok = admin_shell(&format!(
        "launchctl kickstart -k system/{label} 2>/dev/null || launchctl bootstrap system /Library/LaunchDaemons/{label}.plist"
    ));
    log_line(if ok { "start: service kickstarted" } else { "start: FAILED (check service label / plist)" });
}

fn stop_tunnel() {
    log_line("stop: launchd bootout (admin prompt)");
    let label = service_label();
    let _ = admin_shell(&format!("launchctl bootout system/{label} 2>/dev/null"));
    let _ = Command::new("/usr/bin/pkill").arg("-9").arg("sing-box").status();
    thread::sleep(Duration::from_millis(800));
    log_line(if service_running() {
        "stop: FAILED — service still running"
    } else {
        "stop: service stopped"
    });
}

fn wait_for_tunnel(expected: &str, max_secs: u32) -> bool {
    for _ in 0..(max_secs * 2) {
        thread::sleep(Duration::from_millis(500));
        if let Some(ip) = fetch_public_ip() {
            if !expected.is_empty() && ip == expected {
                return true;
            }
        }
    }
    false
}

// ---------- notifications / dialogs (osascript) ----------

fn notify(title: &str, body: &str) {
    let t = title.replace('\"', "'");
    let b = body.replace('\"', "'");
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(format!("display notification \"{b}\" with title \"{t}\""))
        .spawn();
}

/// true = keep closed, false = allow. Runs in its own thread; called once
/// per off-episode.
fn ask_keep_closed() {
    thread::spawn(|| {
        log_line("[ask] proxy off + matched apps closed — asking whether to keep them closed");
        // SAFE DEFAULT FIRST: until answered (or if the dialog can't show),
        // the policy stays "keep closed".
        let out = Command::new("osascript")
            .arg("-e")
            .arg("display dialog \"The proxy is OFF, so matched applications were closed to keep them off the raw connection.\\n\\nKeep them closed while the proxy stays off?\\n\\nYes — they stay closed (relaunches are closed again automatically)\\nNo — allow them to run (the proxy will re-enable itself when they start)\" with title \"singbox-watch — proxy is off\" buttons {\"No — allow\", \"Yes — keep closed\"} default button \"Yes — keep closed\" with icon caution")
            .output();
        let allow = match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_lowercase().contains("no — allow"),
            Err(_) => false,
        };
        if allow {
            KEEP_CLOSED.store(2, Ordering::Relaxed);
            log_line("[ask] answer: allow (auto-enable on start)");
        } else {
            KEEP_CLOSED.store(1, Ordering::Relaxed);
            log_line("[ask] answer: keep closed");
        }
    });
}

// ---------- shared state ----------

// actual tunnel state: 0 unknown, 1 on, 2 off
static ACTUAL: AtomicU8 = AtomicU8::new(0);
static EXPECTED: OnceLock<String> = OnceLock::new();
// keep-closed policy: 1 = keep closed (safe default), 2 = allow (auto-enable)
static KEEP_CLOSED: AtomicU8 = AtomicU8::new(1);
static ASKED_EPISODE: AtomicU8 = AtomicU8::new(0);
static EPISODE_TRIGGERED: AtomicU8 = AtomicU8::new(0);
static TOGGLE_BUSY: AtomicU8 = AtomicU8::new(0);
static TUNE_LAST: AtomicI64 = AtomicI64::new(0);

enum UiMsg {
    RefreshIcon,
}

fn renew_tag() -> String {
    let Ok(txt) = std::fs::read_to_string(base_dir().join("renew-deadline.txt")) else {
        return String::new();
    };
    let mut it = txt.trim().split('-');
    let (Some(y), Some(m), Some(d)) = (it.next(), it.next(), it.next()) else {
        return String::new();
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>()) else {
        return String::new();
    };
    // days-from-civil (Howard Hinnant)
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let deadline = (era * 146097 + doe - 719468) * 86400;
    let nowt = now();
    if nowt >= deadline {
        return " · IP EXPIRED — renew your proxy!".to_string();
    }
    let days = (deadline - nowt) / 86400 + 1;
    if nowt >= deadline - 5 * 86400 {
        return format!(" · RENEW PROXY NOW ({days}d left)");
    }
    format!(" · renew in {days}d")
}

fn make_rgba(on: bool) -> Vec<u8> {
    const S: usize = 32;
    let mut buf = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let i = (y * S + x) * 4;
            let (mut r, mut g, mut b) = if on { (10, 185, 70) } else { (200, 45, 45) };
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            let white = if on {
                let d = ((dx * dx + dy * dy) as f32).sqrt();
                let ring = d >= 9.0 && d <= 12.0;
                let gap = dy < -6 && dx.abs() <= 4;
                let bar = dx.abs() <= 2 && dy >= -13 && dy <= -4;
                (ring && !gap) || bar
            } else {
                let adx = dx.abs();
                let ady = dy.abs();
                let m = adx.max(ady);
                (adx - ady).abs() <= 2 && m >= 5 && m <= 11
            };
            if white {
                r = 255;
                g = 255;
                b = 255;
            }
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = 255;
        }
    }
    buf
}

fn icon_for(on: bool) -> Option<Icon> {
    Icon::from_rgba(make_rgba(on), 32, 32).ok()
}

// ---------- worker threads ----------

fn poll_loop(tx: Sender<UiMsg>) {
    let mut last = 255u8;
    loop {
        let expected = EXPECTED.get().cloned().unwrap_or_default();
        let st = match fetch_public_ip() {
            Some(ip) if !expected.is_empty() && ip == expected => 1u8,
            Some(_) => 2u8,
            None => 0u8,
        };
        let prev = ACTUAL.swap(st, Ordering::Relaxed);
        let _ = prev;
        if st != last {
            let _ = tx.send(UiMsg::RefreshIcon);
            match st {
                1 => {
                    notify("singbox-watch", "Tunnel is UP — exit IP verified");
                    log_line("poll: tunnel UP");
                }
                2 => {
                    notify("singbox-watch", "Tunnel is DOWN — traffic is exiting raw");
                    log_line("poll: tunnel DOWN (raw exit)");
                }
                _ => {}
            }
            last = st;
        }
        thread::sleep(Duration::from_secs(10));
    }
}

struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        TOGGLE_BUSY.store(0, Ordering::Release);
    }
}

fn toggle() {
    if TOGGLE_BUSY.swap(1, Ordering::AcqRel) == 1 {
        log_line("toggle: ignored — another toggle in progress");
        return;
    }
    thread::spawn(|| {
        let _busy = BusyGuard;
        if ACTUAL.load(Ordering::Relaxed) != 1 {
            log_line("toggle: turning ON");
            set_proxy_state(true);
            enforce_defaults();
            start_tunnel();
            let expected = EXPECTED.get().cloned().unwrap_or_default();
            if wait_for_tunnel(&expected, 30) {
                log_line("toggle: tunnel verified up");
            } else {
                log_line("toggle: tunnel did NOT verify within 15s");
            }
        } else {
            log_line("toggle: turning OFF (kill matched apps first)");
            set_proxy_state(false);
            let n = kill_matched();
            log_line(&format!("toggle: fast-killed {n} matched process(es)"));
            thread::spawn(|| {
                // async sweep for anything the fast pass missed
                let _ = sh_ok("pkill -9 -f 'claude-code' 2>/dev/null");
            });
            stop_tunnel();
            if n > 0 && ASKED_EPISODE.swap(1, Ordering::Relaxed) == 0 {
                ask_keep_closed();
            }
        }
        log_line("toggle: actions complete");
    });
}

/// "allow" mode: suspend matched apps, start the tunnel, resume only after
/// the exit IP is verified; stay suspended otherwise (fail closed).
fn auto_enable() {
    if TOGGLE_BUSY.swap(1, Ordering::AcqRel) == 1 {
        return;
    }
    EPISODE_TRIGGERED.store(1, Ordering::Relaxed);
    thread::spawn(|| {
        let _busy = BusyGuard;
        log_line("[guard] matched app while tunnel down — auto-enabling (suspended)");
        let pids: Vec<i32> = matched_processes().iter().map(|(p, _)| *p).collect();
        signal_all(&pids, "-STOP");
        set_proxy_state(true);
        start_tunnel();
        let expected = EXPECTED.get().cloned().unwrap_or_default();
        if wait_for_tunnel(&expected, 60) {
            signal_all(&pids, "-CONT");
            log_line("[guard] tunnel verified up — resumed");
        } else if pids.is_empty() {
            log_line("[guard] tunnel did NOT come up (nothing suspended)");
        } else {
            log_line("[guard] tunnel did NOT come up — left suspended (safe)");
        }
    });
}

// macOS defaults: WebRTC leak policy per Chromium browser (user-level).
fn webrtc_all_blocked() -> bool {
    for dom in ["com.google.Chrome", "com.microsoft.edgemac", "com.brave.Browser"] {
        let v = sh(&format!("defaults read {dom} WebRtcIPHandling 2>/dev/null"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if v != "disable_non_proxied_udp" {
            return false;
        }
    }
    true
}

fn enforce_defaults() {
    if webrtc_all_blocked() {
        log_line("[defaults] verified: WebRTC blocked");
        return;
    }
    if now() - TUNE_LAST.load(Ordering::Relaxed) < 1800 {
        log_line("[defaults] WebRTC drift detected, prompt on cooldown (30 min)");
        return;
    }
    TUNE_LAST.store(now(), Ordering::Relaxed);
    log_line("[defaults] WebRTC drifted — applying browser defaults");
    for dom in ["com.google.Chrome", "com.microsoft.edgemac", "com.brave.Browser"] {
        let _ = sh_ok(&format!("defaults write {dom} WebRtcIPHandling disable_non_proxied_udp"));
    }
}

fn guard_loop() {
    log_line("[guard] watching for matched processes (100 ms tick)");
    loop {
        if ACTUAL.load(Ordering::Relaxed) == 1 {
            EPISODE_TRIGGERED.store(0, Ordering::Relaxed);
            KEEP_CLOSED.store(1, Ordering::Relaxed);
            ASKED_EPISODE.store(0, Ordering::Relaxed);
        } else {
            let targets = matched_processes();
            if !targets.is_empty() {
                if KEEP_CLOSED.load(Ordering::Relaxed) == 1 {
                    let n = kill_matched();
                    if n > 0 {
                        log_line(&format!("[guard] proxy off — closed {n} matched process(es)"));
                    }
                    if ASKED_EPISODE.swap(1, Ordering::Relaxed) == 0 {
                        ask_keep_closed();
                    }
                } else if EPISODE_TRIGGERED.load(Ordering::Relaxed) == 0 {
                    auto_enable();
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ---------- doctor ----------

fn doctor() {
    let expected = read_expected_ip().unwrap_or_default();
    let mut fails = 0;
    let mut out: Vec<String> = Vec::new();
    {
        let mut add = |ok: bool, s: String| {
            if !ok {
                fails += 1;
            }
            out.push(s);
        };

        match fetch_public_ip() {
            Some(ip) if !expected.is_empty() && ip == expected => {
                add(true, format!("tunnel     : UP — exit {ip} matches pinned IP ✓"));
            }
            Some(ip) => add(false, format!("tunnel     : WRONG EXIT IP {ip} (expected {expected}) ✗")),
            None => add(false, "tunnel     : OFFLINE — checker unreachable ✗".to_string()),
        }
        let sb = sh("pgrep -x sing-box >/dev/null && echo yes")
            .map(|s| s.trim() == "yes")
            .unwrap_or(false);
        add(sb, format!("sing-box   : {}", if sb { "running ✓" } else { "NOT running ✗" }));
        add(
            service_running(),
            format!("launchd    : service {}", if service_running() { "loaded ✓" } else { "NOT loaded ✗" }),
        );
        let ka = PathBuf::from(env::var("HOME").unwrap_or_default())
            .join("Library/LaunchAgents/com.singbox.watch.keepalive.plist")
            .exists();
        add(ka, format!("keep-alive : {}", if ka { "LaunchAgent present ✓" } else { "missing (run install.sh) ✗" }));
        add(
            webrtc_all_blocked(),
            format!(
                "webrtc     : {}",
                if webrtc_all_blocked() { "blocked ✓" } else { "LEAKS — run: defaults write <browser> WebRtcIPHandling disable_non_proxied_udp ✗" }
            ),
        );
        match std::fs::read_to_string(base_dir().join("renew-deadline.txt")) {
            Err(_) => add(true, "renewal    : no renew-deadline.txt — countdown disabled ✓".to_string()),
            Ok(_) => {
                let tag = renew_tag();
                let ok = !tag.contains("RENEW") && !tag.contains("EXPIRED");
                add(ok, format!("renewal    : {tag}"));
            }
        }
    }

    for line in &out {
        println!("{line}");
        log_line(&format!("[doctor] {line}"));
    }
    println!("RESULT     : {fails} issue(s)");
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

// ---------- autostart (LaunchAgent) ----------

fn autostart_plist() -> String {
    let exe = env::current_exe().unwrap_or_default().display().to_string();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>com.singbox.watch.tray</string>\n  <key>ProgramArguments</key>\n  <array><string>{exe}</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n</dict>\n</plist>\n"
    )
}

fn install_autostart() {
    let dest = PathBuf::from(env::var("HOME").unwrap_or_default())
        .join("Library/LaunchAgents/com.singbox.watch.tray.plist");
    let _ = std::fs::create_dir_all(dest.parent().unwrap());
    if std::fs::write(&dest, autostart_plist()).is_ok() {
        println!("LaunchAgent written: {}", dest.display());
        println!("Load it now:  launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.singbox.watch.tray.plist");
    } else {
        eprintln!("failed to write LaunchAgent");
    }
}

fn uninstall_autostart() {
    let _ = sh_ok("launchctl bootout gui/$UID/com.singbox.watch.tray 2>/dev/null");
    let dest = PathBuf::from(env::var("HOME").unwrap_or_default())
        .join("Library/LaunchAgents/com.singbox.watch.tray.plist");
    let _ = std::fs::remove_file(dest);
    println!("LaunchAgent removed");
}

// ---------- main ----------

fn main() {
    let args: Vec<String> = env::args().collect();
    let _ = EXPECTED.set(read_expected_ip().unwrap_or_default());

    if args.iter().any(|a| a == "--doctor") {
        doctor();
        return;
    }
    if args.iter().any(|a| a == "--state-once") {
        let exp = read_expected_ip().unwrap_or_default();
        println!("expected: {}", if exp.is_empty() { "<none>" } else { &exp });
        match fetch_public_ip() {
            Some(ip) => println!("exit IP : {ip}\nstate   : {}", if !exp.is_empty() && ip == exp { "ON" } else { "OFF" }),
            None => println!("state   : UNKNOWN (no connectivity)"),
        }
        return;
    }
    if args.iter().any(|a| a == "--kill-list") {
        for (pid, cmd) in matched_processes() {
            let short: String = cmd.chars().take(120).collect();
            println!("pid {pid}  {short}");
        }
        println!("{} target(s)", matched_processes().len());
        return;
    }
    if args.iter().any(|a| a == "--kill-now") {
        let n = kill_matched();
        println!("fast-killed {n} matched process(es)");
        log_line(&format!("[kill-now] fast-killed {n}"));
        return;
    }
    if args.iter().any(|a| a == "--install") {
        install_autostart();
        return;
    }
    if args.iter().any(|a| a == "--uninstall") {
        uninstall_autostart();
        return;
    }

    let event_loop: EventLoop<UiMsg> = EventLoop::with_user_event();
    let proxy_tx = event_loop.create_proxy();

    thread::spawn(move || poll_loop(proxy_tx));
    thread::spawn(guard_loop);

    // tray + menu (created on the main thread inside the run loop)
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            toggle();
        }
    }));
    MenuEvent::set_event_handler(Some(move |event| {
        if event.id() == "toggle" {
            toggle();
        } else if event.id() == "kill" {
            let n = kill_matched();
            log_line(&format!("menu: fast-killed {n} matched process(es)"));
        } else if event.id() == "quit" {
            std::process::exit(0);
        }
    }));

    let mut tray: Option<TrayIcon> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let NewEvents(StartCause::Init) = event {
            let menu = Menu::new();
            let _ = menu.append(&MenuItem::with_id("toggle", "Toggle proxy", true, None));
            let _ = menu.append(&MenuItem::with_id("kill", "Kill matched processes now", true, None));
            let _ = menu.append(&MenuItem::with_id("quit", "Quit singbox-watch", true, None));
            match TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(format!("sing-box proxy: starting…{}", renew_tag()))
                .with_icon(icon_for(false).expect("icon"))
                .build()
            {
                Ok(t) => {
                    tray = Some(t);
                    log_line("tray: menu-bar icon added");
                }
                Err(e) => log_line(&format!("tray: build FAILED: {e}")),
            }
        }
        if let UserEvent(_) = event {
            // icon refresh requested by the poll thread
        }
        if tray.is_some() {
            let st = ACTUAL.load(Ordering::Relaxed);
            if let Some(t) = tray.as_ref() {
                let _ = t.set_icon(if st == 1 { icon_for(true) } else { icon_for(false) });
                let ip = fetch_public_ip_cached();
                let tip = match (&ip, st) {
                    (Some(ip), 1) => format!("sing-box proxy: ON — exit {ip}{}", renew_tag()),
                    (Some(ip), _) => format!("sing-box proxy: OFF — raw exit {ip}{}", renew_tag()),
                    (None, _) => format!("sing-box proxy: checking…{}", renew_tag()),
                };
                let _ = t.set_tooltip(Some(tip));
            }
        }
    });
}

fn fetch_public_ip_cached() -> Option<String> {
    static CACHE: Mutex<Option<(i64, String)>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    if let Some((ts, ip)) = g.clone() {
        if now() - ts < 10 {
            return Some(ip);
        }
    }
    let ip = fetch_public_ip();
    if let Some(ip) = &ip {
        *g = Some((now(), ip.clone()));
    }
    ip
}

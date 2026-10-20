// Shared helpers for singbox-watch (banner watchdog) and proxy-tray (tray switch).

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use winapi::um::winnt::{KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ};
use winapi::um::winreg::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

/// Directory of the running exe (falling back to the working directory).
/// All instance files — log, pid lists, temp captures, expected-exit-ip.txt —
/// live here by default.
pub fn base_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Where the shared state files (expected-exit-ip.txt / proxy-state.txt)
/// live: next to the exe if an expected file is already there, otherwise
/// %USERPROFILE%\.singbox-watch\ (created on demand).
fn state_root() -> std::path::PathBuf {
    let b = base_dir();
    if b.join("expected-exit-ip.txt").exists() {
        return b;
    }
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if !home.is_empty() {
        let h = std::path::PathBuf::from(&home).join(".singbox-watch");
        let _ = fs::create_dir_all(&h);
        return h;
    }
    b
}

pub fn expected_file() -> std::path::PathBuf {
    state_root().join("expected-exit-ip.txt")
}

pub fn state_file() -> std::path::PathBuf {
    state_root().join("proxy-state.txt")
}

/// Name of the scheduled task that starts the tunnel (schtasks /Run /End).
/// Override with the SINGBOX_WATCH_TASK environment variable.
pub fn task_name() -> String {
    std::env::var("SINGBOX_WATCH_TASK").unwrap_or_else(|_| "sing-box-tun".to_string())
}

/// Optional proxy renewal deadline: a renew-deadline.txt next to the exe
/// containing YYYY-MM-DD. Returns midnight UTC of that date.
pub fn renew_deadline_unix() -> Option<i64> {
    let txt = fs::read_to_string(base_dir().join("renew-deadline.txt")).ok()?;
    let mut it = txt.trim().split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // days-from-civil (Howard Hinnant's algorithm)
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400)
}

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

// Both binaries are built with the windows (GUI) subsystem so they never open
// console windows — a closed console kills the process, which kept murdering
// the tray app whenever the user dismissed the black window. CLI output
// (--check-once / --state-once) still works by attaching to the caller's
// console and writing to it directly.
static CONSOLE_TRIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn cli_print(s: &str) {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use winapi::um::consoleapi::WriteConsoleW;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_OUTPUT_HANDLE;
    use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
    // inherited stdout (pipes from shells/agents) works even without a console
    let _ = std::io::stdout().write_all(s.as_bytes());
    let _ = std::io::stdout().flush();
    unsafe {
        if !CONSOLE_TRIED.swap(true, Ordering::Relaxed) {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let w = wide(s);
        let len = w.len().saturating_sub(1) as u32;
        if !h.is_null() && len > 0 {
            let mut written: u32 = 0;
            WriteConsoleW(h, w.as_ptr() as *const _, len, &mut written, std::ptr::null_mut());
        }
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn valid_ipv4(s: &str) -> bool {
    let t = s.trim();
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
        })
}

// Plain HTTP on purpose: works through the tunnel AND when bypassed (which is
// exactly the state we must detect), and needs no TLS stack in the binary.
// HTTP/1.0 keeps the response body unchunked.
pub fn http_get(host: &str, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect((host, 80)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(6))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(6))).ok()?;
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: singbox-watch/1.0\r\nAccept: */*\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.split("\r\n\r\n").nth(1).map(|s| s.to_string())
}

// Multi-endpoint on purpose: proxy providers cap concurrent connections, and
// one refused health-check request must not read as "proxy off".
pub fn fetch_public_ip() -> Option<String> {
    for (host, path) in [
        ("ip-api.com", "/line?fields=query"),
        ("ifconfig.me", "/"),
        ("checkip.amazonaws.com", "/"),
    ] {
        if let Some(body) = http_get(host, path) {
            let ip = body.trim();
            if valid_ipv4(ip) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

// Health checks can be a big share of the proxy's connection budget, so the
// tray and the banner share one result through this file instead of each
// running their own check every cycle.
// Format: "<unix secs> <ip>" or "<unix secs> -" for no connectivity.
fn exit_ip_cache_file() -> std::path::PathBuf {
    state_root().join("exit-ip-cache.txt")
}

/// Exit IP, reusing the other process's result if it is younger than
/// `max_age_secs`. A cached failure is reused too: hammering a saturated
/// proxy with more checks only makes the saturation worse.
pub fn fetch_public_ip_shared(max_age_secs: i64) -> Option<String> {
    let cache = exit_ip_cache_file();
    if let Ok(txt) = fs::read_to_string(&cache) {
        let mut it = txt.split_whitespace();
        if let (Some(ts), Some(val)) = (it.next().and_then(|t| t.parse::<i64>().ok()), it.next()) {
            let age = now() - ts;
            if (0..max_age_secs).contains(&age) {
                if val == "-" {
                    return None;
                }
                if valid_ipv4(val) {
                    return Some(val.to_string());
                }
            }
        }
    }
    let ip = fetch_public_ip();
    let _ = fs::write(&cache, format!("{} {}", now(), ip.as_deref().unwrap_or("-")));
    ip
}

pub fn read_expected_ip() -> Option<String> {
    fs::read_to_string(expected_file())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| valid_ipv4(t))
}

/// True when the tray switch last set the proxy to OFF (intentional state,
/// as opposed to an unexpected tunnel failure).
pub fn proxy_state_off() -> bool {
    fs::read_to_string(state_file())
        .map(|t| t.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

pub fn set_proxy_state(on: bool) {
    let _ = fs::write(state_file(), if on { "on" } else { "off" });
}

/// Read a REG_SZ value; hklm=false → HKCU. None if key/value missing.
pub fn read_reg_sz(hklm: bool, subkey: &str, value: &str) -> Option<String> {
    unsafe {
        let mut hk = std::mem::zeroed();
        let root = if hklm { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
        let sub = wide(subkey);
        if RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut hk) != 0 {
            return None;
        }
        let name = wide(value);
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        if RegQueryValueExW(hk, name.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut len) != 0 {
            RegCloseKey(hk);
            return None;
        }
        let mut buf = vec![0u8; (len as usize).max(2)];
        let rc = RegQueryValueExW(hk, name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), buf.as_mut_ptr(), &mut len);
        RegCloseKey(hk);
        if rc != 0 {
            return None;
        }
        let units: Vec<u16> = buf[..(len as usize).min(buf.len()) / 2]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }
}

pub fn install_autostart(value_name: &str) {
    let exe = env::current_exe().unwrap_or_default();
    let quoted = format!("\"{}\"", exe.display());
    unsafe {
        let mut hk = std::mem::zeroed();
        let subkey = wide(RUN_KEY);
        if RegCreateKeyExW(
            HKEY_CURRENT_USER, subkey.as_ptr(), 0, ptr::null_mut(),
            REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, ptr::null_mut(),
            &mut hk, ptr::null_mut(),
        ) == 0
        {
            let name = wide(value_name);
            let data = wide(&quoted);
            let ok = RegSetValueExW(
                hk, name.as_ptr(), 0, REG_SZ,
                data.as_ptr() as *const u8, (data.len() * 2) as u32,
            );
            RegCloseKey(hk);
            if ok == 0 {
                println!("autostart installed: {quoted}");
            } else {
                eprintln!("RegSetValueExW failed: {ok}");
            }
        } else {
            eprintln!("RegCreateKeyExW failed");
        }
    }
}

pub fn uninstall_autostart(value_name: &str) {
    unsafe {
        let mut hk = std::mem::zeroed();
        let subkey = wide(RUN_KEY);
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut hk) == 0 {
            let name = wide(value_name);
            let rc = RegDeleteValueW(hk, name.as_ptr());
            RegCloseKey(hk);
            if rc == 0 {
                println!("autostart removed");
            } else {
                println!("nothing to remove (rc={rc})");
            }
        } else {
            println!("nothing to remove (no Run key)");
        }
    }
}

#![windows_subsystem = "windows"]
// proxy-tray — system-tray on/off switch and watchdog for a sing-box tunnel.
// Green icon = tunnel up (exit IP matches the pinned IP in
// expected-exit-ip.txt). Red icon = down / exiting raw.
// Left-click (or double-click): toggle.
//   Turning OFF: kills every matched app process FIRST (sub-second, native),
//   then stops the tunnel task, so nothing slips out during the transition.
//   Turning ON: verifies defaults, then starts the tunnel scheduled task
//   (UAC fallback if schtasks is denied).
// Right-click: menu — toggle / kill matched processes / re-add icon / tuning.
// Tooltip always shows the live exit IP. Auto-starts at login.

use std::collections::HashSet;
use std::env;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use singbox_watch::{
    cli_print, fetch_public_ip, install_autostart, proxy_state_off, read_expected_ip, set_proxy_state,
    task_name, uninstall_autostart, wide,
};

use winapi::shared::minwindef::{UINT, WPARAM};
use winapi::shared::windef::{HWND, POINT};
use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::processthreadsapi::{
    CreateProcessW, GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_INFORMATION,
    STARTUPINFOW,
};
use winapi::um::shellapi::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use winapi::um::synchapi::{CreateMutexW, WaitForSingleObject};
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winapi::um::wingdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use winapi::um::winbase::{CREATE_NO_WINDOW, STARTF_USESHOWWINDOW};
use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, PROCESS_VM_READ};
use winapi::um::winuser::*;

use winapi::shared::minwindef::{LPARAM, LRESULT};

const WM_TRAY: UINT = WM_APP + 5; // tray icon callback
const WM_UPDATE: UINT = WM_APP + 6; // icon refresh nudge from poll thread
const RUN_VALUE: &str = "SingBoxTray";

fn pids_file() -> std::path::PathBuf {
    singbox_watch::base_dir().join("claude-pids.txt")
}

/// Substrings that exempt a node.exe command line from the kill match
/// (comma-separated, in SINGBOX_WATCH_KILL_EXCLUDE). Useful to protect
/// other agent/editor runtimes that merely mention the match string.
fn kill_exclusions() -> Vec<String> {
    std::env::var("SINGBOX_WATCH_KILL_EXCLUDE")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn cmdline_is_kill_target(cl: &str) -> bool {
    let lower = cl.to_lowercase();
    if !lower.contains("claude-code") {
        return false;
    }
    !kill_exclusions().iter().any(|x| lower.contains(x))
}

fn renew_tag() -> String {
    let Some(deadline) = singbox_watch::renew_deadline_unix() else {
        return String::new();
    };
    let now = singbox_watch::now();
    if now >= deadline {
        return " · IP EXPIRED — renew your proxy!".to_string();
    }
    let days = (deadline - now) / 86400 + 1;
    if now >= deadline - 5 * 86400 {
        return format!(" · RENEW PROXY NOW ({days}d left)");
    }
    format!(" · renew in {days}d")
}

// actual tunnel state: 0 unknown, 1 on, 2 off
static ACTUAL: AtomicU8 = AtomicU8::new(0);
static TOGGLE_BUSY: AtomicU8 = AtomicU8::new(0);
static LAST_IP: Mutex<String> = Mutex::new(String::new());
static EXPECTED: Mutex<String> = Mutex::new(String::new());
static ICON_ON: Mutex<usize> = Mutex::new(0); // HICON as usize
static ICON_OFF: Mutex<usize> = Mutex::new(0);
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

// NOTIFYICONDATAW holds raw HWND/HICON handles — not Send by default.
struct SendNid(Option<NOTIFYICONDATAW>);
unsafe impl Send for SendNid {}
static NID: Mutex<SendNid> = Mutex::new(SendNid(None));
static ICON_ADDED: AtomicU8 = AtomicU8::new(0);
static LAST_SHOWN: AtomicU8 = AtomicU8::new(255); // 255 = never shown yet

fn log_line(msg: &str) {
    use std::io::Write;
    let path = singbox_watch::base_dir().join("proxy-tray.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {msg}", singbox_watch::now());
    }
}

fn run_sync(cmdline: &str, wait_ms: u32) -> i32 {
    unsafe {
        let mut w = wide(cmdline);
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESHOWWINDOW;
        si.wShowWindow = SW_HIDE as u16;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            w.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut si,
            &mut pi,
        );
        if ok == 0 {
            return -1;
        }
        WaitForSingleObject(pi.hProcess, wait_ms);
        let mut code: u32 = 0;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        code as i32
    }
}

fn is_process_alive(name: &str) -> bool {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let n = String::from_utf16_lossy(&pe.szExeFile[..len]);
                if n.eq_ignore_ascii_case(name) {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

fn kill_claude_blocking() {
    // native Claude Code binary
    log_line("kill: claude.exe");
    run_sync("cmd.exe /c taskkill /F /T /IM claude.exe", 8000);
    // npm installs run as node.exe — match the claude-code package path on the
    // command line, NOT a bare 'claude' substring (too greedy). Exclusions
    // from SINGBOX_WATCH_KILL_EXCLUDE protect other agent/editor runtimes.
    log_line("kill: node.exe claude-code instances");
    let excl = kill_exclusions();
    let notmatch = if excl.is_empty() {
        String::new()
    } else {
        format!("-and $_.CommandLine -notmatch '{}' ", excl.join("|"))
    };
    run_sync(
        &format!(
            r#"powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq 'node.exe' -and $_.CommandLine -match 'claude-code' {notmatch}}} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}""#
        ),
        20000,
    );
}

fn start_tunnel_blocking() {
    log_line("start: schtasks /Run");
    let rc = run_sync(&format!("cmd.exe /c schtasks /Run /TN {}", task_name()), 10000);
    if rc != 0 {
        if is_process_alive("sing-box.exe") {
            // at boot the delayed logon task may already be starting it —
            // "task already running" must NOT trigger a UAC prompt
            log_line(&format!("start: schtasks rc={rc} but sing-box already running — ok"));
        } else {
            // task missing or access denied — elevate the start (UAC prompt)
            log_line(&format!("start: schtasks rc={rc}, UAC fallback"));
            run_sync(
                &format!(
                    r#"powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath 'cmd.exe' -ArgumentList '/c','schtasks /Run /TN {}'""#,
                    task_name()
                ),
                10000,
            );
        }
    }
}

fn stop_tunnel_blocking() {
    log_line("stop: schtasks /End + taskkill");
    run_sync(&format!("cmd.exe /c schtasks /End /TN {}", task_name()), 10000);
    run_sync("cmd.exe /c taskkill /F /IM sing-box.exe", 8000);
    thread::sleep(Duration::from_millis(1500));
    if is_process_alive("sing-box.exe") {
        // sing-box often runs as an elevated task — non-elevated taskkill is denied
        log_line("stop: still alive — UAC fallback");
        run_sync(
            &format!(
                r#"powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath 'cmd.exe' -ArgumentList '/c','schtasks /End /TN {} & taskkill /F /IM sing-box.exe'""#,
                task_name()
            ),
            15000,
        );
        thread::sleep(Duration::from_millis(1500));
    }
    log_line(if is_process_alive("sing-box.exe") {
        "stop: FAILED — sing-box still running (tunnel still up!)"
    } else {
        "stop: sing-box stopped"
    });
}

// ---------- auto-enable guard: never let Claude touch the network from raw HK ----------

static EPISODE_TRIGGERED: AtomicU8 = AtomicU8::new(0);

// policy for matched apps while the proxy is off:
// 1 = keep closed (safe default — killed on sight, dialog pending)
// 2 = allow (falls back to the auto-enable guard: starting the app
//     re-enables the proxy instead)
static KEEP_CLOSED: AtomicU8 = AtomicU8::new(1);
static ASKED_EPISODE: AtomicU8 = AtomicU8::new(0);

fn ask_keep_closed() {
    thread::spawn(|| {
        log_line("[ask] proxy off + matched apps closed — asking whether to keep them closed");
        let mb = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                wide("The proxy is OFF, so matched applications were closed to keep them off the raw connection.\n\nKeep them closed while the proxy stays off?\n\nYes — they stay closed (relaunches are closed again automatically)\nNo — allow them to run (the proxy will re-enable itself when they start)").as_ptr(),
                wide("singbox-watch — proxy is off").as_ptr(),
                MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
            )
        };
        if mb == IDYES {
            KEEP_CLOSED.store(1, Ordering::Relaxed);
            log_line("[ask] answer: keep closed");
        } else {
            KEEP_CLOSED.store(2, Ordering::Relaxed);
            log_line("[ask] answer: allow (auto-enable on start)");
        }
    });
}

// minimal view of a DNS resolver cache entry — only the fields we walk
#[repr(C)]
struct CacheEntry {
    pNext: *mut CacheEntry,
    pName: *mut u8,
}
#[link(name = "dnsapi")]
extern "system" {
    fn DnsGetCacheDataTable(ppEntry: *mut *mut CacheEntry) -> i32;
}

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: *mut winapi::ctypes::c_void) -> i32;
    fn NtResumeProcess(process: *mut winapi::ctypes::c_void) -> i32;
    fn NtQueryInformationProcess(
        process: *mut winapi::ctypes::c_void,
        info_class: u32,
        info: *mut winapi::ctypes::c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[repr(C)]
struct ProcessBasicInformation {
    exit_status: i32,
    peb_base_address: *mut winapi::ctypes::c_void,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_pid: usize,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    _pad: u32,
    buffer: *mut u16,
}

// command line read straight from the target's PEB — no PowerShell, no WMI,
// microseconds per process (x64 offsets)
fn process_cmdline(pid: u32) -> Option<String> {
    use winapi::um::memoryapi::ReadProcessMemory;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return None;
        }
        let result = (|| {
            let mut pbi: ProcessBasicInformation = std::mem::zeroed();
            if NtQueryInformationProcess(
                h,
                0, // ProcessBasicInformation
                &mut pbi as *mut _ as *mut winapi::ctypes::c_void,
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                std::ptr::null_mut(),
            ) != 0 || pbi.peb_base_address.is_null()
            {
                return None;
            }
            let mut params_ptr: *mut winapi::ctypes::c_void = std::ptr::null_mut();
            if ReadProcessMemory(
                h,
                (pbi.peb_base_address as *const u8).add(0x20) as *const _, // PEB.ProcessParameters (x64)
                &mut params_ptr as *mut _ as *mut winapi::ctypes::c_void,
                std::mem::size_of::<*mut winapi::ctypes::c_void>(),
                std::ptr::null_mut(),
            ) == 0 || params_ptr.is_null()
            {
                return None;
            }
            let mut us: UnicodeString = std::mem::zeroed();
            if ReadProcessMemory(
                h,
                (params_ptr as *const u8).add(0x70) as *const _, // ProcessParameters.CommandLine (x64)
                &mut us as *mut _ as *mut winapi::ctypes::c_void,
                std::mem::size_of::<UnicodeString>(),
                std::ptr::null_mut(),
            ) == 0
            {
                return None;
            }
            let len = us.length as usize;
            if us.buffer.is_null() || len == 0 || len > 16384 {
                return None;
            }
            let mut buf = vec![0u16; len / 2];
            if ReadProcessMemory(
                h,
                us.buffer as *const _,
                buf.as_mut_ptr() as *mut winapi::ctypes::c_void,
                len,
                std::ptr::null_mut(),
            ) == 0
            {
                return None;
            }
            Some(String::from_utf16_lossy(&buf))
        })();
        CloseHandle(h);
        result
    }
}

// enumerate kill targets (pid, image name, command line) — shared by the
// fast killer and the --kill-list dry run
fn list_claude_targets() -> Vec<(u32, String, String)> {
    let mut targets = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return targets;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&pe.szExeFile[..len]);
                let pid = pe.th32ProcessID;
                if pid != std::process::id() {
                    if name.eq_ignore_ascii_case("claude.exe") {
                        let cl = process_cmdline(pid).unwrap_or_default();
                        targets.push((pid, name, cl));
                    } else if name.eq_ignore_ascii_case("node.exe") {
                        let cl = process_cmdline(pid).unwrap_or_default().to_lowercase();
                        if cmdline_is_kill_target(&cl) {
                            targets.push((pid, name, cl));
                        }
                    }
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    targets
}

// native kill path: snapshot -> match -> TerminateProcess. No external
// processes spawned; completes in tens of milliseconds.
fn kill_claude_fast() -> usize {
    let mut killed = 0usize;
    for (pid, _, _) in list_claude_targets() {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                if TerminateProcess(h, 1) != 0 {
                    killed += 1;
                }
                CloseHandle(h);
            }
        }
    }
    killed
}

fn bytes_contain(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn pids_named(name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let n = String::from_utf16_lossy(&pe.szExeFile[..len]);
                if n.eq_ignore_ascii_case(name) {
                    out.push(pe.th32ProcessID);
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

// claude.exe by name + node.exe running claude-code (list refreshed
// asynchronously by a PowerShell probe that writes claude-pids.txt)
fn claude_pids_snapshot() -> Vec<u32> {
    let mut v = pids_named("claude.exe");
    if let Ok(txt) = std::fs::read_to_string(pids_file()) {
        for line in txt.lines() {
            if let Ok(p) = line.trim().parse::<u32>() {
                v.push(p);
            }
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

// returns true when a claude.ai / anthropic.com name shows up in the DNS
// resolver cache that we haven't seen before (i.e. a fresh resolution —
// something is trying to open Claude)
fn dns_new_claude_lookup(seen: &mut HashSet<Vec<u8>>) -> bool {
    unsafe {
        let mut head: *mut CacheEntry = std::ptr::null_mut();
        if DnsGetCacheDataTable(&mut head) == 0 {
            return false;
        }
        let mut hit = false;
        let mut p = head;
        while !p.is_null() {
            let name_ptr = (*p).pName;
            if !name_ptr.is_null() {
                let mut bytes = Vec::new();
                let mut q = name_ptr;
                while *q != 0 && bytes.len() < 300 {
                    bytes.push(*q);
                    q = q.add(1);
                }
                let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
                if (bytes_contain(&lower, b"claude.ai") || bytes_contain(&lower, b"anthropic.com"))
                    && !seen.contains(&bytes)
                {
                    seen.insert(bytes);
                    hit = true;
                }
            }
            p = (*p).pNext;
        }
        hit
    }
}

fn suspend_processes(pids: &[u32]) -> Vec<*mut winapi::ctypes::c_void> {
    let mut handles = Vec::new();
    for pid in pids {
        unsafe {
            let h = OpenProcess(PROCESS_SUSPEND_RESUME, 0, *pid);
            if !h.is_null() {
                if NtSuspendProcess(h) == 0 {
                    log_line(&format!("suspended pid {pid} until tunnel is verified"));
                    handles.push(h);
                } else {
                    CloseHandle(h);
                }
            }
        }
    }
    handles
}

fn resume_processes(handles: Vec<*mut winapi::ctypes::c_void>) {
    for h in handles {
        unsafe {
            NtResumeProcess(h);
            CloseHandle(h);
        }
    }
}

// enable the tunnel; any live Claude processes are suspended until the exit IP
// is verified, so nothing can fire a request from raw HK during the ramp
fn auto_enable(pids: Vec<u32>, reason: &str, hwnd: usize) {
    let reason = reason.to_string();
    if TOGGLE_BUSY.swap(1, Ordering::AcqRel) == 1 {
        log_line(&format!("[guard] {reason}: another action in progress — skipping"));
        return;
    }
    EPISODE_TRIGGERED.store(1, Ordering::Relaxed);
    thread::spawn(move || {
        let hwnd = hwnd as HWND;
        let _busy = BusyGuard;
        log_line(&format!("[guard] {reason} while tunnel is down — auto-enabling"));
        let handles = suspend_processes(&pids);
        set_proxy_state(true);
        enforce_defaults();
        start_tunnel_blocking();
        let expected = EXPECTED.lock().unwrap().clone();
        let mut up = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_secs(2));
            if let Some(ip) = fetch_public_ip() {
                if !expected.is_empty() && ip == expected {
                    up = true;
                    break;
                }
            }
        }
        if up {
            resume_processes(handles);
            log_line("[guard] tunnel verified up — claude resumed");
            // NOTE: EPISODE_TRIGGERED stays set — only guard_loop re-arms it
            // after IT observes actual==1; resetting here let the guard
            // re-fire while ACTUAL was still stale, double-suspending Claude
        } else if handles.is_empty() {
            log_line("[guard] tunnel did NOT come up (nothing was suspended)");
        } else {
            // keep them frozen: resuming would let claude fire from raw HK
            log_line("[guard] tunnel did NOT come up after 60s — claude left suspended (safe). Restore the tunnel, then restart claude.");
        }
        unsafe {
            PostMessageW(hwnd, WM_UPDATE, 0, 0);
        }
    });
}

// one-click tuning: runs apply-tuning.cmd from the exe directory (if you
// shipped it) elevated with a UAC prompt. Matched app processes are suspended
// across the script's tunnel restart and resumed once the exit IP is verified.
fn apply_tuning(hwnd: usize) {
    if TOGGLE_BUSY.swap(1, Ordering::AcqRel) == 1 {
        log_line("[tuning] another action in progress — skipping");
        return;
    }
    let script = singbox_watch::base_dir().join("apply-tuning.cmd");
    if !script.exists() {
        log_line("[tuning] apply-tuning.cmd not found next to the exe — skipping");
        return;
    }
    let script = script.display().to_string();
    thread::spawn(move || {
        let hwnd = hwnd as HWND;
        let _busy = BusyGuard;
        log_line("[tuning] apply: tuning script + tunnel restart (UAC)");
        let handles = suspend_processes(&claude_pids_snapshot());
        let rc = run_sync(
            &format!(
                r#"powershell -NoProfile -Command "Start-Process -Verb RunAs -Wait -FilePath '{script}'""#
            ),
            120000,
        );
        log_line(&format!("[tuning] script finished (rc={rc})"));
        let expected = EXPECTED.lock().unwrap().clone();
        let mut up = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_secs(2));
            if let Some(ip) = fetch_public_ip() {
                if !expected.is_empty() && ip == expected {
                    up = true;
                    break;
                }
            }
        }
        if up {
            resume_processes(handles);
            log_line("[tuning] tunnel verified up — claude resumed");
        } else if handles.is_empty() {
            log_line("[tuning] tunnel did NOT come back");
        } else {
            log_line("[tuning] tunnel did NOT come back — claude left suspended (safe)");
        }
        unsafe {
            PostMessageW(hwnd, WM_UPDATE, 0, 0);
        }
    });
}

// ---------- defaults enforcement: verified on every proxy enable ----------
// Checks BBR2 congestion control and the WebRTC leak policy after every
// enable; drift is auto-corrected via the elevated tuning script (with a
// 30-minute prompt cooldown so a flaky detection can never spam UAC dialogs).

fn cc_is_bbr2() -> bool {
    let tmp = singbox_watch::base_dir().join("enforce-cc.txt");
    let tmp = tmp.to_str().unwrap_or("enforce-cc.txt");
    let _ = std::fs::remove_file(&tmp);
    run_sync(&format!("cmd.exe /c netsh int tcp show supplemental > \"{tmp}\" 2>&1"), 8000);
    std::fs::read_to_string(&tmp).unwrap_or_default().to_lowercase().contains("bbr")
}

fn webrtc_all_blocked() -> bool {
    // own temp file — sharing one with cc_is_bbr2 raced between concurrent
    // enforce_defaults calls and produced false "drifted" detections
    let tmp = singbox_watch::base_dir().join("enforce-webrtc.txt");
    let tmp = tmp.to_str().unwrap_or("enforce-webrtc.txt").to_string();
    for pol in [
        "Policies\\Google\\Chrome",
        "Policies\\Microsoft\\Edge",
        "Policies\\BraveSoftware\\Brave",
    ] {
        let mut ok = false;
        for prefix in ["HKLM\\SOFTWARE\\", "HKCU\\Software\\"] {
            let _ = std::fs::remove_file(&tmp);
            run_sync(&format!("cmd.exe /c reg query \"{prefix}{pol}\" /v WebRtcIPHandling > \"{tmp}\" 2>&1"), 8000);
            if std::fs::read_to_string(&tmp)
                .unwrap_or_default()
                .to_lowercase()
                .contains("disable_non_proxied_udp")
            {
                ok = true;
            }
        }
        if !ok {
            return false;
        }
    }
    true
}

static TUNE_LAST_UNIX: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
const TUNE_COOLDOWN_SECS: i64 = 1800;

fn enforce_defaults() {
    if !cc_is_bbr2() || !webrtc_all_blocked() {
        let now_ts = singbox_watch::now();
        if now_ts - TUNE_LAST_UNIX.load(Ordering::Relaxed) < TUNE_COOLDOWN_SECS {
            log_line("[defaults] drift detected but tuning prompt on cooldown (30 min) — run --doctor to see status");
        } else {
            let script = singbox_watch::base_dir().join("apply-tuning.cmd");
            if !script.exists() {
                log_line("[defaults] drift detected but apply-tuning.cmd not found — fix manually, see --doctor");
            } else {
                TUNE_LAST_UNIX.store(now_ts, Ordering::Relaxed);
                log_line("[defaults] BBR2/WebRTC drifted — launching tuning (approve the UAC)");
                let script = script.display().to_string();
                run_sync(
                    &format!(
                        r#"powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '{script}'""#
                    ),
                    3000,
                );
            }
        }
    } else {
        log_line("[defaults] verified: BBR2, WebRTC blocked");
    }
}

fn guard_loop(hwnd: HWND) {
    let mut seen_dns: HashSet<Vec<u8>> = HashSet::new();
    let mut tick: u32 = 0;
    log_line("[guard] watching for Claude processes + claude.ai/anthropic.com DNS lookups");
    loop {
        tick = tick.wrapping_add(1);
        let actual = ACTUAL.load(Ordering::Relaxed);
        if actual == 1 {
            EPISODE_TRIGGERED.store(0, Ordering::Relaxed);
            seen_dns.clear();
            // new off-episode: re-arm the keep-closed policy (safe default)
            KEEP_CLOSED.store(1, Ordering::Relaxed);
            ASKED_EPISODE.store(0, Ordering::Relaxed);
            // drop stale pid list — pids get reused; never act on old data
            let _ = std::fs::remove_file(pids_file());
        } else {
            if tick % 100 == 0 {
                // fire-and-forget refresh of node-based claude pids (file read next ticks)
                let excl = kill_exclusions();
                let notmatch = if excl.is_empty() {
                    String::new()
                } else {
                    format!("-and $_.CommandLine -notmatch '{}' ", excl.join("|"))
                };
                let out = pids_file().display().to_string().replace('\\', "/");
                run_sync(
                    &format!(
                        r#"powershell -NoProfile -Command "(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq 'node.exe' -and $_.CommandLine -match 'claude-code' {notmatch}}}).ProcessId | Set-Content '{out}'""#
                    ),
                    200,
                );
            }
            let pids = claude_pids_snapshot();
            if !pids.is_empty() {
                if KEEP_CLOSED.load(Ordering::Relaxed) == 1 {
                    // proxy off + matched app detected: close immediately, ask once
                    let n = kill_claude_fast();
                    if n > 0 {
                        log_line(&format!("[guard] proxy off — closed {n} matched process(es)"));
                    }
                    if ASKED_EPISODE.swap(1, Ordering::Relaxed) == 0 {
                        ask_keep_closed();
                    }
                } else if EPISODE_TRIGGERED.load(Ordering::Relaxed) == 0 {
                    // user chose "allow": fall back to suspend + auto-enable
                    auto_enable(pids, "claude process detected", hwnd as usize);
                }
            }
            if proxy_state_off()
                && EPISODE_TRIGGERED.load(Ordering::Relaxed) == 0
                && dns_new_claude_lookup(&mut seen_dns)
            {
                auto_enable(Vec::new(), "claude.ai / anthropic.com DNS lookup detected", hwnd as usize);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        TOGGLE_BUSY.store(0, Ordering::Release);
    }
}

fn toggle(hwnd: HWND) {
    // debounce: rapid repeated clicks must not stack concurrent kill/stop
    // sequences on top of each other
    if TOGGLE_BUSY.swap(1, Ordering::AcqRel) == 1 {
        log_line("toggle: ignored — another toggle already in progress");
        return;
    }
    let turn_on = ACTUAL.load(Ordering::Relaxed) != 1;
    let h = hwnd as usize; // raw HWND is not Send
    thread::spawn(move || {
        let _busy = BusyGuard; // clears TOGGLE_BUSY on any exit, incl. panic
        let hwnd = h as HWND;
        if turn_on {
            log_line("toggle: turning ON");
            set_proxy_state(true);
            enforce_defaults();
            start_tunnel_blocking();
        } else {
            log_line("toggle: turning OFF (kill Claude first)");
            set_proxy_state(false);
            // fast native kill first — completes in well under a second
            let n = kill_claude_fast();
            log_line(&format!("toggle: fast-killed {n} matched process(es)"));
            if n > 0 && ASKED_EPISODE.swap(1, Ordering::Relaxed) == 0 {
                ask_keep_closed();
            }
            // async sweep (taskkill /T trees + WMI pass) for anything missed
            thread::spawn(|| {
                kill_claude_blocking();
            });
            stop_tunnel_blocking();
        }
        log_line("toggle: actions complete");
        unsafe {
            PostMessageW(hwnd, WM_UPDATE, 0, 0);
        }
    });
}

fn poll_loop(hwnd: HWND) {
    loop {
        let expected = EXPECTED.lock().unwrap().clone();
        let (st, ip) = match fetch_public_ip() {
            Some(ip) => {
                let s = if !expected.is_empty() && ip == expected { 1u8 } else { 2u8 };
                (s, ip)
            }
            None => (0u8, String::from("<no connectivity>")),
        };
        ACTUAL.store(st, Ordering::Relaxed);
        *LAST_IP.lock().unwrap() = ip;
        // keep the state file truthful: whenever the tunnel is verifiably up,
        // the watcher should never claim "intentionally off" (stale 'off')
        if st == 1 {
            set_proxy_state(true);
        }
        unsafe {
            PostMessageW(hwnd, WM_UPDATE, 0, 0);
        }
        thread::sleep(Duration::from_secs(10));
    }
}

fn update_icon(hwnd: HWND) {
    let st = ACTUAL.load(Ordering::Relaxed);
    let ip = LAST_IP.lock().unwrap().clone();
    let tip = match st {
        1 => format!("sing-box proxy: ON — exit {ip}{}", renew_tag()),
        2 => format!("sing-box proxy: OFF — raw exit {ip}{}", renew_tag()),
        _ => format!("sing-box proxy: checking…{}", renew_tag()),
    };
    let hicon = if st == 1 { *ICON_ON.lock().unwrap() } else { *ICON_OFF.lock().unwrap() };
    let mut guard = NID.lock().unwrap();
    if let Some(nid) = guard.0.as_mut() {
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.hIcon = hicon as _;
        let wtip = wide(&tip);
        let n = wtip.len().min(127); // leave room for the NUL terminator
        nid.szTip[..n].copy_from_slice(&wtip[..n]);

        let prev = LAST_SHOWN.load(Ordering::Relaxed);
        if ICON_ADDED.load(Ordering::Relaxed) == 0 {
            // not registered (fresh start or zombie): clear and add fresh
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, nid);
                let rc = Shell_NotifyIconW(NIM_ADD, nid);
                if rc != 0 {
                    ICON_ADDED.store(1, Ordering::Relaxed);
                    LAST_SHOWN.store(st, Ordering::Relaxed);
                    if prev == 255 {
                        log_line("tray icon added ok");
                    } else {
                        log_line(&format!("[tray] icon re-added, state {st}"));
                    }
                } else {
                    let err = GetLastError();
                    log_line(&format!("NIM_ADD failed, err={err}"));
                }
            }
        } else {
            // healthy registration: MODIFY keeps the taskbar pinning intact
            // (delete+re-add resets Windows pinning — the icon "vanished" into
            // the hidden overflow on every state flip). A failed modify
            // (zombie) falls back to delete+add on the next timer tick.
            let rc = unsafe { Shell_NotifyIconW(NIM_MODIFY, nid) };
            if rc != 0 {
                LAST_SHOWN.store(st, Ordering::Relaxed);
            } else {
                ICON_ADDED.store(0, Ordering::Relaxed);
                let err = unsafe { GetLastError() };
                log_line(&format!("NIM_MODIFY failed, err={err}"));
            }
        }
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let st = ACTUAL.load(Ordering::Relaxed);
    let item1 = if st == 1 {
        "Turn proxy OFF  (kills Claude first)"
    } else {
        "Turn proxy ON"
    };
    AppendMenuW(menu, MF_STRING, 10, wide(item1).as_ptr());
    AppendMenuW(menu, MF_STRING, 11, wide("Kill Claude processes now").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, 12, wide("Re-add tray icon").as_ptr());
    AppendMenuW(menu, MF_STRING, 13, wide("Apply tuning: BBR2+WebRTC, restart tunnel (admin)").as_ptr());
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null_mut(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    log_line(&format!("menu: selected {cmd}"));
    match cmd {
        10 => toggle(hwnd),
        11 => {
            let n = kill_claude_fast();
            log_line(&format!("menu: fast-killed {n} claude process(es)"));
            cli_print(&format!("fast-killed {n} claude process(es)\n"));
            thread::spawn(|| {
                kill_claude_blocking();
            });
        }
        12 => {
            // force a fresh NIM_ADD (fixes icons lost after crashes/restarts)
            ICON_ADDED.store(0, Ordering::Relaxed);
            update_icon(hwnd);
        }
        13 => {
            apply_tuning(hwnd as usize);
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: UINT, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let tbm = TASKBAR_MSG.load(Ordering::Relaxed);
    match msg {
        m if m == WM_TRAY => {
            let mouse = lp as u32 as UINT;
            if mouse == WM_LBUTTONUP || mouse == WM_LBUTTONDBLCLK {
                toggle(hwnd);
            } else if mouse == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        m if m == WM_UPDATE => {
            update_icon(hwnd);
            0
        }
        WM_TIMER => {
            // retry loop: if the first add failed (e.g. explorer not ready at
            // login), keep trying every 5 s until the icon sticks
            if ICON_ADDED.load(Ordering::Relaxed) == 0 {
                update_icon(hwnd);
            }
            0
        }
        m if tbm != 0 && m == tbm => {
            // TaskbarCreated: explorer restarted, re-add the icon
            let mut guard = NID.lock().unwrap();
            if let Some(nid) = guard.0.as_mut() {
                Shell_NotifyIconW(NIM_ADD, nid);
            }
            0
        }
        WM_DESTROY => {
            log_line("wndproc: WM_DESTROY — exiting");
            let mut guard = NID.lock().unwrap();
            if let Some(nid) = guard.0.as_mut() {
                Shell_NotifyIconW(NIM_DELETE, nid);
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// 32x32 ARGB icon via a DIB section (alpha channel — required by the Win11
// tray): green square with a power symbol (on) / red square with X (off).
fn make_icon(on: bool) -> usize {
    const S: i32 = 32;
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = S;
        bmi.bmiHeader.biHeight = S; // positive = bottom-up rows
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut winapi::ctypes::c_void = std::ptr::null_mut();
        let hdc = GetDC(std::ptr::null_mut());
        let hbm_color = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if hbm_color.is_null() || bits.is_null() {
            log_line("CreateDIBSection failed — tray icon will be blank");
            return 0;
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (S * S * 4) as usize);
        for y in 0..S {
            for x in 0..S {
                let flipped = (S - 1 - y) as i32 * S; // bottom-up storage
                let idx = (flipped + x) as usize * 4;
                let (mut r, mut g, mut b) = if on { (10, 185, 70) } else { (200, 45, 45) };
                let dx = x - 16;
                let dy = y - 16;
                let white = if on {
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    let ring = d >= 9.0 && d <= 12.0;
                    let gap = dy < -6 && dx.abs() <= 4; // opening at the top of the ring
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
                px[idx] = b as u8;
                px[idx + 1] = g as u8;
                px[idx + 2] = r as u8;
                px[idx + 3] = 255; // opaque
            }
        }
        let mask = vec![0u8; (S * 4) as usize]; // 1bpp, 0 = opaque
        let hbm_mask = CreateBitmap(S, S, 1, 1, mask.as_ptr() as *const _);
        let mut ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&mut ii);
        DeleteObject(hbm_color as _);
        DeleteObject(hbm_mask as _);
        hicon as usize
    }
}

// full health report across every layer of the setup; exit 1 if anything failed
fn doctor() {
    use singbox_watch::read_reg_sz;
    let expected = {
        let e = EXPECTED.lock().unwrap().clone();
        if e.is_empty() {
            read_expected_ip().unwrap_or_default()
        } else {
            e
        }
    };
    let mut fails = 0u32;
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
            Some(ip) => {
                add(false, format!("tunnel     : WRONG EXIT IP {ip} (expected {expected}) ✗"));
            }
            None => add(false, "tunnel     : OFFLINE — checker unreachable ✗".to_string()),
        }

        add(
            is_process_alive("sing-box.exe"),
            format!("sing-box   : {}", if is_process_alive("sing-box.exe") { "running ✓" } else { "NOT running ✗" }),
        );
        let watcher = !pids_named("singbox-watch.exe").is_empty();
        add(watcher, format!("watcher    : {}", if watcher { "running ✓" } else { "NOT running ✗ (banner guard absent)" }));

        let ka = run_sync("cmd.exe /c schtasks /Query /TN singbox-apps-keepalive", 8000) == 0;
        add(ka, format!("keep-alive : {}", if ka { "task exists ✓" } else { "task MISSING ✗ (run install-keepalive.cmd)" }));

        for name in ["SingBoxWatch", "SingBoxTray"] {
            let v = read_reg_sz(false, "Software\\Microsoft\\Windows\\CurrentVersion\\Run", name);
            add(v.is_some(), format!("autostart  : {name} {}", if v.is_some() { "✓" } else { "✗" }));
        }

        // congestion control (netsh output is locale-mixed; ascii tokens survive)
        let tmp = singbox_watch::base_dir().join("doctor-tmp.txt");
        let tmp = tmp.to_str().unwrap_or("doctor-tmp.txt").to_string();
        let _ = std::fs::remove_file(&tmp);
        run_sync(&format!("cmd.exe /c netsh int tcp show supplemental > \"{tmp}\" 2>&1"), 8000);
        let net = std::fs::read_to_string(&tmp).unwrap_or_default().to_lowercase();
        let cc = if net.contains("bbr") {
            add(true, "tcp cc     : BBR2 ✓".to_string());
            true
        } else if net.contains("cubic") {
            add(false, "tcp cc     : CUBIC — apply tuning (tray menu → Apply tuning) ✗".to_string());
            false
        } else {
            add(false, "tcp cc     : unknown (netsh unreadable) ✗".to_string());
            false
        };
        let _ = cc;

        for (bname, pol) in [
            ("Chrome", "Policies\\Google\\Chrome"),
            ("Edge", "Policies\\Microsoft\\Edge"),
            ("Brave", "Policies\\BraveSoftware\\Brave"),
        ] {
            // via `reg query` file capture (proven pattern; full key path —
            // an earlier version doubled the SOFTWARE segment and
            // false-negatived every check)
            let mut ok = false;
            for prefix in ["HKLM\\SOFTWARE\\", "HKCU\\Software\\"] {
                let _ = std::fs::remove_file(&tmp);
                run_sync(&format!("cmd.exe /c reg query \"{prefix}{pol}\" /v WebRtcIPHandling > \"{tmp}\" 2>&1"), 8000);
                if std::fs::read_to_string(&tmp).unwrap_or_default().to_lowercase().contains("disable_non_proxied_udp") {
                    ok = true;
                }
            }
            add(ok, format!("webrtc {bname:<6}: {}", if ok { "blocked ✓" } else { "LEAKS REAL IP — apply tuning ✗" }));
        }

        let cfg = std::fs::read_to_string(singbox_watch::base_dir().join("config.json")).unwrap_or_default();
        let split = cfg.contains("windowsupdate.com") && cfg.contains("api.onedrive.com");
        add(split, format!("wu/onedrive bypass: {}", if split { "direct rules present ✓" } else { "NOT in config ✗" }));


        match singbox_watch::renew_deadline_unix() {
            None => add(true, "renewal    : no renew-deadline.txt — countdown disabled ✓".to_string()),
            Some(deadline) => {
                let now = singbox_watch::now();
                let days = (deadline - now) / 86400 + 1;
                if now >= deadline {
                    add(false, "renewal    : TERM ENDED — the IP is likely gone; renew NOW ✗".to_string());
                } else if now >= deadline - 5 * 86400 {
                    add(false, format!("renewal    : {days} days left — RENEW NOW ✗"));
                } else {
                    add(true, format!("renewal    : {days} days left ✓"));
                }
            }
        }
    }

    for line in &out {
        cli_print(&format!("{line}\n"));
        log_line(&format!("[doctor] {line}"));
    }
    cli_print(&format!("RESULT     : {} issue(s)\n", fails));
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--install") {
        install_autostart(RUN_VALUE);
        return;
    }
    if args.iter().any(|a| a == "--uninstall") {
        uninstall_autostart(RUN_VALUE);
        return;
    }

    // doctor runs without the single-instance guard: it's read-only and must
    // work while the tray app is live
    if args.iter().any(|a| a == "--doctor") {
        doctor();
        return;
    }
    if args.iter().any(|a| a == "--kill-list") {
        // dry run: show exactly what the fast killer would target
        for (pid, name, cl) in list_claude_targets() {
            let short: String = cl.chars().take(120).collect();
            cli_print(&format!("pid {pid}  {name}  {short}\n"));
        }
        let n = list_claude_targets().len();
        cli_print(&format!("{n} target(s)\n"));
        return;
    }
    if args.iter().any(|a| a == "--kill-now") {
        let n = kill_claude_fast();
        cli_print(&format!("fast-killed {n} claude process(es)\n"));
        log_line(&format!("[kill-now] fast-killed {n} claude process(es)"));
        return;
    }

    unsafe {
        let name = wide("SingBoxTraySingleton");
        CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            cli_print("proxy-tray is already running\n");
            return;
        }
    }

    *EXPECTED.lock().unwrap() = read_expected_ip().unwrap_or_default();

    if args.iter().any(|a| a == "--state-once") {
        let exp = EXPECTED.lock().unwrap().clone();
        cli_print(&format!(
            "expected: {}\n",
            if exp.is_empty() { "<none>" } else { &exp }
        ));
        match fetch_public_ip() {
            Some(ip) => {
                cli_print(&format!("exit IP : {ip}\n"));
                let st = if !exp.is_empty() && ip == exp { "ON" } else { "OFF" };
                cli_print(&format!("state   : {st}\n"));
                std::process::exit(0);
            }
            None => {
                cli_print("state   : UNKNOWN (no connectivity)\n");
                std::process::exit(2);
            }
        }
    }

    // any panic anywhere in the process now lands in the log, not the void
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("PANIC: {info}"));
    }));

    *ICON_ON.lock().unwrap() = make_icon(true);
    *ICON_OFF.lock().unwrap() = make_icon(false);

    unsafe {
        TASKBAR_MSG.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Ordering::Relaxed,
        );
        let hinst = GetModuleHandleW(std::ptr::null());
        let class_name: &'static mut [u16] = Box::leak(wide("SingBoxTrayWnd").into_boxed_slice());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if RegisterClassExW(&wc) == 0 {
            eprintln!("RegisterClassExW failed");
            std::process::exit(1);
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("SingBoxTray").as_ptr(),
            WS_OVERLAPPED,
            0, 0, 0, 0,
            std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            eprintln!("CreateWindowExW failed");
            std::process::exit(1);
        }

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = *ICON_OFF.lock().unwrap() as _;
        let tip = wide("sing-box proxy: starting…");
        let n = tip.len().min(127);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        *NID.lock().unwrap() = SendNid(Some(nid));

        SetTimer(hwnd, 2, 5000, None); // icon-add retry clock
        log_line("proxy-tray started");

        let h = hwnd as usize;
        let h_guard = hwnd as usize;
        thread::spawn(move || poll_loop(h as HWND));
        thread::spawn(move || guard_loop(h_guard as HWND));
        update_icon(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&mut msg);
            DispatchMessageW(&mut msg);
        }
    }
}

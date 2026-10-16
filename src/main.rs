#![windows_subsystem = "windows"]
// singbox-watch — always-on-top warning banner for when the tunnel is down
// or bypassed. The ground truth it checks is the PUBLIC EXIT IP vs the pinned
// IP in expected-exit-ip.txt (next to the exe, or %USERPROFILE%\.singbox-watch\):
//   - exit IP matches            -> banner hidden, silent
//   - exit IP is something else  -> RED banner (traffic is exiting raw!)
//   - no connectivity (2x in a row) -> AMBER banner (blackholed / Wi-Fi down)
//   - proxy intentionally off (tray toggle) -> AMBER "proxy is off" banner
// Left-click the banner = snooze 2 minutes. Right-click = quit the watcher.
// Auto-start at login via HKCU Run key (--install / --uninstall).
// Other flags: --check-once (print state, exit), --test-banner (demo), --ip <addr>.

use std::env;
use std::fs;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use singbox_watch::{
    cli_print, expected_file, fetch_public_ip, install_autostart, now, proxy_state_off,
    uninstall_autostart, valid_ipv4, wide,
};

use winapi::ctypes::c_int;
use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::{HDC, HGDIOBJ, HWND, RECT};
use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::synchapi::CreateMutexW;
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

const WM_APP_STATE: UINT = WM_APP + 1;
const RUN_VALUE: &str = "SingBoxWatch";
const CHECK_INTERVAL_SECS: u64 = 15;
const GRACE_SECS: i64 = 120; // no banners during boot ramp (sing-box task has a 30s delay)
const BANNER_HEIGHT: c_int = 84;

// state: 0 = ok, 1 = leak (wrong exit IP), 2 = offline, 3 = config error
static STATE: AtomicU8 = AtomicU8::new(0);
static EXPECTED_IP: Mutex<String> = Mutex::new(String::new());
static LINES: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
static SNOOZE_UNTIL: AtomicI64 = AtomicI64::new(0);
static GRACE_END: AtomicI64 = AtomicI64::new(0);

fn set_lines(l1: &str, l2: &str) {
    *LINES.lock().unwrap() = (l1.to_string(), l2.to_string());
}

fn singbox_running() -> bool {
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
                let name = String::from_utf16_lossy(&pe.szExeFile[..len]);
                if name.eq_ignore_ascii_case("sing-box.exe") {
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

fn monitor_loop(hwnd: HWND) {
    let expected = EXPECTED_IP.lock().unwrap().clone();
    let mut consecutive_fail: u32 = 0;
    loop {
        let mut state = 0u8;
        match fetch_public_ip() {
            Some(live) => {
                consecutive_fail = 0;
                if live != expected {
                    if proxy_state_off() {
                        // intentional: user switched the proxy off via proxy-tray
                        state = 2;
                        set_lines(
                            "PROXY IS OFF — TRAFFIC EXITS FROM RAW HK IP",
                            &format!(
                                "Exit IP {live} (pinned {expected}) · Claude must stay CLOSED · re-enable: click the tray icon · left-click: snooze"
                            ),
                        );
                    } else {
                        state = 1;
                        let proc = if singbox_running() {
                            "RUNNING (routes misrouted?)"
                        } else {
                            "NOT RUNNING"
                        };
                        set_lines(
                            "SING-BOX DOWN — TUNNEL BYPASSED",
                            &format!(
                                "Exit IP is now {live} (expected {expected}) · sing-box.exe: {proc} · DO NOT USE tunneled apps — restart the tunnel task"
                            ),
                        );
                    }
                }
            }
            None => {
                consecutive_fail += 1;
                if consecutive_fail >= 2 {
                    state = 2;
                    let proc = if singbox_running() {
                        "RUNNING (blackholed?)"
                    } else {
                        "NOT RUNNING"
                    };
                    set_lines(
                        "NO CONNECTIVITY — tunnel blackholed or Wi-Fi down",
                        &format!(
                            "sing-box.exe: {proc} · do NOT retry Claude · if stuck: STOP-VPN-RESTORES-INTERNET.cmd, then start-vpn.cmd"
                        ),
                    );
                }
            }
        }
        STATE.store(state, Ordering::Relaxed);
        unsafe {
            PostMessageW(hwnd, WM_APP_STATE, state as WPARAM, 0);
        }
        thread::sleep(Duration::from_secs(CHECK_INTERVAL_SECS));
    }
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

unsafe fn reassert_topmost(hwnd: HWND) {
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        let state = STATE.load(Ordering::Relaxed);
        let (bg, fg) = match state {
            1 => (rgb(178, 24, 34), 0xFF_FFFF),          // red / white
            2 => (rgb(243, 168, 0), rgb(32, 24, 0)),      // amber / near-black
            _ => (rgb(64, 64, 64), 0xFF_FFFF),            // gray / white
        };
        let brush = CreateSolidBrush(bg);
        FillRect(hdc, &rc, brush);
        DeleteObject(brush as HGDIOBJ);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, fg);

        let (l1, l2) = {
            let g = LINES.lock().unwrap();
            (wide(&g.0), wide(&g.1))
        };
        let h = rc.bottom - rc.top;
        let split = rc.top + 6 + (h as f32 * 0.58) as i32;
        let mut r1 = RECT { left: 10, top: 6, right: rc.right - 10, bottom: split };
        let mut r2 = RECT { left: 10, top: split, right: rc.right - 10, bottom: rc.bottom - 4 };

        let face = wide("Segoe UI");
        let f1 = CreateFontW(
            30, 0, 0, 0, FW_BOLD, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_PITCH, face.as_ptr(),
        );
        let old = SelectObject(hdc, f1 as HGDIOBJ);
        DrawTextW(hdc, l1.as_ptr(), -1, &mut r1, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
        let f2 = CreateFontW(
            19, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_PITCH, face.as_ptr(),
        );
        SelectObject(hdc, f2 as HGDIOBJ);
        DrawTextW(hdc, l2.as_ptr(), -1, &mut r2, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
        SelectObject(hdc, old);
        DeleteObject(f1 as HGDIOBJ);
        DeleteObject(f2 as HGDIOBJ);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: UINT, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_STATE => {
            let s = wp as u8;
            let snoozed = now() < SNOOZE_UNTIL.load(Ordering::Relaxed);
            let in_grace = now() < GRACE_END.load(Ordering::Relaxed);
            if s == 0 || snoozed || in_grace {
                ShowWindow(hwnd, SW_HIDE);
            } else {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                reassert_topmost(hwnd);
            }
            InvalidateRect(hwnd, std::ptr::null(), 1);
            0
        }
        WM_TIMER => {
            if IsWindowVisible(hwnd) != 0 {
                reassert_topmost(hwnd);
            }
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONUP => {
            SNOOZE_UNTIL.store(now() + 120, Ordering::Relaxed);
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_RBUTTONUP => {
            PostQuitMessage(0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn load_expected(args: &[String]) -> Option<String> {
    if let Some(pos) = args.iter().position(|a| a == "--ip") {
        if let Some(v) = args.get(pos + 1) {
            if valid_ipv4(v) {
                return Some(v.trim().to_string());
            }
        }
        eprintln!("--ip given but not a valid IPv4, falling back to {}", expected_file().display());
    }
    fs::read_to_string(expected_file())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| valid_ipv4(t))
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

    // single instance
    unsafe {
        let name = wide("SingBoxWatchSingleton");
        CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            cli_print("singbox-watch is already running\n");
            return;
        }
    }

    match load_expected(&args) {
        Some(ip) => *EXPECTED_IP.lock().unwrap() = ip,
        None => {
            set_lines(
                "SINGBOX-WATCH CONFIG ERROR",
                &format!("No expected IP found (looked in {}) — fix the file or pass --ip <addr>", expected_file().display()),
            );
            STATE.store(3, Ordering::Relaxed);
        }
    }

    if args.iter().any(|a| a == "--check-once") {
        let exp = EXPECTED_IP.lock().unwrap().clone();
        cli_print(&format!(
            "expected : {}\n",
            if exp.is_empty() { "<none>".to_string() } else { exp.clone() }
        ));
        match fetch_public_ip() {
            Some(live) => {
                cli_print(&format!("exit IP  : {live}\n"));
                if exp.is_empty() {
                    cli_print("CONFIG   : no expected IP\n");
                    std::process::exit(3);
                }
                if live == exp {
                    cli_print("STATUS   : OK — tunnel healthy\n");
                    std::process::exit(0);
                } else {
                    cli_print("STATUS   : LEAK — exit IP mismatch!\n");
                    std::process::exit(1);
                }
            }
            None => {
                cli_print("STATUS   : OFFLINE — cannot reach ip-api.com\n");
                std::process::exit(2);
            }
        }
    }

    GRACE_END.store(now() + GRACE_SECS, Ordering::Relaxed);

    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class_name: &'static mut [u16] = Box::leak(wide("SingBoxWatchWnd").into_boxed_slice());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
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
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            wide("SingBoxWatch").as_ptr(),
            WS_POPUP,
            0, 0, screen_w, BANNER_HEIGHT,
            std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            eprintln!("CreateWindowExW failed");
            std::process::exit(1);
        }
        SetTimer(hwnd, 1, 5000, None);

        if args.iter().any(|a| a == "--test-banner") {
            set_lines(
                "SING-BOX DOWN — TUNNEL BYPASSED  (TEST BANNER)",
                "This is what the real warning looks like · left-click: snooze 2 min · right-click: quit singbox-watch",
            );
            STATE.store(1, Ordering::Relaxed);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            reassert_topmost(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 1);
        } else if STATE.load(Ordering::Relaxed) == 3 {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            reassert_topmost(hwnd);
        } else {
            let h = hwnd as usize; // raw HWND is not Send; pass as usize
            thread::spawn(move || monitor_loop(h as HWND));
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&mut msg);
            DispatchMessageW(&mut msg);
        }
    }
}

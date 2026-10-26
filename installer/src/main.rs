#![windows_subsystem = "windows"]
// singbox-watch-setup — one-file installer for the Windows edition. The tray
// switch, the banner watcher and the helper scripts are embedded at build
// time (build-installer.cmd builds the apps first, then this).
//
//   singbox-watch-setup.exe                          interactive install
//   singbox-watch-setup.exe --silent [--dir <path>]  unattended install
//   singbox-watch-setup.exe --uninstall [--silent]
//   singbox-watch-setup.exe --extract <dir>          write the files only
//
// Install = copy the files, login autostart for both apps (HKCU Run), the
// per-minute keep-alive task, an Apps & Features entry, then start both apps.
// Per-user throughout: no admin rights unless the chosen folder needs them.
// Upgrades in place: an existing install is found through its Run-key entry.

use std::env;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use singbox_watch::{cli_print, fetch_public_ip, read_reg_sz, uninstall_autostart, valid_ipv4, wide};

use winapi::shared::minwindef::{DWORD, LPARAM, UINT};
use winapi::shared::windef::HWND;
use winapi::um::combaseapi::{CoInitializeEx, CoTaskMemFree};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::objbase::COINIT_APARTMENTTHREADED;
use winapi::um::processthreadsapi::{OpenProcess, TerminateProcess};
use winapi::um::shlobj::SHGetPathFromIDListW;
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winapi::um::winbase::{QueryFullProcessImageNameW, CREATE_NO_WINDOW};
use winapi::um::winnt::{
    KEY_SET_VALUE, KEY_WRITE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use winapi::um::winreg::{RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW, HKEY_CURRENT_USER};
use winapi::um::winuser::{
    MessageBoxW, IDNO, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION,
    MB_SETFOREGROUND, MB_YESNO, MB_YESNOCANCEL,
};

const APP: &str = "singbox-watch";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SETUP_EXE: &str = "singbox-watch-setup.exe";
const KEEPALIVE_TASK: &str = "singbox-apps-keepalive";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\singbox-watch";
// Run-key value names the apps register themselves under (--install)
const RUN_VALUES: [&str; 2] = ["SingBoxTray", "SingBoxWatch"];
const APPS: [&str; 2] = ["proxy-tray.exe", "singbox-watch.exe"];

// everything the installer owns; uninstall removes exactly these, so the
// user's own files (config.json, expected-exit-ip.txt, logs) are never touched
const FILES: [(&str, &[u8]); 6] = [
    ("proxy-tray.exe", include_bytes!(concat!(env!("SINGBOX_BIN_DIR"), "/proxy-tray.exe"))),
    ("singbox-watch.exe", include_bytes!(concat!(env!("SINGBOX_BIN_DIR"), "/singbox-watch.exe"))),
    ("apply-tuning.cmd", include_bytes!("../../apply-tuning.cmd")),
    ("keep-alive.cmd", include_bytes!("../../keep-alive.cmd")),
    ("hidden.vbs", include_bytes!("../../hidden.vbs")),
    ("install-keepalive.cmd", include_bytes!("../../install-keepalive.cmd")),
];

static SILENT: AtomicBool = AtomicBool::new(false);

fn silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// Dialog in interactive mode; in --silent mode the text goes to the console
/// and `default` stands in for the answer.
fn ask(text: &str, flags: UINT, default: i32) -> i32 {
    if silent() {
        cli_print(&format!("{text}\n"));
        return default;
    }
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide(&format!("{APP} setup")).as_ptr(),
            flags | MB_SETFOREGROUND,
        )
    }
}

fn fail(text: &str) -> ! {
    ask(text, MB_ICONERROR, 0);
    std::process::exit(1);
}

fn hidden(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

fn processes_named(name: &str) -> Vec<u32> {
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
                if String::from_utf16_lossy(&pe.szExeFile[..len]).eq_ignore_ascii_case(name) {
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

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as DWORD;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// Stop both apps so their exes can be replaced. Any copy anywhere goes:
/// the single-instance lock would keep a freshly installed one from starting.
fn stop_apps() {
    for name in APPS {
        for pid in processes_named(name) {
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h.is_null() {
                    TerminateProcess(h, 0);
                    CloseHandle(h);
                }
            }
        }
    }
}

/// Folder of an existing install (from the tray's Run-key entry), else the
/// folder sing-box runs from ("put it next to sing-box"), else
/// %USERPROFILE%\singbox-watch. The label says which one it is.
fn default_dir() -> (PathBuf, &'static str) {
    if let Some(v) = read_reg_sz(false, RUN_KEY, RUN_VALUES[0]) {
        let exe = PathBuf::from(v.trim().trim_matches('"'));
        if let Some(d) = exe.parent().filter(|d| d.exists()) {
            return (d.to_path_buf(), "updates your existing install");
        }
    }
    for pid in processes_named("sing-box.exe") {
        if let Some(d) = process_path(pid).and_then(|p| p.parent().map(Path::to_path_buf)) {
            return (d, "next to your running sing-box");
        }
    }
    let home = env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    (PathBuf::from(home).join(APP), "default folder")
}

#[repr(C)]
struct BrowseInfoW {
    hwnd_owner: HWND,
    pidl_root: *const winapi::ctypes::c_void,
    display_name: *mut u16,
    title: *const u16,
    flags: UINT,
    callback: *const winapi::ctypes::c_void,
    lparam: LPARAM,
    image: i32,
}

#[link(name = "shell32")]
extern "system" {
    fn SHBrowseForFolderW(bi: *mut BrowseInfoW) -> *mut winapi::um::shtypes::ITEMIDLIST;
}

fn pick_folder() -> Option<PathBuf> {
    const BIF_RETURNONLYFSDIRS: UINT = 0x1;
    const BIF_EDITBOX: UINT = 0x10;
    const BIF_NEWDIALOGSTYLE: UINT = 0x40;
    unsafe {
        CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        let mut name = vec![0u16; 260];
        let title = wide("Install singbox-watch into this folder (tip: the folder with sing-box.exe and config.json):");
        let mut bi = BrowseInfoW {
            hwnd_owner: std::ptr::null_mut(),
            pidl_root: std::ptr::null(),
            display_name: name.as_mut_ptr(),
            title: title.as_ptr(),
            flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            callback: std::ptr::null(),
            lparam: 0,
            image: 0,
        };
        let pidl = SHBrowseForFolderW(&mut bi);
        if pidl.is_null() {
            return None;
        }
        let mut path = vec![0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
        CoTaskMemFree(pidl as *mut _);
        let len = path.iter().position(|&c| c == 0).unwrap_or(0);
        (ok != 0 && len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&path[..len])))
    }
}

fn write_files(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    for (name, bytes) in FILES {
        let dest = dir.join(name);
        let mut tries = 0;
        // an exe can stay locked briefly after TerminateProcess, and the
        // keep-alive task may relaunch the old copy mid-install
        while let Err(e) = fs::write(&dest, bytes) {
            tries += 1;
            if tries >= 10 {
                return Err(format!("can't write {}: {e}", dest.display()));
            }
            stop_apps();
            thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(())
}

fn reg_set(key: &str, values: &[(&str, Result<&str, DWORD>)]) -> bool {
    unsafe {
        let mut hk = std::mem::zeroed();
        let sub = wide(key);
        if RegCreateKeyExW(
            HKEY_CURRENT_USER, sub.as_ptr(), 0, std::ptr::null_mut(), REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_SET_VALUE, std::ptr::null_mut(), &mut hk, std::ptr::null_mut(),
        ) != 0
        {
            return false;
        }
        let mut ok = true;
        for (name, val) in values {
            let n = wide(name);
            let rc = match val {
                Ok(s) => {
                    let d = wide(s);
                    RegSetValueExW(hk, n.as_ptr(), 0, REG_SZ, d.as_ptr() as *const u8, (d.len() * 2) as u32)
                }
                Err(dw) => RegSetValueExW(hk, n.as_ptr(), 0, REG_DWORD, dw as *const DWORD as *const u8, 4),
            };
            ok &= rc == 0;
        }
        RegCloseKey(hk);
        ok
    }
}

fn register_uninstall(dir: &Path) -> bool {
    let setup = dir.join(SETUP_EXE).display().to_string();
    let loc = dir.display().to_string();
    let icon = dir.join("proxy-tray.exe").display().to_string();
    let uninstall = format!("\"{setup}\" --uninstall");
    let quiet = format!("\"{setup}\" --uninstall --silent");
    reg_set(
        UNINSTALL_KEY,
        &[
            ("DisplayName", Ok(APP)),
            ("DisplayVersion", Ok(VERSION)),
            ("Publisher", Ok("acus9345")),
            ("InstallLocation", Ok(&loc)),
            ("DisplayIcon", Ok(&icon)),
            ("UninstallString", Ok(&uninstall)),
            ("QuietUninstallString", Ok(&quiet)),
            ("NoModify", Err(1)),
            ("NoRepair", Err(1)),
        ],
    )
}

fn create_keepalive_task(dir: &Path) -> bool {
    let vbs = dir.join("hidden.vbs").display().to_string();
    // raw_arg: schtasks wants the inner quotes exactly as install-keepalive.cmd writes them
    hidden(&mut Command::new("schtasks"))
        .raw_arg(format!(
            "/Create /TN \"{KEEPALIVE_TASK}\" /TR \"wscript.exe \\\"{vbs}\\\"\" /SC MINUTE /MO 1 /F"
        ))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Offer to pin the current public IP when no expected-exit-ip.txt exists
/// yet (next to the apps or in %USERPROFILE%\.singbox-watch, where they look).
fn pin_exit_ip(dir: &Path) -> String {
    let home_file = PathBuf::from(env::var("USERPROFILE").unwrap_or_default())
        .join(".singbox-watch")
        .join("expected-exit-ip.txt");
    let local_file = dir.join("expected-exit-ip.txt");
    if local_file.exists() || home_file.exists() {
        return "pinned exit IP: already set".into();
    }
    let Some(ip) = fetch_public_ip().filter(|ip| valid_ipv4(ip)) else {
        return format!("pinned exit IP: NOT SET — put your tunnel's exit IP in {}", local_file.display());
    };
    let answer = ask(
        &format!(
            "No pinned exit IP yet — the apps compare against it to tell ON from OFF.\n\n\
             Your public IP right now is {ip}.\n\n\
             Is the proxy ON at the moment, so this is your tunnel's exit IP?\n\n\
             Yes — pin {ip}\nNo — skip (create expected-exit-ip.txt yourself later)"
        ),
        MB_YESNO | MB_ICONQUESTION,
        IDNO,
    );
    if answer == IDYES && fs::write(&local_file, &ip).is_ok() {
        format!("pinned exit IP: {ip}")
    } else {
        format!("pinned exit IP: NOT SET — put your tunnel's exit IP in {}", local_file.display())
    }
}

fn install(dir_arg: Option<PathBuf>) {
    let dir = match dir_arg {
        Some(d) => d,
        None => {
            let (d, why) = default_dir();
            if silent() {
                d
            } else {
                match ask(
                    &format!(
                        "Install {APP} {VERSION} to:\n\n    {}\n    ({why})\n\n\
                         Yes — install here\nNo — choose another folder\nCancel — quit",
                        d.display()
                    ),
                    MB_YESNOCANCEL | MB_ICONQUESTION,
                    IDYES,
                ) {
                    IDYES => d,
                    IDNO => pick_folder().unwrap_or_else(|| std::process::exit(0)),
                    _ => std::process::exit(0),
                }
            }
        }
    };

    stop_apps();
    if let Err(e) = write_files(&dir) {
        fail(&format!(
            "Install failed — {e}\n\nIf the folder is under Program Files, run setup as administrator or pick another folder."
        ));
    }
    // keep a copy of setup in the install folder: Apps & Features runs it to uninstall
    if let Ok(me) = env::current_exe() {
        let dest = dir.join(SETUP_EXE);
        if fs::canonicalize(&me).ok() != fs::canonicalize(&dest).ok() {
            let _ = fs::copy(&me, &dest);
        }
    }

    let mut report = vec![format!("installed to {}", dir.display())];
    for exe in APPS {
        let ok = hidden(&mut Command::new(dir.join(exe)))
            .arg("--install")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        report.push(format!("autostart {exe}: {}", if ok { "on" } else { "FAILED" }));
    }
    report.push(format!(
        "keep-alive task: {}",
        if create_keepalive_task(&dir) { "every minute" } else { "FAILED (run install-keepalive.cmd)" }
    ));
    report.push(format!(
        "Apps & Features entry: {}",
        if register_uninstall(&dir) { "added" } else { "FAILED" }
    ));
    report.push(pin_exit_ip(&dir));

    // via explorer, like keep-alive.cmd: the apps must not inherit this
    // process's job object or console
    for exe in APPS {
        let _ = Command::new("explorer.exe").arg(dir.join(exe)).spawn();
    }
    report.push("both apps started".into());

    ask(
        &format!(
            "{APP} {VERSION} is installed.\n\n{}\n\n\
             The tray icon may land in the hidden-icons overflow (^ on the taskbar) — \
             drag it onto the taskbar to keep it visible. Right-click it for the menu.",
            report.iter().map(|l| format!("•  {l}")).collect::<Vec<_>>().join("\n")
        ),
        MB_ICONINFORMATION,
        0,
    );
}

fn uninstall() {
    let dir = read_reg_sz(false, UNINSTALL_KEY, "InstallLocation")
        .map(PathBuf::from)
        .or_else(|| env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)))
        .unwrap_or_else(|| fail("Can't find the install folder."));
    if ask(
        &format!(
            "Remove {APP} from {}?\n\nYour own files there (config.json, expected-exit-ip.txt, logs) are kept.",
            dir.display()
        ),
        MB_YESNO | MB_ICONQUESTION,
        IDYES,
    ) != IDYES
    {
        return;
    }
    stop_apps();
    let _ = hidden(Command::new("schtasks").args(["/Delete", "/TN", KEEPALIVE_TASK, "/F"])).status();
    for v in RUN_VALUES {
        uninstall_autostart(v);
    }
    for (name, _) in FILES {
        let _ = fs::remove_file(dir.join(name));
    }
    unsafe {
        RegDeleteKeyW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr());
    }
    // a running exe can't delete itself: a hidden cmd removes setup once we've
    // exited, then the folder — rmdir only succeeds if nothing else is left
    let setup = dir.join(SETUP_EXE);
    if setup.exists() {
        let _ = hidden(&mut Command::new("cmd.exe"))
            .raw_arg(format!(
                "/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"",
                setup.display(),
                dir.display()
            ))
            .spawn();
    } else {
        let _ = fs::remove_dir(&dir);
    }
    ask(&format!("{APP} was removed."), MB_ICONINFORMATION, 0);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let value_of = |flag: &str| {
        args.iter()
            .position(|a| a.eq_ignore_ascii_case(flag))
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    let has = |flag: &str| args.iter().any(|a| a.eq_ignore_ascii_case(flag));
    SILENT.store(has("--silent") || has("/S"), Ordering::Relaxed);

    if has("--extract") {
        let Some(dir) = value_of("--extract") else {
            fail("usage: singbox-watch-setup.exe --extract <dir>");
        };
        match write_files(&dir) {
            Ok(()) => {
                for (name, bytes) in FILES {
                    cli_print(&format!("{name}  ({} bytes)\n", bytes.len()));
                }
                cli_print(&format!("extracted to {}\n", dir.display()));
            }
            Err(e) => fail(&e),
        }
        return;
    }
    if has("--uninstall") {
        uninstall();
        return;
    }
    install(value_of("--dir"));
}

//! Small Win32 helpers: single instance, window focus, autostart, spawning.

use std::path::Path;
use std::process::Command;

pub const WINDOW_TITLE: &str = "Prune";
/// Name of the HKCU Run value (and the one used before the rename).
const RUN_VALUE: &str = "Prune";
const LEGACY_RUN_VALUE: &str = "DevSweep";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Print to the parent terminal when started from one (release builds have no console).
pub fn attach_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

/// Holds a named mutex for the lifetime of the process.
pub struct Instance(#[allow(dead_code)] isize);

/// Returns None when another process already holds `name`.
pub fn single_instance(name: &str) -> Option<Instance> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let n = wide(name);
        let h = CreateMutexW(std::ptr::null(), 1, n.as_ptr());
        if h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(Instance(h as isize))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Some(Instance(0))
    }
}

pub fn is_running(name: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
        let n = wide(name);
        let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, n.as_ptr());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

/// Bring an already-open Prune window to the front. Returns false if none.
pub fn focus_window() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
        let t = wide(WINDOW_TITLE);
        let h = FindWindowW(std::ptr::null(), t.as_ptr());
        if h.is_null() {
            return false;
        }
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
        true
    }
    #[cfg(not(windows))]
    false
}

/// Start another copy of this exe, detached, with the given args.
pub fn spawn_self(args: &[&str]) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    let _ = cmd.spawn();
}

pub const TRAY_MUTEX: &str = "Local\\Prune.Tray";
pub const GUI_MUTEX: &str = "Local\\Prune.Window";

pub fn ensure_tray() {
    if !is_running(TRAY_MUTEX) {
        spawn_self(&["--tray"]);
    }
}

/// Open the window, or focus it when it's already open.
pub fn open_window() {
    if !focus_window() {
        spawn_self(&[]);
    }
}

pub fn reveal(path: &Path) {
    let _ = Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
}

/// Windows "app mode" is dark (HKCU ...\Themes\Personalize AppsUseLightTheme = 0).
pub fn system_dark() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let k = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let v = wide("AppsUseLightTheme");
        let mut data: u32 = 1;
        let mut size: u32 = 4;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        );
        r == 0 && data == 0
    }
    #[cfg(not(windows))]
    false
}

/// Ask a running window to show a page (read when it gains focus).
pub fn request_page(page: &str) {
    let dir = prune_core::settings::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("open-page"), page);
}

pub fn take_page_request() -> Option<String> {
    let path = prune_core::settings::data_dir().join("open-page");
    let s = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Open the window on a page, or switch the open window to it.
pub fn open_window_page(page: &str) {
    // Write the request first so the window sees it when it gets focus.
    request_page(page);
    if !focus_window() {
        let _ = take_page_request();
        spawn_self(&["--page", page]);
    }
}

/// Give unused pages back to Windows (like Mem Reduct). Most of the window's
/// memory is the graphics driver, which it doesn't need while in the background.
pub fn trim_memory() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

fn run_value_exists(name: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
        let k = wide(RUN_KEY);
        let v = wide(name);
        let mut size: u32 = 0;
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) == 0
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

/// Whether the HKCU Run entry exists (under the current or the pre-rename name).
pub fn autostart_enabled() -> bool {
    run_value_exists(RUN_VALUE) || run_value_exists(LEGACY_RUN_VALUE)
}

/// A "DevSweep" Run entry from before the rename becomes a "Prune" entry
/// pointing at this exe.
pub fn migrate_autostart() {
    if run_value_exists(LEGACY_RUN_VALUE) {
        set_autostart(true);
    }
}

/// Add/remove the HKCU Run entry that starts the tray at login.
pub fn set_autostart(on: bool) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
        };
        let k = wide(RUN_KEY);
        let v = wide(RUN_VALUE);
        let mut key: HKEY = std::ptr::null_mut();
        if RegCreateKeyW(HKEY_CURRENT_USER, k.as_ptr(), &mut key) != 0 {
            return false;
        }
        // Drop the pre-rename entry either way.
        let legacy = wide(LEGACY_RUN_VALUE);
        RegDeleteValueW(key, legacy.as_ptr());
        let ok = if on {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let data = wide(&format!("\"{exe}\" --tray"));
            RegSetValueExW(key, v.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) == 0
        } else {
            let r = RegDeleteValueW(key, v.as_ptr());
            r == 0 || r == 2 // ERROR_FILE_NOT_FOUND
        };
        RegCloseKey(key);
        ok
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        false
    }
}

//! Background residency: notification-area icon, global hotkey and a single
//! running instance. Together they let Glint stay resident (instant to show)
//! while hibernated at ~0 RAM.

use std::ptr::{null, null_mut};

use windows_sys::w;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HWND,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_SPACE,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Tray callback message (lParam = mouse message).
pub const WM_APP_TRAY: u32 = WM_APP + 10;
/// Posted by a second launch: bring the window up.
pub const WM_APP_SHOW: u32 = WM_APP + 11;
pub const HOTKEY_ID: i32 = 1;
pub const HOTKEY_TEXT: &str = "Ctrl+Alt+Space";
pub const CLASS: *const u16 = w!("GlintMainWindow");

const ICON_ID: u32 = 1;
const MUTEX_NAME: *const u16 = w!("Local\\Glint.SingleInstance");

/// Process-wide "Glint is running" marker.
pub struct Instance(HANDLE);

impl Instance {
    /// `None` if another instance runs; it has been asked to show itself.
    pub fn acquire() -> Option<Instance> {
        // SAFETY: static NUL-terminated name; default security.
        let h = unsafe { CreateMutexW(null(), 0, MUTEX_NAME) };
        // SAFETY: reads this thread's last-error value, set by CreateMutexW.
        let err = unsafe { GetLastError() };
        // An elevated instance's mutex denies unelevated callers: still "running".
        let running = if h.is_null() {
            err == ERROR_ACCESS_DENIED
        } else {
            err == ERROR_ALREADY_EXISTS
        };
        if !running {
            return Some(Instance(h)); // (null on unexpected errors: run unguarded)
        }
        if !h.is_null() {
            // SAFETY: `h` is our own handle; the other process keeps the mutex.
            unsafe { CloseHandle(h) };
        }
        // SAFETY: static class name; posting to another process's window is
        // allowed for WM_APP_SHOW (see `allow_show_from_other_integrity`).
        unsafe {
            let other = FindWindowW(CLASS, null());
            if !other.is_null() {
                AllowSetForegroundWindow(ASFW_ANY);
                PostMessageW(other, WM_APP_SHOW, 0, 0);
            }
        }
        None
    }

    /// Let an elevated relaunch start while we shut down.
    pub fn release(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle owned by us, closed exactly once.
            unsafe { CloseHandle(self.0) };
            self.0 = null_mut();
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.release();
    }
}

/// An unelevated second launch may still wake an elevated instance.
/// Also lets an elevated window hear the TaskbarCreated broadcast from Explorer.
pub unsafe fn allow_show_from_other_integrity(hwnd: HWND, taskbar_created: u32) {
    ChangeWindowMessageFilterEx(hwnd, WM_APP_SHOW, MSGFLT_ALLOW, null_mut());
    if taskbar_created != 0 {
        ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, null_mut());
    }
}

pub unsafe fn register_hotkey(hwnd: HWND) -> bool {
    RegisterHotKey(
        hwnd,
        HOTKEY_ID,
        MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
        VK_SPACE as u32,
    ) != 0
}

pub unsafe fn unregister_hotkey(hwnd: HWND) {
    UnregisterHotKey(hwnd, HOTKEY_ID);
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: POD struct; zero is a valid initial state.
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = ICON_ID;
    d
}

/// Add the tray icon. False when there is no taskbar (e.g. Explorer not running).
pub unsafe fn add_icon(hwnd: HWND, hotkey: bool) -> bool {
    let mut d = icon_data(hwnd);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = WM_APP_TRAY;
    d.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    let tip = if hotkey {
        format!("Glint — {HOTKEY_TEXT} to search")
    } else {
        "Glint".to_string()
    };
    for (dst, src) in d.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    Shell_NotifyIconW(NIM_ADD, &d) != 0
}

pub unsafe fn remove_icon(hwnd: HWND) {
    let d = icon_data(hwnd);
    Shell_NotifyIconW(NIM_DELETE, &d);
}

/// Broadcast when Explorer restarts: the icon must be re-added.
pub unsafe fn taskbar_created_message() -> u32 {
    RegisterWindowMessageW(w!("TaskbarCreated"))
}

pub const CMD_TRAY_SHOW: usize = 1100;
pub const CMD_TRAY_EXIT: usize = 1101;

/// Right-click menu. Returns the chosen command (0 if dismissed).
pub unsafe fn context_menu(hwnd: HWND) -> usize {
    let m = CreatePopupMenu();
    AppendMenuW(m, MF_STRING, CMD_TRAY_SHOW, w!("&Show Glint"));
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, CMD_TRAY_EXIT, w!("E&xit"));
    SetMenuDefaultItem(m, CMD_TRAY_SHOW as u32, 0);
    let mut pt = std::mem::zeroed();
    GetCursorPos(&mut pt);
    // Required so the menu closes when clicking elsewhere.
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        m,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        null(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
    cmd.max(0) as usize
}

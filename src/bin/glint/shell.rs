//! Shell integration: icons, opening files, clipboard, elevation.

use std::collections::HashMap;
use std::ptr::{null, null_mut};

use glint::volume::wide;
use windows_sys::w;
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Shell::{
    SHGetFileInfoW, ShellExecuteW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX,
    SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CF_UNICODETEXT: u32 = 13;

/// System image list indices, looked up by extension only (never touches
/// the file, so it stays fast for any number of rows) and cached.
pub struct Icons {
    by_ext: HashMap<String, i32>,
    folder: i32,
    list: isize,
}

unsafe fn sys_icon(name: &str, attrs: u32) -> (i32, isize) {
    let p = wide(name);
    let mut sfi: SHFILEINFOW = std::mem::zeroed();
    let h = SHGetFileInfoW(
        p.as_ptr(),
        attrs,
        &mut sfi,
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
    );
    (sfi.iIcon, h as isize)
}

impl Icons {
    pub fn new() -> Icons {
        // SAFETY: SHGetFileInfoW with USEFILEATTRIBUTES does not touch the disk.
        let (folder, list) = unsafe { sys_icon("folder", FILE_ATTRIBUTE_DIRECTORY) };
        Icons {
            by_ext: HashMap::new(),
            folder,
            list,
        }
    }

    pub fn image_list(&self) -> isize {
        self.list
    }

    pub fn index_for(&mut self, name: &str, dir: bool) -> i32 {
        if dir {
            return self.folder;
        }
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        if let Some(&i) = self.by_ext.get(&ext) {
            return i;
        }
        // SAFETY: as above.
        let (i, _) = unsafe { sys_icon(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL) };
        if self.by_ext.len() < 4096 {
            self.by_ext.insert(ext, i);
        }
        i
    }
}

/// Open a file, or reveal it in Explorer.
pub fn open(hwnd: HWND, path: &str, reveal: bool) {
    // SAFETY: all strings are NUL-terminated and outlive the call.
    unsafe {
        if reveal {
            let args = wide(&format!("/select,\"{path}\""));
            ShellExecuteW(
                hwnd,
                w!("open"),
                w!("explorer.exe"),
                args.as_ptr(),
                null(),
                SW_SHOWNORMAL,
            );
        } else {
            let p = wide(path);
            ShellExecuteW(hwnd, w!("open"), p.as_ptr(), null(), null(), SW_SHOWNORMAL);
        }
    }
}

pub fn copy_text(hwnd: HWND, text: &str) {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // SAFETY: standard clipboard protocol; ownership of the HGLOBAL passes to
    // the system on a successful SetClipboardData, otherwise we free it.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !mem.is_null() {
            let dst = GlobalLock(mem) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
                GlobalUnlock(mem);
                if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                    GlobalFree(mem);
                }
            } else {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
    }
}

/// Start this executable elevated (UAC prompt). True if it launched.
pub fn relaunch_elevated(hwnd: HWND) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = wide(&exe.to_string_lossy());
    // SAFETY: NUL-terminated strings, valid window.
    let r = unsafe {
        ShellExecuteW(
            hwnd,
            w!("runas"),
            exe.as_ptr(),
            null(),
            null_mut::<u16>(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

//! The result list: a virtual list view whose cells are formatted on demand.

use std::ptr::null;

use glint::fmt;
use glint::search::SortKey;
use windows_sys::w;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_APPS, VK_CONTROL};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{App, CMD_COPY_NAME, CMD_COPY_PATH, CMD_OPEN, CMD_OPEN_FOLDER};

impl App {
    pub(crate) unsafe fn take_results(&self) {
        let Some(r) = self.worker.take() else { return };
        if let Some(err) = r.error {
            self.set_status_part(0, &err);
            return;
        }
        let n = r.results.len();
        // Rows shift on a live refresh: remember the focused entry, not its row.
        let focused = if r.reset || r.epoch != self.epoch.get() {
            None
        } else {
            let row = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_FOCUSED as isize,
            );
            usize::try_from(row)
                .ok()
                .and_then(|row| self.results.borrow().get(row))
        };
        *self.results.borrow_mut() = r.results;
        self.epoch.set(r.epoch);
        self.last_ms.set(r.millis);
        if r.reset {
            let clear = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                ..std::mem::zeroed()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &clear as *const LVITEMW as isize,
            );
            SendMessageW(self.list, LVM_SETITEMCOUNT, n, 0);
            if n > 0 {
                SendMessageW(self.list, LVM_ENSUREVISIBLE, 0, 0);
            }
        } else {
            let clear = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                ..std::mem::zeroed()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &clear as *const LVITEMW as isize,
            );
            SendMessageW(self.list, LVM_SETITEMCOUNT, n, LVSICF_NOSCROLL as isize);
            let row = focused.and_then(|e| self.results.borrow().iter().position(|x| x == e));
            if let Some(row) = row {
                let both = LVIS_SELECTED | LVIS_FOCUSED;
                let set = LVITEMW {
                    state: both,
                    stateMask: both,
                    ..std::mem::zeroed()
                };
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    row,
                    &set as *const LVITEMW as isize,
                );
            }
        }
        InvalidateRect(self.list, null(), 0);
        self.set_status_part(
            0,
            &format!("{} objects   ({:.1} ms)", fmt::group(n as u64), r.millis),
        );
    }

    pub(crate) unsafe fn focus_list(&self) {
        SetFocus(self.list);
        let focused = SendMessageW(
            self.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_FOCUSED as isize,
        );
        if focused < 0 && !self.results.borrow().is_empty() {
            let it = LVITEMW {
                state: LVIS_SELECTED | LVIS_FOCUSED,
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                ..std::mem::zeroed()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                0,
                &it as *const LVITEMW as isize,
            );
        }
    }

    pub(crate) unsafe fn on_notify(&self, lp: LPARAM) -> LRESULT {
        let hdr = &*(lp as *const NMHDR);
        if hdr.hwndFrom != self.list {
            return 0;
        }
        match hdr.code {
            LVN_GETDISPINFOW => self.dispinfo(&mut *(lp as *mut NMLVDISPINFOW)),
            NM_DBLCLK => {
                if (*(lp as *const NMITEMACTIVATE)).iItem >= 0 {
                    self.command(CMD_OPEN);
                }
            }
            NM_RETURN => self.command(CMD_OPEN),
            NM_RCLICK => self.context_menu(),
            LVN_COLUMNCLICK => self.sort_by((*(lp as *const NMLISTVIEW)).iSubItem),
            LVN_KEYDOWN => {
                let k = std::ptr::read_unaligned(lp as *const NMLVKEYDOWN);
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                match k.wVKey {
                    0x43 if ctrl => self.command(CMD_COPY_PATH), // Ctrl+C
                    VK_APPS => self.context_menu(),
                    _ => {}
                }
            }
            _ => {}
        }
        0
    }

    /// Fill one cell on demand.
    unsafe fn dispinfo(&self, di: &mut NMLVDISPINFOW) {
        let item = &mut di.item;
        let results = self.results.borrow();
        let Some(e) = usize::try_from(item.iItem)
            .ok()
            .and_then(|r| results.get(r))
        else {
            return;
        };
        let ix = self.engine.read();
        if ix.epoch != self.epoch.get() || e as usize >= ix.len() {
            if item.mask & LVIF_TEXT != 0 {
                write_text(item, "");
            }
            return; // stale until the re-query arrives
        }
        if item.mask & LVIF_TEXT != 0 {
            match item.iSubItem {
                0 => write_text(item, ix.name(e)),
                1 => {
                    let mut p = String::new();
                    ix.parent_path_into(e, &mut p);
                    write_text(item, &p);
                }
                2 => write_text(item, &fmt::size_kb(ix.size(e))),
                3 => write_text(item, &fmt::date(ix.mtime(e))),
                _ => write_text(item, ""),
            }
        }
        if item.mask & LVIF_IMAGE != 0 && item.iSubItem == 0 {
            item.iImage = self.icons.borrow_mut().index_for(ix.name(e), ix.is_dir(e));
        }
    }

    /// Full paths of selected rows (capped for very large selections).
    pub(crate) unsafe fn selected_paths(&self, names_only: bool) -> Vec<String> {
        let results = self.results.borrow();
        let ix = self.engine.read();
        if ix.epoch != self.epoch.get() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i: isize = -1;
        while out.len() < 100_000 {
            i = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                i as usize,
                LVNI_SELECTED as isize,
            );
            let Some(e) = usize::try_from(i).ok().and_then(|r| results.get(r)) else {
                break;
            };
            if (e as usize) < ix.len() {
                out.push(if names_only {
                    ix.name(e).to_string()
                } else {
                    ix.full_path(e)
                });
            }
        }
        out
    }

    unsafe fn context_menu(&self) {
        if SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) == 0 {
            return;
        }
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, CMD_OPEN, w!("&Open"));
        AppendMenuW(m, MF_STRING, CMD_OPEN_FOLDER, w!("Open &containing folder"));
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, CMD_COPY_PATH, w!("Copy full &path\tCtrl+C"));
        AppendMenuW(m, MF_STRING, CMD_COPY_NAME, w!("Copy &name"));
        SetMenuDefaultItem(m, CMD_OPEN as u32, 0);
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            self.hwnd,
            null(),
        );
        DestroyMenu(m);
        if cmd > 0 {
            self.command(cmd as usize);
        }
    }

    unsafe fn sort_by(&self, col: i32) {
        let key = match col {
            1 => SortKey::Path,
            2 => SortKey::Size,
            3 => SortKey::Date,
            _ => SortKey::Name,
        };
        let (cur, desc) = self.sort.get();
        let desc = if cur == key {
            !desc
        } else {
            matches!(key, SortKey::Size | SortKey::Date)
        };
        self.sort.set((key, desc));
        self.set_sort_arrow();
        self.query(true);
    }

    pub(crate) unsafe fn set_sort_arrow(&self) {
        let header = SendMessageW(self.list, LVM_GETHEADER, 0, 0) as HWND;
        let (key, desc) = self.sort.get();
        let active = match key {
            SortKey::Name => 0,
            SortKey::Path => 1,
            SortKey::Size => 2,
            SortKey::Date => 3,
        };
        for col in 0..4 {
            let mut it = HDITEMW {
                mask: HDI_FORMAT,
                ..std::mem::zeroed()
            };
            SendMessageW(header, HDM_GETITEMW, col, &mut it as *mut HDITEMW as isize);
            it.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if col == active {
                it.fmt |= if desc { HDF_SORTDOWN } else { HDF_SORTUP };
            }
            SendMessageW(header, HDM_SETITEMW, col, &it as *const HDITEMW as isize);
        }
    }
}

unsafe fn write_text(item: &mut LVITEMW, s: &str) {
    if item.pszText.is_null() || item.cchTextMax <= 0 {
        return;
    }
    let cap = item.cchTextMax as usize - 1;
    let mut n = 0;
    for u in s.encode_utf16().take(cap) {
        *item.pszText.add(n) = u;
        n += 1;
    }
    *item.pszText.add(n) = 0;
}

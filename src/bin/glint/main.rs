//! Glint GUI: a plain Win32 window with a search box and a virtual
//! (owner-data) list view. Rows are formatted on demand, so a list of
//! millions of results costs nothing beyond the 4-byte id per result.

#![windows_subsystem = "windows"]

mod list;
mod residency;
mod shell;
mod tray;

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::Arc;

use glint::engine::{Engine, Event};
use glint::search::{Hits, Request, SortKey, Worker};
use glint::volume::wide;
use windows_sys::w;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_RESULTS: u32 = WM_APP + 1;
const WM_APP_READY: u32 = WM_APP + 2;
const WM_APP_CHANGED: u32 = WM_APP + 3;
const WM_APP_STATUS: u32 = WM_APP + 4;

const ID_EDIT: usize = 100;
const ID_LIST: usize = 101;
const ID_STATUS: usize = 102;

const CMD_REBUILD: usize = 1001;
const CMD_ADMIN: usize = 1002;
const CMD_EXIT: usize = 1003;
const CMD_SYNTAX: usize = 1004;
pub(crate) const CMD_OPEN: usize = 1010;
pub(crate) const CMD_OPEN_FOLDER: usize = 1011;
pub(crate) const CMD_COPY_PATH: usize = 1012;
pub(crate) const CMD_COPY_NAME: usize = 1013;

const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 400;
const TIMER_HIBERNATE: usize = 2;
/// Hidden/minimized this long => free the index (a quick re-show stays instant).
const HIBERNATE_MS: u32 = 3_000;
const HIBERNATE_RETRY_MS: u32 = 5_000;
const HIBERNATE_RETRY_MAX_MS: u32 = 5 * 60_000;
/// Posted by the hibernation thread; wParam = 1 on success.
const WM_APP_HIBERNATED: u32 = WM_APP + 5;
const EM_SETCUEBANNER: u32 = 0x1501;

const SYNTAX: &str = "foo bar\tboth words (AND)\n\
\"two words\"\tphrase\n\
!tmp\t\tNOT\n\
jpg|png\t\teither\n\
*.rs  a?c.*\twildcards (whole name)\n\
windows\\fonts\tterm with \\ matches the full path\n\
ext:jpg;png\textensions\n\
size:>10mb\tsize filter (< <= = >= >, kb/mb/gb); folders: total size\n\
file:  folder:\ttype filters, e.g. folder:src\n\
regex:^a.*z$\tregular expression\n\
dm:today  dm:7d\tmodified today / in the last 7 days (also h, w)\n\
dm:2024-05\tmodified in May 2024 (YYYY[-MM[-DD]], < > ranges a..b)\n\
attrib:hs\thidden and system (h s r l=link)\n\
depth:<=2  len:>100\tfolder depth, name length\n\
parent:c:\\temp\tdirectly inside a folder\n\n\
Enter/double-click: open   Ctrl+C: copy path   F5: rebuild index   Ctrl+F: search box
Ctrl+Alt+Space: show/hide from anywhere. Closing keeps Glint in the tray at ~0 RAM.";

struct App {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    status: HWND,
    engine: Arc<Engine>,
    worker: Worker,
    results: RefCell<Arc<Hits>>,
    epoch: Cell<u64>,
    sort: Cell<(SortKey, bool)>,
    icons: RefCell<shell::Icons>,
    font: Cell<HFONT>,
    dpi: Cell<u32>,
    refresh_pending: Cell<bool>,
    skip_save: Cell<bool>,
    last_ms: Cell<f64>,
    /// Closing hides to the tray instead of exiting.
    in_tray: Cell<bool>,
    exiting: Cell<bool>,
    taskbar_created: u32,
    instance: RefCell<Option<tray::Instance>>,
    hotkey: Cell<bool>,
    hibernating: Cell<bool>,
    retry_ms: Cell<u32>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn app() -> Option<Rc<App>> {
    APP.with(|a| a.borrow().clone())
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn main() {
    // SAFETY: straightforward Win32 initialisation and message loop; every
    // pointer passed refers to a live local or a static wide string.
    unsafe {
        // Second launch: surface the running instance and leave immediately.
        let Some(instance) = tray::Instance::acquire() else {
            return;
        };
        let start_hidden = std::env::args().any(|a| a == "--tray");
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let hinst = GetModuleHandleW(null());
        let class = tray::CLASS;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszClassName: class,
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);

        let admin = glint::volume::is_admin();
        let title = wide(if admin {
            "Glint (Administrator)"
        } else {
            "Glint"
        });
        let menu = build_menu(admin);
        let hwnd = CreateWindowExW(
            0,
            class,
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            null_mut(),
            menu,
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        let dpi = GetDpiForWindow(hwnd).max(96);
        let child = |cls: *const u16, style: u32, id: usize| {
            CreateWindowExW(
                0,
                cls,
                null(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                hwnd,
                id as HMENU,
                hinst,
                null(),
            )
        };
        let edit = child(
            w!("EDIT"),
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
            ID_EDIT,
        );
        let list = child(
            WC_LISTVIEWW,
            WS_TABSTOP | LVS_REPORT | LVS_OWNERDATA | LVS_SHAREIMAGELISTS | LVS_SHOWSELALWAYS,
            ID_LIST,
        );
        let status = child(STATUSCLASSNAMEW, SBARS_SIZEGRIP, ID_STATUS);

        let h = hwnd as isize;
        let engine = Engine::new(glint::default_db_path(), move |ev| {
            let msg = match ev {
                Event::Ready => WM_APP_READY,
                Event::Changed => WM_APP_CHANGED,
                Event::Status => WM_APP_STATUS,
            };
            // Posting to a window handle is thread-safe; a stale handle
            // after exit is rejected by the OS.
            PostMessageW(h as HWND, msg, 0, 0);
        });
        let worker = Worker::spawn(engine.clone(), move || {
            PostMessageW(h as HWND, WM_APP_RESULTS, 0, 0);
        });

        let a = Rc::new(App {
            hwnd,
            edit,
            list,
            status,
            engine,
            worker,
            results: RefCell::new(Arc::new(Hits::default())),
            epoch: Cell::new(u64::MAX),
            sort: Cell::new((SortKey::Name, false)),
            icons: RefCell::new(shell::Icons::new()),
            font: Cell::new(null_mut()),
            dpi: Cell::new(dpi),
            refresh_pending: Cell::new(false),
            skip_save: Cell::new(false),
            last_ms: Cell::new(0.0),
            in_tray: Cell::new(false),
            exiting: Cell::new(false),
            taskbar_created: tray::taskbar_created_message(),
            instance: RefCell::new(Some(instance)),
            hotkey: Cell::new(false),
            hibernating: Cell::new(false),
            retry_ms: Cell::new(HIBERNATE_RETRY_MS),
        });
        APP.with(|s| *s.borrow_mut() = Some(a.clone()));
        a.init_controls();
        tray::allow_show_from_other_integrity(hwnd, a.taskbar_created);
        a.hotkey.set(tray::register_hotkey(hwnd));
        a.in_tray.set(tray::add_icon(hwnd, a.hotkey.get()));

        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            scale(1100, dpi),
            scale(680, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );
        a.engine.start();
        if start_hidden && a.in_tray.get() {
            a.schedule_hibernate();
        } else {
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetFocus(edit);
        }
        a.query(true);
        drop(a);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_KEYDOWN && handle_global_key(msg.wParam as u16) {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|s| s.borrow_mut().take());
    }
}

/// Window-independent shortcuts. True if consumed.
unsafe fn handle_global_key(vk: u16) -> bool {
    let Some(a) = app() else { return false };
    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
    match vk {
        VK_F5 => {
            a.engine.rebuild_async();
            true
        }
        VK_F3 => {
            a.focus_edit();
            true
        }
        0x46 /* F */ if ctrl => {
            a.focus_edit();
            true
        }
        VK_ESCAPE if GetFocus() == a.list => {
            a.focus_edit();
            true
        }
        _ => false,
    }
}

unsafe fn build_menu(admin: bool) -> HMENU {
    let bar = CreateMenu();
    let file = CreatePopupMenu();
    AppendMenuW(file, MF_STRING, CMD_REBUILD, w!("&Rebuild index\tF5"));
    AppendMenuW(
        file,
        MF_STRING | if admin { MF_GRAYED } else { 0 },
        CMD_ADMIN,
        w!("Restart as &Administrator"),
    );
    AppendMenuW(file, MF_SEPARATOR, 0, null());
    AppendMenuW(file, MF_STRING, CMD_EXIT, w!("E&xit"));
    AppendMenuW(bar, MF_POPUP, file as usize, w!("&File"));
    let help = CreatePopupMenu();
    AppendMenuW(help, MF_STRING, CMD_SYNTAX, w!("&Search syntax"));
    AppendMenuW(bar, MF_POPUP, help as usize, w!("&Help"));
    bar
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(a) = app() else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_SIZE => {
            if wp == SIZE_MINIMIZED as usize {
                a.schedule_hibernate();
            } else {
                let shown = wp == SIZE_RESTORED as usize || wp == SIZE_MAXIMIZED as usize;
                if shown && IsWindowVisible(hwnd) != 0 {
                    a.engine.wake();
                }
                a.layout();
            }
            0
        }
        WM_HOTKEY if wp == tray::HOTKEY_ID as usize => {
            a.toggle();
            0
        }
        tray::WM_APP_SHOW => {
            a.show();
            0
        }
        tray::WM_APP_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => a.show(),
                WM_RBUTTONUP | WM_CONTEXTMENU => match tray::context_menu(hwnd) {
                    tray::CMD_TRAY_SHOW => a.show(),
                    tray::CMD_TRAY_EXIT => a.exit(),
                    _ => {}
                },
                _ => {}
            }
            0
        }
        WM_TIMER if wp == TIMER_HIBERNATE => {
            a.try_hibernate();
            0
        }
        WM_APP_HIBERNATED => {
            a.hibernating.set(false);
            if wp == 1 {
                a.retry_ms.set(HIBERNATE_RETRY_MS);
                if a.engine.is_hibernated() {
                    // Trim again now the helper thread is gone and the UI is idle.
                    glint::volume::release_memory();
                }
            } else {
                // Busy indexing, or the snapshot could not be written: back off.
                let ms = a.retry_ms.get();
                a.retry_ms.set((ms * 2).min(HIBERNATE_RETRY_MAX_MS));
                SetTimer(hwnd, TIMER_HIBERNATE, ms, None);
            }
            0
        }
        WM_ENDSESSION => {
            if wp != 0 {
                let _ = a.engine.save();
            }
            0
        }
        m if m != 0 && m == a.taskbar_created => {
            a.in_tray.set(tray::add_icon(hwnd, a.hotkey.get()));
            0
        }
        WM_SETFOCUS => {
            SetFocus(a.edit);
            0
        }
        WM_COMMAND => {
            let (id, code) = (wp & 0xFFFF, (wp >> 16) & 0xFFFF);
            if id == ID_EDIT {
                if code == EN_CHANGE as usize {
                    a.query(true);
                }
            } else {
                a.command(id);
            }
            0
        }
        WM_NOTIFY => a.on_notify(lp),
        WM_APP_RESULTS => {
            a.take_results();
            0
        }
        WM_APP_READY => {
            a.query(true);
            a.update_status();
            0
        }
        WM_APP_CHANGED => {
            // Throttle (not debounce): refresh at most every REFRESH_MS during churn.
            if !a.refresh_pending.replace(true) {
                SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
            }
            0
        }
        WM_TIMER if wp == TIMER_REFRESH => {
            KillTimer(hwnd, TIMER_REFRESH);
            a.refresh_pending.set(false);
            a.query(false);
            0
        }
        WM_APP_STATUS => {
            a.update_status();
            0
        }
        WM_DPICHANGED => {
            let r = &*(lp as *const RECT);
            a.dpi.set(((wp >> 16) & 0xFFFF) as u32);
            a.apply_font();
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            0
        }
        WM_CLOSE if a.in_tray.get() && !a.exiting.get() => {
            a.hide();
            0
        }
        WM_CLOSE => {
            // Hide first so exit feels instant while the snapshot is written.
            ShowWindow(hwnd, SW_HIDE);
            if !a.skip_save.get() {
                let _ = a.engine.save();
            }
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            tray::remove_icon(hwnd);
            tray::unregister_hotkey(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn edit_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
    match msg {
        WM_KEYDOWN
            if wp == VK_DOWN as usize || wp == VK_RETURN as usize || wp == VK_NEXT as usize =>
        {
            if let Some(a) = app() {
                a.focus_list();
            }
            return 0;
        }
        WM_KEYDOWN if ctrl && wp == 0x41 => {
            SendMessageW(h, EM_SETSEL, 0, -1);
            return 0;
        }
        // Swallow the beeps for Enter / Ctrl+A and the box glyph for Ctrl+Backspace.
        WM_CHAR if wp == 0x0D || wp == 0x01 || wp == 0x7F => return 0,
        _ => {}
    }
    DefSubclassProc(h, msg, wp, lp)
}

impl App {
    unsafe fn init_controls(&self) {
        SetWindowSubclass(self.edit, Some(edit_proc), 1, 0);
        SendMessageW(
            self.edit,
            EM_SETCUEBANNER,
            1,
            w!("Search  (e.g.  *.pdf   ext:jpg;png   size:>10mb   !temp)") as isize,
        );
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP;
        SendMessageW(
            self.list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            ex as usize,
            ex as isize,
        );
        SetWindowTheme(self.list, w!("Explorer"), null());
        SendMessageW(
            self.list,
            LVM_SETIMAGELIST,
            LVSIL_SMALL as usize,
            self.icons.borrow().image_list(),
        );
        let dpi = self.dpi.get();
        for (i, (title, width, right)) in [
            ("Name", 320, false),
            ("Path", 460, false),
            ("Size", 100, true),
            ("Date Modified", 140, false),
        ]
        .iter()
        .enumerate()
        {
            let mut t = wide(title);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                fmt: if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                cx: scale(*width, dpi),
                pszText: t.as_mut_ptr(),
                ..std::mem::zeroed()
            };
            SendMessageW(
                self.list,
                LVM_INSERTCOLUMNW,
                i,
                &col as *const LVCOLUMNW as isize,
            );
        }
        self.apply_font();
        self.set_sort_arrow();
    }

    unsafe fn apply_font(&self) {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        if SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            (&mut ncm as *mut NONCLIENTMETRICSW).cast(),
            0,
            self.dpi.get(),
        ) == 0
        {
            return;
        }
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        for h in [self.edit, self.list, self.status] {
            SendMessageW(h, WM_SETFONT, font as usize, 1);
        }
        let old = self.font.replace(font);
        if !old.is_null() {
            DeleteObject(old as HGDIOBJ);
        }
        self.layout();
    }

    unsafe fn layout(&self) {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(self.hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);
        let dpi = self.dpi.get();
        SendMessageW(self.status, WM_SIZE, 0, 0);
        let mut sr: RECT = std::mem::zeroed();
        GetWindowRect(self.status, &mut sr);
        let sh = sr.bottom - sr.top;
        let parts = [(w - scale(520, dpi)).max(scale(120, dpi)), -1];
        SendMessageW(self.status, SB_SETPARTS, 2, parts.as_ptr() as isize);
        let pad = scale(6, dpi);
        let eh = scale(26, dpi);
        MoveWindow(self.edit, pad, pad, (w - 2 * pad).max(0), eh, 1);
        let top = eh + 2 * pad;
        MoveWindow(self.list, 0, top, w, (h - top - sh).max(0), 1);
    }

    unsafe fn text(&self) -> String {
        let n = GetWindowTextLengthW(self.edit).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(self.edit, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    }

    unsafe fn query(&self, reset: bool) {
        let (sort, desc) = self.sort.get();
        self.worker.submit(Request {
            text: self.text(),
            sort,
            desc,
            reset,
        });
    }

    unsafe fn set_status_part(&self, part: usize, s: &str) {
        let t = wide(s);
        SendMessageW(self.status, SB_SETTEXTW, part, t.as_ptr() as isize);
    }

    unsafe fn update_status(&self) {
        self.set_status_part(1, &self.engine.status());
    }

    unsafe fn focus_edit(&self) {
        SetFocus(self.edit);
        SendMessageW(self.edit, EM_SETSEL, 0, -1);
    }

    unsafe fn command(&self, id: usize) {
        match id {
            CMD_REBUILD => self.engine.rebuild_async(),
            CMD_ADMIN => self.restart_elevated(),
            CMD_EXIT => self.exit(),
            CMD_SYNTAX => {
                let t = wide(SYNTAX);
                MessageBoxW(
                    self.hwnd,
                    t.as_ptr(),
                    w!("Glint search syntax"),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            CMD_OPEN | CMD_OPEN_FOLDER => {
                for p in self.selected_paths(false).iter().take(16) {
                    shell::open(self.hwnd, p, id == CMD_OPEN_FOLDER);
                }
            }
            CMD_COPY_PATH | CMD_COPY_NAME => {
                let items = self.selected_paths(id == CMD_COPY_NAME);
                if !items.is_empty() {
                    shell::copy_text(self.hwnd, &items.join("\r\n"));
                }
            }
            _ => {}
        }
    }

    unsafe fn restart_elevated(&self) {
        let _ = self.engine.save();
        // The elevated copy must not find us still "running".
        let mut inst = self.instance.borrow_mut();
        if let Some(i) = inst.as_mut() {
            i.release();
        }
        if shell::relaunch_elevated(self.hwnd) {
            self.skip_save.set(true);
            self.exit();
        } else {
            *inst = tray::Instance::acquire();
        }
    }
}

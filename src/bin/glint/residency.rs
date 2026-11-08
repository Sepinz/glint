//! Tray residency: show/hide, and hibernation while out of sight.

use std::sync::Arc;

use glint::search::Hits;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Controls::LVM_SETITEMCOUNT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{App, HIBERNATE_MS, TIMER_HIBERNATE, WM_APP_HIBERNATED};

impl App {
    pub(crate) unsafe fn exit(&self) {
        self.exiting.set(true);
        PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
    }

    pub(crate) unsafe fn show(&self) {
        KillTimer(self.hwnd, TIMER_HIBERNATE);
        self.engine.wake(); // reloads in the background; Ready re-queries
        ShowWindow(
            self.hwnd,
            if IsIconic(self.hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(self.hwnd);
        self.focus_edit();
        if self.epoch.get() == u64::MAX {
            self.query(true);
        }
    }

    pub(crate) unsafe fn hide(&self) {
        ShowWindow(self.hwnd, SW_HIDE);
        self.schedule_hibernate();
    }

    pub(crate) unsafe fn toggle(&self) {
        let up = IsWindowVisible(self.hwnd) != 0 && IsIconic(self.hwnd) == 0;
        if up && GetForegroundWindow() == self.hwnd {
            self.hide();
        } else {
            self.show();
        }
    }

    pub(crate) unsafe fn schedule_hibernate(&self) {
        SetTimer(self.hwnd, TIMER_HIBERNATE, HIBERNATE_MS, None);
    }

    /// Out of sight: drop every result and the index itself.
    pub(crate) unsafe fn try_hibernate(&self) {
        KillTimer(self.hwnd, TIMER_HIBERNATE);
        let out_of_sight = IsWindowVisible(self.hwnd) == 0 || IsIconic(self.hwnd) != 0;
        if !out_of_sight || self.engine.is_hibernated() || self.hibernating.get() {
            return;
        }
        self.worker.forget();
        *self.results.borrow_mut() = Arc::new(Hits::default());
        self.epoch.set(u64::MAX);
        SendMessageW(self.list, LVM_SETITEMCOUNT, 0, 0);
        // Saving can take a while: keep the UI (hotkey, tray) responsive.
        self.hibernating.set(true);
        let (engine, h) = (self.engine.clone(), self.hwnd as isize);
        let spawned = std::thread::Builder::new()
            .name("glint-hibernate".into())
            .spawn(move || {
                let ok = engine.hibernate();
                // SAFETY: posting to a window handle is thread-safe; a stale handle is rejected.
                unsafe { PostMessageW(h as HWND, WM_APP_HIBERNATED, ok as usize, 0) };
            });
        if spawned.is_err() {
            PostMessageW(self.hwnd, WM_APP_HIBERNATED, 0, 0);
        }
    }
}

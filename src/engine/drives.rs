//! Keeping the index in step with the drives that are present: USB sticks
//! and cards come and go, another volume may appear behind a known letter,
//! and volumes without a journal go stale. Each change re-indexes only the
//! drives concerned; the others are kept as they are.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::{build_drive, Engine, Mode};
use crate::index::Index;
use crate::volume::{self, Drive};

/// How often the drive letters are checked (one cheap system call).
const POLL: Duration = Duration::from_secs(2);
/// How often the full drive list (media, serials) is re-read regardless,
/// e.g. for a card swapped in a reader, which keeps its letter.
const RECHECK: Duration = Duration::from_secs(30);

/// What it takes to make the index match the drives that are present.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SyncPlan {
    /// Letters of volumes to remove from the index.
    pub drop: Vec<u8>,
    /// Drives to index (again).
    pub build: Vec<Drive>,
    /// Something is missing or wrong (not just stale): index at full speed.
    pub urgent: bool,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.drop.is_empty() && self.build.is_empty()
    }
}

/// What drive syncing needs to know of an indexed volume (never its tables).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Known {
    pub letter: u8,
    pub live: bool,
    pub serial: u64,
}

/// Compare the indexed volumes with the drives present. `stale`: refresh
/// volumes that are not live (no journal keeps them current), except the
/// `fresh` ones just indexed. When elevated, a fixed NTFS drive that was
/// indexed without its journal (by a folder walk) is re-read from its MFT
/// if `may_go_live` says it has a journal and has not been tried yet.
pub(super) fn plan_sync(
    vols: &[Known],
    drives: &[Drive],
    admin: bool,
    (stale, fresh): (bool, &[u8]),
    may_go_live: impl Fn(u8) -> bool,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    for v in vols {
        if !drives.iter().any(|d| d.letter == v.letter) {
            plan.drop.push(v.letter);
        }
    }
    for d in drives {
        let Some(v) = vols.iter().find(|v| v.letter == d.letter) else {
            plan.build.push(*d);
            plan.urgent = true;
            continue;
        };
        let other = v.serial as u32 != d.serial;
        let can_be_live = !v.live && admin && d.ntfs && !d.removable && may_go_live(d.letter);
        let refresh = stale && !v.live && !fresh.contains(&d.letter);
        if other || can_be_live || refresh {
            plan.drop.push(d.letter);
            plan.build.push(*d);
            plan.urgent |= other || can_be_live;
        }
    }
    plan
}

/// Index `drives` in parallel (one thread each), unmerged and unsorted.
fn build_parts(drives: &[Drive], admin: bool, background: bool) -> Vec<Index> {
    thread::scope(|s| {
        let handles: Vec<_> = drives
            .iter()
            .map(|d| s.spawn(move || build_drive(*d, admin, background)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// Releases `building` however the holder leaves (a panic included), so
/// the engine never stays "busy" for good.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Engine {
    fn tried_live(&self, letter: u8) -> bool {
        let m = self.tried_live.lock().unwrap_or_else(|e| e.into_inner());
        *m & 1 << (letter - b'A') != 0
    }

    fn mark_tried_live(&self, letter: u8) {
        let mut m = self.tried_live.lock().unwrap_or_else(|e| e.into_inner());
        *m |= 1 << (letter - b'A');
    }

    /// Bring the index in line with `drives` (see [`plan_sync`]). The caller
    /// holds `building`. Returns the letters indexed (possibly none: only
    /// drops), or `None` when nothing changed.
    pub(super) fn sync_volumes(
        &self,
        drives: &[Drive],
        stale: bool,
        fresh: &[u8],
        mode: Mode,
    ) -> Option<Vec<u8>> {
        let vols: Vec<Known> = self
            .read()
            .volumes
            .iter()
            .map(|v| Known {
                letter: v.letter,
                live: v.live,
                serial: v.serial,
            })
            .collect();
        let plan = plan_sync(&vols, drives, self.admin, (stale, fresh), |l| {
            !self.tried_live(l) && volume::journal_active(l)
        });
        if plan.is_empty() {
            return None;
        }
        if self.admin {
            for d in plan.build.iter().filter(|d| d.ntfs && !d.removable) {
                self.mark_tried_live(d.letter);
            }
        }
        let t = Instant::now();
        if !plan.build.is_empty() {
            let letters: String = plan
                .build
                .iter()
                .map(|d| format!("{}: ", d.letter as char))
                .collect();
            self.set_status(format!("Indexing {letters}…"));
        }
        let background = mode == Mode::Interactive && !plan.urgent;
        let mut parts = build_parts(&plan.build, self.admin, background);
        // A drive pulled (or swapped) during its scan left a partial index:
        // discard it; the watcher sees the change and syncs again.
        let now = volume::drives();
        parts.retain(|p| {
            p.volumes.first().is_some_and(|v| {
                now.iter()
                    .any(|d| d.letter == v.letter && d.serial as u64 == v.serial)
            })
        });
        let built: Vec<u8> = parts
            .iter()
            .filter_map(|p| p.volumes.first().map(|v| v.letter))
            .collect();
        {
            let mut g = self.write();
            let mut old = std::mem::take(&mut *g);
            old.drop_volumes(|v| plan.drop.contains(&v.letter));
            let (epoch, version) = (old.epoch, old.version);
            // With every volume dropped nothing of the old index survives:
            // free it now instead of carrying it through the merge.
            let old = (!old.volumes.is_empty()).then_some(old);
            let mut ix = Index::merge(old.into_iter().chain(parts).collect());
            ix.finalize();
            ix.epoch = epoch.wrapping_add(1);
            ix.version = version.wrapping_add(1);
            ix.mark_dirty();
            *g = ix;
        }
        (self.notify)(super::Event::Ready);
        self.set_status(self.summary(Some(t.elapsed())));
        Some(built)
    }

    /// Interactive: follow drives being plugged in and out (one thread).
    pub(super) fn watch_drives(self: &Arc<Self>) {
        if self.watching.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let spawned = thread::Builder::new()
            .name("glint-drives".into())
            .stack_size(256 * 1024)
            .spawn(move || me.drive_loop());
        if spawned.is_err() {
            self.watching.store(false, Ordering::Release);
        }
    }

    fn drive_loop(self: Arc<Self>) {
        let mut mask = volume::drive_mask();
        let mut checked = Instant::now();
        loop {
            thread::sleep(POLL);
            let now = volume::drive_mask();
            if now == mask && checked.elapsed() < RECHECK {
                continue;
            }
            // Hibernated: waking runs startup, which syncs the drives itself.
            if self.is_hibernated() || self.building.swap(true, Ordering::AcqRel) {
                continue; // busy: try again on the next poll
            }
            let busy = Busy(&self.building);
            if self.is_hibernated() {
                continue; // hibernated just before we took `building`
            }
            mask = now;
            checked = Instant::now();
            let drives = volume::drives();
            if self
                .sync_volumes(&drives, false, &[], Mode::Interactive)
                .is_some()
            {
                self.save_logged_quick();
            }
            if self.admin {
                // Under `building`, like startup: new drives, failed monitors.
                self.spawn_monitors(&drives);
            }
            drop(busy);
        }
    }
}

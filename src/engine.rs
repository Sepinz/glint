//! Orchestration: load snapshot -> journal catch-up -> (re)build if needed ->
//! live monitoring. Shared by the GUI (asynchronously) and the CLI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::fmt::group;
use crate::index::Index;
use crate::volume::{self, Drive};
use crate::{mft, ntfs, store, usn, walk};

mod drives;

/// A file's hard links: (parent folder reference number, name).
type Links = Vec<(u64, String)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A new index was published (entry ids changed).
    Ready,
    /// Content changed (live update or compaction).
    Changed,
    /// `Engine::status` changed.
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Long-running UI: rescan unprivileged indexes in the background, monitor journals.
    Interactive,
    /// One-shot CLI: use the snapshot as-is when it cannot be caught up cheaply.
    OneShot,
}

type Notify = Box<dyn Fn(Event) + Send + Sync>;

pub struct Engine {
    index: RwLock<Index>,
    status: Mutex<String>,
    notify: Notify,
    db_path: PathBuf,
    admin: bool,
    building: AtomicBool,
    monitoring: AtomicBool,
    /// Monitors exit when this moves past the value they were started with.
    monitor_gen: AtomicU64,
    /// (generation, drive letters bit mask) of the running monitors.
    monitors: Mutex<(u64, u32)>,
    /// The drive watcher thread is running.
    watching: AtomicBool,
    /// Letters already re-read from the MFT this session to make them live:
    /// one attempt each, so a drive that cannot go live is not re-indexed
    /// over and over.
    tried_live: Mutex<u32>,
    hibernated: AtomicBool,
    /// Set by `wake`; a hibernation finishing afterwards wakes right back up.
    want_awake: AtomicBool,
}

const USN_BUF: usize = 1 << 16;
/// A file's alternate data streams as (name, size).
type Listed = Vec<(String, u64)>;

/// Volumes without a journal (and every volume, unelevated) older than this
/// are refreshed by a background rescan.
const STALE_AFTER: Duration = Duration::from_secs(30 * 60);
/// A one-shot run saves what it caught up only past this much journal: less
/// is cheaper to replay next time than to write a snapshot now.
const REPLAY_SAVE_BYTES: u64 = 4 << 20;

/// Whether startup should save after catching up `replayed` journal bytes.
/// The interactive app saves anyway, but only once it is live and serving.
fn startup_save_due(mode: Mode, replayed: u64) -> bool {
    mode == Mode::Interactive || replayed >= REPLAY_SAVE_BYTES
}

impl Engine {
    pub fn new(db_path: PathBuf, notify: impl Fn(Event) + Send + Sync + 'static) -> Arc<Engine> {
        Arc::new(Engine {
            index: RwLock::new(Index::default()),
            status: Mutex::new(String::new()),
            notify: Box::new(notify),
            db_path,
            admin: volume::is_admin(),
            building: AtomicBool::new(false),
            monitoring: AtomicBool::new(false),
            monitor_gen: AtomicU64::new(0),
            monitors: Mutex::new((0, 0)),
            watching: AtomicBool::new(false),
            tried_live: Mutex::new(0),
            hibernated: AtomicBool::new(false),
            want_awake: AtomicBool::new(true),
        })
    }

    pub fn read(&self) -> RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> RwLockWriteGuard<'_, Index> {
        self.index.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_admin(&self) -> bool {
        self.admin
    }

    pub fn is_hibernated(&self) -> bool {
        self.hibernated.load(Ordering::Acquire)
    }

    /// Idle: persist the index, free it and give the memory back to the OS.
    /// Journals are not read while hibernated; `wake` replays them from the
    /// saved positions, so nothing is lost. False if busy (indexing) or the
    /// snapshot could not be written; the caller may retry later.
    pub fn hibernate(self: &Arc<Self>) -> bool {
        if self.is_hibernated() {
            return true;
        }
        if self.building.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.want_awake.store(false, Ordering::Release);
        let was_monitoring = self.monitoring.swap(false, Ordering::AcqRel);
        self.monitor_gen.fetch_add(1, Ordering::AcqRel);
        let saved = self.save().is_ok();
        if saved {
            let old = {
                let mut g = self.write();
                let empty = Index {
                    epoch: g.epoch.wrapping_add(1),
                    version: g.version.wrapping_add(1),
                    ..Index::default()
                };
                std::mem::replace(&mut *g, empty)
            };
            drop(old); // outside the lock
            self.hibernated.store(true, Ordering::Release);
        }
        self.building.store(false, Ordering::Release);
        if saved {
            volume::set_memory_priority(true);
            volume::release_memory();
            if self.want_awake.load(Ordering::Acquire) {
                self.wake(); // shown again while we were saving
            }
        } else if was_monitoring {
            self.spawn_monitors(&volume::drives());
        }
        saved
    }

    /// Leave hibernation: reload the snapshot and catch up (asynchronously,
    /// `Event::Ready` follows). No-op when awake.
    pub fn wake(self: &Arc<Self>) {
        self.want_awake.store(true, Ordering::Release);
        if !self.is_hibernated() {
            return; // awake, or a hibernation in progress that will see want_awake
        }
        // Own `building` *before* leaving hibernation so no rebuild or second
        // hibernation can slip in ahead of the reload.
        if self.building.swap(true, Ordering::AcqRel) {
            return;
        }
        self.hibernated.store(false, Ordering::Release);
        volume::set_memory_priority(false);
        self.start(); // startup() keeps `building` and releases it when done
    }

    pub fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
        (self.notify)(Event::Status);
    }

    /// Start up on a background thread (GUI).
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        let _ = thread::Builder::new()
            .name("glint-startup".into())
            .spawn(move || me.startup(Mode::Interactive, false));
    }

    /// Load or build the index. Returns once the index is usable.
    pub fn startup(self: &Arc<Self>, mode: Mode, force_rebuild: bool) {
        // Excludes `rebuild_async` while the snapshot is loaded and caught up.
        self.building.store(true, Ordering::Release);
        self.set_status("Loading index…");
        let t = Instant::now();
        let drives = volume::drives();
        let loaded = if force_rebuild {
            None
        } else {
            store::load(&self.db_path).ok()
        };
        match loaded {
            Some(ix) => self.resume(ix, t, &drives, mode),
            None => self.rebuild(false),
        }
        // Still holding `building`, so no hibernation can slip in between
        // and leave monitors running on an emptied index.
        if mode == Mode::Interactive && self.admin {
            self.spawn_monitors(&volume::drives()); // drives may have changed meanwhile
        }
        self.building.store(false, Ordering::Release);
        if mode == Mode::Interactive {
            self.watch_drives();
        }
    }

    /// Serve a loaded snapshot: fit it to the drives present, replay the
    /// journals, refresh what went stale, and save when worthwhile.
    fn resume(&self, ix: Index, t: Instant, drives: &[Drive], mode: Mode) {
        let stale = mode == Mode::Interactive && self.snapshot_age() > STALE_AFTER;
        let n = ix.count();
        self.publish(ix);
        self.set_status(format!(
            "Loaded {} objects in {:.0} ms",
            group(n as u64),
            t.elapsed().as_secs_f64() * 1e3
        ));
        // First, so the catch-up finds every live volume's drive.
        let built = self.sync_volumes(drives, false, &[], mode);
        let synced = built.is_some();
        let replayed = if self.admin {
            match self.catch_up() {
                Some(bytes) => bytes,
                None => return self.rebuild(false),
            }
        } else {
            0
        };
        if !synced {
            self.set_status(self.summary(None));
        }
        if synced || startup_save_due(mode, replayed) {
            // Before the monitors: a journal update waiting on the write lock
            // behind this save would stall searches.
            self.save_logged_quick();
        }
        // Volumes no journal keeps current: rescan those only, in the
        // background, while the loaded index serves searches.
        let fresh = built.unwrap_or_default();
        if stale && self.sync_volumes(drives, true, &fresh, mode).is_some() {
            self.save_logged_quick();
        }
    }

    fn snapshot_age(&self) -> Duration {
        std::fs::metadata(&self.db_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::MAX)
    }

    pub fn rebuild_async(self: &Arc<Self>) {
        if self.is_hibernated() || self.building.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.is_hibernated() {
            // Hibernated between the two checks: waking rebuilds what it must.
            self.building.store(false, Ordering::Release);
            return;
        }
        let me = self.clone();
        let spawned = thread::Builder::new()
            .name("glint-rebuild".into())
            .spawn(move || {
                me.rebuild(false);
                me.building.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.building.store(false, Ordering::Release);
        }
    }

    fn rebuild(&self, background: bool) {
        if background {
            volume::set_background_priority();
        }
        let t = Instant::now();
        let drives = volume::drives();
        let letters: String = drives
            .iter()
            .map(|d| format!("{}: ", d.letter as char))
            .collect();
        let how = if self.admin {
            "reading MFT"
        } else {
            "scanning folders"
        };
        self.set_status(format!("Indexing {}({how})…", letters));
        let ix = build_all(&drives, self.admin, background);
        self.publish(ix);
        self.set_status(self.summary(Some(t.elapsed())));
        self.save_logged();
    }

    fn summary(&self, took: Option<Duration>) -> String {
        let ix = self.read();
        let live = ix.volumes.iter().filter(|v| v.live).count();
        let took = took
            .map(|d| format!(" in {:.2} s", d.as_secs_f64()))
            .unwrap_or_default();
        let mode = if live == ix.volumes.len() && live > 0 {
            "live".to_string()
        } else if live > 0 {
            format!("{live}/{} volumes live", ix.volumes.len())
        } else {
            "static: run as administrator for instant NTFS indexing + live updates".to_string()
        };
        format!(
            "{} objects indexed{took} · {mode}",
            group(ix.count() as u64)
        )
    }

    fn publish(&self, mut ix: Index) {
        {
            let mut g = self.write();
            ix.epoch = g.epoch.wrapping_add(1);
            ix.version = g.version.wrapping_add(1);
            *g = ix;
        }
        (self.notify)(Event::Ready);
    }

    /// Replay journals from the snapshot's positions. Returns the journal
    /// bytes replayed, or `None` when a rebuild is needed instead.
    fn catch_up(&self) -> Option<u64> {
        let vols: Vec<(usize, u8, u64, i64)> = self
            .read()
            .volumes
            .iter()
            .enumerate()
            .filter(|(_, v)| v.live)
            .map(|(i, v)| (i, v.letter, v.journal_id, v.next_usn))
            .collect();
        let mut buf = vec![0u8; USN_BUF];
        let mut replayed = 0;
        for (vi, letter, jid, start) in vols {
            let f = volume::open_volume(letter).ok()?;
            let j = usn::query(&f).ok()?;
            if j.id != jid || start < j.first || start > j.next {
                return None;
            }
            let mut next = start;
            loop {
                let (n, changes) = usn::read(&f, jid, next, false, &mut buf).ok()?;
                let (applied, epoch) = {
                    let mut g = self.write();
                    let same = g.volumes.get(vi).is_some_and(|v| {
                        v.letter == letter && v.journal_id == jid && v.next_usn == next
                    });
                    if !same {
                        return None;
                    }
                    let applied = usn::apply(&mut g, vi, &changes);
                    g.volumes[vi].next_usn = n;
                    if n != next {
                        g.mark_dirty();
                    }
                    (applied, g.epoch)
                };
                self.refresh_stats(epoch, &applied.stat);
                self.relink(letter, &applied.relink);
                self.restream(letter, &applied.restream);
                // Keep reading while the position advances (a page may hold
                // only records we skip) until we reach the journal head.
                replayed += n.abs_diff(next);
                if n == next || n >= j.next {
                    break;
                }
                next = n;
            }
        }
        Some(replayed)
    }

    /// Re-read size/mtime for changed files (outside the write lock).
    fn refresh_stats(&self, epoch: u64, list: &[u32]) {
        if list.is_empty() {
            return;
        }
        let paths: Vec<(u32, String)> = {
            let g = self.read();
            if g.epoch != epoch {
                return;
            }
            list.iter()
                .filter(|&&i| (i as usize) < g.len())
                .map(|&i| (i, g.full_path(i)))
                .collect()
        };
        let stats: Vec<(u32, u64, u32)> = paths
            .iter()
            .filter_map(|(i, p)| {
                volume::stat(p).map(|(s, t, dir)| (*i, if dir { 0 } else { s }, t))
            })
            .collect();
        let mut g = self.write();
        if g.epoch == epoch {
            for (i, s, t) in stats {
                g.set_stat(i, s, t);
            }
            g.version = g.version.wrapping_add(1);
            g.mark_dirty();
        }
    }

    /// Reconcile files whose hard links changed with the links Windows lists
    /// for them (looked up outside the write lock), then refresh their stats.
    /// A file whose links cannot be read is left as it was. Entries are
    /// resolved by record under the lock, so a compaction in between (new
    /// entry ids) is harmless and the change is not dropped.
    fn relink(&self, letter: u8, files: &[(u64, u32)]) {
        if files.is_empty() {
            return;
        }
        let found: Vec<(u64, u32, Links)> = files
            .iter()
            .filter_map(|&(frn, attrs)| {
                volume::hard_links(letter, frn)
                    .ok()
                    .map(|l| (frn, attrs, l))
            })
            .collect();
        let (stat, epoch): (Vec<u32>, u64) = {
            let mut g = self.write();
            // By letter: a drive sync may have moved the volume meanwhile.
            let Some(vi) = g.volumes.iter().position(|v| v.letter == letter && v.live) else {
                return; // a rebuild replaced this volume: it is current anyway
            };
            let stat = found
                .iter()
                .flat_map(|(frn, attrs, links)| usn::relink(&mut g, vi, *frn, *attrs, links))
                .collect();
            g.mark_dirty();
            (stat, g.epoch)
        };
        self.refresh_stats(epoch, &stat);
    }

    /// Reconcile files whose alternate data streams may have changed with the
    /// streams Windows lists for them (looked up outside the write lock).
    fn restream(&self, letter: u8, files: &[u64]) {
        if files.is_empty() {
            return;
        }
        // `None`: Windows could not list them (access denied, ...).
        let found: Vec<(u64, Option<Listed>)> = files
            .iter()
            .map(|&frn| (frn, volume::streams(letter, frn).ok()))
            .collect();
        let mut g = self.write();
        // By letter: a drive sync may have moved the volume meanwhile.
        let Some(vi) = g.volumes.iter().position(|v| v.letter == letter && v.live) else {
            return;
        };
        let mut changed = false;
        for (frn, s) in found {
            // Unlisted: keep the streams known, under the file's current name.
            let s = s.unwrap_or_else(|| usn::cached_streams(&g, vi, frn));
            changed |= usn::restream(&mut g, vi, frn, &s);
        }
        if changed {
            g.mark_dirty();
        }
    }

    /// Start a journal monitor for each fixed NTFS drive that has none
    /// (again, if one stopped). Removable drives are never monitored.
    fn spawn_monitors(self: &Arc<Self>, drives: &[Drive]) {
        self.monitoring.store(true, Ordering::Release);
        let mut m = self.monitors.lock().unwrap_or_else(|e| e.into_inner());
        let gen = self.monitor_gen.load(Ordering::Acquire);
        if m.0 != gen {
            *m = (gen, 0); // the previous generation's monitors are exiting
        }
        for d in drives.iter().filter(|d| d.ntfs && !d.removable) {
            let bit = 1u32 << (d.letter - b'A');
            if m.1 & bit != 0 {
                continue;
            }
            let (me, letter) = (self.clone(), d.letter);
            let spawned = thread::Builder::new()
                .name(format!("glint-usn-{}", letter as char))
                .stack_size(256 * 1024)
                .spawn(move || {
                    me.clone().monitor(letter, gen);
                    let mut m = me.monitors.lock().unwrap_or_else(|e| e.into_inner());
                    if m.0 == gen {
                        m.1 &= !bit;
                    }
                });
            if spawned.is_ok() {
                m.1 |= bit;
            }
        }
    }

    /// Follow one drive's journal until retired, the drive goes away, or the
    /// volume handle fails (the drive watcher starts a new monitor then).
    fn monitor(self: Arc<Self>, letter: u8, gen: u64) {
        let Ok(f) = volume::open_volume(letter) else {
            return;
        };
        let mut buf = vec![0u8; USN_BUF];
        let retired = || self.monitor_gen.load(Ordering::Acquire) != gen;
        let present = || volume::drive_mask() & 1 << (letter - b'A') != 0;
        loop {
            if retired() {
                return; // hibernated: wake() replays the journal from the snapshot
            }
            if !present() {
                return; // unplugged: let go of the volume handle
            }
            let cur = {
                let g = self.read();
                g.volumes
                    .iter()
                    .position(|v| v.letter == letter && v.live)
                    .map(|vi| (vi, g.volumes[vi].journal_id, g.volumes[vi].next_usn))
            };
            let Some((vi, jid, start)) = cur else {
                thread::sleep(Duration::from_secs(2));
                continue;
            };
            match usn::read(&f, jid, start, true, &mut buf) {
                Ok((next, changes)) => {
                    let applied = {
                        let mut g = self.write();
                        // A rebuild may have swapped the index while we were blocked.
                        let same = !retired()
                            && g.volumes.get(vi).is_some_and(|v| {
                                v.letter == letter && v.journal_id == jid && v.next_usn == start
                            });
                        same.then(|| {
                            let applied = usn::apply(&mut g, vi, &changes);
                            g.volumes[vi].next_usn = next;
                            g.mark_dirty();
                            (applied, g.epoch)
                        })
                    };
                    if let Some((applied, epoch)) = applied {
                        self.refresh_stats(epoch, &applied.stat);
                        self.relink(letter, &applied.relink);
                        self.restream(letter, &applied.restream);
                        if !changes.is_empty() {
                            (self.notify)(Event::Changed);
                        }
                    }
                    // Coalesce bursts (unzip, builds) into fewer, larger batches.
                    thread::sleep(Duration::from_millis(250));
                }
                Err(_) if retired() => return,
                Err(e) if usn::is_journal_reset(&e) => {
                    self.set_status(format!(
                        "{}: change journal reset, re-indexing…",
                        letter as char
                    ));
                    self.rebuild_async();
                    thread::sleep(Duration::from_secs(30));
                }
                Err(_) => return,
            }
        }
    }

    /// Compact (if needed) and write the snapshot when anything changed.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_with(true)
    }

    /// Like [`Engine::save`], but without `compact` the full re-sort runs only
    /// when deleted entries waste real memory (a snapshot need not be sorted).
    fn save_with(&self, compact: bool) -> std::io::Result<()> {
        // Compact under the write lock (in-memory) ...
        let (compacted, edits) = {
            let mut g = self.write();
            if !g.dirty {
                return Ok(());
            }
            let compacted = g.needs_compact() && (compact || g.worth_compacting());
            if compacted {
                g.compact();
            }
            (compacted, g.edits)
        };
        if compacted {
            (self.notify)(Event::Changed);
        }
        // ... but write the file under a read lock so the UI and searches
        // keep running while the disk is busy.
        store::save(&self.read(), &self.db_path)?;
        let mut g = self.write();
        if g.edits == edits {
            g.dirty = false; // otherwise changes arrived meanwhile: save again later
        }
        Ok(())
    }

    fn save_logged(&self) {
        if let Err(e) = self.save() {
            self.set_status(format!("Could not save index: {e}"));
        }
    }

    /// Save without the full re-sort, which would hold the write lock (and
    /// renumber results) while searches run; idle saves compact.
    fn save_logged_quick(&self) {
        if let Err(e) = self.save_with(false) {
            self.set_status(format!("Could not save index: {e}"));
        }
    }
}

/// Index all drives in parallel (one thread per volume) and merge.
pub fn build_all(drives: &[Drive], admin: bool, background: bool) -> Index {
    let parts: Vec<Index> = thread::scope(|s| {
        let handles: Vec<_> = drives
            .iter()
            .map(|d| s.spawn(move || build_drive(*d, admin, background)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut ix = Index::merge(parts);
    ix.finalize();
    ix.mark_dirty();
    ix
}

fn build_drive(d: Drive, admin: bool, background: bool) -> Index {
    let scanned = (admin && d.ntfs).then(|| scan_ntfs(d.letter).ok());
    let mut ix = scanned
        .flatten()
        .unwrap_or_else(|| walk::scan(d.letter, background));
    if let Some(v) = ix.volumes.first_mut() {
        v.serial = d.serial as u64;
        if d.removable {
            v.live = false; // never monitored (see `Drive::removable`)
            v.slot = Vec::new();
            v.links = Vec::new();
            v.streams = Vec::new();
        }
    }
    ix
}

fn scan_ntfs(letter: u8) -> std::io::Result<Index> {
    let f = volume::open_volume(letter)?;
    let info = ntfs::volume_info(&f)?;
    // Query the journal *before* scanning so changes made during the scan replay.
    let journal = usn::query(&f).ok();
    // Fall back to the plain handle if the device rejects unbuffered reads
    // (alignment on unusual geometry).
    let fast = volume::open_volume_unbuffered(letter)
        .and_then(|v| mft::scan(letter, std::slice::from_ref(&v), &info));
    let mut ix = match fast {
        Ok(ix) => ix,
        Err(_) => mft::scan(letter, std::slice::from_ref(&f), &info)?,
    };
    let v = &mut ix.volumes[0];
    v.live = journal.is_some();
    if let Some(j) = journal {
        v.journal_id = j.id;
        v.next_usn = j.next;
    } else {
        v.slot = Vec::new(); // no journal: nothing will ever look records up
        v.links = Vec::new();
        v.streams = Vec::new();
    }
    Ok(ix)
}

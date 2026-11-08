//! NTFS USN change journal: read records and apply them to the index.
//!
//! The journal position is persisted with the snapshot, so a restart only
//! replays what changed while the app was closed instead of rescanning.

use std::fs::File;
use std::io;

use crate::index::{Batch, Index, ATTR_MASK, F_DIR, F_ROOT, NONE};
use crate::ntfs::attr_flags;
use crate::volume::ioctl;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;

const R_DATA: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004; // overwrite/extend/truncate
/// Overwrite/extend/truncate of an alternate data stream.
const R_NAMED_DATA: u32 = 0x0000_0010 | 0x0000_0020 | 0x0000_0040;
const R_CREATE: u32 = 0x0000_0100;
const R_DELETE: u32 = 0x0000_0200;
const R_RENAME_OLD: u32 = 0x0000_1000;
const R_RENAME_NEW: u32 = 0x0000_2000;
const R_BASIC_INFO: u32 = 0x0000_8000;
/// A hard link was added or removed (which one, the record cannot say).
const R_HARD_LINK: u32 = 0x0001_0000;
/// An alternate data stream was added or removed.
const R_STREAM_CHANGE: u32 = 0x0020_0000;
const R_CLOSE: u32 = 0x8000_0000;

const ATTR_DIRECTORY: u32 = 0x10;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Refuse to grow a slot table beyond this many records (corruption guard).
const MAX_RECORDS: usize = 1 << 31;

const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Journal {
    pub id: u64,
    pub first: i64,
    pub next: i64,
}

pub fn query(vol: &File) -> io::Result<Journal> {
    let mut out = [0u8; 80];
    let n = ioctl(vol, FSCTL_QUERY_USN_JOURNAL, &[], &mut out)?;
    if n < 24 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short USN journal data",
        ));
    }
    let q = |o: usize| u64::from_le_bytes(out[o..o + 8].try_into().unwrap_or([0; 8]));
    Ok(Journal {
        id: q(0),
        first: q(8) as i64,
        next: q(16) as i64,
    })
}

/// The journal can no longer be read from our saved position: rescan.
pub fn is_journal_reset(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            ERROR_JOURNAL_DELETE_IN_PROGRESS
                | ERROR_JOURNAL_NOT_ACTIVE
                | ERROR_JOURNAL_ENTRY_DELETED
        )
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub frn: u64,
    pub parent: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Read journal records from `start`. With `wait`, blocks until at least one
/// record exists. Returns the position to continue from.
const WAIT_SECS: u64 = 10;

pub fn read(
    vol: &File,
    journal: u64,
    start: i64,
    wait: bool,
    buf: &mut [u8],
) -> io::Result<(i64, Vec<Change>)> {
    // READ_USN_JOURNAL_DATA_V0 { StartUsn, ReasonMask, ReturnOnlyOnClose,
    //                            Timeout, BytesToWaitFor, UsnJournalID }
    let mut req = [0u8; 40];
    req[0..8].copy_from_slice(&start.to_le_bytes());
    req[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    // Blocking reads give up after WAIT_SECS so the caller can re-check
    // whether it should still be running (hibernation retires monitors).
    req[16..24].copy_from_slice(&(if wait { WAIT_SECS } else { 0 }).to_le_bytes());
    req[24..32].copy_from_slice(&(wait as u64).to_le_bytes());
    req[32..40].copy_from_slice(&journal.to_le_bytes());
    let n = ioctl(vol, FSCTL_READ_USN_JOURNAL, &req, buf)?;
    parse_records(&buf[..n])
}

/// Parse a FSCTL_READ_USN_JOURNAL output buffer (USN_RECORD_V2 records).
pub fn parse_records(b: &[u8]) -> io::Result<(i64, Vec<Change>)> {
    let next = b
        .get(..8)
        .map(|s| i64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short USN buffer"))?;
    let u16_at = |r: &[u8], o: usize| u16::from_le_bytes([r[o], r[o + 1]]);
    let u32_at = |r: &[u8], o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap_or([0; 4]));
    let u64_at = |r: &[u8], o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap_or([0; 8]));
    let mut out = Vec::new();
    let mut off = 8;
    while off + 0x3C <= b.len() {
        let len = u32_at(b, off) as usize;
        if len < 0x3C || off + len > b.len() {
            // Never advance past records we could not read.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed USN record",
            ));
        }
        let r = &b[off..off + len];
        off += len;
        if u16_at(r, 4) != 2 {
            continue;
        }
        let (nlen, noff) = (u16_at(r, 0x38) as usize, u16_at(r, 0x3A) as usize);
        let Some(raw) = r.get(noff..noff + nlen) else {
            continue;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        out.push(Change {
            frn: u64_at(r, 8),
            parent: u64_at(r, 0x10),
            reason: u32_at(r, 0x28),
            attrs: u32_at(r, 0x34),
            name: String::from_utf16_lossy(&units),
        });
    }
    Ok((next, out))
}

fn slot_get(ix: &Index, v: usize, rec: usize) -> u32 {
    ix.volumes[v].slot.get(rec).copied().unwrap_or(NONE)
}

fn slot_set(ix: &mut Index, v: usize, rec: usize, val: u32) {
    let slot = &mut ix.volumes[v].slot;
    if rec >= slot.len() {
        if rec >= MAX_RECORDS {
            return;
        }
        slot.resize(rec + 1 + rec / 64, NONE);
    }
    slot[rec] = val;
}

fn delete(ix: &mut Index, v: usize, rec: usize, b: &mut Batch) {
    let cur = slot_get(ix, v, rec);
    if cur != NONE {
        ix.delete(cur, b);
        slot_set(ix, v, rec, NONE);
    }
    drop_streams(ix, v, rec as u32, b);
}

/// Delete record `rec`'s stream entries and their rows.
fn drop_streams(ix: &mut Index, v: usize, rec: u32, b: &mut Batch) {
    let gone: Vec<u32> = ix.volumes[v].streams_of(rec).iter().map(|s| s[1]).collect();
    if gone.is_empty() {
        return;
    }
    for e in gone {
        ix.delete(e, b);
    }
    ix.volumes[v].streams.retain(|s| s[0] != rec);
}

fn upsert(ix: &mut Index, v: usize, c: &Change, b: &mut Batch) -> Option<u32> {
    let rec = (c.frn & FRN_MASK) as usize;
    let p = slot_get(ix, v, (c.parent & FRN_MASK) as usize);
    if p == NONE {
        delete(ix, v, rec, b); // moved somewhere we don't index
        return None;
    }
    let dir = if c.attrs & ATTR_DIRECTORY != 0 {
        F_DIR
    } else {
        0
    };
    let cur = slot_get(ix, v, rec);
    if cur == NONE {
        let i = ix.push_batched(&c.name, p, dir | attr_flags(c.attrs), b);
        slot_set(ix, v, rec, i);
        return Some(i);
    }
    let ci = cur as usize;
    if ix.parent[ci] != p || ix.name(cur) != c.name {
        ix.mark(cur, b); // must precede the rename (old sort key)
        ix.set_name(cur, &c.name);
        if ix.parent[ci] != p {
            ix.detach_size(cur);
            ix.parent[ci] = p;
            ix.attach_size(cur);
        }
    }
    set_attrs(ix, cur, dir | attr_flags(c.attrs));
    Some(cur)
}

/// Directory bit and attributes are not sort keys: update in place.
fn set_attrs(ix: &mut Index, i: u32, bits: u8) {
    let f = &mut ix.flags[i as usize];
    *f = (*f & !(F_DIR | ATTR_MASK)) | bits;
}

/// What [`apply`] leaves for the caller to do outside the index lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Entries whose size/mtime should be re-read from the file system.
    pub stat: Vec<u32>,
    /// Files whose hard links changed, with their latest attributes: a journal
    /// record names one link but not whether it was added or removed, so these
    /// are reconciled with the file system's own list (see [`relink`]).
    pub relink: Vec<(u64, u32)>,
    /// Files whose alternate data streams may have changed (or whose name
    /// they carry did): reconciled with the streams Windows lists for them
    /// (see [`restream`]), after `relink`.
    pub restream: Vec<u64>,
}

/// Apply journal changes for volume `v`.
pub fn apply(ix: &mut Index, v: usize, changes: &[Change]) -> Applied {
    let mut b = Batch::default();
    let mut out = Applied::default();
    for c in changes {
        let rec = (c.frn & FRN_MASK) as usize;
        // The record now belongs to a newer file (other sequence number):
        // looking up the old one would find nothing and wipe the new one.
        out.relink
            .retain(|r| (r.0 & FRN_MASK) as usize != rec || r.0 == c.frn);
        out.restream
            .retain(|&f| (f & FRN_MASK) as usize != rec || f == c.frn);
        let multi = !ix.volumes[v].links_of(rec as u32).is_empty();
        let streams_may_change = if multi || c.reason & R_HARD_LINK != 0 {
            match out.relink.iter_mut().find(|r| r.0 == c.frn) {
                Some(r) => r.1 = c.attrs,
                None => out.relink.push((c.frn, c.attrs)),
            }
            true // the primary name the streams carry may change
        } else {
            apply_one(ix, v, c, &mut b, &mut out.stat)
        };
        let has_streams = !ix.volumes[v].streams_of(rec as u32).is_empty();
        let queue = c.reason & (R_NAMED_DATA | R_STREAM_CHANGE) != 0 || has_streams;
        if streams_may_change && queue && !out.restream.contains(&c.frn) {
            out.restream.push(c.frn);
        }
    }
    ix.commit(b);
    out.stat.sort_unstable();
    out.stat.dedup();
    out
}

/// Apply one change to a file with a single name. Returns whether its
/// streams may need reconciling (false once it is deleted, or for the old
/// name of a rename).
fn apply_one(ix: &mut Index, v: usize, c: &Change, b: &mut Batch, stat: &mut Vec<u32>) -> bool {
    let rec = (c.frn & FRN_MASK) as usize;
    if c.reason & R_DELETE != 0 {
        delete(ix, v, rec, b);
        return false;
    }
    if c.reason & !(R_RENAME_OLD | R_CLOSE) == 0 {
        return false; // old name of a rename: the new-name record follows
    }
    let known = slot_get(ix, v, rec) != NONE;
    if c.reason & (R_CREATE | R_RENAME_NEW) != 0 || !known {
        if let Some(i) = upsert(ix, v, c, b) {
            if c.attrs & ATTR_DIRECTORY == 0 {
                stat.push(i);
            }
        }
    } else if c.reason & (R_DATA | R_BASIC_INFO | R_NAMED_DATA | R_STREAM_CHANGE) != 0 {
        // Writing a stream changes its file's time too.
        let cur = slot_get(ix, v, rec);
        let dir = ix.flags(cur) & F_DIR;
        set_attrs(ix, cur, dir | attr_flags(c.attrs));
        stat.push(cur);
    }
    true
}

/// The `(name, size)` of file `frn`'s streams as indexed: what [`restream`]
/// falls back on when Windows cannot list them (renames still apply).
pub fn cached_streams(ix: &Index, v: usize, frn: u64) -> Vec<(String, u64)> {
    let Ok(rec) = u32::try_from(frn & FRN_MASK) else {
        return Vec::new();
    };
    let rows = ix.volumes[v].streams_of(rec);
    rows.iter()
        .filter_map(|s| {
            let (_, name) = ix.name(s[1]).split_once(':')?;
            Some((name.to_owned(), ix.size(s[1])))
        })
        .collect()
}

/// Make file `frn`'s stream entries match `streams`, the `(name, size)` of
/// each alternate data stream as Windows lists it (none if the file is
/// gone). Each is named `file:stream` beside the file's primary name, with
/// the file's time and attributes. Unchanged entries are kept as they are.
/// Returns whether anything changed.
pub fn restream(ix: &mut Index, v: usize, frn: u64, streams: &[(String, u64)]) -> bool {
    let Some(r) = u32::try_from(frn & FRN_MASK)
        .ok()
        .filter(|&r| (r as usize) < MAX_RECORDS)
    else {
        return false;
    };
    let primary = slot_get(ix, v, r as usize);
    let owner = primary != NONE && ix.flags(primary) & F_ROOT == 0;
    let (p, bits, mtime) = match owner {
        true => (
            ix.parent(primary),
            ix.flags(primary) & ATTR_MASK,
            ix.mtime(primary),
        ),
        false => (NONE, 0, 0),
    };
    let mut want: Vec<(String, u64)> = match owner {
        true => {
            let base = ix.name(primary);
            let named = streams.iter().filter(|s| !s.0.is_empty());
            named
                .map(|(n, size)| (format!("{base}:{n}"), *size))
                .collect()
        }
        false => Vec::new(),
    };
    let have: Vec<u32> = ix.volumes[v].streams_of(r).iter().map(|s| s[1]).collect();
    let mut b = Batch::default();
    let mut keep: Vec<(u32, u64)> = Vec::with_capacity(want.len());
    for e in have {
        let same = |w: &(String, u64)| ix.parent(e) == p && ix.name(e) == w.0;
        match want.iter().position(same) {
            Some(k) => keep.push((e, want.swap_remove(k).1)),
            None => ix.delete(e, &mut b),
        }
    }
    for (name, size) in want {
        keep.push((ix.push_batched(&name, p, bits, &mut b), size));
    }
    let mut changed = !b.is_empty();
    ix.commit(b);
    for &(e, size) in &keep {
        if ix.size(e) != size || ix.mtime(e) != mtime || ix.flags(e) & ATTR_MASK != bits {
            set_attrs(ix, e, bits);
            ix.set_stat(e, size, mtime);
            changed = true;
        }
    }
    if changed {
        set_stream_rows(ix, v, r, &keep);
    }
    changed
}

/// Point record `r`'s stream rows at `entries` (the table stays sorted).
fn set_stream_rows(ix: &mut Index, v: usize, r: u32, entries: &[(u32, u64)]) {
    let table = &mut ix.volumes[v].streams;
    table.retain(|s| s[0] != r);
    let mut rows: Vec<[u32; 2]> = entries.iter().map(|&(e, _)| [r, e]).collect();
    rows.sort_unstable();
    let at = table.partition_point(|s| s[0] < r);
    table.splice(at..at, rows);
}

/// Make file `frn`'s entries match its hard links as the file system reports
/// them: `(parent folder reference, name)` pairs, none if the file is gone.
/// Links in folders that are not indexed are left out; `attrs` are the file's
/// current Win32 attributes. Returns the entries whose size/mtime to refresh.
pub fn relink(ix: &mut Index, v: usize, frn: u64, attrs: u32, links: &[(u64, String)]) -> Vec<u32> {
    let rec = (frn & FRN_MASK) as usize;
    if rec >= MAX_RECORDS {
        return Vec::new();
    }
    let mut want: Vec<(u32, &str)> = links
        .iter()
        .filter_map(|(p, n)| {
            let pe = slot_get(ix, v, (*p & FRN_MASK) as usize);
            (pe != NONE).then_some((pe, n.as_str()))
        })
        .collect();
    let primary = slot_get(ix, v, rec);
    let have: Vec<u32> = std::iter::once(primary)
        .filter(|&e| e != NONE)
        .chain(ix.volumes[v].links_of(rec as u32).iter().map(|l| l[1]))
        .collect();
    let mut b = Batch::default();
    let mut keep = Vec::with_capacity(want.len());
    for e in have {
        let same = |&(p, n): &(u32, &str)| ix.parent[e as usize] == p && ix.name(e) == n;
        match want.iter().position(same) {
            Some(k) => {
                want.swap_remove(k);
                keep.push(e);
            }
            None => ix.delete(e, &mut b),
        }
    }
    let dir = if attrs & ATTR_DIRECTORY != 0 {
        F_DIR
    } else {
        0
    };
    let bits = dir | attr_flags(attrs);
    for (p, n) in want {
        keep.push(ix.push_batched(n, p, bits, &mut b));
    }
    for &e in &keep {
        set_attrs(ix, e, bits);
    }
    // The primary stays the primary for as long as it exists.
    if let Some(k) = keep.iter().position(|&e| e == primary) {
        keep.swap(0, k);
    }
    set_record(ix, v, rec, &keep);
    if keep.is_empty() {
        drop_streams(ix, v, rec as u32, &mut b);
    }
    ix.commit(b);
    keep
}

/// Point record `rec` at `entries`: the first is its primary, the rest links.
fn set_record(ix: &mut Index, v: usize, rec: usize, entries: &[u32]) {
    slot_set(ix, v, rec, entries.first().copied().unwrap_or(NONE));
    let r = rec as u32;
    let links = &mut ix.volumes[v].links;
    links.retain(|l| l[0] != r);
    let mut rows: Vec<[u32; 2]> = entries.iter().skip(1).map(|&e| [r, e]).collect();
    rows.sort_unstable();
    let at = links.partition_point(|l| l[0] < r);
    links.splice(at..at, rows);
}

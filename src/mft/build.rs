//! Record parsing (on parser threads) and the in-order index builder.
//!
//! A file with several hard links holds one non-DOS `$FILE_NAME` per link.
//! Every link becomes its own entry, like a folder walk would list it: the
//! best-ranked name is the record's primary (`Volume::slot`), the others go
//! to `Volume::links`, and all of them share the record's size, time and
//! attributes.
//!
//! Each alternate data stream becomes an entry too, named `file:stream`
//! beside its file (`Volume::streams`), with its own size and the file's
//! time and attributes.

use std::io;

use crate::index::{Index, Volume, ATTR_MASK, F_DELETED, F_DIR, F_ROOT, MAX_DEPTH, NONE};
use crate::ntfs::{
    apply_fixup, attr_flags, bad, link_names, namespace_rank, parse_record, streams, FileName,
    FIRST_USER_RECORD, ROOT_RECORD,
};
use crate::volume::ft_to_unix;

// Rec::bits
const HAS_SIZE: u8 = 1;
const HAS_MTIME: u8 = 2;
const IS_DIR: u8 = 4;
const HAS_ATTR: u8 = 8;
const HAS_NAME: u8 = 16;
/// An alternate data stream of `target`: name and size only.
const IS_STREAM: u8 = 32;

/// Rank of a Win32 or POSIX name: anything below is a DOS alias or nothing.
const REAL_NAME: u8 = 2;

/// One parsed record (or one extra link of it), ready to apply. Name bytes
/// live in [`Parsed::names`].
#[derive(Clone, Copy, Debug)]
struct Rec {
    target: u32,
    parent: u32,
    size: u64,
    name_off: u32,
    mtime: u32,
    name_len: u16,
    rank: u8,
    attr: u8,
    bits: u8,
}

impl Rec {
    fn empty(target: u32) -> Rec {
        Rec {
            target,
            parent: NONE,
            size: 0,
            name_off: 0,
            mtime: 0,
            name_len: 0,
            rank: 0,
            attr: 0,
            bits: 0,
        }
    }
}

#[derive(Default)]
pub(super) struct Parsed {
    recs: Vec<Rec>,
    names: Vec<u8>,
}

impl Parsed {
    /// Append UTF-16LE `utf16` to the name arena as UTF-8 and attach it to `r`.
    fn push_name(&mut self, r: &mut Rec, utf16: &[u8]) {
        let start = self.names.len();
        let units = utf16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]));
        let mut tmp = [0u8; 4];
        for c in char::decode_utf16(units) {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            self.names
                .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
        }
        r.name_off = start as u32;
        // <= 255 UTF-16 units => <= 765 bytes: always fits a u16.
        r.name_len = (self.names.len() - start) as u16;
    }

    /// One `IS_STREAM` rec per alternate data stream in `raw`, for `target`.
    fn push_streams(&mut self, target: u32, raw: &[u8]) {
        for s in streams(raw) {
            let mut st = Rec::empty(target);
            self.push_name(&mut st, s.utf16);
            st.size = s.size;
            st.bits = IS_STREAM;
            self.recs.push(st);
        }
    }

    /// Append `n` to the name arena and attach it to `r`.
    fn set_name(&mut self, r: &mut Rec, n: &FileName<'_>) {
        self.push_name(r, n.utf16);
        r.parent = u32::try_from(n.parent).unwrap_or(NONE);
        r.rank = namespace_rank(n.namespace);
        r.bits |= HAS_NAME;
    }
}

/// Parse one chunk of raw records (`first` = record number of `buf[0..frs]`).
pub(super) fn parse_chunk(first: u64, buf: &mut [u8], frs: usize, records: usize) -> Parsed {
    let mut out = Parsed {
        recs: Vec::with_capacity(buf.len() / frs),
        names: Vec::with_capacity(buf.len() / 32),
    };
    for (k, raw) in buf.chunks_exact_mut(frs).enumerate() {
        if raw[..4] != *b"FILE" || !apply_fixup(raw) {
            continue;
        }
        let Some(info) = parse_record(raw) else {
            continue;
        };
        let rec_no = first + k as u64;
        let target = if info.base != 0 { info.base } else { rec_no };
        if target as usize >= records || (target < FIRST_USER_RECORD && target != ROOT_RECORD) {
            continue;
        }
        out.push_streams(target as u32, raw);
        if info.name.is_none()
            && info.size.is_none()
            && info.mtime.is_none()
            && info.attrs.is_none()
        {
            continue;
        }
        let mut r = Rec::empty(target as u32);
        r.size = info.size.unwrap_or(0);
        r.mtime = info.mtime.map_or(0, ft_to_unix);
        r.attr = info.attrs.map_or(0, attr_flags);
        r.bits |= if info.size.is_some() { HAS_SIZE } else { 0 };
        r.bits |= if info.mtime.is_some() { HAS_MTIME } else { 0 };
        r.bits |= if info.attrs.is_some() { HAS_ATTR } else { 0 };
        r.bits |= if info.base == 0 && info.is_dir {
            IS_DIR
        } else {
            0
        };
        if let Some(n) = info.name {
            out.set_name(&mut r, &n);
        }
        out.recs.push(r);
        if info.links > 1 {
            // The other hard links of this record: name-only entries.
            for n in link_names(raw).filter(|n| Some(*n) != info.name) {
                let mut l = Rec::empty(target as u32);
                out.set_name(&mut l, &n);
                out.recs.push(l);
            }
        }
    }
    out
}

/// Accumulates records (base or extension, in any order) into entries.
pub(crate) struct Builder {
    idx: Index,
    /// record number -> primary entry
    slot: Vec<u32>,
    /// [record, entry] of every extra hard link
    links: Vec<[u32; 2]>,
    /// (record, name, size) of each stream, until `finish` names its entry
    pending: Vec<(u32, String, u64)>,
    /// [record, entry] of every stream entry
    streams: Vec<[u32; 2]>,
    /// entry -> parent record number (resolved in `finish`)
    parent_rec: Vec<u32>,
    rank: Vec<u8>,
}

impl Builder {
    pub(crate) fn new(records: usize) -> Builder {
        let mut idx = Index::default();
        // Typical volume: ~70% of records in use, ~22 bytes per name.
        idx.reserve(records * 3 / 4, records * 16);
        Builder {
            idx,
            slot: vec![NONE; records],
            links: Vec::new(),
            pending: Vec::new(),
            streams: Vec::new(),
            parent_rec: Vec::new(),
            rank: Vec::new(),
        }
    }

    fn new_entry(&mut self, name: &str, parent_rec: u32, rank: u8) -> u32 {
        let e = self.idx.push(name, NONE, 0, 0, 0);
        self.parent_rec.push(parent_rec);
        self.rank.push(rank);
        e
    }

    fn entry_for(&mut self, rec: usize) -> u32 {
        if self.slot[rec] == NONE {
            self.slot[rec] = self.new_entry("", NONE, 0);
        }
        self.slot[rec]
    }

    /// Give primary entry `e` the name `name`, or, when it already has a
    /// real name and this is another one, record a hard link.
    fn apply_name(&mut self, e: u32, r: &Rec, name: &str) {
        let ei = e as usize;
        let cur = self.rank[ei];
        if cur >= REAL_NAME && r.rank >= REAL_NAME {
            let link = if r.rank > cur {
                // The better name becomes the primary; the old one a link.
                let old = self.idx.name(e).to_owned();
                let link = self.new_entry(&old, self.parent_rec[ei], cur);
                self.set_primary(e, r, name);
                link
            } else {
                self.new_entry(name, r.parent, r.rank)
            };
            self.links.push([r.target, link]);
        } else if r.rank > cur {
            self.set_primary(e, r, name);
        }
    }

    fn set_primary(&mut self, e: u32, r: &Rec, name: &str) {
        self.idx.set_name(e, name);
        self.parent_rec[e as usize] = r.parent;
        self.rank[e as usize] = r.rank;
    }

    pub(super) fn apply(&mut self, p: &Parsed) {
        for r in &p.recs {
            if r.bits & IS_STREAM != 0 {
                let bytes =
                    &p.names[r.name_off as usize..r.name_off as usize + r.name_len as usize];
                let name = String::from_utf8_lossy(bytes).into_owned();
                self.pending.push((r.target, name, r.size));
                continue;
            }
            let e = self.entry_for(r.target as usize);
            let ei = e as usize;
            if r.bits & IS_DIR != 0 {
                self.idx.flags[ei] |= F_DIR;
            }
            if r.bits & HAS_ATTR != 0 {
                self.idx.flags[ei] = (self.idx.flags[ei] & !ATTR_MASK) | r.attr;
            }
            if r.bits & HAS_NAME != 0 {
                let bytes =
                    &p.names[r.name_off as usize..r.name_off as usize + r.name_len as usize];
                // SAFETY: produced by `char::encode_utf8` in `Parsed::set_name`.
                self.apply_name(e, r, unsafe { std::str::from_utf8_unchecked(bytes) });
            }
            if r.bits & HAS_SIZE != 0 {
                self.idx.size[ei] = r.size;
            }
            if r.bits & HAS_MTIME != 0 {
                self.idx.mtime[ei] = r.mtime;
            }
        }
    }

    /// Links share their record's size, time and attributes (which may have
    /// arrived after the link's name, from another record).
    fn share_link_stats(&mut self) {
        let ix = &mut self.idx;
        for &[rec, e] in &self.links {
            let p = self.slot[rec as usize] as usize;
            let e = e as usize;
            ix.size[e] = ix.size[p];
            ix.mtime[e] = ix.mtime[p];
            ix.flags[e] = ix.flags[p] & (F_DIR | ATTR_MASK);
        }
    }

    /// Each stream becomes an entry `file:stream` in its file's folder, with
    /// the file's time and attributes (once every name has arrived).
    fn add_streams(&mut self, root: u32) {
        for (rec, name, size) in std::mem::take(&mut self.pending) {
            let p = self.slot[rec as usize];
            if p == NONE || p == root || self.idx.name_len[p as usize] == 0 {
                continue;
            }
            let full = format!("{}:{}", self.idx.name(p), name);
            let e = self.new_entry(&full, self.parent_rec[p as usize], 0);
            let (pi, ei) = (p as usize, e as usize);
            self.idx.size[ei] = size;
            self.idx.mtime[ei] = self.idx.mtime[pi];
            self.idx.flags[ei] = self.idx.flags[pi] & ATTR_MASK;
            self.streams.push([rec, e]);
        }
    }

    /// Resolve parents, prune orphans/system metafiles, return the volume index
    /// (not yet finalized: call [`Index::finalize`], usually after a merge).
    pub(crate) fn finish(mut self, letter: u8, serial: u64) -> io::Result<Index> {
        let root = *self
            .slot
            .get(ROOT_RECORD as usize)
            .filter(|&&r| r != NONE)
            .ok_or_else(|| bad("root directory record missing"))?;
        self.share_link_stats();
        self.add_streams(root);
        let ix = &mut self.idx;
        ix.set_name(root, &format!("{}:", letter as char));
        ix.flags[root as usize] |= F_DIR | F_ROOT;
        for e in 0..ix.len() {
            let p = self.parent_rec[e];
            ix.parent[e] = if e as u32 == root || p == NONE {
                NONE
            } else {
                self.slot.get(p as usize).copied().unwrap_or(NONE)
            };
        }
        prune_unreachable(ix, root);
        self.links.sort_unstable();
        self.streams.sort_unstable();
        ix.volumes.push(Volume {
            letter,
            live: true,
            serial,
            root,
            slot: self.slot,
            links: self.links,
            streams: self.streams,
            ..Default::default()
        });
        Ok(self.idx)
    }
}

/// Mark entries whose parent chain does not reach `root` (orphans, `$Extend`
/// children, cycles, nameless placeholders) as deleted.
fn prune_unreachable(ix: &mut Index, root: u32) {
    const UNKNOWN: u8 = 0;
    const GOOD: u8 = 1;
    const BAD: u8 = 2;
    const VISITING: u8 = 3;
    let n = ix.len();
    let mut state = vec![UNKNOWN; n];
    state[root as usize] = GOOD;
    let mut stack = Vec::new();
    for start in 0..n {
        let mut cur = start;
        let verdict = loop {
            match state[cur] {
                GOOD => break GOOD,
                BAD | VISITING => break BAD,
                _ => {}
            }
            state[cur] = VISITING;
            stack.push(cur);
            let p = ix.parent[cur];
            if ix.name_len[cur] == 0 || p == NONE || stack.len() > MAX_DEPTH {
                break BAD;
            }
            cur = p as usize;
        };
        for s in stack.drain(..) {
            state[s] = verdict;
        }
    }
    for (e, s) in state.iter().enumerate() {
        if *s != GOOD {
            ix.flags[e] |= F_DELETED;
        }
    }
}

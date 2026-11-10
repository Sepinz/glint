//! Compact struct-of-arrays file index.
//!
//! Every entry costs ~23 bytes of fixed fields plus its UTF-8 name in one
//! shared arena. After [`Index::finalize`] entries are *physically* stored in
//! case-insensitive name order, so a search walks every array sequentially
//! and results come out already sorted by name — no per-query sort.

use rayon::prelude::*;
use std::cmp::Ordering;

pub const NONE: u32 = u32::MAX;
pub const F_DIR: u8 = 0x01;
pub const F_DELETED: u8 = 0x02;
pub const F_ROOT: u8 = 0x04;
// Win32 attributes (spare flag bits: no extra memory per entry).
pub const F_HIDDEN: u8 = 0x08;
pub const F_SYSTEM: u8 = 0x10;
pub const F_READONLY: u8 = 0x20;
/// Reparse point: symlink, junction, cloud placeholder, ...
pub const F_REPARSE: u8 = 0x40;
pub const ATTR_MASK: u8 = F_HIDDEN | F_SYSTEM | F_READONLY | F_REPARSE;
/// Transient: entry is part of an uncommitted [`Batch`].
pub(crate) const F_BATCH: u8 = 0x80;

const MAX_NAME: usize = u16::MAX as usize;
pub(crate) const MAX_DEPTH: usize = 4096;

/// One indexed volume.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Volume {
    pub letter: u8,
    /// Indexed from the MFT with an active USN journal: can be kept live.
    pub live: bool,
    pub serial: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub root: u32,
    /// MFT record number -> entry index (live volumes only). For a file with
    /// several hard links this is its primary link.
    pub slot: Vec<u32>,
    /// `[record, entry]` for every further hard link, sorted.
    pub links: Vec<[u32; 2]>,
    /// `[record, entry]` for every alternate data stream entry (live volumes
    /// only: the journal keeps them current), sorted.
    pub streams: Vec<[u32; 2]>,
}

/// The rows of `rec` in a table sorted by record.
fn rows_of(table: &[[u32; 2]], rec: u32) -> &[[u32; 2]] {
    let a = table.partition_point(|l| l[0] < rec);
    let b = a + table[a..].partition_point(|l| l[0] == rec);
    &table[a..b]
}

impl Volume {
    /// The extra hard-link rows of `rec` (its primary is `slot[rec]`).
    pub fn links_of(&self, rec: u32) -> &[[u32; 2]] {
        rows_of(&self.links, rec)
    }

    /// The stream entries of record `rec`.
    pub fn streams_of(&self, rec: u32) -> &[[u32; 2]] {
        rows_of(&self.streams, rec)
    }

    /// After entries were renumbered or removed: drop rows of removed
    /// stream entries and restore the sort order.
    pub(crate) fn fix_streams(&mut self) {
        self.streams.retain(|s| s[1] != NONE);
        self.streams.sort_unstable();
    }

    /// After entries were renumbered or removed: drop links to removed
    /// entries, promote a link wherever its record lost the primary, and
    /// restore the sort order (renumbering can swap two links of a record).
    pub(crate) fn fix_links(&mut self) {
        self.links.sort_unstable();
        let slot = &mut self.slot;
        self.links.retain(|&[rec, e]| {
            match slot.get_mut(rec as usize) {
                _ if e == NONE => false,
                Some(s) if *s == NONE => {
                    *s = e;
                    false
                }
                Some(_) => true,
                None => false, // no record table (static volume)
            }
        });
    }
}

#[derive(Default, Debug)]
pub struct Index {
    pub(crate) names: Vec<u8>,
    pub(crate) name_off: Vec<u32>,
    pub(crate) name_len: Vec<u16>,
    pub(crate) parent: Vec<u32>,
    pub(crate) size: Vec<u64>,
    /// Last-write time, Unix seconds.
    pub(crate) mtime: Vec<u32>,
    pub(crate) flags: Vec<u8>,
    /// Live (non-deleted) entries in name order. Unused (empty) while
    /// `identity` holds, i.e. entries are laid out sorted with none deleted.
    pub(crate) sorted: Vec<u32>,
    pub(crate) identity: bool,
    pub volumes: Vec<Volume>,
    /// Changes whenever entry indices are renumbered (results become stale).
    pub epoch: u64,
    /// Changes whenever content changes.
    pub version: u64,
    /// Not yet saved; set it with [`Index::mark_dirty`].
    pub dirty: bool,
    /// Counts [`Index::mark_dirty`] calls, so a save can tell whether
    /// anything changed while it was writing.
    pub(crate) edits: u64,
}

/// Name order of live entries without materializing the identity case.
#[derive(Clone, Copy, Debug)]
pub enum Order<'a> {
    /// Entries `0..n` in layout order.
    All(u32),
    List(&'a [u32]),
}

impl Order<'_> {
    pub fn len(&self) -> usize {
        match self {
            Order::All(n) => *n as usize,
            Order::List(l) => l.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn to_vec(&self) -> Vec<u32> {
        match self {
            Order::All(n) => (0..*n).collect(),
            Order::List(l) => l.to_vec(),
        }
    }
}

/// Pending set of mutations whose sort-order maintenance is done in one pass.
#[derive(Default, Debug)]
pub struct Batch {
    touched: Vec<u32>,
    added: Vec<u32>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.touched.is_empty() && self.added.is_empty()
    }
}

/// Push with ~6% growth instead of doubling: keeps commit charge tight for
/// large, long-lived arrays that only grow slowly after the initial build.
#[inline]
fn push_lean<T>(v: &mut Vec<T>, x: T) {
    if v.len() == v.capacity() {
        v.reserve_exact((v.len() / 16).max(256));
    }
    v.push(x);
}

#[inline]
fn extend_lean(v: &mut Vec<u8>, bytes: &[u8]) {
    if v.capacity() - v.len() < bytes.len() {
        v.reserve_exact((v.len() / 16).max(bytes.len()).max(4096));
    }
    v.extend_from_slice(bytes);
}

/// Arena offsets are `u32`; exceeding 4 GiB of names must fail loudly, never
/// wrap (a wrapped offset could slice mid-codepoint behind `name()`).
#[inline]
fn arena_offset(len: usize) -> u32 {
    u32::try_from(len).expect("name arena exceeds 4 GiB")
}

/// Case-insensitive name order. ASCII fast path; falls back to full Unicode
/// lowercase comparison (the result is identical to the slow path).
pub fn cmp_name_ci(a: &[u8], b: &[u8]) -> Ordering {
    for (&x, &y) in a.iter().zip(b) {
        if x == y {
            continue;
        }
        if x >= 0x80 || y >= 0x80 {
            return cmp_unicode_ci(a, b);
        }
        let (lx, ly) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if lx != ly {
            return lx.cmp(&ly);
        }
    }
    a.len().cmp(&b.len())
}

#[cold]
fn cmp_unicode_ci(a: &[u8], b: &[u8]) -> Ordering {
    let a = String::from_utf8_lossy(a);
    let b = String::from_utf8_lossy(b);
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

impl Index {
    pub fn len(&self) -> usize {
        self.flags.len()
    }
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
    /// Number of live entries.
    pub fn count(&self) -> usize {
        if self.identity {
            self.len()
        } else {
            self.sorted.len()
        }
    }
    /// Live entries in name order.
    pub fn order(&self) -> Order<'_> {
        if self.identity {
            Order::All(self.len() as u32)
        } else {
            Order::List(&self.sorted)
        }
    }
    fn materialize(&mut self) {
        if self.identity {
            self.sorted = (0..self.len() as u32).collect();
            self.identity = false;
        }
    }

    #[inline]
    /// Identifies the stored name: equal keys => equal names (names are
    /// de-duplicated, so many entries share one key).
    pub fn name_key(&self, i: u32) -> u64 {
        // Length too: an empty name may sit at any offset.
        (self.name_off[i as usize] as u64) << 16 | self.name_len[i as usize] as u64
    }

    #[inline]
    pub fn name_bytes(&self, i: u32) -> &[u8] {
        let i = i as usize;
        let o = self.name_off[i] as usize;
        &self.names[o..o + self.name_len[i] as usize]
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        // SAFETY: names only enter the arena as `&str` (push/set_name; the MFT
        // builder passes `encode_utf8` output), or from a snapshot whose arena and
        // every slice boundary are UTF-8-validated in `store::load`.
        unsafe { std::str::from_utf8_unchecked(self.name_bytes(i)) }
    }

    #[inline]
    pub fn flags(&self, i: u32) -> u8 {
        self.flags[i as usize]
    }
    #[inline]
    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & F_DIR != 0
    }
    #[inline]
    pub fn size(&self, i: u32) -> u64 {
        self.size[i as usize]
    }
    #[inline]
    pub fn mtime(&self, i: u32) -> u32 {
        self.mtime[i as usize]
    }
    #[inline]
    pub fn parent(&self, i: u32) -> u32 {
        self.parent[i as usize]
    }

    fn store_name(&mut self, name: &str) -> (u32, u16) {
        let mut end = name.len().min(MAX_NAME);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let off = arena_offset(self.names.len());
        extend_lean(&mut self.names, &name.as_bytes()[..end]);
        (off, end as u16)
    }

    /// Append an entry; returns its index.
    pub fn push(&mut self, name: &str, parent: u32, flags: u8, size: u64, mtime: u32) -> u32 {
        let i = self.flags.len() as u32;
        let (off, len) = self.store_name(name);
        push_lean(&mut self.name_off, off);
        push_lean(&mut self.name_len, len);
        push_lean(&mut self.parent, parent);
        push_lean(&mut self.size, size);
        push_lean(&mut self.mtime, mtime);
        push_lean(&mut self.flags, flags);
        i
    }

    /// Pre-size all arrays (bulk builders).
    pub(crate) fn reserve(&mut self, entries: usize, name_bytes: usize) {
        self.names.reserve(name_bytes);
        self.name_off.reserve(entries);
        self.name_len.reserve(entries);
        self.parent.reserve(entries);
        self.size.reserve(entries);
        self.mtime.reserve(entries);
        self.flags.reserve(entries);
    }

    /// Room for exactly this much more, with no growth slack.
    fn reserve_exact(&mut self, entries: usize, name_bytes: usize) {
        self.names.reserve_exact(name_bytes);
        self.name_off.reserve_exact(entries);
        self.name_len.reserve_exact(entries);
        self.parent.reserve_exact(entries);
        self.size.reserve_exact(entries);
        self.mtime.reserve_exact(entries);
        self.flags.reserve_exact(entries);
    }

    pub(crate) fn set_name(&mut self, i: u32, name: &str) {
        if self.name(i) == name {
            return;
        }
        let (o, l) = self.store_name(name);
        self.name_off[i as usize] = o;
        self.name_len[i as usize] = l;
    }

    /// Full path (`C:\dir\file`). The root alone renders as `C:\`.
    pub fn path_into(&self, i: u32, out: &mut String) {
        out.clear();
        let mut stack = [0u32; 96];
        let mut n = 0;
        let mut deep: Vec<u32> = Vec::new();
        let mut cur = i;
        while cur != NONE && (cur as usize) < self.len() && n + deep.len() < MAX_DEPTH {
            if n < stack.len() {
                stack[n] = cur;
                n += 1;
            } else {
                deep.push(cur);
            }
            cur = self.parent[cur as usize];
        }
        for &e in deep.iter().rev().chain(stack[..n].iter().rev()) {
            if !out.is_empty() {
                out.push('\\');
            }
            out.push_str(self.name(e));
        }
        if n == 1 && deep.is_empty() && self.flags(i) & F_ROOT != 0 {
            out.push('\\');
        }
    }

    /// Path of the containing folder (empty for a volume root).
    pub fn parent_path_into(&self, i: u32, out: &mut String) {
        match self.parent(i) {
            NONE => out.clear(),
            p => self.path_into(p, out),
        }
    }

    pub fn full_path(&self, i: u32) -> String {
        let mut s = String::new();
        self.path_into(i, &mut s);
        s
    }

    /// Total order: case-insensitive name, then raw bytes, then index.
    #[inline]
    pub fn cmp_entries(&self, a: u32, b: u32) -> Ordering {
        let (x, y) = (self.name_bytes(a), self.name_bytes(b));
        cmp_name_ci(x, y)
            .then_with(|| x.cmp(y))
            .then_with(|| a.cmp(&b))
    }

    /// Rebuild all arrays so that `order[k]` becomes entry `k`; entries not in
    /// `order` are dropped. Renumbers indices (bumps `epoch`).
    fn reorder(&mut self, order: &[u32]) {
        let n = self.len();
        let m = order.len();
        let mut remap = vec![NONE; n];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u32;
        }
        let map = |x: u32| if x == NONE { NONE } else { remap[x as usize] };
        // Small headroom so live updates don't immediately trigger reallocation.
        let head = |k: usize| k + k / 64 + 64;
        let (names, name_off) = self.gather_names(order, head);
        self.names = names;
        self.name_off = name_off;
        // Each array is gathered in parallel and replaces its old copy at once,
        // so the peak is the old index plus one new array, not two indexes.
        fn gather<T: Copy + Send + Sync>(
            old: &[T],
            order: &[u32],
            cap: usize,
            f: impl Fn(T) -> T + Sync,
        ) -> Vec<T> {
            let mut v = Vec::with_capacity(cap);
            v.par_extend(order.par_iter().map(|&o| f(old[o as usize])));
            v
        }
        self.name_len = gather(&self.name_len, order, head(m), |x| x);
        self.parent = gather(&self.parent, order, head(m), map);
        self.size = gather(&self.size, order, head(m), |x| x);
        self.mtime = gather(&self.mtime, order, head(m), |x| x);
        self.flags = gather(&self.flags, order, head(m), |f| f & !F_BATCH);
        for v in &mut self.volumes {
            v.root = map(v.root);
            v.slot.par_iter_mut().for_each(|s| *s = map(*s));
            v.links.iter_mut().for_each(|l| l[1] = map(l[1]));
            v.fix_links();
            v.streams.iter_mut().for_each(|s| s[1] = map(s[1]));
            v.fix_streams();
        }
        self.sorted = Vec::new();
        self.identity = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.version = self.version.wrapping_add(1);
    }

    /// New name arena and offsets for `order`. Equal names are adjacent in
    /// sorted order, so de-duplicating them is one comparison with the
    /// previous entry (shared arena slices are safe: the arena is append-only).
    fn gather_names(&self, order: &[u32], head: impl Fn(usize) -> usize) -> (Vec<u8>, Vec<u32>) {
        let m = order.len();
        let is_dup = |k: usize| k > 0 && self.name_bytes(order[k]) == self.name_bytes(order[k - 1]);
        let dup: Vec<bool> = (0..m).into_par_iter().map(is_dup).collect();
        let mut name_off = Vec::with_capacity(head(m));
        const BLOCK: usize = 1 << 16;
        let mut total = 0usize;
        // Arena bytes used before each block of BLOCK entries.
        let mut block_start = Vec::with_capacity(m / BLOCK + 2);
        for (k, &d) in dup.iter().enumerate() {
            if k % BLOCK == 0 {
                block_start.push(total);
            }
            if d {
                name_off.push(*name_off.last().unwrap_or(&0));
            } else {
                name_off.push(arena_offset(total));
                total += self.name_len[order[k] as usize] as usize;
            }
        }
        let mut names = Vec::with_capacity(head(total));
        names.resize(total, 0u8);
        // Copy in parallel: split the arena at block boundaries.
        let mut parts = Vec::with_capacity(m / BLOCK + 1);
        let mut rest: &mut [u8] = &mut names;
        let mut consumed = 0usize;
        for k0 in (0..m).step_by(BLOCK) {
            let k1 = (k0 + BLOCK).min(m);
            let end = if k1 == m {
                total
            } else {
                block_start[k1 / BLOCK]
            };
            let (part, tail) = std::mem::take(&mut rest).split_at_mut(end - consumed);
            parts.push((k0, k1, consumed, part));
            rest = tail;
            consumed = end;
        }
        parts.into_par_iter().for_each(|(k0, k1, base, part)| {
            for k in k0..k1 {
                if !dup[k] {
                    let src = self.name_bytes(order[k]);
                    let o = name_off[k] as usize - base;
                    part[o..o + src.len()].copy_from_slice(src);
                }
            }
        });
        (names, name_off)
    }

    /// Drop deleted entries, sort by name, and lay entries out in that order.
    pub fn finalize(&mut self) {
        let mut order: Vec<u32> = (0..self.len() as u32)
            .into_par_iter()
            .filter(|&i| self.flags[i as usize] & F_DELETED == 0)
            .collect();
        order.par_sort_unstable_by(|&a, &b| self.cmp_entries(a, b));
        self.reorder(&order);
        self.compute_dir_sizes();
    }

    /// True if entries are not laid out contiguously in sorted order.
    pub fn needs_compact(&self) -> bool {
        !self.identity
    }

    /// Forget the volumes `gone` selects and mark all their entries deleted.
    /// Entries stay where they are: call [`Index::finalize`] after.
    pub(crate) fn drop_volumes(&mut self, gone: impl Fn(&Volume) -> bool) {
        let roots: Vec<u32> = self
            .volumes
            .iter()
            .filter(|v| gone(v))
            .map(|v| v.root)
            .collect();
        if roots.is_empty() {
            return;
        }
        for (e, top) in self.tops().into_iter().enumerate() {
            if roots.contains(&top) {
                self.flags[e] |= F_DELETED;
            }
        }
        self.volumes.retain(|v| !gone(v));
    }

    /// For every entry, the top of its parent chain (a volume root), or
    /// `NONE` for a chain too deep to be real.
    fn tops(&self) -> Vec<u32> {
        const UNSET: u32 = NONE - 1;
        let mut top = vec![UNSET; self.len()];
        let mut stack = Vec::new();
        for start in 0..self.len() as u32 {
            let mut cur = start;
            let found = loop {
                match top[cur as usize] {
                    UNSET => {}
                    t => break t,
                }
                stack.push(cur);
                let p = self.parent[cur as usize];
                if p == NONE {
                    break cur;
                }
                if stack.len() > MAX_DEPTH {
                    break NONE;
                }
                cur = p;
            };
            for e in stack.drain(..) {
                top[e as usize] = found;
            }
        }
        top
    }

    /// Deleted entries hold enough memory (over 1/16 of all entries) to pay
    /// for a compaction, a full re-sort that is too slow for a hot path.
    pub fn worth_compacting(&self) -> bool {
        !self.identity && (self.len() - self.sorted.len()) * 16 > self.len()
    }

    /// Re-lay out entries in current sorted order, dropping deleted ones.
    pub fn compact(&mut self) {
        if self.needs_compact() {
            let order = std::mem::take(&mut self.sorted);
            self.reorder(&order);
        }
    }

    /// Concatenate independently built per-volume indexes. Call `finalize` after.
    /// The largest part grows in place, so it is never held twice.
    pub fn merge(mut parts: Vec<Index>) -> Index {
        if let Some(big) = (0..parts.len()).max_by_key(|&k| parts[k].len()) {
            parts.swap(0, big);
        }
        let mut parts = parts.into_iter();
        let Some(mut out) = parts.next() else {
            return Index::default();
        };
        out.sorted = Vec::new();
        out.identity = false;
        let rest: Vec<Index> = parts.collect();
        let total: usize = rest.iter().map(Index::len).sum();
        let names: usize = rest.iter().map(|p| p.names.len()).sum();
        out.reserve_exact(total, names);
        for p in rest {
            let base = out.len() as u32;
            let nbase = arena_offset(out.names.len() + p.names.len()) - p.names.len() as u32;
            let shift = |x: u32| if x == NONE { NONE } else { x + base };
            out.names.extend_from_slice(&p.names);
            out.name_off.extend(p.name_off.iter().map(|o| o + nbase));
            out.name_len.extend_from_slice(&p.name_len);
            out.parent.extend(p.parent.iter().map(|&x| shift(x)));
            out.size.extend_from_slice(&p.size);
            out.mtime.extend_from_slice(&p.mtime);
            out.flags.extend_from_slice(&p.flags);
            for mut v in p.volumes {
                v.root = shift(v.root);
                v.slot.iter_mut().for_each(|s| *s = shift(*s));
                v.links.iter_mut().for_each(|l| l[1] = shift(l[1]));
                v.streams.iter_mut().for_each(|s| s[1] = shift(s[1]));
                out.volumes.push(v);
            }
        }
        out.volumes.sort_by_key(|v| v.letter); // whichever part came first
        out
    }

    // ---- incremental maintenance -------------------------------------------------

    /// Register an existing entry as about to change (call *before* renaming).
    pub(crate) fn mark(&mut self, i: u32, b: &mut Batch) {
        self.materialize();
        let f = &mut self.flags[i as usize];
        if *f & F_BATCH != 0 {
            return;
        }
        if *f & F_DELETED == 0 {
            b.touched.push(i); // currently present in `sorted`
        }
        *f |= F_BATCH;
        b.added.push(i);
    }

    /// Append a brand-new entry as part of a batch.
    pub(crate) fn push_batched(
        &mut self,
        name: &str,
        parent: u32,
        flags: u8,
        b: &mut Batch,
    ) -> u32 {
        self.materialize(); // before the push: the new entry is not yet sorted
        let i = self.push(name, parent, flags | F_BATCH, 0, 0);
        b.added.push(i);
        i
    }

    pub(crate) fn delete(&mut self, i: u32, b: &mut Batch) {
        self.detach_size(i);
        self.mark(i, b);
        self.flags[i as usize] |= F_DELETED;
    }

    /// Apply a batch to the sorted order: O(n) retain + merge instead of
    /// O(n) per individual insert.
    pub(crate) fn commit(&mut self, b: Batch) {
        if b.is_empty() {
            return;
        }
        if !b.touched.is_empty() {
            let mut mask = vec![false; self.len()];
            for &t in &b.touched {
                mask[t as usize] = true;
            }
            self.sorted.retain(|&x| !mask[x as usize]);
        }
        let mut add = Vec::with_capacity(b.added.len());
        for &i in &b.added {
            let f = &mut self.flags[i as usize];
            *f &= !F_BATCH;
            if *f & F_DELETED == 0 {
                add.push(i);
            }
        }
        add.sort_unstable_by(|&x, &y| self.cmp_entries(x, y));
        self.merge_sorted(&add);
        self.version = self.version.wrapping_add(1);
        self.mark_dirty();
    }

    /// Merge `add` (sorted, not yet listed) into `sorted`, in place and from
    /// the back: a binary search per new entry and one move of each old one,
    /// instead of comparing every old entry or shifting the tail per insert.
    fn merge_sorted(&mut self, add: &[u32]) {
        if add.is_empty() {
            return;
        }
        let mut sorted = std::mem::take(&mut self.sorted);
        let mut end = sorted.len();
        if sorted.capacity() - end < add.len() {
            sorted.reserve_exact(add.len().max(end / 64));
        }
        sorted.resize(end + add.len(), 0);
        let mut write = sorted.len();
        for &a in add.iter().rev() {
            let at = sorted[..end].partition_point(|&p| self.cmp_entries(p, a) == Ordering::Less);
            write -= end - at;
            sorted.copy_within(at..end, write);
            write -= 1;
            sorted[write] = a;
            end = at;
        }
        self.sorted = sorted;
    }

    /// Note an unsaved change (content, journal position, or both).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.edits = self.edits.wrapping_add(1);
    }
}

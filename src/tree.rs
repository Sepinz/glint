//! Folder sizes: every folder's `size` is the total of the files below it.
//!
//! Folders carry no size of their own, so the existing `size` column holds the
//! total at zero extra memory. It is computed bottom-up once per full build
//! and then kept exact by applying deltas along the ancestor chain whenever a
//! file grows, shrinks, moves or disappears.

use crate::index::{Index, F_DELETED, F_DIR, MAX_DEPTH, NONE};

impl Index {
    /// Recompute all folder totals from file sizes (O(n), ~6 bytes/entry
    /// transient). Deleted entries are ignored.
    pub(crate) fn compute_dir_sizes(&mut self) {
        const UNKNOWN: u16 = u16::MAX;
        let n = self.len();
        let mut depth = vec![UNKNOWN; n];
        let mut chain = Vec::new();
        for start in 0..n {
            let mut cur = start;
            let mut d = loop {
                if depth[cur] != UNKNOWN {
                    break depth[cur];
                }
                let p = self.parent[cur];
                if p == NONE || p as usize >= n || chain.len() >= MAX_DEPTH {
                    depth[cur] = 0;
                    break 0;
                }
                chain.push(cur);
                cur = p as usize;
            };
            while let Some(c) = chain.pop() {
                d = d.saturating_add(1).min(MAX_DEPTH as u16);
                depth[c] = d;
            }
        }
        for i in 0..n {
            if self.flags[i] & F_DIR != 0 {
                self.size[i] = 0;
            }
        }
        // Counting sort by depth, deepest first, so children finish before parents.
        let max = depth.iter().copied().max().unwrap_or(0) as usize;
        let mut start = vec![0u32; max + 2];
        for &d in &depth {
            start[max - d as usize + 1] += 1;
        }
        for k in 1..start.len() {
            start[k] += start[k - 1];
        }
        let mut order = vec![0u32; n];
        for (i, &d) in depth.iter().enumerate() {
            let slot = &mut start[max - d as usize];
            order[*slot as usize] = i as u32;
            *slot += 1;
        }
        drop(depth);
        for &i in &order {
            let i = i as usize;
            let p = self.parent[i];
            let live_dir = |k: usize| self.flags[k] & (F_DELETED | F_DIR) == F_DIR;
            if p != NONE && self.flags[i] & F_DELETED == 0 && live_dir(p as usize) {
                self.size[p as usize] = self.size[p as usize].saturating_add(self.size[i]);
            }
        }
    }

    /// Apply `+amount` (grow) or `-amount` to every live ancestor of `i`.
    /// Stops at a deleted ancestor: its total already left the tree with it.
    fn bubble(&mut self, i: u32, amount: u64, grow: bool) {
        if amount == 0 {
            return;
        }
        let mut cur = self.parent[i as usize];
        let mut steps = 0;
        while cur != NONE && (cur as usize) < self.len() && steps < MAX_DEPTH {
            let c = cur as usize;
            // A deleted folder's total already left the tree; a "parent" that
            // is not a folder (corrupt image) never accumulates.
            if self.flags[c] & (F_DELETED | F_DIR) != F_DIR {
                break;
            }
            self.size[c] = if grow {
                self.size[c].saturating_add(amount)
            } else {
                self.size[c].saturating_sub(amount)
            };
            cur = self.parent[c];
            steps += 1;
        }
    }

    /// Take `i` (and its subtree total) out of its ancestors' totals. Call
    /// before it is deleted or moved.
    pub(crate) fn detach_size(&mut self, i: u32) {
        if self.flags[i as usize] & F_DELETED == 0 {
            self.bubble(i, self.size[i as usize], false);
        }
    }

    /// Add `i` (and its subtree total) to its ancestors. Call after a move.
    pub(crate) fn attach_size(&mut self, i: u32) {
        if self.flags[i as usize] & F_DELETED == 0 {
            self.bubble(i, self.size[i as usize], true);
        }
    }

    /// Update size and mtime; a file's size change also updates its folders.
    /// A folder keeps its computed total (only its mtime changes).
    pub(crate) fn set_stat(&mut self, i: u32, size: u64, mtime: u32) {
        let k = i as usize;
        self.mtime[k] = mtime;
        if self.flags[k] & F_DIR != 0 {
            return;
        }
        let old = self.size[k];
        self.size[k] = size;
        if self.flags[k] & F_DELETED == 0 {
            if size > old {
                self.bubble(i, size - old, true);
            } else {
                self.bubble(i, old - size, false);
            }
        }
    }
}

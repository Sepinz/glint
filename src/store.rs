//! Flat binary snapshot of the index.
//!
//! The arrays are written verbatim, so loading is a handful of large
//! `read_exact` calls straight into the final allocations (no parsing, no
//! per-entry allocation). The snapshot is untrusted input: sizes are checked
//! against the file length before allocating, and every offset, parent link
//! and UTF-8 boundary is validated before the index is used.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use bytemuck::{cast_slice, cast_slice_mut, Pod, Zeroable};

use crate::index::{Index, Volume, F_BATCH, F_DELETED, NONE};

const MAGIC: [u8; 8] = *b"GLINTDB\x06";
/// `sorted_len` sentinel: entries are laid out in name order (no array stored).
const IDENTITY: u64 = u64::MAX;
const HEADER: usize = 8 + 4 * 8;
/// letter, live, serial, journal id, next USN, root, slot length, link
/// count, stream count
const VOL_META: usize = 64;
/// name_off + name_len + parent + size + mtime + flags
const PER_ENTRY: u64 = 4 + 2 + 4 + 8 + 4 + 1;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("index snapshot: {msg}"))
}

/// Write atomically (temp file + rename). The index should be compacted.
pub fn save(ix: &Index, path: &Path) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(&MAGIC)?;
        let sorted_len = if ix.identity {
            IDENTITY
        } else {
            ix.sorted.len() as u64
        };
        for x in [
            ix.len() as u64,
            ix.names.len() as u64,
            sorted_len,
            ix.volumes.len() as u64,
        ] {
            w.write_all(&x.to_le_bytes())?;
        }
        for v in &ix.volumes {
            let mut m = [0u8; VOL_META];
            m[0] = v.letter;
            m[1] = v.live as u8;
            m[8..16].copy_from_slice(&v.serial.to_le_bytes());
            m[16..24].copy_from_slice(&v.journal_id.to_le_bytes());
            m[24..32].copy_from_slice(&v.next_usn.to_le_bytes());
            m[32..36].copy_from_slice(&v.root.to_le_bytes());
            m[40..48].copy_from_slice(&(v.slot.len() as u64).to_le_bytes());
            m[48..56].copy_from_slice(&(v.links.len() as u64).to_le_bytes());
            m[56..64].copy_from_slice(&(v.streams.len() as u64).to_le_bytes());
            w.write_all(&m)?;
        }
        w.write_all(&ix.names)?;
        w.write_all(cast_slice(&ix.name_off))?;
        w.write_all(cast_slice(&ix.name_len))?;
        w.write_all(cast_slice(&ix.parent))?;
        w.write_all(cast_slice(&ix.size))?;
        w.write_all(cast_slice(&ix.mtime))?;
        w.write_all(&ix.flags)?;
        if !ix.identity {
            w.write_all(cast_slice(&ix.sorted))?;
        }
        for v in &ix.volumes {
            w.write_all(cast_slice(&v.slot))?;
            w.write_all(cast_slice(&v.links))?;
            w.write_all(cast_slice(&v.streams))?;
        }
        w.flush()?;
    }
    fs::rename(&tmp, path)
}

fn read_vec<T: Pod + Zeroable>(f: &mut File, len: usize) -> io::Result<Vec<T>> {
    let mut v = vec![T::zeroed(); len];
    f.read_exact(cast_slice_mut(&mut v))?;
    Ok(v)
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]))
}

pub fn load(path: &Path) -> io::Result<Index> {
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();
    let mut hdr = [0u8; HEADER];
    f.read_exact(&mut hdr)?;
    if hdr[..8] != MAGIC {
        return Err(invalid("bad magic/version"));
    }
    let (n, names_len, sorted_len, vols) = (
        le64(&hdr, 8),
        le64(&hdr, 16),
        le64(&hdr, 24),
        le64(&hdr, 32),
    );
    let identity = sorted_len == IDENTITY;
    let sorted_len = if identity { 0 } else { sorted_len };
    if n >= NONE as u64 || vols > 26 {
        return Err(invalid("implausible header"));
    }
    let mut metas = Vec::with_capacity(vols as usize);
    for _ in 0..vols {
        let mut m = [0u8; VOL_META];
        f.read_exact(&mut m)?;
        metas.push(m);
    }
    // Exact expected length, computed with overflow checks before any big allocation.
    let mut expected = (HEADER as u64 + vols * VOL_META as u64)
        .checked_add(names_len)
        .and_then(|x| x.checked_add(n.checked_mul(PER_ENTRY)?))
        .and_then(|x| x.checked_add(sorted_len.checked_mul(4)?));
    for m in &metas {
        expected = expected.and_then(|a| a.checked_add(le64(m, 40).checked_mul(4)?));
        expected = expected.and_then(|a| a.checked_add(le64(m, 48).checked_mul(8)?));
        expected = expected.and_then(|a| a.checked_add(le64(m, 56).checked_mul(8)?));
    }
    if expected != Some(file_len) {
        return Err(invalid("length mismatch"));
    }
    let n = n as usize;
    let mut ix = Index {
        names: read_vec(&mut f, names_len as usize)?,
        name_off: read_vec(&mut f, n)?,
        name_len: read_vec(&mut f, n)?,
        parent: read_vec(&mut f, n)?,
        size: read_vec(&mut f, n)?,
        mtime: read_vec(&mut f, n)?,
        flags: read_vec(&mut f, n)?,
        sorted: read_vec(&mut f, sorted_len as usize)?,
        identity,
        ..Default::default()
    };
    for m in &metas {
        ix.volumes.push(Volume {
            letter: m[0],
            live: m[1] != 0,
            serial: le64(m, 8),
            journal_id: le64(m, 16),
            next_usn: le64(m, 24) as i64,
            root: u32::from_le_bytes(m[32..36].try_into().unwrap_or([0; 4])),
            slot: read_vec(&mut f, le64(m, 40) as usize)?,
            links: read_vec(&mut f, le64(m, 48) as usize)?,
            streams: read_vec(&mut f, le64(m, 56) as usize)?,
        });
    }
    validate(&mut ix)?;
    Ok(ix)
}

fn validate(ix: &mut Index) -> io::Result<()> {
    let n = ix.len();
    let text = std::str::from_utf8(&ix.names).map_err(|_| invalid("names are not UTF-8"))?;
    for i in 0..n {
        let (o, l) = (ix.name_off[i] as usize, ix.name_len[i] as usize);
        if o + l > text.len() || !text.is_char_boundary(o) || !text.is_char_boundary(o + l) {
            return Err(invalid("name out of bounds"));
        }
        let p = ix.parent[i];
        if p != NONE && p as usize >= n {
            return Err(invalid("parent out of range"));
        }
        ix.flags[i] &= !F_BATCH;
    }
    // `sorted` must hold every live entry exactly once (batch invariant).
    let mut seen = vec![false; n];
    for &s in &ix.sorted {
        let s = s as usize;
        if s >= n || seen[s] || ix.flags[s] & F_DELETED != 0 {
            return Err(invalid("bad sort order"));
        }
        seen[s] = true;
    }
    let live = ix.flags.iter().filter(|&&f| f & F_DELETED == 0).count();
    if (ix.identity && (!ix.sorted.is_empty() || live != n))
        || (!ix.identity && ix.sorted.len() != live)
    {
        return Err(invalid("sort order incomplete"));
    }
    for v in &ix.volumes {
        if !v.letter.is_ascii_uppercase()
            || v.root as usize >= n
            || v.slot.iter().any(|&s| s != NONE && s as usize >= n)
            || !links_valid(v, n)
            || !streams_valid(v, &ix.flags)
        {
            return Err(invalid("bad volume table"));
        }
    }
    Ok(())
}

/// Links are sorted and unique, name existing entries, and belong to records
/// that have a different primary (the invariant `Volume::fix_links` keeps).
fn links_valid(v: &Volume, n: usize) -> bool {
    v.links.windows(2).all(|w| w[0] < w[1])
        && v.links.iter().all(|&[rec, e]| {
            (e as usize) < n
                && v.slot
                    .get(rec as usize)
                    .is_some_and(|&s| s != NONE && s != e)
        })
}

/// Stream rows are sorted, each names a live entry of its own (not its
/// file's primary entry or a link, nor another row's), and belongs to a
/// record that has a primary entry.
fn streams_valid(v: &Volume, flags: &[u8]) -> bool {
    let mut seen = vec![false; flags.len()];
    v.streams.windows(2).all(|w| w[0] < w[1])
        && v.streams.iter().all(|&[rec, e]| {
            let ok = flags.get(e as usize).is_some_and(|&f| f & F_DELETED == 0)
                && !seen[e as usize]
                && v.slot
                    .get(rec as usize)
                    .is_some_and(|&s| s != NONE && s != e)
                && !v.links_of(rec).iter().any(|l| l[1] == e);
            if ok {
                seen[e as usize] = true;
            }
            ok
        })
}

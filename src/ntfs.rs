//! Raw `$MFT` reader.
//!
//! Instead of asking the file system about every directory, read the Master
//! File Table straight off the volume in large sequential chunks: one record
//! per file, containing its name, parent directory, size and timestamps.
//! Ranges that the `$MFT` bitmap marks as unused (deleted files) are not read
//! at all. The scan pipeline that drives this lives in [`crate::mft`].
//!
//! Everything here parses untrusted on-disk bytes, so all access is
//! bounds-checked and malformed records are skipped, never trusted.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::ptr::NonNull;

use crate::index::{F_HIDDEN, F_READONLY, F_REPARSE, F_SYSTEM};
use crate::volume::ioctl;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
pub(crate) const ROOT_RECORD: u64 = 5;
pub(crate) const FIRST_USER_RECORD: u64 = 16;
pub(crate) const CHUNK: usize = 4 << 20;
const ALIGN: usize = 4096;
/// NTFS multi-sector protection always uses a 512-byte stride.
const USA_STRIDE: usize = 512;

const AT_STANDARD_INFORMATION: u32 = 0x10;
const AT_FILE_NAME: u32 = 0x30;
const AT_DATA: u32 = 0x80;
const AT_BITMAP: u32 = 0xB0;
/// Unit of the in-use test: reads are never split finer than this (it keeps
/// requests large and sector-aligned).
const GRAIN: usize = 64 << 10;
/// Sanity cap for the `$MFT` bitmap (1 GiB covers 8 G records).
const MAX_BITMAP: u64 = 1 << 30;
const AT_END: u32 = 0xFFFF_FFFF;

// ---- little-endian, bounds-checked readers ---------------------------------------

#[inline]
fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap_or([0; 4])))
}
#[inline]
fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8)
        .map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
}

// ---- volume geometry -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub serial: u64,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub frs_size: u32,
    pub mft_valid_len: u64,
    pub mft_start_lcn: u64,
}

impl VolumeInfo {
    fn parse(b: &[u8]) -> io::Result<VolumeInfo> {
        let v = VolumeInfo {
            serial: u64_at(b, 0),
            bytes_per_sector: u32_at(b, 40),
            bytes_per_cluster: u32_at(b, 44),
            frs_size: u32_at(b, 48),
            mft_valid_len: u64_at(b, 56),
            mft_start_lcn: u64_at(b, 64),
        };
        let pow2 = |x: u32, lo: u32, hi: u32| x.is_power_of_two() && (lo..=hi).contains(&x);
        if b.len() < 72
            || !pow2(v.bytes_per_sector, 512, 4096)
            || !pow2(v.bytes_per_cluster, 512, 2 << 20)
            || !pow2(v.frs_size, 1024, 4096)
        {
            return Err(bad("unexpected NTFS volume geometry"));
        }
        Ok(v)
    }
}

pub fn volume_info(vol: &File) -> io::Result<VolumeInfo> {
    let mut out = [0u8; 128];
    let n = ioctl(vol, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut out)?;
    VolumeInfo::parse(&out[..n])
}

pub(crate) fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

// ---- record parsing (pure, unit-tested) --------------------------------------------

/// Undo NTFS update-sequence fixups in place. False if the record is torn.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    let uo = u16_at(rec, 4) as usize;
    let uc = u16_at(rec, 6) as usize;
    if uc < 2 || uo + uc * 2 > rec.len() || (uc - 1) * USA_STRIDE > rec.len() {
        return false;
    }
    let usn = [rec[uo], rec[uo + 1]];
    for k in 1..uc {
        let end = k * USA_STRIDE - 2;
        if rec[end..end + 2] != usn {
            return false;
        }
        rec[end] = rec[uo + 2 * k];
        rec[end + 1] = rec[uo + 2 * k + 1];
    }
    true
}

/// Iterate `(type, attribute bytes)` of a fixed-up FILE record.
fn attrs(rec: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14) as usize;
    std::iter::from_fn(move || {
        if off + 16 > used {
            return None;
        }
        let ty = u32_at(rec, off);
        let len = u32_at(rec, off + 4) as usize;
        if ty == AT_END || len < 16 || off + len > used {
            return None;
        }
        let a = &rec[off..off + len];
        off += len;
        Some((ty, a))
    })
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    if a.get(8) != Some(&0) {
        return None;
    }
    let len = u32_at(a, 0x10) as usize;
    let off = u16_at(a, 0x14) as usize;
    a.get(off..off.checked_add(len)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileName<'a> {
    pub parent: u64,
    pub namespace: u8,
    /// Raw UTF-16LE bytes.
    pub utf16: &'a [u8],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordInfo<'a> {
    /// Base record number for extension records, 0 for base records.
    pub base: u64,
    pub is_dir: bool,
    pub name: Option<FileName<'a>>,
    pub size: Option<u64>,
    /// $STANDARD_INFORMATION last-write FILETIME.
    pub mtime: Option<u64>,
    /// $STANDARD_INFORMATION Win32 file attributes.
    pub attrs: Option<u32>,
    /// $FILE_NAME attributes that are not DOS 8.3 aliases, i.e. hard links
    /// stored in this record (only the best-ranked one is in `name`).
    pub links: u16,
}

/// Win32 `FILE_ATTRIBUTE_*` bits -> index attribute flags.
pub fn attr_flags(a: u32) -> u8 {
    let mut f = 0;
    f |= if a & 0x1 != 0 { F_READONLY } else { 0 };
    f |= if a & 0x2 != 0 { F_HIDDEN } else { 0 };
    f |= if a & 0x4 != 0 { F_SYSTEM } else { 0 };
    f |= if a & 0x400 != 0 { F_REPARSE } else { 0 };
    f
}

/// Prefer Win32 names over POSIX, and anything over DOS 8.3 aliases.
pub fn namespace_rank(ns: u8) -> u8 {
    match ns {
        1 | 3 => 3,
        0 => 2,
        2 => 1,
        _ => 0,
    }
}

/// $FILE_NAME namespace of an 8.3 alias: another spelling of a link, not a link.
const DOS_NAMESPACE: u8 = 2;

fn file_name(a: &[u8]) -> Option<FileName<'_>> {
    let v = resident_value(a)?;
    let (&nlen, &ns) = (v.get(0x40)?, v.get(0x41)?);
    let utf16 = v.get(0x42..0x42 + nlen as usize * 2)?;
    Some(FileName {
        parent: u64_at(v, 0) & FRN_MASK,
        namespace: ns,
        utf16,
    })
}

/// Every hard link stored in a (fixed-up) record: its non-DOS $FILE_NAMEs.
pub fn link_names(rec: &[u8]) -> impl Iterator<Item = FileName<'_>> {
    attrs(rec)
        .filter(|&(ty, _)| ty == AT_FILE_NAME)
        .filter_map(|(_, a)| file_name(a))
        .filter(|n| n.namespace != DOS_NAMESPACE)
}

/// `WofCompressedData` (UTF-16LE): where Windows keeps the compressed
/// contents of a CompactOS / WOF file. Internal, not an alternate data
/// stream of the user's, so skipped (also when Windows lists streams).
pub const WOF_STREAM: [u8; 34] = {
    let s = *b"WofCompressedData";
    let mut out = [0u8; 34];
    let mut i = 0;
    while i < s.len() {
        out[2 * i] = s[i];
        i += 1;
    }
    out
};

/// An alternate data stream as a record stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream<'a> {
    /// Raw UTF-16LE name.
    pub utf16: &'a [u8],
    pub size: u64,
}

/// The alternate data streams (named `$DATA` attributes) of a fixed-up
/// record. A large stream can be split over several records; only the part
/// starting at cluster 0 carries its size, so each stream is listed once.
pub fn streams(rec: &[u8]) -> impl Iterator<Item = Stream<'_>> {
    attrs(rec)
        .filter(|&(ty, _)| ty == AT_DATA)
        .filter_map(|(_, a)| {
            let units = *a.get(9)? as usize;
            if units == 0 {
                return None; // the unnamed stream: the file's contents
            }
            let off = u16_at(a, 10) as usize;
            let resident = *a.get(8)? == 0;
            // The name follows the (resident or non-resident) header.
            if off < if resident { 0x18 } else { 0x40 } {
                return None;
            }
            let utf16 = a.get(off..off + 2 * units)?;
            if utf16 == WOF_STREAM.as_slice() {
                return None; // see `WOF_STREAM`
            }
            let size = match resident {
                true => u32_at(a, 0x10) as u64,
                false if u64_at(a, 0x10) == 0 => u64_at(a, 0x30),
                false => return None,
            };
            Some(Stream { utf16, size })
        })
}

/// Parse a fixed-up FILE record. `None` if not a FILE record or not in use.
pub fn parse_record(rec: &[u8]) -> Option<RecordInfo<'_>> {
    if rec.get(..4) != Some(b"FILE") {
        return None;
    }
    let flags = u16_at(rec, 0x16);
    if flags & 0x01 == 0 {
        return None;
    }
    let mut info = RecordInfo {
        base: u64_at(rec, 0x20) & FRN_MASK,
        is_dir: flags & 0x02 != 0,
        ..Default::default()
    };
    for (ty, a) in attrs(rec) {
        match ty {
            AT_STANDARD_INFORMATION => {
                if let Some(v) = resident_value(a) {
                    if v.len() >= 16 {
                        info.mtime = Some(u64_at(v, 8));
                    }
                    if v.len() >= 0x24 {
                        info.attrs = Some(u32_at(v, 0x20));
                    }
                }
            }
            AT_FILE_NAME => {
                let Some(n) = file_name(a) else { continue };
                if n.namespace != DOS_NAMESPACE {
                    info.links = info.links.saturating_add(1);
                }
                let better = info
                    .name
                    .is_none_or(|b| namespace_rank(n.namespace) > namespace_rank(b.namespace));
                if better {
                    info.name = Some(n);
                }
            }
            AT_DATA if a.get(9) == Some(&0) => {
                // Unnamed stream = file contents.
                if a[8] == 0 {
                    info.size = Some(u32_at(a, 0x10) as u64);
                } else if u64_at(a, 0x10) == 0 {
                    info.size = Some(u64_at(a, 0x30));
                }
            }
            _ => {}
        }
    }
    Some(info)
}

/// Decode an NTFS mapping-pairs run list into `(lcn, clusters)`; `lcn == -1`
/// marks a sparse run.
pub fn parse_runs(data: &[u8]) -> Option<Vec<(i64, u64)>> {
    let mut out = Vec::new();
    let mut p = 0;
    let mut lcn: i64 = 0;
    while let Some(&h) = data.get(p) {
        if h == 0 {
            break;
        }
        p += 1;
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 || p + ls + os > data.len() {
            return None;
        }
        let len = (0..ls).fold(0u64, |acc, k| acc | (data[p + k] as u64) << (8 * k));
        p += ls;
        if os == 0 {
            out.push((-1, len));
            continue;
        }
        let mut delta = (0..os).fold(0i64, |acc, k| acc | (data[p + k] as i64) << (8 * k));
        if os < 8 && data[p + os - 1] & 0x80 != 0 {
            delta |= -1i64 << (8 * os);
        }
        p += os;
        lcn = lcn.checked_add(delta)?;
        if lcn < 0 {
            return None;
        }
        out.push((lcn, len));
    }
    Some(out)
}

/// Run list of the unnamed $DATA attribute of $MFT (record 0).
pub(crate) fn mft_runs(rec0: &[u8]) -> Option<Vec<(i64, u64)>> {
    attrs(rec0)
        .find(|(ty, a)| *ty == AT_DATA && a.get(8) == Some(&1) && a.get(9) == Some(&0))
        .and_then(|(_, a)| a.get(u16_at(a, 0x20) as usize..))
        .and_then(parse_runs)
}

/// `$MFT`'s `$BITMAP` (bit n set = record n in use). `None` (e.g. stored in an
/// attribute list) just means "read every record".
pub(crate) fn mft_bitmap(vol: &File, rec0: &[u8], bpc: u64) -> Option<Vec<u8>> {
    let (_, a) = attrs(rec0).find(|(ty, a)| *ty == AT_BITMAP && a.get(9) == Some(&0))?;
    if a.get(8) == Some(&0) {
        return resident_value(a).map(<[u8]>::to_vec);
    }
    let size = u64_at(a, 0x30);
    let runs = a.get(u16_at(a, 0x20) as usize..).and_then(parse_runs)?;
    let total = runs
        .iter()
        .try_fold(0u64, |t, r| t.checked_add(r.1.checked_mul(bpc)?))?;
    if size == 0 || size > total || total > MAX_BITMAP {
        return None;
    }
    let mut buf = AlignedBuf::new(total as usize);
    let mut off = 0usize;
    for (lcn, clusters) in runs {
        let n = (clusters * bpc) as usize;
        let dst = &mut buf[off..off + n];
        if lcn < 0 {
            dst.fill(0);
        } else {
            read_at(vol, lcn as u64 * bpc, dst).ok()?;
        }
        off += n;
    }
    Some(buf[..size as usize].to_vec())
}

/// Any record overlapping MFT bytes `[abs, abs + len)` in use? Bits past the
/// end of the bitmap count as in use (never skip what we cannot vouch for).
fn any_in_use(bm: &[u8], abs: u64, len: u64, frs: u64) -> bool {
    let (first, end) = (abs / frs, (abs + len).div_ceil(frs));
    (first..end).any(|r| {
        bm.get((r / 8) as usize)
            .is_none_or(|b| (b >> (r % 8)) & 1 != 0)
    })
}

/// Split `len` bytes starting at MFT byte `abs` into maximal `(start, end,
/// in_use)` spans (relative offsets) of GRAIN-sized steps.
fn plan_spans(bm: &[u8], abs: u64, len: usize, frs: u64) -> Vec<(usize, usize, bool)> {
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    let mut p = 0;
    while p < len {
        let q = (p + GRAIN).min(len);
        let used = any_in_use(bm, abs + p as u64, (q - p) as u64, frs);
        match out.last_mut() {
            Some(last) if last.2 == used => last.1 = q,
            _ => out.push((p, q, used)),
        }
        p = q;
    }
    out
}

/// Read MFT bytes into `dst` from disk offset `disk`, zero-filling (instead
/// of reading) spans that hold no record in use. Returns bytes skipped.
pub(crate) fn read_used(
    vol: &File,
    bm: Option<&[u8]>,
    abs: u64,
    frs: u64,
    disk: u64,
    dst: &mut [u8],
) -> io::Result<u64> {
    let Some(bm) = bm else {
        read_at(vol, disk, dst)?;
        return Ok(0);
    };
    let mut skipped = 0;
    for (a, b, used) in plan_spans(bm, abs, dst.len(), frs) {
        if used {
            read_at(vol, disk + a as u64, &mut dst[a..b])?;
        } else {
            dst[a..b].fill(0); // zeroed => no "FILE" magic => ignored
            skipped += (b - a) as u64;
        }
    }
    Ok(skipped)
}

// ---- aligned I/O buffers -------------------------------------------------------------

/// Heap buffer aligned for unbuffered volume reads.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: uniquely owned heap allocation, no interior sharing.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub(crate) fn new(len: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len.max(ALIGN), ALIGN).expect("valid layout");
        // SAFETY: non-zero size layout.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf {
            ptr,
            len: layout.size(),
        }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr/len describe our live, initialized (zeroed) allocation.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}
impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` guarantees uniqueness.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: same layout as allocated in `new`.
        unsafe {
            dealloc(
                self.ptr.as_ptr(),
                Layout::from_size_align_unchecked(self.len, ALIGN),
            )
        }
    }
}

pub(crate) fn read_at(f: &File, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

//! `$MFT` scan pipeline: readers, parallel record parsers, one in-order builder.
//!
//! Each reader holds its own volume handle, since a synchronous handle
//! serializes its I/O. Glint uses one reader, because more did not make
//! scans faster. Fix-ups,
//! attribute walking and UTF-16 → UTF-8 conversion run off the reader thread
//! (~55 ns per record, i.e. far faster than any disk delivers records), so the
//! scan runs at disk speed. Chunks are applied strictly in table order, so
//! the result is identical to a single-threaded scan. At most
//! `parsers + readers + 2` chunk buffers exist at any time, so memory stays bounded.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::mpsc::{channel, sync_channel, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use crate::index::Index;
use crate::ntfs::{
    apply_fixup, bad, mft_bitmap, mft_runs, read_at, read_used, AlignedBuf, VolumeInfo, CHUNK,
};

mod build;

use build::{parse_chunk, Builder, Parsed};

/// Record-parsing threads.
const PARSERS: usize = 2;

type Job = (u64, u64, AlignedBuf, usize);

/// Stream `records` records of `frs` bytes and build the entries. `readers`
/// threads call `fill(reader, logical, dst)`, which copies table bytes
/// `[logical, logical + dst.len())` into `dst`.
fn run<F>(records: usize, frs: usize, readers: usize, fill: F) -> io::Result<Builder>
where
    F: Fn(usize, u64, &mut [u8]) -> io::Result<()> + Sync,
{
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    let limit = (records * frs) as u64;
    let chunk = (CHUNK / frs).max(1) * frs;
    let chunks = limit.div_ceil(chunk as u64);
    let parsers = PARSERS;
    let mut builder = Builder::new(records);
    let (work_tx, work_rx) = sync_channel::<Job>(parsers);
    let work_rx = Mutex::new(work_rx);
    let failed = AtomicBool::new(false);
    let (free_tx, free_rx) = channel::<AlignedBuf>();
    let free_rx = Mutex::new(free_rx);
    // Chunk numbers are handed out only *after* a buffer is held, so the
    // lowest unfinished chunk always owns a buffer: the in-order builder can
    // never wait on a chunk that is itself waiting for a buffer.
    let next_chunk = AtomicU64::new(0);
    let fill = &fill;
    std::thread::scope(|s| -> io::Result<()> {
        let (done_tx, done_rx) = channel::<(u64, AlignedBuf, Parsed)>();
        for _ in 0..parsers + readers + 2 {
            let _ = free_tx.send(AlignedBuf::new(chunk));
        }
        let mut reader_threads = Vec::with_capacity(readers);
        for w in 0..readers {
            let (work_tx, free_rx, next_chunk, failed) =
                (work_tx.clone(), &free_rx, &next_chunk, &failed);
            reader_threads.push(s.spawn(move || -> io::Result<()> {
                loop {
                    if failed.load(Relaxed) {
                        return Ok(());
                    }
                    // Wait for a free buffer, but wake periodically so that a
                    // reader which has failed (below) can release the others
                    // even once the pool has run dry; drop the lock between
                    // waits so every reader gets to look.
                    let mut buf = loop {
                        let got = match free_rx.lock() {
                            Ok(guard) => guard.recv_timeout(Duration::from_millis(50)),
                            Err(_) => return Ok(()),
                        };
                        match got {
                            Ok(buf) => break buf,
                            Err(RecvTimeoutError::Disconnected) => return Ok(()),
                            Err(RecvTimeoutError::Timeout) if failed.load(Relaxed) => return Ok(()),
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    };
                    let seq = next_chunk.fetch_add(1, Relaxed);
                    if seq >= chunks || failed.load(Relaxed) {
                        return Ok(());
                    }
                    let logical = seq * chunk as u64;
                    let n = (limit - logical).min(chunk as u64) as usize;
                    // A reader must never leave its chunk unsent while the others
                    // are live, or the in-order builder would wait on it forever.
                    // On any error OR panic, record the failure (which wakes every
                    // other reader) and carry the error out through `join`.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        fill(w, logical, &mut buf[..n])
                    }));
                    match r {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            failed.store(true, Relaxed);
                            return Err(e);
                        }
                        Err(_) => {
                            failed.store(true, Relaxed);
                            return Err(bad("MFT reader thread panicked"));
                        }
                    }
                    if work_tx.send((seq, logical / frs as u64, buf, n)).is_err() {
                        return Ok(());
                    }
                }
            }));
        }
        drop(work_tx);
        for _ in 0..parsers {
            let (rx, done, failed) = (&work_rx, done_tx.clone(), &failed);
            s.spawn(move || loop {
                let job = match rx.lock() {
                    Ok(guard) => guard.recv(),
                    Err(_) => break,
                };
                let Ok((seq, first, mut buf, n)) = job else {
                    break;
                };
                // A parser must always answer for its chunk, or the in-order
                // builder would wait forever: turn a panic into a failed scan.
                let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse_chunk(first, &mut buf[..n], frs, records)
                }));
                let parsed = parsed.unwrap_or_else(|_| {
                    failed.store(true, Relaxed);
                    Parsed::default()
                });
                if done.send((seq, buf, parsed)).is_err() {
                    break;
                }
            });
        }
        drop(done_tx);
        let (mut next, mut pending) = (0u64, BTreeMap::new());
        for (seq, buf, parsed) in done_rx {
            pending.insert(seq, (buf, parsed));
            while let Some((buf, parsed)) = pending.remove(&next) {
                builder.apply(&parsed);
                let _ = free_tx.send(buf);
                next += 1;
            }
        }
        drop(free_tx);
        for r in reader_threads {
            r.join()
                .unwrap_or_else(|_| Err(bad("MFT reader thread panicked")))?;
        }
        if next != chunks || failed.load(Relaxed) {
            return Err(bad("MFT parser failed"));
        }
        Ok(())
    })?;
    Ok(builder)
}

/// Copies `$MFT` bytes from the volume by following its run list.
struct RunReader {
    /// (first logical byte, first disk byte or -1 if sparse, bytes)
    runs: Vec<(u64, i64, u64)>,
    bitmap: Option<Vec<u8>>,
    frs: u64,
}

impl RunReader {
    fn fill(&self, vol: &File, mut logical: u64, mut dst: &mut [u8]) -> io::Result<()> {
        while !dst.is_empty() {
            let k = self.runs.partition_point(|r| r.0 + r.2 <= logical);
            // $MFT extents continue in an attribute list we don't follow:
            // fail so the caller falls back to a complete folder walk.
            let &(start, disk, len) = self
                .runs
                .get(k)
                .ok_or_else(|| bad("$MFT run list does not cover the whole table"))?;
            let off = logical - start;
            let n = (len - off).min(dst.len() as u64) as usize;
            let (head, tail) = std::mem::take(&mut dst).split_at_mut(n);
            if disk < 0 {
                head.fill(0);
            } else {
                read_used(
                    vol,
                    self.bitmap.as_deref(),
                    logical,
                    self.frs,
                    disk as u64 + off,
                    head,
                )?;
            }
            dst = tail;
            logical += n as u64;
        }
        Ok(())
    }
}

/// Scan one NTFS volume's MFT. `vols` are elevated `\\.\X:` handles to the
/// same volume, one per concurrent reader (at least one).
pub fn scan(letter: u8, vols: &[File], info: &VolumeInfo) -> io::Result<Index> {
    let vol = vols.first().ok_or_else(|| bad("no volume handle"))?;
    let frs = info.frs_size as usize;
    let bpc = info.bytes_per_cluster as u64;
    let mut rec0 = AlignedBuf::new(frs.next_multiple_of(info.bytes_per_sector as usize));
    read_at(vol, info.mft_start_lcn * bpc, &mut rec0)?;
    if !apply_fixup(&mut rec0[..frs]) {
        return Err(bad("$MFT record 0 is damaged"));
    }
    let raw_runs = mft_runs(&rec0[..frs]).ok_or_else(|| bad("cannot decode $MFT run list"))?;
    let mut runs = Vec::with_capacity(raw_runs.len());
    let mut logical = 0u64;
    for (lcn, clusters) in raw_runs {
        let len = clusters
            .checked_mul(bpc)
            .ok_or_else(|| bad("$MFT run too long"))?;
        let disk = if lcn < 0 {
            -1
        } else {
            (lcn as u64)
                .checked_mul(bpc)
                .ok_or_else(|| bad("bad $MFT run"))? as i64
        };
        runs.push((logical, disk, len));
        logical = logical
            .checked_add(len)
            .ok_or_else(|| bad("$MFT run list too long"))?;
    }
    let reader = RunReader {
        runs,
        bitmap: mft_bitmap(vol, &rec0[..frs], bpc),
        frs: frs as u64,
    };
    let records = (info.mft_valid_len / frs as u64) as usize;
    let b = run(records, frs, vols.len(), |w, at, dst| {
        reader.fill(&vols[w], at, dst)
    })?;
    b.finish(letter, info.serial)
}

/// Index an offline copy of a `$MFT` (records back to back, as exported by
/// forensic tools). Entries render as `letter:\...`; the result is not live.
pub fn scan_image(path: &Path, letter: u8) -> io::Result<Index> {
    let f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = [0u8; 0x20];
    read_at(&f, 0, &mut head)?;
    if head[..4] != *b"FILE" {
        return Err(bad("not an $MFT image (record 0 has no FILE signature)"));
    }
    let frs = u32::from_le_bytes([head[0x1C], head[0x1D], head[0x1E], head[0x1F]]) as usize;
    if !frs.is_power_of_two() || !(1024..=4096).contains(&frs) {
        return Err(bad("not an $MFT image (unexpected record size)"));
    }
    let records = usize::try_from(len / frs as u64).map_err(|_| bad("image too large"))?;
    let mut ix = run(records, frs, 1, |_, at, dst| read_at(&f, at, dst))?.finish(letter, 0)?;
    for v in &mut ix.volumes {
        v.live = false;
        v.slot = Vec::new();
        v.links = Vec::new();
        v.streams = Vec::new();
    }
    Ok(ix)
}

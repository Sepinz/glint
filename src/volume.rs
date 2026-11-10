//! Thin, safe wrappers over the Win32 calls the indexer needs.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FileIdType, FileStreamInfo, FindClose, FindFirstFileNameW, FindNextFileNameW, GetDriveTypeW,
    GetFileAttributesExW, GetFileExInfoStandard, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetLogicalDrives,
    GetVolumeInformationW, OpenFileById, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    WIN32_FILE_ATTRIBUTE_DATA,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact, SetProcessWorkingSetSizeEx};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, ProcessMemoryPriority, SetProcessInformation,
    SetThreadPriority, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_NORMAL,
    MEMORY_PRIORITY_VERY_LOW, THREAD_MODE_BACKGROUND_BEGIN,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// FILETIME (100 ns since 1601) -> Unix seconds, clamped into `u32`.
pub fn ft_to_unix(ft: u64) -> u32 {
    if ft <= FILETIME_UNIX_EPOCH {
        return 0;
    }
    ((ft - FILETIME_UNIX_EPOCH) / 10_000_000).min(u32::MAX as u64) as u32
}

pub fn unix_to_ft(secs: u32) -> u64 {
    secs as u64 * 10_000_000 + FILETIME_UNIX_EPOCH
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drive {
    pub letter: u8,
    pub ntfs: bool,
    /// USB stick, SD card, ...: indexed when present, never monitored live
    /// (an open volume handle would block "Safely Remove").
    pub removable: bool,
    /// Volume serial number (for NTFS, the low half of its 64-bit serial):
    /// tells a different volume behind the same letter apart.
    pub serial: u32,
}

/// Bit `n` set: drive letter `'A' + n` exists.
pub fn drive_mask() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetLogicalDrives() }
}

/// Local drives to index: fixed ones, and removable ones with media in
/// (no network, optical or RAM drives).
pub fn drives() -> Vec<Drive> {
    let mask = drive_mask();
    (0..26u8)
        .filter(|b| mask & (1 << b) != 0)
        .filter_map(|b| {
            let letter = b'A' + b;
            let root = wide(&format!("{}:\\", letter as char));
            // SAFETY: `root` is a valid NUL-terminated wide string.
            let removable = match unsafe { GetDriveTypeW(root.as_ptr()) } {
                DRIVE_FIXED => false,
                DRIVE_REMOVABLE => true,
                _ => return None,
            };
            let mut fs = [0u16; 32];
            let mut serial = 0u32;
            // SAFETY: all out-pointers are either null or point to live buffers
            // whose length is passed alongside.
            let ok = unsafe {
                GetVolumeInformationW(
                    root.as_ptr(),
                    null_mut(),
                    0,
                    &mut serial,
                    null_mut(),
                    null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            if ok == 0 {
                return None; // e.g. a card reader without a card
            }
            let end = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
            Some(Drive {
                letter,
                ntfs: String::from_utf16_lossy(&fs[..end]) == "NTFS",
                removable,
                serial,
            })
        })
        .collect()
}

pub fn is_admin() -> bool {
    // SAFETY: no arguments.
    unsafe { IsUserAnAdmin() != 0 }
}

/// Open `\\.\X:` for raw reads and FSCTLs (requires elevation).
pub fn open_volume(letter: u8) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(format!(r"\\.\{}:", letter as char))
}

/// Like [`open_volume`], but reads bypass the cache manager and go straight
/// to the device as large DMA transfers (offsets, lengths and buffer
/// addresses must be sector-aligned).
pub fn open_volume_unbuffered(letter: u8) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(format!(r"\\.\{}:", letter as char))
}

/// Does the volume have an active USN journal (i.e. can it be kept live)?
pub fn journal_active(letter: u8) -> bool {
    open_volume(letter).is_ok_and(|f| crate::usn::query(&f).is_ok())
}

/// `DeviceIoControl` returning the number of output bytes.
pub fn ioctl(f: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut ret = 0u32;
    let inp = if input.is_empty() {
        null()
    } else {
        input.as_ptr().cast()
    };
    // SAFETY: the handle is owned by `f` and live; buffer pointers/lengths
    // describe valid memory; no OVERLAPPED (synchronous handle).
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle() as HANDLE,
            code,
            inp,
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut ret,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

// ---- hard links -----------------------------------------------------------------------

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NOT_FOUND: i32 = 1168;

/// Open a file or folder for attribute queries only.
fn open_meta(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Open the file whose 64-bit reference number (with sequence) is `frn`.
fn open_by_id(hint: &File, frn: u64) -> io::Result<File> {
    let id = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
    };
    // SAFETY: `hint` is a live handle on the volume; `id` outlives the call.
    let h = unsafe {
        OpenFileById(
            hint.as_raw_handle() as HANDLE,
            &id,
            FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle we now own exclusively.
    Ok(unsafe { File::from_raw_handle(h as _) })
}

/// `\\?\C:\dir\name` of an open file.
fn final_path(f: &File) -> io::Result<Vec<u16>> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the buffer length passed is the buffer's own.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                f.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                0,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            buf.push(0);
            return Ok(buf);
        }
        buf.resize(n + 1, 0); // too small: `n` is the size needed
    }
}

/// Calls `FindFirstFileNameW` / `FindNextFileNameW` (one per link) with a
/// growing buffer.
fn next_link(
    mut call: impl FnMut(&mut u32, *mut u16) -> bool,
    buf: &mut Vec<u16>,
) -> io::Result<Option<String>> {
    loop {
        let mut len = buf.len() as u32;
        if call(&mut len, buf.as_mut_ptr()) {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            return Ok(Some(String::from_utf16_lossy(&buf[..end])));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(ERROR_MORE_DATA) => buf.resize(len as usize + 1, 0),
            Some(ERROR_HANDLE_EOF) => return Ok(None),
            _ => return Err(e),
        }
    }
}

/// Every link of the file at `path` (NUL-terminated), volume-relative
/// (`\dir\name`).
fn link_paths(path: &[u16]) -> io::Result<Vec<String>> {
    let mut buf = vec![0u16; 512];
    let mut find = INVALID_HANDLE_VALUE;
    let first = next_link(
        |len, out| {
            // SAFETY: `path` is NUL-terminated; `out`/`len` describe `buf`.
            find = unsafe { FindFirstFileNameW(path.as_ptr(), 0, len, out) };
            find != INVALID_HANDLE_VALUE
        },
        &mut buf,
    )?;
    let Some(first) = first else {
        return Ok(Vec::new());
    };
    let mut out = vec![first];
    let rest = (|| -> io::Result<()> {
        // SAFETY: `find` is the live search handle; `out`/`len` describe `buf`.
        while let Some(l) = next_link(
            |len, o| unsafe { FindNextFileNameW(find, len, o) != 0 },
            &mut buf,
        )? {
            out.push(l);
        }
        Ok(())
    })();
    if find != INVALID_HANDLE_VALUE {
        // SAFETY: closes the search handle opened above, exactly once.
        unsafe { FindClose(find) };
    }
    rest.map(|()| out)
}

/// 64-bit file reference number (with sequence) of an open file or folder.
fn file_id(f: &File) -> io::Result<u64> {
    // SAFETY: plain-old-data out struct, zero is a valid bit pattern.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: live handle; `info` is the struct the call fills.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64)
}

/// The file no longer exists (or its record now holds another file).
fn is_gone(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_INVALID_PARAMETER | ERROR_NOT_FOUND
        )
    )
}

/// Every hard link of file `frn` on drive `letter`, as (parent folder
/// reference number, name). Empty if the file is gone.
pub fn hard_links(letter: u8, frn: u64) -> io::Result<Vec<(u64, String)>> {
    let drive = letter as char;
    let root = open_meta(&format!("{drive}:\\"))?;
    let file = match open_by_id(&root, frn) {
        Err(e) if is_gone(&e) => return Ok(Vec::new()),
        r => r?,
    };
    let mut out = Vec::new();
    for link in link_paths(&final_path(&file)?)? {
        let (dir, name) = link.rsplit_once('\\').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "link path without folder")
        })?;
        // Extended-length form: a link's folder may sit deeper than MAX_PATH.
        let parent = file_id(&open_meta(&format!(r"\\?\{drive}:{dir}\"))?)?;
        out.push((parent, name.to_owned()));
    }
    Ok(out)
}

/// Every alternate data stream of file `frn` on drive `letter`, as (name,
/// size). Empty if the file is gone.
pub fn streams(letter: u8, frn: u64) -> io::Result<Vec<(String, u64)>> {
    let root = open_meta(&format!("{}:\\", letter as char))?;
    match open_by_id(&root, frn) {
        Err(e) if is_gone(&e) => Ok(Vec::new()),
        f => file_streams(&f?),
    }
}

/// `FILE_STREAM_INFO` header: next offset, name bytes, size, allocation.
const STREAM_INFO_HEADER: usize = 24;

fn file_streams(f: &File) -> io::Result<Vec<(String, u64)>> {
    // u64 elements: the entries need 8-byte alignment.
    let mut buf = vec![0u64; 512];
    loop {
        let bytes = (buf.len() * 8) as u32;
        // SAFETY: live handle; the pointer is `buf`'s (u64, so 8-byte aligned
        // as FILE_STREAM_INFO needs) and `bytes` its exact length in bytes
        // (at most 8 MiB): the call writes no further.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                f.as_raw_handle() as HANDLE,
                FileStreamInfo,
                buf.as_mut_ptr().cast(),
                bytes,
            )
        };
        if ok != 0 {
            return Ok(parse_streams(bytemuck::cast_slice(&buf)));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(ERROR_MORE_DATA) if buf.len() < 1 << 20 => buf.resize(buf.len() * 2, 0),
            Some(ERROR_HANDLE_EOF) => return Ok(Vec::new()), // no streams at all
            _ => return Err(e),
        }
    }
}

/// Named `$DATA` streams in a `FILE_STREAM_INFO` list (`:name:$DATA`); the
/// unnamed one (`::$DATA`) is the file's contents.
fn parse_streams(b: &[u8]) -> Vec<(String, u64)> {
    let le32 = |at: usize| {
        b.get(at..at + 4)
            .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let (next, len) = (le32(at) as usize, le32(at + 4) as usize);
        let size = b
            .get(at + 8..at + 16)
            .map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap_or_default()));
        let start = at + STREAM_INFO_HEADER;
        let Some(raw) = b.get(start..start + len) else {
            break;
        };
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let full = String::from_utf16_lossy(&units);
        if let Some(name) = full
            .strip_prefix(':')
            .and_then(|n| n.strip_suffix(":$DATA"))
        {
            if !name.is_empty() && name != "WofCompressedData" {
                out.push((name.to_owned(), size));
            }
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

/// Size and mtime without opening the file. `None` if it is gone.
pub fn stat(path: &str) -> Option<(u64, u32, bool)> {
    let p = wide(&format!(r"\\?\{path}"));
    // SAFETY: plain-old-data out struct, zero is a valid bit pattern.
    let mut d: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    // SAFETY: `p` is NUL-terminated; `d` matches GetFileExInfoStandard.
    let ok = unsafe {
        GetFileAttributesExW(
            p.as_ptr(),
            GetFileExInfoStandard,
            (&mut d as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        )
    };
    if ok == 0 {
        return None;
    }
    let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
    let ft =
        ((d.ftLastWriteTime.dwHighDateTime as u64) << 32) | d.ftLastWriteTime.dwLowDateTime as u64;
    Some((
        size,
        ft_to_unix(ft),
        d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
    ))
}

/// Lower CPU *and* I/O priority of the calling thread.
pub fn set_background_priority() {
    // SAFETY: pseudo-handle for the current thread.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

/// Hand everything we can back to the OS: coalesce the heap, then drop the
/// whole working set (pages fault back in on demand). Call after freeing the
/// index; with nothing left to keep, this leaves the process at ~1-2 MB.
pub fn release_memory() {
    // SAFETY: process heap / pseudo-handle; (usize::MAX, usize::MAX) is the
    // documented "remove as many pages as possible" request.
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
        SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}

/// While idle, our pages are the first the memory manager reclaims.
pub fn set_memory_priority(idle: bool) {
    let info = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: if idle {
            MEMORY_PRIORITY_VERY_LOW
        } else {
            MEMORY_PRIORITY_NORMAL
        },
    };
    // SAFETY: pointer and size describe `info`, which outlives the call.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
            std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        );
    }
}

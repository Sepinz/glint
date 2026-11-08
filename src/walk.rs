//! Directory-walk fallback (unelevated runs, FAT/exFAT/ReFS volumes).
//!
//! Parallel `FindFirstFileExW` with `FindExInfoBasic` (skips 8.3 names) and
//! `FIND_FIRST_EX_LARGE_FETCH` (bigger kernel batches). Each directory is
//! listed without holding any lock; its children are inserted under one
//! short lock, then subdirectories fan out as rayon tasks.

use std::ffi::c_void;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH,
    WIN32_FIND_DATAW,
};

use crate::index::{Index, Volume, F_DIR, F_ROOT, NONE};
use crate::ntfs::attr_flags;
use crate::volume::{ft_to_unix, set_background_priority, wide};

struct Child {
    name: String,
    attrs: u32,
    tag: u32,
    size: u64,
    mtime: u32,
}

const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// Never follow junctions/symlinks (cycles, duplicates), but do descend into
/// other reparse points such as OneDrive/cloud placeholders, which are real folders.
fn is_link(attrs: u32, tag: u32) -> bool {
    attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && matches!(tag, IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK)
}

fn list_dir(dir: &str, out: &mut Vec<Child>) {
    let pattern = wide(&format!("{dir}*"));
    // SAFETY: POD out struct.
    let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated pattern; `fd` matches FindExInfoBasic.
    let h = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            (&mut fd as *mut WIN32_FIND_DATAW).cast::<c_void>(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    loop {
        let end = fd
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fd.cFileName.len());
        let n = &fd.cFileName[..end];
        let dots = n == [b'.' as u16] || n == [b'.' as u16, b'.' as u16];
        if !dots {
            let ft = ((fd.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | fd.ftLastWriteTime.dwLowDateTime as u64;
            out.push(Child {
                name: String::from_utf16_lossy(n),
                attrs: fd.dwFileAttributes,
                tag: fd.dwReserved0, // reparse tag when REPARSE_POINT is set
                size: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
                mtime: ft_to_unix(ft),
            });
        }
        // SAFETY: `h` is a live find handle, `fd` a valid out struct.
        if unsafe { FindNextFileW(h, &mut fd) } == 0 {
            break;
        }
    }
    // SAFETY: `h` came from FindFirstFileExW and is closed exactly once.
    unsafe { FindClose(h) };
}

fn walk<'s>(s: &rayon::Scope<'s>, ix: &'s Mutex<Index>, dir: String, parent: u32) {
    let mut kids = Vec::new();
    list_dir(&dir, &mut kids);
    let mut subdirs = Vec::new();
    {
        let mut g = ix.lock().unwrap_or_else(|e| e.into_inner());
        for k in kids {
            let is_dir = k.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            let flags = if is_dir { F_DIR } else { 0 } | attr_flags(k.attrs);
            let i = g.push(
                &k.name,
                parent,
                flags,
                if is_dir { 0 } else { k.size },
                k.mtime,
            );
            if is_dir && !is_link(k.attrs, k.tag) {
                subdirs.push((i, k.name));
            }
        }
    }
    for (i, name) in subdirs {
        let path = format!("{dir}{name}\\");
        s.spawn(move |s| walk(s, ix, path, i));
    }
}

/// Walk `X:\` completely. `background` lowers CPU and I/O priority.
pub fn scan(letter: u8, background: bool) -> Index {
    scan_path(
        &format!(r"\\?\{}:\", letter as char),
        &format!("{}:", letter as char),
        letter,
        background,
    )
}

/// Walk an arbitrary directory (`dir` must end with a backslash).
pub(crate) fn scan_path(dir: &str, root_name: &str, letter: u8, background: bool) -> Index {
    let ix = Mutex::new(Index::default());
    let root =
        ix.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(root_name, NONE, F_DIR | F_ROOT, 0, 0);
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let threads = if background {
        2
    } else {
        (cores * 2).clamp(4, 32)
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("glint-walk-{i}"))
        .start_handler(move |_| {
            if background {
                set_background_priority();
            }
        })
        .build();
    let run = || rayon::scope(|s| walk(s, &ix, dir.to_string(), root));
    match pool {
        Ok(p) => p.install(run),
        Err(_) => run(),
    }
    let mut ix = ix.into_inner().unwrap_or_else(|e| e.into_inner());
    ix.volumes.push(Volume {
        letter,
        live: false,
        root,
        ..Default::default()
    });
    ix
}

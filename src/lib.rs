//! Glint: instant file-name search for Windows.
//!
//! Pipeline:
//! * [`ntfs`]   parses raw `$MFT` records; [`mft`] streams them (sequential I/O, parallel parsing)
//! * [`walk`]   directory-walk fallback for non-NTFS volumes / unelevated runs
//! * [`index`]  compact struct-of-arrays index, pre-sorted by name
//! * [`usn`]    USN change journal: catch-up at boot and live monitoring
//! * [`store`]  flat binary snapshot for ~instant cold start
//! * [`query`] / [`search`]  query language and parallel matching
//! * [`engine`] orchestration shared by the GUI and CLI

pub mod engine;
pub mod fmt;
pub mod index;
pub mod mft;
pub mod ntfs;
pub mod query;
pub mod search;
pub mod store;
mod tree;
pub mod usn;
pub mod volume;
pub mod walk;
pub mod when;

use std::path::PathBuf;

/// `%GLINT_DB%` if set, else `%LOCALAPPDATA%\Glint\index.db` (temp dir fallback).
pub fn default_db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GLINT_DB").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Glint").join("index.db")
}

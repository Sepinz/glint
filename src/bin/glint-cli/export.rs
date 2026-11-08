//! Result output: plain paths, or CSV / TSV / JSON records for export.
//!
//! Rows are formatted in parallel, a bounded batch at a time, and written in
//! order, so exporting millions of entries is fast and needs little memory.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::{self, Write};

use rayon::prelude::*;

use glint::fmt::{group, DateCache};
use glint::index::{Index, F_HIDDEN, F_READONLY, F_REPARSE, F_SYSTEM};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Plain,
    Csv,
    Tsv,
    Json,
}

/// Rows formatted per task, and tasks per written batch.
const CHUNK: usize = 16 << 10;
const BATCH: usize = 16;

fn attrs(f: u8, out: &mut String) {
    for (bit, c) in [
        (F_READONLY, 'R'),
        (F_HIDDEN, 'H'),
        (F_SYSTEM, 'S'),
        (F_REPARSE, 'L'),
    ] {
        if f & bit != 0 {
            out.push(c);
        }
    }
}

/// RFC 4180 field: quoted when it holds a separator, quote or line break.
fn csv_field(s: &str, out: &mut String) {
    if s.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

thread_local! {
    /// One cache per worker thread, reused across chunks (mtimes are random in name order).
    static DATES: RefCell<DateCache> = RefCell::new(DateCache::default());
}

/// Line-oriented formats: replace control characters (possible in NTFS
/// POSIX names, e.g. from an untrusted `--mft` image) so they can neither
/// split a row nor emit terminal escape sequences.
fn push_clean(s: &str, out: &mut String) {
    if s.chars().any(char::is_control) {
        out.extend(
            s.chars()
                .map(|c| if c.is_control() { '\u{FFFD}' } else { c }),
        );
    } else {
        out.push_str(s);
    }
}

struct Row<'a> {
    ix: &'a Index,
    format: Format,
    long: bool,
    path: String,
    dates: &'a mut DateCache,
}

impl Row<'_> {
    fn format(&mut self, i: u32, first: bool, out: &mut String) {
        let ix = self.ix;
        ix.path_into(i, &mut self.path);
        let kind = if ix.is_dir(i) { "folder" } else { "file" };
        let path = &self.path;
        match self.format {
            Format::Plain if self.long => {
                let size = format!("{} KB", group(ix.size(i).div_ceil(1024)));
                let mut d = String::new();
                self.dates.write(ix.mtime(i), &mut d);
                let _ = write!(out, "{size:>14}  {d:16}  ");
                push_clean(path, out);
                out.push('\n');
            }
            Format::Plain => {
                push_clean(path, out);
                out.push('\n');
            }
            Format::Csv => {
                csv_field(path, out);
                out.push(',');
                csv_field(ix.name(i), out);
                let _ = write!(out, ",{},", ix.size(i));
                self.dates.write(ix.mtime(i), out);
                out.push(',');
                attrs(ix.flags(i), out);
                let _ = write!(out, ",{kind}\r\n");
            }
            Format::Tsv => {
                push_clean(path, out);
                out.push('\t');
                push_clean(ix.name(i), out);
                let _ = write!(out, "\t{}\t", ix.size(i));
                self.dates.write(ix.mtime(i), out);
                out.push('\t');
                attrs(ix.flags(i), out);
                let _ = writeln!(out, "\t{kind}");
            }
            Format::Json => {
                out.push_str(if first {
                    "\n{\"path\":"
                } else {
                    ",\n{\"path\":"
                });
                json_str(path, out);
                out.push_str(",\"name\":");
                json_str(ix.name(i), out);
                let _ = write!(
                    out,
                    ",\"size\":{},\"mtime\":{},\"attributes\":\"",
                    ix.size(i),
                    ix.mtime(i)
                );
                attrs(ix.flags(i), out);
                let _ = write!(out, "\",\"type\":\"{kind}\"}}");
            }
        }
    }
}

/// Write `rows` of `ix` in `format`. `long` adds size and date to plain output.
pub fn write(
    w: &mut impl Write,
    ix: &Index,
    rows: impl Iterator<Item = u32>,
    format: Format,
    long: bool,
) -> io::Result<()> {
    match format {
        Format::Csv => w.write_all(b"path,name,size,modified,attributes,type\r\n")?,
        Format::Tsv => w.write_all(b"path\tname\tsize\tmodified\tattributes\ttype\n")?,
        Format::Json => w.write_all(b"[")?,
        Format::Plain => {}
    }
    let mut rows = rows.peekable();
    let mut start = 0usize;
    let mut batch: Vec<u32> = Vec::with_capacity(CHUNK * BATCH);
    while rows.peek().is_some() {
        batch.clear();
        batch.extend(rows.by_ref().take(CHUNK * BATCH));
        let texts: Vec<String> = batch
            .par_chunks(CHUNK)
            .enumerate()
            .map(|(c, ids)| {
                DATES.with_borrow_mut(|dates| {
                    let mut row = Row {
                        ix,
                        format,
                        long,
                        path: String::new(),
                        dates,
                    };
                    let mut out = String::with_capacity(ids.len() * 96);
                    for (k, &i) in ids.iter().enumerate() {
                        row.format(i, start + c * CHUNK + k == 0, &mut out);
                    }
                    out
                })
            })
            .collect();
        for t in texts {
            w.write_all(t.as_bytes())?;
        }
        start += batch.len();
    }
    if format == Format::Json {
        w.write_all(b"\n]\n")?;
    }
    Ok(())
}

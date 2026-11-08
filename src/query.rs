//! Search language.
//!
//! ```text
//! foo bar          both substrings (AND), case-insensitive
//! "program files"  phrase with spaces
//! !tmp             NOT
//! jpg|png          either (per term)
//! *.rs  report?.*  wildcards: match the whole name
//! windows\fonts    a term with '\' or '/' matches the full path
//! ext:jpg;png      extension list (files only)
//! size:>10mb       size filter: < <= = >= >, units b/kb/mb/gb/tb
//!                  (folders: total size of everything inside)
//! file:  folder:   type filters (may prefix a term: folder:src)
//! regex:^a.*z$     regular expression on the name
//! dm:2024-05       modified: YYYY[-MM[-DD]], today, yesterday, 7d/12h/2w,
//!                  > >= < <= and ranges (dm:2024-01..2024-03)
//! attrib:hs        attributes, all of: h(idden) s(ystem) r(ead-only) l(ink/reparse)
//! depth:<=2        folder depth (C:\x is 1)     len:>100  name length (characters)
//! parent:c:\temp   direct children of a folder
//! ```

use memchr::memmem;

use crate::index::{Index, F_HIDDEN, F_READONLY, F_REPARSE, F_SYSTEM, MAX_DEPTH, NONE};
use crate::when;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

#[derive(Debug)]
enum Pat {
    Sub {
        needle: Vec<u8>,
        ascii: bool,
    },
    Wild {
        bytes: Vec<u8>,
        chars: Vec<char>,
        ascii: bool,
    },
    Regex(regex::Regex),
    Ext(Vec<Vec<u8>>),
    Size(Cmp, u64),
    Files,
    Folders,
    /// All of these attribute flags set.
    Attr(u8),
    /// Modified in `[lo, hi)` (Unix seconds; unknown dates never match).
    Date(u32, u32),
    Depth(Cmp, u32),
    Len(Cmp, u32),
    /// Lowercased full path of the containing folder, and its last component.
    Parent(String, String),
}

#[derive(Debug)]
struct Term {
    alts: Vec<Pat>,
    neg: bool,
    path: bool,
    /// Verdict depends on the name alone: memoized per stored name.
    by_name: bool,
    cost: u8,
}

#[derive(Debug, Default)]
pub struct Query {
    terms: Vec<Term>,
}

/// Per-thread scratch buffers so matching never allocates per entry.
#[derive(Default)]
pub struct Scratch {
    path: String,
    tmp: Tmp,
    /// Per term: (name key, verdict) of the last name tested. Equal names are
    /// adjacent in name order, so this skips most repeated work.
    memo: Vec<(u64, bool)>,
    /// `parent:` verdicts per containing folder (direct-mapped cache).
    parents: Vec<(u32, bool)>,
}

const PARENT_CACHE: usize = 1024;

const NO_KEY: u64 = u64::MAX;

#[derive(Default)]
struct Tmp {
    lower: String,
    /// `parent:` path buffer (must not clobber the entry's own path).
    folder: String,
    chars: Vec<char>,
}

/// True for queries made only of plain substring terms; extending such a
/// query can only narrow its results (enables incremental search).
pub fn is_plain(text: &str) -> bool {
    !text.contains(['!', '|', '"', ':', '*', '?', '\\', '/', '<', '>'])
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let head = s.get(..kw.len())?;
    head.eq_ignore_ascii_case(kw).then(|| &s[kw.len()..])
}

fn text_pat(s: &str) -> Pat {
    let lower: String = s
        .replace('/', "\\")
        .chars()
        .flat_map(char::to_lowercase)
        .collect();
    if lower.contains(['*', '?']) {
        Pat::Wild {
            ascii: lower.is_ascii(),
            chars: lower.chars().collect(),
            bytes: lower.into_bytes(),
        }
    } else {
        Pat::Sub {
            ascii: lower.is_ascii(),
            needle: lower.into_bytes(),
        }
    }
}

fn split_cmp(s: &str) -> (Cmp, &str) {
    [
        (">=", Cmp::Ge),
        ("<=", Cmp::Le),
        (">", Cmp::Gt),
        ("<", Cmp::Lt),
        ("=", Cmp::Eq),
    ]
    .iter()
    .find_map(|(p, c)| s.strip_prefix(p).map(|r| (*c, r)))
    .unwrap_or((Cmp::Eq, s))
}

fn parse_count(s: &str, what: &str) -> Result<(Cmp, u32), String> {
    let (cmp, rest) = split_cmp(s);
    let n = rest
        .trim()
        .parse()
        .map_err(|_| format!("invalid {what}: {s}"))?;
    Ok((cmp, n))
}

fn parse_attr(s: &str) -> Result<Pat, String> {
    let mut mask = 0;
    for c in s.chars() {
        mask |= match c.to_ascii_lowercase() {
            'h' => F_HIDDEN,
            's' => F_SYSTEM,
            'r' => F_READONLY,
            'l' => F_REPARSE,
            _ => return Err(format!("unknown attribute '{c}' (use h, s, r, l)")),
        };
    }
    if mask == 0 {
        return Err("attrib: needs letters (h, s, r, l)".into());
    }
    Ok(Pat::Attr(mask))
}

/// `parent:` value as the lowercased path `path_into` would render.
fn parent_pat(s: &str) -> Pat {
    let mut p: String = s
        .replace('/', "\\")
        .chars()
        .flat_map(char::to_lowercase)
        .collect();
    while p.ends_with('\\') && !p.ends_with(":\\") {
        p.pop();
    }
    if p.ends_with(':') {
        p.push('\\');
    }
    let last = p
        .trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or("")
        .to_string();
    Pat::Parent(p, last)
}

fn parse_size(s: &str) -> Result<Pat, String> {
    let (cmp, rest) = split_cmp(s);
    let rest = rest.trim().to_ascii_lowercase();
    let split = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (num, unit) = rest.split_at(split);
    let v: f64 = num.parse().map_err(|_| format!("invalid size: {s}"))?;
    let mul: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return Err(format!("unknown size unit: {unit}")),
    };
    Ok(Pat::Size(cmp, (v * mul as f64) as u64))
}

impl Query {
    pub fn parse(text: &str) -> Result<Query, String> {
        let mut terms = Vec::new();
        let mut push = |alts: Vec<Pat>, neg: bool, path: bool, cost: u8| {
            if !alts.is_empty() {
                let by_name = !path
                    && alts
                        .iter()
                        .all(|p| matches!(p, Pat::Sub { .. } | Pat::Wild { .. } | Pat::Regex(_)));
                terms.push(Term {
                    alts,
                    neg,
                    path,
                    by_name,
                    cost,
                });
            }
        };
        for tok in tokenize(text) {
            let (neg, body) = match tok.strip_prefix('!') {
                Some(b) => (true, b),
                None => (false, tok.as_str()),
            };
            if body.is_empty() {
                continue;
            }
            if let Some(r) = strip_kw(body, "ext:") {
                let exts = r
                    .split([';', '|', ','])
                    .filter(|e| !e.is_empty())
                    .map(|e| e.trim_start_matches('.').to_lowercase().into_bytes())
                    .collect();
                push(vec![Pat::Ext(exts)], neg, false, 0);
            } else if let Some(r) = strip_kw(body, "size:") {
                push(vec![parse_size(r)?], neg, false, 0);
            } else if let Some(r) =
                strip_kw(body, "dm:").or_else(|| strip_kw(body, "datemodified:"))
            {
                let (lo, hi) = when::parse_range(r, &when::Local)?;
                push(vec![Pat::Date(lo, hi)], neg, false, 0);
            } else if let Some(r) = strip_kw(body, "attrib:").or_else(|| strip_kw(body, "attr:")) {
                push(vec![parse_attr(r)?], neg, false, 0);
            } else if let Some(r) = strip_kw(body, "depth:") {
                let (c, n) = parse_count(r, "depth")?;
                push(vec![Pat::Depth(c, n)], neg, false, 2);
            } else if let Some(r) = strip_kw(body, "len:") {
                let (c, n) = parse_count(r, "length")?;
                push(vec![Pat::Len(c, n)], neg, false, 1);
            } else if let Some(r) =
                strip_kw(body, "parent:").or_else(|| strip_kw(body, "infolder:"))
            {
                if r.is_empty() {
                    return Err("parent: needs a folder path".into());
                }
                push(vec![parent_pat(r)], neg, false, 4);
            } else if let Some(r) = strip_kw(body, "regex:") {
                let re = regex::RegexBuilder::new(r)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| format!("invalid regex: {e}"))?;
                push(vec![Pat::Regex(re)], neg, false, 3);
            } else if let Some((filter, r)) = strip_kw(body, "file:")
                .map(|r| (Pat::Files, r))
                .or_else(|| strip_kw(body, "folder:").map(|r| (Pat::Folders, r)))
            {
                if r.is_empty() {
                    push(vec![filter], neg, false, 0);
                } else {
                    push(vec![filter], false, false, 0);
                    push(alts_of(r), neg, has_sep(r), 2);
                }
            } else {
                push(
                    alts_of(body),
                    neg,
                    has_sep(body),
                    if has_sep(body) { 4 } else { 1 },
                );
            }
        }
        terms.sort_by_key(|t| t.cost);
        Ok(Query { terms })
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    #[inline]
    pub fn matches(&self, ix: &Index, i: u32, s: &mut Scratch) -> bool {
        let Scratch {
            path,
            tmp,
            memo,
            parents,
        } = s;
        if memo.len() != self.terms.len() {
            memo.clear();
            memo.resize(self.terms.len(), (NO_KEY, false));
        }
        let mut have_path = false;
        for (t, m) in self.terms.iter().zip(memo.iter_mut()) {
            let hit = if t.by_name {
                let key = ix.name_key(i);
                if m.0 != key {
                    let name = ix.name_bytes(i);
                    *m = (key, t.alts.iter().any(|p| p.test(ix, i, name, tmp)));
                }
                m.1
            } else if t.path {
                if !have_path {
                    ix.path_into(i, path);
                    have_path = true;
                }
                t.alts.iter().any(|p| p.test(ix, i, path.as_bytes(), tmp))
            } else if let [Pat::Parent(want, last)] = t.alts.as_slice() {
                parent_is(ix, i, want, last, tmp, parents)
            } else {
                let name = ix.name_bytes(i);
                t.alts.iter().any(|p| p.test(ix, i, name, tmp))
            };
            if hit == t.neg {
                return false;
            }
        }
        true
    }
}

/// Is `i` directly inside the folder whose lowercased path is `want`?
/// Per-char lowercasing, exactly as query text is lowered (no final-sigma rule).
fn lower_eq(s: &str, lowered: &str) -> bool {
    s.chars().flat_map(char::to_lowercase).eq(lowered.chars())
}

fn parent_is(
    ix: &Index,
    i: u32,
    want: &str,
    last: &str,
    tmp: &mut Tmp,
    cache: &mut Vec<(u32, bool)>,
) -> bool {
    let p = ix.parent(i);
    if p == NONE {
        return false;
    }
    if cache.is_empty() {
        cache.resize(PARENT_CACHE, (NONE, false));
    }
    let slot = &mut cache[p as usize % PARENT_CACHE];
    if slot.0 != p {
        // Cheap reject on the folder's own name before building its path.
        let hit = lower_eq(ix.name(p), last) && {
            ix.path_into(p, &mut tmp.folder);
            lower_eq(&tmp.folder, want)
        };
        *slot = (p, hit);
    }
    slot.1
}

fn depth_of(ix: &Index, i: u32) -> u32 {
    let (mut d, mut cur) = (0u32, ix.parent(i));
    while cur != NONE && (d as usize) < MAX_DEPTH {
        d += 1;
        cur = ix.parent(cur);
    }
    d
}

#[inline]
fn compare<T: Ord>(c: Cmp, a: T, b: T) -> bool {
    match c {
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Eq => a == b,
        Cmp::Ge => a >= b,
        Cmp::Gt => a > b,
    }
}

fn has_sep(s: &str) -> bool {
    s.contains(['\\', '/'])
}

fn alts_of(s: &str) -> Vec<Pat> {
    s.split('|')
        .filter(|a| !a.is_empty())
        .map(text_pat)
        .collect()
}

impl Pat {
    #[inline]
    fn test(&self, ix: &Index, i: u32, hay: &[u8], tmp: &mut Tmp) -> bool {
        match self {
            Pat::Sub {
                needle,
                ascii: true,
            } => contains_ci_ascii(hay, needle),
            Pat::Sub {
                needle,
                ascii: false,
            } => {
                lower_into(hay, &mut tmp.lower);
                memmem::find(tmp.lower.as_bytes(), needle).is_some()
            }
            Pat::Wild {
                bytes, ascii: true, ..
            } if hay.is_ascii() => glob_ascii(bytes, hay),
            Pat::Wild { chars, .. } => {
                tmp.chars.clear();
                tmp.chars.extend(
                    String::from_utf8_lossy(hay)
                        .chars()
                        .flat_map(char::to_lowercase),
                );
                glob_chars(chars, &tmp.chars)
            }
            Pat::Regex(re) => std::str::from_utf8(hay).is_ok_and(|s| re.is_match(s)),
            Pat::Ext(list) => {
                !ix.is_dir(i)
                    && memchr::memrchr(b'.', hay)
                        .map(|d| &hay[d + 1..])
                        .is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
            }
            Pat::Size(c, n) => compare(*c, ix.size(i), *n),
            Pat::Files => !ix.is_dir(i),
            Pat::Folders => ix.is_dir(i),
            Pat::Attr(mask) => ix.flags(i) & mask == *mask,
            Pat::Date(lo, hi) => {
                let t = ix.mtime(i);
                t != 0 && t >= *lo && t < *hi
            }
            Pat::Depth(c, n) => compare(*c, depth_of(ix, i), *n),
            Pat::Len(c, n) => {
                let len = if hay.is_ascii() {
                    hay.len()
                } else {
                    String::from_utf8_lossy(hay).chars().count()
                };
                compare(*c, len as u32, *n)
            }
            Pat::Parent(..) => false, // evaluated in `matches` (needs the scratch cache)
        }
    }
}

fn lower_into(hay: &[u8], out: &mut String) {
    out.clear();
    out.extend(
        String::from_utf8_lossy(hay)
            .chars()
            .flat_map(char::to_lowercase),
    );
}

/// Case-insensitive substring search for an already-lowercased ASCII needle.
/// SIMD `memchr2` jumps between candidate first characters.
#[inline]
pub fn contains_ci_ascii(hay: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if hay.len() < needle.len() {
        return false;
    }
    let last = hay.len() - needle.len();
    let up = first.to_ascii_uppercase();
    let mut pos = 0;
    while pos <= last {
        match memchr::memchr2(first, up, &hay[pos..=last]) {
            None => return false,
            Some(k) => {
                let s = pos + k;
                if hay[s + 1..s + needle.len()].eq_ignore_ascii_case(rest) {
                    return true;
                }
                pos = s + 1;
            }
        }
    }
    false
}

/// Whole-string glob (`*`, `?`) over ASCII, pattern pre-lowercased.
pub fn glob_ascii(pat: &[u8], s: &[u8]) -> bool {
    let plain = |lit: &[u8]| !lit.contains(&b'*') && !lit.contains(&b'?');
    let same = |a: &[u8], b: &[u8]| a.iter().zip(b).all(|(c, p)| c.to_ascii_lowercase() == *p);
    // The common shapes `*.ext` and `name*` need one comparison, no search.
    match pat {
        [b'*', lit @ ..] if plain(lit) => {
            s.len() >= lit.len() && same(&s[s.len() - lit.len()..], lit)
        }
        [lit @ .., b'*'] if plain(lit) => s.len() >= lit.len() && same(&s[..lit.len()], lit),
        _ => glob(pat, s, |p, c| p == c.to_ascii_lowercase(), b'*', b'?'),
    }
}

fn glob_chars(pat: &[char], s: &[char]) -> bool {
    glob(pat, s, |p, c| p == c, '*', '?')
}

fn glob<T: Copy + PartialEq>(
    pat: &[T],
    s: &[T],
    eq: impl Fn(T, T) -> bool,
    star: T,
    one: T,
) -> bool {
    let (mut p, mut i) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while i < s.len() {
        if p < pat.len() && pat[p] == star {
            back = Some((p, i));
            p += 1;
        } else if p < pat.len() && (pat[p] == one || eq(pat[p], s[i])) {
            p += 1;
            i += 1;
        } else if let Some((bp, bi)) = back {
            p = bp + 1;
            i = bi + 1;
            back = Some((bp, bi + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == star)
}

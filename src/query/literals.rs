//! Literal text a regex needs, so most names can skip the regex engine.

use std::cmp::Reverse;

use regex_syntax::hir::{Hir, HirKind};

/// More alternatives than this cost more to test than they save.
const MAX_ALTERNATIVES: usize = 8;

/// Lowercased ASCII strings, at least one of which appears in every match of
/// `pattern`, ignoring ASCII case. Empty when no such text is known.
pub(super) fn required(pattern: &str) -> Vec<Vec<u8>> {
    regex_syntax::parse(pattern)
        .ok()
        .and_then(|h| any_of(&h))
        .unwrap_or_default()
}

fn any_of(h: &Hir) -> Option<Vec<Vec<u8>>> {
    match h.kind() {
        HirKind::Literal(l) if !l.0.is_empty() && l.0.is_ascii() => {
            Some(vec![l.0.to_ascii_lowercase()])
        }
        HirKind::Capture(c) => any_of(&c.sub),
        HirKind::Repetition(r) if r.min > 0 => any_of(&r.sub),
        HirKind::Concat(parts) => parts.iter().filter_map(any_of).max_by_key(|a| strength(a)),
        HirKind::Alternation(branches) => {
            let sets = branches.iter().map(any_of).collect::<Option<Vec<_>>>()?;
            let all = sets.concat();
            (all.len() <= MAX_ALTERNATIVES).then_some(all)
        }
        _ => None,
    }
}

/// Longer shortest text rejects more names; fewer alternatives test faster.
fn strength(any: &[Vec<u8>]) -> (usize, Reverse<usize>) {
    let shortest = any.iter().map(Vec::len).min().unwrap_or(0);
    (shortest, Reverse(any.len()))
}

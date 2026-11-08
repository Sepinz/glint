//! Parallel matching, sorting, and the background search worker.

use std::sync::atomic::{AtomicU64, Ordering as AtOrd};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rayon::prelude::*;

use crate::engine::Engine;
use crate::index::{Index, Order};
use crate::query::{is_plain, Query, Scratch};

const CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Date,
}

/// Search results in display order. `All` avoids materializing the
/// "everything" result (the empty query) as millions of ids.
#[derive(Clone, Debug)]
pub enum Hits {
    All(u32),
    Ids(Vec<u32>),
}

impl Default for Hits {
    fn default() -> Hits {
        Hits::Ids(Vec::new())
    }
}

impl Hits {
    pub fn len(&self) -> usize {
        match self {
            Hits::All(n) => *n as usize,
            Hits::Ids(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, k: usize) -> Option<u32> {
        match self {
            Hits::All(n) => (k < *n as usize).then_some(k as u32),
            Hits::Ids(v) => v.get(k).copied(),
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len()).filter_map(move |k| self.get(k))
    }
    pub fn order(&self) -> Order<'_> {
        match self {
            Hits::All(n) => Order::All(*n),
            Hits::Ids(v) => Order::List(v),
        }
    }
    fn into_vec(self) -> Vec<u32> {
        match self {
            Hits::All(n) => (0..n).collect(),
            Hits::Ids(v) => v,
        }
    }
}

fn scan(ix: &Index, q: &Query, ids: impl Iterator<Item = u32>) -> Vec<u32> {
    let mut s = Scratch::default();
    ids.filter(|&i| q.matches(ix, i, &mut s)).collect()
}

/// Filter `base` (display order) by `q`, preserving order.
/// Returns `None` if `cancel` fired mid-way.
pub fn filter(
    ix: &Index,
    q: &Query,
    base: Order<'_>,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Option<Hits> {
    if q.is_empty() {
        return Some(match base {
            Order::All(n) => Hits::All(n),
            Order::List(l) => Hits::Ids(l.to_vec()),
        });
    }
    let parts: Option<Vec<Vec<u32>>> = match base {
        Order::List(ids) => ids
            .par_chunks(CHUNK)
            .map(|c| (!cancel()).then(|| scan(ix, q, c.iter().copied())))
            .collect(),
        Order::All(n) => (0..(n as usize).div_ceil(CHUNK))
            .into_par_iter()
            .map(|c| {
                let lo = (c * CHUNK) as u32;
                let hi = ((c + 1) * CHUNK).min(n as usize) as u32;
                (!cancel()).then(|| scan(ix, q, lo..hi))
            })
            .collect(),
    };
    let parts = parts?;
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for p in parts {
        out.extend_from_slice(&p);
    }
    Some(Hits::Ids(out))
}

/// Search the whole index (results in name order).
pub fn run(ix: &Index, q: &Query) -> Hits {
    filter(ix, q, ix.order(), &|| false).unwrap_or_default()
}

/// Reorder results. Name order is free (it is the index order); other keys
/// use a stable parallel sort with name order as the tie-break.
pub fn sort(ix: &Index, hits: Hits, key: SortKey, desc: bool) -> Hits {
    if key == SortKey::Name && !desc {
        return hits;
    }
    let mut v = hits.into_vec();
    match key {
        SortKey::Name => {}
        SortKey::Size => v.par_sort_by_key(|&i| (!ix.is_dir(i), ix.size(i))),
        SortKey::Date => v.par_sort_by_key(|&i| ix.mtime(i)),
        SortKey::Path => v.par_sort_by_cached_key(|&i| {
            let mut s = String::new();
            ix.path_into(i, &mut s);
            s.to_lowercase()
        }),
    }
    if desc {
        v.reverse();
    }
    Hits::Ids(v)
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub text: String,
    pub sort: SortKey,
    pub desc: bool,
    /// Scroll back to the top when the results arrive.
    pub reset: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub results: Arc<Hits>,
    pub epoch: u64,
    pub reset: bool,
    pub millis: f64,
    pub error: Option<String>,
}

struct Previous {
    text: String,
    sort: SortKey,
    desc: bool,
    epoch: u64,
    version: u64,
    results: Arc<Hits>,
}

impl Previous {
    /// Typing more characters into a plain query can only narrow it, so the
    /// new search only needs to scan the previous results.
    fn refines(&self, r: &Request, ix: &Index) -> bool {
        self.epoch == ix.epoch
            && self.version == ix.version
            && self.sort == r.sort
            && self.desc == r.desc
            && !self.text.trim().is_empty()
            && r.text.starts_with(&self.text)
            && is_plain(&r.text)
    }
}

/// Runs searches on a dedicated thread; only the newest request matters and
/// superseded searches are cancelled cooperatively.
pub struct Worker {
    tx: Sender<(u64, Option<Request>)>,
    generation: Arc<AtomicU64>,
    out: Arc<Mutex<Option<Response>>>,
}

impl Worker {
    pub fn spawn(engine: Arc<Engine>, done: impl Fn() + Send + 'static) -> Worker {
        let (tx, rx) = channel::<(u64, Option<Request>)>();
        let generation = Arc::new(AtomicU64::new(0));
        let out = Arc::new(Mutex::new(None));
        let (g2, o2) = (generation.clone(), out.clone());
        let spawned = std::thread::Builder::new()
            .name("glint-search".into())
            .spawn(move || {
                let mut prev: Option<Previous> = None;
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    let (gen, req) = job;
                    let Some(req) = req else {
                        // forget(): release the cached results (hibernation).
                        prev = None;
                        o2.lock().unwrap_or_else(|p| p.into_inner()).take();
                        continue;
                    };
                    let t = Instant::now();
                    let q = match Query::parse(&req.text) {
                        Ok(q) => q,
                        Err(e) => {
                            *o2.lock().unwrap_or_else(|p| p.into_inner()) = Some(Response {
                                error: Some(e),
                                ..Default::default()
                            });
                            done();
                            continue;
                        }
                    };
                    let ix = engine.read();
                    let cancel = || g2.load(AtOrd::Relaxed) != gen;
                    let refine = prev
                        .as_ref()
                        .filter(|p| p.refines(&req, &ix))
                        .map(|p| p.results.clone());
                    let base = refine.as_ref().map_or_else(|| ix.order(), |r| r.order());
                    let Some(mut found) = filter(&ix, &q, base, &cancel) else {
                        continue;
                    };
                    if refine.is_none() {
                        found = sort(&ix, found, req.sort, req.desc);
                    }
                    let results = Arc::new(found);
                    prev = Some(Previous {
                        text: req.text.clone(),
                        sort: req.sort,
                        desc: req.desc,
                        epoch: ix.epoch,
                        version: ix.version,
                        results: results.clone(),
                    });
                    let resp = Response {
                        results,
                        epoch: ix.epoch,
                        reset: req.reset,
                        millis: t.elapsed().as_secs_f64() * 1e3,
                        error: None,
                    };
                    drop(ix);
                    if cancel() {
                        continue;
                    }
                    *o2.lock().unwrap_or_else(|p| p.into_inner()) = Some(resp);
                    done();
                }
            });
        if spawned.is_err() {
            *out.lock().unwrap_or_else(|p| p.into_inner()) = Some(Response {
                error: Some("could not start search thread".into()),
                ..Default::default()
            });
        }
        Worker {
            tx,
            generation,
            out,
        }
    }

    pub fn submit(&self, r: Request) {
        let gen = self.generation.fetch_add(1, AtOrd::Relaxed) + 1;
        let _ = self.tx.send((gen, Some(r)));
    }

    /// Cancel any running query and drop everything cached.
    pub fn forget(&self) {
        let gen = self.generation.fetch_add(1, AtOrd::Relaxed) + 1;
        let _ = self.tx.send((gen, None));
    }

    pub fn take(&self) -> Option<Response> {
        self.out.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

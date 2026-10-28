//! Query execution: score every shard in parallel with corpus-wide statistics, merge the best
//! candidates, re-rank them with signals that need the stored page, then build snippets.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use crate::ranks::host;
use crate::segment::{self, strip_scheme, truncate, Segment};
use crate::tokenize::{query_terms, token_spans, tokens};

/// BM25 term-frequency saturation (k1) and length normalization (b). Overridable with
/// ERICKSON_K1 / ERICKSON_B for tuning runs.
fn bm25_params() -> (f32, f32) {
    static P: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        (env("ERICKSON_K1", 1.2), env("ERICKSON_B", 0.6))
    })
}
/// How many first-stage candidates reach re-ranking.
const CANDIDATES: usize = 300;
const PER_HOST: usize = 2;
const SNIPPET_BYTES: usize = 240;

pub struct Index {
    pub generation: String,
    segs: Vec<Segment>,
    pub n_docs: u64,
    pub n_deleted: u64,
    avgdl: f32,
}

pub struct Hit {
    pub url: String,
    pub title: String,
    /// HTML: escaped text with matched terms in <b>.
    pub snippet: String,
}

pub struct Results {
    pub hits: Vec<Hit>,
    pub matched: u64,
    pub micros: u128,
}

/// Which ranking signals run. Web search uses everything; benchmarks switch signals off one at
/// a time to measure what each contributes.
#[derive(Clone, Copy)]
pub struct Opts {
    /// Scale scores by the IDF-weighted share of query terms a page contains.
    pub coverage: bool,
    /// Second-stage multipliers: title, URL shape, host match, proximity.
    pub rerank: bool,
    /// At most this many results per host.
    pub per_host: Option<usize>,
    pub snippets: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { coverage: true, rerank: true, per_host: Some(PER_HOST), snippets: true }
    }
}

#[derive(Clone, Copy)]
struct Cand {
    score: f32,
    seg: u32,
    doc: u32,
}

impl PartialEq for Cand {
    fn eq(&self, o: &Self) -> bool {
        self.score == o.score
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
/// Reversed, so a `BinaryHeap<Cand>` keeps the lowest score on top.
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> Ordering {
        o.score.total_cmp(&self.score)
    }
}

fn push_top(heap: &mut BinaryHeap<Cand>, c: Cand, cap: usize) {
    if heap.len() < cap {
        heap.push(c);
    } else if heap.peek().is_some_and(|min| c.score > min.score) {
        heap.pop();
        heap.push(c);
    }
}

impl Index {
    /// Opens whatever generation `CURRENT` points at.
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = segment::current(root)?;
        let mut dirs: Vec<_> = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("meta.txt").exists())
            .collect();
        dirs.sort();
        let segs = dirs.iter().map(|p| Segment::open(p)).collect::<io::Result<Vec<_>>>()?;
        let n_docs: u64 = segs.iter().map(|s| s.n_docs as u64).sum();
        let total: u64 = segs.iter().map(|s| s.total_len).sum();
        Ok(Self {
            generation: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            n_deleted: segs.iter().map(|s| s.n_deleted() as u64).sum(),
            avgdl: (total as f32 / n_docs.max(1) as f32).max(1.0),
            n_docs,
            segs,
        })
    }

    pub fn n_segments(&self) -> usize {
        self.segs.len()
    }

    pub fn search(&self, q: &str, k: usize, offset: usize) -> Results {
        self.search_with(q, k, offset, Opts::default())
    }

    pub fn search_with(&self, q: &str, k: usize, offset: usize, opts: Opts) -> Results {
        let t0 = Instant::now();
        let terms = query_terms(q);
        if terms.is_empty() || self.segs.is_empty() {
            return Results { hits: Vec::new(), matched: 0, micros: t0.elapsed().as_micros() };
        }

        // IDF from document frequencies summed over every shard, so a page scores the same
        // whichever shard it landed in.
        let n = self.n_docs as f32;
        let idf: Vec<f32> = terms
            .iter()
            .map(|t| {
                let df: u64 = self.segs.iter().filter_map(|s| s.lookup(t)).map(|p| p.df as u64).sum();
                if df == 0 {
                    0.0
                } else {
                    (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln()
                }
            })
            .collect();

        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(self.segs.len());
        let chunk = self.segs.len().div_ceil(threads);
        let (heap, matched) = std::thread::scope(|s| {
            let handles: Vec<_> = (0..self.segs.len())
                .step_by(chunk)
                .map(|from| {
                    let (terms, idf) = (&terms, &idf);
                    let to = (from + chunk).min(self.segs.len());
                    s.spawn(move || self.score_shards(from, to, terms, idf, opts.coverage))
                })
                .collect();
            let mut heap = BinaryHeap::with_capacity(CANDIDATES + 1);
            let mut matched = 0;
            for h in handles {
                let (cands, m) = h.join().unwrap();
                matched += m;
                for c in cands {
                    push_top(&mut heap, c, CANDIDATES);
                }
            }
            (heap, matched)
        });

        let term_set: HashSet<&str> = terms.iter().map(String::as_str).collect();
        let mut ranked: Vec<(f32, Cand)> = heap
            .into_vec()
            .into_iter()
            .map(|c| {
                if !opts.rerank {
                    return (c.score, c);
                }
                let (url, title, text) = self.segs[c.seg as usize].doc(c.doc);
                (c.score * rerank(url, title, text, &terms, &term_set), c)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut per_host: HashMap<&str, usize> = HashMap::new();
        let (mut hits, mut skipped) = (Vec::new(), 0);
        for (_, c) in ranked {
            let (url, title, text) = self.segs[c.seg as usize].doc(c.doc);
            if let Some(cap) = opts.per_host {
                let n = per_host.entry(host(url)).or_insert(0);
                if *n >= cap {
                    continue;
                }
                *n += 1;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            hits.push(Hit {
                url: url.to_owned(),
                title: if title.is_empty() { strip_scheme(url).to_owned() } else { title.to_owned() },
                snippet: if opts.snippets { snippet(text, &term_set) } else { String::new() },
            });
            if hits.len() == k {
                break;
            }
        }
        Results { hits, matched, micros: t0.elapsed().as_micros() }
    }

    /// First stage: field-weighted BM25 × term coverage × static page prior, over shards `from..to`.
    /// Coverage is weighted by IDF, so missing "faucet" costs far more than missing "how".
    fn score_shards(&self, from: usize, to: usize, terms: &[String], idf: &[f32], use_coverage: bool) -> (Vec<Cand>, u64) {
        let mut heap = BinaryHeap::with_capacity(CANDIDATES + 1);
        let mut matched = 0u64;
        let (mut acc, mut covered, mut touched) = (Vec::<f32>::new(), Vec::<f32>::new(), Vec::<u32>::new());
        let total_idf: f32 = idf.iter().sum::<f32>().max(f32::MIN_POSITIVE);
        let (k1, b) = bm25_params();
        for si in from..to {
            let seg = &self.segs[si];
            let nd = seg.n_docs as usize;
            acc.clear();
            acc.resize(nd, 0.0);
            covered.clear();
            covered.resize(nd, 0.0);
            touched.clear();
            for (t, &w) in terms.iter().zip(idf) {
                if w == 0.0 {
                    continue;
                }
                let Some(p) = seg.lookup(t) else { continue };
                p.for_each(|d, tf| {
                    let di = d as usize;
                    let tf = tf as f32;
                    let dl = seg.doc_len(d) as f32;
                    acc[di] += w * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / self.avgdl));
                    if covered[di] == 0.0 {
                        touched.push(d);
                    }
                    covered[di] += w;
                });
            }
            for &d in &touched {
                if seg.is_deleted(d) {
                    continue;
                }
                matched += 1;
                let di = d as usize;
                let coverage = if use_coverage { (covered[di] / total_idf).powi(3) } else { 1.0 };
                let prior = (0.6 + 0.8 * seg.authority(d)) * (0.5 + 0.5 * seg.quality(d));
                push_top(&mut heap, Cand { score: acc[di] * coverage * prior, seg: si as u32, doc: d }, CANDIDATES);
            }
        }
        (heap.into_vec(), matched)
    }
}

/// Second stage: multiplier from title coverage, URL shape, host match and term proximity.
fn rerank(url: &str, title: &str, text: &str, terms: &[String], set: &HashSet<&str>) -> f32 {
    let nt = terms.len() as f32;
    let mut in_title = HashSet::new();
    tokens(title, &mut |t: &str| {
        if set.contains(t) {
            in_title.insert(t.to_owned());
        }
    });
    let mut p = 1.0 + 0.6 * in_title.len() as f32 / nt;

    let path = strip_scheme(url).split_once('/').map_or("", |(_, p)| p);
    let path = path.split(['?', '#']).next().unwrap_or("");
    let depth = path.split('/').filter(|s| !s.is_empty()).count() as f32;
    p /= 1.0 + 0.08 * depth;
    if depth == 0.0 {
        p *= 1.3;
    }

    let mut in_host = 0usize;
    tokens(host(url), &mut |t: &str| {
        if set.contains(t) {
            in_host += 1;
        }
    });
    if in_host > 0 {
        p *= 1.0 + 0.4 * in_host.min(2) as f32;
    }
    if url.len() > 120 {
        p *= 0.85;
    }
    if url.contains('?') {
        p *= 0.75;
    }
    let h = host(url);
    if h.starts_with("staging") || h.starts_with("dev.") || h.starts_with("ec2-") || h.contains(".compute.") {
        p *= 0.5;
    }
    if terms.len() > 1 {
        if let Some(span) = min_span(title, text, terms) {
            p *= 1.0 + 0.6 * (nt / span as f32).min(1.0);
        }
    }
    p
}

/// Smallest window, in tokens, containing every query term; `None` if some term is missing.
fn min_span(title: &str, text: &str, terms: &[String]) -> Option<usize> {
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    for part in [title, text] {
        tokens(part, &mut |t: &str| {
            if let Some(i) = terms.iter().position(|x| x == t) {
                hits.push((pos, i));
            }
            pos += 1;
        });
        pos += 50; // a phrase can't straddle the title and the body
    }
    let mut count = vec![0usize; terms.len()];
    let (mut have, mut lo, mut best) = (0, 0, None);
    for hi in 0..hits.len() {
        let ti = hits[hi].1;
        if count[ti] == 0 {
            have += 1;
        }
        count[ti] += 1;
        while have == terms.len() {
            let span = hits[hi].0 - hits[lo].0 + 1;
            if best.map_or(true, |b| span < b) {
                best = Some(span);
            }
            let tl = hits[lo].1;
            count[tl] -= 1;
            if count[tl] == 0 {
                have -= 1;
            }
            lo += 1;
        }
    }
    best
}

/// The ~240-byte window covering the most distinct query terms, with matches in <b>.
fn snippet(text: &str, set: &HashSet<&str>) -> String {
    let mut m: Vec<(usize, usize, &str)> = Vec::new();
    token_spans(text, &mut |s, e, t: &str| {
        if let Some(&term) = set.get(t) {
            m.push((s, e, term));
        }
    });
    if m.is_empty() {
        let mut out = escape(truncate(text, SNIPPET_BYTES));
        if text.len() > SNIPPET_BYTES {
            out.push('…');
        }
        return out;
    }

    let (mut best, mut best_n) = (0, 0);
    let scan = &m[..m.len().min(200)];
    for (i, &(start, _, _)) in scan.iter().enumerate() {
        let mut seen: Vec<&str> = Vec::new();
        for &(s, _, t) in &scan[i..] {
            if s > start + SNIPPET_BYTES - 40 {
                break;
            }
            if !seen.contains(&t) {
                seen.push(t);
            }
        }
        if seen.len() > best_n {
            (best, best_n) = (i, seen.len());
        }
    }

    let mut start = m[best].0.saturating_sub(60);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    if start > 0 {
        // Begin at a word boundary.
        if let Some(sp) = text[start..].find(char::is_whitespace) {
            if sp < 30 {
                start += sp + text[start + sp..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    let mut end = (start + SNIPPET_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut at = start;
    for &(s, e, _) in &m {
        if s < start {
            continue;
        }
        if e > end {
            break;
        }
        out.push_str(&escape(&text[at..s]));
        out.push_str("<b>");
        out.push_str(&escape(&text[s..e]));
        out.push_str("</b>");
        at = e;
    }
    out.push_str(&escape(&text[at..end]));
    if end < text.len() {
        out.push('…');
    }
    out
}

/// HTML-escapes and collapses runs of whitespace (WET text is full of newlines).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

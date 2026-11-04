//! Query execution: score every shard in parallel with corpus-wide statistics, merge the best
//! candidates, re-rank them with signals that need the stored page, then build snippets.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use crate::ranks::host;
use crate::segment::{self, strip_scheme, truncate, Postings, Segment, BLOCK};
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
/// Long queries (at least this many terms with an IDF) get a reduced first pass, see `first_pass_idf`.
/// Overridable with ERICKSON_FIRST_MASS / ERICKSON_FIRST_TERMS for tuning runs.
fn first_pass_params() -> (f32, usize) {
    static P: std::sync::OnceLock<(f32, usize)> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let mass = std::env::var("ERICKSON_FIRST_MASS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.6);
        let terms = std::env::var("ERICKSON_FIRST_TERMS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        (mass, terms)
    })
}

/// IDF for the first pass. A long query leaves out its most common terms there, keeping the
/// rarest ones that together reach FIRST_MASS of its total IDF: their lists are the longest and
/// they barely move a score. The candidates found are then rescored with every term.
fn first_pass_idf(idf: &[f32]) -> Vec<f32> {
    let (mass, min_terms) = first_pass_params();
    if mass >= 1.0 || idf.iter().filter(|&&w| w > 0.0).count() < min_terms {
        return idf.to_vec();
    }
    let total: f32 = idf.iter().sum();
    let mut order: Vec<usize> = (0..idf.len()).collect();
    order.sort_by(|&a, &b| idf[b].total_cmp(&idf[a]).then(a.cmp(&b)));
    let (mut out, mut kept) = (vec![0.0; idf.len()], 0.0);
    for i in order {
        if kept >= mass * total {
            break;
        }
        out[i] = idf[i];
        kept += idf[i];
    }
    out
}
const SNIPPET_BYTES: usize = 240;

pub struct Index {
    pub generation: String,
    segs: Vec<Segment>,
    /// Per segment: docs are numbered by prior, highest first (see `segment::merge`), so the
    /// prior of the doc a query is on bounds every doc after it.
    by_prior: Vec<bool>,
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
    /// Time and major page faults per phase: term lookup, first-stage scoring, re-rank + snippets.
    pub phase_micros: [u128; 3],
    pub phase_faults: [u64; 3],
}

/// Major page faults so far in this process: each is a read from disk. Linux only; 0 elsewhere.
fn major_faults() -> u64 {
    if !cfg!(target_os = "linux") {
        return 0;
    }
    // Field 12 of /proc/self/stat, counted after the parenthesised command name.
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(9)).and_then(|v| v.parse().ok()).unwrap_or(0)
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
    /// Score every posting, with every term, instead of skipping with block-max WAND. Same
    /// results as the default for queries too short for a reduced first pass (`first_pass_idf`),
    /// slower; kept to check that the skipping never changes an answer.
    pub exhaustive: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { coverage: true, rerank: true, per_host: Some(PER_HOST), snippets: true, exhaustive: false }
    }
}

/// Upper bound of the static page prior, (0.6 + 0.8 * authority) * (0.5 + 0.5 * quality).
const PRIOR_MAX: f32 = 1.4;

fn page_prior(seg: &Segment, d: u32) -> f32 {
    prior(seg.authority(d), seg.quality(d))
}

/// Static page prior from link authority and content quality, both in 0..=1.
pub fn prior(authority: f32, quality: f32) -> f32 {
    (0.6 + 0.8 * authority) * (0.5 + 0.5 * quality)
}

/// One query term's position in one segment's postings, for block-max WAND.
struct Cursor<'a> {
    p: Postings<'a>,
    /// Position of the term in the query.
    term: usize,
    /// The term's IDF.
    w: f32,
    /// Highest score any doc can get from this term in this segment.
    max_score: f32,
    decoded: usize,
    docs: [u32; BLOCK],
    tfs: [u32; BLOCK],
    len: usize,
    pos: usize,
    /// Block the last `block_max` lookup landed in; only moves forward.
    shallow: usize,
    /// Current doc, or `u32::MAX` once the list is exhausted.
    doc: u32,
}

impl<'a> Cursor<'a> {
    fn new(p: Postings<'a>, term: usize, w: f32, bound: &impl Fn(u32, u32) -> f32) -> Self {
        let max_score = (0..p.n_blocks()).map(|b| {
            let (tf, dl) = p.block_limits(b);
            w * bound(tf, dl)
        });
        let mut c = Cursor {
            p,
            term,
            w,
            max_score: max_score.fold(0.0, f32::max),
            decoded: 0,
            docs: [0; BLOCK],
            tfs: [0; BLOCK],
            len: 0,
            pos: 0,
            shallow: 0,
            doc: u32::MAX,
        };
        c.load(0);
        c
    }

    fn load(&mut self, b: usize) {
        self.len = self.p.decode_block(b, &mut self.docs, &mut self.tfs);
        self.decoded = b;
        self.shallow = self.shallow.max(b);
        self.pos = 0;
        self.doc = self.docs[0];
    }

    fn tf(&self) -> u32 {
        self.tfs[self.pos]
    }

    fn next(&mut self) {
        self.pos += 1;
        if self.pos < self.len {
            self.doc = self.docs[self.pos];
        } else if self.decoded + 1 < self.p.n_blocks() {
            self.load(self.decoded + 1);
        } else {
            self.doc = u32::MAX;
        }
    }

    /// Moves to the first doc >= `target`, decoding only the block that holds it.
    fn seek(&mut self, target: u32) {
        if self.doc >= target {
            return;
        }
        let n = self.p.n_blocks();
        let mut b = self.decoded;
        while b < n && self.p.last_doc(b) < target {
            b += 1;
        }
        if b == n {
            self.doc = u32::MAX;
            return;
        }
        if b != self.decoded {
            self.load(b);
        }
        while self.docs[self.pos] < target {
            self.pos += 1;
        }
        self.doc = self.docs[self.pos];
    }

    /// Best score this term can give any doc in the block holding `target`, and that block's
    /// last doc. Reads only the block directory.
    fn block_max(&mut self, target: u32, bound: &impl Fn(u32, u32) -> f32) -> (f32, u32) {
        let n = self.p.n_blocks();
        while self.shallow < n && self.p.last_doc(self.shallow) < target {
            self.shallow += 1;
        }
        if self.shallow == n {
            // The list ends before `target`: this term can't contribute there.
            return (0.0, u32::MAX);
        }
        let (tf, dl) = self.p.block_limits(self.shallow);
        (self.w * bound(tf, dl), self.p.last_doc(self.shallow))
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
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
/// "Better first": higher score, then lower (segment, doc) so ties break the same way however
/// the candidates were found. A `BinaryHeap<Cand>` therefore keeps the worst one on top.
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> Ordering {
        o.score.total_cmp(&self.score).then((self.seg, self.doc).cmp(&(o.seg, o.doc)))
    }
}

fn push_top(heap: &mut BinaryHeap<Cand>, c: Cand, cap: usize) {
    if heap.len() < cap {
        heap.push(c);
    } else if heap.peek().is_some_and(|worst| c < *worst) {
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
            by_prior: segs.iter().map(|s| (1..s.n_docs).all(|d| page_prior(s, d) <= page_prior(s, d - 1))).collect(),
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
        let f0 = major_faults();
        let terms = query_terms(q);
        if terms.is_empty() || self.segs.is_empty() {
            return Results {
                hits: Vec::new(),
                matched: 0,
                micros: t0.elapsed().as_micros(),
                phase_micros: [0; 3],
                phase_faults: [0; 3],
            };
        }

        // Each term is looked up once per shard; IDF and scoring both use the result.
        let lists: Vec<Vec<Option<Postings>>> =
            self.segs.iter().map(|s| terms.iter().map(|t| s.lookup(t)).collect()).collect();

        // IDF from document frequencies summed over every shard, so a page scores the same
        // whichever shard it landed in.
        let n = self.n_docs as f32;
        let idf: Vec<f32> = (0..terms.len())
            .map(|t| {
                let df: u64 = lists.iter().filter_map(|l| l[t]).map(|p| p.df as u64).sum();
                if df == 0 {
                    0.0
                } else {
                    (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln()
                }
            })
            .collect();

        let first_idf = if opts.exhaustive { idf.clone() } else { first_pass_idf(&idf) };

        let (t1, f1) = (t0.elapsed().as_micros(), major_faults());

        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(self.segs.len());
        let chunk = self.segs.len().div_ceil(threads);
        let (heap, matched) = std::thread::scope(|s| {
            let handles: Vec<_> = (0..self.segs.len())
                .step_by(chunk)
                .map(|from| {
                    let (lists, idf, first_idf) = (&lists, &idf, &first_idf);
                    let to = (from + chunk).min(self.segs.len());
                    s.spawn(move || {
                        if opts.exhaustive {
                            self.score_shards(from, to, lists, idf, opts.coverage)
                        } else {
                            self.wand_shards(from, to, lists, first_idf, opts.coverage)
                        }
                    })
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
        let mut cands = heap.into_vec();
        if first_idf != idf {
            self.rescore(&mut cands, &lists, &idf, opts.coverage);
        }

        let (t2, f2) = (t0.elapsed().as_micros(), major_faults());

        let term_set: HashSet<&str> = terms.iter().map(String::as_str).collect();
        let mut ranked: Vec<(f32, Cand)> = cands
            .into_iter()
            .map(|c| {
                if !opts.rerank {
                    return (c.score, c);
                }
                let (url, title, text) = self.segs[c.seg as usize].doc(c.doc);
                (c.score * rerank(url, title, text, &terms, &term_set), c)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.1.seg, a.1.doc).cmp(&(b.1.seg, b.1.doc))));

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
        let (t3, f3) = (t0.elapsed().as_micros(), major_faults());
        Results {
            hits,
            matched,
            micros: t3,
            phase_micros: [t1, t2 - t1, t3 - t2],
            phase_faults: [f1 - f0, f2 - f1, f3 - f2],
        }
    }

    /// First stage: field-weighted BM25 × term coverage × static page prior, over shards `from..to`.
    /// Coverage is weighted by IDF, so missing "faucet" costs far more than missing "how".
    fn score_shards(
        &self,
        from: usize,
        to: usize,
        lists: &[Vec<Option<Postings>>],
        idf: &[f32],
        use_coverage: bool,
    ) -> (Vec<Cand>, u64) {
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
            for (&p, &w) in lists[si].iter().zip(idf) {
                if w == 0.0 {
                    continue;
                }
                let Some(p) = p else { continue };
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
                let prior = page_prior(seg, d);
                push_top(&mut heap, Cand { score: acc[di] * coverage * prior, seg: si as u32, doc: d }, CANDIDATES);
            }
        }
        (heap.into_vec(), matched)
    }
}

impl Index {
    /// The same first-stage score as `score_shards`, computed with block-max WAND: docs are
    /// visited in id order across all query terms, and any doc or block whose upper bound can't
    /// beat the current top-`CANDIDATES` threshold is skipped without being decoded. Coverage makes
    /// the bound tight: a doc missing the rare terms can't reach the threshold at all.
    ///
    /// `matched` is a lower bound here (the largest single-term df per shard), since skipped docs
    /// are never counted.
    fn wand_shards(
        &self,
        from: usize,
        to: usize,
        lists: &[Vec<Option<Postings>>],
        idf: &[f32],
        use_coverage: bool,
    ) -> (Vec<Cand>, u64) {
        let mut heap = BinaryHeap::with_capacity(CANDIDATES + 1);
        let mut matched = 0u64;
        let total_idf: f32 = idf.iter().sum::<f32>().max(f32::MIN_POSITIVE);
        let (k1, b) = bm25_params();
        let avgdl = self.avgdl;
        let tf_part = |tf: f32, dl: f32| tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avgdl));
        let bound = |tf: u32, dl: u32| tf_part(tf as f32, dl as f32);
        let coverage = |covered: f32| if use_coverage { (covered / total_idf).powi(3) } else { 1.0 };

        for si in from..to {
            let seg = &self.segs[si];
            let prior_from = |d: u32| if self.by_prior[si] { page_prior(seg, d) } else { PRIOR_MAX };
            let mut cs: Vec<Cursor> = lists[si]
                .iter()
                .zip(idf)
                .filter(|(_, &w)| w > 0.0)
                .enumerate()
                .filter_map(|(term, (&p, &w))| p.map(|p| Cursor::new(p, term, w, &bound)))
                .collect();
            matched += cs.iter().map(|c| c.p.df as u64).max().unwrap_or(0);

            // Cursors in doc order, as `doc << 32 | index` keys: a cursor holds two decoded blocks,
            // so sorting the cursors themselves would copy kilobytes on every step. Only the keys
            // of cursors that moved are refreshed (`moved`), always a prefix of `ord`.
            let key = |c: &Cursor, i: usize| (c.doc as u64) << 32 | i as u64;
            let mut ord: Vec<u64> = cs.iter().enumerate().map(|(i, c)| key(c, i)).collect();
            let mut moved = 0;
            loop {
                for k in &mut ord[..moved] {
                    let i = *k as u32 as usize;
                    *k = key(&cs[i], i);
                }
                ord.sort_unstable();
                while ord.last().is_some_and(|&k| (k >> 32) as u32 == u32::MAX) {
                    ord.pop();
                }
                if ord.is_empty() {
                    break;
                }
                let at = |n: usize| ord[n] as u32 as usize;
                // A doc that only ties the threshold can still get in on a lower doc id, so
                // skipping requires a bound strictly below it.
                let threshold = if heap.len() < CANDIDATES { 0.0 } else { heap.peek().map_or(0.0, |c: &Cand| c.score) };

                // Pivot: the first position where the terms so far could reach the threshold.
                // No doc before the first cursor's is left, so its prior bounds every doc still to come.
                let prior_max = prior_from((ord[0] >> 32) as u32);
                let (mut max_sum, mut idf_sum, mut pivot) = (0.0, 0.0, None);
                for n in 0..ord.len() {
                    let c = &cs[at(n)];
                    max_sum += c.max_score;
                    idf_sum += c.w;
                    if prior_max * coverage(idf_sum) * max_sum >= threshold {
                        pivot = Some(n);
                        break;
                    }
                }
                let Some(mut last) = pivot else { break };
                let d = (ord[last] >> 32) as u32;
                while last + 1 < ord.len() && (ord[last + 1] >> 32) as u32 == d {
                    last += 1;
                }

                // Tighter check with the maxima of the blocks that actually hold `d`.
                let (mut block_sum, mut idf_sum, mut skip_to) = (0.0, 0.0, u32::MAX);
                for n in 0..=last {
                    let c = &mut cs[at(n)];
                    let (m, block_last) = c.block_max(d, &bound);
                    block_sum += m;
                    idf_sum += c.w;
                    skip_to = skip_to.min(block_last.saturating_add(1));
                }
                if prior_from(d) * coverage(idf_sum) * block_sum < threshold {
                    // Nothing before the end of the shortest of these blocks can make it, and
                    // terms after the pivot haven't reached `d` yet.
                    if let Some(&k) = ord.get(last + 1) {
                        skip_to = skip_to.min((k >> 32) as u32);
                    }
                    for n in 0..=last {
                        cs[at(n)].seek(skip_to);
                    }
                    moved = last + 1;
                    continue;
                }

                if (ord[0] >> 32) as u32 == d {
                    if !seg.is_deleted(d) {
                        // Summed in query-term order, exactly as `score_shards` does, so both
                        // paths produce bit-identical scores.
                        let dl = seg.doc_len(d) as f32;
                        let mut parts = [(0usize, 0.0f32, 0.0f32); 12];
                        for (n, slot) in parts[..=last].iter_mut().enumerate() {
                            let c = &cs[at(n)];
                            *slot = (c.term, c.w * tf_part(c.tf() as f32, dl), c.w);
                        }
                        let parts = &mut parts[..=last];
                        parts.sort_unstable_by_key(|p| p.0);
                        let (mut s, mut covered) = (0.0, 0.0);
                        for &(_, contribution, w) in parts.iter() {
                            s += contribution;
                            covered += w;
                        }
                        let score = s * coverage(covered) * page_prior(seg, d);
                        push_top(&mut heap, Cand { score, seg: si as u32, doc: d }, CANDIDATES);
                    }
                    for n in 0..=last {
                        cs[at(n)].next();
                    }
                    moved = last + 1;
                } else {
                    for n in 0..last {
                        cs[at(n)].seek(d);
                    }
                    moved = last;
                }
            }
        }
        (heap.into_vec(), matched)
    }

    /// Exact first-stage scores, with every query term, for candidates a reduced first pass
    /// found. Summed in query-term order like `score_shards`.
    fn rescore(&self, cands: &mut [Cand], lists: &[Vec<Option<Postings>>], idf: &[f32], use_coverage: bool) {
        let total_idf: f32 = idf.iter().sum::<f32>().max(f32::MIN_POSITIVE);
        let (k1, b) = bm25_params();
        let avgdl = self.avgdl;
        let tf_part = |tf: f32, dl: f32| tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avgdl));
        let bound = |tf: u32, dl: u32| tf_part(tf as f32, dl as f32);
        cands.sort_unstable_by_key(|c| (c.seg, c.doc));
        for run in cands.chunk_by_mut(|a, b| a.seg == b.seg) {
            let si = run[0].seg as usize;
            let seg = &self.segs[si];
            let (mut s, mut covered) = (vec![0.0f32; run.len()], vec![0.0f32; run.len()]);
            for (term, (&p, &w)) in lists[si].iter().zip(idf).enumerate() {
                let Some(p) = p.filter(|_| w > 0.0) else { continue };
                let mut c = Cursor::new(p, term, w, &bound);
                for (j, cand) in run.iter().enumerate() {
                    c.seek(cand.doc);
                    if c.doc == cand.doc {
                        s[j] += w * tf_part(c.tf() as f32, seg.doc_len(cand.doc) as f32);
                        covered[j] += w;
                    }
                }
            }
            for (j, cand) in run.iter_mut().enumerate() {
                let coverage = if use_coverage { (covered[j] / total_idf).powi(3) } else { 1.0 };
                cand.score = s[j] * coverage * page_prior(seg, cand.doc);
            }
        }
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
            if best.is_none_or(|b| span < b) {
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

#[cfg(test)]
mod tests {
    use super::{prior, Index, Opts};
    use crate::segment::{self, SegmentWriter};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Writes `segs` segments of random pages over a small Zipf-shaped vocabulary, so common words
    /// have postings across many blocks and rare ones only a few, and publishes them as gen-0001.
    /// With `merged`, also merges them into one prior-ordered segment as gen-0002, like `merge`.
    fn build(root: &Path, segs: u32, docs: u32, merged: bool) -> Index {
        let _ = fs::remove_dir_all(root);
        let words: Vec<String> = (0..300).map(|i| format!("w{i}")).collect();
        let weights: Vec<f64> = (0..words.len()).map(|i| 1.0 / (i + 1) as f64).collect();
        let total: f64 = weights.iter().sum();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        let gen1 = root.join("gen-0001");
        let mut dirs = Vec::new();
        for s in 0..segs {
            let dir = gen1.join(segment::seg_name(s));
            let mut w = SegmentWriter::create(&dir).unwrap();
            for d in 0..docs {
                let len = 20 + (next() * 180.0) as usize;
                let mut pick = || {
                    let mut r = next() * total;
                    let i = weights
                        .iter()
                        .position(|&p| {
                            r -= p;
                            r <= 0.0
                        })
                        .unwrap_or(words.len() - 1);
                    words[i].as_str()
                };
                let text: Vec<&str> = (0..len).map(|_| pick()).collect();
                let title: Vec<&str> = (0..4).map(|_| pick()).collect();
                let url = format!("https://h{}.example/{s}/{d}", (next() * 500.0) as u32);
                w.add(&url, &title.join(" "), &text.join(" "), 0.3 + 0.7 * next() as f32, next() as f32).unwrap();
            }
            w.finish().unwrap();
            dirs.push(dir);
        }
        fs::write(gen1.join("manifest.tsv"), "").unwrap();
        segment::publish(root, "gen-0001").unwrap();
        if merged {
            let gen2 = root.join("gen-0002");
            segment::merge(&gen2.join(segment::seg_name(0)), &dirs, Some(prior)).unwrap();
            fs::write(gen2.join("manifest.tsv"), "").unwrap();
            segment::publish(root, "gen-0002").unwrap();
        }
        Index::open(root).unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("erickson-test-{name}-{}", std::process::id()))
    }

    /// Block-max skipping must return exactly what scoring every posting returns, for queries
    /// short enough to get the full first pass: common words, rare words, and mixes of both.
    #[test]
    fn skipping_matches_exhaustive() {
        let queries =
            ["w0", "w1 w2", "w0 w1 w2", "w5 w250", "w299", "w3 w40 w120", "w0 w299", "w7 w8 w9 w10", "w100 w101"];
        for (name, merged) in [("segments", false), ("merged", true)] {
            let root = tmp(name);
            let ix = build(&root, 3, 1500, merged);
            assert_eq!(ix.by_prior.iter().all(|&b| b), merged, "{name}: prior order detected");
            for q in queries {
                for offset in [0, 10] {
                    let fast = ix.search_with(q, 10, offset, Opts::default());
                    let full = ix.search_with(q, 10, offset, Opts { exhaustive: true, ..Default::default() });
                    let urls = |r: &super::Results| r.hits.iter().map(|h| h.url.clone()).collect::<Vec<_>>();
                    assert!(!full.hits.is_empty(), "{name}: {q} found nothing");
                    assert_eq!(urls(&fast), urls(&full), "{name}: {q} (offset {offset})");
                }
            }
            let _ = fs::remove_dir_all(&root);
        }
    }
}

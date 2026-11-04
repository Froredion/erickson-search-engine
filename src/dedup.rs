//! Corpus-wide duplicate removal, run as a batch pass after every shard has been written.
//!
//! Ingest workers never coordinate: each shard writes one fingerprint record per page. This pass
//! then loads every record, links pages that share a canonical URL, share their exact token
//! sequence, or have SimHashes within 3 bits, and keeps one page per cluster. The survivor is
//! chosen by `key`, never by arrival order: the copy on the higher-authority domain wins, then the
//! shorter URL. Everything else in the cluster is tombstoned in its shard's `deleted.bin`.
//!
//! Near-duplicates use the table scheme from Manku et al. (2007): split the 64-bit fingerprint into
//! four 16-bit bands, and the remaining 48 bits into four 12-bit sub-bands. Two fingerprints within
//! 3 bits must agree on some band and, within the rest, on some sub-band, so sorting by each of
//! the 16 (band, sub-band) keys puts every candidate pair next to each other.

use crate::filter::{hash, Fingerprint};
use crate::segment::{self, strip_scheme};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

/// Bytes per fingerprint record: url hash, content hash, simhash, authority position, url length.
pub const RECORD: usize = 32;
const MAX_SIM_DISTANCE: u32 = 3;
const TABLES: usize = 16;
/// Pages sharing a table key are compared against at most this many earlier pages in the run,
/// so a template repeated across a million pages can't make the pass quadratic.
const MAX_COMPARE: usize = 64;
const URL_SALT: u64 = 0x5bd1_e995_7f4a_7c15;

pub fn push_record(out: &mut Vec<u8>, url: &str, fp: Fingerprint, authority_pos: u32) {
    let url_key = hash(strip_scheme(url).trim_start_matches("www.").as_bytes()) ^ URL_SALT;
    out.extend_from_slice(&url_key.to_le_bytes());
    out.extend_from_slice(&fp.exact.to_le_bytes());
    out.extend_from_slice(&fp.sim.to_le_bytes());
    out.extend_from_slice(&authority_pos.to_le_bytes());
    out.extend_from_slice(&(url.len() as u32).to_le_bytes());
}

struct Page {
    url: u64,
    content: u64,
    sim: u64,
    /// (authority position << 32 | url length): smaller is better.
    key: u64,
    seg: u32,
    doc: u32,
}

pub struct Report {
    pub pages: u64,
    pub removed: u64,
}

/// Dedups every finished segment under `gen_dir` and rewrites their tombstones.
pub fn run(gen_dir: &Path, threads: usize) -> io::Result<Report> {
    let mut pages = Vec::new();
    for e in fs::read_dir(gen_dir)? {
        let dir = e?.path();
        let Some(seg) = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("seg-"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if !dir.join("meta.txt").exists() {
            continue;
        }
        let Ok(bytes) = fs::read(dir.join("fingerprints.bin")) else { continue };
        for (doc, r) in bytes.as_chunks::<RECORD>().0.iter().enumerate() {
            let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
            let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
            pages.push(Page {
                url: u64_at(0),
                content: u64_at(8),
                sim: u64_at(16),
                key: (u32_at(24) as u64) << 32 | u32_at(28) as u64,
                seg,
                doc: doc as u32,
            });
        }
    }
    let n = pages.len();
    let mut uf = UnionFind::new(n);
    link_equal(&pages, &mut uf, |p| p.url);
    link_equal(&pages, &mut uf, |p| p.content);

    let parallel = threads.clamp(1, TABLES);
    let tables: Vec<usize> = (0..TABLES).collect();
    for batch in tables.chunks(parallel) {
        let pairs: Vec<Vec<(u32, u32)>> = std::thread::scope(|s| {
            let pages = &pages;
            let handles: Vec<_> = batch.iter().map(|&t| s.spawn(move || near_pairs(pages, t))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (a, b) in pairs.into_iter().flatten() {
            uf.union(a, b);
        }
    }

    let better = |a: &Page, b: &Page| (a.key, a.seg, a.doc) < (b.key, b.seg, b.doc);
    let mut best = vec![u32::MAX; n];
    for i in 0..n {
        let r = uf.find(i as u32) as usize;
        if best[r] == u32::MAX || better(&pages[i], &pages[best[r] as usize]) {
            best[r] = i as u32;
        }
    }
    let mut deleted: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, p) in pages.iter().enumerate() {
        if best[uf.find(i as u32) as usize] != i as u32 {
            deleted.entry(p.seg).or_default().push(p.doc);
        }
    }

    let mut removed = 0;
    let segs: std::collections::HashSet<u32> = pages.iter().map(|p| p.seg).collect();
    for seg in segs {
        let dir = gen_dir.join(segment::seg_name(seg));
        match deleted.get_mut(&seg) {
            Some(docs) => {
                docs.sort_unstable();
                removed += docs.len() as u64;
                segment::write_tombstones(&dir, docs)?;
            }
            None => {
                let _ = fs::remove_file(dir.join("deleted.bin"));
            }
        }
    }
    Ok(Report { pages: n as u64, removed })
}

fn link_equal(pages: &[Page], uf: &mut UnionFind, key: impl Fn(&Page) -> u64) {
    let mut v: Vec<(u64, u32)> = pages.iter().enumerate().map(|(i, p)| (key(p), i as u32)).collect();
    v.sort_unstable();
    for w in v.windows(2) {
        if w[0].0 == w[1].0 {
            uf.union(w[0].1, w[1].1);
        }
    }
}

/// The 28-bit sort key of `sim` in table `t`: one full 16-bit band plus one 12-bit sub-band of
/// the other 48 bits.
fn table_key(sim: u64, t: usize) -> u64 {
    let (band, sub) = (t / 4, t % 4);
    let band_bits = (sim >> (band * 16)) & 0xffff;
    let rest = if band == 3 {
        sim & 0xffff_ffff_ffff
    } else {
        let low = sim & ((1u64 << (band * 16)) - 1);
        let high = sim >> ((band + 1) * 16);
        low | (high << (band * 16))
    };
    band_bits << 12 | (rest >> (sub * 12)) & 0xfff
}

fn near_pairs(pages: &[Page], t: usize) -> Vec<(u32, u32)> {
    let mut v: Vec<u64> = pages.iter().enumerate().map(|(i, p)| table_key(p.sim, t) << 32 | i as u64).collect();
    v.sort_unstable();
    let mut pairs = Vec::new();
    let mut start = 0;
    while start < v.len() {
        let k = v[start] >> 32;
        let mut end = start + 1;
        while end < v.len() && v[end] >> 32 == k {
            end += 1;
        }
        let run = &v[start..end];
        for (x, &a) in run.iter().enumerate().skip(1) {
            let ia = (a & 0xffff_ffff) as usize;
            if let Some(&b) = run[..x.min(MAX_COMPARE)]
                .iter()
                .find(|&&b| (pages[ia].sim ^ pages[(b & 0xffff_ffff) as usize].sim).count_ones() <= MAX_SIM_DISTANCE)
            {
                pairs.push((ia as u32, (b & 0xffff_ffff) as u32));
            }
        }
        start = end;
    }
    pairs
}

struct UnionFind {
    parent: Vec<u32>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self { parent: (0..n as u32).collect() }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let p = self.parent[x as usize];
            self.parent[x as usize] = self.parent[p as usize];
            x = p;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi as usize] = lo;
        }
    }
}

/// Lowercase host, no fragment, no tracking parameters.
pub fn canonical_url(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let (host, path) = rest.find(['/', '?']).map_or((rest, ""), |i| rest.split_at(i));
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let query: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let name = p.split('=').next().unwrap_or("");
            !(name.starts_with("utm_") || matches!(name, "fbclid" | "gclid" | "msclkid" | "ref"))
        })
        .collect();
    let mut out = format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase());
    out.push_str(if path.is_empty() { "/" } else { path });
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::table_key;

    /// Any two fingerprints within 3 bits must share a key in at least one table.
    #[test]
    fn tables_cover_three_bit_neighbours() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let (i, j, k) = ((x % 64) as u32, ((x >> 8) % 64) as u32, ((x >> 16) % 64) as u32);
            let y = x ^ (1u64 << i) ^ (1u64 << j) ^ (1u64 << k);
            assert!((0..16).any(|t| table_key(x, t) == table_key(y, t)), "{x:x} vs {y:x}");
        }
    }
}

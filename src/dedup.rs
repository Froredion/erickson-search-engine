//! Corpus-wide duplicate removal, shared by every ingest worker.
//!
//! A page is a duplicate of an earlier one when it has the same canonical URL, the same token
//! sequence, or a SimHash within 3 bits. The survivor is chosen by `Key`, never by arrival
//! order: the copy on the higher-authority domain wins, then the shorter URL. When a later page
//! beats a copy that was already written, the old copy is tombstoned rather than rewritten.

use crate::filter::{hash, Fingerprint};
use crate::segment::strip_scheme;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// (domain authority position, URL length): smaller is better.
pub type Key = (u32, u32);
/// (segment, document within the segment).
pub type DocRef = (u32, u32);

const MAX_SIM_DISTANCE: u32 = 3;
const URL_SALT: u64 = 0x5bd1_e995_7f4a_7c15;

struct Sim {
    fp: u64,
    key: Key,
    doc: DocRef,
    alive: bool,
}

#[derive(Default)]
struct State {
    exact: HashMap<u64, (Key, DocRef)>,
    sims: Vec<Sim>,
    /// SimHash split into four 16-bit bands. Two fingerprints within 3 bits of each other
    /// must agree on at least one band, so candidates only come from shared buckets.
    bands: HashMap<u64, Vec<u32>>,
    deleted: Vec<DocRef>,
}

#[derive(Default)]
pub struct Dedup {
    state: Mutex<State>,
    rejected: AtomicU64,
}

fn band_keys(sim: u64) -> [u64; 4] {
    std::array::from_fn(|i| ((i as u64) << 16) | ((sim >> (16 * i)) & 0xffff))
}

impl Dedup {
    /// Returns false when a copy at least as good already exists, so `doc` should not be written.
    pub fn admit(&self, url: &str, fp: Fingerprint, key: Key, doc: DocRef) -> bool {
        let url_key = hash(strip_scheme(url).trim_start_matches("www.").as_bytes()) ^ URL_SALT;
        let exact_keys = [url_key, fp.exact];
        let bands = band_keys(fp.sim);
        let mut s = self.state.lock().unwrap();

        let mut losers = Vec::new();
        let mut dead_sims = Vec::new();
        for h in exact_keys {
            if let Some(&(k, d)) = s.exact.get(&h) {
                if k <= key {
                    return self.reject();
                }
                losers.push(d);
            }
        }
        for b in bands {
            for &i in s.bands.get(&b).into_iter().flatten() {
                let e = &s.sims[i as usize];
                if e.alive && (e.fp ^ fp.sim).count_ones() <= MAX_SIM_DISTANCE {
                    if e.key <= key {
                        return self.reject();
                    }
                    if !dead_sims.contains(&i) {
                        dead_sims.push(i);
                        losers.push(e.doc);
                    }
                }
            }
        }

        for h in exact_keys {
            s.exact.insert(h, (key, doc));
        }
        for i in dead_sims {
            s.sims[i as usize].alive = false;
        }
        let idx = s.sims.len() as u32;
        s.sims.push(Sim { fp: fp.sim, key, doc, alive: true });
        for b in bands {
            s.bands.entry(b).or_default().push(idx);
        }
        s.deleted.extend(losers);
        true
    }

    fn reject(&self) -> bool {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Tombstones grouped by segment, plus the number of pages rejected before being written.
    pub fn finish(self) -> (HashMap<u32, Vec<u32>>, u64) {
        let s = self.state.into_inner().unwrap();
        let mut by_seg: HashMap<u32, Vec<u32>> = HashMap::new();
        for (seg, doc) in s.deleted {
            by_seg.entry(seg).or_default().push(doc);
        }
        for docs in by_seg.values_mut() {
            docs.sort_unstable();
            docs.dedup();
        }
        (by_seg, self.rejected.into_inner())
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

//! Page quality heuristics and duplicate fingerprints.

use crate::tokenize::tokens;
use std::collections::HashSet;

const STOPWORDS: [&str; 8] = ["the", "be", "to", "of", "and", "that", "have", "with"];
/// SimHash only looks at the start of a page; that's enough to catch copies and keeps ingest fast.
const SIMHASH_TOKENS: usize = 3000;

pub struct Quality {
    pub keep: bool,
    /// 0.3..1.0, higher for pages that read like prose rather than menus and lists.
    pub score: f32,
}

pub fn assess(text: &str) -> Quality {
    let reject = Quality { keep: false, score: 0.0 };
    let (mut words, mut chars, mut alpha, mut stop) = (0usize, 0usize, 0usize, 0u32);
    for w in text.split_whitespace() {
        words += 1;
        chars += w.chars().count();
        if w.chars().any(char::is_alphabetic) {
            alpha += 1;
        }
        let core = w.trim_matches(|c: char| !c.is_alphanumeric());
        if core.len() <= 4 {
            if let Some(i) = STOPWORDS.iter().position(|s| s.eq_ignore_ascii_case(core)) {
                stop |= 1 << i;
            }
        }
    }
    if !(50..=100_000).contains(&words) {
        return reject;
    }
    let mean = chars as f32 / words as f32;
    if !(3.0..=10.0).contains(&mean) || (alpha as f32) < 0.8 * words as f32 || stop.count_ones() < 2 {
        return reject;
    }

    let (mut lines, mut ellipsis, mut dups) = (0usize, 0usize, 0usize);
    let (mut prose_bytes, mut total_bytes) = (0usize, 0usize);
    let mut seen = HashSet::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        lines += 1;
        if line.ends_with("...") || line.ends_with('…') {
            ellipsis += 1;
        }
        if !seen.insert(hash(line.as_bytes())) {
            dups += 1;
        }
        total_bytes += line.len();
        if line.split_whitespace().count() >= 6 && line.ends_with(['.', '!', '?', '"', '”']) {
            prose_bytes += line.len();
        }
    }
    if ellipsis * 10 > lines * 3 || dups * 10 > lines * 3 {
        return reject;
    }
    Quality { keep: true, score: 0.3 + 0.7 * prose_bytes as f32 / total_bytes.max(1) as f32 }
}

/// Titles of redirect, error and bot-check pages that the crawler saved as if they were content.
pub fn is_error_page(title: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "moved permanently",
        "302 found",
        "404",
        "page not found",
        "not found",
        "access denied",
        "just a moment",
        "attention required",
        "403 forbidden",
    ];
    let t = title.to_lowercase();
    MARKERS.iter().any(|m| t.contains(m))
}

#[derive(Clone, Copy)]
pub struct Fingerprint {
    /// Hash of the full token sequence: equal for pages that differ only in markup or spacing.
    pub exact: u64,
    /// 64-bit SimHash over word 3-shingles: near-duplicates differ in only a few bits.
    pub sim: u64,
}

pub fn fingerprint(text: &str) -> Fingerprint {
    let mut exact = 0xcbf2_9ce4_8422_2325u64;
    let mut votes = [0i32; 64];
    let (mut a, mut b, mut n) = (0u64, 0u64, 0usize);
    tokens(text, &mut |t| {
        let h = hash(t.as_bytes());
        exact = mix(exact ^ h).wrapping_add(h);
        if (2..SIMHASH_TOKENS).contains(&n) {
            let shingle = mix(a.rotate_left(42) ^ b.rotate_left(21) ^ h);
            for (i, v) in votes.iter_mut().enumerate() {
                *v += if (shingle >> i) & 1 == 1 { 1 } else { -1 };
            }
        }
        (a, b, n) = (b, h, n + 1);
    });
    let mut sim = 0u64;
    for (i, &v) in votes.iter().enumerate() {
        if v > 0 {
            sim |= 1 << i;
        }
    }
    Fingerprint { exact, sim }
}

/// FNV-1a, finished with a 64-bit mixer so every output bit is usable.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in bytes {
        h ^= x as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    mix(h)
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

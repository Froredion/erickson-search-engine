//! Domain authority from a link-graph ranking: one line per domain, best first.
//!
//! Accepts the Common Crawl domain-ranks format (`#harmonicc_pos ... #host_rev ...`, domains
//! reversed like `com.example`) or a two-column `pos<TAB>reversed-domain` file, gzipped or not.

use crate::segment::strip_scheme;
use flate2::read::MultiGzDecoder;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

/// Second-level labels under country TLDs that act like TLDs themselves (bbc.co.uk).
const SECOND_LEVEL: [&str; 12] = ["co", "com", "org", "net", "ac", "gov", "edu", "ne", "or", "gob", "go", "nic"];

#[derive(Default)]
pub struct Ranks {
    map: HashMap<Box<str>, u32>,
    worst: u32,
}

impl Ranks {
    pub fn load(path: &Path) -> io::Result<Self> {
        let f = File::open(path)?;
        let r: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "gz") {
            Box::new(BufReader::new(MultiGzDecoder::new(f)))
        } else {
            Box::new(BufReader::new(f))
        };
        let mut ranks = Self::default();
        for line in r.lines() {
            let line = line?;
            if line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            let (pos, rev) = match cols.len() {
                2 => (cols[0], cols[1]),
                n if n >= 5 => (cols[0], cols[4]),
                _ => continue,
            };
            let Ok(pos) = pos.parse::<u32>() else { continue };
            let domain: Vec<&str> = rev.split('.').rev().collect();
            ranks.map.insert(domain.join(".").into_boxed_str(), pos);
            ranks.worst = ranks.worst.max(pos);
        }
        Ok(ranks)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Rank position of a domain; `u32::MAX` when it isn't in the list.
    pub fn position(&self, domain: &str) -> u32 {
        self.map.get(domain).copied().unwrap_or(u32::MAX)
    }

    /// 0..1, log-scaled so the gap between #1 and #1000 matters more than #1M and #2M.
    pub fn authority(&self, pos: u32) -> f32 {
        if pos == u32::MAX || self.worst == 0 {
            return 0.0;
        }
        1.0 - (pos as f32).ln() / (self.worst as f32 + 1.0).ln()
    }
}

/// Host without scheme, port or a leading "www.".
pub fn host(url: &str) -> &str {
    let s = strip_scheme(url);
    let h = &s[..s.find(['/', '?', '#']).unwrap_or(s.len())];
    let h = h.split(':').next().unwrap_or(h);
    h.strip_prefix("www.").unwrap_or(h)
}

/// Platforms where anyone can publish under a subdomain. A site there is its own domain: it must
/// not inherit the platform's authority.
const SHARED_HOSTS: [&str; 24] = [
    "blogspot.com", "github.io", "gitlab.io", "netlify.app", "vercel.app", "lovable.app", "pages.dev",
    "workers.dev", "wordpress.com", "wixsite.com", "weebly.com", "herokuapp.com", "web.app",
    "firebaseapp.com", "tumblr.com", "medium.com", "substack.com", "glitch.me", "replit.app",
    "onrender.com", "fly.dev", "azurewebsites.net", "amazonaws.com", "digitaloceanspaces.com",
];

/// Registrable domain, approximately: `news.bbc.co.uk` -> `bbc.co.uk`, `a.b.example.com` -> `example.com`.
pub fn domain(url: &str) -> &str {
    let h = host(url);
    if SHARED_HOSTS.iter().any(|s| h.len() > s.len() && h.ends_with(s) && h.as_bytes()[h.len() - s.len() - 1] == b'.') {
        return h;
    }
    let mut labels = h.rsplit('.');
    let tld = labels.next().unwrap_or("");
    let sld = labels.next().unwrap_or("");
    let take = if tld.len() == 2 && SECOND_LEVEL.contains(&sld) { 3 } else { 2 };
    let (mut start, mut end) = (0, h.len());
    for _ in 0..take {
        match h[..end].rfind('.') {
            Some(i) => (start, end) = (i + 1, i),
            None => return h,
        }
    }
    &h[start..]
}

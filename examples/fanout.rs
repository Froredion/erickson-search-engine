//! Simulates a query fanned out to N index nodes, from per-query timings measured on one node.
//!
//!   cargo run --release --example fanout -- --score S.txt --rest R.txt --jitter A.txt,B.txt \
//!       [--nodes 100] [--scale 1.24] [--skew 0,0.1,0.2,0.3] [--net 1] [--trials 20]
//!
//! Every file is the output of `erickson eval` over the same query set. For each query, one node
//! takes `score * scale * skew + rest` ms, times a run-to-run jitter factor:
//! - `score` is the query's scoring time from S; `scale` grows it to a bigger node (scoring is
//!   linear in postings, so 100M / 80.8M = 1.24 for a 100M-page node measured at 80.8M).
//! - `rest` is lookup + rerank from R; it doesn't grow with the node (rerank sees 300 candidates).
//! - `skew` is how much more or less of the query's work a node's slice holds: lognormal with
//!   mean 1 and the given sigma, drawn per node. 0 means every slice is like the measured one.
//! - jitter is drawn from the measured per-query ratios of scoring time between A and B (the same
//!   code on two separate machines), split evenly between the two runs.
//! The fanned-out query takes the slowest node plus `net` ms for the round trip and merge.
use std::collections::HashMap;
use std::fs;

struct Timing {
    score: f64,
    rest: f64,
}

/// Parses `erickson eval` output: query -> phase times.
fn load(path: &str) -> HashMap<String, Timing> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = strip_ansi(line);
        let Some((q, rest)) = line.split_once("  (") else { continue };
        if q.starts_with(' ') || q.is_empty() {
            continue;
        }
        let num = |key: &str| -> Option<f64> {
            let at = rest.find(key)? + key.len();
            rest[at..].split(|c: char| c != '.' && !c.is_ascii_digit()).next()?.parse().ok()
        };
        if let (Some(l), Some(s), Some(r)) = (num("lookup "), num("score "), num("rerank ")) {
            out.insert(q.to_string(), Timing { score: s, rest: l + r });
        }
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            chars.by_ref().find(|&c| c == 'm');
        } else {
            out.push(c);
        }
    }
    out
}

/// xorshift64*: deterministic, good enough for a simulation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-12), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).map(String::as_str);
    let need = |k: &str| get(k).unwrap_or_else(|| panic!("missing {k}"));
    let nodes: usize = get("--nodes").unwrap_or("100").parse().unwrap();
    let scale: f64 = get("--scale").unwrap_or("1.24").parse().unwrap();
    let net: f64 = get("--net").unwrap_or("1").parse().unwrap();
    let trials: usize = get("--trials").unwrap_or("20").parse().unwrap();
    let skews: Vec<f64> = get("--skew").unwrap_or("0,0.1,0.2,0.3").split(',').map(|s| s.parse().unwrap()).collect();

    let score = load(need("--score"));
    let rest = load(need("--rest"));
    let (ja, jb) = need("--jitter").split_once(',').expect("--jitter A,B");
    let (ja, jb) = (load(ja), load(jb));
    // One run's factor is the square root of the ratio between two runs; both directions are kept.
    let jitter: Vec<f64> = ja
        .iter()
        .filter_map(|(q, a)| jb.get(q).filter(|b| a.score > 1.0 && b.score > 1.0).map(|b| (a.score / b.score).sqrt()))
        .flat_map(|r| [r, 1.0 / r])
        .collect();
    let queries: Vec<(f64, f64)> = score.iter().filter_map(|(q, s)| rest.get(q).map(|r| (s.score, r.rest))).collect();
    assert!(!queries.is_empty() && !jitter.is_empty(), "no queries in common between the files");

    let mut single: Vec<f64> = queries.iter().map(|&(s, r)| s * scale + r).collect();
    single.sort_by(f64::total_cmp);
    let mut j = jitter.clone();
    j.sort_by(f64::total_cmp);
    println!(
        "{} queries; jitter p1 {:.2} / p50 {:.2} / p99 {:.2} ({} samples); one {:.0}M node: p50 {:.0} ms, p99 {:.0} ms",
        queries.len(),
        pct(&j, 0.01),
        pct(&j, 0.5),
        pct(&j, 0.99),
        jitter.len(),
        80.8 * scale,
        pct(&single, 0.5),
        pct(&single, 0.99)
    );
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for &sigma in &skews {
        let mut fanned = Vec::with_capacity(queries.len() * trials);
        for &(s, r) in &queries {
            for _ in 0..trials {
                let slowest = (0..nodes)
                    .map(|_| {
                        let skew = (sigma * rng.normal() - sigma * sigma / 2.0).exp();
                        let jit = jitter[(rng.next() * jitter.len() as f64) as usize];
                        (s * scale * skew + r) * jit
                    })
                    .fold(0.0, f64::max);
                fanned.push(slowest + net);
            }
        }
        fanned.sort_by(f64::total_cmp);
        println!(
            "{nodes} nodes, skew sigma {sigma:.2}: p50 {:.0} ms, p90 {:.0} ms, p99 {:.0} ms, p99.9 {:.0} ms",
            pct(&fanned, 0.5),
            pct(&fanned, 0.9),
            pct(&fanned, 0.99),
            pct(&fanned, 0.999)
        );
    }
}

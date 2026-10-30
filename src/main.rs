mod dedup;
mod filter;
mod ingest;
mod ranks;
mod search;
mod segment;
mod server;
mod stem;
mod tokenize;
mod wet;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

const USAGE: &str = "usage:
  erickson ingest  --index DIR [--ranks FILE] [--lang eng|any] [--files-per-segment N] [--threads N]
                   [--resume] [--no-dedup] [--no-publish] [--keep 2]
                   (WET_FILES... | --paths wet.paths [--limit N] [--fetch https|s3] [--work DIR])
  erickson ingest-tsv --index DIR [--shard-size 100000] [--threads N] PASSAGES.tsv   (id<TAB>text per line)
  erickson bench   --index DIR --queries QUERIES.tsv --qrels QRELS [--rel 1] [--run OUT.run] [--exhaustive]
  erickson serve   --index DIR [--port 8080]
  erickson query   --index DIR QUERY...
  erickson eval    --index DIR QUERIES_FILE [--exhaustive]
  erickson merge   --index DIR [--segments 16] [--threads N] [--no-publish] [--keep 2]
  erickson publish --index DIR GENERATION
  erickson stats   --index DIR";

/// Flags that take no value.
const SWITCHES: [&str; 4] = ["no-publish", "resume", "no-dedup", "exhaustive"];

pub struct Args {
    flags: HashMap<String, String>,
    pos: Vec<String>,
}

impl Args {
    fn parse(args: &[String]) -> Self {
        let (mut flags, mut pos, mut i) = (HashMap::new(), Vec::new(), 0);
        while i < args.len() {
            match args[i].strip_prefix("--") {
                Some(k) if SWITCHES.contains(&k) => {
                    flags.insert(k.to_owned(), String::new());
                    i += 1;
                }
                Some(k) => {
                    flags.insert(k.to_owned(), args.get(i + 1).cloned().unwrap_or_default());
                    i += 2;
                }
                None => {
                    pos.push(args[i].clone());
                    i += 1;
                }
            }
        }
        Self { flags, pos }
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(String::as_str)
    }

    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }

    fn index(&self) -> io::Result<PathBuf> {
        self.get("index").map(PathBuf::from).ok_or_else(|| io::Error::other(format!("missing --index\n{USAGE}")))
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(argv.get(1..).unwrap_or(&[]));
    let r = match argv.first().map(String::as_str) {
        Some("ingest") => ingest::wet(&a),
        Some("ingest-tsv") => ingest::tsv(&a),
        Some("merge") => ingest::merge(&a),
        Some("bench") => bench(&a),
        Some("serve") => a.index().and_then(|root| {
            let port = a.get("port").and_then(|p| p.parse().ok()).unwrap_or(8080);
            server::serve(search::Index::open(&root)?, port)
        }),
        Some("query") => query(&a),
        Some("eval") => eval(&a),
        Some("publish") => a.index().and_then(|root| {
            let generation = a.pos.first().ok_or_else(|| io::Error::other("which generation?"))?;
            segment::publish(&root, generation)?;
            eprintln!("published {generation}");
            Ok(())
        }),
        Some("stats") => stats(&a),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1)
    }
}

/// Scores ranking variants against relevance judgments (TREC or MS MARCO qrels): nDCG@10 with
/// graded gains, MRR@10 counting grades >= `--rel` as relevant.
fn bench(a: &Args) -> io::Result<()> {
    let ix = search::Index::open(&a.index()?)?;
    let need = |k: &str| a.get(k).ok_or_else(|| io::Error::other(format!("missing --{k}\n{USAGE}")));
    let rel_min: u32 = a.get("rel").and_then(|r| r.parse().ok()).unwrap_or(1);

    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in fs::read_to_string(need("qrels")?)?.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if let [qid, _, pid, rel] = f[..] {
            qrels.entry(qid.to_owned()).or_default().insert(pid.to_owned(), rel.parse().unwrap_or(0));
        }
    }
    let queries: Vec<(String, String)> = fs::read_to_string(need("queries")?)?
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(qid, _)| qrels.contains_key(*qid))
        .map(|(qid, q)| (qid.to_owned(), q.to_owned()))
        .collect();
    println!("{} judged queries, {} pages in index\n", queries.len(), ix.n_docs);

    let off = search::Opts { coverage: false, rerank: false, per_host: None, snippets: false, exhaustive: a.has("exhaustive") };
    let variants = [
        ("BM25 only", off),
        ("+ coverage", search::Opts { coverage: true, ..off }),
        ("+ rerank (full)", search::Opts { coverage: true, rerank: true, ..off }),
    ];
    println!("{:<18} {:>9} {:>9} {:>9}", "variant", "nDCG@10", "MRR@10", "p50 ms");
    for (vi, (name, opts)) in variants.iter().enumerate() {
        let (mut ndcg, mut mrr, mut lat) = (0.0, 0.0, Vec::new());
        let mut run = String::new();
        for (qid, q) in &queries {
            let r = ix.search_with(q, 10, 0, *opts);
            lat.push(r.micros);
            let rels = &qrels[qid];
            let ranked: Vec<&str> = r.hits.iter().map(|h| h.url.as_str()).collect();
            ndcg += ndcg_at_10(&ranked, rels);
            if let Some(i) = ranked.iter().position(|p| rels.get(*p).is_some_and(|&g| g >= rel_min)) {
                mrr += 1.0 / (i + 1) as f64;
            }
            for (i, pid) in ranked.iter().enumerate() {
                run.push_str(&format!("{qid} Q0 {pid} {} {} erickson\n", i + 1, 10 - i));
            }
        }
        lat.sort_unstable();
        let n = queries.len().max(1) as f64;
        println!("{name:<18} {:>9.4} {:>9.4} {:>9.1}", ndcg / n, mrr / n, lat.get(lat.len() / 2).copied().unwrap_or(0) as f64 / 1000.0);
        if let (Some(path), true) = (a.get("run"), vi == variants.len() - 1) {
            fs::write(path, run)?;
        }
    }
    Ok(())
}

fn ndcg_at_10(ranked: &[&str], rels: &HashMap<String, u32>) -> f64 {
    let discount = |i: usize| ((i + 2) as f64).log2();
    let dcg: f64 = ranked.iter().take(10).enumerate().map(|(i, p)| rels.get(*p).copied().unwrap_or(0) as f64 / discount(i)).sum();
    let mut ideal: Vec<u32> = rels.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal.iter().take(10).enumerate().map(|(i, &g)| g as f64 / discount(i)).sum();
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

fn query(a: &Args) -> io::Result<()> {
    let ix = search::Index::open(&a.index()?)?;
    let q = a.pos.join(" ");
    let r = ix.search(&q, 10, 0);
    println!("{} matches in {:.2} ms\n", r.matched, r.micros as f64 / 1000.0);
    for (i, h) in r.hits.iter().enumerate() {
        let snippet = h
            .snippet
            .replace("<b>", "\x1b[1m")
            .replace("</b>", "\x1b[0m")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        println!("{:>2}. \x1b[1m{}\x1b[0m\n    \x1b[32m{}\x1b[0m\n    {}\n", i + 1, h.title, h.url, snippet);
    }
    Ok(())
}

/// Runs a query set and prints the top 3 per query plus latency percentiles.
fn eval(a: &Args) -> io::Result<()> {
    let ix = search::Index::open(&a.index()?)?;
    let file = a.pos.first().ok_or_else(|| io::Error::other("which queries file?"))?;
    let mut latencies = Vec::new();
    let (mut phase_micros, mut phase_faults) = ([0u128; 3], [0u64; 3]);
    for line in fs::read_to_string(file)?.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let q = line.strip_prefix("nav:").unwrap_or(line).trim();
        let r = ix.search_with(q, 3, 0, search::Opts { exhaustive: a.has("exhaustive"), ..Default::default() });
        latencies.push(r.micros);
        let [lookup, first, rest] = r.phase_micros.map(|us| us as f64 / 1000.0);
        let [fl, ff, fr] = r.phase_faults;
        println!(
            "\x1b[1m{q}\x1b[0m  ({:.1} ms, {} matches; lookup {lookup:.1} / score {first:.1} / rerank {rest:.1} ms; faults {fl} / {ff} / {fr})",
            r.micros as f64 / 1000.0,
            r.matched
        );
        for i in 0..3 {
            phase_micros[i] += r.phase_micros[i];
            phase_faults[i] += r.phase_faults[i];
        }
        for h in &r.hits {
            println!("    {:<60}  \x1b[32m{}\x1b[0m", segment::truncate(&h.title, 60), h.url);
        }
    }
    latencies.sort_unstable();
    let pct = |p: usize| latencies.get((latencies.len() * p / 100).min(latencies.len().saturating_sub(1))).copied().unwrap_or(0) as f64 / 1000.0;
    let n = latencies.len().max(1);
    let [lookup, first, rest] = phase_micros.map(|us| us as f64 / 1000.0 / n as f64);
    let [fl, ff, fr] = phase_faults.map(|f| f / n as u64);
    println!("\nmean per query: lookup {lookup:.1} / score {first:.1} / rerank {rest:.1} ms; faults {fl} / {ff} / {fr}");
    println!("{} queries: p50 {:.1} ms, p99 {:.1} ms", latencies.len(), pct(50), pct(99));
    Ok(())
}

fn stats(a: &Args) -> io::Result<()> {
    let ix = search::Index::open(&a.index()?)?;
    println!(
        "generation {}\nsegments   {}\npages      {}\nreplaced   {}",
        ix.generation,
        ix.n_segments(),
        ix.n_docs - ix.n_deleted,
        ix.n_deleted
    );
    Ok(())
}

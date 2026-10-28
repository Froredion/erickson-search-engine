mod dedup;
mod filter;
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use dedup::Dedup;
use ranks::Ranks;
use segment::SegmentWriter;

const USAGE: &str = "usage:
  erickson ingest  --index DIR [--ranks FILE] [--threads N] [--lang eng|any] [--no-publish] WET_FILES...
  erickson ingest-tsv --index DIR [--shard-size 100000] [--threads N] PASSAGES.tsv   (id<TAB>text per line)
  erickson bench   --index DIR --queries QUERIES.tsv --qrels QRELS [--rel 1] [--run OUT.run]
  erickson serve   --index DIR [--port 8080]
  erickson query   --index DIR QUERY...
  erickson eval    --index DIR QUERIES_FILE
  erickson publish --index DIR GENERATION
  erickson stats   --index DIR";

struct Args {
    flags: HashMap<String, String>,
    pos: Vec<String>,
}

impl Args {
    fn parse(args: &[String]) -> Self {
        let (mut flags, mut pos, mut i) = (HashMap::new(), Vec::new(), 0);
        while i < args.len() {
            match args[i].strip_prefix("--") {
                Some("no-publish") => {
                    flags.insert("no-publish".to_owned(), String::new());
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

    fn index(&self) -> io::Result<PathBuf> {
        self.get("index").map(PathBuf::from).ok_or_else(|| io::Error::other(format!("missing --index\n{USAGE}")))
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(argv.get(1..).unwrap_or(&[]));
    let r = match argv.first().map(String::as_str) {
        Some("ingest") => ingest(&a),
        Some("ingest-tsv") => ingest_tsv(&a),
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

/// Builds a new index generation from WET files: one segment per file, deduplicated across all
/// of them, then publishes it unless `--no-publish`.
fn ingest(a: &Args) -> io::Result<()> {
    let root = a.index()?;
    let files: Vec<PathBuf> = a.pos.iter().map(PathBuf::from).collect();
    if files.is_empty() {
        return Err(io::Error::other(format!("no input files\n{USAGE}")));
    }
    let threads = threads(a);
    let lang = a.get("lang").unwrap_or("eng");
    let ranks = match a.get("ranks") {
        Some(p) => {
            let r = Ranks::load(Path::new(p))?;
            eprintln!("loaded authority ranks for {} domains", r.len());
            r
        }
        None => Ranks::default(),
    };

    let (generation, tmp) = begin_generation(&root)?;
    let dedup = Dedup::default();
    let next = AtomicUsize::new(0);
    let (seen, kept) = (AtomicU64::new(0), AtomicU64::new(0));
    let manifest = Mutex::new(Vec::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.min(files.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Relaxed);
                let Some(file) = files.get(i) else { break };
                match ingest_file(file, i as u32, &tmp, lang, &ranks, &dedup) {
                    Ok((n_seen, n_kept)) => {
                        seen.fetch_add(n_seen, Relaxed);
                        let total = kept.fetch_add(n_kept, Relaxed) + n_kept;
                        manifest.lock().unwrap().push((i, format!("{}\t{}\t{n_seen}\t{n_kept}", segment::seg_name(i as u32), file.display())));
                        eprintln!("[{}/{}] {} kept {n_kept}/{n_seen}  (total {total}, {:.0}s)", i + 1, files.len(), file.display(), t0.elapsed().as_secs_f32());
                    }
                    Err(e) => eprintln!("skipping {}: {e}", file.display()),
                }
            });
        }
    });

    let (tombstones, rejected) = dedup.finish();
    let mut replaced = 0;
    for (seg, docs) in &tombstones {
        let dir = tmp.join(segment::seg_name(*seg));
        if dir.join("meta.txt").exists() {
            segment::write_tombstones(&dir, docs)?;
            replaced += docs.len();
        }
    }
    let kept = kept.into_inner();
    eprintln!(
        "{generation}: {} pages indexed from {} records in {:.0}s ({rejected} duplicates skipped, {replaced} replaced by better copies)",
        kept as usize - replaced,
        seen.into_inner(),
        t0.elapsed().as_secs_f32()
    );
    end_generation(a, &root, &generation, &tmp, manifest.into_inner().unwrap())
}

fn threads(a: &Args) -> usize {
    a.get("threads")
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
}

/// Picks the next generation name and an empty staging directory for it.
fn begin_generation(root: &Path) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(root)?;
    let generation = segment::next_generation(root)?;
    let tmp = root.join(format!(".tmp-{generation}"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    Ok((generation, tmp))
}

/// Writes the manifest, moves the staging directory into place and publishes unless `--no-publish`.
fn end_generation(a: &Args, root: &Path, generation: &str, tmp: &Path, mut manifest: Vec<(usize, String)>) -> io::Result<()> {
    manifest.sort();
    let lines: Vec<String> = manifest.into_iter().map(|(_, l)| l).collect();
    fs::write(tmp.join("manifest.tsv"), lines.join("\n") + "\n")?;
    fs::rename(tmp, root.join(generation))?;
    if a.get("no-publish").is_none() {
        segment::publish(root, generation)?;
        eprintln!("published {generation}");
    }
    Ok(())
}

/// Builds a generation from `id<TAB>text` lines (e.g. the MS MARCO passage collection): no
/// filtering or dedup, the id stored as the URL, fixed-size shards.
fn ingest_tsv(a: &Args) -> io::Result<()> {
    let root = a.index()?;
    let file = a.pos.first().ok_or_else(|| io::Error::other(format!("no input file\n{USAGE}")))?;
    let shard_size: usize = a.get("shard-size").and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let threads = threads(a);
    let (generation, tmp) = begin_generation(&root)?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<(u32, Vec<String>)>(threads);
    let rx = Mutex::new(rx);
    let manifest = Mutex::new(Vec::new());
    let total = AtomicU64::new(0);
    let t0 = Instant::now();

    std::thread::scope(|s| -> io::Result<()> {
        for _ in 0..threads {
            s.spawn(|| loop {
                let Ok((seg, lines)) = rx.lock().unwrap().recv() else { break };
                match write_passages(&tmp, seg, &lines) {
                    Ok(n) => {
                        let sum = total.fetch_add(n, Relaxed) + n;
                        manifest.lock().unwrap().push((seg as usize, format!("{}\t{file}\t{n}\t{n}", segment::seg_name(seg))));
                        eprintln!("{} done ({sum} passages, {:.0}s)", segment::seg_name(seg), t0.elapsed().as_secs_f32());
                    }
                    Err(e) => eprintln!("{} failed: {e}", segment::seg_name(seg)),
                }
            });
        }
        let reader = io::BufReader::with_capacity(1 << 20, fs::File::open(file)?);
        let (mut batch, mut seg) = (Vec::with_capacity(shard_size), 0u32);
        for line in io::BufRead::lines(reader) {
            batch.push(line?);
            if batch.len() == shard_size {
                tx.send((seg, std::mem::take(&mut batch))).map_err(io::Error::other)?;
                seg += 1;
            }
        }
        if !batch.is_empty() {
            tx.send((seg, batch)).map_err(io::Error::other)?;
        }
        drop(tx);
        Ok(())
    })?;

    eprintln!("{generation}: {} passages indexed in {:.0}s", total.into_inner(), t0.elapsed().as_secs_f32());
    end_generation(a, &root, &generation, &tmp, manifest.into_inner().unwrap())
}

fn write_passages(dir: &Path, seg: u32, lines: &[String]) -> io::Result<u64> {
    let mut w = SegmentWriter::create(&dir.join(segment::seg_name(seg)))?;
    for line in lines {
        if let Some((id, text)) = line.split_once('\t') {
            w.add(id, "", text, 1.0, 0.0)?;
        }
    }
    let n = w.len() as u64;
    w.finish()?;
    Ok(n)
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

    let off = search::Opts { coverage: false, rerank: false, per_host: None, snippets: false };
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

fn ingest_file(path: &Path, seg: u32, dir: &Path, lang: &str, ranks: &Ranks, dedup: &Dedup) -> io::Result<(u64, u64)> {
    let mut w = SegmentWriter::create(&dir.join(segment::seg_name(seg)))?;
    let (mut seen, mut kept) = (0u64, 0u64);
    for rec in wet::open(path)? {
        seen += 1;
        if lang != "any" && rec.lang.as_deref().and_then(|l| l.split(',').next()) != Some(lang) {
            continue;
        }
        let (title, body) = match rec.body.split_once('\n') {
            Some((t, b)) if t.len() <= 300 && !b.trim().is_empty() => (t.trim(), b),
            _ => ("", rec.body.as_str()),
        };
        if filter::is_error_page(title) {
            continue;
        }
        let quality = filter::assess(body);
        if !quality.keep {
            continue;
        }
        let url = dedup::canonical_url(&rec.uri);
        let pos = ranks.position(ranks::domain(&url));
        let fp = filter::fingerprint(body);
        if !dedup.admit(&url, fp, (pos, url.len() as u32), (seg, w.len())) {
            continue;
        }
        w.add(&url, title, body, quality.score, ranks.authority(pos))?;
        kept += 1;
    }
    if kept > 0 {
        w.finish()?;
    } else {
        w.abandon()?;
    }
    Ok((seen, kept))
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
    for line in fs::read_to_string(file)?.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let q = line.strip_prefix("nav:").unwrap_or(line).trim();
        let r = ix.search(q, 3, 0);
        latencies.push(r.micros);
        println!("\x1b[1m{q}\x1b[0m  ({:.1} ms, {} matches)", r.micros as f64 / 1000.0, r.matched);
        for h in &r.hits {
            println!("    {:<60}  \x1b[32m{}\x1b[0m", segment::truncate(&h.title, 60), h.url);
        }
    }
    latencies.sort_unstable();
    let pct = |p: usize| latencies.get((latencies.len() * p / 100).min(latencies.len().saturating_sub(1))).copied().unwrap_or(0) as f64 / 1000.0;
    println!("\n{} queries: p50 {:.1} ms, p99 {:.1} ms", latencies.len(), pct(50), pct(99));
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

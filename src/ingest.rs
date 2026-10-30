//! Building index generations, from Common Crawl WET files (local, or fetched one at a time from
//! S3/HTTPS and deleted once indexed) or from `id<TAB>text` passage files.
//!
//! Workers never coordinate while indexing: each owns whole segments, and duplicates are removed
//! afterwards by one batch pass over the fingerprints. A segment is complete once its `meta.txt`
//! exists, so an interrupted build continues with `--resume` and redoes only unfinished segments.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ranks::{self, Ranks};
use crate::segment::{self, SegmentWriter};
use crate::{dedup, filter, search, wet, Args, USAGE};

enum Input {
    Local(PathBuf),
    /// A key in the Common Crawl bucket, e.g. `crawl-data/CC-MAIN-.../wet/....warc.wet.gz`.
    Remote(String),
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

pub fn threads(a: &Args, per_core: usize) -> usize {
    a.get("threads").and_then(|t| t.parse().ok()).unwrap_or_else(|| cores() * per_core)
}

/// `ingest`: WET files to a new generation, deduplicated and published.
pub fn wet(a: &Args) -> io::Result<()> {
    let root = a.index()?;
    let inputs: Vec<Input> = match a.get("paths") {
        Some(list) => {
            let limit = a.get("limit").and_then(|l| l.parse().ok()).unwrap_or(usize::MAX);
            fs::read_to_string(list)?
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .take(limit)
                .map(|l| Input::Remote(l.to_owned()))
                .collect()
        }
        None => a.pos.iter().map(|p| Input::Local(PathBuf::from(p))).collect(),
    };
    if inputs.is_empty() {
        return Err(io::Error::other(format!("no input files\n{USAGE}")));
    }
    let fetch = a.get("fetch").unwrap_or("https");
    if !matches!(fetch, "https" | "s3") {
        return Err(io::Error::other("--fetch must be https or s3"));
    }
    let work = PathBuf::from(a.get("work").unwrap_or("data/work"));
    fs::create_dir_all(&work)?;
    // Remote workers spend part of their time waiting on downloads, so run more of them.
    let threads = threads(a, if matches!(inputs[0], Input::Remote(_)) { 2 } else { 1 });
    let per_segment: usize = a.get("files-per-segment").and_then(|n| n.parse().ok()).unwrap_or(1).max(1);
    let lang = a.get("lang").unwrap_or("eng");
    let ranks = match a.get("ranks") {
        Some(p) => {
            let r = Ranks::load(Path::new(p))?;
            eprintln!("loaded authority ranks for {} domains", r.len());
            r
        }
        None => Ranks::default(),
    };

    let (generation, tmp) = begin_generation(&root, a.has("resume"))?;
    let groups: Vec<&[Input]> = inputs.chunks(per_segment).collect();
    eprintln!("{generation}: {} inputs in {} segments, {threads} workers", inputs.len(), groups.len());
    let next = AtomicUsize::new(0);
    let (seen, kept, done, resumed) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let failed = Mutex::new(Vec::new());
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.min(groups.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Relaxed);
                let Some(group) = groups.get(i) else { break };
                let name = segment::seg_name(i as u32);
                let dir = tmp.join(&name);
                if dir.join("meta.txt").exists() {
                    resumed.fetch_add(1, Relaxed);
                    continue;
                }
                if dir.exists() {
                    let _ = fs::remove_dir_all(&dir);
                }
                match ingest_segment(group, &dir, lang, &ranks, fetch, &work) {
                    Ok((n_seen, n_kept)) => {
                        seen.fetch_add(n_seen, Relaxed);
                        let total = kept.fetch_add(n_kept, Relaxed) + n_kept;
                        let d = done.fetch_add(1, Relaxed) + 1;
                        let secs = t0.elapsed().as_secs_f32();
                        eprintln!("[{d}/{}] {name} kept {n_kept}/{n_seen}  (total {total}, {:.0} pages/s, {secs:.0}s)", groups.len(), total as f32 / secs);
                    }
                    Err(e) => {
                        eprintln!("{name} failed: {e}");
                        failed.lock().unwrap().push(name);
                    }
                }
            });
        }
    });

    let resumed = resumed.into_inner();
    if resumed > 0 {
        eprintln!("{resumed} segments were already complete from the interrupted run");
    }
    eprintln!("indexed {} pages from {} records in {:.0}s", kept.into_inner(), seen.into_inner(), t0.elapsed().as_secs_f32());
    let failed = failed.into_inner().unwrap();
    if !failed.is_empty() {
        return Err(io::Error::other(format!(
            "{} segments failed ({}...); rerun the same command with --resume to retry them",
            failed.len(),
            failed[0]
        )));
    }
    if !a.has("no-dedup") {
        let t = Instant::now();
        let r = dedup::run(&tmp, cores())?;
        eprintln!("dedup: removed {} of {} pages as duplicates ({:.0}s)", r.removed, r.pages, t.elapsed().as_secs_f32());
    }
    end_generation(a, &root, &generation, &tmp)
}

fn ingest_segment(group: &[Input], dir: &Path, lang: &str, ranks: &Ranks, fetch: &str, work: &Path) -> io::Result<(u64, u64)> {
    let mut w = SegmentWriter::create(dir)?;
    let mut fingerprints = Vec::new();
    let mut seen = 0;
    for input in group {
        let (path, downloaded) = match input {
            Input::Local(p) => (p.clone(), false),
            Input::Remote(key) => (download(key, fetch, work)?, true),
        };
        let r = index_file(&path, &mut w, &mut fingerprints, lang, ranks);
        if downloaded {
            let _ = fs::remove_file(&path);
        }
        seen += r?;
    }
    let kept = w.len() as u64;
    if kept == 0 {
        w.abandon()?;
        return Ok((seen, 0));
    }
    fs::write(dir.join("fingerprints.bin"), &fingerprints)?;
    w.finish()?;
    Ok((seen, kept))
}

fn index_file(path: &Path, w: &mut SegmentWriter, fingerprints: &mut Vec<u8>, lang: &str, ranks: &Ranks) -> io::Result<u64> {
    let mut seen = 0;
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
        dedup::push_record(fingerprints, &url, filter::fingerprint(body), pos);
        w.add(&url, title, body, quality.score, ranks.authority(pos))?;
    }
    Ok(seen)
}

/// Fetches one Common Crawl file into `work`, retrying with backoff.
fn download(key: &str, fetch: &str, work: &Path) -> io::Result<PathBuf> {
    let local = work.join(key.rsplit('/').next().unwrap_or(key));
    let part = local.with_extension("part");
    for attempt in 0..5u32 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
        }
        let status = match fetch {
            "s3" => Command::new("aws")
                .args(["s3", "cp", "--only-show-errors"])
                .arg(format!("s3://commoncrawl/{key}"))
                .arg(&part)
                .status(),
            _ => Command::new("curl")
                .args(["-fsSL", "--retry", "3", "-o"])
                .arg(&part)
                .arg(format!("https://data.commoncrawl.org/{key}"))
                .status(),
        };
        if status.is_ok_and(|s| s.success()) {
            fs::rename(&part, &local)?;
            return Ok(local);
        }
    }
    let _ = fs::remove_file(&part);
    Err(io::Error::other(format!("could not fetch {key}")))
}

/// `ingest-tsv`: `id<TAB>text` lines (e.g. the MS MARCO passage collection) to a new generation.
/// No filtering or dedup; the id is stored as the URL; fixed-size shards.
pub fn tsv(a: &Args) -> io::Result<()> {
    let root = a.index()?;
    let file = a.pos.first().ok_or_else(|| io::Error::other(format!("no input file\n{USAGE}")))?;
    let shard_size: usize = a.get("shard-size").and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let threads = threads(a, 1);
    let (generation, tmp) = begin_generation(&root, false)?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<(u32, Vec<String>)>(threads);
    let rx = Mutex::new(rx);
    let total = AtomicU64::new(0);
    let t0 = Instant::now();

    std::thread::scope(|s| -> io::Result<()> {
        for _ in 0..threads {
            s.spawn(|| loop {
                let Ok((seg, lines)) = rx.lock().unwrap().recv() else { break };
                match write_passages(&tmp, seg, &lines) {
                    Ok(n) => {
                        let sum = total.fetch_add(n, Relaxed) + n;
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
    end_generation(a, &root, &generation, &tmp)
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

/// `merge`: rewrites the published generation as `--segments` (default 16) larger segments, each
/// a run of consecutive old ones, into a new generation. Docs are renumbered by page prior so
/// queries can stop early (see `segment::merge`); scores are unchanged, so results are too, up
/// to the order of exact ties. `--keep-order` keeps doc order, and with it tie order.
pub fn merge(a: &Args) -> io::Result<()> {
    let root = a.index()?;
    let from = segment::current(&root)?;
    let mut sources: Vec<(PathBuf, u64)> = Vec::new();
    for e in fs::read_dir(&from)? {
        let p = e?.path();
        if let Ok(meta) = fs::read_to_string(p.join("meta.txt")) {
            sources.push((p, meta.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)));
        }
    }
    sources.sort();
    let want: u64 = a.get("segments").and_then(|n| n.parse().ok()).unwrap_or(16).max(1);
    let total: u64 = sources.iter().map(|s| s.1).sum();
    // Cut wherever the running doc count passes the next multiple of total / want.
    let mut groups: Vec<Vec<PathBuf>> = vec![Vec::new()];
    let mut seen = 0;
    for (p, docs) in sources {
        if seen * want >= total * groups.len() as u64 && !groups.last().unwrap().is_empty() {
            groups.push(Vec::new());
        }
        seen += docs;
        groups.last_mut().unwrap().push(p);
    }

    let prior = (!a.has("keep-order")).then_some(search::prior as fn(f32, f32) -> f32);
    let (generation, tmp) = begin_generation(&root, false)?;
    let threads = threads(a, 1).min(groups.len());
    eprintln!("{generation}: merging {total} docs into {} segments, {threads} workers", groups.len());
    let t0 = Instant::now();
    let next = AtomicUsize::new(0);
    let failed = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let g = next.fetch_add(1, Relaxed);
                let Some(group) = groups.get(g) else { break };
                match segment::merge(&tmp.join(segment::seg_name(g as u32)), group, prior) {
                    Ok(n) => eprintln!("{} done ({} segments, {n} docs, {:.0}s)", segment::seg_name(g as u32), group.len(), t0.elapsed().as_secs_f32()),
                    Err(e) => {
                        eprintln!("{} failed: {e}", segment::seg_name(g as u32));
                        failed.lock().unwrap().get_or_insert(e);
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    eprintln!("merged in {:.0}s", t0.elapsed().as_secs_f32());
    end_generation(a, &root, &generation, &tmp)
}

/// The staging directory for a new generation, or with `resume` the newest unfinished one.
fn begin_generation(root: &Path, resume: bool) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(root)?;
    if resume {
        let mut staged: Vec<String> = fs::read_dir(root)?
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| n.starts_with(".tmp-gen-"))
            .collect();
        staged.sort();
        if let Some(name) = staged.pop() {
            let generation = name.trim_start_matches(".tmp-").to_owned();
            eprintln!("resuming {generation}");
            return Ok((generation, root.join(name)));
        }
    }
    let generation = segment::next_generation(root)?;
    let tmp = root.join(format!(".tmp-{generation}"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    Ok((generation, tmp))
}

/// Writes the manifest, moves the staging directory into place, then (unless `--no-publish`)
/// publishes it and deletes all but the newest `--keep` generations (default 2).
fn end_generation(a: &Args, root: &Path, generation: &str, tmp: &Path) -> io::Result<()> {
    let mut segs: Vec<PathBuf> = fs::read_dir(tmp)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("meta.txt").exists())
        .collect();
    segs.sort();
    let mut manifest = String::new();
    for p in segs {
        let docs = fs::read_to_string(p.join("meta.txt"))?.split_whitespace().next().unwrap_or("0").to_owned();
        manifest.push_str(&format!("{}\t{docs}\n", p.file_name().unwrap_or_default().to_string_lossy()));
    }
    fs::write(tmp.join("manifest.tsv"), manifest)?;
    fs::rename(tmp, root.join(generation))?;
    if !a.has("no-publish") {
        segment::publish(root, generation)?;
        eprintln!("published {generation}");
        let keep = a.get("keep").and_then(|k| k.parse().ok()).unwrap_or(2);
        for g in segment::prune(root, keep)? {
            eprintln!("removed old {g}");
        }
    }
    Ok(())
}

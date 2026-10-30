//! One shard of the index: an inverted index over the pages of one input file, plus the stored
//! fields needed to render results.
//!
//! Files in a segment directory:
//!   terms.bin     term bytes, concatenated in sorted order
//!   terms.idx     28 bytes per term: term offset u64, term len u32, df u32, postings offset u64, postings len u32
//!   terms.sample  every SAMPLE-th term, held in RAM by readers: count u32, count + 1 u32 offsets, term bytes
//!   postings.bin  per term: a block directory, then the blocks (see below)
//!   docmeta.bin   12 bytes per doc: body length u32, quality f32, authority f32
//!   store.bin     per doc: url, title, text, each as u32 length + bytes
//!   store.idx     u64 offset into store.bin per doc
//!   deleted.bin   u32 ids of docs removed by deduplication (optional)
//!   fingerprints.bin  dedup records, one per doc (see `dedup`)
//!   meta.txt      "<docs> <total body length> <format version>"
//!
//! A term's postings are split into blocks of 128 (doc, weighted tf) pairs, varint-encoded with
//! doc ids delta-coded. The directory in front holds 16 bytes per block: last doc id, payload
//! offset, max tf and min doc length in the block. Those last two bound the best BM25 score any
//! doc in the block can reach, whatever k1 and b are, which lets queries skip whole blocks
//! (block-max WAND) and jump straight to the block holding a given doc.
//!
//! Segments live in a generation directory (`gen-0001/`). A generation only becomes visible to
//! queries once it's complete and `CURRENT` points at it.

use memmap2::{Advice, Mmap};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::dedup;
use crate::tokenize::tokens;

const ENTRY: usize = 28;
const META: usize = 12;
pub const BLOCK: usize = 128;
const DIR_ENTRY: usize = 16;
const FORMAT: &str = "2";
const STORED_TEXT_BYTES: usize = 4096;
/// Every SAMPLE-th term of the dictionary is kept in memory, so a lookup binary-searches RAM and
/// then reads one small run of `terms.idx` instead of ~20 scattered entries.
const SAMPLE: usize = 64;
/// Term frequency weights per field: a word in the title or URL counts as several in the body.
const TITLE_WEIGHT: u32 = 3;
const URL_WEIGHT: u32 = 2;

pub fn seg_name(i: u32) -> String {
    format!("seg-{i:05}")
}

pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

pub fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url)
}

fn varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_varint(b: &[u8], i: &mut usize) -> u32 {
    let (mut v, mut shift) = (0u32, 0);
    while *i < b.len() {
        let x = b[*i];
        *i += 1;
        v |= ((x & 0x7f) as u32) << shift;
        if x & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    v
}

pub struct SegmentWriter {
    dir: PathBuf,
    postings: HashMap<Box<str>, Vec<u32>>,
    scratch: HashMap<String, u32>,
    meta: Vec<u8>,
    n_docs: u32,
    total_len: u64,
    store: BufWriter<File>,
    store_offsets: Vec<u64>,
    store_pos: u64,
}

impl SegmentWriter {
    pub fn create(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            postings: HashMap::new(),
            scratch: HashMap::new(),
            meta: Vec::new(),
            n_docs: 0,
            total_len: 0,
            store: BufWriter::with_capacity(1 << 20, File::create(dir.join("store.bin"))?),
            store_offsets: Vec::new(),
            store_pos: 0,
        })
    }

    pub fn len(&self) -> u32 {
        self.n_docs
    }

    pub fn add(&mut self, url: &str, title: &str, text: &str, quality: f32, authority: f32) -> io::Result<()> {
        let doc = self.n_docs;
        let mut len = 0u32;
        {
            let scratch = &mut self.scratch;
            let mut count = |t: &str, w: u32| match scratch.get_mut(t) {
                Some(c) => *c += w,
                None => {
                    scratch.insert(t.to_owned(), w);
                }
            };
            tokens(strip_scheme(url), &mut |t| count(t, URL_WEIGHT));
            tokens(title, &mut |t| count(t, TITLE_WEIGHT));
            tokens(text, &mut |t| {
                len += 1;
                count(t, 1)
            });
        }
        for (term, tf) in self.scratch.drain() {
            match self.postings.get_mut(term.as_str()) {
                Some(p) => p.extend_from_slice(&[doc, tf]),
                None => {
                    self.postings.insert(term.into_boxed_str(), vec![doc, tf]);
                }
            }
        }

        self.store_offsets.push(self.store_pos);
        for field in [url, truncate(title, 300), truncate(text, STORED_TEXT_BYTES)] {
            self.store.write_all(&(field.len() as u32).to_le_bytes())?;
            self.store.write_all(field.as_bytes())?;
            self.store_pos += 4 + field.len() as u64;
        }
        let len = len.max(1);
        self.meta.extend_from_slice(&len.to_le_bytes());
        self.meta.extend_from_slice(&quality.to_le_bytes());
        self.meta.extend_from_slice(&authority.to_le_bytes());
        self.n_docs += 1;
        self.total_len += len as u64;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.store.flush()?;
        let mut terms: Vec<(Box<str>, Vec<u32>)> = self.postings.drain().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut dict = DictWriter::create(&self.dir)?;
        for (term, p) in &terms {
            dict.add(term.as_bytes(), p, &self.meta)?;
        }
        dict.finish()?;
        fs::write(self.dir.join("docmeta.bin"), &self.meta)?;
        let offsets: Vec<u8> = self.store_offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        fs::write(self.dir.join("store.idx"), offsets)?;
        fs::write(self.dir.join("meta.txt"), format!("{} {} {FORMAT}\n", self.n_docs, self.total_len))
    }

    pub fn abandon(self) -> io::Result<()> {
        let dir = self.dir.clone();
        drop(self);
        fs::remove_dir_all(dir)
    }
}

/// Writes a segment's dictionary and postings, one term at a time in sorted order.
struct DictWriter {
    dir: PathBuf,
    blob: BufWriter<File>,
    idx: BufWriter<File>,
    post: BufWriter<File>,
    term_off: u64,
    post_off: u64,
    buf: Vec<u8>,
    payload: Vec<u8>,
    sample: Vec<u8>,
    sample_at: Vec<u32>,
    n_terms: usize,
}

impl DictWriter {
    fn create(dir: &Path) -> io::Result<Self> {
        let open = |name: &str| File::create(dir.join(name)).map(|f| BufWriter::with_capacity(1 << 20, f));
        Ok(Self {
            dir: dir.to_path_buf(),
            blob: open("terms.bin")?,
            idx: open("terms.idx")?,
            post: open("postings.bin")?,
            term_off: 0,
            post_off: 0,
            buf: Vec::new(),
            payload: Vec::new(),
            sample: Vec::new(),
            sample_at: vec![0],
            n_terms: 0,
        })
    }

    /// `pairs` is (doc, weighted tf) flattened, docs ascending; `meta` is the segment's docmeta.
    fn add(&mut self, term: &[u8], pairs: &[u32], meta: &[u8]) -> io::Result<()> {
        if self.n_terms % SAMPLE == 0 {
            self.sample.extend_from_slice(term);
            self.sample_at.push(self.sample.len() as u32);
        }
        self.n_terms += 1;
        let (buf, payload) = (&mut self.buf, &mut self.payload);
        buf.clear();
        payload.clear();
        let mut prev = 0;
        for block in pairs.chunks(2 * BLOCK) {
            let offset = payload.len() as u32;
            let (mut max_tf, mut min_dl) = (0u32, u32::MAX);
            for pair in block.chunks_exact(2) {
                let (doc, tf) = (pair[0], pair[1]);
                varint(payload, doc - prev);
                varint(payload, tf);
                prev = doc;
                max_tf = max_tf.max(tf);
                min_dl = min_dl.min(u32_at(meta, doc as usize * META));
            }
            for v in [prev, offset, max_tf, min_dl] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        buf.extend_from_slice(payload);
        self.blob.write_all(term)?;
        self.post.write_all(buf)?;
        self.idx.write_all(&self.term_off.to_le_bytes())?;
        self.idx.write_all(&(term.len() as u32).to_le_bytes())?;
        self.idx.write_all(&((pairs.len() / 2) as u32).to_le_bytes())?;
        self.idx.write_all(&self.post_off.to_le_bytes())?;
        self.idx.write_all(&(buf.len() as u32).to_le_bytes())?;
        self.term_off += term.len() as u64;
        self.post_off += buf.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        self.blob.flush()?;
        self.idx.flush()?;
        self.post.flush()?;
        fs::write(self.dir.join("terms.sample"), encode_sample(&self.sample_at, &self.sample))
    }
}

/// Writes one segment at `dir` holding every doc of `sources`. Tombstoned docs are kept and stay
/// tombstoned, so document frequencies and lengths, and with them every score, are unchanged.
///
/// With `prior`, docs are renumbered by it, highest first (ties in source order), so a query can
/// bound the prior of every doc after the one it's on by that doc's own prior. Without it, doc d
/// of the i-th source becomes d + (docs in the sources before it), which also keeps tie-breaks.
/// Reads the dictionaries and postings front to back; memory is one term's postings plus a few
/// bytes per doc. Returns the number of docs written.
pub fn merge(dir: &Path, sources: &[PathBuf], prior: Option<fn(f32, f32) -> f32>) -> io::Result<u32> {
    fs::create_dir_all(dir)?;
    let segs = sources.iter().map(|p| Segment::open(p)).collect::<io::Result<Vec<_>>>()?;
    let mut base = Vec::with_capacity(segs.len());
    let (mut n_docs, mut total_len) = (0u32, 0u64);
    for s in &segs {
        base.push(n_docs);
        n_docs = n_docs.checked_add(s.n_docs).ok_or_else(|| io::Error::other("merged segment would exceed 2^32 docs"))?;
        total_len += s.total_len;
        for m in [&s.terms, &s.idx, &s.postings] {
            let _ = m.advise(Advice::Sequential);
        }
    }

    // (source, doc) of every doc in its new order, and the new id of every old one.
    let mut order: Vec<(u32, u32)> = segs.iter().enumerate().flat_map(|(i, s)| (0..s.n_docs).map(move |d| (i as u32, d))).collect();
    if let Some(prior) = prior {
        let key = |&(i, d): &(u32, u32)| {
            let s = &segs[i as usize];
            prior(s.authority(d), s.quality(d))
        };
        order.sort_by(|x, y| key(y).total_cmp(&key(x)));
    }
    let mut new_id = vec![0u32; n_docs as usize];
    for (n, &(i, d)) in order.iter().enumerate() {
        new_id[(base[i as usize] + d) as usize] = n as u32;
    }

    let meta: Vec<u8> = order.iter().flat_map(|&(i, d)| {
        let o = d as usize * META;
        segs[i as usize].meta[o..o + META].iter().copied()
    }).collect();
    let mut deleted: Vec<u32> =
        order.iter().enumerate().filter(|(_, &(i, d))| segs[i as usize].deleted[d as usize]).map(|(n, _)| n as u32).collect();
    deleted.sort_unstable();
    if !deleted.is_empty() {
        write_tombstones(dir, &deleted)?;
    }

    // Stored pages and dedup records, in the new order. Reads the sources' stores front to back
    // unless renumbering shuffled them.
    let mut store = BufWriter::with_capacity(1 << 20, File::create(dir.join("store.bin"))?);
    let mut store_idx = BufWriter::with_capacity(1 << 20, File::create(dir.join("store.idx"))?);
    let mut store_pos = 0u64;
    let advice = if prior.is_some() { Advice::Random } else { Advice::Sequential };
    for s in &segs {
        let _ = s.store.advise(advice);
    }
    for &(i, d) in &order {
        let s = &segs[i as usize];
        let from = u64_at(&s.store_idx, d as usize * 8) as usize;
        let to = if d + 1 < s.n_docs { u64_at(&s.store_idx, (d as usize + 1) * 8) as usize } else { s.store.len() };
        store.write_all(&s.store[from..to])?;
        store_idx.write_all(&store_pos.to_le_bytes())?;
        store_pos += (to - from) as u64;
    }
    store.flush()?;
    store_idx.flush()?;
    let fps: Vec<Vec<u8>> = sources.iter().map(|p| fs::read(p.join("fingerprints.bin")).unwrap_or_default()).collect();
    if fps.iter().zip(&segs).all(|(f, s)| f.len() == s.n_docs as usize * dedup::RECORD) {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(dir.join("fingerprints.bin"))?);
        for &(i, d) in &order {
            let o = d as usize * dedup::RECORD;
            out.write_all(&fps[i as usize][o..o + dedup::RECORD])?;
        }
        out.flush()?;
    }
    drop(fps);

    // k-way merge of the sorted dictionaries. Ties pop in source order, so without renumbering
    // docs come out ascending already.
    let mut heap: BinaryHeap<Reverse<(&[u8], usize, usize)>> =
        segs.iter().enumerate().filter(|(_, s)| s.n_terms > 0).map(|(i, s)| Reverse((s.term(0), i, 0))).collect();
    let mut dict = DictWriter::create(dir)?;
    let (mut packed, mut pairs) = (Vec::<u64>::new(), Vec::new());
    let (mut docs, mut tfs) = ([0u32; BLOCK], [0u32; BLOCK]);
    while let Some(Reverse((term, _, _))) = heap.peek().copied() {
        packed.clear();
        while let Some(&Reverse((t, i, ti))) = heap.peek() {
            if t != term {
                break;
            }
            heap.pop();
            let p = segs[i].postings_at(ti);
            for b in 0..p.n_blocks() {
                let n = p.decode_block(b, &mut docs, &mut tfs);
                for k in 0..n {
                    packed.push((new_id[(base[i] + docs[k]) as usize] as u64) << 32 | tfs[k] as u64);
                }
            }
            if ti + 1 < segs[i].n_terms {
                heap.push(Reverse((segs[i].term(ti + 1), i, ti + 1)));
            }
        }
        if prior.is_some() {
            packed.sort_unstable();
        }
        pairs.clear();
        pairs.extend(packed.iter().flat_map(|&x| [(x >> 32) as u32, x as u32]));
        dict.add(term, &pairs, &meta)?;
    }
    dict.finish()?;
    fs::write(dir.join("docmeta.bin"), &meta)?;
    // Written last: a segment without meta.txt is unfinished.
    fs::write(dir.join("meta.txt"), format!("{n_docs} {total_len} {FORMAT}\n"))?;
    Ok(n_docs)
}

pub fn write_tombstones(seg_dir: &Path, docs: &[u32]) -> io::Result<()> {
    let bytes: Vec<u8> = docs.iter().flat_map(|d| d.to_le_bytes()).collect();
    fs::write(seg_dir.join("deleted.bin"), bytes)
}

fn encode_sample(at: &[u32], bytes: &[u8]) -> Vec<u8> {
    let mut out = ((at.len() - 1) as u32).to_le_bytes().to_vec();
    out.extend(at.iter().flat_map(|o| o.to_le_bytes()));
    out.extend_from_slice(bytes);
    out
}

fn decode_sample(b: &[u8]) -> Option<(Vec<u32>, Vec<u8>)> {
    let n = u32_at(b.get(..4)?, 0) as usize;
    let end = 4 + 4 * (n + 1);
    let at: Vec<u32> = b.get(4..end)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let bytes = b.get(end..)?.to_vec();
    (at.last().copied() == Some(bytes.len() as u32)).then_some((at, bytes))
}

fn map(path: &Path, advice: Advice) -> io::Result<Mmap> {
    let f = File::open(path)?;
    // Safety: segment files are written once and never modified while mapped.
    let m = unsafe { Mmap::map(&f) }?;
    // Only a hint, so failing to give it is harmless.
    let _ = m.advise(advice);
    Ok(m)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn f32_at(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn read_field<'a>(store: &'a [u8], o: &mut usize) -> &'a str {
    let n = u32_at(store, *o) as usize;
    let s = std::str::from_utf8(&store[*o + 4..*o + 4 + n]).unwrap_or("");
    *o += 4 + n;
    s
}

#[derive(Clone, Copy)]
pub struct Postings<'a> {
    pub df: u32,
    data: &'a [u8],
}

impl Postings<'_> {
    pub fn n_blocks(&self) -> usize {
        (self.df as usize).div_ceil(BLOCK)
    }

    pub fn last_doc(&self, b: usize) -> u32 {
        u32_at(self.data, b * DIR_ENTRY)
    }

    /// (max weighted tf, min doc length) over block `b`.
    pub fn block_limits(&self, b: usize) -> (u32, u32) {
        (u32_at(self.data, b * DIR_ENTRY + 8), u32_at(self.data, b * DIR_ENTRY + 12))
    }

    /// Decodes block `b` into `docs`/`tfs`, returning how many pairs it holds.
    pub fn decode_block(&self, b: usize, docs: &mut [u32; BLOCK], tfs: &mut [u32; BLOCK]) -> usize {
        let n_blocks = self.n_blocks();
        let payload = &self.data[n_blocks * DIR_ENTRY..];
        let mut i = u32_at(self.data, b * DIR_ENTRY + 4) as usize;
        let len = if b + 1 == n_blocks { self.df as usize - b * BLOCK } else { BLOCK };
        let mut doc = if b == 0 { 0 } else { self.last_doc(b - 1) };
        for k in 0..len {
            doc += read_varint(payload, &mut i);
            docs[k] = doc;
            tfs[k] = read_varint(payload, &mut i);
        }
        len
    }

    pub fn for_each(&self, mut f: impl FnMut(u32, u32)) {
        let (mut docs, mut tfs) = ([0u32; BLOCK], [0u32; BLOCK]);
        for b in 0..self.n_blocks() {
            let n = self.decode_block(b, &mut docs, &mut tfs);
            for k in 0..n {
                f(docs[k], tfs[k]);
            }
        }
    }
}

pub struct Segment {
    pub n_docs: u32,
    pub total_len: u64,
    n_terms: usize,
    terms: Mmap,
    idx: Mmap,
    postings: Mmap,
    /// Read for every scored doc, so held in RAM (12 bytes per doc).
    meta: Vec<u8>,
    store: Mmap,
    store_idx: Mmap,
    /// Every SAMPLE-th term, concatenated; term i*SAMPLE spans sample[sample_at[i]..sample_at[i + 1]].
    sample: Vec<u8>,
    sample_at: Vec<u32>,
    deleted: Vec<bool>,
    n_deleted: u32,
}

impl Segment {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let meta_txt = fs::read_to_string(dir.join("meta.txt"))?;
        let bad = || io::Error::other(format!("bad meta.txt in {}", dir.display()));
        let mut it = meta_txt.split_whitespace();
        let n_docs: u32 = it.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let total_len: u64 = it.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        if it.next() != Some(FORMAT) {
            return Err(io::Error::other(format!("{} uses an older index format; rebuild the index", dir.display())));
        }

        let mut deleted = vec![false; n_docs as usize];
        let mut n_deleted = 0;
        if let Ok(bytes) = fs::read(dir.join("deleted.bin")) {
            for c in bytes.chunks_exact(4) {
                let d = u32::from_le_bytes(c.try_into().unwrap()) as usize;
                if d < deleted.len() && !deleted[d] {
                    deleted[d] = true;
                    n_deleted += 1;
                }
            }
        }
        // The dictionary and the stored pages are read at random, so readahead only pulls in
        // pages nobody asked for and evicts the ones that matter. Postings are read front to back.
        let (idx, terms) = (map(&dir.join("terms.idx"), Advice::Random)?, map(&dir.join("terms.bin"), Advice::Random)?);
        let n_terms = idx.len() / ENTRY;
        let sample_path = dir.join("terms.sample");
        let (sample_at, sample) = match fs::read(&sample_path).ok().as_deref().and_then(decode_sample) {
            Some(s) => s,
            None => {
                // Index built before terms.sample existed: sample the dictionary once, front to
                // back, and keep the result so later opens skip this.
                let _ = idx.advise(Advice::Sequential);
                let _ = terms.advise(Advice::Sequential);
                let (mut sample, mut sample_at) = (Vec::new(), vec![0u32]);
                for i in (0..n_terms).step_by(SAMPLE) {
                    let e = i * ENTRY;
                    let off = u64_at(&idx, e) as usize;
                    sample.extend_from_slice(&terms[off..off + u32_at(&idx, e + 8) as usize]);
                    sample_at.push(sample.len() as u32);
                }
                let _ = idx.advise(Advice::Random);
                let _ = terms.advise(Advice::Random);
                let _ = fs::write(&sample_path, encode_sample(&sample_at, &sample));
                (sample_at, sample)
            }
        };
        Ok(Self {
            n_docs,
            total_len,
            n_terms,
            terms,
            idx,
            postings: map(&dir.join("postings.bin"), Advice::Normal)?,
            meta: fs::read(dir.join("docmeta.bin"))?,
            store: map(&dir.join("store.bin"), Advice::Random)?,
            store_idx: map(&dir.join("store.idx"), Advice::Random)?,
            sample,
            sample_at,
            deleted,
            n_deleted,
        })
    }

    fn term(&self, i: usize) -> &[u8] {
        let e = i * ENTRY;
        let off = u64_at(&self.idx, e) as usize;
        &self.terms[off..off + u32_at(&self.idx, e + 8) as usize]
    }

    pub fn lookup(&self, term: &str) -> Option<Postings<'_>> {
        // Last sampled term <= `term`; the term, if present, is in the SAMPLE entries from there.
        let sampled = |i: usize| &self.sample[self.sample_at[i] as usize..self.sample_at[i + 1] as usize];
        let n_samples = self.sample_at.len() - 1;
        let (mut lo, mut hi) = (0, n_samples);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if sampled(mid) <= term.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let (mut lo, mut hi) = ((lo - 1) * SAMPLE, (lo * SAMPLE).min(self.n_terms));
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.term(mid).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.postings_at(mid)),
            }
        }
        None
    }

    fn postings_at(&self, i: usize) -> Postings<'_> {
        let e = i * ENTRY;
        let off = u64_at(&self.idx, e + 16) as usize;
        let len = u32_at(&self.idx, e + 24) as usize;
        Postings { df: u32_at(&self.idx, e + 12), data: &self.postings[off..off + len] }
    }

    pub fn doc_len(&self, d: u32) -> u32 {
        u32_at(&self.meta, d as usize * META)
    }

    pub fn quality(&self, d: u32) -> f32 {
        f32_at(&self.meta, d as usize * META + 4)
    }

    pub fn authority(&self, d: u32) -> f32 {
        f32_at(&self.meta, d as usize * META + 8)
    }

    pub fn is_deleted(&self, d: u32) -> bool {
        self.deleted[d as usize]
    }

    pub fn n_deleted(&self) -> u32 {
        self.n_deleted
    }

    /// (url, title, stored text)
    pub fn doc(&self, d: u32) -> (&str, &str, &str) {
        let mut o = u64_at(&self.store_idx, d as usize * 8) as usize;
        let url = read_field(&self.store, &mut o);
        let title = read_field(&self.store, &mut o);
        let text = read_field(&self.store, &mut o);
        (url, title, text)
    }
}

pub fn next_generation(root: &Path) -> io::Result<String> {
    let mut max = 0;
    for e in fs::read_dir(root)? {
        let name = e?.file_name();
        if let Some(n) = name.to_str().and_then(|n| n.strip_prefix("gen-")).and_then(|n| n.parse::<u32>().ok()) {
            max = max.max(n);
        }
    }
    Ok(format!("gen-{:04}", max + 1))
}

/// Points `CURRENT` at a finished generation. The rename is atomic, so a server opening the
/// index sees either the old generation or the new one, never a mix.
pub fn publish(root: &Path, generation: &str) -> io::Result<()> {
    if !root.join(generation).join("manifest.tsv").exists() {
        return Err(io::Error::other(format!("{generation} is not a finished generation")));
    }
    let tmp = root.join("CURRENT.tmp");
    fs::write(&tmp, format!("{generation}\n"))?;
    fs::rename(tmp, root.join("CURRENT"))
}

/// Deletes all but the newest `keep` generations, never the published one. A server that still
/// has an old generation open keeps working: its mapped files stay valid until it lets go.
pub fn prune(root: &Path, keep: usize) -> io::Result<Vec<String>> {
    let current = fs::read_to_string(root.join("CURRENT")).unwrap_or_default().trim().to_owned();
    let mut gens: Vec<String> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.starts_with("gen-"))
        .collect();
    gens.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed = Vec::new();
    for g in gens.into_iter().skip(keep) {
        if g != current {
            fs::remove_dir_all(root.join(&g))?;
            removed.push(g);
        }
    }
    Ok(removed)
}

pub fn current(root: &Path) -> io::Result<PathBuf> {
    let generation = fs::read_to_string(root.join("CURRENT"))
        .map_err(|_| io::Error::other(format!("no published index in {}", root.display())))?;
    Ok(root.join(generation.trim()))
}

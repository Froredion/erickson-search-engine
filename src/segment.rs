//! One shard of the index: an inverted index over the pages of one input file, plus the stored
//! fields needed to render results.
//!
//! Files in a segment directory:
//!   terms.bin     term bytes, concatenated in sorted order
//!   terms.idx     28 bytes per term: term offset u64, term len u32, df u32, postings offset u64, postings len u32
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

use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::tokenize::tokens;

const ENTRY: usize = 28;
const META: usize = 12;
pub const BLOCK: usize = 128;
const DIR_ENTRY: usize = 16;
const FORMAT: &str = "2";
const STORED_TEXT_BYTES: usize = 4096;
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

        let mut blob = BufWriter::new(File::create(self.dir.join("terms.bin"))?);
        let mut idx = BufWriter::new(File::create(self.dir.join("terms.idx"))?);
        let mut post = BufWriter::new(File::create(self.dir.join("postings.bin"))?);
        let (mut term_off, mut post_off) = (0u64, 0u64);
        let (mut buf, mut payload) = (Vec::new(), Vec::new());
        for (term, p) in &terms {
            buf.clear();
            payload.clear();
            let mut prev = 0;
            for block in p.chunks(2 * BLOCK) {
                let offset = payload.len() as u32;
                let (mut max_tf, mut min_dl) = (0u32, u32::MAX);
                for pair in block.chunks_exact(2) {
                    let (doc, tf) = (pair[0], pair[1]);
                    varint(&mut payload, doc - prev);
                    varint(&mut payload, tf);
                    prev = doc;
                    max_tf = max_tf.max(tf);
                    min_dl = min_dl.min(u32_at(&self.meta, doc as usize * META));
                }
                for v in [prev, offset, max_tf, min_dl] {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            buf.extend_from_slice(&payload);
            blob.write_all(term.as_bytes())?;
            post.write_all(&buf)?;
            idx.write_all(&term_off.to_le_bytes())?;
            idx.write_all(&(term.len() as u32).to_le_bytes())?;
            idx.write_all(&((p.len() / 2) as u32).to_le_bytes())?;
            idx.write_all(&post_off.to_le_bytes())?;
            idx.write_all(&(buf.len() as u32).to_le_bytes())?;
            term_off += term.len() as u64;
            post_off += buf.len() as u64;
        }
        blob.flush()?;
        idx.flush()?;
        post.flush()?;

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

pub fn write_tombstones(seg_dir: &Path, docs: &[u32]) -> io::Result<()> {
    let bytes: Vec<u8> = docs.iter().flat_map(|d| d.to_le_bytes()).collect();
    fs::write(seg_dir.join("deleted.bin"), bytes)
}

fn map(path: &Path) -> io::Result<Mmap> {
    let f = File::open(path)?;
    // Safety: segment files are written once and never modified while mapped.
    unsafe { Mmap::map(&f) }
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
    meta: Mmap,
    store: Mmap,
    store_idx: Mmap,
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
        let idx = map(&dir.join("terms.idx"))?;
        Ok(Self {
            n_docs,
            total_len,
            n_terms: idx.len() / ENTRY,
            terms: map(&dir.join("terms.bin"))?,
            idx,
            postings: map(&dir.join("postings.bin"))?,
            meta: map(&dir.join("docmeta.bin"))?,
            store: map(&dir.join("store.bin"))?,
            store_idx: map(&dir.join("store.idx"))?,
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
        let (mut lo, mut hi) = (0, self.n_terms);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.term(mid).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let e = mid * ENTRY;
                    let off = u64_at(&self.idx, e + 16) as usize;
                    let len = u32_at(&self.idx, e + 24) as usize;
                    return Some(Postings { df: u32_at(&self.idx, e + 12), data: &self.postings[off..off + len] });
                }
            }
        }
        None
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

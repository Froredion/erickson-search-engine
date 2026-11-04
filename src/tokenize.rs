//! Lowercased, stemmed alphanumeric tokens. The same tokenizer runs at index and query time,
//! and for snippets and proximity, so every part of the engine agrees on what a term is.

use crate::stem::stem;
use std::collections::HashSet;
use std::sync::OnceLock;

const MAX_TOKEN_BYTES: usize = 40;

/// Common English function words. Indexed (so "the who" still works) but dropped from queries
/// that have other terms: they barely affect ranking and cost the most to score.
const STOPWORDS: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not", "of",
    "on", "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

/// Calls `f(start, end, token)` for every token, with byte offsets into `text`.
pub fn token_spans(text: &str, f: &mut impl FnMut(usize, usize, &str)) {
    let mut buf = String::with_capacity(32);
    let mut start = 0;
    let mut emit = |buf: &mut String, start: usize, end: usize| {
        if buf.len() <= MAX_TOKEN_BYTES {
            stem(buf);
            f(start, end, buf);
        }
        buf.clear();
    };
    for (i, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if buf.is_empty() {
                start = i;
            }
            buf.extend(ch.to_lowercase());
        } else if !buf.is_empty() {
            emit(&mut buf, start, i);
        }
    }
    if !buf.is_empty() {
        emit(&mut buf, start, text.len());
    }
}

pub fn tokens(text: &str, f: &mut impl FnMut(&str)) {
    token_spans(text, &mut |_, _, t| f(t));
}

/// Stopwords as the tokenizer emits them (stemmed: "was" -> "wa").
fn is_stopword(term: &str) -> bool {
    static SET: OnceLock<HashSet<String>> = OnceLock::new();
    SET.get_or_init(|| {
        STOPWORDS
            .iter()
            .map(|w| {
                let mut s = (*w).to_owned();
                stem(&mut s);
                s
            })
            .collect()
    })
    .contains(term)
}

/// Unique query terms in order, without stopwords unless the query is nothing but stopwords.
/// Capped so a pasted paragraph can't stall the server.
pub fn query_terms(q: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    tokens(q, &mut |t| {
        if !all.iter().any(|o| o == t) {
            all.push(t.to_owned());
        }
    });
    let content: Vec<String> = all.iter().filter(|t| !is_stopword(t)).cloned().collect();
    let mut out = if content.is_empty() { all } else { content };
    out.truncate(12);
    out
}

//! HTTP front end: the search page and a JSON API.
//!
//! Made to face the public internet behind a TLS proxy: each client IP gets a small token bucket,
//! at most MAX_SEARCHES searches run at once (more wait up to SLOT_WAIT for a turn, then get a 503),
//! repeated queries come from a cache, and over-long queries are refused.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response, Server};

use crate::search::{Index, Results};

const PAGE: &str = include_str!("../static/index.html");
const WORKERS: usize = 16;
/// Each search already runs on every core, so a few at a time keeps each one fast.
const MAX_SEARCHES: usize = 4;
const SLOT_WAIT: Duration = Duration::from_millis(1500);
const MAX_QUERY_CHARS: usize = 200;
/// Token bucket per client IP: RATE searches per second, bursts of up to BURST.
const RATE: f64 = 2.0;
const BURST: f64 = 10.0;
const CACHE_ENTRIES: usize = 20_000;

struct State {
    index: Index,
    running: Mutex<usize>,
    slot_freed: Condvar,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
    cache: Mutex<HashMap<(String, usize), String>>,
}

pub fn serve(index: Index, bind: &str, port: u16) -> io::Result<()> {
    let server = Arc::new(Server::http((bind, port)).map_err(|e| io::Error::other(e.to_string()))?);
    eprintln!(
        "serving {} pages ({} segments, {}) at http://{bind}:{port}",
        index.n_docs - index.n_deleted,
        index.n_segments(),
        index.generation
    );
    let state = Arc::new(State {
        index,
        running: Mutex::new(0),
        slot_freed: Condvar::new(),
        buckets: Mutex::new(HashMap::new()),
        cache: Mutex::new(HashMap::new()),
    });
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, state) = (server.clone(), state.clone());
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(&state, req);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn handle(st: &State, req: Request) {
    let url = req.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let ix = &st.index;
    match path {
        "/" => respond(req, 200, PAGE.to_owned(), "text/html; charset=utf-8"),
        "/api/search" => {
            let q = param(query, "q").unwrap_or_default();
            let q = q.trim();
            let page: usize = param(query, "page").and_then(|p| p.parse().ok()).unwrap_or(0).min(20);
            if q.chars().count() > MAX_QUERY_CHARS {
                return respond(req, 400, error_json("query too long"), "application/json");
            }
            if !allow(st, client_ip(&req)) {
                return respond(req, 429, error_json("too many searches, slow down"), "application/json");
            }
            let key = (q.to_lowercase(), page);
            if let Some(body) = st.cache.lock().unwrap().get(&key).cloned() {
                return respond(req, 200, body, "application/json");
            }
            let running = st.running.lock().unwrap();
            let (mut running, wait) =
                st.slot_freed.wait_timeout_while(running, SLOT_WAIT, |n| *n >= MAX_SEARCHES).unwrap();
            if wait.timed_out() {
                drop(running);
                return respond(req, 503, error_json("busy, try again in a moment"), "application/json");
            }
            *running += 1;
            drop(running);
            let r = ix.search(q, 10, page * 10);
            *st.running.lock().unwrap() -= 1;
            st.slot_freed.notify_one();
            let body = results_json(q, &r);
            let mut cache = st.cache.lock().unwrap();
            if cache.len() >= CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(key, body.clone());
            drop(cache);
            respond(req, 200, body, "application/json")
        }
        "/api/stats" => {
            let body = format!(
                "{{\"pages\":{},\"duplicates_removed\":{},\"segments\":{},\"generation\":{}}}",
                ix.n_docs - ix.n_deleted,
                ix.n_deleted,
                ix.n_segments(),
                json_str(&ix.generation)
            );
            respond(req, 200, body, "application/json")
        }
        _ => respond(req, 404, "not found".to_owned(), "text/plain"),
    }
}

/// The client's address. Behind a proxy on this machine the peer is loopback and the proxy
/// appends the real client to X-Forwarded-For, so its last entry is the one to trust.
fn client_ip(req: &Request) -> Option<IpAddr> {
    let peer = req.remote_addr()?.ip();
    if !peer.is_loopback() {
        return Some(peer);
    }
    req.headers()
        .iter()
        .find(|h| h.field.equiv("X-Forwarded-For"))
        .and_then(|h| h.value.as_str().rsplit(',').next()?.trim().parse().ok())
        .or(Some(peer))
}

/// Takes one token from the client's bucket, if there is one.
fn allow(st: &State, ip: Option<IpAddr>) -> bool {
    let Some(ip) = ip else { return true };
    let now = Instant::now();
    let mut buckets = st.buckets.lock().unwrap();
    if buckets.len() > 100_000 {
        // Full buckets carry no state worth keeping.
        buckets.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * RATE < BURST);
    }
    let (tokens, at) = buckets.entry(ip).or_insert((BURST, now));
    *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * RATE).min(BURST);
    *at = now;
    if *tokens < 1.0 {
        return false;
    }
    *tokens -= 1.0;
    true
}

fn error_json(msg: &str) -> String {
    format!("{{\"error\":{}}}", json_str(msg))
}

fn respond(req: Request, status: u16, body: String, content_type: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
    let nosniff = Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap();
    let response = Response::from_string(body).with_status_code(status).with_header(header).with_header(nosniff);
    let _ = req.respond(response);
}

fn results_json(q: &str, r: &Results) -> String {
    let mut out = format!(
        "{{\"query\":{},\"matched\":{},\"ms\":{:.2},\"results\":[",
        json_str(q),
        r.matched,
        r.micros as f64 / 1000.0
    );
    for (i, h) in r.hits.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"url\":{},\"title\":{},\"snippet\":{}}}",
            json_str(&h.url),
            json_str(&h.title),
            json_str(&h.snippet)
        ));
    }
    out.push_str("]}");
    out
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            x => out.push(x),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

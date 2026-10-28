//! HTTP front end: the search page and a JSON API.

use std::io;
use std::sync::Arc;
use tiny_http::{Header, Request, Response, Server};

use crate::search::{Index, Results};

const PAGE: &str = include_str!("../static/index.html");
const WORKERS: usize = 8;

pub fn serve(index: Index, port: u16) -> io::Result<()> {
    let server = Arc::new(Server::http(("127.0.0.1", port)).map_err(|e| io::Error::other(e.to_string()))?);
    let index = Arc::new(index);
    eprintln!(
        "serving {} pages ({} segments, {}) at http://127.0.0.1:{port}",
        index.n_docs - index.n_deleted,
        index.n_segments(),
        index.generation
    );
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, index) = (server.clone(), index.clone());
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(&index, req);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn handle(ix: &Index, req: Request) {
    let url = req.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    match path {
        "/" => respond(req, 200, PAGE.to_owned(), "text/html; charset=utf-8"),
        "/api/search" => {
            let q = param(query, "q").unwrap_or_default();
            let page: usize = param(query, "page").and_then(|p| p.parse().ok()).unwrap_or(0).min(20);
            let r = ix.search(&q, 10, page * 10);
            respond(req, 200, results_json(&q, &r), "application/json")
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

fn respond(req: Request, status: u16, body: String, content_type: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
    let _ = req.respond(Response::from_string(body).with_status_code(status).with_header(header));
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

//! Streaming reader for Common Crawl WET files (WARC records of extracted page text).

use flate2::read::MultiGzDecoder;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

pub struct Record {
    pub uri: String,
    /// `WARC-Identified-Content-Language`, e.g. "eng" or "eng,fra".
    pub lang: Option<String>,
    /// Extracted text. Common Crawl puts the HTML title on the first line.
    pub body: String,
}

pub struct WetReader {
    r: Box<dyn BufRead>,
    line: String,
}

pub fn open(path: &Path) -> io::Result<WetReader> {
    let f = File::open(path)?;
    let r: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(BufReader::with_capacity(1 << 20, MultiGzDecoder::new(f)))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, f))
    };
    Ok(WetReader { r, line: String::new() })
}

impl WetReader {
    fn read_line(&mut self) -> Option<&str> {
        self.line.clear();
        match self.r.read_line(&mut self.line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(self.line.trim_end()),
        }
    }
}

impl Iterator for WetReader {
    type Item = Record;

    fn next(&mut self) -> Option<Record> {
        loop {
            while !self.read_line()?.starts_with("WARC/") {}
            let (mut ty, mut uri, mut lang, mut len) = (String::new(), String::new(), None, 0usize);
            loop {
                let l = self.read_line()?;
                if l.is_empty() {
                    break;
                }
                if let Some((k, v)) = l.split_once(':') {
                    let v = v.trim();
                    match k {
                        "WARC-Type" => ty = v.to_owned(),
                        "WARC-Target-URI" => uri = v.to_owned(),
                        "WARC-Identified-Content-Language" => lang = Some(v.to_owned()),
                        "Content-Length" => len = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            let mut body = vec![0; len];
            self.r.read_exact(&mut body).ok()?;
            if ty == "conversion" && !uri.is_empty() {
                let body = String::from_utf8(body).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                return Some(Record { uri, lang, body });
            }
        }
    }
}

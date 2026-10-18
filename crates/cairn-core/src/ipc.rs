//! Wire formats.
//!
//! * Worker pipes: frames of `u32 LE header length, JSON header, u32 LE
//!   payload length, payload bytes`. Lengths are bounded on read.
//! * Control socket: one JSON request line, one JSON response line. The
//!   socket lives in a 0700 directory and the daemon also checks SO_PEERCRED.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::query::ParsedSummary;

pub const MAX_HEADER: u32 = 4 << 20;
pub const MAX_PAYLOAD: u32 = 64 << 20;
pub const MAX_REQUEST_LINE: usize = 64 << 10;

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, header: &T, payload: &[u8]) -> Result<()> {
    let h = serde_json::to_vec(header)?;
    if h.len() as u64 > MAX_HEADER as u64 || payload.len() as u64 > MAX_PAYLOAD as u64 {
        bail!("frame too large");
    }
    w.write_all(&(h.len() as u32).to_le_bytes())?;
    w.write_all(&h)?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Returns None on clean EOF before a frame starts.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<(T, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let hl = u32::from_le_bytes(len);
    if hl > MAX_HEADER {
        bail!("header too large");
    }
    let mut h = vec![0u8; hl as usize];
    r.read_exact(&mut h)?;
    r.read_exact(&mut len)?;
    let pl = u32::from_le_bytes(len);
    if pl > MAX_PAYLOAD {
        bail!("payload too large");
    }
    let mut p = vec![0u8; pl as usize];
    r.read_exact(&mut p)?;
    Ok(Some((serde_json::from_slice(&h)?, p)))
}

/// A path that survives JSON even when it is not valid UTF-8.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WirePath {
    /// Display form (lossy if needed).
    pub path: String,
    /// Hex of the raw bytes, present only when `path` is lossy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl WirePath {
    pub fn from_bytes(b: &[u8]) -> WirePath {
        match std::str::from_utf8(b) {
            Ok(s) => WirePath { path: s.to_string(), raw: None },
            Err(_) => WirePath { path: String::from_utf8_lossy(b).into_owned(), raw: Some(b.iter().map(|x| format!("{x:02x}")).collect()) },
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        if let Some(raw) = &self.raw {
            let v: Option<Vec<u8>> = (0..raw.len() / 2).map(|i| u8::from_str_radix(raw.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
            if let Some(v) = v {
                return v;
            }
        }
        self.path.as_bytes().to_vec()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Search {
        query: String,
        #[serde(default)]
        cwd: Option<WirePath>,
        #[serde(default = "default_limit")]
        limit: usize,
        /// Restrict results to `cwd` and below (Nautilus "Search here").
        #[serde(default)]
        scope: bool,
        /// Kind filter chosen in a UI (e.g. ["image"]); overrides kinds
        /// recognised in the text when non-empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        kinds: Vec<String>,
    },
    Status,
    Exclude { path: WirePath },
    Include { path: WirePath },
    Pause,
    Resume,
    Rescan,
}

fn default_limit() -> usize {
    40
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub path: WirePath,
    pub category: String,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Best known date: capture time for media, else modification time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    pub score: f32,
    /// Why it matched, e.g. "folder “Manali trek Dec 25”", "image content".
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub parsed: ParsedSummary,
    pub took_ms: u64,
    /// True while the initial index is still being built.
    pub partial: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub roots: Vec<String>,
    pub files: i64,
    pub dirs: i64,
    pub chunks: i64,
    pub text_vectors: i64,
    pub image_vectors: i64,
    pub pending: Vec<(String, i64)>,
    pub failed: i64,
    pub paused: bool,
    pub throttle: Option<String>,
    pub scanning: bool,
    pub models: Vec<(String, bool)>,
    pub sandbox: String,
    pub db_bytes: u64,
    pub exclusions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Search(SearchResult),
    Status(Status),
    Ok { message: String },
    Error { message: String },
}

/// Client side: send one request, read one response. Never hangs forever.
pub fn call(req: &Request, timeout: Duration) -> Result<Response> {
    let sock = crate::paths::socket_path()?;
    let mut s = UnixStream::connect(&sock).with_context(|| format!("connecting to {} (is cairnd running?)", sock.display()))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut r = BufReader::new(s);
    let mut resp = String::new();
    r.read_line(&mut resp).context("reading response")?;
    if resp.is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&resp)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_path_roundtrip() {
        for b in [&b"/home/u/caf\xe9.txt"[..], b"/home/u/normal.txt", b"/"] {
            let w = WirePath::from_bytes(b);
            let j = serde_json::to_string(&w).unwrap();
            let back: WirePath = serde_json::from_str(&j).unwrap();
            assert_eq!(back.bytes(), b.to_vec());
        }
    }

    #[test]
    fn frames_roundtrip_and_bounds() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::json!({"a": 1}), b"xyz").unwrap();
        let (h, p): (serde_json::Value, Vec<u8>) = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(h["a"], 1);
        assert_eq!(p, b"xyz");
        let evil = [0xff, 0xff, 0xff, 0xff];
        assert!(read_frame::<_, serde_json::Value>(&mut evil.as_slice()).is_err());
        assert!(read_frame::<_, serde_json::Value>(&mut [].as_slice()).unwrap().is_none());
    }
}

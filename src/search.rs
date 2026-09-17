//! Deep search across transcript bodies.
//!
//! Fuzzy matching on titles is done in memory by the UI; this module is for the
//! expensive kind -- "which session did I fix the NVENC thing in" -- which has
//! to read the actual conversations. A parallel byte scan over the corpus lands
//! around two tenths of a second, so it runs on a background thread with the
//! results folded in when they arrive.

use crate::model::Session;
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Anything, anywhere in the conversation.
    Content,
    /// Files the session actually read or edited (tool inputs, not mentions).
    File,
    /// Sessions that invoked a given tool.
    Tool,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Content => "content",
            Mode::File => "file touched",
            Mode::Tool => "tool used",
        }
    }
    pub fn next(self) -> Mode {
        match self {
            Mode::Content => Mode::File,
            Mode::File => Mode::Tool,
            Mode::Tool => Mode::Content,
        }
    }
}

/// ASCII case-insensitive substring search. `needle` must already be lowercase.
/// Candidate positions come from memchr on both cases of the first byte, which
/// keeps this close to a plain memmem while avoiding lowercasing the haystack.
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let lo = needle[0];
    let up = lo.to_ascii_uppercase();
    let mut start = 0usize;
    let limit = hay.len() - needle.len() + 1;
    while start < limit {
        let rel = if lo == up {
            memchr::memchr(lo, &hay[start..limit])?
        } else {
            memchr::memchr2(lo, up, &hay[start..limit])?
        };
        let at = start + rel;
        if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        start = at + 1;
    }
    None
}

/// Turn a raw JSON line into something readable around the hit.
fn snippet(line: &[u8], at: usize) -> String {
    const PAD: usize = 90;
    let lo = at.saturating_sub(PAD);
    let hi = (at + PAD).min(line.len());
    let raw = String::from_utf8_lossy(&line[lo..hi]);
    let cleaned: String = raw
        .replace("\\n", " ")
        .replace("\\\"", "\"")
        .replace("\\t", " ");
    let mut out = crate::scan::squash(&cleaned, 200);
    if lo > 0 {
        out.insert_str(0, "…");
    }
    if hi < line.len() {
        out.push('…');
    }
    out
}

/// Spans of injected context inside a line, as byte ranges.
///
/// Every session gets the user's memory index and other `<system-reminder>`
/// blocks pasted into its context. Those are not things that happened in the
/// conversation, so a naive substring search reports every session that ever
/// ran as a match for any word in them. Matches landing inside these spans are
/// ignored.
fn injected_spans(line: &[u8]) -> Vec<(usize, usize)> {
    const OPEN: &[u8] = b"<system-reminder>";
    const CLOSE: &[u8] = b"</system-reminder>";
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = memmem::find(&line[from..], OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let end = match memmem::find(&line[after..], CLOSE) {
            Some(r) => after + r + CLOSE.len(),
            None => line.len(),
        };
        spans.push((start, end));
        from = end;
    }
    spans
}

/// Is this line something that was actually said, rather than machinery?
///
/// Uses the same classification verified in `scan`: metadata lines begin
/// literally with `{"type":"`, and real turns carry `"role":"user"` or
/// `"role":"assistant"`. This is what keeps `attachment` lines -- which is
/// where the injected memory index lives -- out of content search.
fn is_conversation(line: &[u8]) -> bool {
    if line.starts_with(b"{\"type\":\"") {
        return false;
    }
    if memmem::find(line, b"\"isMeta\":true").is_some() {
        return false;
    }
    memmem::find(line, b"\"role\":\"user\"").is_some()
        || memmem::find(line, b"\"role\":\"assistant\"").is_some()
}

/// Is the match sitting inside a base64 blob rather than prose?
///
/// Pasted images and binary tool payloads arrive as long unbroken base64, and
/// its alphabet happily spells short words like "nvenc" by chance. Prose has
/// spaces; a blob does not, so a wide window with no whitespace in it is the
/// tell.
fn looks_like_blob(line: &[u8], at: usize) -> bool {
    const W: usize = 70;
    let lo = at.saturating_sub(W);
    let hi = (at + W).min(line.len());
    if hi - lo < W {
        return false;
    }
    !line[lo..hi].iter().any(|c| matches!(c, b' ' | b'\\' | b'\t' | b'>' | b',' | b'.' | b';'))
}

/// First occurrence of `needle` that is real conversation, not injected context.
fn content_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    if !is_conversation(line) {
        return None;
    }
    let first = find_ci(line, needle)?;
    let spans = injected_spans(line);
    let bad = |at: usize| {
        spans.iter().any(|(a, b)| at >= *a && at < *b) || looks_like_blob(line, at)
    };
    if !bad(first) {
        return Some(first);
    }
    let mut at = first;
    loop {
        let next_rel = find_ci(&line[at + 1..], needle)?;
        at = at + 1 + next_rel;
        if !bad(at) {
            return Some(at);
        }
    }
}

/// Does this line record a tool actually touching `needle` as a path?
fn file_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    for key in [
        &b"\"file_path\":\""[..],
        &b"\"notebook_path\":\""[..],
        &b"\"path\":\""[..],
    ] {
        let mut from = 0;
        while let Some(rel) = memmem::find(&line[from..], key) {
            let vs = from + rel + key.len();
            if let Some(end) = memchr::memchr(b'"', &line[vs..]) {
                let val = &line[vs..vs + end];
                if find_ci(val, needle).is_some() {
                    return Some(vs);
                }
                from = vs + end;
            } else {
                break;
            }
        }
    }
    None
}

fn tool_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    // tool_use blocks look like {"type":"tool_use","id":...,"name":"Edit",...}
    let mut from = 0;
    let key = &b"\"name\":\""[..];
    let has_tool = memmem::find(line, b"\"tool_use\"").is_some();
    if !has_tool {
        return None;
    }
    while let Some(rel) = memmem::find(&line[from..], key) {
        let vs = from + rel + key.len();
        if let Some(end) = memchr::memchr(b'"', &line[vs..]) {
            let val = &line[vs..vs + end];
            if find_ci(val, needle).is_some() {
                return Some(vs);
            }
            from = vs + end;
        } else {
            break;
        }
    }
    None
}

/// Scan one file, returning the first readable hit.
fn search_file(path: &std::path::Path, needle: &[u8], mode: Mode) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut rdr = BufReader::with_capacity(1 << 18, f);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 14);
    loop {
        buf.clear();
        let n = rdr.read_until(b'\n', &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        let line = &buf[..n];
        let hit = match mode {
            Mode::Content => content_hit(line, needle),
            Mode::File => file_hit(line, needle),
            Mode::Tool => tool_hit(line, needle),
        };
        if let Some(at) = hit {
            return Some(snippet(line, at));
        }
        if buf.capacity() > (1 << 20) {
            buf = Vec::with_capacity(1 << 14);
        }
    }
}

/// Search every session in parallel. Returns path -> snippet for hits only.
pub fn run(sessions: &[Session], query: &str, mode: Mode) -> HashMap<String, String> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return HashMap::new();
    }
    let nb = needle.as_bytes();
    sessions
        .par_iter()
        .filter_map(|s| {
            search_file(&s.path, nb, mode)
                .map(|snip| (s.path.to_string_lossy().to_string(), snip))
        })
        .collect()
}

//! Conversation preview.
//!
//! The old tool read the entire transcript to show the last handful of
//! messages, which on the biggest file here means reading 386 MB to print
//! eight lines. We seek to the end and read backwards instead, so preview cost
//! is independent of session size.

use crate::model::Session;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Debug)]
pub struct Turn {
    pub role: &'static str,
    pub text: String,
}

const TAIL_BYTES: u64 = 512 * 1024;

fn extract_turns(bytes: &[u8], drop_first_partial: bool) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut iter = bytes.split(|b| *b == b'\n');
    if drop_first_partial {
        iter.next();
    }
    for line in iter {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let role = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => "you",
            Some("assistant") => "claude",
            _ => continue,
        };
        let Some(c) = v.get("message").and_then(|m| m.get("content")) else { continue };
        let text = flatten(c);
        let text = crate::scan::squash(&text, 700);
        if text.is_empty() || !crate::scan::is_real_user_text(&text) {
            continue;
        }
        turns.push(Turn { role, text });
    }
    turns
}

fn flatten(c: &serde_json::Value) -> String {
    match c {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let mut parts: Vec<String> = Vec::new();
            for b in a {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        parts.push(format!("[{name}]"));
                    }
                    _ => {}
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

/// The last `want` readable turns of a session.
pub fn tail_turns(s: &Session, want: usize) -> Vec<Turn> {
    let Ok(mut f) = std::fs::File::open(&s.path) else { return Vec::new() };
    let len = s.size;
    let (start, partial) = if len > TAIL_BYTES {
        (len - TAIL_BYTES, true)
    } else {
        (0, false)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if (&mut f).take(TAIL_BYTES + 4096).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let mut turns = extract_turns(&buf, partial);
    // A single enormous final message can swallow the whole window; if we found
    // nothing at all, fall back to a bigger bite before giving up.
    if turns.is_empty() && start > 0 {
        let bigger = (TAIL_BYTES * 8).min(len);
        if f.seek(SeekFrom::Start(len - bigger)).is_ok() {
            buf.clear();
            if (&mut f).take(bigger + 4096).read_to_end(&mut buf).is_ok() {
                turns = extract_turns(&buf, true);
            }
        }
    }
    let n = turns.len();
    if n > want {
        turns.drain(..n - want);
    }
    turns
}

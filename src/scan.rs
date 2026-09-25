//! Transcript discovery and parsing.
//!
//! Performance notes, because this is the part that has to touch ~2.5 GB:
//!
//! * We never run `serde_json` over a whole transcript. Instead each line is
//!   classified with a byte-level test and only the handful of lines that
//!   actually carry a title or prompt get parsed properly.
//! * Classification relies on two facts verified against the real corpus:
//!   metadata lines begin literally with `{"type":"`, and message lines carry
//!   `"role":"user"` / `"role":"assistant"` near their front. Looking for the
//!   first `"type":"` in a line does NOT work -- message lines embed a nested
//!   `"type":"text"` content block before their own top-level `type`, which
//!   misclassifies about half of all lines.
//! * Transcripts are append-only, so we remember how many bytes we already
//!   consumed (`scanned_len`) and on later runs read only the new tail.

use crate::model::Session;
use anyhow::Result;
use memchr::memmem;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub fn projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".claude/projects")
}

/// Every transcript on disk: `(path, is_subagent, parent_session_id)`.
pub fn discover(include_subagents: bool) -> Vec<(PathBuf, bool, Option<String>)> {
    let root = projects_dir();
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(&root) else {
        return out;
    };
    for proj in projects.flatten() {
        let pdir = proj.path();
        if !pdir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&pdir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
                out.push((p, false, None));
            } else if include_subagents && p.is_dir() {
                // <project>/<parent-session-id>/subagents/agent-*.jsonl
                let parent = p.file_name().map(|s| s.to_string_lossy().to_string());
                let sub = p.join("subagents");
                if let Ok(agents) = fs::read_dir(&sub) {
                    for a in agents.flatten() {
                        let ap = a.path();
                        if ap.is_file() && ap.extension().is_some_and(|x| x == "jsonl") {
                            out.push((ap, true, parent.clone()));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Pull a JSON string value out of raw bytes without parsing the document.
/// Good enough for the flat, machine-written fields (cwd, gitBranch,
/// timestamp, model, version) which never contain escapes in practice.
fn raw_str(hay: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let i = memmem::find(hay, needle.as_bytes())? + needle.len();
    let rest = &hay[i..];
    let end = memchr::memchr(b'"', rest)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Pull a JSON number out of raw bytes. The key must match exactly, so
/// `"output_tokens":` does not also catch `"output_tokens_details":`.
fn raw_num(hay: &[u8], key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let i = memmem::find(hay, needle.as_bytes())? + needle.len();
    let rest = &hay[i..];
    let digits: Vec<u8> = rest
        .iter()
        .skip_while(|c| **c == b' ')
        .take_while(|c| c.is_ascii_digit())
        .copied()
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

fn iso_to_epoch(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// Flatten a message `content` field (string, or array of blocks) to what
/// the user wrote.
///
/// Each block is judged on its own. A message sent from an IDE is an
/// `<ide_opened_file>` block and then the words, and joined before being
/// judged the whole of it looked like an envelope: the session lost its
/// opening prompt, and with no AI title, its title.
fn content_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let mut parts = Vec::new();
            for b in a {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if is_real_user_text(t) {
                            parts.push(t);
                        }
                    }
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

/// Reject the synthetic user turns: slash-command envelopes, system reminders,
/// local command output, and the resume caveat. These are not things the user
/// typed, so they make misleading titles.
pub fn is_real_user_text(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:")
}

/// Collapse whitespace, drop control characters, clip to `max` characters.
///
/// Control characters are dropped rather than kept for two reasons. They are
/// counted when a column is measured but occupy no cell when drawn, so one
/// bell in a title shifted every column after it by one. And an ESC in a
/// title would be handed straight to the terminal, which is a transcript
/// deciding what your screen does.
pub fn squash(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(s.len()));
    // Counted as it goes, the joining spaces included: counting only after
    // each character let a space and the character after it both through,
    // one past `max`.
    let mut n = 0usize;
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if c.is_control() {
            continue;
        }
        if space && n > 0 {
            if n + 1 >= max {
                break;
            }
            out.push(' ');
            n += 1;
        }
        space = false;
        out.push(c);
        n += 1;
        if n >= max {
            break;
        }
    }
    out
}

/// Context Claude Code injected rather than anything either party said:
/// memory files, system reminders, command envelopes.
///
/// The search and the index have to agree on this. They did not: the search
/// skipped these lines and the index harvested them, so the same query
/// answered differently depending on which engine ran.
/// How much prose one session may contribute to the index.
const HARVEST_CAP: usize = 64 << 20;

pub fn is_injected_meta(line: &[u8]) -> bool {
    memmem::find(line, b"\"isMeta\":true").is_some()
}

/// A line handing a tool's output back, rather than anything said.
fn carries_tool_result(line: &[u8]) -> bool {
    memmem::find(line, b"\"type\":\"tool_result\"").is_some()
}

/// Pull the prose out of a conversation line for the search index.
///
/// Only `{"type":"text"}` content blocks are taken, which is what makes the
/// index small: the corpus is 2.5 GB but barely two per cent of it is
/// anything a person said. Tool payloads, base64 images and JSON scaffolding
/// are skipped by construction rather than by heuristic, and injected
/// `<system-reminder>` context is dropped so it cannot make every session
/// match every word in your memory file.
pub fn harvest_text(line: &[u8], out: &mut String) {
    harvest_blocks(line, b"\"type\":\"text\"", b"\"text\":\"", out);
    // Reasoning is part of the conversation and is where a lot of the
    // substance ends up; leaving it out cost noticeable recall.
    harvest_blocks(line, b"\"type\":\"thinking\"", b"\"thinking\":\"", out);
    // The commands that were actually run, so "which session touched zfs"
    // still finds a shell invocation rather than only chatter about it.
    harvest_values(line, b"\"command\":\"", out);
    harvest_values(line, b"\"description\":\"", out);
    // A message is not always a list of blocks: plenty are stored as
    // `"content":"<the text>"`. Those are overwhelmingly what the user typed,
    // and taking only block content left 16% of the prose in this corpus --
    // 341 of 344 transcripts -- unfindable by the default search.
    //
    // Anchored on the role so this takes the *message's* content and not a
    // tool_result's, which is output rather than conversation and is what
    // the index exists to leave out.
    harvest_values(line, b"\"role\":\"user\",\"content\":\"", out);
    harvest_values(line, b"\"role\":\"assistant\",\"content\":\"", out);
}

fn harvest_blocks(line: &[u8], marker: &[u8], value_key: &[u8], out: &mut String) {
    let mut from = 0usize;
    while let Some(rel) = memmem::find(&line[from..], marker) {
        let at = from + rel;
        from = at + marker.len();
        // the value key follows within a few bytes in either ordering
        let window_end = (from + 48).min(line.len());
        let Some(vrel) = memmem::find(&line[from..window_end], value_key) else {
            continue;
        };
        let mut i = from + vrel + value_key.len();
        let start = i;
        // walk the JSON string, honouring escapes
        while i < line.len() {
            match line[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        if i > line.len() {
            break;
        }
        let raw = String::from_utf8_lossy(&line[start..i.min(line.len())]);
        push_unescaped(&raw, out);
        from = i;
    }
}

/// Collect every value of a given JSON key, wherever it appears in the line.
fn harvest_values(line: &[u8], key: &[u8], out: &mut String) {
    let mut from = 0usize;
    while let Some(rel) = memmem::find(&line[from..], key) {
        let mut i = from + rel + key.len();
        let start = i;
        while i < line.len() {
            match line[i] {
                b'\\' => i += 2,
                b'"' => break,
                _ => i += 1,
            }
        }
        let raw = String::from_utf8_lossy(&line[start..i.min(line.len())]);
        push_unescaped(&raw, out);
        // An escape at the very end steps the walk one past it.
        from = i.max(start + 1).min(line.len());
    }
}

/// Minimal JSON string unescaping, with injected context removed.
fn push_unescaped(raw: &str, out: &mut String) {
    let cleaned = strip_reminders(raw);
    let mut chars = cleaned.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('t') | Some('r') => out.push(' '),
            Some('u') => {
                // skip the four hex digits; exact glyphs do not matter to a
                // tokeniser and decoding them here is not worth the code
                for _ in 0..4 {
                    chars.next();
                }
                out.push(' ');
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out.push(' ');
}

fn strip_reminders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<system-reminder>") {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find("</system-reminder>") {
            Some(j) => &rest[i + j + "</system-reminder>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

const BIG_LINE: usize = 1 << 23; // 8 MiB: base64 attachments live up here
const FRONT: usize = 1 << 16;
const TAIL: usize = 1 << 13;

fn front(line: &[u8]) -> &[u8] {
    if line.len() > BIG_LINE {
        &line[..FRONT]
    } else {
        line
    }
}
fn tail(line: &[u8]) -> &[u8] {
    if line.len() > BIG_LINE {
        &line[line.len() - TAIL..]
    } else {
        line
    }
}

fn process_line(s: &mut Session, line: &[u8], text: &mut Option<&mut String>) {
    if line.is_empty() {
        return;
    }
    s.entries += 1;

    // --- timestamps: a late key, so look at the tail of huge lines first ---
    if let Some(ts) = raw_str(tail(line), "timestamp").or_else(|| raw_str(front(line), "timestamp"))
    {
        let e = iso_to_epoch(&ts);
        if e > 0 {
            if s.first_ts == 0 {
                s.first_ts = e;
            }
            if e > s.last_ts {
                s.last_ts = e;
            }
        }
    }

    let f = front(line);
    if s.cwd.is_empty() {
        if let Some(v) = raw_str(f, "cwd") {
            s.cwd = v;
        }
    }
    if s.git_branch.is_empty() {
        if let Some(v) = raw_str(f, "gitBranch") {
            s.git_branch = v;
        }
    }
    if s.version.is_empty() {
        if let Some(v) = raw_str(f, "version") {
            s.version = v;
        }
    }
    if s.agent_id.is_empty() {
        if let Some(v) = raw_str(f, "agentId") {
            s.agent_id = v;
        }
    }

    // --- metadata lines: exact, they literally start with {"type":" ---
    if line.starts_with(b"{\"type\":\"") {
        let rest = &line[9..];
        let Some(q) = memchr::memchr(b'"', rest) else {
            return;
        };
        let kind = &rest[..q];
        match kind {
            // A name given with `/rename`. Written again as the session goes
            // on, and a later one is a rename, so the last is the one.
            b"custom-title" => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                    if let Some(t) = v.get("customTitle").and_then(|x| x.as_str()) {
                        s.custom_title = squash(t, 160);
                    }
                }
            }
            b"ai-title" => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                    if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                        if !t.trim().is_empty() {
                            s.ai_title = squash(t, 160);
                        }
                    }
                }
            }
            b"last-prompt" => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                    if let Some(t) = v.get("lastPrompt").and_then(|x| x.as_str()) {
                        if is_real_user_text(t) {
                            s.last_prompt = squash(t, 200);
                        }
                    }
                }
            }
            b"permission-mode" => {
                // The mode the session STARTED in: keep the first record and
                // ignore later ones. Only 1 session in 257 here ever changed
                // mode mid-run, and first-seen is also stable under the
                // incremental tail scan.
                if let (true, Some(v)) =
                    (s.permission_mode.is_empty(), raw_str(f, "permissionMode"))
                {
                    s.permission_mode = v;
                }
            }
            _ => {}
        }
        return;
    }

    // --- message lines ---
    let is_user = memmem::find(f, b"\"role\":\"user\"").is_some();
    let is_asst = !is_user && memmem::find(f, b"\"role\":\"assistant\"").is_some();

    // Whole line, not the truncated front: the search scans all of it, and
    // the two have to reach the same verdict. `isMeta` is a top-level field
    // and lands wherever the writer put it -- in one real transcript it sat
    // 410 bytes from the end of a 268KB line.
    //
    // Nor a tool's result. It comes back as a user line, and a result can be
    // a list of `{"type":"text"}` blocks just like prose -- MCP output,
    // page dumps, a subagent's report -- which took 1.7M characters of tool
    // output into an index that exists to leave it out. Across 99,853 such
    // lines here not one also carried anything the user wrote.
    if (is_user || is_asst) && !is_injected_meta(line) && !carries_tool_result(line) {
        if let Some(sink) = text.as_deref_mut() {
            // Bounded, so one pathological session cannot eat the index.
            // Raised well clear of the largest real session (4.5MB here) and
            // no longer silent: losing half a transcript's searchable text
            // should not be something you have to measure to discover.
            if sink.len() < HARVEST_CAP {
                harvest_text(line, sink);
                if sink.len() >= HARVEST_CAP {
                    eprintln!(
                        "mnemosyne: {} is larger than the {}MB index limit — \
                         the rest of it will not be searchable",
                        s.path.display(),
                        HARVEST_CAP >> 20
                    );
                }
            }
        }
    }

    if is_user {
        s.user_msgs += 1;
        if s.first_prompt.is_empty() && memmem::find(f, b"\"isMeta\":true").is_none() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                    if let Some(c) = v.get("message").and_then(|m| m.get("content")) {
                        let t = content_text(c);
                        if is_real_user_text(&t) {
                            s.first_prompt = squash(&t, 200);
                        }
                    }
                }
            }
        }
    } else if is_asst {
        s.assistant_msgs += 1;
        if memmem::find(f, b"\"usage\"").is_some() {
            // Claude Code writes one response as a line per content block --
            // thinking, text, each tool call -- and every line carries the
            // whole response's usage. Summed per line, it was counted once
            // per block: 30,172 usage records here for 16,136 responses, and
            // every total nearly doubled. The first `"id"` of the line is the
            // response's; its blocks are written together, so the one before
            // is all there is to compare with.
            let id = raw_str(f, "id").unwrap_or_default();
            if id.is_empty() || id != s.last_msg_id {
                s.in_tokens += raw_num(f, "input_tokens").unwrap_or(0);
                s.out_tokens += raw_num(f, "output_tokens").unwrap_or(0);
                s.cache_read += raw_num(f, "cache_read_input_tokens").unwrap_or(0);
                s.cache_write += raw_num(f, "cache_creation_input_tokens").unwrap_or(0);
                s.last_msg_id = id;
            }
        }
        if let Some(m) = raw_str(f, "model") {
            // `<synthetic>` marks locally-generated turns; `inherit` and
            // `sniff` are internal placeholders. None are resumable ids.
            if !m.is_empty() && !matches!(m.as_str(), "<synthetic>" | "inherit" | "sniff") {
                s.model = m;
            }
        }
    }
}

/// Scan one transcript, reusing `prev` when the file only grew.
///
/// Without the text, so only for tests: a refresh that read on past new
/// lines without keeping their text would never index them.
#[cfg(test)]
pub fn scan(
    path: &Path,
    is_subagent: bool,
    parent: Option<String>,
    prev: Option<&Session>,
) -> Result<Session> {
    scan_inner(path, is_subagent, parent, prev, None)
}

/// Scan, and also collect the conversation prose for the search index.
pub fn scan_with_text(
    path: &Path,
    is_subagent: bool,
    parent: Option<String>,
    prev: Option<&Session>,
    text: &mut String,
) -> Result<Session> {
    scan_inner(path, is_subagent, parent, prev, Some(text))
}

/// Whether a file now `size` bytes long is read on from where `prev` stopped,
/// rather than from the start. Only the text after that point is harvested,
/// so whoever stores it has to know which of the two happened.
pub fn resumes(prev: &Session, size: u64) -> bool {
    prev.scanned_len > 0 && prev.scanned_len <= size && prev.size <= size
}

fn scan_inner(
    path: &Path,
    is_subagent: bool,
    parent: Option<String>,
    prev: Option<&Session>,
    mut text: Option<&mut String>,
) -> Result<Session> {
    let md = fs::metadata(path)?;
    let size = md.len();
    let mtime = md
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // untouched since last index -> nothing to do
    if let Some(p) = prev {
        if p.size == size && p.mtime == mtime {
            return Ok(p.clone());
        }
    }

    // append-only fast path: resume from where we stopped
    let resume_from = match prev {
        Some(p) if resumes(p, size) => p.scanned_len,
        _ => 0,
    };

    let mut s = if resume_from > 0 {
        prev.unwrap().clone()
    } else {
        let id = if is_subagent {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        Session {
            id,
            path: path.to_path_buf(),
            project_dir: path
                .parent()
                .and_then(|p| {
                    if is_subagent {
                        p.parent().and_then(|q| q.parent())
                    } else {
                        Some(p)
                    }
                })
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            is_subagent,
            parent: parent.clone(),
            ..Default::default()
        }
    };

    let mut file = File::open(path)?;
    if resume_from > 0 {
        file.seek(SeekFrom::Start(resume_from))?;
    }
    let mut rdr = BufReader::with_capacity(1 << 18, file);
    let mut consumed = resume_from;
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 14);

    loop {
        buf.clear();
        let n = rdr.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if !buf.ends_with(b"\n") {
            // A partial last line: Claude is mid-write. Leave it unconsumed so
            // the next run picks it up whole.
            break;
        }
        consumed += n as u64;
        let line = &buf[..n - 1];
        let line = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };
        process_line(&mut s, line, &mut text);
        if buf.capacity() > (1 << 20) {
            buf = Vec::with_capacity(1 << 14);
        }
    }

    s.scanned_len = consumed;
    s.size = size;
    s.mtime = mtime;
    s.path = path.to_path_buf();
    s.is_subagent = is_subagent;
    if s.parent.is_none() {
        s.parent = parent;
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A transcript shaped like the real thing. Note the message lines put a
    /// nested `"type":"text"` content block BEFORE their own top-level
    /// `"type"` -- that ordering is why classifying a line by its first
    /// `"type":"` occurrence is wrong.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("11112222-3333-4444-5555-666677778888.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for line in [
            r#"{"type":"mode","mode":"normal","sessionId":"s"}"#,
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#,
            r#"{"parentUuid":null,"isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>ignore me</system-reminder>"}]},"cwd":"/home/u/proj","gitBranch":"main","version":"2.1.0","type":"user","uuid":"u1","timestamp":"2026-09-01T10:00:00.000Z"}"#,
            r#"{"parentUuid":"u1","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"  fix   the   build  "}]},"cwd":"/home/u/proj","type":"user","uuid":"u2","timestamp":"2026-09-01T10:00:05.000Z"}"#,
            r#"{"parentUuid":"u2","isSidechain":false,"message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"on it"}]},"type":"assistant","uuid":"a1","timestamp":"2026-09-01T10:00:09.000Z"}"#,
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}"#,
            r#"{"type":"ai-title","aiTitle":"Fix the build","sessionId":"s"}"#,
            r#"{"type":"last-prompt","lastPrompt":"ship it","leafUuid":"x","sessionId":"s"}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }
        (dir, path)
    }

    #[test]
    fn a_first_prompt_sent_from_an_ide_is_still_the_first_prompt() {
        let line = br#"{"parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>The user opened src/main.rs</ide_opened_file>"},{"type":"text","text":"why does the build fail"}]},"type":"user"}"#;
        let mut s = Session::default();
        let mut sink: Option<&mut String> = None;
        process_line(&mut s, line, &mut sink);
        assert_eq!(s.first_prompt, "why does the build fail");
    }

    #[test]
    fn squash_never_returns_more_than_it_was_asked_for() {
        assert_eq!(squash("abc def", 4), "abc");
        assert_eq!(squash("abc def", 5), "abc d");
        assert_eq!(squash("  a   b  ", 3), "a b");
        for max in 1..20 {
            assert!(
                squash("one two three four five", max).chars().count() <= max,
                "{max}"
            );
        }
    }

    #[test]
    fn the_name_given_with_rename_is_the_title() {
        // Claude Code's own picker shows it; the list showed the AI title
        // it replaced. Both are written again as the session goes on.
        let mut s = Session::default();
        let mut sink: Option<&mut String> = None;
        for l in [
            r#"{"type":"ai-title","aiTitle":"Discuss project feedback and thoughts","sessionId":"s"}"#,
            r#"{"type":"custom-title","customTitle":"Project feedback","sessionId":"s"}"#,
            r#"{"type":"ai-title","aiTitle":"Discuss project feedback and thoughts","sessionId":"s"}"#,
            r#"{"type":"custom-title","customTitle":"Renamed again","sessionId":"s"}"#,
        ] {
            process_line(&mut s, l.as_bytes(), &mut sink);
        }
        assert_eq!(s.title(), "Renamed again");
        assert_eq!(s.ai_title, "Discuss project feedback and thoughts");
    }

    #[test]
    fn derives_the_fields_we_actually_display() {
        let (_d, path) = fixture();
        let s = scan(&path, false, None, None).unwrap();
        // Claude Code's own title wins over the first user message
        assert_eq!(s.ai_title, "Fix the build");
        assert_eq!(s.title(), "Fix the build");
        assert_eq!(s.last_prompt, "ship it");
        assert_eq!(s.cwd, "/home/u/proj");
        assert_eq!(s.git_branch, "main");
        assert_eq!(s.model, "claude-opus-5");
        // a <system-reminder> turn is not something the user typed
        assert_eq!(s.first_prompt, "fix the build");
    }

    #[test]
    fn counts_messages_correctly() {
        // The naive "first \"type\":\" in the line" rule misreads message
        // lines as `text` blocks; these counts are the regression guard.
        let (_d, path) = fixture();
        let s = scan(&path, false, None, None).unwrap();
        assert_eq!(s.user_msgs, 2);
        assert_eq!(s.assistant_msgs, 1);
        assert_eq!(s.entries, 8);
    }

    #[test]
    fn permission_mode_is_the_one_it_started_in() {
        // The fixture starts `default` and later switches to bypass. Resuming
        // should honour how it began, not how it ended.
        let (_d, path) = fixture();
        let s = scan(&path, false, None, None).unwrap();
        assert_eq!(s.permission_mode, "default");
    }

    #[test]
    fn timestamps_span_first_to_last() {
        let (_d, path) = fixture();
        let s = scan(&path, false, None, None).unwrap();
        assert!(s.first_ts > 0 && s.last_ts >= s.first_ts);
        assert_eq!(s.duration_secs(), 9);
    }

    #[test]
    fn appending_only_reads_the_new_tail() {
        let (_d, path) = fixture();
        let first = scan(&path, false, None, None).unwrap();
        let consumed = first.scanned_len;
        assert_eq!(consumed, std::fs::metadata(&path).unwrap().len());

        // grow the file, then rescan reusing the previous result
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            r#"{{"parentUuid":"a1","isSidechain":false,"message":{{"role":"user","content":[{{"type":"text","text":"and again"}}]}},"type":"user","uuid":"u3","timestamp":"2026-09-01T10:05:00.000Z"}}"#
        )
        .unwrap();
        drop(f);

        let second = scan(&path, false, None, Some(&first)).unwrap();
        assert_eq!(second.user_msgs, 3, "counts accumulate across the delta");
        assert_eq!(second.entries, 9);
        assert!(second.scanned_len > consumed);
        // the head-derived fields survive an incremental pass
        assert_eq!(second.cwd, "/home/u/proj");
        assert_eq!(second.permission_mode, "default");
    }

    #[test]
    fn unchanged_file_is_not_reread() {
        let (_d, path) = fixture();
        let first = scan(&path, false, None, None).unwrap();
        let again = scan(&path, false, None, Some(&first)).unwrap();
        assert_eq!(again.entries, first.entries);
        assert_eq!(again.scanned_len, first.scanned_len);
    }

    #[test]
    fn a_partial_trailing_line_is_left_for_next_time() {
        // Claude may be mid-write; a half-written line must not be consumed.
        let (_d, path) = fixture();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, r#"{{"type":"ai-title","aiTitle":"half"#).unwrap();
        drop(f);
        let s = scan(&path, false, None, None).unwrap();
        assert_eq!(s.ai_title, "Fix the build", "partial line ignored");
        assert!(s.scanned_len < std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn squash_collapses_whitespace_and_clips() {
        assert_eq!(squash("  a \n b\tc  ", 99), "a b c");
        assert_eq!(squash("abcdef", 3), "abc");
        // must not panic on multi-byte input
        assert_eq!(squash("héllo wörld", 5).chars().count(), 5);
    }

    #[test]
    fn squash_drops_control_characters() {
        // A bell counted as a character when a column was measured and drew
        // nothing, so one title shifted every column after it by one. An
        // ESC would have been handed straight to the terminal.
        assert_eq!(squash("a\x07b", 99), "ab");
        assert_eq!(squash("x\x1b[31mred", 99), "x[31mred");
        assert_eq!(squash("keep \u{2014} this", 99), "keep — this");
    }

    #[test]
    fn synthetic_user_turns_are_not_titles() {
        assert!(!is_real_user_text("<command-name>/foo</command-name>"));
        assert!(!is_real_user_text("Caveat: the messages below..."));
        assert!(!is_real_user_text("   "));
        assert!(is_real_user_text("actually do the thing"));
    }
}

#[cfg(test)]
mod harvest_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn token_usage_is_summed() {
        let line = br#"{"parentUuid":"x","isSidechain":false,"message":{"model":"claude-opus-5","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":2985,"cache_creation_input_tokens":2998,"cache_read_input_tokens":41234,"output_tokens":57}},"type":"assistant","uuid":"a1"}"#;
        let mut s = Session::default();
        let mut sink: Option<&mut String> = None;
        process_line(&mut s, line, &mut sink);
        assert_eq!(s.assistant_msgs, 1, "classified as an assistant turn");
        assert_eq!(s.in_tokens, 2985);
        assert_eq!(s.out_tokens, 57);
        assert_eq!(s.cache_read, 41234);
        assert_eq!(s.cache_write, 2998);
    }

    #[test]
    fn a_line_cut_off_after_a_backslash_does_not_stop_the_scan() {
        // A value that runs to the end of the line and ends mid-escape: the
        // walk steps past the end, and the next search began from there --
        // a panic on every refresh, for as long as the file stayed as it was.
        for line in [
            &br#"{"message":{"role":"user","content":"cut off\"#[..],
            &br#"{"message":{"role":"assistant","content":[{"type":"tool_use","input":{"command":"ls \"#[..],
            &br#"{"message":{"role":"assistant","content":[{"type":"text","text":"half\"#[..],
        ] {
            let mut out = String::new();
            harvest_text(line, &mut out);
            let mut s = Session::default();
            let mut text = String::new();
            process_line(&mut s, line, &mut Some(&mut text));
        }
    }

    /// One content block of the response `id`, as Claude Code writes it.
    fn block(id: &str, content: &str) -> String {
        format!(
            r#"{{"parentUuid":"x","message":{{"model":"claude-opus-5","id":"{id}","type":"message","role":"assistant","content":[{content}],"usage":{{"input_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":4000,"output_tokens":100}}}},"type":"assistant"}}"#
        )
    }

    #[test]
    fn a_response_written_a_block_per_line_is_counted_once() {
        let mut s = Session::default();
        let mut sink: Option<&mut String> = None;
        for l in [
            block("msg_1", r#"{"type":"thinking","thinking":"hmm"}"#),
            block("msg_1", r#"{"type":"text","text":"let me look"}"#),
            block(
                "msg_1",
                r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}"#,
            ),
            block("msg_2", r#"{"type":"text","text":"done"}"#),
        ] {
            process_line(&mut s, l.as_bytes(), &mut sink);
        }
        assert_eq!(s.assistant_msgs, 4, "each line is still a turn");
        assert_eq!(s.out_tokens, 200, "two responses, not four");
        assert_eq!(s.in_tokens, 6);
        assert_eq!(s.cache_read, 8000);
        assert_eq!(s.cache_write, 40);
    }

    #[test]
    fn a_response_split_across_two_scans_is_counted_once() {
        // The scan can stop between one response's blocks, and pick up the
        // rest next time from where it stopped.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "{}", block("msg_1", r#"{"type":"text","text":"a"}"#)).unwrap();
        drop(f);
        let first = scan(&p, false, None, None).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", block("msg_1", r#"{"type":"text","text":"b"}"#)).unwrap();
        drop(f);
        let second = scan(&p, false, None, Some(&first)).unwrap();
        assert_eq!(second.out_tokens, 100);
    }

    #[test]
    fn raw_num_reads_only_the_exact_key() {
        let b = br#"{"output_tokens":57,"output_tokens_details":{"thinking_tokens":9}}"#;
        assert_eq!(raw_num(b, "output_tokens"), Some(57));
        assert_eq!(raw_num(b, "thinking_tokens"), Some(9));
        assert_eq!(raw_num(b, "nope"), None);
    }

    #[test]
    fn harvest_takes_prose_and_leaves_scaffolding() {
        let line = br#"{"message":{"role":"assistant","content":[{"type":"text","text":"the disk is full"},{"type":"tool_use","name":"Bash","input":{"command":"zpool status"}}]},"type":"assistant"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(out.contains("the disk is full"));
        // the command that ran is worth finding, the JSON around it is not
        assert!(out.contains("zpool status"));
        assert!(!out.contains("tool_use"));
        assert!(!out.contains("role"));
    }

    #[test]
    fn harvest_drops_injected_context() {
        // Otherwise every session matches every word in the memory file.
        let line = br#"{"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>NVENC needs cuda</system-reminder>check the disk"}]},"type":"user"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(out.contains("check the disk"));
        assert!(!out.to_lowercase().contains("nvenc"));
    }

    #[test]
    fn a_message_stored_as_a_plain_string_is_harvested() {
        // Not every message is a list of blocks; many are
        // `"content":"<the text>"`, and those are overwhelmingly what the
        // user typed. Taking only block content left 16% of the prose in a
        // real corpus unfindable by the default search.
        let line = br#"{"message":{"role":"user","content":"help me figure out why beamng is slow"},"type":"user"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(
            out.contains("beamng"),
            "string content was skipped: {out:?}"
        );
    }

    #[test]
    fn tool_output_is_still_left_out_of_the_index() {
        // The anchor is the role, so a tool_result's own "content" -- which
        // is output, not conversation -- stays out.
        let line = br#"{"message":{"role":"user","content":[{"type":"tool_result","content":"ripgrep found 4000 lines"}]},"type":"user"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(!out.contains("ripgrep"), "tool output leaked in: {out:?}");
    }

    #[test]
    fn tool_output_shaped_like_prose_is_left_out_too() {
        // MCP tools, page dumps and subagent reports come back as a list of
        // text blocks, which the harvester took for conversation.
        let line = br#"{"message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":[{"type":"text","text":"Ran Playwright code page dump"}]}]},"type":"user"}"#;
        let mut s = Session::default();
        let mut text = String::new();
        process_line(&mut s, line, &mut Some(&mut text));
        assert!(
            !text.contains("Playwright"),
            "tool output leaked in: {text:?}"
        );
        assert_eq!(s.user_msgs, 1, "it is still a turn");
    }

    #[test]
    fn injected_context_is_not_conversation() {
        let meta = br#"{"message":{"role":"user","content":"remember the zpool is raidz2"},"type":"user","isMeta":true}"#;
        let real = br#"{"message":{"role":"user","content":"remember the zpool is raidz2"},"type":"user"}"#;
        assert!(is_injected_meta(meta));
        assert!(!is_injected_meta(real));
    }

    #[test]
    fn harvest_takes_thinking() {
        let line = br#"{"message":{"role":"assistant","content":[{"type":"thinking","thinking":"maybe the pool is degraded"}]},"type":"assistant"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(out.contains("degraded"));
    }

    #[test]
    fn harvest_unescapes_enough_to_tokenise() {
        let line = br#"{"message":{"role":"user","content":[{"type":"text","text":"line one\nline two\ttabbed \"quoted\""}]},"type":"user"}"#;
        let mut out = String::new();
        harvest_text(line, &mut out);
        assert!(
            out.contains("line one line two"),
            "escapes became spaces: {out:?}"
        );
        assert!(out.contains("tabbed"));
        assert!(out.contains("quoted"));
    }
}

#[cfg(test)]
mod diag {
    use super::*;
    #[test]
    #[ignore]
    fn scan_a_real_transcript() {
        let p = std::env::var("DIAG_FILE").expect("DIAG_FILE");
        let s = scan(std::path::Path::new(&p), false, None, None).unwrap();
        eprintln!(
            "entries={} user={} asst={} in={} out={} cr={} cw={}",
            s.entries,
            s.user_msgs,
            s.assistant_msgs,
            s.in_tokens,
            s.out_tokens,
            s.cache_read,
            s.cache_write
        );
        assert!(s.entries > 0);
    }
}

#[cfg(test)]
mod robustness_tests {
    use super::*;
    use std::io::Write;

    fn scan_lines(lines: &[&str]) -> Session {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("11112222-3333-4444-5555-666677778888.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        scan(&path, false, None, None).unwrap()
    }

    #[test]
    fn an_empty_transcript_scans_to_nothing() {
        let s = scan_lines(&[]);
        assert_eq!(s.entries, 0);
        assert_eq!(s.title(), "(untitled session)");
        assert_eq!(s.duration_secs(), 0);
    }

    #[test]
    fn junk_lines_do_not_derail_the_scan() {
        let s = scan_lines(&[
            "not json",
            "",
            "{\"unterminated\": ",
            r#"{"type":"ai-title","aiTitle":"survived","sessionId":"s"}"#,
        ]);
        assert_eq!(s.ai_title, "survived");
    }

    #[test]
    fn carriage_returns_are_not_part_of_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let line = r#"{"type":"ai-title","aiTitle":"windows line","sessionId":"s"}"#;
        write!(f, "{line}\r\n").unwrap();
        drop(f);
        let s = scan(&path, false, None, None).unwrap();
        assert_eq!(s.ai_title, "windows line");
    }

    #[test]
    fn a_file_with_no_trailing_newline_still_gets_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"ai-title","aiTitle":"no newline","sessionId":"s"}"#,
        )
        .unwrap();
        let s = scan(&path, false, None, None).unwrap();
        // deliberately left unconsumed: a line without its terminator may be
        // a write in progress
        assert_eq!(s.ai_title, "");
        assert_eq!(s.scanned_len, 0);
    }

    #[test]
    fn unicode_survives_the_round_trip() {
        let s = scan_lines(&[
            r#"{"type":"ai-title","aiTitle":"Чёрный альбом — 日本語 ≈","sessionId":"s"}"#,
        ]);
        assert_eq!(s.ai_title, "Чёрный альбом — 日本語 ≈");
    }

    #[test]
    fn the_latest_title_wins() {
        let s = scan_lines(&[
            r#"{"type":"ai-title","aiTitle":"first guess","sessionId":"s"}"#,
            r#"{"type":"ai-title","aiTitle":"better title","sessionId":"s"}"#,
        ]);
        assert_eq!(s.ai_title, "better title");
    }

    #[test]
    fn placeholder_models_are_not_recorded() {
        for bogus in ["<synthetic>", "inherit", "sniff"] {
            let line = format!(
                r#"{{"message":{{"role":"assistant","model":"{bogus}","content":[]}},"type":"assistant"}}"#
            );
            let s = scan_lines(&[&line]);
            assert_eq!(s.model, "", "{bogus} should not be treated as a model");
        }
    }

    #[test]
    fn the_last_real_model_is_the_one_kept() {
        let s = scan_lines(&[
            r#"{"message":{"role":"assistant","model":"claude-opus-4-8","content":[]},"type":"assistant"}"#,
            r#"{"message":{"role":"assistant","model":"<synthetic>","content":[]},"type":"assistant"}"#,
            r#"{"message":{"role":"assistant","model":"claude-opus-5","content":[]},"type":"assistant"}"#,
        ]);
        assert_eq!(s.model, "claude-opus-5");
    }

    #[test]
    fn a_title_falls_back_to_the_opening_prompt_then_to_a_placeholder() {
        let s = scan_lines(&[
            r#"{"message":{"role":"user","content":[{"type":"text","text":"just do the thing"}]},"type":"user"}"#,
        ]);
        assert_eq!(s.title(), "just do the thing");

        let empty = scan_lines(&[r#"{"type":"mode","mode":"normal","sessionId":"s"}"#]);
        assert_eq!(empty.title(), "(untitled session)");
    }

    #[test]
    fn token_counts_accumulate_over_many_turns() {
        let turn = r#"{"message":{"role":"assistant","model":"claude-opus-5","content":[],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":1}},"type":"assistant"}"#;
        let s = scan_lines(&[turn, turn, turn]);
        assert_eq!(s.in_tokens, 30);
        assert_eq!(s.out_tokens, 15);
        assert_eq!(s.cache_read, 300);
        assert_eq!(s.cache_write, 3);
        assert_eq!(s.total_tokens(), 348);
    }

    #[test]
    fn a_turn_without_usage_contributes_nothing() {
        let s = scan_lines(&[
            r#"{"message":{"role":"assistant","model":"claude-opus-5","content":[]},"type":"assistant"}"#,
        ]);
        assert_eq!(s.total_tokens(), 0);
    }

    #[test]
    fn subagents_know_who_their_parent_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-abc.jsonl");
        std::fs::write(&path, "{\"type\":\"mode\",\"mode\":\"normal\"}\n").unwrap();
        let s = scan(&path, true, Some("parent-id".into()), None).unwrap();
        assert!(s.is_subagent);
        assert_eq!(s.parent.as_deref(), Some("parent-id"));
        assert_eq!(s.title(), "(subagent)");
    }

    #[test]
    fn a_shrinking_file_is_rescanned_from_scratch() {
        // Truncation means our byte offset is meaningless.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                r#"{"type":"ai-title","aiTitle":"long version","sessionId":"s"}"#,
                r#"{"message":{"role":"user","content":[{"type":"text","text":"hello"}]},"type":"user"}"#
            ),
        )
        .unwrap();
        let first = scan(&path, false, None, None).unwrap();
        assert_eq!(first.entries, 2);

        std::fs::write(
            &path,
            format!(
                "{}\n",
                r#"{"type":"ai-title","aiTitle":"short","sessionId":"s"}"#
            ),
        )
        .unwrap();
        let second = scan(&path, false, None, Some(&first)).unwrap();
        assert_eq!(
            second.entries, 1,
            "counted fresh, not added to the old total"
        );
        assert_eq!(second.ai_title, "short");
    }
}

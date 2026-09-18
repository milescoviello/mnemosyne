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

fn iso_to_epoch(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// Flatten a message `content` field (string, or array of blocks) to text.
fn content_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let mut parts = Vec::new();
            for b in a {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        parts.push(t);
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

pub fn squash(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(s.len()));
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
        if out.chars().count() >= max {
            break;
        }
    }
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

fn process_line(s: &mut Session, line: &[u8]) {
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
pub fn scan(
    path: &Path,
    is_subagent: bool,
    parent: Option<String>,
    prev: Option<&Session>,
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
        Some(p) if p.scanned_len > 0 && p.scanned_len <= size && p.size <= size => p.scanned_len,
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
        process_line(&mut s, line);
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
    fn synthetic_user_turns_are_not_titles() {
        assert!(!is_real_user_text("<command-name>/foo</command-name>"));
        assert!(!is_real_user_text("Caveat: the messages below..."));
        assert!(!is_real_user_text("   "));
        assert!(is_real_user_text("actually do the thing"));
    }
}

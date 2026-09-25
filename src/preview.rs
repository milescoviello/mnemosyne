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
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let role = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => "you",
            Some("assistant") => "claude",
            _ => continue,
        };
        let Some(c) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let text = flatten(c, role == "you");
        let text = crate::scan::squash(&text, 700);
        // Only what you sent is checked for being machinery: Claude's reply
        // is never an injected envelope, and one that happens to open with
        // `<` is still what it said.
        if text.is_empty() || (role == "you" && !crate::scan::is_real_user_text(&text)) {
            continue;
        }
        turns.push(Turn { role, text });
    }
    turns
}

/// A message's content as one line of text.
///
/// For what you sent, each block is judged on its own. A message typed in
/// an IDE arrives as an `<ide_opened_file>` block and then what you wrote;
/// joined first and judged after, the whole turn looked like machinery and
/// vanished from the rail and the viewer.
fn flatten(c: &serde_json::Value, user: bool) -> String {
    match c {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => {
            let mut parts: Vec<String> = Vec::new();
            for b in a {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !user || crate::scan::is_real_user_text(t) {
                                parts.push(t.to_string());
                            }
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

/// How long the file is now, not when it was last indexed.
///
/// The window read is the last so many bytes. Measured from the indexed
/// size, a session that had grown since -- a live one, or any on the first
/// frame before the rescan lands -- showed an older end, and in the viewer
/// the newest turns were simply missing.
fn current_len(f: &std::fs::File, s: &Session) -> u64 {
    f.metadata().map(|m| m.len()).unwrap_or(s.size)
}

/// Load turns for the full-screen viewer.
///
/// Reads from the end rather than the start: a session here can be 400 MB and
/// you almost always want the recent end of it. Returns whether anything was
/// left off, so the viewer can say so instead of pretending it showed you
/// everything.
pub fn load_turns(s: &Session, max_bytes: u64, want: usize) -> (Vec<Turn>, bool) {
    let Ok(mut f) = std::fs::File::open(&s.path) else {
        return (Vec::new(), false);
    };
    let len = current_len(&f, s);
    let (start, partial) = if len > max_bytes {
        (len - max_bytes, true)
    } else {
        (0, false)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), partial);
    }
    let mut buf = Vec::new();
    if (&mut f)
        .take(max_bytes + 4096)
        .read_to_end(&mut buf)
        .is_err()
    {
        return (Vec::new(), partial);
    }
    let mut turns = extract_turns(&buf, partial);
    let clipped = turns.len() > want;
    if clipped {
        let n = turns.len();
        turns.drain(..n - want);
    }
    (turns, partial || clipped)
}

/// The last `want` readable turns of a session.
pub fn tail_turns(s: &Session, want: usize) -> Vec<Turn> {
    let Ok(mut f) = std::fs::File::open(&s.path) else {
        return Vec::new();
    };
    let len = current_len(&f, s);
    let (start, partial) = if len > TAIL_BYTES {
        (len - TAIL_BYTES, true)
    } else {
        (0, false)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if (&mut f)
        .take(TAIL_BYTES + 4096)
        .read_to_end(&mut buf)
        .is_err()
    {
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
                // Partial only if it starts part way in: from the top of
                // the file, the first line is a whole one.
                turns = extract_turns(&buf, len > bigger);
            }
        }
    }
    let n = turns.len();
    if n > want {
        turns.drain(..n - want);
    }
    turns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;
    use std::io::Write;

    fn write_transcript(lines: &[String]) -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        let size = std::fs::metadata(&path).unwrap().len();
        let s = Session {
            path,
            size,
            ..Default::default()
        };
        (dir, s)
    }

    fn user(text: &str) -> String {
        format!(
            r#"{{"parentUuid":"p","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}},"type":"user"}}"#
        )
    }
    fn asst(text: &str) -> String {
        format!(
            r#"{{"parentUuid":"p","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"type":"assistant"}}"#
        )
    }

    #[test]
    fn a_message_sent_from_an_ide_is_still_what_you_said() {
        let ide = r#"{"parentUuid":"p","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>The user opened src/main.rs</ide_opened_file>"},{"type":"text","text":"why does this fail"}]},"type":"user"}"#;
        let (_d, s) = write_transcript(&[ide.to_string(), asst("because")]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0].text, "why does this fail");
        assert_eq!(load_turns(&s, 1 << 20, 8).0.len(), 2);
    }

    #[test]
    fn what_was_said_since_the_last_index_is_shown() {
        // The window is the file's last bytes, and was measured from its
        // indexed size -- so what came after that was left off.
        let old: Vec<String> = (0..800)
            .map(|i| user(&format!("old question {i} {}", "x".repeat(700))))
            .collect();
        let (_d, s) = write_transcript(&old); // indexed at this size
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        for i in 0..12 {
            writeln!(
                f,
                "{}",
                asst(&format!("since then {i} {}", "y".repeat(700)))
            )
            .unwrap();
        }
        writeln!(f, "{}", user("the newest question")).unwrap();
        drop(f);
        assert_eq!(
            tail_turns(&s, 8).last().unwrap().text,
            "the newest question"
        );
        assert_eq!(
            load_turns(&s, 256 << 10, 8).0.last().unwrap().text,
            "the newest question"
        );
    }

    #[test]
    fn a_first_line_read_from_the_top_of_the_file_is_not_dropped_as_partial() {
        // One readable turn and then a line too big for the first window:
        // the second, bigger read starts at the top of the file, where the
        // first line is a whole one and not a fragment to skip.
        let blob = format!(
            r#"{{"message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Read","input":{{"x":"{}"}}}}]}},"type":"progress"}}"#,
            "b".repeat(600 * 1024)
        );
        let (_d, s) = write_transcript(&[user("the only real turn"), blob]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].text, "the only real turn");
    }

    #[test]
    fn reads_the_last_turns_in_order() {
        let (_d, s) = write_transcript(&[user("one"), asst("two"), user("three")]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].role, "you");
        assert_eq!(t[2].text, "three");
    }

    #[test]
    fn asking_for_fewer_gives_the_most_recent() {
        let (_d, s) = write_transcript(&[user("one"), asst("two"), user("three")]);
        let t = tail_turns(&s, 1);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].text, "three");
    }

    #[test]
    fn tool_calls_are_named_not_dumped() {
        let line = r#"{"parentUuid":"p","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la /very/long/path"}}]},"type":"assistant"}"#;
        let (_d, s) = write_transcript(&[line.to_string()]);
        let t = tail_turns(&s, 4);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].text, "[Bash]");
    }

    #[test]
    fn injected_and_meta_turns_are_skipped() {
        let meta = r#"{"isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"bookkeeping"}]},"type":"user"}"#;
        let reminder = user("<system-reminder>not something you said</system-reminder>");
        let (_d, s) = write_transcript(&[meta.to_string(), reminder, user("real question")]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 1, "got {t:?}");
        assert_eq!(t[0].text, "real question");
    }

    #[test]
    fn malformed_lines_are_stepped_over() {
        let (_d, s) = write_transcript(&[
            user("before"),
            "{not json at all".into(),
            String::new(),
            asst("after"),
        ]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].text, "after");
    }

    #[test]
    fn an_empty_transcript_yields_nothing_rather_than_panicking() {
        let (_d, s) = write_transcript(&[]);
        assert!(tail_turns(&s, 8).is_empty());
    }

    #[test]
    fn a_missing_file_yields_nothing() {
        let s = Session {
            path: "/definitely/not/here.jsonl".into(),
            size: 999,
            ..Default::default()
        };
        assert!(tail_turns(&s, 8).is_empty());
        assert_eq!(load_turns(&s, 1 << 20, 10).0.len(), 0);
    }

    #[test]
    fn load_turns_reports_when_it_left_something_out() {
        let many: Vec<String> = (0..40).map(|i| user(&format!("line {i}"))).collect();
        let (_d, s) = write_transcript(&many);
        let (turns, more) = load_turns(&s, 1 << 20, 10);
        assert_eq!(turns.len(), 10);
        assert!(more, "it clipped, so it must say so");
        let (all, more) = load_turns(&s, 1 << 20, 500);
        assert_eq!(all.len(), 40);
        assert!(!more, "nothing was left out");
    }

    #[test]
    fn a_tiny_byte_budget_still_returns_something_readable() {
        let many: Vec<String> = (0..200).map(|i| user(&format!("line {i}"))).collect();
        let (_d, s) = write_transcript(&many);
        let (turns, more) = load_turns(&s, 2048, 500);
        assert!(!turns.is_empty(), "the tail should still parse");
        assert!(more, "reading only the tail means there was more");
        assert!(turns.last().unwrap().text.contains("199"));
    }
}

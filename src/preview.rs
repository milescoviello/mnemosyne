//! Conversation preview.
//!
//! The old tool read the entire transcript to show the last handful of
//! messages, which on the biggest file here means reading 386 MB to print
//! eight lines. We seek to the end and read backwards instead, so preview cost
//! is independent of session size.

use crate::model::{Harness, Session};
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Debug)]
pub struct Turn {
    pub role: &'static str,
    pub text: String,
}

const TAIL_BYTES: u64 = 512 * 1024;

/// A turn as it is shown: its first 700 characters -- or, when what was
/// searched for is only further on, the start of it and the stretch around
/// that, so the viewer has the match to show.
fn shown(full: &str, find: Option<&crate::search::Spotter>) -> String {
    let text = crate::scan::squash(full, 700);
    let Some(find) = find.filter(|f| !f.is_empty()) else {
        return text;
    };
    // Cut where it is when the cut has all the turn has: with any one of
    // several words in its first 700 characters, the rest were past the
    // end and never shown.
    if find.count(&text) >= find.count(full) {
        return text;
    }
    // The start, then the stretch around what the start does not have.
    let head = crate::scan::squash(full, 300);
    let after = full.char_indices().nth(300).map_or(full.len(), |(i, _)| i);
    format!(
        "{head} {}",
        crate::search::excerpt_by(&full[after..], &find.missing_from(&head))
    )
}

fn extract_turns(
    h: Harness,
    bytes: &[u8],
    drop_first_partial: bool,
    find: Option<&crate::search::Spotter>,
) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut iter = bytes.split(|b| *b == b'\n');
    if drop_first_partial {
        iter.next();
    }
    for line in iter {
        if line.is_empty() {
            continue;
        }
        let Some(v) = crate::scan::parse_line(line) else {
            continue;
        };
        let Some((role, said)) = (match h {
            Harness::Claude => claude_turn(&v),
            Harness::Pi | Harness::Omp => pi_turn(h, &v),
            Harness::Codex => codex_turn(&v),
            Harness::Hermes => None,
        }) else {
            continue;
        };
        let text = shown(&said, find);
        // Only what you sent is checked for being machinery: the agent's
        // reply is never an injected envelope, and one that happens to open
        // with `<` is still what it said.
        if text.is_empty() || (role == "you" && !crate::scan::is_real_user_text(&text)) {
            continue;
        }
        turns.push(Turn { role, text });
    }
    turns
}

fn claude_turn(v: &serde_json::Value) -> Option<(&'static str, String)> {
    if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }
    let role = match v.get("type").and_then(|t| t.as_str()) {
        Some("user") => "you",
        Some("assistant") => "claude",
        _ => return None,
    };
    let c = v.get("message").and_then(|m| m.get("content"))?;
    Some((role, flatten(c, role == "you")))
}

/// pi's and omp's: a `message` entry, whose tools' results are left out
/// as Claude's are. A command run with `!` is yours.
fn pi_turn(h: Harness, v: &serde_json::Value) -> Option<(&'static str, String)> {
    if v.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    let m = v.get("message")?;
    match m.get("role").and_then(|r| r.as_str())? {
        "user" => Some(("you", flatten(m.get("content")?, true))),
        "assistant" => Some((h.name(), flatten(m.get("content")?, false))),
        "bashExecution" => Some(("you", format!("!{}", m.get("command")?.as_str()?))),
        _ => None,
    }
}

/// Codex's: what went to and from the model, without the context it sends
/// along, and each command it ran named as Claude's tools are.
fn codex_turn(v: &serde_json::Value) -> Option<(&'static str, String)> {
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }
    let p = v.get("payload")?;
    let blocks = |kind: &str| -> Vec<String> {
        p.get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(kind))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect()
    };
    match p.get("type").and_then(|t| t.as_str())? {
        "message" => match p.get("role").and_then(|r| r.as_str())? {
            "user" => {
                let said: Vec<String> = blocks("input_text")
                    .into_iter()
                    .filter(|t| !crate::codex::sent_along(t))
                    .collect();
                Some(("you", said.join(" ")))
            }
            "assistant" => Some(("codex", blocks("output_text").join(" "))),
            _ => None,
        },
        "function_call" | "custom_tool_call" => {
            let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            Some(("codex", format!("[{name}]")))
        }
        _ => None,
    }
}

/// A Hermes session's last `want` turns, from its database, and whether
/// there were more.
fn hermes_turns(s: &Session, want: usize) -> (Vec<Turn>, bool) {
    let Some((db, id)) = crate::hermes::split_key(&s.path.to_string_lossy()) else {
        return (Vec::new(), false);
    };
    let Some(c) = crate::hermes::open(&db) else {
        return (Vec::new(), false);
    };
    // Newest first, only what was said -- the tools' rows can be many and
    // long, and are not shown, and a reply that only called a tool is empty
    // -- each cut to what a turn shows, and one more
    // than asked for, to know whether there was more.
    let limit = (want + 1) as i64;
    let Ok(mut st) = c.prepare(
        "SELECT role, substr(content, 1, 8192) FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant')
           AND trim(coalesce(content, '')) != ''
         ORDER BY id DESC LIMIT ?2",
    ) else {
        return (Vec::new(), false);
    };
    let Ok(rows) = st.query_map(rusqlite::params![id, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    }) else {
        return (Vec::new(), false);
    };
    let rows: Vec<(String, Option<String>)> = rows.flatten().collect();
    let mut more = rows.len() as i64 >= limit;
    let mut turns: Vec<Turn> = rows
        .into_iter()
        .rev()
        .filter_map(|(role, content)| {
            let role = match role.as_str() {
                "user" => "you",
                "assistant" => "hermes",
                _ => return None,
            };
            let text = crate::scan::squash(content.as_deref()?, 700);
            (!text.is_empty() && (role != "you" || crate::scan::is_real_user_text(&text)))
                .then_some(Turn { role, text })
        })
        .collect();
    if turns.len() > want {
        turns.drain(..turns.len() - want);
        more = true;
    }
    (turns, more)
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
                    // Claude's, and pi's
                    Some("tool_use") | Some("toolCall") => {
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
///
/// A turn too long to show whole keeps in sight what `find` finds in it.
pub fn load_turns(
    s: &Session,
    max_bytes: u64,
    want: usize,
    find: Option<&crate::search::Spotter>,
) -> (Vec<Turn>, bool) {
    if s.harness == Harness::Hermes {
        let (mut turns, more) = hermes_turns(s, want);
        for t in &mut turns {
            t.text = shown(&t.text, find);
        }
        return (turns, more);
    }
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
    let mut turns = extract_turns(s.harness, &buf, partial, find);
    let clipped = turns.len() > want;
    if clipped {
        let n = turns.len();
        turns.drain(..n - want);
    }
    (turns, partial || clipped)
}

/// The last `want` readable turns of a session.
pub fn tail_turns(s: &Session, want: usize) -> Vec<Turn> {
    if s.harness == Harness::Hermes {
        return hermes_turns(s, want).0;
    }
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
    let mut turns = extract_turns(s.harness, &buf, partial, None);
    // A single enormous final message can swallow the whole window; if we found
    // nothing at all, fall back to a bigger bite before giving up.
    if turns.is_empty() && start > 0 {
        let bigger = (TAIL_BYTES * 8).min(len);
        if f.seek(SeekFrom::Start(len - bigger)).is_ok() {
            buf.clear();
            if (&mut f).take(bigger + 4096).read_to_end(&mut buf).is_ok() {
                // Partial only if it starts part way in: from the top of
                // the file, the first line is a whole one.
                turns = extract_turns(s.harness, &buf, len > bigger, None);
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
    fn a_long_turn_keeps_in_sight_what_was_searched_for() {
        // Turns are cut at 700 characters, and a match past that was not
        // there to be shown.
        let long = format!(
            "{} the zpool is degraded {}",
            "a ".repeat(600),
            "b ".repeat(300)
        );
        let (_d, s) = write_transcript(&[user(&long), asst("ok")]);
        let find = crate::search::Spotter::new("zpool");
        let (turns, _) = load_turns(&s, 1 << 20, 10, Some(&find));
        assert!(
            turns[0].text.contains("zpool is degraded"),
            "{:?}",
            turns[0].text
        );
        assert!(
            turns[0].text.starts_with("a a"),
            "and still begins at the start"
        );
        // without a search it is cut as it was
        let (turns, _) = load_turns(&s, 1 << 20, 10, None);
        assert!(!turns[0].text.contains("zpool"));
        // and with several words, the one past the cut is still brought in
        let long = format!(
            "hyprland is the compositor {} then it crashed",
            "a ".repeat(600)
        );
        let (_d, s) = write_transcript(&[user(&long), asst("ok")]);
        let find = crate::search::Spotter::new("hyprland crashed");
        let (turns, _) = load_turns(&s, 1 << 20, 10, Some(&find));
        assert!(turns[0].text.contains("crashed"), "{:?}", turns[0].text);
        assert!(turns[0].text.starts_with("hyprland"));
    }

    #[test]
    fn a_reply_with_half_an_emoji_in_it_is_still_shown() {
        // JavaScript writes a string cut between the halves of an emoji as
        // a lone `\ud83d`. serde refuses it, and the whole reply was gone
        // from the viewer: fifteen of Claude's, in the transcripts here.
        let (_d, s) = write_transcript(&[
            user("hello"),
            asst(r"cut here \ud83d and a stray \ude00 too"),
            asst(r"a whole one \ud83d\ude00 and an escaped \\ud83d"),
        ]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 3, "{t:?}");
        assert_eq!(t[1].text, "cut here \u{fffd} and a stray \u{fffd} too");
        assert_eq!(t[2].text, "a whole one 😀 and an escaped \\ud83d");
    }

    #[test]
    fn a_message_sent_from_an_ide_is_still_what_you_said() {
        let ide = r#"{"parentUuid":"p","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>The user opened src/main.rs</ide_opened_file>"},{"type":"text","text":"why does this fail"}]},"type":"user"}"#;
        let (_d, s) = write_transcript(&[ide.to_string(), asst("because")]);
        let t = tail_turns(&s, 8);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0].text, "why does this fail");
        assert_eq!(load_turns(&s, 1 << 20, 8, None).0.len(), 2);
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
            load_turns(&s, 256 << 10, 8, None).0.last().unwrap().text,
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

    fn of(h: crate::model::Harness, lines: &[&str]) -> (tempfile::TempDir, Session) {
        let (d, mut s) = write_transcript(&lines.iter().map(|l| l.to_string()).collect::<Vec<_>>());
        s.harness = h;
        (d, s)
    }

    #[test]
    fn a_pi_session_is_shown_as_pi_said_it() {
        use crate::model::Harness;
        let lines = [
            r#"{"type":"session","version":3,"id":"0f6c1d2e-1111-4222-8333-944455556666","timestamp":"2026-10-01T09:00:00.000Z","cwd":"/w"}"#,
            r#"{"type":"message","id":"a2","parentId":null,"timestamp":"2026-10-01T09:00:05.000Z","message":{"role":"user","content":"replace the lexer"}}"#,
            r#"{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-10-01T09:00:09.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Looking at the grammar."},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}]}}"#,
            r#"{"type":"message","id":"a4","parentId":"a3","timestamp":"2026-10-01T09:00:10.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"TOOLOUTPUT"}],"isError":false}}"#,
            r#"{"type":"message","id":"a5","parentId":"a4","timestamp":"2026-10-01T09:01:00.000Z","message":{"role":"bashExecution","command":"cargo test","output":"ok","exitCode":0}}"#,
        ];
        for h in [Harness::Pi, Harness::Omp] {
            let (_d, s) = of(h, &lines);
            let t = tail_turns(&s, 8);
            let got: Vec<(&str, &str)> = t.iter().map(|t| (t.role, t.text.as_str())).collect();
            assert_eq!(
                got,
                [
                    ("you", "replace the lexer"),
                    (h.name(), "Looking at the grammar. [bash]"),
                    ("you", "!cargo test"),
                ]
            );
        }
    }

    #[test]
    fn a_codex_thread_is_shown_without_what_codex_sent_along() {
        use crate::model::Harness;
        let (_d, s) = of(
            Harness::Codex,
            &[
                r#"{"timestamp":"2026-10-06T18:18:40Z","type":"session_meta","payload":{"id":"x","cwd":"/w"}}"#,
                r#"{"timestamp":"2026-10-06T18:18:41Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>INJECTED</environment_context>"}]}}"#,
                r#"{"timestamp":"2026-10-06T18:18:42Z","type":"event_msg","payload":{"type":"user_message","message":"why does the footer overlap"}}"#,
                r#"{"timestamp":"2026-10-06T18:18:42Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"why does the footer overlap"}]}}"#,
                r#"{"timestamp":"2026-10-06T18:18:50Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rg footer\"}","call_id":"c"}}"#,
                r#"{"timestamp":"2026-10-06T18:18:51Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"TOOLOUTPUT"}}"#,
                r#"{"timestamp":"2026-10-06T18:19:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"It is fixed; make it static."}]}}"#,
            ],
        );
        let t = tail_turns(&s, 8);
        let got: Vec<(&str, &str)> = t.iter().map(|t| (t.role, t.text.as_str())).collect();
        assert_eq!(
            got,
            [
                ("you", "why does the footer overlap"),
                ("codex", "[exec_command]"),
                ("codex", "It is fixed; make it static."),
            ]
        );
    }

    #[test]
    fn a_hermes_session_is_read_from_its_database() {
        use crate::hermes::tests::{add_session, make_db, say};
        use crate::model::Harness;
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        add_session(&c, "h1", "cli", Some("/w"));
        add_session(&c, "other", "cli", Some("/w"));
        say(&c, "other", "user", "NOT THIS ONE", 1.0);
        say(&c, "h1", "user", "set up a webhook", 2.0);
        say(&c, "h1", "assistant", "Registering it.", 3.0);
        say(&c, "h1", "tool", "TOOLOUTPUT", 4.0);
        say(&c, "h1", "assistant", "Done.", 5.0);
        let s = Session {
            harness: Harness::Hermes,
            id: "h1".into(),
            path: crate::hermes::key(&db, "h1").into(),
            ..Default::default()
        };
        let got: Vec<(&str, String)> = tail_turns(&s, 8)
            .into_iter()
            .map(|t| (t.role, t.text))
            .collect();
        assert_eq!(
            got,
            [
                ("you", "set up a webhook".to_string()),
                ("hermes", "Registering it.".to_string()),
                ("hermes", "Done.".to_string()),
            ]
        );
        let (two, more) = load_turns(&s, 8 << 20, 2, None);
        assert_eq!(two.len(), 2);
        assert!(more, "the viewer is told it is not the whole of it");
        assert_eq!(two[1].text, "Done.");
    }

    #[test]
    fn a_hermes_session_ending_in_tool_output_still_shows_what_was_said() {
        // Its last rows are a tool's, many of them and long. Counted against
        // the turns asked for, they crowded the conversation out.
        use crate::hermes::tests::{add_session, make_db, say};
        use crate::model::Harness;
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        add_session(&c, "h1", "cli", Some("/w"));
        say(&c, "h1", "user", "run the backups", 1.0);
        say(&c, "h1", "assistant", "Running them.", 2.0);
        for i in 0..40 {
            say(&c, "h1", "tool", &"x".repeat(100_000), 3.0 + i as f64);
        }
        let s = Session {
            harness: Harness::Hermes,
            id: "h1".into(),
            path: crate::hermes::key(&db, "h1").into(),
            ..Default::default()
        };
        let got: Vec<String> = tail_turns(&s, 2).into_iter().map(|t| t.text).collect();
        assert_eq!(got, ["run the backups", "Running them."]);
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
        assert_eq!(load_turns(&s, 1 << 20, 10, None).0.len(), 0);
    }

    #[test]
    fn load_turns_reports_when_it_left_something_out() {
        let many: Vec<String> = (0..40).map(|i| user(&format!("line {i}"))).collect();
        let (_d, s) = write_transcript(&many);
        let (turns, more) = load_turns(&s, 1 << 20, 10, None);
        assert_eq!(turns.len(), 10);
        assert!(more, "it clipped, so it must say so");
        let (all, more) = load_turns(&s, 1 << 20, 500, None);
        assert_eq!(all.len(), 40);
        assert!(!more, "nothing was left out");
    }

    #[test]
    fn a_tiny_byte_budget_still_returns_something_readable() {
        let many: Vec<String> = (0..200).map(|i| user(&format!("line {i}"))).collect();
        let (_d, s) = write_transcript(&many);
        let (turns, more) = load_turns(&s, 2048, 500, None);
        assert!(!turns.is_empty(), "the tail should still parse");
        assert!(more, "reading only the tail means there was more");
        assert!(turns.last().unwrap().text.contains("199"));
    }
}

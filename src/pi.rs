//! pi and oh-my-pi (omp) sessions.
//!
//! Both write pi's session format (v3): a JSONL file per session under
//! `<agent dir>/sessions/<encoded cwd>/<timestamp>_<uuid>.jsonl`, opening with
//! a `{"type":"session"}` header that names the id and the folder, then one
//! entry per line -- `message` entries holding a `user`, `assistant` or
//! `toolResult` message, plus `model_change`, `session_info` (a name given
//! with `/name`) and, from omp, `title`/`title_change`.
//!
//! The folder is read from the header, never from the directory name: both
//! agents encode it lossily (every `/` becomes `-`), and omp has used three
//! different encodings.
//!
//! Entries form a tree, so a file can hold branches the conversation left.
//! They are counted like the rest; the list only needs the gist.

use crate::model::{Harness, Session};
use crate::scan;
use memchr::memmem;
use std::path::PathBuf;

/// Where an agent keeps its sessions.
pub fn sessions_dir(h: Harness) -> Option<PathBuf> {
    let home = crate::paths::home();
    match h {
        // pi lets its whole directory be moved, and says so in --help.
        Harness::Pi => Some(
            std::env::var_os("PI_CODING_AGENT_DIR")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pi/agent"))
                .join("sessions"),
        ),
        Harness::Omp => Some(home.join(".omp/agent/sessions")),
        _ => None,
    }
}

/// Every session file an agent has.
pub fn discover(h: Harness) -> Vec<PathBuf> {
    sessions_dir(h).map(|r| discover_in(&r)).unwrap_or_default()
}

/// Every session file under `root`: `<root>/<folder>/<file>.jsonl`. omp
/// keeps a directory of tool logs beside each file, which is passed over.
fn discover_in(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(folders) = std::fs::read_dir(root) else {
        return out;
    };
    for folder in folders.flatten() {
        let Ok(files) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "jsonl") && p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// Whose message a line holds, read where pi writes it: the first key of
/// its `message`. Looked for anywhere, a role inside a tool call's
/// arguments -- an object, so not escaped -- passed for the message's own.
pub fn role_of(line: &[u8]) -> Option<&str> {
    const AT: &[u8] = br#""message":{"role":""#;
    let from = memmem::find(line, AT)? + AT.len();
    let len = memchr::memchr(b'"', &line[from..])?;
    std::str::from_utf8(&line[from..from + len]).ok()
}

/// Take in one line of a pi-format session.
pub fn process_line(s: &mut Session, line: &[u8], text: &mut Option<&mut String>) {
    if line.is_empty() {
        return;
    }
    s.entries += 1;
    let f = scan::front(line);

    // The entry's own time, an ISO string. A message also carries a
    // `timestamp`, but as a number, which this does not match.
    if let Some(ts) = scan::raw_str(f, "timestamp") {
        scan::note_time(s, &ts);
    }

    let role = role_of(f);
    // What a tool handed back: output, not conversation, and where the big
    // lines are. Counted and nothing more.
    if role == Some("toolResult") {
        return;
    }
    let is_user = role == Some("user");
    let is_asst = role == Some("assistant");
    // A command run with `!` is the user's own doing, and worth finding.
    let is_bash = role == Some("bashExecution");
    if is_user || is_asst || is_bash {
        scan::harvest_capped(s, line, text);
    }
    if line.len() > scan::BIG_LINE {
        // An image pasted in. Its words were taken above; parsing all of it
        // for the rest is not worth eight megabytes a line.
        if is_user {
            s.user_msgs += 1;
        } else if is_asst {
            s.assistant_msgs += 1;
        }
        return;
    }
    let Some(v) = scan::parse_line(line) else {
        return;
    };
    let str_of =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match v.get("type").and_then(|t| t.as_str()) {
        Some("session") => {
            if let Some(id) = str_of(&v, "id").filter(|x| !x.is_empty()) {
                s.id = id;
            }
            if let Some(cwd) = str_of(&v, "cwd") {
                s.cwd = cwd;
            }
            if let Some(t) = str_of(&v, "title").filter(|t| !t.trim().is_empty()) {
                s.ai_title = scan::squash(&t, 160);
            }
        }
        // omp names its sessions itself, and says when you did instead.
        Some("title") | Some("title_change") => {
            if let Some(t) = str_of(&v, "title").filter(|t| !t.trim().is_empty()) {
                let t = scan::squash(&t, 160);
                match v.get("source").and_then(|x| x.as_str()) {
                    Some("auto") | None => s.ai_title = t,
                    Some(_) => s.custom_title = t,
                }
            }
        }
        // pi's `/name`. A later one renames, and one without a name clears.
        Some("session_info") => {
            s.custom_title = str_of(&v, "name")
                .map(|n| scan::squash(&n, 160))
                .unwrap_or_default();
        }
        Some("model_change") => {
            // pi says `modelId`; omp says `model`, as `provider/model`.
            let m = str_of(&v, "modelId").or_else(|| {
                str_of(&v, "model").map(|m| m.rsplit('/').next().unwrap_or(&m).to_string())
            });
            if let Some(m) = m.filter(|m| !m.is_empty()) {
                s.model = m;
            }
        }
        Some("message") => {
            let Some(m) = v.get("message") else {
                return;
            };
            if is_user {
                s.user_msgs += 1;
                if let Some(c) = m.get("content") {
                    let t = scan::content_text(c);
                    if scan::is_real_user_text(&t) {
                        if s.first_prompt.is_empty() {
                            s.first_prompt = scan::squash(&t, 200);
                        }
                        s.last_prompt = scan::squash(&t, 200);
                    }
                }
            } else if is_asst {
                s.assistant_msgs += 1;
                if let Some(model) = str_of(m, "model").filter(|x| !x.is_empty()) {
                    s.model = model;
                }
                if let Some(u) = m.get("usage") {
                    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    s.in_tokens += n("input");
                    s.out_tokens += n("output");
                    s.cache_read += n("cacheRead");
                    s.cache_write += n("cacheWrite");
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A pi session as pi's own documentation (docs/session-format.md)
    /// describes it: header, a named session, a model switch, a turn with a
    /// tool call and its result, and a `!` command.
    const PI: &[&str] = &[
        r#"{"type":"session","version":3,"id":"0f6c1d2e-1111-4222-8333-944455556666","timestamp":"2026-10-01T09:00:00.000Z","cwd":"/home/u/parser"}"#,
        r#"{"type":"model_change","id":"a0000001","parentId":null,"timestamp":"2026-10-01T09:00:00.100Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
        r#"{"type":"message","id":"a0000002","parentId":"a0000001","timestamp":"2026-10-01T09:00:05.000Z","message":{"role":"user","content":"replace the hand-rolled lexer","timestamp":1790845205000}}"#,
        r#"{"type":"message","id":"a0000003","parentId":"a0000002","timestamp":"2026-10-01T09:00:09.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"start with the tokens"},{"type":"text","text":"Looking at the grammar first."},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"rg --files src"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1200,"output":80,"cacheRead":400,"cacheWrite":10,"totalTokens":1690,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1790845209000}}"#,
        r#"{"type":"message","id":"a0000004","parentId":"a0000003","timestamp":"2026-10-01T09:00:10.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"src/lexer.rs SECRETOUTPUT"}],"isError":false,"timestamp":1790845210000}}"#,
        r#"{"type":"session_info","id":"a0000005","parentId":"a0000004","timestamp":"2026-10-01T09:01:00.000Z","name":"Lexer rewrite"}"#,
        r#"{"type":"message","id":"a0000006","parentId":"a0000005","timestamp":"2026-10-01T09:02:00.000Z","message":{"role":"bashExecution","command":"cargo test lexer","output":"ok","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1790845320000}}"#,
        r#"{"type":"message","id":"a0000007","parentId":"a0000006","timestamp":"2026-10-01T09:03:00.000Z","message":{"role":"user","content":[{"type":"text","text":"now the error spans"}],"timestamp":1790845380000}}"#,
        r#"{"type":"message","id":"a0000008","parentId":"a0000007","timestamp":"2026-10-01T09:03:30.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"provider":"anthropic","model":"claude-opus-4-5","usage":{"input":300,"output":20,"cacheRead":0,"cacheWrite":0,"totalTokens":320},"stopReason":"stop","timestamp":1790845410000}}"#,
    ];

    /// omp's shape, as it writes it: a padded title line first, a header
    /// with the title in it too, `provider/model` switches and messages
    /// with `attribution`.
    const OMP: &[&str] = &[
        r#"{"type":"title","v":1,"title":"Find the slow query","source":"auto","updatedAt":"2026-10-02T10:00:20.000Z","pad":"                    "}"#,
        r#"{"type":"session","version":3,"id":"01a10e6c-aaaa-7000-bbbb-0f1c045e7bd0","timestamp":"2026-10-02T10:00:00.000Z","cwd":"/home/u/api","title":"Find the slow query","titleSource":"auto"}"#,
        r#"{"type":"model_change","id":"b0000001","parentId":null,"timestamp":"2026-10-02T10:00:00.050Z","model":"flashnext/flash-next","resolvedModelIsFallback":false}"#,
        r#"{"type":"thinking_level_change","id":"b0000002","parentId":"b0000001","timestamp":"2026-10-02T10:00:00.050Z","thinkingLevel":null,"configured":null}"#,
        r#"{"type":"message","id":"b0000003","parentId":"b0000002","timestamp":"2026-10-02T10:00:10.000Z","message":{"role":"user","content":[{"type":"text","text":"why is /search slow"}],"attribution":"user","timestamp":1790935210000}}"#,
        r#"{"type":"title_change","id":"b0000004","parentId":"b0000003","timestamp":"2026-10-02T10:00:20.000Z","title":"Find the slow query","source":"auto"}"#,
        r#"{"type":"message","id":"b0000005","parentId":"b0000004","timestamp":"2026-10-02T10:00:30.000Z","message":{"role":"assistant","content":[{"type":"text","text":"The index is missing."}],"api":"openai-completions","provider":"flashnext","model":"flash-next","usage":{"input":7777,"output":54,"cacheRead":0,"cacheWrite":0,"totalTokens":7831},"stopReason":"stop","timestamp":1790935230000}}"#,
        r#"{"type":"custom","id":"b0000006","parentId":"b0000005","timestamp":"2026-10-02T10:00:31.000Z","customType":"todo","data":{"items":[]}}"#,
    ];

    fn write(lines: &[&str], name: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        (d, p)
    }

    fn read(h: Harness, p: &std::path::Path) -> (Session, String) {
        let mut text = String::new();
        let found = scan::Found::new(p.to_path_buf(), h);
        let s = scan::scan_found(&found, None, &mut text).unwrap();
        (s, text)
    }

    #[test]
    fn a_pi_session_is_read_from_its_header_and_entries() {
        let (_d, p) = write(
            PI,
            "2026-10-01T09-00-00-000Z_0f6c1d2e-1111-4222-8333-944455556666.jsonl",
        );
        let (s, text) = read(Harness::Pi, &p);
        assert_eq!(s.harness, Harness::Pi);
        assert_eq!(
            s.id, "0f6c1d2e-1111-4222-8333-944455556666",
            "the id is the header's, not the file name's"
        );
        assert_eq!(s.cwd, "/home/u/parser");
        assert_eq!(s.custom_title, "Lexer rewrite");
        assert_eq!(s.title(), "Lexer rewrite");
        assert_eq!(s.first_prompt, "replace the hand-rolled lexer");
        assert_eq!(s.last_prompt, "now the error spans");
        assert_eq!(s.model, "claude-opus-4-5", "the model of the last reply");
        assert_eq!((s.user_msgs, s.assistant_msgs), (2, 2));
        assert_eq!(s.entries, 9);
        assert_eq!(
            (s.in_tokens, s.out_tokens, s.cache_read, s.cache_write),
            (1500, 100, 400, 10)
        );
        assert_eq!(s.first_ts, 1790845200);
        assert_eq!(s.last_ts, 1790845410);
        for want in [
            "hand-rolled lexer",
            "start with the tokens",
            "grammar first",
            "rg --files src",
            "cargo test lexer",
            "error spans",
        ] {
            assert!(text.contains(want), "{want:?} is not searchable: {text:?}");
        }
        assert!(
            !text.contains("SECRETOUTPUT"),
            "a tool's output was indexed"
        );
    }

    #[test]
    fn an_omp_session_keeps_the_title_omp_gave_it() {
        let (_d, p) = write(
            OMP,
            "2026-10-02T10-00-00-000Z_01a10e6c-aaaa-7000-bbbb-0f1c045e7bd0.jsonl",
        );
        let (s, text) = read(Harness::Omp, &p);
        assert_eq!(s.harness, Harness::Omp);
        assert_eq!(s.id, "01a10e6c-aaaa-7000-bbbb-0f1c045e7bd0");
        assert_eq!(s.cwd, "/home/u/api");
        assert_eq!(s.ai_title, "Find the slow query");
        assert!(
            s.custom_title.is_empty(),
            "omp's own title is not one you gave it"
        );
        assert_eq!(s.first_prompt, "why is /search slow");
        assert_eq!(s.model, "flash-next");
        assert_eq!((s.user_msgs, s.assistant_msgs), (1, 1));
        assert_eq!(s.in_tokens, 7777);
        assert!(text.contains("index is missing"));
    }

    #[test]
    fn a_rename_in_omp_is_your_title() {
        let mut lines = OMP.to_vec();
        lines.push(r#"{"type":"title_change","id":"b0000007","parentId":"b0000006","timestamp":"2026-10-02T10:05:00.000Z","title":"Search latency","source":"user"}"#);
        let (_d, p) = write(&lines, "x.jsonl");
        let (s, _) = read(Harness::Omp, &p);
        assert_eq!(s.custom_title, "Search latency");
        assert_eq!(s.title(), "Search latency");
    }

    #[test]
    fn a_session_that_grew_is_read_on_from_where_it_stopped() {
        let (_d, p) = write(&PI[..4], "g.jsonl");
        let mut t1 = String::new();
        let found = scan::Found::new(p.clone(), Harness::Pi);
        let s1 = scan::scan_found(&found, None, &mut t1).unwrap();
        assert_eq!(s1.assistant_msgs, 1);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        for l in &PI[4..] {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        let mut t2 = String::new();
        let s2 = scan::scan_found(&found, Some(&s1), &mut t2).unwrap();
        assert_eq!(s2.resumed_from, Some(s1.scanned_len));
        assert_eq!(s2.id, "0f6c1d2e-1111-4222-8333-944455556666");
        assert_eq!(s2.cwd, "/home/u/parser");
        assert_eq!(s2.custom_title, "Lexer rewrite");
        assert_eq!((s2.user_msgs, s2.assistant_msgs), (2, 2));
        assert_eq!(s2.in_tokens, 1500);
        assert!(
            t2.contains("error spans") && !t2.contains("hand-rolled"),
            "only the new part: {t2:?}"
        );
    }

    #[test]
    fn a_role_inside_a_tools_arguments_is_not_the_messages() {
        // An assistant writing a chat request for you: its arguments hold
        // `"role":"user"`, unescaped, being an object and not a string.
        let lines = [
            PI[0],
            PI[2],
            r#"{"type":"message","id":"x1","parentId":"a2","timestamp":"2026-10-01T09:00:07.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c9","name":"write","arguments":{"messages":[{"role":"user","content":"NOT A PROMPT"},{"role":"toolResult"}]}}],"model":"m","usage":{"input":5,"output":1,"cacheRead":0,"cacheWrite":0}}}"#,
        ];
        let (_d, p) = write(&lines, "r.jsonl");
        let (s, _) = read(Harness::Pi, &p);
        assert_eq!((s.user_msgs, s.assistant_msgs), (1, 1));
        assert_eq!(s.last_prompt, "replace the hand-rolled lexer");
        assert_eq!(s.in_tokens, 5, "the reply was taken for a tool's result");
    }

    #[test]
    fn a_name_taken_away_leaves_the_session_unnamed() {
        let mut lines = PI.to_vec();
        lines.push(r#"{"type":"session_info","id":"a0000009","parentId":"a0000008","timestamp":"2026-10-01T09:04:00.000Z"}"#);
        let (_d, p) = write(&lines, "n.jsonl");
        let (s, _) = read(Harness::Pi, &p);
        assert!(s.custom_title.is_empty());
        assert_eq!(s.title(), "replace the hand-rolled lexer");
    }

    #[test]
    fn sessions_are_found_in_every_folder_but_not_among_omps_tool_logs() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("sessions");
        std::fs::create_dir_all(root.join("-/2026_x")).unwrap();
        std::fs::create_dir_all(root.join("--srv-app--")).unwrap();
        std::fs::write(root.join("-/2026_x.jsonl"), "{}\n").unwrap();
        std::fs::write(root.join("-/2026_x/3.bash.log"), "log").unwrap();
        std::fs::write(root.join("--srv-app--/2026_y.jsonl"), "{}\n").unwrap();
        let mut got: Vec<String> = discover_in(&root)
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        got.sort();
        assert_eq!(got, ["--srv-app--/2026_y.jsonl", "-/2026_x.jsonl"]);
    }
}

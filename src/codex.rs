//! Codex sessions ("rollouts").
//!
//! One JSONL file per thread under `$CODEX_HOME/sessions/YYYY/MM/DD/`, named
//! `rollout-<time>-<uuid>.jsonl`. Every line is `{"timestamp", "type",
//! "payload"}`:
//!
//! * `session_meta` opens the file: the thread's id, folder, git branch and
//!   the CLI's version.
//! * `turn_context` starts each turn, naming the model.
//! * `response_item` is what went to and from the model: the prompts, the
//!   replies, reasoning summaries and the commands run. Its `user` messages
//!   also carry the AGENTS.md and environment Codex sends along, as blocks of
//!   their own, which are left out.
//! * `event_msg` is what the interface showed. It repeats the prompts and
//!   replies, but `codex exec` writes none of it, so they are read from
//!   `response_item` instead and only `token_count` (the running totals) and
//!   a rename are taken from here.

use crate::model::Session;
use crate::scan;
use memchr::memmem;
use std::path::{Path, PathBuf};

/// Where Codex keeps its rollouts.
pub fn sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join(".codex"));
    Some(home.join("sessions"))
}

/// Every rollout Codex has.
pub fn discover() -> Vec<PathBuf> {
    sessions_dir().map(|r| discover_in(&r)).unwrap_or_default()
}

/// Every rollout under `root`, three folders down: year, month, day.
fn discover_in(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if depth == 0 {
                let rollout = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                if rollout && p.is_file() {
                    out.push(p);
                }
            } else if p.is_dir() {
                walk(&p, depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 3, &mut out);
    out
}

/// What a tool handed back, or a record of a command's output: the big
/// lines, and not conversation. Counted, never parsed.
const OUTPUTS: &[&[u8]] = &[
    br#""payload":{"type":"function_call_output""#,
    br#""payload":{"type":"custom_tool_call_output""#,
    br#""payload":{"type":"exec_command_end""#,
    br#""payload":{"type":"mcp_tool_call_end""#,
    br#""payload":{"type":"patch_apply_end""#,
];

/// Context Codex sends with a prompt rather than anything you typed: the
/// project's AGENTS.md and `<environment_context>` and the like.
pub fn sent_along(t: &str) -> bool {
    let t = t.trim_start();
    !scan::is_real_user_text(t) || t.starts_with("# AGENTS.md instructions")
}

/// Take in one line of a rollout.
pub fn process_line(s: &mut Session, line: &[u8], text: &mut Option<&mut String>) {
    if line.is_empty() {
        return;
    }
    s.entries += 1;
    let f = scan::front(line);
    // The line's own time comes first; a payload's are later, and numbers.
    if let Some(ts) = scan::raw_str(&f[..f.len().min(64)], "timestamp") {
        scan::note_time(s, &ts);
    }
    if OUTPUTS.iter().any(|o| memmem::find(f, o).is_some()) || line.len() > scan::BIG_LINE {
        return;
    }
    let Some(v) = scan::parse_line(line) else {
        return;
    };
    let Some(p) = v.get("payload") else {
        return;
    };
    let str_of =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let keep = |s: &Session, said: &str, text: &mut Option<&mut String>| {
        if let Some(sink) = text.as_deref_mut() {
            if !said.trim().is_empty() && sink.len() < scan::HARVEST_CAP {
                sink.push_str(said);
                sink.push(' ');
                if sink.len() >= scan::HARVEST_CAP {
                    scan::say_over_cap(s);
                }
            }
        }
    };
    match (
        v.get("type").and_then(|t| t.as_str()),
        str_of(p, "type").as_deref(),
    ) {
        (Some("session_meta"), _) => {
            if let Some(id) = str_of(p, "id").filter(|x| !x.is_empty()) {
                s.id = id;
            }
            if let Some(cwd) = str_of(p, "cwd") {
                s.cwd = cwd;
            }
            if let Some(b) = p.get("git").and_then(|g| str_of(g, "branch")) {
                s.git_branch = b;
            }
            if let Some(ver) = str_of(p, "cli_version") {
                s.version = ver;
            }
            // When the thread began, which its first line is written after.
            let began = str_of(p, "timestamp")
                .map(|t| scan::iso_to_epoch(&t))
                .unwrap_or(0);
            if began > 0 && (s.first_ts == 0 || began < s.first_ts) {
                s.first_ts = began;
            }
        }
        (Some("turn_context"), _) => {
            if let Some(m) = str_of(p, "model").filter(|m| !m.is_empty()) {
                s.model = m;
            }
        }
        // What you asked, among what Codex sent along with it: AGENTS.md
        // and the environment arrive as user messages too, in blocks of
        // their own.
        (Some("response_item"), Some("message")) => {
            let blocks = |kind: &str| -> Vec<String> {
                p.get("content")
                    .and_then(|c| c.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(kind))
                    .filter_map(|b| str_of(b, "text"))
                    .collect()
            };
            match str_of(p, "role").as_deref() {
                Some("user") => {
                    let said: Vec<String> = blocks("input_text")
                        .into_iter()
                        .filter(|t| !sent_along(t))
                        .collect();
                    if said.is_empty() {
                        return;
                    }
                    let said = said.join(" ");
                    s.user_msgs += 1;
                    if s.first_prompt.is_empty() {
                        s.first_prompt = scan::squash(&said, 200);
                    }
                    s.last_prompt = scan::squash(&said, 200);
                    keep(s, &said, text);
                }
                Some("assistant") => {
                    s.assistant_msgs += 1;
                    keep(s, &blocks("output_text").join(" "), text);
                }
                _ => {}
            }
        }
        (Some("response_item"), Some("reasoning")) => {
            for t in p
                .get("summary")
                .and_then(|x| x.as_array())
                .into_iter()
                .flatten()
                .filter_map(|b| str_of(b, "text"))
            {
                keep(s, &t, text);
            }
        }
        // Running totals for the whole thread, so the last one is the sum.
        // Its input counts the cached part again.
        (Some("event_msg"), Some("token_count")) => {
            if let Some(t) = p.get("info").and_then(|i| i.get("total_token_usage")) {
                let n = |k: &str| t.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let cached = n("cached_input_tokens");
                s.in_tokens = n("input_tokens").saturating_sub(cached);
                s.cache_read = cached;
                s.cache_write = n("cache_write_input_tokens");
                s.out_tokens = n("output_tokens");
            }
        }
        (Some("event_msg"), Some("thread_name_updated")) => {
            s.custom_title = str_of(p, "thread_name")
                .map(|n| scan::squash(&n, 160))
                .unwrap_or_default();
        }
        // The commands it ran, so a search for one finds the thread.
        (Some("response_item"), Some("function_call")) => {
            let args = str_of(p, "arguments").unwrap_or_default();
            if let Ok(a) = serde_json::from_str::<serde_json::Value>(&args) {
                if let Some(cmd) = str_of(&a, "cmd") {
                    keep(s, &cmd, text);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Harness;
    use std::io::Write;

    /// A rollout shaped like codex-cli 0.149's: the meta line (with its
    /// instructions), a turn, a command and its output, the running totals,
    /// and the injected context on a `response_item` user message.
    const ROLLOUT: &[&str] = &[
        r#"{"timestamp":"2026-10-06T18:18:40.100Z","type":"session_meta","payload":{"id":"01a113f0-ee19-7b12-b5a8-d3c549683529","timestamp":"2026-10-06T18:18:39.000Z","cwd":"/home/u/site","originator":"codex-tui","cli_version":"0.149.1","source":"cli","model_provider":"openai","base_instructions":{"text":"You are Codex, a coding agent."},"git":{"commit_hash":"abc","branch":"redesign","repository_url":"git@example.com:u/site.git"}}}"#,
        r#"{"timestamp":"2026-10-06T18:18:41.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>INJECTEDCONTEXT</environment_context>"}]}}"#,
        r#"{"timestamp":"2026-10-06T18:18:41.500Z","type":"turn_context","payload":{"cwd":"/home/u/site","approval_policy":"on-request","sandbox_policy":{"type":"workspace-write"},"model":"gpt-5.5","effort":"medium"}}"#,
        r#"{"timestamp":"2026-10-06T18:18:42.000Z","type":"event_msg","payload":{"type":"user_message","message":"why does the footer overlap on mobile","images":[],"local_images":[],"text_elements":[]}}"#,
        r#"{"timestamp":"2026-10-06T18:18:42.001Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"why does the footer overlap on mobile"}]}}"#,
        r#"{"timestamp":"2026-10-06T18:18:50.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\": \"rg -n footer src/styles\", \"workdir\": \"/home/u/site\"}","call_id":"call_1"}}"#,
        r#"{"timestamp":"2026-10-06T18:18:51.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"src/styles/footer.css:3 TOOLOUTPUT"}}"#,
        r#"{"timestamp":"2026-10-06T18:18:51.100Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_1","aggregated_output":"TOOLOUTPUT","exit_code":0}}"#,
        r#"{"timestamp":"2026-10-06T18:18:55.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"position fixed is the culprit"}}"#,
        r#"{"timestamp":"2026-10-06T18:18:55.001Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"position fixed is the culprit"}],"content":null,"encrypted_content":"gAAAA"}}"#,
        r#"{"timestamp":"2026-10-06T18:19:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"The footer is fixed; make it static below 600px.","phase":"final"}}"#,
        r#"{"timestamp":"2026-10-06T18:19:00.001Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The footer is fixed; make it static below 600px."}],"phase":"final"}}"#,
        r#"{"timestamp":"2026-10-06T18:19:00.100Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9000,"cached_input_tokens":4000,"output_tokens":300,"reasoning_output_tokens":120,"total_tokens":9300},"last_token_usage":{"input_tokens":9000}},"rate_limits":null}}"#,
        r#"{"timestamp":"2026-10-06T18:20:00.000Z","type":"event_msg","payload":{"type":"user_message","message":"ship it","images":[]}}"#,
        r#"{"timestamp":"2026-10-06T18:20:00.001Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"ship it"}]}}"#,
        r#"{"timestamp":"2026-10-06T18:20:05.000Z","type":"turn_context","payload":{"cwd":"/home/u/site","model":"gpt-5.5-mini"}}"#,
        r#"{"timestamp":"2026-10-06T18:20:30.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Done."}}"#,
        r#"{"timestamp":"2026-10-06T18:20:30.001Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}"#,
        r#"{"timestamp":"2026-10-06T18:20:30.100Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12000,"cached_input_tokens":8000,"output_tokens":350,"total_tokens":12350}}}}"#,
    ];

    fn write(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d
            .path()
            .join("rollout-2026-10-06T18-18-39-01a113f0-ee19-7b12-b5a8-d3c549683529.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        (d, p)
    }

    fn read(p: &Path, prev: Option<&Session>) -> (Session, String) {
        let mut text = String::new();
        let found = scan::Found::new(p.to_path_buf(), Harness::Codex);
        (scan::scan_found(&found, prev, &mut text).unwrap(), text)
    }

    #[test]
    fn a_rollout_is_read_as_the_thread_it_records() {
        let (_d, p) = write(ROLLOUT);
        let (s, text) = read(&p, None);
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.id, "01a113f0-ee19-7b12-b5a8-d3c549683529");
        assert_eq!(s.cwd, "/home/u/site");
        assert_eq!(s.git_branch, "redesign");
        assert_eq!(s.version, "0.149.1");
        assert_eq!(s.model, "gpt-5.5-mini", "the model of the last turn");
        assert_eq!(s.first_prompt, "why does the footer overlap on mobile");
        assert_eq!(s.last_prompt, "ship it");
        assert_eq!(s.title(), "why does the footer overlap on mobile");
        assert_eq!((s.user_msgs, s.assistant_msgs), (2, 2));
        assert_eq!(s.entries, ROLLOUT.len() as u32);
        // the last running total, with the cached part apart
        assert_eq!((s.in_tokens, s.cache_read, s.out_tokens), (4000, 8000, 350));
        assert_eq!(s.first_ts, 1791310719, "when the thread began");
        assert_eq!(s.last_ts, 1791310830);
        for want in [
            "footer overlap",
            "rg -n footer src/styles",
            "culprit",
            "static below 600px",
            "ship it",
        ] {
            assert!(text.contains(want), "{want:?} is not searchable: {text:?}");
        }
        assert!(
            !text.contains("TOOLOUTPUT"),
            "a command's output was indexed"
        );
        assert!(
            !text.contains("INJECTEDCONTEXT"),
            "what Codex sent along was indexed"
        );
        assert!(
            !text.contains("You are Codex"),
            "its instructions were indexed"
        );
    }

    #[test]
    fn a_thread_run_with_codex_exec_has_its_prompt_too() {
        // `codex exec` writes no interface events: what was asked and
        // answered is only in what went to and from the model.
        let lines = [
            r#"{"timestamp":"2026-10-06T18:18:40.100Z","type":"session_meta","payload":{"id":"01a113f0-0000-7b12-b5a8-d3c549683529","timestamp":"2026-10-06T18:18:39.000Z","cwd":"/tmp/ws","originator":"codex_exec","cli_version":"0.149.1","source":"exec"}}"#,
            r#"{"timestamp":"2026-10-06T18:18:40.200Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>sandbox</permissions instructions>"}]}}"#,
            r##"{"timestamp":"2026-10-06T18:18:40.300Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/ws\n\n<INSTRUCTIONS>be brief</INSTRUCTIONS>"},{"type":"input_text","text":"<environment_context><cwd>/tmp/ws</cwd></environment_context>"}]}}"##,
            r#"{"timestamp":"2026-10-06T18:18:41.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"use the desktops in AGENTS.md to test the installer"}]}}"#,
            r#"{"timestamp":"2026-10-06T18:19:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"All three desktops pass."}]}}"#,
        ];
        let (_d, p) = write(&lines);
        let (s, text) = read(&p, None);
        assert_eq!(
            s.first_prompt,
            "use the desktops in AGENTS.md to test the installer"
        );
        assert_eq!((s.user_msgs, s.assistant_msgs), (1, 1));
        assert!(text.contains("desktops pass"));
        assert!(
            !text.contains("be brief") && !text.contains("sandbox"),
            "{text:?}"
        );
    }

    #[test]
    fn a_renamed_thread_keeps_its_name() {
        let mut lines = ROLLOUT.to_vec();
        lines.push(r#"{"timestamp":"2026-10-06T18:21:00.000Z","type":"event_msg","payload":{"type":"thread_name_updated","thread_id":"01a113f0-ee19-7b12-b5a8-d3c549683529","thread_name":"Footer on mobile"}}"#);
        let (_d, p) = write(&lines);
        let (s, _) = read(&p, None);
        assert_eq!(s.custom_title, "Footer on mobile");
        assert_eq!(s.title(), "Footer on mobile");
    }

    #[test]
    fn a_rollout_that_grew_is_read_on_from_where_it_stopped() {
        let (_d, p) = write(&ROLLOUT[..13]);
        let (s1, t1) = read(&p, None);
        assert!(t1.contains("footer overlap"));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        for l in &ROLLOUT[13..] {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        let (s2, t2) = read(&p, Some(&s1));
        assert_eq!(s2.resumed_from, Some(s1.scanned_len));
        assert_eq!(s2.id, s1.id);
        assert_eq!(s2.cwd, "/home/u/site");
        assert_eq!(s2.first_prompt, "why does the footer overlap on mobile");
        assert_eq!(s2.last_prompt, "ship it");
        assert_eq!(
            (s2.in_tokens, s2.cache_read),
            (4000, 8000),
            "the totals were added up"
        );
        assert!(
            t2.contains("ship it") && !t2.contains("footer overlap"),
            "{t2:?}"
        );
    }

    #[test]
    fn rollouts_are_found_by_day_and_nothing_else_is() {
        let d = tempfile::tempdir().unwrap();
        let day = d.path().join("2026/10/06");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-06T18-18-39-x.jsonl"), "{}\n").unwrap();
        std::fs::write(day.join("notes.jsonl"), "{}\n").unwrap();
        std::fs::write(d.path().join("rollout-at-the-top.jsonl"), "{}\n").unwrap();
        let got = discover_in(d.path());
        assert_eq!(got, [day.join("rollout-2026-10-06T18-18-39-x.jsonl")]);
    }
}

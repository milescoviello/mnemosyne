//! Which sessions are running right now.
//!
//! There is no session id in a Claude process's environment, so there is no
//! general pid -> session map. Two signals, in descending order of trust:
//!
//!   exact  -- the process has `--resume <uuid>` / `-r <uuid>` on its command
//!             line, which names the session outright.
//!   guess  -- the process's cwd matches a session's cwd and that session is
//!             the most recently touched one there. Plain `claude` with no
//!             `--resume` can't do better than this.
//!
//! Also recovers the `--model` a live process was started with, so resuming a
//! session doesn't silently drop it back to the default model, and notes which
//! processes wsx started, since those are wsx's to bring back rather than ours.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub cwd: String,
    pub resume_id: Option<String>,
    pub model: Option<String>,
    /// wsx started it: an agent in one of its workspaces.
    pub under_wsx: bool,
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Can this platform tell us which sessions are running?
///
/// Detection walks `/proc`, so it is Linux-only. Everywhere else we say so
/// rather than quietly reporting that nothing is running, which looks
/// identical to a broken feature.
pub const fn detection_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Pull `--resume` and `--model` out of a claude command line.
///
/// `-r` with no argument is normal (it opens Claude's own picker), so a value
/// is only taken when it actually looks like a session id — otherwise the
/// next flag would be swallowed as one.
pub fn parse_claude_args(args: &[String]) -> (Option<String>, Option<String>) {
    let mut resume_id = None;
    let mut model = None;
    for (i, a) in args.iter().enumerate() {
        let a = a.as_str();
        let next = args.get(i + 1).map(|s| s.as_str());
        match a {
            "--resume" | "-r" => {
                if let Some(v) = next {
                    if looks_like_uuid(v) {
                        resume_id = Some(v.to_string());
                    }
                }
            }
            "--model" => {
                if let Some(v) = next {
                    model = Some(v.to_string());
                }
            }
            _ => {
                if let Some(v) = a.strip_prefix("--resume=") {
                    if looks_like_uuid(v) {
                        resume_id = Some(v.to_string());
                    }
                } else if let Some(v) = a.strip_prefix("--model=") {
                    model = Some(v.to_string());
                }
            }
        }
    }
    (resume_id, model)
}

/// The parent pid out of `/proc/<pid>/stat`.
///
/// The command name comes second, in parentheses, and may itself contain
/// spaces and parentheses; only the last `)` reliably ends it.
fn ppid_from_stat(stat: &str) -> Option<i32> {
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Was this process started by wsx itself?
///
/// Its parent, not an ancestor, and not the `WSX_*` variables wsx sets:
/// both of those reach anything started from inside an agent's shell as
/// well, and a `claude` run by hand from there is not an agent wsx will put
/// back. wsx runs each agent directly under its own process.
fn started_by_wsx(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| ppid_from_stat(&s))
        .and_then(|ppid| std::fs::read_to_string(format!("/proc/{ppid}/comm")).ok())
        .is_some_and(|comm| comm.trim() == "wsx")
}

pub fn scan_procs() -> Vec<Proc> {
    let mut out = Vec::new();
    if !detection_supported() {
        return out;
    }
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    let me = std::process::id() as i32;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let base = format!("/proc/{pid}");
        let Ok(raw) = std::fs::read(format!("{base}/cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if args.is_empty() {
            continue;
        }
        let exe = args[0].rsplit('/').next().unwrap_or("");
        if exe != "claude" {
            continue;
        }

        let (resume_id, model) = parse_claude_args(&args);

        let cwd = std::fs::read_link(format!("{base}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        out.push(Proc {
            pid,
            cwd,
            resume_id,
            model,
            under_wsx: started_by_wsx(pid),
        });
    }
    out
}

pub struct LiveMap {
    pub by_id: HashMap<String, Proc>,
    pub by_cwd: HashMap<String, Proc>,
    pub count: usize,
    /// False when the platform cannot tell us, as opposed to there being
    /// nothing to tell.
    pub supported: bool,
}

impl LiveMap {
    /// A value that changes whenever the set of running sessions does.
    ///
    /// The refresh used to compare counts, which misses the case that
    /// matters most: one session ends and another starts between two polls,
    /// the count is unchanged, and the markers quietly describe a state that
    /// no longer exists. Pids are enough to tell those apart.
    pub fn fingerprint(&self) -> u64 {
        let mut pids: Vec<i32> = self
            .by_id
            .values()
            .chain(self.by_cwd.values())
            .map(|p| p.pid)
            .collect();
        pids.sort_unstable();
        let mut h: u64 = 1469598103934665603; // FNV-1a
        for pid in pids {
            for b in pid.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(1099511628211);
            }
        }
        h
    }
}

pub fn live_map() -> LiveMap {
    let procs = scan_procs();
    let count = procs.len();
    let mut by_id = HashMap::new();
    let mut by_cwd = HashMap::new();
    for p in procs {
        match &p.resume_id {
            Some(id) => {
                by_id.insert(id.clone(), p);
            }
            None => {
                if !p.cwd.is_empty() {
                    by_cwd.insert(p.cwd.clone(), p);
                }
            }
        }
    }
    LiveMap {
        by_id,
        by_cwd,
        count,
        supported: detection_supported(),
    }
}

/// Prefix for the tmux sessions this tool creates. Short, and namespaced so we
/// never touch a session the user made themselves.
pub const TMUX_PREFIX: &str = "mn-";

/// A stable, short tmux session name for a Claude session id.
pub fn tmux_name(session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    format!("{TMUX_PREFIX}{short}")
}

/// Which Claude session each tmux session is running, by asking tmux what
/// command it was started with.
///
/// Matching on the session *name* only worked while every name was ours to
/// choose. Once a session can be named whatever you like, the name says
/// nothing — but `pane_start_command` still carries the `--resume <uuid>`
/// the pane was launched with, whatever the session ended up called.
pub fn tmux_by_session_id() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let res = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_start_command}",
        ])
        .output();
    let Ok(o) = res else { return out };
    if !o.status.success() {
        return out;
    }
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let Some((name, cmd)) = line.split_once('\t') else {
            continue;
        };
        if let Some(id) = resume_id_in(cmd) {
            // First pane wins: a second window on the same chat is still
            // that chat, and the session it lives in is the one to go to.
            out.entry(id).or_insert_with(|| name.to_string());
        }
    }
    out
}

/// Pull the session id out of a command line that mentions `--resume`.
pub fn resume_id_in(cmd: &str) -> Option<String> {
    let words: Vec<String> = cmd
        .split([' ', '"', '\''])
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    parse_claude_args(&words).0
}

/// tmux treats `:` and `.` as target syntax, so a name carrying either
/// cannot be addressed afterwards. Anything else unusual is flattened for
/// the same reason: a name you cannot type at is not a name.
pub fn clean_tmux_name(raw: &str) -> String {
    let mut out: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(procs: Vec<Proc>) -> LiveMap {
        let count = procs.len();
        let mut by_id = HashMap::new();
        let mut by_cwd = HashMap::new();
        for p in procs {
            match &p.resume_id {
                Some(id) => {
                    by_id.insert(id.clone(), p);
                }
                None => {
                    by_cwd.insert(p.cwd.clone(), p);
                }
            }
        }
        LiveMap {
            by_id,
            by_cwd,
            count,
            supported: true,
        }
    }

    fn proc(pid: i32, id: &str) -> Proc {
        Proc {
            pid,
            cwd: format!("/home/u/{id}"),
            resume_id: Some(id.to_string()),
            model: None,
            under_wsx: false,
        }
    }

    #[test]
    fn the_parent_is_read_past_a_command_name_with_parentheses_in_it() {
        assert_eq!(
            ppid_from_stat("123 (claude) S 11830 123 123 0"),
            Some(11830)
        );
        assert_eq!(
            ppid_from_stat("123 (odd) name (x)) S 77 123 123 0"),
            Some(77),
            "only the last parenthesis closes the name"
        );
        assert_eq!(ppid_from_stat("garbage"), None);
        assert_eq!(ppid_from_stat(""), None);
    }

    #[test]
    fn this_test_was_not_started_by_wsx() {
        // Its parent is cargo, whatever terminal it runs in -- including a
        // wsx agent's, which is exactly the case the parent check is for.
        assert!(!started_by_wsx(std::process::id() as i32));
    }

    #[test]
    fn swapping_one_session_for_another_is_a_change() {
        // The refresh used to compare counts. One session ending as another
        // starts keeps the count identical, and the markers went stale.
        let a = map_of(vec![proc(1, "one"), proc(2, "two")]);
        let b = map_of(vec![proc(1, "one"), proc(3, "three")]);
        assert_eq!(
            a.count, b.count,
            "the count is exactly what does not change"
        );
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "the change went unnoticed"
        );
    }

    #[test]
    fn the_same_set_in_a_different_order_is_not_a_change() {
        let a = map_of(vec![proc(7, "one"), proc(9, "two")]);
        let b = map_of(vec![proc(9, "two"), proc(7, "one")]);
        assert_eq!(a.fingerprint(), b.fingerprint(), "a redraw for nothing");
    }

    #[test]
    fn a_session_is_recognised_whatever_its_tmux_session_is_called() {
        // The name used to be the only signal, which stopped working the
        // moment you could choose it yourself.
        let cmd =
            "\"exec claude --resume 026bcdb5-8d88-4ad7-9f23-58649bf4f353 --model claude-opus-5\"";
        assert_eq!(
            resume_id_in(cmd).as_deref(),
            Some("026bcdb5-8d88-4ad7-9f23-58649bf4f353")
        );
        assert_eq!(resume_id_in("\"exec claude\""), None);
        assert_eq!(resume_id_in(""), None);
    }

    #[test]
    fn a_chosen_tmux_name_is_one_tmux_can_address() {
        // ':' and '.' are target syntax: a session carrying either cannot be
        // attached to afterwards.
        assert_eq!(clean_tmux_name("eft work"), "eft-work");
        assert_eq!(clean_tmux_name("a:b.c"), "a-b-c");
        assert_eq!(clean_tmux_name("  --trimmed--  "), "trimmed");
        assert_eq!(clean_tmux_name(""), "");
        assert_eq!(clean_tmux_name("!!!"), "");
        assert!(clean_tmux_name(&"x".repeat(80)).chars().count() <= 32);
    }

    #[test]
    fn tmux_names_are_namespaced_and_short() {
        let n = tmux_name("026bcdb5-8d88-4ad7-9f23-58649bf4f353");
        assert_eq!(n, "mn-026bcdb5");
        // the prefix is what keeps us away from sessions the user made
        assert!(n.starts_with(TMUX_PREFIX));
    }

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_resumed_session_is_identified_exactly() {
        let (id, model) = parse_claude_args(&argv(
            "claude --resume 026bcdb5-8d88-4ad7-9f23-58649bf4f353 --model claude-opus-5",
        ));
        assert_eq!(id.as_deref(), Some("026bcdb5-8d88-4ad7-9f23-58649bf4f353"));
        assert_eq!(model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn the_equals_form_works_too() {
        let (id, model) = parse_claude_args(&argv(
            "claude --resume=026bcdb5-8d88-4ad7-9f23-58649bf4f353 --model=qwen3.8-27b",
        ));
        assert!(id.is_some());
        assert_eq!(model.as_deref(), Some("qwen3.8-27b"));
    }

    #[test]
    fn a_bare_dash_r_does_not_swallow_the_next_flag() {
        // `claude -r` on its own opens Claude's own picker; the flag after it
        // is a flag, not a session.
        let (id, _) = parse_claude_args(&argv("claude --dangerously-skip-permissions -r"));
        assert_eq!(id, None);
        let (id, _) = parse_claude_args(&argv("claude -r --dangerously-skip-permissions"));
        assert_eq!(id, None, "swallowed a flag as a session id");
    }

    #[test]
    fn a_plain_claude_has_nothing_to_identify_it() {
        let (id, model) = parse_claude_args(&argv("claude"));
        assert!(id.is_none() && model.is_none());
    }

    #[test]
    fn only_real_uuids_are_accepted_as_resume_targets() {
        assert!(looks_like_uuid("026bcdb5-8d88-4ad7-9f23-58649bf4f353"));
        assert!(!looks_like_uuid("026bcdb5-8d88-4ad7-9f23-58649bf4f35"));
        assert!(!looks_like_uuid("not-a-uuid-at-all-really-nope-nope-x"));
        assert!(!looks_like_uuid(""));
        // `-r` with no argument must not swallow the next flag
        assert!(!looks_like_uuid("--dangerously-skip-permissions"));
    }
}

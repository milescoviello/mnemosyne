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
//! session doesn't silently drop it back to the default model.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub cwd: String,
    pub resume_id: Option<String>,
    pub model: Option<String>,
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

/// Names of every existing tmux session. Empty when no server is running,
/// which is the normal case rather than an error.
pub fn tmux_sessions() -> HashSet<String> {
    let out = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => HashSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

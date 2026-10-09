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

use crate::model::Harness;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    /// Which agent it is, so a guess by folder picks among its own sessions.
    pub harness: Harness,
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
///
/// Which session a process is writing to is what matters. `--session-id`
/// names it outright. `--fork-session` means it is not the one resumed --
/// the fork gets an id of its own -- so the one resumed is not running.
pub fn parse_claude_args(args: &[String]) -> (Option<String>, Option<String>) {
    let mut resume_id = None;
    let mut session_id = None;
    let mut fork = false;
    let mut model = None;
    let uuid = |v: Option<&str>| v.filter(|v| looks_like_uuid(v)).map(str::to_string);
    for (i, a) in args.iter().enumerate() {
        let a = a.as_str();
        let next = args.get(i + 1).map(|s| s.as_str());
        match a {
            "--resume" | "-r" => resume_id = uuid(next).or(resume_id),
            "--session-id" => session_id = uuid(next).or(session_id),
            "--fork-session" => fork = true,
            "--model" => {
                if let Some(v) = next {
                    model = Some(v.to_string());
                }
            }
            _ => {
                if let Some(v) = a.strip_prefix("--resume=") {
                    resume_id = uuid(Some(v)).or(resume_id);
                } else if let Some(v) = a.strip_prefix("--session-id=") {
                    session_id = uuid(Some(v)).or(session_id);
                } else if let Some(v) = a.strip_prefix("--model=") {
                    model = Some(v.to_string());
                }
            }
        }
    }
    let id = session_id.or(if fork { None } else { resume_id });
    (id, model)
}

/// Which agent a command line runs, and where its own words start.
///
/// Most are their own program. omp is a script bun runs, hermes one python
/// runs, and pi one node runs -- though pi renames itself `pi` and leaves no
/// arguments behind. Codex's `codex` is a node script that starts the real
/// one, which is the one counted.
pub fn agent_of(args: &[String]) -> Option<(Harness, usize)> {
    let base = |i: usize| args.get(i).map(|a| a.rsplit('/').next().unwrap_or(a));
    let first = base(0)?;
    if let Some(h) = Harness::from_name(first) {
        return Some((h, 0));
    }
    if !(first == "bun" || first == "node" || first.starts_with("python")) {
        return None;
    }
    match base(1).and_then(Harness::from_name) {
        Some(Harness::Codex) | None => None,
        Some(h) => Some((h, 1)),
    }
}

/// Whether an agent's command line is a conversation, rather than one of
/// its other jobs: an MCP server, an app server, a gateway, a login, a
/// one-off run for a script. `args` begins with the program.
///
/// Claude's are its own to say. The others' subcommands are the ones each
/// lists in its `--help`; a word that is none of them is a prompt.
pub fn is_agent_chat(h: Harness, args: &[String]) -> bool {
    const CODEX_JOBS: &[&str] = &[
        "agents",
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "mcp-server",
        "app-server",
        "remote-control",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "queue",
        "archive",
        "delete",
        "migrate-rollouts",
        "unarchive",
        "cloud",
        "exec-server",
        "features",
        "help",
    ];
    const OMP_JOBS: &[&str] = &[
        "acp",
        "agents",
        "auth-broker",
        "auth-gateway",
        "bench",
        "browser-relay",
        "cleanse",
        "clip",
        "collab",
        "commit",
        "completions",
        "compress",
        "config",
        "dry-balance",
        "find",
        "gallery",
        "gc",
        "git",
        "grep",
        "grievances",
        "if-bench",
        "images",
        "install",
        "join",
        "login",
        "models",
        "play",
        "plugin",
        "predict",
        "ps",
        "read",
        "render",
        "say",
        "search",
        "setup",
        "share",
        "shell",
        "skill",
        "ssh",
        "stats",
        "stream",
        "tiny-models",
        "token",
        "toks",
        "ttsr",
        "update",
        "usage",
        "worktree",
    ];
    const PI_JOBS: &[&str] = &[
        "install",
        "remove",
        "uninstall",
        "update",
        "list",
        "config",
        "auth",
    ];
    // The flags whose value is the next word, which is not a subcommand.
    let takes_value: &[&str] = match h {
        Harness::Codex => &[
            "-c",
            "--config",
            "-m",
            "--model",
            "-i",
            "--image",
            "-p",
            "--profile",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-C",
            "--cd",
            "--enable",
            "--disable",
            "--remote",
            "--remote-auth-token-env",
            "--local-provider",
            "--add-dir",
        ],
        Harness::Hermes => &[
            "-z",
            "--oneshot",
            "--usage-file",
            "-m",
            "--model",
            "--provider",
            "-t",
            "--toolsets",
            "-r",
            "--resume",
            "-c",
            "--continue",
            "-s",
            "--skills",
        ],
        Harness::Omp => &[
            "-r",
            "--resume",
            "--model",
            "--plan",
            "--session-dir",
            "--profile",
            "--cwd",
            "--thinking",
            "--models",
            "--approval-mode",
            "--mode",
            "--max-time",
        ],
        Harness::Pi => &[
            "--session",
            "--session-id",
            "--fork",
            "--session-dir",
            "--name",
            "-n",
            "--model",
            "--models",
            "--provider",
        ],
        Harness::Claude => &[],
    };
    let rest = args.get(1..).unwrap_or_default();
    let first = {
        let mut skip = false;
        rest.iter().find(|a| {
            if std::mem::take(&mut skip) {
                return false;
            }
            if a.starts_with('-') {
                skip = takes_value.contains(&a.as_str());
                return false;
            }
            true
        })
    };
    let job = |jobs: &[&str]| first.is_some_and(|w| jobs.contains(&w.as_str()));
    let print = rest.iter().any(|a| a == "-p" || a == "--print");
    match h {
        Harness::Claude => is_claude_session(args),
        Harness::Codex => !job(CODEX_JOBS),
        Harness::Hermes => first.is_none_or(|w| w == "chat"),
        Harness::Omp => !print && !job(OMP_JOBS),
        Harness::Pi => !print && !job(PI_JOBS),
    }
}

/// The session an agent's command line names, and for Claude the model.
pub fn resume_of(h: Harness, args: &[String]) -> (Option<String>, Option<String>) {
    match h {
        Harness::Claude => parse_claude_args(args),
        // A fork is a session of its own, not the one it came from.
        Harness::Pi
            if args
                .iter()
                .any(|a| a == "--fork" || a.starts_with("--fork=")) =>
        {
            (None, None)
        }
        Harness::Pi => (
            flag_value(args, &["--session", "--session-id"]).and_then(|v| session_named(&v)),
            None,
        ),
        Harness::Omp => (
            flag_value(args, &["--resume", "-r", "--session-id"]).and_then(|v| session_named(&v)),
            None,
        ),
        // `codex resume <id>`, with flags anywhere around it.
        Harness::Codex => (
            args.iter()
                .position(|a| a == "resume")
                .and_then(|i| args[i + 1..].iter().find(|a| looks_like_uuid(a)))
                .cloned(),
            None,
        ),
        // Hermes's ids are its own: `20260910_004421_837570`.
        Harness::Hermes => (flag_value(args, &["--resume", "-r"]), None),
    }
}

/// The value of the first of `names` given, as `--flag value` or
/// `--flag=value`. A flag followed by another flag has none.
fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        for n in names {
            if a == n {
                return args.get(i + 1).filter(|v| !v.starts_with('-')).cloned();
            }
            if let Some(v) = a.strip_prefix(n).and_then(|r| r.strip_prefix('=')) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// A session named by its whole id, or by its file -- pi and omp take
/// either, and call the file `<time>_<id>.jsonl`. A prefix is left alone:
/// it is a guess at which session, not a name.
fn session_named(v: &str) -> Option<String> {
    let v = match v.strip_suffix(".jsonl") {
        Some(stem) => stem.rsplit(['/', '_']).next().unwrap_or(stem),
        None => v,
    };
    looks_like_uuid(v).then(|| v.to_string())
}

/// The thread a Codex rollout file is for: `rollout-<time>-<id>.jsonl`.
pub fn rollout_id(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    looks_like_uuid(id).then(|| id.to_string())
}

/// The rollout a running codex has open: which thread it is, when its
/// command line does not say.
fn open_rollout(pid: i32) -> Option<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()?
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .find_map(|p| rollout_id(&p.to_string_lossy()))
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
    parent_of(pid).is_some_and(|ppid| comm_of(ppid) == "wsx")
}

fn parent_of(pid: i32) -> Option<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| ppid_from_stat(&s))
}

fn comm_of(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|c| c.trim().to_string())
        .unwrap_or_default()
}

/// Is this command line a claude you talk to? `claude` itself, with or
/// without a prompt or `--resume` -- not one of its commands (`mcp serve`
/// for an IDE, `doctor`, the background host's `daemon`), and not `-p` or
/// `--print`, which answers once and exits. Those run in some folder too:
/// each was matched to that folder's newest session, marked it running,
/// and had it put back after a reboot.
pub fn is_claude_session(args: &[String]) -> bool {
    const NOT_CHATS: &[&str] = &[
        "agents",
        "auth",
        "auto-mode",
        "config",
        "daemon",
        "doctor",
        "gateway",
        "import",
        "install",
        "kill",
        "logs",
        "mcp",
        "migrate-installer",
        "plugin",
        "plugins",
        "purge",
        "respawn",
        "rm",
        "setup-token",
        "stop",
        "ultrareview",
        "update",
        "upgrade",
    ];
    let Some(exe) = args.first() else {
        return false;
    };
    if exe.rsplit('/').next() != Some("claude") {
        return false;
    }
    if args.get(1).is_some_and(|a| NOT_CHATS.contains(&a.as_str())) {
        return false;
    }
    !args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "-p" || a == "--print" || a.starts_with("--print="))
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
        let Some((harness, at)) = agent_of(&args) else {
            continue;
        };
        // `claude -p`, `codex mcp-server`, `hermes gateway` and the rest
        // are not chats.
        if !is_agent_chat(harness, &args[at..]) {
            continue;
        }

        let (mut resume_id, model) = resume_of(harness, &args[at..]);
        if resume_id.is_none() && harness == Harness::Codex {
            resume_id = open_rollout(pid);
        }

        let cwd = std::fs::read_link(format!("{base}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        // wsx starts Codex's node script, and that starts this.
        let under_wsx = started_by_wsx(pid)
            || (harness == Harness::Codex && parent_of(pid).is_some_and(started_by_wsx));

        out.push(Proc {
            pid,
            harness,
            cwd,
            resume_id,
            model,
            under_wsx,
        });
    }
    out
}

pub struct LiveMap {
    pub by_id: HashMap<String, Proc>,
    /// The ones that do not say which session they are, by agent and folder.
    pub by_cwd: HashMap<(Harness, String), Proc>,
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
                    by_cwd.insert((p.harness, p.cwd.clone()), p);
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

/// One pane of one tmux session, and what it runs.
#[derive(Debug, Clone)]
pub struct Pane {
    pub session: String,
    /// The pane's process: for a chat started in it, the claude itself.
    pub pid: i32,
    pub start_command: String,
}

/// Every pane tmux has, or none if it will not say in time.
///
/// Bounded like every other call out. A tmux server that is stopped or
/// wedged never answers, and this is asked at startup and whenever what is
/// running changes: the browser, `--list` and `--json` all froze on it.
pub fn tmux_panes() -> Vec<Pane> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let Ok(out) = crate::wsx::run(
        &["tmux"],
        &[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_pid}\t#{pane_start_command}",
        ],
        deadline,
        crate::wsx::Keep::Stdout,
    ) else {
        return Vec::new();
    };
    parse_panes(&out)
}

fn parse_panes(out: &str) -> Vec<Pane> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (session, pid, cmd) = (f.next()?, f.next()?, f.next().unwrap_or(""));
            Some(Pane {
                session: session.to_string(),
                pid: pid.trim().parse().ok()?,
                start_command: cmd.to_string(),
            })
        })
        .collect()
}

/// Which Claude session each tmux session is running, by what command its
/// panes were started with.
///
/// Matching on the session *name* only worked while every name was ours to
/// choose. Once a session can be named whatever you like, the name says
/// nothing — but `pane_start_command` still carries the `--resume <uuid>`
/// the pane was launched with, whatever the session ended up called.
pub fn by_session_id<'a>(panes: impl IntoIterator<Item = &'a Pane>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for p in panes {
        if let Some(id) = resume_id_in(&p.start_command) {
            // First pane wins: a second window on the same chat is still
            // that chat, and the session it lives in is the one to go to.
            out.entry(id).or_insert_with(|| p.session.clone());
        }
    }
    out
}

/// Pull the session id out of a command line that names one, in whichever
/// agent's way it runs.
pub fn resume_id_in(cmd: &str) -> Option<String> {
    let words: Vec<String> = cmd
        .split([' ', '"', '\''])
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    // The agent that names a session, not the first word that happens to
    // be an agent's name: `cd ~/src/codex && claude --resume ID` is Claude's.
    (0..words.len())
        .filter_map(|i| agent_of(&words[i..]).map(|(h, at)| (h, i + at)))
        .find_map(|(h, at)| resume_of(h, &words[at..]).0)
        .or_else(|| parse_claude_args(&words).0)
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
                    by_cwd.insert((p.harness, p.cwd.clone()), p);
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
            harness: Harness::Claude,
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

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    const PI_ID: &str = "01a10e6c-aaaa-7000-bbbb-0f1c045e7bd0";
    const CODEX_ID: &str = "01a113f0-ee19-7b12-b5a8-d3c549683529";
    const HERMES_ID: &str = "20260910_004421_837570";

    #[test]
    fn each_agent_is_known_by_how_it_runs() {
        // As /proc showed them. pi renames itself and leaves no arguments.
        assert_eq!(agent_of(&words("pi")), Some((Harness::Pi, 0)));
        assert_eq!(
            agent_of(&words("bun /home/u/.bun/bin/omp --resume=/p/x.jsonl")),
            Some((Harness::Omp, 1))
        );
        let native = format!("/n/vendor/x86_64-unknown-linux-musl/bin/codex resume {CODEX_ID}");
        assert_eq!(agent_of(&words(&native)), Some((Harness::Codex, 0)));
        assert_eq!(
            agent_of(&words(&format!(
                "node /home/u/.local/npm-global/bin/codex resume {CODEX_ID}"
            ))),
            None,
            "Codex's wrapper is counted through the codex it starts"
        );
        assert_eq!(
            agent_of(&words(&format!(
                "/h/venv/bin/python3 /h/venv/bin/hermes --resume {HERMES_ID}"
            ))),
            Some((Harness::Hermes, 1))
        );
        assert_eq!(
            agent_of(&words("claude --resume x")),
            Some((Harness::Claude, 0))
        );
        assert_eq!(agent_of(&words("/usr/bin/python3 -m http.server")), None);
        assert_eq!(agent_of(&words("vim pi")), None);
    }

    #[test]
    fn each_agent_says_which_session_its_own_way() {
        let id = |h: Harness, cmd: &str| resume_of(h, &words(cmd)).0;
        assert_eq!(
            id(Harness::Pi, &format!("pi --session {PI_ID}")).as_deref(),
            Some(PI_ID)
        );
        assert_eq!(
            id(
                Harness::Pi,
                &format!("pi --session /h/.pi/agent/sessions/--w--/2026-10-01T09-00-00-000Z_{PI_ID}.jsonl")
            )
            .as_deref(),
            Some(PI_ID),
            "a session file names its session"
        );
        assert_eq!(
            id(Harness::Pi, "pi --session 01a10e6c"),
            None,
            "a prefix is not certain"
        );
        assert_eq!(
            id(Harness::Pi, &format!("pi --fork {PI_ID}")),
            None,
            "a fork is new"
        );
        assert_eq!(
            id(Harness::Omp, &format!("omp --resume={PI_ID}")).as_deref(),
            Some(PI_ID)
        );
        assert_eq!(
            id(Harness::Omp, &format!("omp -r {PI_ID} --allow-home")).as_deref(),
            Some(PI_ID)
        );
        assert_eq!(id(Harness::Omp, "omp --resume"), None, "the picker");
        assert_eq!(
            id(Harness::Codex, &format!("codex resume {CODEX_ID}")).as_deref(),
            Some(CODEX_ID)
        );
        assert_eq!(
            id(
                Harness::Codex,
                &format!("codex -c a=b resume --all {CODEX_ID}")
            )
            .as_deref(),
            Some(CODEX_ID)
        );
        assert_eq!(id(Harness::Codex, "codex resume --last"), None);
        assert_eq!(id(Harness::Codex, &format!("codex fork {CODEX_ID}")), None);
        assert_eq!(
            id(Harness::Hermes, &format!("hermes --resume {HERMES_ID}")).as_deref(),
            Some(HERMES_ID)
        );
        assert_eq!(
            id(Harness::Hermes, &format!("hermes -r {HERMES_ID} --yolo")).as_deref(),
            Some(HERMES_ID)
        );
        assert_eq!(id(Harness::Hermes, "hermes --continue"), None);
    }

    #[test]
    fn an_agents_other_jobs_are_not_chats() {
        // `codex mcp-server` started by Claude Code, the VS Code
        // extension's app-server, Hermes's gateway: running in a folder,
        // they were taken for its newest session.
        let chat = |h: Harness, cmd: &str| is_agent_chat(h, &words(cmd));
        for cmd in [
            "codex mcp-server",
            "codex -c a=b app-server",
            "codex login",
            "codex exec fix-the-build",
        ] {
            assert!(!chat(Harness::Codex, cmd), "{cmd}");
        }
        for cmd in [
            "codex",
            &format!("codex resume {CODEX_ID}"),
            "codex fix-the-build",
        ] {
            assert!(chat(Harness::Codex, cmd), "{cmd}");
        }
        for cmd in [
            "hermes gateway run",
            "hermes cron tick",
            "hermes -m x setup",
        ] {
            assert!(!chat(Harness::Hermes, cmd), "{cmd}");
        }
        for cmd in [
            "hermes".to_string(),
            format!("hermes --resume {HERMES_ID}"),
            "hermes -z find-the-lecture".to_string(),
            "hermes chat".to_string(),
        ] {
            assert!(chat(Harness::Hermes, &cmd), "{cmd}");
        }
        assert!(!chat(Harness::Omp, "omp acp"));
        assert!(!chat(Harness::Omp, "omp -p list-the-files"));
        assert!(chat(Harness::Omp, &format!("omp --resume={PI_ID}")));
        assert!(!chat(Harness::Pi, "pi install npm:x"));
        assert!(!chat(Harness::Pi, "pi --print hi"));
        assert!(chat(Harness::Pi, "pi"));
        assert!(!chat(Harness::Claude, "claude mcp serve"));
        assert!(chat(Harness::Claude, "claude --resume x"));
    }

    #[test]
    fn a_folder_named_for_an_agent_does_not_hide_the_one_running() {
        let cmd = format!(
            "cd /home/u/src/codex && claude --resume {}",
            "026bcdb5-8d88-4ad7-9f23-58649bf4f353"
        );
        assert_eq!(
            resume_id_in(&cmd).as_deref(),
            Some("026bcdb5-8d88-4ad7-9f23-58649bf4f353")
        );
    }

    #[test]
    fn a_rollout_codex_holds_open_names_its_thread() {
        assert_eq!(
            rollout_id(&format!(
                "/h/.codex/sessions/2026/10/06/rollout-2026-10-06T18-18-39-{CODEX_ID}.jsonl"
            ))
            .as_deref(),
            Some(CODEX_ID)
        );
        assert_eq!(rollout_id("/h/.codex/log/codex-tui.log"), None);
        assert_eq!(rollout_id("/h/.codex/state_5.sqlite"), None);
    }

    #[test]
    fn any_agent_is_recognised_in_a_tmux_pane() {
        for (cmd, want) in [
            (format!("/home/u/.local/bin/pi --session {PI_ID}"), PI_ID),
            (
                format!("/home/u/.local/npm-global/bin/codex resume {CODEX_ID}"),
                CODEX_ID,
            ),
            (
                format!("/home/u/.local/bin/hermes --resume {HERMES_ID}"),
                HERMES_ID,
            ),
            (
                format!("/home/u/.bun/bin/omp --resume {PI_ID} --allow-home"),
                PI_ID,
            ),
        ] {
            assert_eq!(resume_id_in(&cmd).as_deref(), Some(want), "{cmd}");
        }
    }

    #[test]
    fn only_a_claude_you_talk_to_is_a_session() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        for chat in [
            "claude",
            "/home/x/.local/bin/claude --resume 026bcdb5-8d88-4ad7-9f23-58649bf4f353",
            "claude --model opus fix the parser",
            "claude attach 3f2a",
        ] {
            assert!(is_claude_session(&a(chat)), "{chat}");
        }
        // An MCP server for an IDE, a script's one-shot answer, the
        // background host: each looked like a chat in its folder, was
        // marked running, and was put back after a reboot.
        for not in [
            "claude mcp serve",
            "claude -p summarise the diff",
            "claude --model haiku --print hello",
            "claude daemon run",
            "claude doctor",
            "claude update",
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
        ] {
            assert!(!is_claude_session(&a(not)), "{not}");
        }
    }

    #[test]
    fn a_fork_is_not_the_session_it_was_forked_from() {
        let id = "026bcdb5-8d88-4ad7-9f23-58649bf4f353";
        let other = "11112222-3333-4444-5555-666677778888";
        let v = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            parse_claude_args(&v(&format!("claude --resume {id} --fork-session"))).0,
            None
        );
        assert_eq!(
            parse_claude_args(&v(&format!("claude --session-id {other}")))
                .0
                .as_deref(),
            Some(other)
        );
        assert_eq!(
            parse_claude_args(&v(&format!(
                "claude --resume {id} --fork-session --session-id={other}"
            )))
            .0
            .as_deref(),
            Some(other)
        );
        assert_eq!(
            parse_claude_args(&v(&format!("claude --resume {id}")))
                .0
                .as_deref(),
            Some(id)
        );
    }

    #[test]
    fn panes_are_read_with_their_pid_and_whole_command() {
        let out = "work\t12825\t/home/u/.local/bin/claude --resume 026bcdb5-8d88-4ad7-9f23-58649bf4f353 --model x\n\
                   wsx-mnemosyne-mn-bug-hunt\t395298\tclaude --continue --settings \"{\\\"a\\\":1}\"\tand a tab\n\
                   garbage line\n";
        let panes = parse_panes(out);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[1].pid, 395298);
        assert!(panes[1].start_command.ends_with("and a tab"));
        let ids = by_session_id(&panes);
        assert_eq!(ids["026bcdb5-8d88-4ad7-9f23-58649bf4f353"], "work");
        assert_eq!(ids.len(), 1);
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

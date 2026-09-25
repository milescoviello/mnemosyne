//! wsx workspaces.
//!
//! [wsx](https://github.com/bakedbean/workspacex) runs each Claude session in
//! a git worktree of its own, at `<state>/wsx/worktrees/<repo>/<slug>`. As a
//! plain folder that is a long path shared by every one of them, with the two
//! parts worth reading -- the repo and the workspace -- at the end, which is
//! where clipping cuts first.
//!
//! Everything here reads; nothing writes. What a path means is worked out
//! from the path alone, because for an archived workspace that is all there
//! is: wsx deletes a workspace's row when it archives it. What is still live,
//! and where each repo's own checkout is, can only come from `wsx` itself, and
//! comes from its command line rather than its database -- the output is its
//! interface, the schema is not.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long wsx gets to answer. It takes about twenty milliseconds; this is
/// for a loaded machine, and a wedged wsx must never be why the list is late.
const DEADLINE: Duration = Duration::from_millis(500);

/// A jump either hands a line to a running wsx or starts a terminal and
/// leaves it; both are quick. The browser waits on it, so it is bounded too.
const JUMP_DEADLINE: Duration = Duration::from_secs(3);

/// Where every machine's wsx keeps worktrees unless told otherwise. Matched
/// anywhere in a path, so a transcript synced from another machine -- another
/// home, another user name -- is still recognised for what it is.
const SEGMENT: &str = "/.local/state/wsx/worktrees/";

/// What every automatic tag starts with. `wsx/os-dev` rather than
/// `wsx:OS-DEV`: it has to survive the same normalising a typed tag goes
/// through, which lowercases it and drops the colon.
pub const TAG_PREFIX: &str = "wsx/";

/// What a wsx session's folder is labelled with, ahead of its name. Without
/// it, `OS-DEV/shy-daffodil` could be any folder of that name.
pub const MARK: &str = "wsx ";

/// A session's folder, read as a place in a wsx workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    /// The repo, by the name wsx gives it.
    pub repo: String,
    /// The worktree's folder. That is the slug the workspace was created
    /// with, and it stays that: `wsx workspace rename` changes the slug and
    /// the branch but never moves the folder.
    pub dir: String,
    /// Where under the worktree the session ran; empty at its top.
    pub rest: String,
    /// Under this machine's own worktree root. Otherwise it was recognised by
    /// its shape alone, and the local wsx knows nothing about it.
    pub local: bool,
}

/// One line of `wsx workspace list`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub repo: String,
    pub slug: String,
    pub branch: String,
    pub path: String,
}

/// Where this machine's wsx keeps its worktrees.
pub fn worktrees_root() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg = std::env::var("XDG_STATE_HOME").ok();
    root_for(&home, xdg.as_deref(), cfg!(target_os = "macos"))
}

/// The rule wsx follows, with everything it reads handed in.
///
/// wsx asks `dirs::state_dir()` and falls back to `~/.local/state`. On Linux
/// that honours `$XDG_STATE_HOME`, but only an absolute one. On macOS there is
/// no state directory at all, so it is always `~/.local/state` whatever the
/// environment says. Both branches are compiled and tested everywhere: a rule
/// only exercised on a platform nobody here runs is a rule nobody finds out
/// is wrong.
fn root_for(home: &str, xdg_state: Option<&str>, macos: bool) -> Option<String> {
    let state = match xdg_state {
        Some(x) if !macos && x.starts_with('/') => x.trim_end_matches('/').to_string(),
        _ if home.is_empty() => return None,
        _ => format!("{}/.local/state", home.trim_end_matches('/')),
    };
    Some(format!("{state}/wsx/worktrees"))
}

/// Read a folder as a wsx workspace, if it is one, given this machine's
/// worktree root. Handed in rather than looked up, so the tests never have to
/// set an environment variable to get a known one.
pub fn parse_path_in(cwd: &str, root: Option<&str>) -> Option<Ref> {
    if let Some(root) = root
        .map(|r| r.trim_end_matches('/'))
        .filter(|r| !r.is_empty())
    {
        // The slash is part of the match: `worktrees-old/` begins with the
        // same letters and is not the same folder.
        if let Some(after) = cwd.strip_prefix(root).and_then(|a| a.strip_prefix('/')) {
            return split(after, true);
        }
    }
    let at = cwd.find(SEGMENT)?;
    split(&cwd[at + SEGMENT.len()..], false)
}

/// `<repo>/<dir>[/<rest>]`, as found under a worktree root.
fn split(after: &str, local: bool) -> Option<Ref> {
    let after = after.trim_end_matches('/');
    let mut parts = after.splitn(3, '/');
    let repo = parts.next().unwrap_or("");
    let dir = parts.next().unwrap_or("");
    // A repo's folder with no workspace under it is not a workspace.
    if repo.is_empty() || dir.is_empty() {
        return None;
    }
    Some(Ref {
        repo: repo.to_string(),
        dir: dir.to_string(),
        rest: parts.next().unwrap_or("").to_string(),
        local,
    })
}

/// `wsx workspace list`: repo, slug, branch and worktree, a tab apart.
///
/// Split on tabs and nothing else. A slug can contain spaces -- wsx takes
/// whatever `--name` it is given -- and so can a repo name.
pub fn parse_workspace_list(out: &str) -> Vec<Workspace> {
    out.lines()
        .filter_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut f = line.splitn(4, '\t');
            let (repo, slug, branch, path) = (f.next()?, f.next()?, f.next()?, f.next()?);
            if repo.is_empty() || slug.is_empty() || !path.starts_with('/') {
                return None;
            }
            Some(Workspace {
                repo: repo.to_string(),
                slug: slug.to_string(),
                branch: branch.to_string(),
                path: path.trim_end_matches('/').to_string(),
            })
        })
        .collect()
}

/// Where `repo`'s own checkout is, out of `wsx repo list`.
///
/// That output is for reading, not parsing: the name padded to twenty
/// columns, a space, then the path. A name can contain spaces, so there is no
/// column to split on. What can be done safely is to look for a name already
/// known -- from a workspace's path -- and accept the line only if what
/// follows it is padding and then an absolute path. `OS` is then not found in
/// the line for `OS-DEV`, and `meals` not in the one for `meals backend`.
pub fn parse_repo_list(out: &str, repo: &str) -> Option<String> {
    if repo.is_empty() {
        return None;
    }
    out.lines().find_map(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let after = line.strip_prefix(repo)?;
        // at least the one space that follows even a name longer than the
        // padding
        if !after.starts_with(' ') {
            return None;
        }
        let path = after.trim_start_matches(' ');
        path.starts_with('/').then(|| path.to_string())
    })
}

/// What the list shows for a session that ran in a wsx workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub repo: String,
    pub slug: String,
    /// Where under the worktree it ran; empty at its top.
    pub rest: String,
    pub status: Status,
}

/// What wsx says about the workspace now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Only the path says it is one: wsx is not here to ask, or the folder
    /// is another machine's.
    Unknown,
    /// Still in `wsx workspace list`, with its worktree there.
    Live { worktree: String },
    /// This machine's, and no longer listed: archived. wsx deletes the
    /// worktree with it unless told to keep it, and forgets the workspace
    /// entirely, but the repo is still there -- `checkout` is where its own
    /// copy lives, if wsx still knows the repo by that name.
    Archived { checkout: Option<String> },
}

impl Place {
    /// `OS-DEV/shy-daffodil`, and the folder under it if it was not the top.
    pub fn label(&self) -> String {
        format!("{}/{}", self.repo, self.tail())
    }

    /// `wsx/os-dev`: the tag every session from this repo carries without
    /// anyone giving it, so `T` can pull one project's sessions out of a list
    /// where each workspace is a folder of its own.
    pub fn tag(&self) -> String {
        format!("{TAG_PREFIX}{}", crate::meta::normalize_tag(&self.repo))
    }

    fn tail(&self) -> String {
        if self.rest.is_empty() {
            self.slug.clone()
        } else {
            format!("{}/{}", self.slug, self.rest)
        }
    }

    /// The label in `w` columns. When it has to give, the repo gives first:
    /// every row from one project shares it, and the workspace is what tells
    /// them apart -- which is exactly the part that plain clipping cut.
    pub fn fit(&self, w: usize) -> String {
        use crate::model::{fit, width};
        let full = self.label();
        if width(&full) <= w {
            return full;
        }
        let tail = self.tail();
        let room = w.saturating_sub(width(&tail) + 1);
        // two columns is the least that still says there was a repo: "O…"
        if room >= 2 {
            format!("{}/{tail}", fit(&self.repo, room))
        } else {
            fit(&tail, w)
        }
    }
}

impl Status {
    /// One word for it, as `--json` puts it.
    pub fn word(&self) -> &'static str {
        match self {
            Status::Unknown => "unknown",
            Status::Live { .. } => "live",
            Status::Archived { .. } => "archived",
        }
    }
}

/// What this machine's wsx has to say about a folder.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct State {
    /// Where its worktrees are, if there is a home to find them under.
    pub root: Option<String>,
    /// wsx answered. When it did not -- not installed, failed, too slow --
    /// everything that would have asked it behaves as though it were not
    /// there, which is how it all behaved before this knew about wsx.
    pub available: bool,
    pub workspaces: Vec<Workspace>,
    /// `wsx repo list` as it was printed. It cannot be split up front; see
    /// `parse_repo_list`.
    pub repos: String,
    /// The tmux sessions shared workspaces run their agents in. A claude in
    /// one is wsx's agent although its parent is the tmux server, not wsx.
    pub shared: Vec<String>,
}

impl State {
    /// Only what the environment says. Nothing is run.
    pub fn here() -> State {
        State {
            root: worktrees_root(),
            ..State::default()
        }
    }

    /// The live workspace a folder is in: the one whose worktree holds it.
    pub fn live(&self, cwd: &str) -> Option<&Workspace> {
        let cwd = cwd.trim_end_matches('/');
        self.workspaces
            .iter()
            .filter(|w| {
                cwd == w.path
                    || cwd
                        .strip_prefix(w.path.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            })
            // a slug with a slash in it puts one worktree inside another's
            // folder, and the deeper one is the one it ran in
            .max_by_key(|w| w.path.len())
    }

    /// The workspace a session's folder belongs to, if it is in one.
    ///
    /// A live one is named by what wsx lists, not by its folder: renaming a
    /// workspace never moves the folder, so after a rename only the list
    /// knows what it is called.
    pub fn place(&self, cwd: &str) -> Option<Place> {
        if self.available {
            if let Some(w) = self.live(cwd) {
                let rest = cwd.trim_end_matches('/')[w.path.len()..].trim_start_matches('/');
                return Some(Place {
                    repo: w.repo.clone(),
                    slug: w.slug.clone(),
                    rest: rest.to_string(),
                    status: Status::Live {
                        worktree: w.path.clone(),
                    },
                });
            }
        }
        let r = parse_path_in(cwd, self.root.as_deref())?;
        // Only this machine's can be said to be archived. Another machine's
        // workspaces were never in this list to begin with.
        let status = if self.available && r.local {
            Status::Archived {
                checkout: parse_repo_list(&self.repos, &r.repo),
            }
        } else {
            Status::Unknown
        };
        Some(Place {
            repo: r.repo,
            slug: r.dir,
            rest: r.rest,
            status,
        })
    }
}

/// A live workspace to hand back to wsx instead of resuming here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jump {
    pub repo: String,
    pub slug: String,
    /// Which workspace it is, whatever it is called by the time the jump
    /// happens: a rename changes the slug, and never this.
    pub worktree: String,
}

impl Jump {
    pub fn label(&self) -> String {
        format!("{}/{}", self.repo, self.slug)
    }
}

/// Bring a live workspace forward: wsx selects it in the wsx already
/// running, or opens one on it. Whether that window is also raised is up to
/// wsx, and today only happens under Hyprland.
pub fn jump(j: &Jump) -> Result<(), String> {
    jump_with(&["wsx"], j, cfg!(target_os = "macos"))
}

/// The same command filed under each desktop's integration: `waybar` on
/// Linux, `menubar` on macOS.
fn jump_with(wsx: &[&str], j: &Jump, macos: bool) -> Result<(), String> {
    let group = if macos { "menubar" } else { "waybar" };
    run(
        wsx,
        &[group, "jump", &j.repo, &j.slug],
        Instant::now() + JUMP_DEADLINE,
        Keep::Stderr,
    )
    .map(|_| ())
}

/// Ask this machine's wsx what is live, and where each repo lives.
///
/// An answer is kept, for the browser to start from next time: wsx takes
/// about sixty milliseconds to say, and a start that waited for it spent
/// three quarters of its time doing so.
pub fn load() -> State {
    let state = load_with(&["wsx"]);
    if state.available {
        remember(&state, &cache_path());
    }
    state
}

fn cache_path() -> std::path::PathBuf {
    crate::index::state_dir().join("wsx.json")
}

/// What wsx said last time, to draw with until it answers again. Where its
/// worktrees are is read afresh: that is the environment's to say, not the
/// cache's.
pub fn remembered() -> State {
    remembered_at(&cache_path())
}

fn remembered_at(p: &std::path::Path) -> State {
    let mut s: State = std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    s.root = worktrees_root();
    s
}

fn remember(state: &State, p: &std::path::Path) {
    let Ok(body) = serde_json::to_vec(state) else {
        return;
    };
    // A name of its own for every write: a jump asks wsx on the main
    // thread while the ask behind the list may still be going, and two
    // writes into one temporary file could put half of each in place.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = p.with_extension(format!("json.tmp.{}.{n}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `load`, with the command that stands for wsx handed in.
fn load_with(wsx: &[&str]) -> State {
    let mut state = State::here();
    let deadline = Instant::now() + DEADLINE;
    // All three at once. Each is about twenty milliseconds of wsx starting
    // up, and this is asked at startup and on every rescan: one after
    // another they were the sum, together they are the slowest.
    let ask = |args: &'static [&'static str]| move || run(wsx, args, deadline, Keep::Stdout);
    let (workspaces, repos, shared) = std::thread::scope(|t| {
        let w = t.spawn(ask(&["workspace", "list"]));
        let r = t.spawn(ask(&["repo", "list"]));
        let s = t.spawn(ask(&["shared", "list"]));
        let join = |h: std::thread::ScopedJoinHandle<'_, Result<String, String>>| {
            h.join().unwrap_or_else(|_| Err("it panicked".into()))
        };
        (join(w), join(r), join(s))
    });
    let (Ok(workspaces), Ok(repos)) = (workspaces, repos) else {
        return state;
    };
    state.available = true;
    state.workspaces = parse_workspace_list(&workspaces);
    state.repos = repos;
    // A wsx without the command has no shared workspaces to report.
    if let Ok(shared) = shared {
        state.shared = parse_shared_list(&shared);
    }
    state
}

/// The tmux session of each line of `wsx shared list`: repo, slug, tmux
/// session, state, tab-separated.
pub fn parse_shared_list(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split('\t').nth(2))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Which of a child's streams is worth reading. The other goes nowhere.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    /// An answer.
    Stdout,
    /// An explanation, if it fails.
    Stderr,
}

/// Run `cmd args`, and give up at `deadline`, returning what it wrote on
/// the stream kept -- or, when it fails, the first thing it said.
///
/// Neither stream is ever inherited. mnemosyne's own stdout is the plan the
/// shell wrapper carries out line by line, so anything a child printed there
/// would be run as though it had been chosen. Its stderr is where the browser
/// is drawn, and a stray line would tear it.
pub(crate) fn run(
    cmd: &[&str],
    args: &[&str],
    deadline: Instant,
    keep: Keep,
) -> Result<String, String> {
    use std::io::Read;
    let (bin, first) = cmd.split_first().ok_or("nothing to run")?;
    let (out, err) = match keep {
        Keep::Stdout => (Stdio::piped(), Stdio::null()),
        Keep::Stderr => (Stdio::null(), Stdio::piped()),
    };
    let mut child = Command::new(bin)
        .args(first)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut pipe: Box<dyn Read + Send> = match keep {
        Keep::Stdout => Box::new(child.stdout.take().ok_or("no pipe")?),
        Keep::Stderr => Box::new(child.stderr.take().ok_or("no pipe")?),
    };
    // Read as it runs, not after. A child that fills the pipe waits for a
    // reader, and a reader that waits for the child to exit is not one.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("it did not answer in time".into());
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    // Bounded as well: anything it started in the background could still
    // be holding the pipe open after it has gone.
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(50));
    // Not finished is not an answer. Taken as an empty one, a list whose
    // writer left a child holding the pipe read as "no repos", and every
    // archived workspace resumed wherever you were.
    let out = rx
        .recv_timeout(left)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(|_| "it did not finish answering".to_string())?;
    if status.success() {
        Ok(out)
    } else {
        // wsx says `error: <what>`; the status line has no room for the
        // prefix, nor for anything after the first line.
        Err(out
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .map(|l| l.strip_prefix("error: ").unwrap_or(l).to_string())
            .unwrap_or_else(|| format!("it failed ({status})")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/home/u/.local/state/wsx/worktrees";

    fn parse(cwd: &str) -> Option<Ref> {
        parse_path_in(cwd, Some(ROOT))
    }

    fn at(repo: &str, dir: &str, rest: &str, local: bool) -> Option<Ref> {
        Some(Ref {
            repo: repo.into(),
            dir: dir.into(),
            rest: rest.into(),
            local,
        })
    }

    #[test]
    fn a_worktree_reads_as_its_repo_and_workspace() {
        assert_eq!(
            parse(&format!("{ROOT}/OS-DEV/shy-daffodil")),
            at("OS-DEV", "shy-daffodil", "", true)
        );
    }

    #[test]
    fn a_trailing_slash_is_the_same_folder() {
        assert_eq!(
            parse(&format!("{ROOT}/OS-DEV/shy-daffodil/")),
            at("OS-DEV", "shy-daffodil", "", true)
        );
        // and a root handed in with one matches the same way
        assert_eq!(
            parse_path_in(
                &format!("{ROOT}/OS-DEV/shy-daffodil"),
                Some(&format!("{ROOT}/"))
            ),
            at("OS-DEV", "shy-daffodil", "", true)
        );
    }

    #[test]
    fn a_folder_inside_a_worktree_belongs_to_that_workspace() {
        assert_eq!(
            parse(&format!("{ROOT}/OS-DEV/shy-daffodil/kernel/mm")),
            at("OS-DEV", "shy-daffodil", "kernel/mm", true)
        );
    }

    #[test]
    fn a_slug_with_a_space_in_it_is_still_one_slug() {
        // wsx takes whatever --name it is given, and does not refuse spaces.
        assert_eq!(
            parse(&format!("{ROOT}/mnemosyne/wsx integration")),
            at("mnemosyne", "wsx integration", "", true)
        );
        assert_eq!(
            parse(&format!("{ROOT}/meals backend/api fix/src")),
            at("meals backend", "api fix", "src", true)
        );
    }

    #[test]
    fn a_folder_outside_wsx_is_not_a_workspace() {
        for cwd in [
            "",
            "/",
            "/home/u",
            "/home/u/OS-DEV",
            "/home/u/.local/state",
            "/home/u/.local/state/wsx",
            ROOT,
            &format!("{ROOT}/"),
            // a repo's folder, with no workspace under it
            &format!("{ROOT}/OS-DEV"),
            &format!("{ROOT}/OS-DEV/"),
        ] {
            assert_eq!(parse(cwd), None, "{cwd:?}");
        }
    }

    #[test]
    fn a_folder_that_only_starts_the_same_is_not_a_worktree() {
        for cwd in [
            "/home/u/.local/state/wsx/worktrees-old/OS-DEV/shy-daffodil",
            "/home/u/.local/state/wsx/worktreesX/OS-DEV/shy-daffodil",
            "/home/u/.local/state/wsx-old/worktrees/OS-DEV/shy-daffodil",
        ] {
            assert_eq!(parse(cwd), None, "{cwd:?}");
        }
    }

    #[test]
    fn a_transcript_from_another_machine_is_recognised_but_not_local() {
        // Synced from a Mac, or from another user: the shape is wsx's even
        // though the root is not this machine's, so it gets a label -- and
        // only a label, since the wsx here has never heard of it.
        assert_eq!(
            parse("/Users/miles/.local/state/wsx/worktrees/OS-DEV/gdisk-app"),
            at("OS-DEV", "gdisk-app", "", false)
        );
        // the same with no local root known at all
        assert_eq!(
            parse_path_in("/home/u/.local/state/wsx/worktrees/OS-DEV/x", None),
            at("OS-DEV", "x", "", false)
        );
    }

    #[test]
    fn a_state_root_moved_elsewhere_is_this_machines_own() {
        let root = root_for("/home/u", Some("/data/state"), false).unwrap();
        assert_eq!(root, "/data/state/wsx/worktrees");
        assert_eq!(
            parse_path_in("/data/state/wsx/worktrees/OS-DEV/x", Some(&root)),
            at("OS-DEV", "x", "", true)
        );
        // the usual place is then somebody else's: labelled, not local
        assert_eq!(
            parse_path_in("/home/u/.local/state/wsx/worktrees/OS-DEV/x", Some(&root)),
            at("OS-DEV", "x", "", false)
        );
    }

    #[test]
    fn the_root_follows_xdg_on_linux_only_when_it_is_absolute() {
        let usual = Some("/home/u/.local/state/wsx/worktrees".to_string());
        assert_eq!(root_for("/home/u", None, false), usual);
        assert_eq!(root_for("/home/u", Some(""), false), usual);
        assert_eq!(root_for("/home/u", Some("relative/state"), false), usual);
        assert_eq!(
            root_for("/home/u", Some("/x/state/"), false).as_deref(),
            Some("/x/state/wsx/worktrees")
        );
        // an absolute XDG answers even with no home to fall back on
        assert_eq!(
            root_for("", Some("/x/state"), false).as_deref(),
            Some("/x/state/wsx/worktrees")
        );
        assert_eq!(root_for("", None, false), None);
    }

    #[test]
    fn the_root_ignores_xdg_on_macos() {
        // dirs::state_dir() has no answer on macOS, so wsx always falls back
        // to ~/.local/state there, whatever the environment says.
        assert_eq!(
            root_for("/Users/u", Some("/x/state"), true).as_deref(),
            Some("/Users/u/.local/state/wsx/worktrees")
        );
        assert_eq!(
            root_for("/Users/u", None, true).as_deref(),
            Some("/Users/u/.local/state/wsx/worktrees")
        );
        assert_eq!(root_for("", Some("/x/state"), true), None);
    }

    fn place(repo: &str, slug: &str, rest: &str) -> Place {
        Place {
            repo: repo.into(),
            slug: slug.into(),
            rest: rest.into(),
            status: Status::Unknown,
        }
    }

    fn listed(repo: &str, slug: &str, path: &str) -> Workspace {
        Workspace {
            repo: repo.into(),
            slug: slug.into(),
            branch: slug.into(),
            path: path.into(),
        }
    }

    fn known(workspaces: Vec<Workspace>) -> State {
        State {
            root: Some(ROOT.into()),
            available: true,
            workspaces,
            repos: [repo_line("OS-DEV", "/home/u/OS-DEV")].concat(),
            shared: Vec::new(),
        }
    }

    #[test]
    fn a_live_workspace_is_named_by_the_list_not_its_folder() {
        // Renamed from shy-daffodil to page-tables: the folder stayed where
        // it was, and only wsx knows what it is called now.
        let s = known(vec![listed(
            "OS-DEV",
            "page-tables",
            &format!("{ROOT}/OS-DEV/shy-daffodil"),
        )]);
        let p = s.place(&format!("{ROOT}/OS-DEV/shy-daffodil")).unwrap();
        assert_eq!(p.label(), "OS-DEV/page-tables");
        assert_eq!(
            p.status,
            Status::Live {
                worktree: format!("{ROOT}/OS-DEV/shy-daffodil")
            }
        );
        let deeper = s
            .place(&format!("{ROOT}/OS-DEV/shy-daffodil/kernel/"))
            .unwrap();
        assert_eq!(deeper.label(), "OS-DEV/page-tables/kernel");
    }

    #[test]
    fn a_folder_is_in_the_worktree_that_holds_it_and_no_other() {
        let s = known(vec![
            listed("r", "a", "/w/r/a"),
            listed("r", "a/b", "/w/r/a/b"),
            listed("r", "ab", "/w/r/ab"),
        ]);
        let slug = |cwd: &str| s.live(cwd).map(|w| w.slug.clone());
        assert_eq!(slug("/w/r/a").as_deref(), Some("a"));
        assert_eq!(slug("/w/r/a/src").as_deref(), Some("a"));
        assert_eq!(
            slug("/w/r/a/b/src").as_deref(),
            Some("a/b"),
            "the deeper one"
        );
        assert_eq!(slug("/w/r/ab").as_deref(), Some("ab"), "not a's");
        assert_eq!(slug("/w/r/abc"), None);
        assert_eq!(slug("/w/r"), None);
    }

    #[test]
    fn a_workspace_no_longer_listed_has_been_archived() {
        let s = known(vec![]);
        assert_eq!(
            s.place(&format!("{ROOT}/OS-DEV/gdisk-app")).unwrap().status,
            Status::Archived {
                checkout: Some("/home/u/OS-DEV".into())
            }
        );
        // a repo wsx no longer knows by that name leaves nowhere better
        assert_eq!(
            s.place(&format!("{ROOT}/renamed-repo/x")).unwrap().status,
            Status::Archived { checkout: None }
        );
    }

    #[test]
    fn another_machines_workspace_is_never_called_archived() {
        // It is missing from this list because this wsx never had it.
        let s = known(vec![]);
        let p = s
            .place("/Users/miles/.local/state/wsx/worktrees/OS-DEV/gdisk-app")
            .unwrap();
        assert_eq!(p.status, Status::Unknown);
    }

    #[test]
    fn a_list_nobody_could_get_is_never_consulted() {
        // wsx not there: a folder that would be live is only a label, and
        // nothing is claimed about whether it still is.
        let mut s = known(vec![listed("OS-DEV", "x", &format!("{ROOT}/OS-DEV/x"))]);
        s.available = false;
        let p = s.place(&format!("{ROOT}/OS-DEV/x")).unwrap();
        assert_eq!(p.status, Status::Unknown);
    }

    #[test]
    fn a_missing_wsx_leaves_everything_as_it_was() {
        let s = load_with(&["/nonexistent/definitely-not-wsx"]);
        assert!(!s.available);
        assert!(s.workspaces.is_empty() && s.repos.is_empty());
    }

    /// A stand-in for wsx: a script that answers the questions asked of
    /// it. The real one is never run from a test. It is handed to `sh` rather
    /// than made executable and run: executing a file just written races any
    /// other test forking at that moment, and fails as "text file busy".
    fn fake_wsx(dir: &std::path::Path, body: &str) -> String {
        let p = dir.join("wsx");
        std::fs::write(&p, format!("{body}\n")).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn what_wsx_answers_is_what_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_wsx(
            d.path(),
            r#"case "$1 $2" in
  "workspace list") printf 'OS-DEV\tshy-daffodil\tb\t/w/OS-DEV/shy-daffodil\n' ;;
  "repo list") printf '%-20s %s\n' OS-DEV /home/u/OS-DEV ;;
  *) exit 2 ;;
esac"#,
        );
        let s = load_with(&["sh", &bin]);
        assert!(s.available);
        assert_eq!(s.workspaces.len(), 1);
        assert_eq!(s.workspaces[0].slug, "shy-daffodil");
        assert_eq!(
            parse_repo_list(&s.repos, "OS-DEV").as_deref(),
            Some("/home/u/OS-DEV")
        );
    }

    #[test]
    fn a_wsx_that_fails_is_a_wsx_that_is_not_there() {
        let d = tempfile::tempdir().unwrap();
        let half = fake_wsx(
            d.path(),
            r#"[ "$1" = workspace ] && { printf 'r\ts\tb\t/w/r/s\n'; exit 0; }
echo 'no such command' >&2; exit 1"#,
        );
        let s = load_with(&["sh", &half]);
        assert!(!s.available, "half an answer is treated as none");
        assert!(s.workspaces.is_empty());
    }

    #[test]
    fn a_wsx_that_hangs_is_given_up_on() {
        let t = Instant::now();
        let r = run(
            &["sh"],
            &["-c", "exec sleep 5"],
            Instant::now() + Duration::from_millis(100),
            Keep::Stdout,
        );
        assert!(r.is_err(), "{r:?}");
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "waited {:?} for something that was given 100ms",
            t.elapsed()
        );
    }

    #[test]
    fn what_wsx_said_is_there_to_start_from_next_time() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wsx.json");
        assert!(!remembered_at(&p).available, "nothing said yet");
        let said = known(vec![listed(
            "OS-DEV",
            "shy-daffodil",
            &format!("{ROOT}/OS-DEV/shy-daffodil"),
        )]);
        remember(&said, &p);
        let back = remembered_at(&p);
        assert!(back.available);
        assert_eq!(back.workspaces, said.workspaces);
        assert_eq!(back.repos, said.repos);
    }

    #[test]
    fn wsx_is_asked_its_three_questions_at_once() {
        // Each answer takes as long as wsx takes to start; asked in turn,
        // startup paid for all three.
        let t = Instant::now();
        let state = load_with(&["sh", "-c", "sleep 0.15", "sh"]);
        assert!(state.available);
        assert!(
            t.elapsed() < Duration::from_millis(350),
            "{:?}: asked one after another",
            t.elapsed()
        );
    }

    #[test]
    fn an_answer_still_being_written_is_not_an_answer() {
        // It exits, but leaves something behind holding its output open.
        // What it has said so far is not the whole list.
        let r = run(
            &["sh"],
            &["-c", "echo OS-DEV /home/u/OS-DEV; (sleep 3) &"],
            Instant::now() + Duration::from_millis(200),
            Keep::Stdout,
        );
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn shared_workspaces_are_read_by_their_tmux_session() {
        let out = "mnemosyne\tmn-bug-hunt\twsx-mnemosyne-mn-bug-hunt\talive\n\nOS-DEV\tx\t\tdead\n";
        assert_eq!(parse_shared_list(out), vec!["wsx-mnemosyne-mn-bug-hunt"]);
    }

    #[test]
    fn a_long_answer_does_not_wedge_the_reader() {
        // More than a pipe holds. Waiting for the exit before reading would
        // leave the child blocked on a full pipe until the deadline.
        let r = run(
            &["sh"],
            &["-c", "head -c 300000 /dev/zero | tr '\\0' x"],
            Instant::now() + Duration::from_secs(5),
            Keep::Stdout,
        )
        .unwrap();
        assert_eq!(r.len(), 300_000);
    }

    fn jump_to(slug: &str) -> Jump {
        Jump {
            repo: "OS-DEV".into(),
            slug: slug.into(),
            worktree: format!("{ROOT}/OS-DEV/{slug}"),
        }
    }

    #[test]
    fn a_jump_asks_each_desktops_integration() {
        // The stand-in echoes its arguments back as a failure, which is the
        // one stream a jump reads.
        let d = tempfile::tempdir().unwrap();
        let echo = fake_wsx(d.path(), r#"echo "$@" >&2; exit 1"#);
        let said = |macos| jump_with(&["sh", &echo], &jump_to("shy daffodil"), macos).unwrap_err();
        assert_eq!(said(false), "waybar jump OS-DEV shy daffodil");
        assert_eq!(said(true), "menubar jump OS-DEV shy daffodil");
    }

    #[test]
    fn a_jump_that_fails_says_why_in_wsxs_words() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake_wsx(
            d.path(),
            r#"echo "on stdout, which goes nowhere"
printf '\nerror: no workspace named x\nmore detail\n' >&2
exit 1"#,
        );
        assert_eq!(
            jump_with(&["sh", &bin], &jump_to("x"), false).unwrap_err(),
            "no workspace named x"
        );
        let ok = fake_wsx(d.path(), "echo chatter >&2; exit 0");
        assert!(jump_with(&["sh", &ok], &jump_to("x"), false).is_ok());
    }

    #[test]
    fn a_non_zero_exit_is_a_failure_whatever_it_printed() {
        let r = run(
            &["sh"],
            &["-c", "echo looks fine; exit 3"],
            Instant::now() + Duration::from_secs(5),
            Keep::Stdout,
        );
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn a_label_is_the_repo_and_the_workspace() {
        assert_eq!(
            place("OS-DEV", "shy-daffodil", "").label(),
            "OS-DEV/shy-daffodil"
        );
        assert_eq!(
            place("OS-DEV", "shy-daffodil", "kernel/mm").label(),
            "OS-DEV/shy-daffodil/kernel/mm",
            "a session further down says where"
        );
    }

    #[test]
    fn a_repo_tag_is_what_typing_it_would_give() {
        // T normalises what you type; a tag it could never match is no tag.
        for repo in ["OS-DEV", "meals backend", "Mixed.Case_repo"] {
            let t = place(repo, "x", "").tag();
            assert!(t.starts_with(TAG_PREFIX), "{t}");
            assert_eq!(crate::meta::normalize_tag(&t), t, "{repo:?} gave {t:?}");
        }
        assert_eq!(place("OS-DEV", "x", "").tag(), "wsx/os-dev");
        assert_eq!(place("meals backend", "x", "").tag(), "wsx/meals-backend");
    }

    #[test]
    fn a_label_that_must_be_cut_loses_the_repo_before_the_workspace() {
        // The folder column is 12, 16 or 20 cells. Clipping from the end
        // kept "OS-DEV/shy-daf…", which is the part every row shares.
        let p = place("OS-DEV", "shy-daffodil", "");
        assert_eq!(p.fit(20), "OS-DEV/shy-daffodil");
        assert_eq!(p.fit(16), "OS…/shy-daffodil");
        assert_eq!(p.fit(15), "O…/shy-daffodil");
        assert_eq!(p.fit(14), "shy-daffodil", "one column of repo says nothing");
        assert_eq!(p.fit(12), "shy-daffodil", "no room for any of the repo");
        assert_eq!(p.fit(8), "shy-daf…");
        for w in 0..30 {
            assert!(
                crate::model::width(&p.fit(w)) <= w,
                "fit({w}) = {:?} is too wide",
                p.fit(w)
            );
        }
    }

    #[test]
    fn a_state_with_no_home_still_knows_the_usual_place() {
        // No local root: nothing is this machine's, but the shape is still
        // recognised, so the label does not depend on the environment.
        let s = State::default();
        assert_eq!(
            s.place("/home/u/.local/state/wsx/worktrees/OS-DEV/shy-daffodil"),
            Some(place("OS-DEV", "shy-daffodil", ""))
        );
        assert_eq!(s.place("/home/u/OS-DEV"), None);
    }

    #[test]
    fn the_workspace_list_is_split_on_tabs_only() {
        let out = "OS-DEV\tshy-daffodil\tmiles/shy-daffodil\t/home/u/.local/state/wsx/worktrees/OS-DEV/shy-daffodil\n\
                   meals backend\tapi fix\tapi-fix\t/home/u/.local/state/wsx/worktrees/meals backend/api fix\n";
        let got = parse_workspace_list(out);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].repo, "OS-DEV");
        assert_eq!(got[0].slug, "shy-daffodil");
        assert_eq!(got[0].branch, "miles/shy-daffodil");
        assert_eq!(
            got[0].path,
            "/home/u/.local/state/wsx/worktrees/OS-DEV/shy-daffodil"
        );
        assert_eq!(got[1].repo, "meals backend");
        assert_eq!(got[1].slug, "api fix", "a space is not a separator");
        assert!(got[1].path.ends_with("/meals backend/api fix"));
    }

    #[test]
    fn a_line_that_is_not_a_workspace_is_skipped() {
        let out = "\n\
                   \t\t\t\n\
                   just some words\n\
                   OS-DEV\tno-path\tbranch\n\
                   OS-DEV\trelative\tbranch\tnot/absolute\n\
                   \tno-repo\tbranch\t/p\n\
                   OS-DEV\t\tbranch\t/p\n\
                   OS-DEV\tkept\t\t/p/kept/\r\n";
        let got = parse_workspace_list(out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].slug, "kept", "an empty branch is still a workspace");
        assert_eq!(got[0].path, "/p/kept", "no trailing slash or \\r");
        assert!(parse_workspace_list("").is_empty());
    }

    /// What `wsx repo list` prints: `{:<20} {}`.
    fn repo_line(name: &str, path: &str) -> String {
        format!("{name:<20} {path}\n")
    }

    #[test]
    fn a_repo_is_found_by_its_name_and_nothing_like_it() {
        let out = [
            repo_line("OS", "/home/u/os"),
            repo_line("OS-DEV", "/home/u/OS-DEV"),
            repo_line("mnemosyne", "/home/u/mnemosyne"),
        ]
        .concat();
        assert_eq!(
            parse_repo_list(&out, "OS-DEV").as_deref(),
            Some("/home/u/OS-DEV")
        );
        assert_eq!(parse_repo_list(&out, "OS").as_deref(), Some("/home/u/os"));
        assert_eq!(parse_repo_list(&out, "OS-D"), None, "only part of a name");
        assert_eq!(parse_repo_list(&out, "nothing"), None);
        assert_eq!(parse_repo_list(&out, ""), None);
    }

    #[test]
    fn a_repo_name_longer_than_the_padding_still_parses() {
        let name = "a-repo-with-a-rather-long-name";
        assert!(name.len() > 20);
        let out = repo_line(name, "/srv/code/long");
        assert_eq!(
            out,
            format!("{name} /srv/code/long\n"),
            "one space, no padding"
        );
        assert_eq!(
            parse_repo_list(&out, name).as_deref(),
            Some("/srv/code/long")
        );
    }

    #[test]
    fn a_repo_name_with_a_space_is_not_mistaken_for_another() {
        let out = [
            repo_line("meals", "/home/u/meals"),
            repo_line("meals backend", "/home/u/My Code/meals-backend"),
        ]
        .concat();
        assert_eq!(
            parse_repo_list(&out, "meals backend").as_deref(),
            Some("/home/u/My Code/meals-backend"),
            "a space in the path survives too"
        );
        assert_eq!(
            parse_repo_list(&out, "meals").as_deref(),
            Some("/home/u/meals")
        );
        // with the shorter one listed second, `meals` must still not stop at
        // the line for `meals backend`
        let reversed = [
            repo_line("meals backend", "/home/u/meals-backend"),
            repo_line("meals", "/home/u/meals"),
        ]
        .concat();
        assert_eq!(
            parse_repo_list(&reversed, "meals").as_deref(),
            Some("/home/u/meals")
        );
    }

    #[test]
    fn a_malformed_repo_line_is_not_a_checkout() {
        let out = "\n\
                   OS-DEV\n\
                   OS-DEV               \n\
                   OS-DEV               relative/path\n\
                   OS-DEV/home/u/glued\n";
        assert_eq!(parse_repo_list(out, "OS-DEV"), None);
        assert_eq!(
            parse_repo_list("OS-DEV               /home/u/OS-DEV\r\n", "OS-DEV").as_deref(),
            Some("/home/u/OS-DEV")
        );
    }
}

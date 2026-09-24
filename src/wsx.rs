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

/// Where every machine's wsx keeps worktrees unless told otherwise. Matched
/// anywhere in a path, so a transcript synced from another machine -- another
/// home, another user name -- is still recognised for what it is.
const SEGMENT: &str = "/.local/state/wsx/worktrees/";

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
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl Place {
    /// `OS-DEV/shy-daffodil`, and the folder under it if it was not the top.
    pub fn label(&self) -> String {
        format!("{}/{}", self.repo, self.tail())
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

/// What this machine's wsx has to say about a folder.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// Where its worktrees are, if there is a home to find them under.
    pub root: Option<String>,
}

impl State {
    /// Only what the environment says. Nothing is run.
    pub fn here() -> State {
        State {
            root: worktrees_root(),
        }
    }

    /// The workspace a session's folder belongs to, if it is in one.
    pub fn place(&self, cwd: &str) -> Option<Place> {
        let r = parse_path_in(cwd, self.root.as_deref())?;
        Some(Place {
            repo: r.repo,
            slug: r.dir,
            rest: r.rest,
        })
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
        }
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

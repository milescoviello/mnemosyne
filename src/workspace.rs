//! What was open last time the machine was up.
//!
//! A reboot takes every Claude session with it, and the transcripts alone
//! cannot say which of two hundred sessions you actually had in front of you.
//! So this file remembers the set: a small JSON record of the sessions that
//! were running, kept beside the favourites for the same reason — it is not
//! derivable from anything else.
//!
//! There is no daemon here. mnemosyne exists only while you are looking at
//! it, so the snapshot is written whenever it runs and whenever it launches
//! something. That makes the record as fresh as your last `mn`, which in
//! practice is most of the way there, and costs nothing to run.
//!
//! The structure is two lists rather than one. `current` is this boot's set,
//! rewritten as it changes; `previous` is the set from the boot before, which
//! is what the reopen offer is made from. Keeping them apart is what stops
//! the first run after a reboot — when nothing is running yet — from
//! overwriting the very list it is meant to offer you.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// One session worth reopening.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub perms: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Snapshot {
    /// Which boot these were seen in. Empty when the platform would not say.
    #[serde(default)]
    pub boot: String,
    #[serde(default)]
    pub saved_at: i64,
    #[serde(default)]
    pub sessions: Vec<Entry>,
    /// Waved away, so the offer stops appearing on its own. The list is kept
    /// rather than deleted: `--reopen` is still allowed to act on it, which
    /// is what makes dismissing it a safe thing to do.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Workspace {
    #[serde(default)]
    pub current: Snapshot,
    /// The last set seen under a different boot, still waiting to be offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Snapshot>,
}

/// More than this and the offer stops being an offer and becomes a fork bomb
/// of terminal windows. Newest first, so the cut falls on the oldest.
const MAX: usize = 24;

pub fn path() -> PathBuf {
    crate::index::state_dir().join("workspace.json")
}

/// An identifier that changes when the machine reboots and not otherwise.
///
/// Linux hands one out directly. Everywhere else the boot *time* serves the
/// same purpose, since it too is constant within a boot and different after
/// one. When nothing can be determined the result is empty, and the caller
/// falls back to never claiming a reboot happened — an offer that never
/// appears is better than one that appears after every run.
pub fn boot_id() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/stat") {
        if let Some(b) = parse_btime(&s) {
            return b;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Ok(o) = std::process::Command::new("sysctl")
            .args(["-n", "kern.boottime"])
            .output()
        {
            if o.status.success() {
                if let Some(b) = parse_boottime(&String::from_utf8_lossy(&o.stdout)) {
                    return b;
                }
            }
        }
    }
    String::new()
}

/// `btime 1758300000` out of /proc/stat.
fn parse_btime(stat: &str) -> Option<String> {
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .map(|v| format!("boot-{}", v.trim()))
}

/// `{ sec = 1758300000, usec = 0 } Mon Sep 15 ...` out of sysctl on BSD/macOS.
///
/// Only called off Linux, but compiled and tested everywhere: a parser that
/// is only exercised on the platform nobody here runs is a parser nobody
/// finds out is broken.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_boottime(out: &str) -> Option<String> {
    let after = out.split("sec").nth(1)?; // " = 1758300000, usec = 0 } ..."
    let digits: String = after
        .trim_start_matches([' ', '='])
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("boot-{digits}"))
    }
}

pub fn load() -> Workspace {
    load_at(&path())
}

pub fn save(w: &Workspace) -> std::io::Result<()> {
    save_at(&path(), w)
}

/// Everything that touches disk goes through an explicit path, so the tests
/// can exercise it against a temporary directory instead of whatever the
/// machine running them happens to have in its home.
pub fn load_at(p: &std::path::Path) -> Workspace {
    let mut w: Workspace = match std::fs::read(p) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Workspace::default(),
    };
    // The titles are drawn on the reopen banner, and this file is only ever
    // as clean as whatever last wrote it -- a hand edit can put an escape
    // sequence in one as easily as mn can put a title.
    let snaps = std::iter::once(&mut w.current).chain(w.previous.as_mut());
    for e in snaps.flat_map(|s| s.sessions.iter_mut()) {
        e.title = crate::scan::squash(&e.title, 200);
    }
    w
}

pub fn save_at(p: &std::path::Path, w: &Workspace) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Same temp-then-rename as the favourites file: a crash mid-write should
    // lose the update, never the file.
    // Named for this process: every mn writes this file, and two sharing a
    // temp name could truncate each other's half-written copy and rename it
    // into place, which a third then read as nothing to offer.
    let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        let body = serde_json::to_vec_pretty(w).unwrap_or_else(|_| b"{}".to_vec());
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, p)
}

/// Fold a freshly observed set into the stored one.
///
/// `authoritative` says whether `observed` is the whole truth. On Linux it is:
/// the process table was read, so anything absent from it really has gone.
/// Elsewhere there is no way to enumerate running sessions, and `observed`
/// holds only what this run launched or found waiting in tmux — so it is
/// merged on top of what was already recorded rather than replacing it.
pub fn fold(
    mut stored: Workspace,
    observed: Vec<Entry>,
    boot: &str,
    authoritative: bool,
) -> Workspace {
    let rolled = !boot.is_empty() && !stored.current.boot.is_empty() && stored.current.boot != boot;
    if rolled {
        // A reboot happened since the last write. The set from before it
        // becomes the offer -- unless it was empty, in which case an earlier
        // offer is still outstanding and must not be thrown away by a boot
        // where nothing ever ran.
        if !stored.current.sessions.is_empty() {
            stored.previous = Some(std::mem::take(&mut stored.current));
        }
        stored.current = Snapshot::default();
    }

    let mut next = observed;
    if !authoritative {
        // Keep what we cannot re-observe, newest first, without duplicating
        // anything the caller just saw.
        for e in stored.current.sessions {
            if !next.iter().any(|n| n.id == e.id) {
                next.push(e);
            }
        }
    }
    next.truncate(MAX);

    stored.current = Snapshot {
        boot: boot.to_string(),
        saved_at: chrono::Utc::now().timestamp(),
        sessions: next,
        dismissed: false,
    };
    stored
}

/// Record what is open now. Best effort throughout: this is a convenience,
/// and it must never be the reason the tool fails to start.
pub fn record(observed: Vec<Entry>, authoritative: bool) {
    let w = fold(load(), observed, &boot_id(), authoritative);
    let _ = save(&w);
}

/// The sessions worth offering to reopen, after a reboot and no later.
///
/// `running` names sessions that are already up; reopening one of those would
/// put a second client on a transcript that already has one.
pub fn pending(
    w: &Workspace,
    boot: &str,
    include_dismissed: bool,
    running: &dyn Fn(&str) -> bool,
) -> Vec<Entry> {
    // No boot identity means no way to know a reboot happened, so nothing is
    // claimed. `--reopen` still works by hand.
    if boot.is_empty() {
        return Vec::new();
    }
    let Some(prev) = &w.previous else {
        return Vec::new();
    };
    if prev.boot == boot || (prev.dismissed && !include_dismissed) {
        return Vec::new();
    }
    prev.sessions
        .iter()
        .filter(|e| !e.id.is_empty() && !running(&e.id))
        .filter(|e| !e.cwd.is_empty() && std::path::Path::new(&e.cwd).is_dir())
        .cloned()
        .collect()
}

/// Stop offering, without forgetting. `mn --reopen` can still act on it.
pub fn dismiss_previous() {
    dismiss_previous_at(&path());
}

pub fn dismiss_previous_at(p: &std::path::Path) {
    let mut w = load_at(p);
    if let Some(prev) = w.previous.as_mut() {
        if !prev.dismissed {
            prev.dismissed = true;
            let _ = save_at(p, &w);
        }
    }
}

/// The offer was taken: forget it, so it is never made twice.
pub fn clear_previous() {
    clear_previous_at(&path());
}

pub fn clear_previous_at(p: &std::path::Path) {
    let mut w = load_at(p);
    if w.previous.is_some() {
        w.previous = None;
        let _ = save_at(p, &w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str) -> Entry {
        Entry {
            id: id.into(),
            cwd: "/tmp".into(),
            ..Default::default()
        }
    }

    fn ws(boot: &str, ids: &[&str]) -> Workspace {
        Workspace {
            current: Snapshot {
                boot: boot.into(),
                saved_at: 1,
                sessions: ids.iter().map(|i| e(i)).collect(),
                dismissed: false,
            },
            previous: None,
        }
    }

    #[test]
    fn btime_is_read_out_of_proc_stat() {
        let stat = "cpu  1 2 3\nbtime 1758300000\nprocesses 99\n";
        assert_eq!(parse_btime(stat).as_deref(), Some("boot-1758300000"));
        assert_eq!(parse_btime("cpu 1 2 3\n"), None);
    }

    #[test]
    fn bsd_boottime_is_parsed_out_of_sysctl() {
        let out = "{ sec = 1758300000, usec = 0 } Mon Sep 15 10:00:00 2025\n";
        assert_eq!(parse_boottime(out).as_deref(), Some("boot-1758300000"));
        assert_eq!(parse_boottime("nonsense"), None);
    }

    #[test]
    fn within_one_boot_the_set_is_simply_replaced() {
        let w = fold(ws("A", &["one", "two"]), vec![e("two")], "A", true);
        assert_eq!(
            w.current.sessions,
            vec![e("two")],
            "a closed session lingered"
        );
        assert!(
            w.previous.is_none(),
            "no reboot happened, so nothing to offer"
        );
    }

    #[test]
    fn a_fresh_install_makes_no_offer() {
        // Nothing recorded yet: there is no earlier boot to have lost
        // anything, and an offer here would be inventing one.
        let w = fold(Workspace::default(), vec![e("one")], "A", true);
        assert!(w.previous.is_none());
        assert!(pending(&w, "A", false, &|_| false).is_empty());
    }

    #[test]
    fn a_reboot_moves_the_old_set_into_the_offer() {
        let w = fold(ws("A", &["one", "two"]), vec![], "B", true);
        let prev = w.previous.expect("the pre-reboot set should be on offer");
        assert_eq!(prev.sessions.len(), 2);
        assert_eq!(prev.boot, "A");
        assert!(w.current.sessions.is_empty());
        assert_eq!(w.current.boot, "B");
    }

    #[test]
    fn a_quiet_boot_does_not_wipe_an_outstanding_offer() {
        // Reboot with two open -> the offer is made. Then a boot where mn runs
        // but nothing is open at all, then another reboot. The original offer
        // has still never been answered, so it must survive.
        let w = fold(ws("A", &["one", "two"]), vec![], "B", true);
        let w = fold(w, vec![], "C", true);
        let prev = w.previous.expect("offer was dropped by an empty boot");
        assert_eq!(prev.sessions.len(), 2, "the wrong set was kept");
        assert_eq!(prev.boot, "A");
    }

    #[test]
    fn without_process_detection_earlier_launches_are_not_forgotten() {
        // macOS: run mn and launch `one`, then run mn again with nothing new.
        // The second run cannot see processes, so it must not conclude that
        // the first launch has ended.
        let w = fold(ws("A", &["one"]), vec![e("two")], "A", false);
        let ids: Vec<&str> = w.current.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["two", "one"], "a launch was lost, or duplicated");
    }

    #[test]
    fn a_merge_never_duplicates_a_session() {
        let w = fold(ws("A", &["one"]), vec![e("one")], "A", false);
        assert_eq!(w.current.sessions.len(), 1);
    }

    #[test]
    fn the_recorded_set_is_capped() {
        let ids: Vec<String> = (0..MAX + 10).map(|i| format!("s{i}")).collect();
        let observed: Vec<Entry> = ids.iter().map(|i| e(i)).collect();
        let w = fold(Workspace::default(), observed, "A", true);
        assert_eq!(w.current.sessions.len(), MAX);
        assert_eq!(w.current.sessions[0].id, "s0", "kept the wrong end");
    }

    #[test]
    fn nothing_is_offered_until_a_reboot_has_happened() {
        let w = fold(ws("A", &["one"]), vec![e("one")], "A", true);
        assert!(pending(&w, "A", false, &|_| false).is_empty());
    }

    #[test]
    fn the_offer_skips_what_is_already_running() {
        let w = fold(ws("A", &["one", "two"]), vec![], "B", true);
        let got = pending(&w, "B", false, &|id| id == "one");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "two");
    }

    #[test]
    fn the_offer_skips_folders_that_are_gone() {
        let mut w = fold(ws("A", &["one"]), vec![], "B", true);
        w.previous.as_mut().unwrap().sessions[0].cwd = "/no/such/place/at/all".into();
        assert!(pending(&w, "B", false, &|_| false).is_empty());
    }

    #[test]
    fn dismissing_stops_the_offer_but_keeps_the_list() {
        let mut w = fold(ws("A", &["one"]), vec![], "B", true);
        w.previous.as_mut().unwrap().dismissed = true;
        assert!(
            pending(&w, "B", false, &|_| false).is_empty(),
            "a dismissed offer should not keep appearing"
        );
        assert_eq!(
            pending(&w, "B", true, &|_| false).len(),
            1,
            "--reopen must still be able to act on it"
        );
    }

    #[test]
    fn an_unknown_boot_makes_no_claims() {
        // Some platform we cannot identify a boot on: better to offer nothing
        // than to offer the same list after every single run.
        let w = fold(ws("", &["one"]), vec![], "", true);
        assert!(pending(&w, "", false, &|_| false).is_empty());
    }

    #[test]
    fn dismissing_survives_a_restart_but_keeps_the_list() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("workspace.json");
        let w = fold(ws("A", &["one"]), vec![], "B", true);
        save_at(&p, &w).unwrap();

        dismiss_previous_at(&p);
        let back = load_at(&p);
        let prev = back.previous.expect("dismissing must not delete the list");
        assert!(prev.dismissed);
        assert_eq!(prev.sessions.len(), 1, "the list itself was thrown away");
    }

    #[test]
    fn taking_the_offer_forgets_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("workspace.json");
        save_at(&p, &fold(ws("A", &["one"]), vec![], "B", true)).unwrap();

        clear_previous_at(&p);
        assert!(
            load_at(&p).previous.is_none(),
            "an accepted offer would be made twice"
        );
    }

    #[test]
    fn a_half_written_file_is_replaced_not_appended_to() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("workspace.json");
        std::fs::write(&p, b"{\"current\": {\"sessions\": [{\"id\"").unwrap();
        assert!(load_at(&p).current.sessions.is_empty());
        save_at(&p, &fold(Workspace::default(), vec![e("one")], "A", true)).unwrap();
        assert_eq!(load_at(&p).current.sessions.len(), 1);
    }

    #[test]
    fn an_unreadable_file_is_an_empty_workspace_not_a_crash() {
        let w: Workspace = serde_json::from_slice(b"{ this is not json").unwrap_or_default();
        assert!(w.current.sessions.is_empty() && w.previous.is_none());
    }

    #[test]
    fn a_workspace_survives_a_round_trip() {
        let w = fold(
            Workspace::default(),
            vec![Entry {
                id: "one".into(),
                cwd: "/tmp".into(),
                model: "claude-opus-5".into(),
                perms: "bypassPermissions".into(),
                title: "a title".into(),
            }],
            "A",
            true,
        );
        let text = serde_json::to_vec(&w).unwrap();
        let back: Workspace = serde_json::from_slice(&text).unwrap();
        assert_eq!(back.current.sessions, w.current.sessions);
    }

    #[test]
    fn a_title_cannot_carry_an_escape_onto_the_reopen_banner() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("workspace.json");
        std::fs::write(
            &p,
            r#"{"previous":{"boot":"A","sessions":[{"id":"x","cwd":"/tmp","title":"evil \u001b[2J\u001b]0;pwned\u0007 title"}]}}"#,
        )
        .unwrap();
        let w = load_at(&p);
        let title = &w.previous.unwrap().sessions[0].title;
        assert!(!title.chars().any(|c| c.is_control()), "{title:?}");
        assert!(title.starts_with("evil"));
    }
}

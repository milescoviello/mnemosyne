//! The user's own overlay: favourites, tags and notes.
//!
//! Deliberately a separate JSON file rather than rows in the index database.
//! The index is a disposable cache that can be deleted and rebuilt from the
//! transcripts at any time; this file is the only thing here that cannot be
//! regenerated, so it stays small, human-readable and easy to back up.
//!
//! Never committed to the repository: tag names and session titles reference
//! real hosts.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "is_false")]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize, Default)]
pub struct Meta {
    #[serde(default)]
    pub sessions: BTreeMap<String, Entry>,
    /// The sessions this run has changed, which are all a save writes over
    /// what is on disk. Never stored.
    #[serde(skip)]
    changed: std::collections::BTreeSet<String>,
}

/// Counts of what the overlay marks, split from what it no longer reaches.
#[derive(Default, Debug, PartialEq)]
pub struct Summary {
    pub favourites: usize,
    pub tags: usize,
    /// Entries whose session is not in the corpus any more.
    pub orphans: usize,
}

pub fn meta_path() -> PathBuf {
    crate::index::state_dir().join("meta.json")
}

impl Meta {
    pub fn load() -> Meta {
        Meta::load_at(&meta_path())
    }

    pub fn load_at(p: &std::path::Path) -> Meta {
        let mut m: Meta = match std::fs::read(p) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(m) => m,
                Err(e) => {
                    // A hand edit with a stray comma, or a sync caught
                    // halfway. It used to be read as empty without a word,
                    // saved over at the next favourite, and three edits
                    // later rotated out of every backup too. It goes where
                    // the rotation never reaches, and says where.
                    let aside = p.with_extension(format!(
                        "json.unreadable-{}",
                        chrono::Utc::now().timestamp()
                    ));
                    let _ = std::fs::rename(p, &aside);
                    eprintln!(
                        "mnemosyne: {} could not be read ({e}) — it is kept as {}",
                        p.display(),
                        aside.display()
                    );
                    Meta::default()
                }
            },
            Err(_) => Meta::default(),
        };
        m.clean();
        m
    }

    /// Clean on the way in, not only on the way out. This file is edited by
    /// hand and synced between machines, and everything in it is drawn to a
    /// terminal -- an escape sequence in a note or a tag is a file deciding
    /// what your screen does. Tags are also only ever matched in their
    /// normal form, so a hand-typed `HomeLab` could be neither removed nor
    /// renamed.
    fn clean(&mut self) {
        for e in self.sessions.values_mut() {
            e.note = clean_note(&e.note);
            e.tags = e
                .tags
                .iter()
                .map(|t| normalize_tag(t))
                .filter(|t| !t.is_empty())
                .collect();
            e.tags.sort();
            e.tags.dedup();
        }
    }

    /// Write via temp file + rename, keeping a few generations behind it.
    ///
    /// Everything else here can be rebuilt from the transcripts; favourites,
    /// tags and notes cannot. Atomic replacement stops a crash mid-write from
    /// truncating it, and the rotation covers the other way of losing data —
    /// a write that succeeds but contains the wrong thing.
    pub fn save(&mut self) -> Result<()> {
        self.save_at(&meta_path())
    }

    /// Save what this run changed, over whatever is on disk now.
    ///
    /// Two browsers open at once each loaded the file once and saved the
    /// whole of what they had: a tag added in one was gone the moment the
    /// other favourited something. Only the sessions changed here are
    /// written over the file as it is when saving, under a lock so two
    /// saves take turns, and what the other wrote comes back in. A file
    /// that is missing or unreadable by now has nothing to merge with, and
    /// is written from everything this run knows.
    pub fn save_at(&mut self, p: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(p.parent().unwrap())?;
        let _lock = lock(p)?;
        let on_disk = std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok());
        if let Some(mut disk) = on_disk {
            disk.clean();
            for id in std::mem::take(&mut self.changed) {
                match self.sessions.get(&id) {
                    Some(e) => disk.sessions.insert(id, e.clone()),
                    None => disk.sessions.remove(&id),
                };
            }
            self.sessions = disk.sessions;
        }
        self.changed.clear();
        rotate_backups(p);
        // Ours alone. Two instances saving at once shared one temp name, so
        // one could truncate the file the other was halfway through writing
        // and then rename it into place.
        let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    /// What this overlay actually describes, given the sessions that exist.
    ///
    /// Entries outlive their transcripts -- retention deletes those, and
    /// the overlay is deliberately never pruned, so a favourite survives a
    /// session coming back. Counting them regardless meant `--stats` could
    /// report favourites that nothing in the list was marked with.
    pub fn summary(&self, known: &std::collections::HashSet<String>) -> Summary {
        let mut s = Summary::default();
        let mut tags = std::collections::HashSet::new();
        for (id, e) in &self.sessions {
            if known.contains(id) {
                if e.favorite {
                    s.favourites += 1;
                }
                tags.extend(e.tags.iter().cloned());
            } else {
                s.orphans += 1;
            }
        }
        s.tags = tags.len();
        s
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.sessions.get(id)
    }

    pub fn toggle_favorite(&mut self, id: &str) -> bool {
        self.changed.insert(id.to_string());
        let e = self.sessions.entry(id.to_string()).or_default();
        e.favorite = !e.favorite;
        let now = e.favorite;
        self.gc(id);
        now
    }

    pub fn add_tag(&mut self, id: &str, tag: &str) {
        let tag = normalize_tag(tag);
        if tag.is_empty() {
            return;
        }
        self.changed.insert(id.to_string());
        let e = self.sessions.entry(id.to_string()).or_default();
        if !e.tags.iter().any(|t| t == &tag) {
            e.tags.push(tag);
            e.tags.sort();
        }
    }

    pub fn remove_tag(&mut self, id: &str, tag: &str) {
        self.changed.insert(id.to_string());
        let tag = normalize_tag(tag);
        if let Some(e) = self.sessions.get_mut(id) {
            e.tags.retain(|t| t != &tag);
        }
        self.gc(id);
    }

    pub fn set_note(&mut self, id: &str, note: &str) {
        self.changed.insert(id.to_string());
        let e = self.sessions.entry(id.to_string()).or_default();
        e.note = clean_note(note);
        self.gc(id);
    }

    /// Drop entries that no longer carry any information.
    fn gc(&mut self, id: &str) {
        if let Some(e) = self.sessions.get(id) {
            if !e.favorite && e.tags.is_empty() && e.note.is_empty() {
                self.sessions.remove(id);
            }
        }
    }

    /// Rename a tag everywhere it appears. Returns how many sessions changed.
    pub fn rename_tag(&mut self, from: &str, to: &str) -> usize {
        let (from, to) = (normalize_tag(from), normalize_tag(to));
        if from.is_empty() || to.is_empty() || from == to {
            return 0;
        }
        let mut n = 0;
        for (id, e) in self.sessions.iter_mut() {
            if let Some(pos) = e.tags.iter().position(|t| *t == from) {
                self.changed.insert(id.clone());
                e.tags.remove(pos);
                if !e.tags.contains(&to) {
                    e.tags.push(to.clone());
                }
                e.tags.sort();
                n += 1;
            }
        }
        n
    }

    /// Every tag in use, with counts, most-used first. Drives completion.
    pub fn all_tags(&self) -> Vec<(String, usize)> {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for e in self.sessions.values() {
            for t in &e.tags {
                *m.entry(t.clone()).or_insert(0) += 1;
            }
        }
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

/// Hold the file's lock until the returned handle is dropped.
fn lock(p: &std::path::Path) -> Result<std::fs::File> {
    let f = std::fs::File::create(p.with_extension("json.lock"))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: a valid descriptor for as long as `f` lives.
        unsafe {
            libc::flock(f.as_raw_fd(), libc::LOCK_EX);
        }
    }
    Ok(f)
}

/// How many previous versions of the overlay to keep.
const BACKUPS: usize = 3;

/// Shuffle meta.json -> .1 -> .2 -> .3 before it is replaced.
///
/// Only rotates when the current file differs from the newest backup, so
/// toggling one favourite repeatedly cannot push real history out.
fn rotate_backups(path: &std::path::Path) {
    let Ok(current) = std::fs::read(path) else {
        return;
    };
    let newest = path.with_extension("json.1");
    if std::fs::read(&newest).is_ok_and(|b| b == current) {
        return;
    }
    for i in (1..BACKUPS).rev() {
        let from = path.with_extension(format!("json.{i}"));
        let to = path.with_extension(format!("json.{}", i + 1));
        let _ = std::fs::rename(from, to);
    }
    let _ = std::fs::write(newest, current);
}

/// A note, with anything that would reach the terminal as an instruction
/// taken out. `squash` drops control characters and folds whitespace; the
/// cap stops one note pushing everything else off a rail.
fn clean_note(note: &str) -> String {
    crate::scan::squash(note.trim(), 500)
}

pub fn normalize_tag(t: &str) -> String {
    t.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '/' || *c == '.')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_normalised() {
        assert_eq!(normalize_tag("  HomeLab "), "homelab");
        assert_eq!(normalize_tag("two words"), "two-words");
        assert_eq!(normalize_tag("weird!!chars@@"), "weirdchars");
        assert_eq!(
            normalize_tag("keep/slash.dot_under-dash"),
            "keep/slash.dot_under-dash"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_kept_rather_than_saved_over() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("meta.json");
        let precious = br#"{"sessions":{"precious-1":{"favorite":true},}}"#;
        std::fs::write(&p, precious).unwrap();

        let mut m = Meta::load_at(&p);
        assert!(m.sessions.is_empty());
        // four edits: enough to rotate it out of every backup
        for id in ["a", "b", "c", "d"] {
            m.toggle_favorite(id);
            m.save_at(&p).unwrap();
        }
        let kept = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path()).is_ok_and(|b| b == precious));
        assert!(kept, "the unreadable original is gone");
    }

    #[test]
    fn two_browsers_saving_keep_each_others_changes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("meta.json");
        let mut first = Meta::default();
        first.add_tag("s0", "old");
        first.add_tag("s9", "gone-soon");
        first.save_at(&p).unwrap();

        let mut a = Meta::load_at(&p);
        let mut b = Meta::load_at(&p);
        a.add_tag("s1", "from-a");
        a.remove_tag("s9", "gone-soon");
        a.save_at(&p).unwrap();
        b.toggle_favorite("s2");
        b.save_at(&p).unwrap();

        let now = Meta::load_at(&p);
        assert_eq!(
            now.get("s1").map(|e| e.tags.clone()),
            Some(vec!["from-a".to_string()])
        );
        assert!(now.get("s2").is_some_and(|e| e.favorite), "b's star");
        assert!(now.get("s9").is_none(), "a's removal came back");
        assert_eq!(
            now.get("s0").map(|e| e.tags.clone()),
            Some(vec!["old".to_string()])
        );
        // and b now knows what a did
        assert!(b.get("s1").is_some());
    }

    #[test]
    fn tags_from_a_hand_edit_are_normalised_on_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("meta.json");
        std::fs::write(
            &p,
            r#"{"sessions":{"a":{"tags":["HomeLab","eft","  ","x\u001b]0;y\u0007z","homelab"]}}}"#,
        )
        .unwrap();
        let mut m = Meta::load_at(&p);
        assert_eq!(m.get("a").unwrap().tags, vec!["eft", "homelab", "x0yz"]);
        // and so it can be taken off again
        m.remove_tag("a", "HomeLab");
        assert_eq!(m.get("a").unwrap().tags, vec!["eft", "x0yz"]);
    }

    #[test]
    fn entries_disappear_when_they_hold_nothing() {
        let mut m = Meta::default();
        assert!(m.toggle_favorite("abc"));
        assert!(m.get("abc").is_some());
        assert!(!m.toggle_favorite("abc"));
        assert!(m.get("abc").is_none(), "an empty entry is not kept");
    }

    #[test]
    fn tags_add_dedupe_and_remove() {
        let mut m = Meta::default();
        m.add_tag("abc", "Homelab");
        m.add_tag("abc", "homelab");
        m.add_tag("abc", "eft");
        assert_eq!(
            m.get("abc").unwrap().tags,
            vec!["eft", "homelab"],
            "sorted, no dupes"
        );
        m.remove_tag("abc", "eft");
        assert_eq!(m.get("abc").unwrap().tags, vec!["homelab"]);
        m.remove_tag("abc", "homelab");
        assert!(m.get("abc").is_none());
    }

    #[test]
    fn rename_moves_a_tag_everywhere_and_merges() {
        let mut m = Meta::default();
        m.add_tag("a", "homelab");
        m.add_tag("b", "homelab");
        m.add_tag("b", "fleet");
        assert_eq!(m.rename_tag("homelab", "fleet"), 2);
        // b already had the destination, so it must not end up twice
        assert_eq!(m.get("b").unwrap().tags, vec!["fleet"]);
        assert_eq!(m.get("a").unwrap().tags, vec!["fleet"]);
        // renaming something absent, or onto itself, changes nothing
        assert_eq!(m.rename_tag("nope", "x"), 0);
        assert_eq!(m.rename_tag("fleet", "fleet"), 0);
        assert_eq!(m.rename_tag("fleet", ""), 0);
    }

    #[test]
    fn all_tags_counts_most_used_first() {
        let mut m = Meta::default();
        m.add_tag("a", "homelab");
        m.add_tag("b", "homelab");
        m.add_tag("c", "eft");
        let t = m.all_tags();
        assert_eq!(t[0], ("homelab".to_string(), 2));
        assert_eq!(t[1], ("eft".to_string(), 1));
    }

    #[test]
    fn survives_a_round_trip_through_json() {
        let mut m = Meta::default();
        m.add_tag("abc", "homelab");
        m.toggle_favorite("abc");
        m.set_note("abc", "  the important one  ");
        let encoded = serde_json::to_vec(&m).unwrap();
        let back: Meta = serde_json::from_slice(&encoded).unwrap();
        let e = back.get("abc").unwrap();
        assert!(e.favorite);
        assert_eq!(e.tags, vec!["homelab"]);
        assert_eq!(e.note, "the important one");
    }

    #[test]
    fn counts_describe_what_is_actually_attached() {
        // meta.json outlives the transcripts it refers to: retention
        // deletes them, and an entry for a session that no longer exists
        // still counted. --stats said "favourites 2" while the list showed
        // none, which is two parts of the tool disagreeing about the same
        // thing.
        let mut m = Meta::default();
        m.toggle_favorite("alive-1");
        m.toggle_favorite("gone-1");
        m.add_tag("alive-1", "here");
        m.add_tag("gone-2", "nowhere");

        let known: std::collections::HashSet<String> =
            ["alive-1".to_string()].into_iter().collect();
        let sum = m.summary(&known);
        assert_eq!(sum.favourites, 1, "counted a favourite with no session");
        assert_eq!(sum.tags, 1, "counted a tag on a session that is gone");
        assert_eq!(sum.orphans, 2, "gone-1 and gone-2");

        // and with everything present, nothing is orphaned
        let all: std::collections::HashSet<String> = ["alive-1", "gone-1", "gone-2"]
            .into_iter()
            .map(String::from)
            .collect();
        let sum = m.summary(&all);
        assert_eq!((sum.favourites, sum.tags, sum.orphans), (2, 2, 0));
    }

    #[test]
    fn a_note_cannot_carry_terminal_instructions() {
        // meta.json is hand-editable and synced between machines, and
        // every part of it is drawn to a terminal.
        let mut m = Meta::default();
        m.set_note("abc", "red \x1b[31m and a \x07 bell\nand a newline");
        let note = &m.get("abc").unwrap().note;
        assert!(!note.contains('\u{1b}'), "escape survived: {note:?}");
        assert!(!note.contains('\u{7}'), "bell survived: {note:?}");
        assert!(!note.contains('\n'), "newline survived: {note:?}");
        assert!(note.contains("red") && note.contains("bell"), "{note:?}");
    }

    #[test]
    fn a_tag_is_reduced_to_something_safe_to_draw() {
        assert_eq!(normalize_tag(" Eft Work "), "eft-work");
        assert_eq!(normalize_tag("esc\x1b[31m"), "esc31m");
        assert_eq!(normalize_tag("\x07"), "");
    }

    #[test]
    fn saving_keeps_previous_versions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("meta.json");
        std::fs::write(&p, b"first").unwrap();
        rotate_backups(&p);
        assert_eq!(std::fs::read(p.with_extension("json.1")).unwrap(), b"first");

        std::fs::write(&p, b"second").unwrap();
        rotate_backups(&p);
        assert_eq!(
            std::fs::read(p.with_extension("json.1")).unwrap(),
            b"second"
        );
        assert_eq!(std::fs::read(p.with_extension("json.2")).unwrap(), b"first");

        // an identical save must not push real history out of the window
        rotate_backups(&p);
        assert_eq!(std::fs::read(p.with_extension("json.2")).unwrap(), b"first");
    }

    #[test]
    fn a_corrupt_file_does_not_take_the_tool_down() {
        // meta.json is the only unreproducible file; a bad parse must degrade
        // to empty rather than panicking on startup.
        let m: Meta = serde_json::from_slice(b"{ this is not json").unwrap_or_default();
        assert!(m.sessions.is_empty());
    }
}

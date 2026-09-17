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
}

pub fn meta_path() -> PathBuf {
    crate::index::state_dir().join("meta.json")
}

impl Meta {
    pub fn load() -> Meta {
        match std::fs::read(meta_path()) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => Meta::default(),
        }
    }

    /// Write via temp file + rename so an interrupted save cannot truncate the
    /// one file in here that isn't reproducible.
    pub fn save(&self) -> Result<()> {
        let p = meta_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.sessions.get(id)
    }

    pub fn toggle_favorite(&mut self, id: &str) -> bool {
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
        let e = self.sessions.entry(id.to_string()).or_default();
        if !e.tags.iter().any(|t| t == &tag) {
            e.tags.push(tag);
            e.tags.sort();
        }
    }

    pub fn remove_tag(&mut self, id: &str, tag: &str) {
        let tag = normalize_tag(tag);
        if let Some(e) = self.sessions.get_mut(id) {
            e.tags.retain(|t| t != &tag);
        }
        self.gc(id);
    }

    pub fn set_note(&mut self, id: &str, note: &str) {
        let e = self.sessions.entry(id.to_string()).or_default();
        e.note = note.trim().to_string();
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

    pub fn favorite_count(&self) -> usize {
        self.sessions.values().filter(|e| e.favorite).count()
    }
}

pub fn normalize_tag(t: &str) -> String {
    t.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '/' || *c == '.')
        .collect()
}

//! SQLite-backed index cache.
//!
//! Scanning every transcript from cold costs a full pass over the corpus. The
//! cache keys each row on `(path, mtime, size)` and stores how far we read, so
//! an unchanged transcript costs one `stat()` and a transcript that merely grew
//! costs only its new tail.

use crate::model::Session;
use crate::scan;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude/mnemosyne")
}

pub fn db_path() -> PathBuf {
    state_dir().join("index.db")
}

/// Bump this whenever the scanner changes what it derives from a transcript.
///
/// The cache is keyed on `(path, mtime, size)`, so an unchanged file is never
/// re-read -- which means a logic change would otherwise keep serving values
/// produced by the old logic forever. This was not hypothetical: switching
/// `permission_mode` from last-seen to first-seen left one session still
/// reporting the old answer until the cache was deleted by hand.
///
/// History:
///   1  initial
///   2  permission_mode records the mode the session STARTED in
pub const SCANNER_VERSION: u32 = 2;

pub struct Index {
    conn: Connection,
}

impl Index {
    pub fn open() -> Result<Index> {
        std::fs::create_dir_all(state_dir())?;
        let conn = Connection::open(db_path())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                path            TEXT PRIMARY KEY,
                id              TEXT NOT NULL,
                project_dir     TEXT NOT NULL DEFAULT '',
                cwd             TEXT NOT NULL DEFAULT '',
                git_branch      TEXT NOT NULL DEFAULT '',
                ai_title        TEXT NOT NULL DEFAULT '',
                first_prompt    TEXT NOT NULL DEFAULT '',
                last_prompt     TEXT NOT NULL DEFAULT '',
                model           TEXT NOT NULL DEFAULT '',
                permission_mode TEXT NOT NULL DEFAULT '',
                version         TEXT NOT NULL DEFAULT '',
                size            INTEGER NOT NULL DEFAULT 0,
                mtime           INTEGER NOT NULL DEFAULT 0,
                first_ts        INTEGER NOT NULL DEFAULT 0,
                last_ts         INTEGER NOT NULL DEFAULT 0,
                entries         INTEGER NOT NULL DEFAULT 0,
                user_msgs       INTEGER NOT NULL DEFAULT 0,
                assistant_msgs  INTEGER NOT NULL DEFAULT 0,
                scanned_len     INTEGER NOT NULL DEFAULT 0,
                is_subagent     INTEGER NOT NULL DEFAULT 0,
                parent          TEXT,
                agent_id        TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_mtime  ON sessions(mtime DESC);
            CREATE INDEX IF NOT EXISTS idx_parent ON sessions(parent);
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            "#,
        )?;

        // Discard rows derived by an older scanner rather than trusting them.
        let stored: Option<u32> = conn
            .query_row("SELECT value FROM meta WHERE key='scanner_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| v.parse().ok());
        if stored != Some(SCANNER_VERSION) {
            conn.execute("DELETE FROM sessions", [])?;
            conn.execute(
                "INSERT INTO meta (key,value) VALUES ('scanner_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=?1",
                [SCANNER_VERSION.to_string()],
            )?;
        }
        Ok(Index { conn })
    }

    pub fn load(&self) -> Result<HashMap<String, Session>> {
        let mut st = self.conn.prepare(
            "SELECT path,id,project_dir,cwd,git_branch,ai_title,first_prompt,last_prompt,
                    model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                    user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id
             FROM sessions",
        )?;
        let rows = st.query_map([], |r| {
            let path: String = r.get(0)?;
            Ok(Session {
                path: PathBuf::from(&path),
                id: r.get(1)?,
                project_dir: r.get(2)?,
                cwd: r.get(3)?,
                git_branch: r.get(4)?,
                ai_title: r.get(5)?,
                first_prompt: r.get(6)?,
                last_prompt: r.get(7)?,
                model: r.get(8)?,
                permission_mode: r.get(9)?,
                version: r.get(10)?,
                size: r.get::<_, i64>(11)? as u64,
                mtime: r.get(12)?,
                first_ts: r.get(13)?,
                last_ts: r.get(14)?,
                entries: r.get::<_, i64>(15)? as u32,
                user_msgs: r.get::<_, i64>(16)? as u32,
                assistant_msgs: r.get::<_, i64>(17)? as u32,
                scanned_len: r.get::<_, i64>(18)? as u64,
                is_subagent: r.get::<_, i64>(19)? != 0,
                parent: r.get(20)?,
                agent_id: r.get(21)?,
                ..Default::default()
            })
        })?;
        let mut map = HashMap::new();
        for s in rows.flatten() {
            map.insert(s.path.to_string_lossy().to_string(), s);
        }
        Ok(map)
    }

    pub fn store(&mut self, sessions: &[Session]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO sessions (path,id,project_dir,cwd,git_branch,ai_title,first_prompt,
                    last_prompt,model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                    user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
                 ON CONFLICT(path) DO UPDATE SET
                    id=?2,project_dir=?3,cwd=?4,git_branch=?5,ai_title=?6,first_prompt=?7,
                    last_prompt=?8,model=?9,permission_mode=?10,version=?11,size=?12,mtime=?13,
                    first_ts=?14,last_ts=?15,entries=?16,user_msgs=?17,assistant_msgs=?18,
                    scanned_len=?19,is_subagent=?20,parent=?21,agent_id=?22",
            )?;
            for s in sessions {
                st.execute(params![
                    s.path.to_string_lossy(),
                    s.id,
                    s.project_dir,
                    s.cwd,
                    s.git_branch,
                    s.ai_title,
                    s.first_prompt,
                    s.last_prompt,
                    s.model,
                    s.permission_mode,
                    s.version,
                    s.size as i64,
                    s.mtime,
                    s.first_ts,
                    s.last_ts,
                    s.entries as i64,
                    s.user_msgs as i64,
                    s.assistant_msgs as i64,
                    s.scanned_len as i64,
                    s.is_subagent as i64,
                    s.parent,
                    s.agent_id,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop rows whose transcript no longer exists (Claude prunes old ones).
    pub fn prune(&mut self, live_paths: &[String]) -> Result<usize> {
        let existing: Vec<String> = {
            let mut st = self.conn.prepare("SELECT path FROM sessions")?;
            let v = st
                .query_map([], |r| r.get::<_, String>(0))?
                .flatten()
                .collect();
            v
        };
        let set: std::collections::HashSet<&str> = live_paths.iter().map(|s| s.as_str()).collect();
        let gone: Vec<&String> = existing.iter().filter(|p| !set.contains(p.as_str())).collect();
        let n = gone.len();
        if n > 0 {
            let tx = self.conn.transaction()?;
            {
                let mut st = tx.prepare("DELETE FROM sessions WHERE path=?1")?;
                for p in gone {
                    st.execute([p])?;
                }
            }
            tx.commit()?;
        }
        Ok(n)
    }
}

/// Live counters so a caller can render a real progress bar while we work.
#[derive(Clone, Default)]
pub struct Progress {
    pub total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub done: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Full refresh: discover transcripts, scan in parallel reusing cached rows,
/// persist, and return everything sorted newest-first.
pub fn refresh(include_subagents: bool) -> Result<Vec<Session>> {
    refresh_with_progress(include_subagents, None)
}

pub fn refresh_with_progress(
    include_subagents: bool,
    progress: Option<Progress>,
) -> Result<Vec<Session>> {
    use std::sync::atomic::Ordering;

    let mut idx = Index::open()?;
    let cached = idx.load()?;
    let found = scan::discover(include_subagents);
    if let Some(p) = &progress {
        p.total.store(found.len(), Ordering::Relaxed);
    }

    let sessions: Vec<Session> = found
        .par_iter()
        .filter_map(|(path, is_sub, parent)| {
            let key = path.to_string_lossy().to_string();
            let prev = cached.get(&key);
            let out = scan::scan(path, *is_sub, parent.clone(), prev).ok();
            if let Some(p) = &progress {
                p.done.fetch_add(1, Ordering::Relaxed);
                if let Some(s) = &out {
                    p.bytes.fetch_add(s.size, Ordering::Relaxed);
                }
            }
            out
        })
        .collect();

    idx.store(&sessions)?;
    let paths: Vec<String> = found
        .iter()
        .map(|(p, _, _)| p.to_string_lossy().to_string())
        .collect();
    let _ = idx.prune(&paths);
    if let Some(p) = &progress {
        p.finished.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    Ok(sessions)
}

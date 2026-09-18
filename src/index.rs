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
///   3  conversation prose is harvested into the full-text index
///   4  thinking blocks and shell commands are harvested too
///   5  token usage is summed per session
pub const SCANNER_VERSION: u32 = 5;

/// Every column the loader expects. Compared against what the database
/// actually has, so drift is detected rather than assumed away.
const EXPECTED_COLUMNS: &[&str] = &[
    "path",
    "id",
    "project_dir",
    "cwd",
    "git_branch",
    "ai_title",
    "first_prompt",
    "last_prompt",
    "model",
    "permission_mode",
    "version",
    "size",
    "mtime",
    "first_ts",
    "last_ts",
    "entries",
    "user_msgs",
    "assistant_msgs",
    "in_tokens",
    "out_tokens",
    "cache_read",
    "cache_write",
    "scanned_len",
    "is_subagent",
    "parent",
    "agent_id",
];

fn schema_current(conn: &Connection) -> bool {
    let Ok(mut st) = conn.prepare("PRAGMA table_info(sessions)") else {
        return false;
    };
    let Ok(rows) = st.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    let have: std::collections::HashSet<String> = rows.flatten().collect();
    if have.is_empty() {
        return false; // no table yet
    }
    EXPECTED_COLUMNS.iter().all(|c| have.contains(*c))
}

pub struct Index {
    conn: Connection,
    /// True when we could not use the on-disk cache and fell back to memory.
    /// Everything still works; it is just rebuilt on every run.
    pub ephemeral: bool,
}

impl Index {
    /// Open the cache, and never let a bad one stop the tool starting.
    ///
    /// The index is derived data that can always be rebuilt from the
    /// transcripts, so a corrupt file is deleted and recreated rather than
    /// reported. If the location cannot be written to at all -- a read-only
    /// home, a full disk -- we fall back to an in-memory index, which is
    /// slower but entirely usable.
    pub fn open() -> Result<Index> {
        let _ = std::fs::create_dir_all(state_dir());
        let path = db_path();
        match Index::open_at(&path) {
            Ok(i) => Ok(i),
            Err(first) => {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(path.with_extension("db-wal"));
                let _ = std::fs::remove_file(path.with_extension("db-shm"));
                match Index::open_at(&path) {
                    Ok(i) => Ok(i),
                    Err(second) => {
                        eprintln!(
                            "mnemosyne: cache unusable ({first}; after reset: {second}) — running without it"
                        );
                        Index::open_memory()
                    }
                }
            }
        }
    }

    /// An index that lives only for this run.
    pub fn open_memory() -> Result<Index> {
        let mut i = Index::open_conn(Connection::open_in_memory()?)?;
        i.ephemeral = true;
        Ok(i)
    }

    /// Open a specific database file. Split out from `open` so tests can use a
    /// temporary path instead of racing each other over `$HOME`.
    pub fn open_at(path: &std::path::Path) -> Result<Index> {
        Index::open_conn(Connection::open(path)?)
    }

    fn open_conn(conn: Connection) -> Result<Index> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;

        // Read the version before creating anything. Everything else here is
        // derived from the transcripts, so a mismatch is dropped rather than
        // migrated -- and it has to be a DROP, because `CREATE TABLE IF NOT
        // EXISTS` will not add a new column to a table that already exists,
        // which fails every later query with "no such column".
        let stored: Option<u32> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='scanner_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok());

        // Trusting the version number alone is not enough: an interrupted run
        // can record a new version against a table that was never recreated,
        // and then every query fails with "no such column" and the recorded
        // version says nothing is wrong. Check the columns that are actually
        // there.
        let mut reset = stored != Some(SCANNER_VERSION) || !schema_current(&conn);
        if reset {
            conn.execute_batch("DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS body;")?;
        }

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
                in_tokens       INTEGER NOT NULL DEFAULT 0,
                out_tokens      INTEGER NOT NULL DEFAULT 0,
                cache_read      INTEGER NOT NULL DEFAULT 0,
                cache_write     INTEGER NOT NULL DEFAULT 0,
                scanned_len     INTEGER NOT NULL DEFAULT 0,
                is_subagent     INTEGER NOT NULL DEFAULT 0,
                parent          TEXT,
                agent_id        TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_mtime  ON sessions(mtime DESC);
            CREATE INDEX IF NOT EXISTS idx_parent ON sessions(parent);
            -- Full text of what was actually said. Small, because prose is
            -- about two per cent of a transcript; searching it beats
            -- re-reading gigabytes of tool output on every query.
            CREATE VIRTUAL TABLE IF NOT EXISTS body USING fts5(
                path UNINDEXED, text, tokenize = 'unicode61'
            );
            "#,
        )?;

        // Only claim the version once the tables really match it.
        if !schema_current(&conn) {
            conn.execute_batch("DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS body;")?;
            reset = true;
        }
        if reset {
            conn.execute(
                "INSERT INTO meta (key,value) VALUES ('scanner_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=?1",
                [SCANNER_VERSION.to_string()],
            )?;
        }
        Ok(Index {
            conn,
            ephemeral: false,
        })
    }

    pub fn load(&self) -> Result<HashMap<String, Session>> {
        let mut st = self.conn.prepare(
            "SELECT path,id,project_dir,cwd,git_branch,ai_title,first_prompt,last_prompt,
                    model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                    user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id,
                    in_tokens,out_tokens,cache_read,cache_write
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
                in_tokens: r.get::<_, i64>(22)? as u64,
                out_tokens: r.get::<_, i64>(23)? as u64,
                cache_read: r.get::<_, i64>(24)? as u64,
                cache_write: r.get::<_, i64>(25)? as u64,
                ..Default::default()
            })
        })?;
        let mut map = HashMap::new();
        for s in rows.flatten() {
            map.insert(s.path.to_string_lossy().to_string(), s);
        }
        Ok(map)
    }

    /// Replace the indexed text for a transcript.
    pub fn store_text(&mut self, rows: &[(String, String)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM body WHERE path = ?1")?;
            let mut ins = tx.prepare("INSERT INTO body (path, text) VALUES (?1, ?2)")?;
            for (path, text) in rows {
                del.execute([path])?;
                if !text.is_empty() {
                    ins.execute(params![path, text])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Paths whose conversation matches, with a readable excerpt.
    ///
    /// The caller passes an already-escaped FTS5 expression.
    /// Deliberately does not build excerpts. `snippet()` has to re-locate the
    /// match inside every hit, which for a common word over a thousand
    /// documents cost seconds — slower than the brute scan it replaced. The
    /// excerpt for the row you are actually looking at is fetched on demand.
    pub fn search_text(&self, expr: &str) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT path FROM body WHERE body MATCH ?1")?;
        let rows = st.query_map([expr], |r| r.get::<_, String>(0))?;
        Ok(rows.flatten().collect())
    }

    /// A readable excerpt for one hit.
    pub fn snippet_for(&self, path: &str, expr: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT snippet(body, 1, '', '', '…', 16) FROM body
                 WHERE path = ?1 AND body MATCH ?2",
                params![path, expr],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    pub fn existing_text(&self, path: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT text FROM body WHERE path = ?1")?;
        let mut rows = st.query([path])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    pub fn text_rows(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM body", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize)
    }

    pub fn store(&mut self, sessions: &[Session]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO sessions (path,id,project_dir,cwd,git_branch,ai_title,first_prompt,
                    last_prompt,model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                    user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id,
                    in_tokens,out_tokens,cache_read,cache_write)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,
                         ?21,?22,?23,?24,?25,?26)
                 ON CONFLICT(path) DO UPDATE SET
                    id=?2,project_dir=?3,cwd=?4,git_branch=?5,ai_title=?6,first_prompt=?7,
                    last_prompt=?8,model=?9,permission_mode=?10,version=?11,size=?12,mtime=?13,
                    first_ts=?14,last_ts=?15,entries=?16,user_msgs=?17,assistant_msgs=?18,
                    scanned_len=?19,is_subagent=?20,parent=?21,agent_id=?22,
                    in_tokens=?23,out_tokens=?24,cache_read=?25,cache_write=?26",
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
                    s.in_tokens as i64,
                    s.out_tokens as i64,
                    s.cache_read as i64,
                    s.cache_write as i64,
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
        let gone: Vec<&String> = existing
            .iter()
            .filter(|p| !set.contains(p.as_str()))
            .collect();
        let n = gone.len();
        if n > 0 {
            let tx = self.conn.transaction()?;
            {
                let mut st = tx.prepare("DELETE FROM sessions WHERE path=?1")?;
                // The indexed text has to go with it. Missing this leaked a
                // row per deleted transcript, and since nothing ages out of
                // the index on its own it only ever grew.
                let mut sb = tx.prepare("DELETE FROM body WHERE path=?1")?;
                for p in gone {
                    st.execute([p])?;
                    sb.execute([p])?;
                }
            }
            tx.commit()?;

            // SQLite keeps freed pages unless told otherwise, so a deletion
            // on its own reclaims nothing. Only worth doing when something
            // actually went.
            let _ = self
                .conn
                .execute("INSERT INTO body(body) VALUES('optimize')", []);
            let _ = self.conn.execute_batch("VACUUM");
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

    let scanned: Vec<(Session, Option<String>)> = found
        .par_iter()
        .filter_map(|(path, is_sub, parent)| {
            let key = path.to_string_lossy().to_string();
            let prev = cached.get(&key);
            // Only harvest text when the file actually needs reading; an
            // untouched transcript keeps whatever is already indexed.
            let unchanged = prev.is_some_and(|p| {
                std::fs::metadata(path)
                    .map(|m| {
                        p.size == m.len()
                            && p.mtime
                                == m.modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0)
                    })
                    .unwrap_or(false)
            });
            let out = if unchanged {
                scan::scan(path, *is_sub, parent.clone(), prev)
                    .ok()
                    .map(|s| (s, None))
            } else {
                let mut text = String::new();
                scan::scan_with_text(path, *is_sub, parent.clone(), prev, &mut text)
                    .ok()
                    .map(|s| (s, Some(text)))
            };
            if let Some(p) = &progress {
                p.done.fetch_add(1, Ordering::Relaxed);
                if let Some((s, _)) = &out {
                    p.bytes.fetch_add(s.size, Ordering::Relaxed);
                }
            }
            out
        })
        .collect();

    // A transcript that only grew contributes its new tail; one read from
    // scratch replaces its row outright.
    let mut text_rows: Vec<(String, String)> = Vec::new();
    for (s, t) in &scanned {
        if let Some(t) = t {
            let key = s.path.to_string_lossy().to_string();
            let merged = match cached.get(&key) {
                Some(p) if p.scanned_len > 0 && p.scanned_len < s.scanned_len => {
                    match idx.existing_text(&key) {
                        Ok(Some(old)) => format!("{old} {t}"),
                        _ => t.clone(),
                    }
                }
                _ => t.clone(),
            };
            text_rows.push((key, merged));
        }
    }
    let sessions: Vec<Session> = scanned.into_iter().map(|(s, _)| s).collect();
    let _ = idx.store_text(&text_rows);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;

    fn sample(path: &str, title: &str) -> Session {
        Session {
            id: "11112222-3333-4444-5555-666677778888".into(),
            path: PathBuf::from(path),
            ai_title: title.into(),
            cwd: "/home/u/proj".into(),
            permission_mode: "default".into(),
            size: 1234,
            mtime: 99,
            entries: 7,
            scanned_len: 1234,
            ..Default::default()
        }
    }

    #[test]
    fn rows_survive_a_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        let mut idx = Index::open_at(&db).unwrap();
        idx.store(&[sample("/a.jsonl", "hello")]).unwrap();

        let loaded = Index::open_at(&db).unwrap().load().unwrap();
        let got = loaded.get("/a.jsonl").expect("row is there");
        assert_eq!(got.ai_title, "hello");
        assert_eq!(got.permission_mode, "default");
        assert_eq!(got.scanned_len, 1234);
    }

    #[test]
    fn storing_the_same_path_updates_rather_than_duplicates() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        let mut idx = Index::open_at(&db).unwrap();
        idx.store(&[sample("/a.jsonl", "first")]).unwrap();
        idx.store(&[sample("/a.jsonl", "second")]).unwrap();
        let loaded = idx.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["/a.jsonl"].ai_title, "second");
    }

    #[test]
    fn prune_drops_rows_whose_transcript_is_gone() {
        // Claude Code deletes transcripts past cleanupPeriodDays; the cache
        // must not keep showing them.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        let mut idx = Index::open_at(&db).unwrap();
        idx.store(&[sample("/a.jsonl", "keep"), sample("/b.jsonl", "gone")])
            .unwrap();
        let removed = idx.prune(&["/a.jsonl".to_string()]).unwrap();
        assert_eq!(removed, 1);
        let loaded = idx.load().unwrap();
        assert!(loaded.contains_key("/a.jsonl"));
        assert!(!loaded.contains_key("/b.jsonl"));
    }

    #[test]
    fn prune_also_drops_the_indexed_text() {
        // Cleaning only the sessions table leaked a body row per deleted
        // transcript, and nothing ever aged them out.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        let mut idx = Index::open_at(&db).unwrap();
        idx.store(&[sample("/a.jsonl", "keep"), sample("/b.jsonl", "gone")])
            .unwrap();
        idx.store_text(&[
            ("/a.jsonl".into(), "alpha text".into()),
            ("/b.jsonl".into(), "beta text".into()),
        ])
        .unwrap();
        assert_eq!(idx.text_rows().unwrap(), 2);

        idx.prune(&["/a.jsonl".to_string()]).unwrap();
        assert_eq!(
            idx.text_rows().unwrap(),
            1,
            "the dead transcript's text went too"
        );
        assert!(idx.search_text("\"beta\"*").unwrap().is_empty());
        assert_eq!(idx.search_text("\"alpha\"*").unwrap().len(), 1);
    }

    #[test]
    fn a_scanner_change_invalidates_the_cache() {
        // The cache keys on (path, mtime, size), so an unchanged transcript is
        // never re-read. Without this, a change to what the scanner derives
        // would keep serving values produced by the old logic forever -- which
        // is exactly what happened when permission_mode moved from last-seen
        // to first-seen.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        {
            let mut idx = Index::open_at(&db).unwrap();
            idx.store(&[sample("/a.jsonl", "derived by the old scanner")])
                .unwrap();
        }
        // pretend the row was written by an earlier version
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE meta SET value='0' WHERE key='scanner_version'", [])
                .unwrap();
        }
        let idx = Index::open_at(&db).unwrap();
        assert!(
            idx.load().unwrap().is_empty(),
            "stale rows are dropped so they get rescanned"
        );
    }

    #[test]
    fn a_matching_version_keeps_the_cache() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        {
            let mut idx = Index::open_at(&db).unwrap();
            idx.store(&[sample("/a.jsonl", "still good")]).unwrap();
        }
        let idx = Index::open_at(&db).unwrap();
        assert_eq!(idx.load().unwrap().len(), 1, "no needless rescan");
    }
}

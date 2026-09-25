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
///   6  prose stored as a plain `"content"` string is harvested too
///   7  rebuild text that an incremental rescan had doubled or deleted
///   8  a response's usage is counted once, not once per content block
///   9  the title given with /rename is read
///   10 a tool's result is left out of the text, list-shaped ones too
pub const SCANNER_VERSION: u32 = 10;

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
    "last_msg_id",
    "custom_title",
];

/// Columns added after the table was first made, and how to add them.
///
/// A table missing one gets it added rather than being dropped. Dropping it
/// would be correct -- everything here can be rebuilt -- but it leaves the
/// first run after an update with nothing to show while it re-reads every
/// transcript, where the rows it has are still worth showing in the
/// meantime. Whatever made the column necessary bumps `SCANNER_VERSION`
/// too, so the rows are rescanned underneath and the default never lasts.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("last_msg_id", "TEXT NOT NULL DEFAULT ''"),
    ("custom_title", "TEXT NOT NULL DEFAULT ''"),
];

fn add_missing_columns(conn: &Connection) {
    let Ok(mut st) = conn.prepare("PRAGMA table_info(sessions)") else {
        return;
    };
    let have: std::collections::HashSet<String> = match st.query_map([], |r| r.get(1)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => return,
    };
    if have.is_empty() {
        return; // no table yet: it is created whole
    }
    for (col, ddl) in ADDED_COLUMNS {
        if !have.contains(*col) {
            let _ = conn.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {col} {ddl}"));
        }
    }
}

/// FTS5 columns are not indexed for equality, so `WHERE path = ?` scans the
/// whole table. Deleting a row that way cost 41 of the 43 seconds a full
/// re-index took -- 1100 deletes, each reading everything. This maps a path
/// to its rowid, which FTS5 *can* delete by directly.
const BODY_REF: &str = "CREATE TABLE IF NOT EXISTS body_ref (
    path TEXT PRIMARY KEY,
    rid  INTEGER NOT NULL
)";

/// Whether opening the cache failed because the file is not a sound
/// database -- the one failure that deleting it fixes.
fn is_corrupt(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<rusqlite::Error>()
                .and_then(|e| e.sqlite_error_code()),
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
        )
    })
}

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
    if !EXPECTED_COLUMNS.iter().all(|c| have.contains(*c)) {
        return false;
    }
    // The rowid map has to exist, and has to describe the rows that are
    // there: half a map would delete the wrong text or none at all.
    let refs: i64 = conn
        .query_row("SELECT count(*) FROM body_ref", [], |r| r.get(0))
        .unwrap_or(-1);
    let bodies: i64 = conn
        .query_row("SELECT count(*) FROM body", [], |r| r.get(0))
        .unwrap_or(-1);
    refs >= 0 && bodies >= 0 && refs == bodies
}

pub struct Index {
    conn: Connection,
    /// True when we could not use the on-disk cache and fell back to memory.
    /// Everything still works; it is just rebuilt on every run.
    pub ephemeral: bool,
    /// The rows were written by an older scanner. They are still worth
    /// showing -- a title from yesterday's logic is a title -- but they must
    /// not be reused to skip reading a file, or the new logic never runs.
    pub stale: bool,
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
            // Only a file that is itself bad is deleted. Anything else --
            // above all another instance holding the write lock past the
            // busy timeout -- says nothing against the file, and deleting it
            // from under that instance threw away everything it went on to
            // write, while its commits kept reporting success.
            Err(first) if !is_corrupt(&first) => {
                eprintln!("mnemosyne: cache unusable ({first}) — running without it");
                Index::open_memory()
            }
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
        // A changed schema is fatal to the old rows: a query would fail with
        // "no such column". A changed *scanner* is not -- the columns are
        // fine, the values are merely out of date. Throwing them away meant
        // the first run after an update had nothing to show and sat on a
        // splash for twelve seconds re-reading 2.5GB. Keep them, show them,
        // and let the rescan replace them underneath.
        add_missing_columns(&conn);
        let schema_ok = schema_current(&conn);
        let stale = stored != Some(SCANNER_VERSION);
        let mut reset = !schema_ok;
        if reset {
            conn.execute_batch(
            "DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS body; DROP TABLE IF EXISTS body_ref;",
        )?;
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
                agent_id        TEXT NOT NULL DEFAULT '',
                last_msg_id     TEXT NOT NULL DEFAULT '',
                custom_title    TEXT NOT NULL DEFAULT ''
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
        conn.execute_batch(BODY_REF)?;

        // Only claim the version once the tables really match it.
        if !schema_current(&conn) {
            conn.execute_batch(
                "DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS body;
                 DROP TABLE IF EXISTS body_ref;",
            )?;
            reset = true;
        }
        // Only a wiped cache can claim the new version here; there is
        // nothing left that the old scanner wrote. A merely stale cache
        // claims it after the rescan, in `mark_current` -- recording it now
        // would tell the very next open that everything is up to date and
        // the new logic would never run.
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
            stale: stale || reset,
        })
    }

    pub fn load(&self) -> Result<HashMap<String, Session>> {
        let mut st = self.conn.prepare(
            "SELECT path,id,project_dir,cwd,git_branch,ai_title,first_prompt,last_prompt,
                    model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                    user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id,
                    in_tokens,out_tokens,cache_read,cache_write,last_msg_id,custom_title
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
                last_msg_id: r.get(26)?,
                custom_title: r.get(27)?,
                ..Default::default()
            })
        })?;
        let mut map = HashMap::new();
        for s in rows.flatten() {
            map.insert(s.path.to_string_lossy().to_string(), s);
        }
        Ok(map)
    }

    /// Start a write, holding the write lock from the outset.
    ///
    /// A deferred transaction reads first and asks for the lock when it
    /// comes to write. If another instance is writing then, SQLite cannot
    /// wait for it -- this one's reads are already out of date -- and fails
    /// at once with "database is locked", however long the busy timeout.
    /// Taken up front, the lock is simply waited for.
    fn begin(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
    }

    /// Replace the indexed text for a transcript.
    #[cfg(test)]
    pub fn store_text(&mut self, rows: &[(String, String)]) -> Result<()> {
        let tx = self.begin()?;
        write_text(&tx, rows)?;
        tx.commit()?;
        Ok(())
    }

    /// Everything a refresh read, in one go: the sessions and their text
    /// land together or not at all.
    ///
    /// Apart, a write of the text that failed was ignored while the
    /// sessions after it were stored. Their `scanned_len` said the new text
    /// had been read, so it was never read again, and never searchable.
    pub fn persist(&mut self, sessions: &[Session], text: &[(String, String)]) -> Result<()> {
        let tx = self.begin()?;
        write_text(&tx, text)?;
        write_sessions(&tx, sessions)?;
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

    /// How many paths the rowid map knows about. Should always equal the
    /// number of text rows.
    #[cfg(test)]
    pub fn rowid_map_len(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM body_ref", [], |r| r.get(0))?)
    }

    /// Record that the rows now match this scanner. Called once the rescan
    /// that makes it true has finished.
    pub fn mark_current(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key,value) VALUES ('scanner_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=?1",
            [SCANNER_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// A readable excerpt for one hit.
    /// Excerpts for every hit, cut from the stored prose.
    ///
    /// Built one row at a time and never held as whole documents, so the
    /// memory stays flat whether the query matches five sessions or every
    /// one of them.
    pub fn excerpts(&self, expr: &str, needle: &str) -> Result<HashMap<String, String>> {
        let mut st = self
            .conn
            .prepare("SELECT path, text FROM body WHERE body MATCH ?1")?;
        let mut out = HashMap::new();
        let mut rows = st.query([expr])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let text: String = r.get(1)?;
            out.insert(path, crate::search::excerpt(&text, needle));
        }
        Ok(out)
    }

    /// One row's excerpt, for the row you are actually looking at.
    ///
    /// Reads that row's prose and cuts the window out of it. FTS5's own
    /// `snippet()` re-runs the match to build one, which on a large
    /// transcript is most of a tenth of a second -- per row, as the cursor
    /// moves.
    pub fn excerpt_for(&self, path: &str, needle: &str) -> Option<String> {
        let text: String = self
            .conn
            .query_row(
                "SELECT b.text FROM body b JOIN body_ref r ON b.rowid = r.rid WHERE r.path = ?1",
                params![path],
                |r| r.get(0),
            )
            .ok()?;
        Some(crate::search::excerpt(&text, needle))
    }

    pub fn existing_text(&self, path: &str) -> Result<Option<String>> {
        // Through the rowid map, for the same reason the deletes are:
        // `path` is UNINDEXED, so matching on it reads the whole table.
        let mut st = self.conn.prepare(
            "SELECT b.text FROM body b JOIN body_ref r ON b.rowid = r.rid WHERE r.path = ?1",
        )?;
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

    #[cfg(test)]
    pub fn store(&mut self, sessions: &[Session]) -> Result<()> {
        let tx = self.begin()?;
        write_sessions(&tx, sessions)?;
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
            let tx = self.begin()?;
            {
                let mut st = tx.prepare("DELETE FROM sessions WHERE path=?1")?;
                // The indexed text has to go with it. Missing this leaked a
                // row per deleted transcript, and since nothing ages out of
                // the index on its own it only ever grew.
                let mut sfind = tx.prepare("SELECT rid FROM body_ref WHERE path=?1")?;
                let mut sb = tx.prepare("DELETE FROM body WHERE rowid=?1")?;
                let mut sunref = tx.prepare("DELETE FROM body_ref WHERE path=?1")?;
                for p in gone {
                    st.execute([p])?;
                    if let Ok(rid) = sfind.query_row([p], |r| r.get::<_, i64>(0)) {
                        sb.execute([rid])?;
                    }
                    sunref.execute([p])?;
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

fn write_text(tx: &rusqlite::Transaction, rows: &[(String, String)]) -> Result<()> {
    // Delete by rowid, never by path: `path` is UNINDEXED, so
    // `WHERE path = ?` reads the entire table. Doing that once per
    // transcript was 41 of the 43 seconds a full re-index took.
    let mut find = tx.prepare("SELECT rid FROM body_ref WHERE path = ?1")?;
    let mut del = tx.prepare("DELETE FROM body WHERE rowid = ?1")?;
    let mut unref = tx.prepare("DELETE FROM body_ref WHERE path = ?1")?;
    let mut ins = tx.prepare("INSERT INTO body (path, text) VALUES (?1, ?2)")?;
    let mut reref = tx.prepare("INSERT OR REPLACE INTO body_ref (path, rid) VALUES (?1, ?2)")?;
    for (path, text) in rows {
        if let Ok(rid) = find.query_row([path], |r| r.get::<_, i64>(0)) {
            del.execute([rid])?;
        }
        unref.execute([path])?;
        if !text.is_empty() {
            ins.execute(params![path, text])?;
            reref.execute(params![path, tx.last_insert_rowid()])?;
        }
    }
    Ok(())
}

fn write_sessions(tx: &rusqlite::Transaction, sessions: &[Session]) -> Result<()> {
    let mut st = tx.prepare(
        "INSERT INTO sessions (path,id,project_dir,cwd,git_branch,ai_title,first_prompt,
                last_prompt,model,permission_mode,version,size,mtime,first_ts,last_ts,entries,
                user_msgs,assistant_msgs,scanned_len,is_subagent,parent,agent_id,
                in_tokens,out_tokens,cache_read,cache_write,last_msg_id,custom_title)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,
                     ?21,?22,?23,?24,?25,?26,?27,?28)
             ON CONFLICT(path) DO UPDATE SET
                id=?2,project_dir=?3,cwd=?4,git_branch=?5,ai_title=?6,first_prompt=?7,
                last_prompt=?8,model=?9,permission_mode=?10,version=?11,size=?12,mtime=?13,
                first_ts=?14,last_ts=?15,entries=?16,user_msgs=?17,assistant_msgs=?18,
                scanned_len=?19,is_subagent=?20,parent=?21,agent_id=?22,
                in_tokens=?23,out_tokens=?24,cache_read=?25,cache_write=?26,
                last_msg_id=?27,custom_title=?28",
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
            s.last_msg_id,
            s.custom_title,
        ])?;
    }
    Ok(())
}

/// Live counters so a caller can render a real progress bar while we work.
#[derive(Clone, Default)]
pub struct Progress {
    pub total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub done: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What a transcript's indexed text should be once it has been scanned, or
/// `None` to leave what is stored alone.
///
/// A transcript read from scratch replaces its row outright. One read on from
/// `prev` contributes only its new tail, which is joined to what is there --
/// and when that tail is empty, as it is for a file that was only touched or
/// is halfway through writing a line, what is there stays.
///
/// This used to decide by comparing against the cached row rather than the
/// `prev` the scan was actually given. After a scanner change there is no
/// `prev` and every file is read from the start, so each one that had grown
/// was stored as its old text followed by all of it again.
fn text_after_scan(
    idx: &Index,
    prev: Option<&Session>,
    s: &Session,
    fresh: &str,
) -> Option<String> {
    if !prev.is_some_and(|p| scan::resumes(p, s.size)) {
        return Some(fresh.to_string());
    }
    if fresh.is_empty() {
        return None;
    }
    let key = s.path.to_string_lossy().to_string();
    Some(match idx.existing_text(&key) {
        Ok(Some(old)) => format!("{old} {fresh}"),
        _ => fresh.to_string(),
    })
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
    // However this ends, the splash waiting on it has to hear that it did.
    // An error on the way used to leave `finished` unset, and a cold start
    // sat out its whole time limit before showing an empty list.
    struct Finish(Option<Progress>);
    impl Drop for Finish {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                p.finished.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let _finish = Finish(progress.clone());
    let idx = Index::open()?;
    refresh_in(idx, scan::discover(include_subagents), progress.as_ref())
}

/// A refresh against a given index and set of transcripts.
fn refresh_in(
    mut idx: Index,
    found: Vec<(PathBuf, bool, Option<String>)>,
    progress: Option<&Progress>,
) -> Result<Vec<Session>> {
    use std::sync::atomic::Ordering;

    let stale = idx.stale;
    let cached = idx.load()?;
    if let Some(p) = &progress {
        p.total.store(found.len(), Ordering::Relaxed);
    }

    let scanned: Vec<(Session, String)> = found
        .par_iter()
        .filter_map(|(path, is_sub, parent)| {
            let key = path.to_string_lossy().to_string();
            // Rows from an older scanner are shown, never reused: skipping a
            // file because its size and mtime match would leave yesterday's
            // logic in place forever.
            let prev = if stale { None } else { cached.get(&key) };
            // Always with the text. An untouched transcript is not read at
            // all, so this costs nothing there and leaves its text alone.
            // Deciding "untouched" here instead, from a stat of our own,
            // raced the scan's: a file that grew in between was read on
            // past its new lines without keeping their text, and they were
            // never searchable.
            let mut text = String::new();
            let out = scan::scan_with_text(path, *is_sub, parent.clone(), prev, &mut text)
                .ok()
                .map(|s| (s, text));
            if let Some(p) = &progress {
                p.done.fetch_add(1, Ordering::Relaxed);
                if let Some((s, _)) = &out {
                    p.bytes.fetch_add(s.size, Ordering::Relaxed);
                }
            }
            out
        })
        .collect();

    let mut text_rows: Vec<(String, String)> = Vec::new();
    for (s, t) in &scanned {
        let key = s.path.to_string_lossy().to_string();
        let prev = if stale { None } else { cached.get(&key) };
        if let Some(text) = text_after_scan(&idx, prev, s, t) {
            text_rows.push((key, text));
        }
    }
    let sessions: Vec<Session> = scanned.into_iter().map(|(s, _)| s).collect();

    // Keeping what was read is for next time; this run has it either way.
    // A cache that cannot be written -- left owned by root after a `sudo
    // mn`, or on a full disk -- used to fail the whole refresh, and with it
    // every command-line mode and `R` in the browser, while promising that
    // a bad cache never stops the tool.
    if idx.persist(&sessions, &text_rows).is_ok() {
        let paths: Vec<String> = found
            .iter()
            .map(|(p, _, _)| p.to_string_lossy().to_string())
            .collect();
        let _ = idx.prune(&paths);
        // Everything has been re-read with the current scanner and stored,
        // so the rows may now claim its version. Not before it is stored:
        // claimed over rows the old scanner wrote, the new one never runs.
        let _ = idx.mark_current();
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
            last_msg_id: "msg_1".into(),
            custom_title: "named".into(),
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
        assert_eq!(got.last_msg_id, "msg_1");
        assert_eq!(got.custom_title, "named");
    }

    #[test]
    fn a_table_from_before_a_column_was_added_keeps_its_rows() {
        // Added in place, not dropped: the rows are still worth showing
        // while the rescan the scanner bump forces replaces them.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        {
            let mut idx = Index::open_at(&db).unwrap();
            idx.store(&[sample("/a.jsonl", "hello")]).unwrap();
            idx.conn
                .execute_batch(
                    "ALTER TABLE sessions DROP COLUMN last_msg_id;
                     ALTER TABLE sessions DROP COLUMN custom_title;
                     UPDATE meta SET value='7' WHERE key='scanner_version';",
                )
                .unwrap();
        }
        let idx = Index::open_at(&db).unwrap();
        assert!(idx.stale, "the rows are an older scanner's");
        let loaded = idx.load().unwrap();
        assert_eq!(
            loaded["/a.jsonl"].ai_title, "hello",
            "the rows were dropped"
        );
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
    fn re_storing_text_replaces_it_rather_than_piling_up() {
        // Replacing a transcript's text used to be a DELETE matched on an
        // UNINDEXED column, which reads the whole table: 41 of the 43
        // seconds a full re-index took. It goes through a rowid map now, so
        // the map has to stay in step or the old text is never removed.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        let mut idx = Index::open_at(&db).unwrap();
        idx.store(&[sample("/a.jsonl", "one")]).unwrap();

        idx.store_text(&[("/a.jsonl".into(), "the first text".into())])
            .unwrap();
        idx.store_text(&[("/a.jsonl".into(), "the second text".into())])
            .unwrap();

        assert_eq!(idx.text_rows().unwrap(), 1, "the old row was left behind");
        assert!(
            idx.search_text("\"first\"*").unwrap().is_empty(),
            "replaced text is still findable"
        );
        assert_eq!(idx.search_text("\"second\"*").unwrap().len(), 1);
        assert_eq!(idx.rowid_map_len().unwrap(), 1, "the map drifted");

        // and emptying it removes both sides
        idx.store_text(&[("/a.jsonl".into(), String::new())])
            .unwrap();
        assert_eq!(idx.text_rows().unwrap(), 0);
        assert_eq!(idx.rowid_map_len().unwrap(), 0, "the map kept a dead entry");
    }

    #[test]
    fn only_a_bad_file_is_taken_for_one_worth_deleting() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        std::fs::write(&db, b"this is not a database, it is a sentence").unwrap();
        let bad = Index::open_at(&db).err().expect("garbage opened");
        assert!(is_corrupt(&bad), "{bad}");

        // Another instance holding the lock is no reason to delete it.
        let busy: anyhow::Error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )
        .into();
        assert!(!is_corrupt(&busy));
    }

    #[test]
    fn a_drifted_rowid_map_forces_a_rebuild() {
        // A map that does not describe the rows would delete the wrong text,
        // or none. Better to notice and start again than to quietly rot.
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("i.db");
        {
            let mut idx = Index::open_at(&db).unwrap();
            idx.store(&[sample("/a.jsonl", "one")]).unwrap();
            idx.store_text(&[("/a.jsonl".into(), "some text".into())])
                .unwrap();
        }
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("DELETE FROM body_ref", []).unwrap();
        }
        let idx = Index::open_at(&db).unwrap();
        assert!(
            idx.load().unwrap().is_empty(),
            "a half-written map should have reset the cache"
        );
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
    fn a_scanner_change_rescans_without_blanking_the_list() {
        // The cache keys on (path, mtime, size), so an unchanged transcript
        // is never re-read; a change to what the scanner derives would
        // otherwise serve old values forever. It used to drop the rows,
        // which meant the first run after an update had nothing to show and
        // sat there re-reading gigabytes. Keep them, mark them stale.
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
        assert!(idx.stale, "rows from an older scanner must be marked stale");
        assert_eq!(
            idx.load().unwrap().len(),
            1,
            "they are still worth showing while the rescan runs"
        );

        // Opening again must still say stale. Claiming the version here
        // would tell the background rescan the cache was current, and the
        // new logic would never run at all.
        drop(idx);
        let idx = Index::open_at(&db).unwrap();
        assert!(
            idx.stale,
            "the version was claimed before anything had been rescanned"
        );

        // Only the rescan earns it.
        idx.mark_current().unwrap();
        drop(idx);
        assert!(
            !Index::open_at(&db).unwrap().stale,
            "after a rescan the rows are current"
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

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use std::io::Write;

    /// A transcript on disk, scanned, indexed, and searched — the whole path
    /// a query actually travels.
    fn indexed(lines: &[&str]) -> (tempfile::TempDir, Index, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("11112222-3333-4444-5555-666677778888.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);

        let mut text = String::new();
        let session = crate::scan::scan_with_text(&path, false, None, None, &mut text).unwrap();
        let key = session.path.to_string_lossy().to_string();

        let mut idx = Index::open_at(&dir.path().join("i.db")).unwrap();
        idx.store(&[session]).unwrap();
        idx.store_text(&[(key.clone(), text)]).unwrap();
        (dir, idx, key)
    }

    fn said(role: &str, text: &str) -> String {
        format!(
            r#"{{"parentUuid":"p","message":{{"role":"{role}","content":[{{"type":"text","text":"{text}"}}]}},"type":"{role}"}}"#
        )
    }

    /// A message stored as a bare string rather than a list of blocks.
    fn said_plain(role: &str, text: &str) -> String {
        format!(
            r#"{{"parentUuid":"p","message":{{"role":"{role}","content":"{text}"}},"type":"{role}"}}"#
        )
    }

    #[test]
    fn a_phrase_matches_the_word_it_starts() {
        // A single word has always been a prefix query, so "pool" finds
        // "pooling". A phrase was not, so "connection pool" did not --
        // the same search behaving two ways depending on its length.
        let (_d, idx, key) = indexed(&[
            &said("user", "the fix was connection pooling"),
            &said("assistant", "and 110,000 major page faults"),
        ]);
        for q in ["connection pool", "page fault"] {
            let hits = idx.search_text(&crate::search::fts_expr(q)).unwrap();
            assert_eq!(hits, vec![key.clone()], "{q:?} found nothing");
        }
    }

    #[test]
    fn a_prompt_typed_as_a_plain_string_is_searchable() {
        // 16% of the prose in a real corpus is stored this way -- almost all
        // of it the user's own prompts -- and none of it was in the index.
        let (_d, idx, key) = indexed(&[
            &said_plain("user", "help me figure out why beamng is slow"),
            &said("assistant", "let us look at the frame times"),
        ]);
        let hits = idx.search_text(&crate::search::fts_expr("beamng")).unwrap();
        assert_eq!(hits, vec![key], "the typed prompt was not indexed");
    }

    #[test]
    fn injected_memory_is_not_searchable_prose() {
        // The scan skipped isMeta lines and the index kept them, so a term
        // that appeared only in an injected memory file matched in one
        // engine and not the other.
        let meta = r#"{"parentUuid":"p","message":{"role":"user","content":"the zpool is raidz2"},"type":"user","isMeta":true}"#;
        let (_d, idx, _key) = indexed(&[meta, &said("assistant", "understood")]);
        assert!(
            idx.search_text(&crate::search::fts_expr("raidz2"))
                .unwrap()
                .is_empty(),
            "injected context was indexed as if someone had said it"
        );
    }

    #[test]
    fn something_said_can_be_found_again() {
        let (_d, idx, key) = indexed(&[
            &said("user", "the zpool is degraded"),
            &said("assistant", "checking the array now"),
        ]);
        let hits = idx.search_text(&crate::search::fts_expr("zpool")).unwrap();
        assert_eq!(hits, vec![key.clone()]);
        assert!(idx
            .search_text(&crate::search::fts_expr("degraded"))
            .unwrap()
            .contains(&key));
        assert!(idx
            .search_text(&crate::search::fts_expr("never mentioned"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_phrase_matches_only_when_the_words_are_adjacent() {
        let (_d, idx, key) = indexed(&[&said("user", "the page fault happened at boot")]);
        assert_eq!(
            idx.search_text(&crate::search::fts_expr("page fault"))
                .unwrap(),
            vec![key]
        );
        assert!(idx
            .search_text(&crate::search::fts_expr("fault page"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_partial_word_matches_by_prefix() {
        let (_d, idx, key) = indexed(&[&said("user", "checkpatch was clean")]);
        assert_eq!(
            idx.search_text(&crate::search::fts_expr("checkp")).unwrap(),
            vec![key]
        );
    }

    #[test]
    fn injected_context_is_never_indexed() {
        // The whole reason search was unusable before.
        let (_d, idx, _) = indexed(&[&said(
            "user",
            "<system-reminder>NVENC needs cuda</system-reminder>look at the disk",
        )]);
        assert!(
            idx.search_text(&crate::search::fts_expr("nvenc"))
                .unwrap()
                .is_empty(),
            "a memory file leaked into the index"
        );
        assert!(!idx
            .search_text(&crate::search::fts_expr("disk"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn thinking_and_commands_are_searchable() {
        let (_d, idx, _) = indexed(&[
            r#"{"parentUuid":"p","message":{"role":"assistant","content":[{"type":"thinking","thinking":"perhaps the pool is resilvering"}]},"type":"assistant"}"#,
            r#"{"parentUuid":"p","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"zpool status -v"}}]},"type":"assistant"}"#,
        ]);
        assert!(!idx
            .search_text(&crate::search::fts_expr("resilvering"))
            .unwrap()
            .is_empty());
        assert!(!idx
            .search_text(&crate::search::fts_expr("zpool"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_query_full_of_syntax_cannot_break_the_search() {
        let (_d, idx, _) = indexed(&[&said("user", "ordinary words")]);
        for nasty in [
            "\"",
            "AND OR NOT",
            "*",
            "(unbalanced",
            "a\"b\"c",
            "^x",
            "NEAR/2",
        ] {
            let expr = crate::search::fts_expr(nasty);
            // must not error; finding nothing is a fine answer
            let _ = idx.search_text(&expr).unwrap_or_default();
        }
    }

    #[test]
    fn an_excerpt_comes_back_for_a_hit() {
        let (_d, idx, key) = indexed(&[&said("user", "the quick brown fox jumped over it")]);
        let snip = idx
            .excerpt_for(&key, "brown")
            .expect("a hit has an excerpt");
        assert!(snip.to_lowercase().contains("brown"), "{snip:?}");
    }

    #[test]
    fn every_hit_gets_an_excerpt_in_one_pass() {
        // The command line prints every hit and exits, so it wants them all
        // at once. Asking FTS5 for a snippet per row cost eighty-five
        // seconds on a word that appears in every transcript.
        let (_d, idx, key) = indexed(&[
            &said("user", "the connection pool was exhausted"),
            &said("assistant", "the pool is the problem"),
        ]);
        let all = idx
            .excerpts(&crate::search::fts_expr("pool"), "pool")
            .unwrap();
        assert_eq!(all.len(), 1, "one transcript, one excerpt");
        assert!(all[&key].to_lowercase().contains("pool"), "{:?}", all[&key]);
    }

    #[test]
    fn appended_text_joins_what_was_already_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", said("user", "zebra came first")).unwrap();
        drop(f);

        let mut t1 = String::new();
        let s1 = crate::scan::scan_with_text(&path, false, None, None, &mut t1).unwrap();
        let key = s1.path.to_string_lossy().to_string();
        let mut idx = Index::open_at(&dir.path().join("i.db")).unwrap();
        idx.store(std::slice::from_ref(&s1)).unwrap();
        idx.store_text(&[(key.clone(), t1)]).unwrap();

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{}", said("user", "quokka came later")).unwrap();
        drop(f);

        let mut t2 = String::new();
        let s2 = crate::scan::scan_with_text(&path, false, None, Some(&s1), &mut t2).unwrap();
        let merged = format!("{} {}", idx.existing_text(&key).unwrap().unwrap(), t2);
        idx.store(&[s2]).unwrap();
        idx.store_text(&[(key.clone(), merged)]).unwrap();

        for word in ["zebra", "quokka"] {
            assert!(
                !idx.search_text(&crate::search::fts_expr(word))
                    .unwrap()
                    .is_empty(),
                "{word} went missing after the append"
            );
        }
    }

    /// Scan `path` and store what a refresh would, in the order it does.
    fn rescan(idx: &mut Index, path: &std::path::Path, prev: Option<&Session>) -> Session {
        let mut text = String::new();
        let s = crate::scan::scan_with_text(path, false, None, prev, &mut text).unwrap();
        let key = s.path.to_string_lossy().to_string();
        if let Some(t) = text_after_scan(idx, prev, &s, &text) {
            idx.store_text(&[(key, t)]).unwrap();
        }
        idx.store(std::slice::from_ref(&s)).unwrap();
        s
    }

    fn first_scan(lines: &[String]) -> (tempfile::TempDir, Index, std::path::PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);
        let mut idx = Index::open_at(&dir.path().join("i.db")).unwrap();
        let s = rescan(&mut idx, &path, None);
        (dir, idx, path, s)
    }

    fn finds(idx: &Index, word: &str) -> bool {
        !idx.search_text(&crate::search::fts_expr(word))
            .unwrap()
            .is_empty()
    }

    #[test]
    fn a_write_waits_for_another_instance_instead_of_losing_its_text() {
        // Another mn -- or `R` while the rescan behind the splash is still
        // going -- holding the write lock. The refresh read first and asked
        // for the lock second, so when the other committed SQLite could not
        // hand it over, and the new text was dropped while the rows saying
        // it had been read were stored.
        let (_d, mut idx, path, s1) = first_scan(&[said("user", "zebra came first")]);
        let db = _d.path().join("i.db");
        let other = Connection::open(&db).unwrap();
        other
            .execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO meta (key,value) VALUES ('other','writing');",
            )
            .unwrap();
        let done = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.execute_batch("COMMIT").unwrap();
        });
        let key = s1.path.to_string_lossy().to_string();
        let r = idx.persist(&[s1], &[(key, "zebra quokka".into())]);
        done.join().unwrap();
        r.expect("it should have waited for the other writer");
        assert!(finds(&idx, "quokka"));
        drop(path);
    }

    #[test]
    fn a_cache_that_cannot_be_written_still_refreshes() {
        use std::os::unix::fs::PermissionsExt;
        let (d, idx, path, _s1) = first_scan(&[said("user", "zebra came first")]);
        drop(idx);
        let db = d.path().join("i.db");
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{}", said("user", "quokka came later")).unwrap();
        drop(f);

        let idx = Index::open_at(&db).unwrap();
        let got = refresh_in(idx, vec![(path, false, None)], None)
            .expect("an unwritable cache stopped the refresh");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].user_msgs, 2, "the new line was read all the same");
    }

    #[test]
    fn a_transcript_touched_but_not_grown_keeps_its_text() {
        // Its mtime moved, so it is read again from where it stopped, and
        // there is nothing after that. Nothing new is not "nothing at all":
        // storing the empty tail as the whole text unindexed the session.
        let (_d, mut idx, path, s1) = first_scan(&[said("user", "zebra came first")]);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        rescan(&mut idx, &path, Some(&s1));
        assert!(finds(&idx, "zebra"), "touching the file unindexed it");
    }

    #[test]
    fn a_half_written_line_keeps_what_was_indexed() {
        // Claude is mid-write: the file grew, but only by a line that is not
        // finished yet, so the scan consumes nothing new.
        let (_d, mut idx, path, s1) = first_scan(&[said("user", "zebra came first")]);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "{{\"parentUuid\":\"p\",\"mess").unwrap();
        drop(f);
        let s2 = rescan(&mut idx, &path, Some(&s1));
        assert!(finds(&idx, "zebra"), "a partial line unindexed the session");

        // ...and once the line is finished, both halves are there.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            "age\":{{\"role\":\"user\",\"content\":\"quokka later\"}},\"type\":\"user\"}}"
        )
        .unwrap();
        drop(f);
        rescan(&mut idx, &path, Some(&s2));
        assert!(finds(&idx, "zebra") && finds(&idx, "quokka"));
    }

    #[test]
    fn a_full_rescan_replaces_the_text_rather_than_adding_to_it() {
        // A new scanner reads every transcript from the start. One that had
        // also grown since was stored as its old text followed by all of
        // its text again, so everything in it was indexed twice -- and the
        // old half was what the new scanner existed to replace.
        let (_d, mut idx, path, _s1) = first_scan(&[said("user", "zebra came first")]);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{}", said("user", "quokka came later")).unwrap();
        drop(f);
        // stale: nothing cached is trusted, so there is no `prev`
        let s2 = rescan(&mut idx, &path, None);
        let key = s2.path.to_string_lossy().to_string();
        let text = idx.existing_text(&key).unwrap().unwrap();
        assert_eq!(text.matches("zebra").count(), 1, "{text:?}");
        assert!(text.contains("quokka"), "{text:?}");
    }
}

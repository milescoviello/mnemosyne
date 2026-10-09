//! Hermes sessions.
//!
//! Hermes keeps every conversation in one SQLite database,
//! `$HERMES_HOME/state.db`: a `sessions` table (id, where it came from, the
//! folder, title, model, branch and token totals) and a `messages` table, a
//! row a message. It is the agent behind cron jobs and chat gateways as
//! well as a terminal, so only the sessions started in a terminal are
//! listed: `cli`, and `oneshot` (`hermes -z`).
//!
//! A session's row in the index is keyed `<state.db>#<id>`. What a file's
//! byte offset is to the others, the last message's id is here: a refresh
//! reads only the messages after it. The database is opened read-only, and
//! not at all when neither it nor its write-ahead log has changed since the
//! last refresh.

use crate::model::{Harness, Session};
use crate::scan;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where the sessions come from that a terminal started.
const SOURCES: &[&str] = &["cli", "oneshot"];

/// Hermes's database, wherever `HERMES_HOME` puts it.
pub fn db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HERMES_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join(".hermes"));
    Some(home.join("state.db"))
}

/// The key a session's row is stored under.
pub fn key(db: &Path, id: &str) -> String {
    format!("{}#{id}", db.display())
}

/// The database and session a row's key names.
pub fn split_key(key: &str) -> Option<(PathBuf, String)> {
    let (db, id) = key.rsplit_once('#')?;
    Some((PathBuf::from(db), id.to_string()))
}

/// Whether an index row is one of Hermes's.
pub fn is_key_of(db: &Path, key: &str) -> bool {
    key.strip_prefix(&db.display().to_string())
        .is_some_and(|rest| rest.starts_with('#'))
}

/// A value that changes whenever the database could have: the size and time
/// of the file and of its write-ahead log, where new messages land first.
pub fn stamp(db: &Path) -> Option<String> {
    let one = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{}:{t}", m.len())
            })
            .unwrap_or_default()
    };
    let main = std::fs::metadata(db).ok()?;
    if !main.is_file() {
        return None;
    }
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    Some(format!("{}/{}", one(db), one(&wal)))
}

/// Open it to read and nothing else, seeing what a running Hermes has
/// written to its log but not yet folded in.
pub fn open(db: &Path) -> Option<Connection> {
    let uri = format!("file:{}?mode=ro", db.display());
    let c = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // Hermes may be writing; a moment's wait beats a missing refresh.
    let _ = c.busy_timeout(std::time::Duration::from_millis(500));
    Some(c)
}

/// Every terminal session in the database, each read on from where `cached`
/// left it. `None` when the database cannot be read, so the caller keeps
/// what it had rather than taking every session for deleted.
///
/// Each comes with the text of the messages read this time, and with
/// `resumed_from` set as a file's would be: `None` when it was read whole,
/// the last message id before when only what followed was.
pub fn scan(
    db: &Path,
    cached: &HashMap<String, Session>,
    everything: bool,
) -> Option<Vec<(Session, String)>> {
    let c = open(db)?;
    let have: std::collections::HashSet<String> = {
        let mut st = c.prepare("PRAGMA table_info(sessions)").ok()?;
        let rows = st.query_map([], |r| r.get::<_, String>(1)).ok()?;
        rows.flatten().collect()
    };
    if !have.contains("id") || !have.contains("source") {
        return None;
    }
    // Columns Hermes added as it went: an older database lacks some, and
    // what is not there is simply not known.
    let col = |name: &str| {
        if have.contains(name) {
            format!("s.{name}")
        } else {
            "NULL".to_string()
        }
    };
    let archived = if have.contains("archived") {
        "AND COALESCE(s.archived, 0) = 0"
    } else {
        ""
    };
    let sources = SOURCES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT s.id, {cwd}, {title}, {model}, {started}, {ended}, {branch},
                {inp}, {out}, {cr}, {cw},
                (SELECT max(m.id) FROM messages m WHERE m.session_id = s.id)
         FROM sessions s WHERE s.source IN ({sources}) {archived}",
        cwd = col("cwd"),
        title = col("title"),
        model = col("model"),
        started = col("started_at"),
        ended = col("ended_at"),
        branch = col("git_branch"),
        inp = col("input_tokens"),
        out = col("output_tokens"),
        cr = col("cache_read_tokens"),
        cw = col("cache_write_tokens"),
    );
    struct Row {
        id: String,
        cwd: Option<String>,
        title: Option<String>,
        model: Option<String>,
        started: Option<f64>,
        ended: Option<f64>,
        branch: Option<String>,
        tokens: [Option<i64>; 4],
        last: Option<i64>,
    }
    let rows: Vec<Row> = {
        let mut st = c.prepare(&sql).ok()?;
        let it = st
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    cwd: r.get(1)?,
                    title: r.get(2)?,
                    model: r.get(3)?,
                    started: r.get(4)?,
                    ended: r.get(5)?,
                    branch: r.get(6)?,
                    tokens: [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?],
                    last: r.get(11)?,
                })
            })
            .ok()?;
        it.flatten().collect()
    };

    let mut msgs = c
        .prepare(
            "SELECT id, role, content, timestamp FROM messages
             WHERE session_id = ?1 AND id > ?2 ORDER BY id",
        )
        .ok()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let k = key(db, &row.id);
        let last = row.last.unwrap_or(0).max(0) as u64;
        let prev = if everything { None } else { cached.get(&k) };
        // Read on from the last message seen, unless that is past the end:
        // messages gone means the session is not the one remembered.
        let from = prev.filter(|p| p.scanned_len <= last);
        let mut s = match from {
            Some(p) => p.clone(),
            None => Session {
                harness: Harness::Hermes,
                id: row.id.clone(),
                path: PathBuf::from(&k),
                ..Default::default()
            },
        };
        s.cwd = row.cwd.unwrap_or_default();
        s.ai_title = row.title.map(|t| scan::squash(&t, 160)).unwrap_or_default();
        s.model = row.model.unwrap_or_default();
        s.git_branch = row.branch.unwrap_or_default();
        let n = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
        s.in_tokens = n(row.tokens[0]);
        s.out_tokens = n(row.tokens[1]);
        s.cache_read = n(row.tokens[2]);
        s.cache_write = n(row.tokens[3]);
        let started = row.started.unwrap_or(0.0) as i64;
        if started > 0 {
            s.first_ts = started;
        }

        let mut text = String::new();
        let seen = s.scanned_len;
        if from.is_none() || last > seen {
            let read = msgs
                .query_map(rusqlite::params![row.id, seen as i64], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                    ))
                })
                .ok()?;
            for (id, role, content, ts) in read.flatten() {
                take_message(&mut s, &role, content.as_deref().unwrap_or(""), &mut text);
                s.scanned_len = s.scanned_len.max(id.max(0) as u64);
                let ts = ts.unwrap_or(0.0) as i64;
                if ts > s.last_ts {
                    s.last_ts = ts;
                }
            }
        }
        if let Some(e) = row.ended.map(|e| e as i64) {
            s.last_ts = s.last_ts.max(e);
        }
        s.last_ts = s.last_ts.max(s.first_ts);
        s.mtime = s.last_ts;
        s.resumed_from = from.map(|p| p.scanned_len);
        out.push((s, text));
    }
    Some(out)
}

/// Take in one message.
fn take_message(s: &mut Session, role: &str, content: &str, text: &mut String) {
    s.entries += 1;
    s.size += content.len() as u64;
    match role {
        "user" => {
            s.user_msgs += 1;
            if scan::is_real_user_text(content) {
                if s.first_prompt.is_empty() {
                    s.first_prompt = scan::squash(content, 200);
                }
                s.last_prompt = scan::squash(content, 200);
            }
        }
        "assistant" => s.assistant_msgs += 1,
        // A tool's result, or Hermes's own bookkeeping: counted, not read.
        _ => return,
    }
    if !content.trim().is_empty() && text.len() < scan::HARVEST_CAP {
        text.push_str(content);
        text.push(' ');
        if text.len() >= scan::HARVEST_CAP {
            scan::say_over_cap(s);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A database with the parts of Hermes's schema this reads -- as an
    /// older Hermes had it when `old` is set, before cwd and branch.
    pub fn make_db(path: &Path, old: bool) -> Connection {
        let c = Connection::open(path).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT,
                started_at REAL NOT NULL, ended_at REAL, message_count INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
                title TEXT {}
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                role TEXT NOT NULL, content TEXT, tool_name TEXT, timestamp REAL NOT NULL
             );",
            if old {
                ""
            } else {
                ", cwd TEXT, git_branch TEXT, archived INTEGER NOT NULL DEFAULT 0"
            }
        ))
        .unwrap();
        c
    }

    pub fn add_session(c: &Connection, id: &str, source: &str, cwd: Option<&str>) {
        c.execute(
            "INSERT INTO sessions (id, source, model, started_at, input_tokens, output_tokens,
                cache_read_tokens, title) VALUES (?1, ?2, 'gpt-5.6-terra', 1790845200.5, 900, 80, 300,
                'Set up music request webhook')",
            rusqlite::params![id, source],
        )
        .unwrap();
        if let Some(cwd) = cwd {
            c.execute(
                "UPDATE sessions SET cwd = ?2, git_branch = 'main' WHERE id = ?1",
                rusqlite::params![id, cwd],
            )
            .unwrap();
        }
    }

    pub fn say(c: &Connection, id: &str, role: &str, content: &str, at: f64) {
        c.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, role, content, at],
        )
        .unwrap();
    }

    fn conversation(c: &Connection) {
        add_session(c, "20260817_192329_cfe3e9", "cli", Some("/home/u/bot"));
        say(
            c,
            "20260817_192329_cfe3e9",
            "session_meta",
            "",
            1790845200.6,
        );
        say(
            c,
            "20260817_192329_cfe3e9",
            "user",
            "set up a webhook for music requests",
            1790845201.0,
        );
        say(
            c,
            "20260817_192329_cfe3e9",
            "assistant",
            "Registering the webhook route now.",
            1790845210.0,
        );
        say(
            c,
            "20260817_192329_cfe3e9",
            "tool",
            r#"{"output": "TOOLOUTPUT"}"#,
            1790845211.0,
        );
        say(c, "20260817_192329_cfe3e9", "assistant", "", 1790845212.0);
        // the others Hermes keeps, which are not a terminal's
        add_session(
            c,
            "cron_aba2eeef9c66_20261009_070030",
            "cron",
            Some("/home/u"),
        );
        say(
            c,
            "cron_aba2eeef9c66_20261009_070030",
            "user",
            "daily debrief",
            1790845300.0,
        );
        add_session(c, "20261008_183534_34561342", "telegram", None);
        add_session(c, "20260928_184824_c77606", "oneshot", None);
        say(
            c,
            "20260928_184824_c77606",
            "user",
            "find the lecture",
            1790845400.0,
        );
    }

    #[test]
    fn terminal_sessions_are_read_from_the_database() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        conversation(&c);
        let got = scan(&db, &HashMap::new(), false).unwrap();
        let mut ids: Vec<&str> = got.iter().map(|(s, _)| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            ["20260817_192329_cfe3e9", "20260928_184824_c77606"],
            "only cli and oneshot"
        );

        let (s, text) = got
            .iter()
            .find(|(s, _)| s.id == "20260817_192329_cfe3e9")
            .unwrap();
        assert_eq!(s.harness, Harness::Hermes);
        assert_eq!(s.path, PathBuf::from(key(&db, &s.id)));
        assert_eq!(s.cwd, "/home/u/bot");
        assert_eq!(s.git_branch, "main");
        assert_eq!(s.title(), "Set up music request webhook");
        assert_eq!(s.model, "gpt-5.6-terra");
        assert_eq!(s.first_prompt, "set up a webhook for music requests");
        assert_eq!((s.user_msgs, s.assistant_msgs, s.entries), (1, 2, 5));
        assert_eq!((s.in_tokens, s.out_tokens, s.cache_read), (900, 80, 300));
        assert_eq!(s.first_ts, 1790845200);
        assert_eq!(s.last_ts, 1790845212);
        assert_eq!(s.mtime, s.last_ts, "listed by when it was last used");
        assert_eq!(s.resumed_from, None);
        assert!(text.contains("music requests") && text.contains("webhook route"));
        assert!(!text.contains("TOOLOUTPUT"), "a tool's output was indexed");

        let (one, _) = got
            .iter()
            .find(|(s, _)| s.id == "20260928_184824_c77606")
            .unwrap();
        assert!(one.cwd.is_empty(), "no folder recorded is no folder");
    }

    #[test]
    fn a_session_that_went_on_is_read_from_its_last_message() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        conversation(&c);
        let first = scan(&db, &HashMap::new(), false).unwrap();
        let cached: HashMap<String, Session> = first
            .into_iter()
            .map(|(s, _)| (s.path.to_string_lossy().to_string(), s))
            .collect();
        let id = "20260817_192329_cfe3e9";
        let before = cached[&key(&db, id)].clone();
        say(&c, id, "user", "now add a rate limit", 1790845500.0);
        say(
            &c,
            id,
            "assistant",
            "Limited to five a minute.",
            1790845510.0,
        );

        let again = scan(&db, &cached, false).unwrap();
        let (s, text) = again.iter().find(|(s, _)| s.id == id).unwrap();
        assert_eq!(s.resumed_from, Some(before.scanned_len));
        assert!(s.scanned_len > before.scanned_len);
        assert_eq!((s.user_msgs, s.assistant_msgs), (2, 3));
        assert_eq!(s.first_prompt, "set up a webhook for music requests");
        assert_eq!(s.last_prompt, "now add a rate limit");
        assert_eq!(s.last_ts, 1790845510);
        assert!(
            text.contains("rate limit") && !text.contains("music requests"),
            "{text:?}"
        );

        // and one with nothing new is left as it was
        let (o, t) = again
            .iter()
            .find(|(s, _)| s.id == "20260928_184824_c77606")
            .unwrap();
        assert_eq!(o.resumed_from, Some(o.scanned_len));
        assert!(t.is_empty());
        assert_eq!(o.user_msgs, 1, "counted twice");
    }

    #[test]
    fn an_older_database_without_folders_is_still_read() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, true);
        add_session(&c, "20260603_223256_2d44b2", "cli", None);
        say(&c, "20260603_223256_2d44b2", "user", "hello", 1790845200.7);
        let got = scan(&db, &HashMap::new(), false).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].0.cwd.is_empty());
        assert_eq!(got[0].0.first_prompt, "hello");
    }

    #[test]
    fn an_archived_session_is_left_out_as_hermes_leaves_it_out() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        add_session(&c, "a", "cli", Some("/w"));
        add_session(&c, "b", "cli", Some("/w"));
        c.execute("UPDATE sessions SET archived = 1 WHERE id = 'b'", [])
            .unwrap();
        let got = scan(&db, &HashMap::new(), false).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.id, "a");
    }

    #[test]
    fn no_database_is_not_an_empty_one() {
        let d = tempfile::tempdir().unwrap();
        assert!(scan(&d.path().join("state.db"), &HashMap::new(), false).is_none());
        assert!(stamp(&d.path().join("state.db")).is_none());
    }

    #[test]
    fn the_stamp_moves_when_the_log_does() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        std::fs::write(&db, "x").unwrap();
        let a = stamp(&db).unwrap();
        std::fs::write(d.path().join("state.db-wal"), "more").unwrap();
        assert_ne!(stamp(&db).unwrap(), a);
    }

    #[test]
    fn a_key_belongs_to_its_database() {
        let db = Path::new("/h/.hermes/state.db");
        assert!(is_key_of(db, &key(db, "x")));
        assert!(!is_key_of(db, "/h/.hermes/state.db2#x"));
        assert!(!is_key_of(db, "/h/.claude/projects/a/b.jsonl"));
    }
}

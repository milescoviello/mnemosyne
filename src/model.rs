//! Shared data types.
//!
//! A `Session` is one Claude Code transcript (`~/.claude/projects/<enc>/<uuid>.jsonl`)
//! plus everything we could cheaply learn about it, plus the user's own
//! favourite/tag overlay and whether it is running right now.

use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct Session {
    // identity / location
    pub id: String,
    pub path: PathBuf,
    pub project_dir: String,
    pub cwd: String,
    pub git_branch: String,

    // what it was about
    pub ai_title: String,
    pub first_prompt: String,
    pub last_prompt: String,
    pub model: String,
    pub permission_mode: String,
    pub version: String,

    // size / time
    pub size: u64,
    pub mtime: i64,
    pub first_ts: i64,
    pub last_ts: i64,
    pub entries: u32,
    pub user_msgs: u32,
    pub assistant_msgs: u32,
    /// Token usage, summed from the `usage` records the transcripts already
    /// carry. Cache reads are counted separately because they dominate the
    /// totals and are not comparable to fresh input.
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,

    /// How many bytes of this file the indexer has already consumed.
    /// Transcripts are append-only, so on the next run we read only the tail
    /// past this offset instead of re-parsing the whole file.
    pub scanned_len: u64,

    // subagent linkage: <project>/<parent-session-id>/subagents/agent-*.jsonl
    pub is_subagent: bool,
    pub parent: Option<String>,
    pub agent_id: String,

    // ---- runtime overlay, never persisted in the index ----
    pub favorite: bool,
    pub tags: Vec<String>,
    pub note: String,
    pub live_pid: Option<i32>,
    /// true when we matched the pid by `--resume <id>`, false when we only
    /// matched its working directory (so it is a guess).
    pub live_exact: bool,
    pub subagent_count: u32,
    /// A tmux session named for this one already exists, so we can attach.
    pub has_tmux: bool,
    /// The directory this session ran in is gone. Resuming still works, but
    /// lands wherever you happen to be standing, so it is worth seeing first.
    pub cwd_missing: bool,
}

impl Session {
    /// Best human label: Claude's own generated title wins, then the opening
    /// prompt, because a pasted mega-prompt makes a terrible list entry.
    pub fn title(&self) -> &str {
        if !self.ai_title.is_empty() {
            &self.ai_title
        } else if !self.first_prompt.is_empty() {
            &self.first_prompt
        } else if self.is_subagent {
            "(subagent)"
        } else {
            "(untitled session)"
        }
    }

    /// Everything the model was charged for reading or writing.
    pub fn total_tokens(&self) -> u64 {
        self.in_tokens + self.out_tokens + self.cache_read + self.cache_write
    }

    pub fn duration_secs(&self) -> i64 {
        if self.first_ts > 0 && self.last_ts > self.first_ts {
            self.last_ts - self.first_ts
        } else {
            0
        }
    }

    pub fn is_live(&self) -> bool {
        self.live_pid.is_some()
    }

    /// Short model label for the table column.
    pub fn model_short(&self) -> &str {
        let m = self.model.as_str();
        m.strip_prefix("claude-").unwrap_or(m)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Recency,
    Size,
    Entries,
    Duration,
    Title,
    Folder,
    Tokens,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Recency => "recency",
            Sort::Size => "size",
            Sort::Entries => "entries",
            Sort::Duration => "duration",
            Sort::Title => "title",
            Sort::Folder => "folder",
            Sort::Tokens => "tokens",
        }
    }
    pub fn next(self) -> Sort {
        match self {
            Sort::Recency => Sort::Size,
            Sort::Size => Sort::Entries,
            Sort::Entries => Sort::Duration,
            Sort::Duration => Sort::Title,
            Sort::Title => Sort::Folder,
            Sort::Folder => Sort::Tokens,
            Sort::Tokens => Sort::Recency,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateRange {
    All,
    Today,
    Days(i64),
}

impl DateRange {
    pub fn label(self) -> String {
        match self {
            DateRange::All => "any date".into(),
            DateRange::Today => "today".into(),
            DateRange::Days(n) => format!("last {n}d"),
        }
    }
    pub fn next(self) -> DateRange {
        match self {
            DateRange::All => DateRange::Today,
            DateRange::Today => DateRange::Days(7),
            DateRange::Days(7) => DateRange::Days(30),
            DateRange::Days(30) => DateRange::Days(90),
            _ => DateRange::All,
        }
    }
}

// ---------- small formatting helpers shared by the UI ----------

pub fn reltime(epoch: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let d = (now - epoch).max(0);
    if d < 90 {
        format!("{d}s")
    } else if d < 5400 {
        format!("{}m", d / 60)
    } else if d < 129_600 {
        format!("{}h", d / 3600)
    } else {
        format!("{}d", d / 86_400)
    }
}

pub fn human_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", b, U[0])
    } else if v < 10.0 {
        format!("{v:.1}{}", U[i])
    } else {
        format!("{v:.0}{}", U[i])
    }
}

pub fn human_dur(secs: i64) -> String {
    if secs <= 0 {
        return "-".into();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// 412 -> "412", 6312 -> "6.3k", 1_200_000 -> "1.2m"
pub fn compact_count(n: u32) -> String {
    if n < 1000 {
        format!("{n}")
    } else if n < 1_000_000 {
        let v = n as f64 / 1000.0;
        if v < 10.0 {
            format!("{v:.1}k")
        } else {
            format!("{v:.0}k")
        }
    } else {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    }
}

/// Like `compact_count` but for the larger numbers token totals reach.
pub fn human_count(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else {
        format!("{:.2}b", n as f64 / 1_000_000_000.0)
    }
}

/// Clip to `w` display columns, ending in an ellipsis when it had to cut.
pub fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    if w <= 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

/// `/home/miles/OS-DEV` -> `~/OS-DEV`
pub fn short_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if cwd == home {
        "~".into()
    } else if !home.is_empty() && cwd.starts_with(&format!("{home}/")) {
        format!("~/{}", &cwd[home.len() + 1..])
    } else {
        cwd.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_compactly() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1.0k");
        assert_eq!(compact_count(6_312), "6.3k");
        assert_eq!(compact_count(66_810), "67k");
        assert_eq!(compact_count(1_200_000), "1.2m");
    }

    #[test]
    fn fit_never_splits_a_character() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello", 5), "hello");
        assert_eq!(fit("hello", 4), "hel…");
        assert_eq!(fit("hello", 1), "…");
        // multi-byte input must clip by character, not by byte
        let s = fit("héllo wörld ≈≈≈", 7);
        assert_eq!(s.chars().count(), 7);
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(403_800_000), "385M");
        assert_eq!(human_dur(0), "-");
        assert_eq!(human_dur(45), "45s");
        assert_eq!(human_dur(3_600), "1h0m");
        assert_eq!(human_dur(90_000), "1d1h");
    }

    #[test]
    fn home_is_abbreviated() {
        std::env::set_var("HOME", "/home/u");
        assert_eq!(short_cwd("/home/u"), "~");
        assert_eq!(short_cwd("/home/u/proj"), "~/proj");
        // a path that merely starts with the same letters is left alone
        assert_eq!(short_cwd("/home/us2/proj"), "/home/us2/proj");
        assert_eq!(short_cwd("/etc"), "/etc");
    }

    #[test]
    fn sort_cycles_through_every_mode_and_returns() {
        // Counted rather than hardcoded, so adding a mode makes this fail
        // usefully instead of silently going stale.
        let mut s = Sort::Recency;
        let mut seen = Vec::new();
        loop {
            seen.push(s.label());
            s = s.next();
            if s == Sort::Recency || seen.len() > 32 {
                break;
            }
        }
        assert_eq!(s, Sort::Recency, "cycling returns to the start");
        let mut uniq = seen.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            seen.len(),
            "every mode appears exactly once: {seen:?}"
        );
        assert!(seen.contains(&"tokens"), "tokens is reachable: {seen:?}");
    }

    #[test]
    fn date_ranges_cycle_back_to_all() {
        let mut d = DateRange::All;
        for _ in 0..5 {
            d = d.next();
        }
        assert_eq!(d, DateRange::All);
    }
}

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
    /// What you named it with `/rename`. Outranks everything.
    pub custom_title: String,
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
    /// The API response whose usage was counted last. One response is
    /// written as a line per content block, each carrying all of its usage,
    /// so the lines after the first are the same numbers again.
    pub last_msg_id: String,

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
    /// The process running it was started by wsx: it is a workspace's agent.
    pub live_in_wsx: bool,
    pub subagent_count: u32,
    /// A tmux session is already running this one, so we can attach.
    pub has_tmux: bool,
    /// What that tmux session is called. Not always `mn-<id>`: a session can
    /// be given any name when it is created, and is then recognised by the
    /// command it was started with rather than by what it ended up called.
    pub tmux_session: String,
    /// The directory this session ran in is gone. Resuming still works, but
    /// lands wherever you happen to be standing, so it is worth seeing first.
    pub cwd_missing: bool,
    /// The wsx workspace it ran in, read off the folder. Worked out on every
    /// load rather than stored in the index: it depends on what wsx says now,
    /// not on anything in the transcript.
    pub wsx: Option<crate::wsx::Place>,
}

impl Session {
    /// Best human label: the name you gave it, then Claude's own generated
    /// title, then the opening prompt, because a pasted mega-prompt makes a
    /// terrible list entry.
    pub fn title(&self) -> &str {
        if !self.custom_title.is_empty() {
            &self.custom_title
        } else if !self.ai_title.is_empty() {
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

    /// Where it ran, as the list names it: `OS-DEV/shy-daffodil` for a wsx
    /// workspace, the folder otherwise.
    pub fn folder(&self) -> String {
        match &self.wsx {
            Some(w) => w.label(),
            None => short_cwd(&self.cwd),
        }
    }

    /// The folder resuming it lands in, when that is not the one it ran in.
    ///
    /// Archiving a wsx workspace deletes its worktree, but the repo it was a
    /// worktree of is still checked out -- a better place to carry on than
    /// wherever you happen to be standing. A worktree archived and kept is
    /// still there, and that is where it resumes.
    pub fn resumes_elsewhere(&self) -> Option<&str> {
        if !self.cwd_missing {
            return None;
        }
        match &self.wsx.as_ref()?.status {
            crate::wsx::Status::Archived { checkout: Some(c) } => Some(c),
            _ => None,
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
    if b < 1024 {
        return format!("{b}B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    // One decimal below ten, none above -- judged on what will be shown,
    // not on the raw value, or 9.96 came out as "10.0" and 1023.9 as
    // "1024" of a unit that should have rolled over.
    let shown = |v: f64| {
        let one = format!("{v:.1}");
        if one.len() <= 3 {
            one
        } else {
            format!("{v:.0}")
        }
    };
    let mut text = shown(v);
    if text == "1024" && i < U.len() - 1 {
        text = shown(v / 1024.0);
        i += 1;
    }
    format!("{text}{}", U[i])
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
    // Rounded in whole numbers, and the unit chosen from the rounded value:
    // picking it first made 9,960 "10.0k" and 999,600 "1000k".
    let n = u64::from(n);
    let tenths_k = (n + 50) / 100;
    let k = (n + 500) / 1_000;
    let tenths_m = (n + 50_000) / 100_000;
    let tenths_b = (n + 50_000_000) / 100_000_000;
    if n < 1000 {
        format!("{n}")
    } else if tenths_k < 100 {
        format!("{}.{}k", tenths_k / 10, tenths_k % 10)
    } else if k < 1000 {
        format!("{k}k")
    } else if tenths_m < 10_000 {
        format!("{}.{}m", tenths_m / 10, tenths_m % 10)
    } else {
        format!("{}.{}b", tenths_b / 10, tenths_b % 10)
    }
}

/// Like `compact_count` but for the larger numbers token totals reach.
pub fn human_count(n: u64) -> String {
    // As compact_count: round, then pick the unit, so 999,600 is "1.0m".
    let n = u128::from(n);
    let k = (n + 500) / 1_000;
    let tenths_m = (n + 50_000) / 100_000;
    let hundredths_b = (n + 5_000_000) / 10_000_000;
    let hundredths_t = (n + 5_000_000_000) / 10_000_000_000;
    if n < 1_000 {
        format!("{n}")
    } else if k < 1_000 {
        format!("{k}k")
    } else if tenths_m < 10_000 {
        format!("{}.{}m", tenths_m / 10, tenths_m % 10)
    } else if hundredths_b < 100_000 {
        format!("{}.{:02}b", hundredths_b / 100, hundredths_b % 100)
    } else {
        format!("{}.{:02}t", hundredths_t / 100, hundredths_t % 100)
    }
}

/// How many terminal cells a string occupies.
///
/// Not its length in characters: CJK and emoji take two cells each, so a
/// title of twelve characters can be twenty-four columns wide. Counting
/// characters made every row containing one overflow its column and push
/// the ones after it off the screen.
pub fn width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// Clip to `w` display columns, ending in an ellipsis when it had to cut.
pub fn fit(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        // A column with no room left gets nothing. Returning an ellipsis
        // here put one column into a zero-column space.
        return String::new();
    }
    if w == 1 {
        return "…".into();
    }
    // Leave a column for the ellipsis, and never cut a wide character in
    // half -- half of a wide character is not half a column, it is a
    // different character or a broken cell.
    //
    // Measured as a whole, as `width` and the terminal measure it. Adding up
    // characters one at a time counts ⚠ and its emoji selector as one
    // column, where the pair draws two: a clipped title with ⚠️ in it came
    // out wider than its column, and every column after it on that row
    // slid right.
    let mut out = String::new();
    for c in s.chars() {
        out.push(c);
        if width(&out) > w - 1 {
            out.pop();
            break;
        }
    }
    out.push('…');
    out
}

/// Clip to `w` display columns and pad to exactly that many.
///
/// `format!("{:<w$}", ..)` pads by character count, which is the same bug in
/// the other direction: a row with a wide character came out short.
pub fn pad_fit(s: &str, w: usize) -> String {
    let mut out = fit(s, w);
    let used = width(&out);
    if used < w {
        out.push_str(&" ".repeat(w - used));
    }
    out
}

/// `/home/miles/OS-DEV` -> `~/OS-DEV`
pub fn short_cwd(cwd: &str) -> String {
    short_cwd_in(cwd, &std::env::var("HOME").unwrap_or_default())
}

/// The same, with the home directory handed in. Its test used to set
/// `HOME` for the whole process to get a known value -- and every other
/// test that draws a folder column reads `HOME` too, so the suite failed
/// now and then depending on which ran first. Environment variables are
/// global; a test must never write one.
pub fn short_cwd_in(cwd: &str, home: &str) -> String {
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
    fn a_number_that_rounds_up_takes_the_next_unit() {
        // The unit used to be chosen before rounding, so a value just under
        // a boundary rounded up past it and kept the smaller unit.
        assert_eq!(compact_count(9_960), "10k", "not 10.0k");
        assert_eq!(compact_count(999_600), "1.0m", "not 1000k");
        assert_eq!(human_size(10_200), "10K", "not 10.0K");
        assert_eq!(human_size(1_048_500), "1.0M", "not 1024K");
        assert_eq!(human_count(999_600), "1.0m", "not 1000k");
        assert_eq!(human_count(999_960_000), "1.00b", "not 1000.0m");
        // and just under stays where it was
        assert_eq!(compact_count(9_940), "9.9k");
        assert_eq!(human_size(1_047_000), "1022K");
    }

    #[test]
    fn no_number_is_shown_at_or_past_its_units_limit() {
        // Every threshold, from just below to just above, for all three.
        // What is shown must be under 1000 of its unit (1024 for sizes),
        // and a decimal is only ever shown below ten -- one rule, checked
        // everywhere rather than at a few chosen examples.
        fn split(s: &str) -> (f64, &str) {
            let at = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
            (
                s[..at].parse().unwrap_or_else(|_| panic!("{s:?}")),
                &s[at..],
            )
        }
        let near = |edge: u64| (edge.saturating_sub(2_000)..edge + 2_000).step_by(7);
        let mut edges: Vec<u64> = Vec::new();
        // 10, 100, 1000 of each unit: where a decimal is dropped, and where
        // the unit rolls over.
        for p in 0..4 {
            let k = 1000u64.pow(p + 1);
            edges.extend([k / 100, k / 10, k]);
        }
        for n in edges
            .iter()
            .flat_map(|&e| near(e))
            .chain(near(4_294_967_295))
        {
            if let Ok(small) = u32::try_from(n) {
                let s = compact_count(small);
                let (v, unit) = split(&s);
                assert!(v < 1000.0, "compact_count({n}) = {s}");
                if s.contains('.') && unit != "m" {
                    assert!(v < 10.0, "compact_count({n}) = {s}");
                }
            }
            let s = human_count(n);
            let (v, _) = split(&s);
            assert!(v < 1000.0, "human_count({n}) = {s}");
        }
        for p in 1..5u32 {
            let k = 1024u64.pow(p);
            for n in (k * 9 - 3_000..k * 11)
                .step_by((k / 512) as usize)
                .chain(k.saturating_sub(3_000)..k + 3_000)
            {
                let s = human_size(n);
                let (v, _) = split(&s);
                assert!(v < 1024.0, "human_size({n}) = {s}");
                if s.contains('.') {
                    assert!(v < 10.0, "human_size({n}) = {s}");
                }
            }
        }
    }

    #[test]
    fn a_wide_character_counts_as_two_columns() {
        // The whole column layout was built on chars().count(), so a title
        // of twelve characters could be twenty-four columns wide and push
        // everything after it off the screen.
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本語"), 6, "CJK is two cells per character");
        assert_eq!(width("😀"), 2, "so is an emoji");
        assert_eq!(width(""), 0);
    }

    #[test]
    fn padding_fills_columns_rather_than_characters() {
        for s in ["abc", "日本語", "😀x", "", "Чёрный"] {
            for w in [0usize, 1, 2, 3, 6, 10, 20] {
                let out = pad_fit(s, w);
                assert_eq!(width(&out), w, "pad_fit({s:?}, {w}) = {out:?}");
            }
        }
    }

    #[test]
    fn an_emoji_with_its_selector_is_clipped_to_the_width_it_draws() {
        // ⚠ is one column and ⚠️ -- with U+FE0F after it -- is two.
        for s in [
            "ab\u{26a0}\u{fe0f}cdef",
            &"\u{26a0}\u{fe0f} ".repeat(40),
            "fix \u{2764}\u{fe0f} bug here",
        ] {
            for w in 0..12 {
                assert!(
                    width(&fit(s, w)) <= w,
                    "fit({s:?}, {w}) is {}",
                    width(&fit(s, w))
                );
                assert_eq!(width(&pad_fit(s, w)), w, "pad_fit({s:?}, {w})");
            }
        }
    }

    #[test]
    fn clipping_never_cuts_a_wide_character_in_half() {
        // Half a wide character is not half a column; it is a broken cell.
        for w in 1..12usize {
            let out = fit("日本語です", w);
            assert!(
                width(&out) <= w,
                "fit(.., {w}) = {out:?} is {} wide",
                width(&out)
            );
        }
        assert_eq!(
            fit("日本語", 6),
            "日本語",
            "it fits exactly, so leave it alone"
        );
    }

    #[test]
    fn a_transcript_from_the_future_does_not_break_the_list() {
        // Transcripts arrive from other machines, and not all of their clocks
        // agree. A negative age must clamp, not underflow into nonsense.
        let now = chrono::Utc::now().timestamp();
        for skew in [60_i64, 3600, 86_400, 86_400 * 400] {
            let t = now + skew;
            let r = reltime(t);
            assert!(
                !r.is_empty() && r.chars().count() <= 5,
                "reltime({skew}) = {r:?}"
            );
            let _ = human_dur(-skew);
        }
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
        let h = "/home/u";
        assert_eq!(short_cwd_in("/home/u", h), "~");
        assert_eq!(short_cwd_in("/home/u/proj", h), "~/proj");
        // a path that merely starts with the same letters is left alone
        assert_eq!(short_cwd_in("/home/us2/proj", h), "/home/us2/proj");
        assert_eq!(short_cwd_in("/etc", h), "/etc");
        // and an unknown home abbreviates nothing
        assert_eq!(short_cwd_in("/home/u/proj", ""), "/home/u/proj");
    }

    #[test]
    fn no_test_writes_to_the_process_environment() {
        // Environment variables are shared by every test running at once.
        // One of them setting HOME made an unrelated filter test fail about
        // one run in several, which is the worst kind of failure to chase.
        for f in [
            "app.rs",
            "ui.rs",
            "model.rs",
            "index.rs",
            "scan.rs",
            "search.rs",
            "meta.rs",
            "update.rs",
            "workspace.rs",
            "live.rs",
            "main.rs",
            "preview.rs",
            "config.rs",
            "splash.rs",
            "art.rs",
            "wsx.rs",
        ] {
            let src = std::fs::read_to_string(format!("{}/src/{f}", env!("CARGO_MANIFEST_DIR")))
                .unwrap_or_default();
            // this test names the functions it forbids, so skip its own text
            let body = src
                .split("fn no_test_writes_to_the_process_environment")
                .next()
                .unwrap();
            for bad in ["env::set_var(", "env::remove_var("] {
                assert!(!body.contains(bad), "{f} calls {bad}");
            }
        }
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

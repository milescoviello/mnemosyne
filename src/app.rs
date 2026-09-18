//! Application state, filtering and key handling.
//!
//! Key design rule for this TUI: arrow keys, Enter, Esc and Tab are the
//! documented path and are always shown in the footer, so nothing has to be
//! memorised. Vim motions are wired up as silent aliases alongside them.

use crate::live::LiveMap;
use crate::meta::Meta;
use crate::model::{DateRange, Session, Sort};
use crate::preview::{self, Turn};
use crate::search;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};

/// Which page of the help screen is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpPage {
    Guide,
    Keys,
}

impl HelpPage {
    pub fn next(self) -> HelpPage {
        match self {
            HelpPage::Guide => HelpPage::Keys,
            HelpPage::Keys => HelpPage::Guide,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            HelpPage::Guide => "using it",
            HelpPage::Keys => "every key",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    Normal,
    /// Reading a session's conversation without resuming it.
    Viewer,
    Fuzzy,
    Deep,
    TagAdd,
    TagFilter,
    Note,
    Help,
}

#[derive(Clone, Debug)]
pub enum Row {
    /// Folder heading, used by grouped mode.
    Header(String, usize),
    /// Dim date band ("today", "yesterday", …) used when sorting by recency,
    /// so a 300-row list has some rhythm to scan against.
    Divider(String),
    Item(usize),
    Sub(usize),
}

impl Row {
    /// Rows the cursor is allowed to land on.
    pub fn selectable(&self) -> bool {
        matches!(self, Row::Item(_) | Row::Sub(_))
    }
}

/// Which date band an mtime falls into, relative to local midnight.
fn date_band(mtime: i64) -> &'static str {
    use chrono::{Local, TimeZone};
    let now = Local::now();
    let then = match Local.timestamp_opt(mtime, 0) {
        chrono::offset::LocalResult::Single(t) => t,
        _ => return "earlier",
    };
    let days = (now.date_naive() - then.date_naive()).num_days();
    match days {
        d if d <= 0 => "today",
        1 => "yesterday",
        2..=6 => "this week",
        7..=30 => "this month",
        31..=365 => "this year",
        _ => "older",
    }
}

/// Something a click can trigger. Mouse and keyboard funnel into the same
/// `do_action`, so the two can never drift apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Resume,
    View,
    Tmux,
    NewWindow,
    Filter,
    Search,
    Favorite,
    Tag,
    TagFilter,
    CycleSort,
    GroupByDir,
    CycleDate,
    FavOnly,
    LiveOnly,
    Subagents,
    Preview,
    Clear,
    Help,
    Quit,
}

/// Where things ended up on screen last frame. Rebuilt every draw, because the
/// layout depends on the terminal size and on which panes are showing.
#[derive(Clone, Default)]
pub struct Hits {
    pub list: ratatui::layout::Rect,
    /// Screen row of the footer and of the column headings, so a click is
    /// tested against the row it actually landed on. Without this, a footer
    /// hint would claim every click that shared its x.
    pub footer_y: u16,
    pub colhead_y: u16,
    /// The list's scroll position, needed to turn a screen row into an index.
    pub list_offset: usize,
    /// Column header spans: (x start, x end inclusive, what it sorts by).
    pub columns: Vec<(u16, u16, Sort)>,
    /// Footer hint spans: (x start, x end inclusive, action).
    pub footer: Vec<(u16, u16, Action)>,
}

#[derive(Clone, Debug)]
pub struct ResumeTarget {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub perms: String,
    pub title: String,
}

/// Where a resumed session should land.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// This terminal: the shell cds and reattaches in place.
    Here,
    /// A new terminal emulator window.
    Window,
    /// A tmux session named for this Claude session. If one already exists we
    /// attach to it rather than starting a second client on the same
    /// transcript, so the work continues from its latest state.
    Tmux,
}

impl Target {
    pub fn tag(self) -> &'static str {
        match self {
            Target::Here => "here",
            Target::Window => "window",
            Target::Tmux => "tmux",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Resume {
        targets: Vec<ResumeTarget>,
        target: Target,
    },
}

pub struct DeepResult {
    pub generation: u64,
    pub hits: HashMap<String, String>,
}

pub struct App {
    pub all: Vec<Session>,
    pub view: Vec<Row>,
    pub cursor: usize,
    pub selected: HashSet<String>,
    pub meta: Meta,
    pub live: LiveMap,

    pub input_mode: InputMode,
    pub fuzzy: String,
    pub deep: String,
    pub deep_mode: search::Mode,
    pub deep_hits: Option<HashMap<String, String>>,
    /// Parents of subagents that matched a deep search. Without this a hit
    /// inside a subagent is invisible whenever its parent did not also match,
    /// because only parents appear at the top level.
    deep_parent_hits: HashSet<String>,
    /// The FTS expression behind the current results, if they came from the
    /// index. Excerpts are fetched per row rather than for every hit.
    deep_expr: Option<String>,
    snippet_cache: HashMap<String, String>,
    pub deep_busy: bool,
    pub deep_generation: u64,
    pub input: String,

    pub sort: Sort,
    pub group_by_dir: bool,
    pub date: DateRange,
    pub fav_only: bool,
    pub live_only: bool,
    pub tag_filter: Option<String>,
    pub show_subagents: bool,
    pub show_preview: bool,
    pub expanded: HashSet<String>,

    /// Conversation loaded for the viewer, with a flag for "there was more".
    pub viewer: Option<(Vec<Turn>, bool)>,
    pub viewer_scroll: u16,
    /// Total wrapped height of the viewer, filled in by the renderer so
    /// scrolling can stop at the bottom instead of running off into blank.
    pub viewer_height: u16,
    pub viewer_page: u16,

    /// Every token on this machine, subagents included. Deliberately does not
    /// follow the filters: it is a property of the corpus, not of the view,
    /// and a number that moved while you typed would be hard to read.
    pub corpus_tokens: u64,
    /// What the background update check found, if anything. Shown, never
    /// acted on: the running process keeps the binary it started with.
    pub update_notice: Option<crate::update::Found>,

    pub help_page: HelpPage,
    pub help_scroll: u16,
    /// Filled in by the renderer, so scrolling can stop at the end.
    pub help_height: u16,
    pub help_rows: u16,

    pub status: String,
    pub outcome: Option<Outcome>,
    pub quit: bool,
    /// Mouse reporting steals the terminal's own text selection, so it can be
    /// turned off (key `M`, or --no-mouse) when you want to copy text.
    pub mouse_on: bool,
    pub hits: Hits,
    pub sub_span: (u16, u16),
    /// Set when `M` flipped mouse capture, so main can apply it.
    pub mouse_toggled: bool,
    pub list_state: ratatui::widgets::ListState,
    last_click: Option<(std::time::Instant, usize)>,
    pub restore_model: bool,
    pub want_refresh: bool,

    preview_cache: HashMap<String, Vec<Turn>>,
    matcher: Matcher,
    deep_tx: Sender<DeepResult>,
    pub deep_rx: Receiver<DeepResult>,
}

impl App {
    pub fn new(mut all: Vec<Session>, meta: Meta, live: LiveMap, restore_model: bool) -> App {
        let (deep_tx, deep_rx) = std::sync::mpsc::channel();
        let mut app = App {
            all: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            selected: HashSet::new(),
            meta,
            live,
            input_mode: InputMode::Normal,
            fuzzy: String::new(),
            deep: String::new(),
            deep_mode: search::Mode::Content,
            deep_hits: None,
            deep_parent_hits: HashSet::new(),
            deep_expr: None,
            snippet_cache: HashMap::new(),
            deep_busy: false,
            deep_generation: 0,
            input: String::new(),
            sort: Sort::Recency,
            group_by_dir: false,
            date: DateRange::All,
            fav_only: false,
            live_only: false,
            tag_filter: None,
            show_subagents: false,
            show_preview: true,
            expanded: HashSet::new(),
            corpus_tokens: 0,
            update_notice: None,
            help_page: HelpPage::Guide,
            help_scroll: 0,
            help_height: 0,
            help_rows: 0,
            viewer: None,
            viewer_scroll: 0,
            viewer_height: 0,
            viewer_page: 0,
            status: String::new(),
            outcome: None,
            quit: false,
            mouse_on: true,
            hits: Hits::default(),
            sub_span: (0, 0),
            mouse_toggled: false,
            list_state: ratatui::widgets::ListState::default(),
            last_click: None,
            restore_model,
            want_refresh: false,
            preview_cache: HashMap::new(),
            matcher: Matcher::new(Config::DEFAULT),
            deep_tx,
            deep_rx,
        };
        all.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        app.all = all;
        app.recompute_totals();
        app.apply_overlay();
        app.rebuild();
        app
    }

    /// Sum the corpus token count. Cheap, and only on a reload.
    pub fn recompute_totals(&mut self) {
        self.corpus_tokens = self.all.iter().map(|s| s.total_tokens()).sum();
    }

    /// Fold favourites/tags/notes and live-process state onto the sessions.
    pub fn apply_overlay(&mut self) {
        let tmux = crate::live::tmux_sessions();
        // One stat per distinct directory, not per session: 234 of the
        // sessions here share a single cwd.
        let mut dir_exists: HashMap<String, bool> = HashMap::new();
        for s in &self.all {
            if !s.cwd.is_empty() && !dir_exists.contains_key(&s.cwd) {
                dir_exists.insert(s.cwd.clone(), std::path::Path::new(&s.cwd).is_dir());
            }
        }
        let mut subcount: HashMap<String, u32> = HashMap::new();
        for s in &self.all {
            if s.is_subagent {
                if let Some(p) = &s.parent {
                    *subcount.entry(p.clone()).or_insert(0) += 1;
                }
            }
        }
        for s in &mut self.all {
            if let Some(e) = self.meta.get(&s.id) {
                s.favorite = e.favorite;
                s.tags = e.tags.clone();
                s.note = e.note.clone();
            } else {
                s.favorite = false;
                s.tags.clear();
                s.note.clear();
            }
            s.subagent_count = *subcount.get(&s.id).unwrap_or(&0);
            s.has_tmux = tmux.contains(&crate::live::tmux_name(&s.id));
            s.cwd_missing = !s.cwd.is_empty() && !dir_exists.get(&s.cwd).copied().unwrap_or(true);
            s.live_pid = None;
            s.live_exact = false;
            if let Some(p) = self.live.by_id.get(&s.id) {
                s.live_pid = Some(p.pid);
                s.live_exact = true;
                if s.model.is_empty() {
                    if let Some(m) = &p.model {
                        s.model = m.clone();
                    }
                }
            }
        }
        // cwd-only guesses: attribute to the newest session in that folder
        let mut best: HashMap<String, usize> = HashMap::new();
        for (i, s) in self.all.iter().enumerate() {
            if s.is_subagent || s.cwd.is_empty() || s.live_exact {
                continue;
            }
            match best.get(&s.cwd) {
                Some(&j) if self.all[j].mtime >= s.mtime => {}
                _ => {
                    best.insert(s.cwd.clone(), i);
                }
            }
        }
        for (cwd, p) in &self.live.by_cwd {
            if let Some(&i) = best.get(cwd) {
                if self.all[i].live_pid.is_none() {
                    self.all[i].live_pid = Some(p.pid);
                    self.all[i].live_exact = false;
                }
            }
        }
    }

    fn passes(&mut self, i: usize) -> bool {
        let now = chrono::Utc::now().timestamp();
        let s = &self.all[i];
        if s.is_subagent {
            return false; // shown only as children
        }
        if self.fav_only && !s.favorite {
            return false;
        }
        if self.live_only && !s.is_live() {
            return false;
        }
        if let Some(t) = &self.tag_filter {
            if !s.tags.iter().any(|x| x == t) {
                return false;
            }
        }
        match self.date {
            DateRange::All => {}
            DateRange::Today => {
                if now - s.mtime > 86_400 {
                    return false;
                }
            }
            DateRange::Days(n) => {
                if now - s.mtime > n * 86_400 {
                    return false;
                }
            }
        }
        if let Some(hits) = &self.deep_hits {
            let own = hits.contains_key(&s.path.to_string_lossy().to_string());
            if !own && !self.deep_parent_hits.contains(&s.id) {
                return false;
            }
        }
        if !self.fuzzy.trim().is_empty() {
            // The id is in here so you can paste one from a log or a
            // `--resume` line and land on that session.
            let hay = format!(
                "{} {} {} {} {} {}",
                s.title(),
                crate::model::short_cwd(&s.cwd),
                s.git_branch,
                s.tags.join(" "),
                s.last_prompt,
                s.id
            );
            let pat = Pattern::parse(
                self.fuzzy.trim(),
                CaseMatching::Ignore,
                Normalization::Smart,
            );
            let mut cbuf = Vec::new();
            if pat
                .score(Utf32Str::new(&hay, &mut cbuf), &mut self.matcher)
                .is_none()
            {
                return false;
            }
        }
        true
    }

    fn fuzzy_score(&mut self, i: usize) -> u32 {
        if self.fuzzy.trim().is_empty() {
            return 0;
        }
        let s = &self.all[i];
        let hay = format!("{} {}", s.title(), crate::model::short_cwd(&s.cwd));
        let pat = Pattern::parse(
            self.fuzzy.trim(),
            CaseMatching::Ignore,
            Normalization::Smart,
        );
        let mut cbuf = Vec::new();
        pat.score(Utf32Str::new(&hay, &mut cbuf), &mut self.matcher)
            .unwrap_or(0)
    }

    pub fn rebuild(&mut self) {
        let keep_path = self.current().map(|s| s.path.to_string_lossy().to_string());

        let mut idx: Vec<usize> = Vec::new();
        for i in 0..self.all.len() {
            if self.passes(i) {
                idx.push(i);
            }
        }

        // Favourites always float to the top; then the chosen sort; then a
        // fuzzy-score tiebreak when the user is typing.
        let scores: HashMap<usize, u32> = if self.fuzzy.trim().is_empty() {
            HashMap::new()
        } else {
            idx.iter().map(|&i| (i, self.fuzzy_score(i))).collect()
        };
        let sort = self.sort;
        let all = &self.all;
        idx.sort_by(|&a, &b| {
            let (x, y) = (&all[a], &all[b]);
            y.favorite
                .cmp(&x.favorite)
                .then_with(|| {
                    let sa = scores.get(&a).copied().unwrap_or(0);
                    let sb = scores.get(&b).copied().unwrap_or(0);
                    sb.cmp(&sa)
                })
                .then_with(|| match sort {
                    Sort::Recency => y.mtime.cmp(&x.mtime),
                    Sort::Size => y.size.cmp(&x.size),
                    Sort::Entries => y.entries.cmp(&x.entries),
                    Sort::Duration => y.duration_secs().cmp(&x.duration_secs()),
                    Sort::Title => x.title().to_lowercase().cmp(&y.title().to_lowercase()),
                    Sort::Folder => x.cwd.cmp(&y.cwd).then_with(|| y.mtime.cmp(&x.mtime)),
                    Sort::Tokens => y.total_tokens().cmp(&x.total_tokens()),
                })
        });

        let mut rows: Vec<Row> = Vec::new();
        if self.group_by_dir {
            // Accumulate per directory. Matching only against the previous
            // group would start a new one every time a folder reappears, and
            // the list is in time order, so `~` came out five separate times.
            let mut order: Vec<String> = Vec::new();
            let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
            for &i in &idx {
                let d = crate::model::short_cwd(&self.all[i].cwd);
                if !buckets.contains_key(&d) {
                    order.push(d.clone());
                }
                buckets.entry(d).or_default().push(i);
            }
            let mut by_dir: Vec<(String, Vec<usize>)> = order
                .into_iter()
                .map(|d| {
                    let v = buckets.remove(&d).unwrap_or_default();
                    (d, v)
                })
                .collect();
            // group order: most recently touched folder first
            by_dir.sort_by(|a, b| {
                let am = a.1.iter().map(|&i| self.all[i].mtime).max().unwrap_or(0);
                let bm = b.1.iter().map(|&i| self.all[i].mtime).max().unwrap_or(0);
                bm.cmp(&am)
            });
            for (d, items) in by_dir {
                rows.push(Row::Header(d, items.len()));
                for i in items {
                    rows.push(Row::Item(i));
                    self.push_subs(&mut rows, i);
                }
            }
        } else {
            // Date bands only make sense when the list is in time order.
            let banded = self.sort == Sort::Recency;
            let mut band = "";
            for &i in &idx {
                if banded {
                    let b = date_band(self.all[i].mtime);
                    if b != band {
                        band = b;
                        rows.push(Row::Divider(b.to_string()));
                    }
                }
                rows.push(Row::Item(i));
                self.push_subs(&mut rows, i);
            }
        }

        self.view = rows;
        // keep the cursor on the same session across a rebuild where possible
        self.cursor = 0;
        if let Some(p) = keep_path {
            if let Some(pos) = self.view.iter().position(|r| match r {
                Row::Item(i) | Row::Sub(i) => self.all[*i].path.to_string_lossy() == p.as_str(),
                _ => false,
            }) {
                self.cursor = pos;
            }
        }
        self.ensure_on_item(1);
    }

    fn push_subs(&self, rows: &mut Vec<Row>, parent_idx: usize) {
        if !self.show_subagents {
            return;
        }
        let pid = &self.all[parent_idx].id;
        if !self.expanded.contains(pid) {
            return;
        }
        let mut kids: Vec<usize> = (0..self.all.len())
            .filter(|&j| {
                self.all[j].is_subagent && self.all[j].parent.as_deref() == Some(pid.as_str())
            })
            // During a deep search, show the children that matched rather than
            // burying them among twenty-seven that did not.
            .filter(|&j| match &self.deep_hits {
                Some(hits) => hits.contains_key(&self.all[j].path.to_string_lossy().to_string()),
                None => true,
            })
            .collect();
        kids.sort_by(|&a, &b| self.all[a].mtime.cmp(&self.all[b].mtime));
        for k in kids {
            rows.push(Row::Sub(k));
        }
    }

    /// Land the cursor on a real row, searching first in `dir` then the other
    /// way, so grouped mode can never park the cursor on a folder header.
    fn ensure_on_item(&mut self, dir: i32) {
        let n = self.view.len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        if self.cursor >= n {
            self.cursor = n - 1;
        }
        let is_item = |r: &Row| r.selectable();
        if is_item(&self.view[self.cursor]) {
            return;
        }
        let step = if dir >= 0 { 1i64 } else { -1i64 };
        for &s in &[step, -step] {
            let mut c = self.cursor as i64;
            loop {
                c += s;
                if c < 0 || c >= n as i64 {
                    break;
                }
                if is_item(&self.view[c as usize]) {
                    self.cursor = c as usize;
                    return;
                }
            }
        }
    }

    pub fn current_idx(&self) -> Option<usize> {
        match self.view.get(self.cursor)? {
            Row::Item(i) | Row::Sub(i) => Some(*i),
            _ => None,
        }
    }

    pub fn current(&self) -> Option<&Session> {
        self.current_idx().map(|i| &self.all[i])
    }

    pub fn item_count(&self) -> usize {
        self.view.iter().filter(|r| r.selectable()).count()
    }

    pub fn preview(&mut self, want: usize) -> Vec<Turn> {
        let Some(i) = self.current_idx() else {
            return Vec::new();
        };
        let key = self.all[i].path.to_string_lossy().to_string();
        if let Some(v) = self.preview_cache.get(&key) {
            return v.clone();
        }
        let turns = preview::tail_turns(&self.all[i], want);
        self.preview_cache.insert(key, turns.clone());
        turns
    }

    /// Excerpt for the row under the cursor.
    ///
    /// Indexed results arrive without excerpts on purpose, so this fetches the
    /// one you are looking at and remembers it.
    pub fn deep_snippet(&mut self) -> Option<String> {
        let path = self.current()?.path.to_string_lossy().to_string();
        let stored = self.deep_hits.as_ref()?.get(&path)?.clone();
        if !stored.is_empty() {
            return Some(stored);
        }
        if let Some(hit) = self.snippet_cache.get(&path) {
            return Some(hit.clone());
        }
        let expr = self.deep_expr.clone()?;
        let text = crate::index::Index::open()
            .ok()
            .and_then(|i| i.snippet_for(&path, &expr))?;
        self.snippet_cache.insert(path, text.clone());
        Some(text)
    }

    // ---------------- movement ----------------

    fn move_by(&mut self, delta: i32) {
        if self.view.is_empty() {
            return;
        }
        let n = self.view.len() as i32;
        let mut c = self.cursor as i32;
        let step = delta.signum();
        let mut remaining = delta.abs();
        while remaining > 0 {
            let mut next = c + step;
            // skip headers
            while next >= 0 && next < n && !self.view[next as usize].selectable() {
                next += step;
            }
            if next < 0 || next >= n {
                break;
            }
            c = next;
            remaining -= 1;
        }
        self.cursor = c.clamp(0, n - 1) as usize;
        self.ensure_on_item(if step >= 0 { 1 } else { -1 });
    }

    fn goto_top(&mut self) {
        self.cursor = 0;
        self.ensure_on_item(1);
    }

    fn goto_bottom(&mut self) {
        if !self.view.is_empty() {
            self.cursor = self.view.len() - 1;
        }
        self.ensure_on_item(-1);
    }

    // ---------------- actions ----------------

    fn toggle_favorite(&mut self) {
        let Some(i) = self.current_idx() else { return };
        let id = self.all[i].id.clone();
        let now = self.meta.toggle_favorite(&id);
        let _ = self.meta.save();
        self.all[i].favorite = now;
        self.status = if now {
            "★ favourited".into()
        } else {
            "unfavourited".into()
        };
        self.rebuild();
    }

    fn toggle_select(&mut self) {
        let Some(i) = self.current_idx() else { return };
        let key = self.all[i].path.to_string_lossy().to_string();
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
        self.move_by(1);
    }

    fn toggle_expand(&mut self, open: bool) {
        let Some(i) = self.current_idx() else { return };
        if self.all[i].is_subagent {
            return;
        }
        if self.all[i].subagent_count == 0 {
            self.status = "no subagents in this session".into();
            return;
        }
        if !self.show_subagents {
            self.show_subagents = true;
        }
        let id = self.all[i].id.clone();
        if open {
            self.expanded.insert(id);
        } else {
            self.expanded.remove(&id);
        }
        self.rebuild();
    }

    pub fn targets(&self) -> Vec<ResumeTarget> {
        let mk = |s: &Session| ResumeTarget {
            id: if s.is_subagent {
                s.parent.clone().unwrap_or_else(|| s.id.clone())
            } else {
                s.id.clone()
            },
            cwd: s.cwd.clone(),
            model: if self.restore_model {
                s.model.clone()
            } else {
                String::new()
            },
            perms: s.permission_mode.clone(),
            title: s.title().to_string(),
        };
        if !self.selected.is_empty() {
            let mut out = Vec::new();
            for r in &self.view {
                if let Row::Item(i) | Row::Sub(i) = r {
                    let key = self.all[*i].path.to_string_lossy().to_string();
                    if self.selected.contains(&key) {
                        out.push(mk(&self.all[*i]));
                    }
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        self.current().map(mk).into_iter().collect()
    }

    fn resume(&mut self, target: Target) {
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        // Guard against silently starting a second client on a transcript that
        // already has one. Tmux is exempt: attaching to the existing session is
        // exactly the right move there, and is what "resume" should mean.
        if target == Target::Here {
            if let Some(s) = self.current() {
                // A waiting tmux session is as strong a signal as an exact pid
                // match: resuming here would fork a second client instead of
                // picking up where that one left off.
                if (s.live_exact || s.has_tmux) && self.selected.is_empty() {
                    self.status = if s.has_tmux {
                        format!(
                            "{} is already running in tmux — ctrl+t attaches to it",
                            crate::live::tmux_name(&s.id)
                        )
                    } else {
                        format!(
                            "already running as pid {} — ctrl+n opens another window anyway",
                            s.live_pid.unwrap_or(0)
                        )
                    };
                    return;
                }
            }
        }
        self.outcome = Some(Outcome::Resume { targets, target });
        self.quit = true;
    }

    fn start_deep(&mut self) {
        let q = self.deep.trim().to_string();
        if q.is_empty() {
            self.deep_hits = None;
            self.deep_parent_hits.clear();
            self.deep_busy = false;
            self.rebuild();
            return;
        }
        self.deep_generation += 1;
        let generation = self.deep_generation;
        let mode = self.deep_mode;
        let tx = self.deep_tx.clone();
        // Always search subagent transcripts. They are the majority of the
        // corpus, and excluding them by default meant the answer could sit in
        // a file the search never opened.
        let sessions: Vec<Session> = self.all.clone();
        self.deep_expr = if mode == search::Mode::Content {
            let e = search::fts_expr(&q);
            if e.is_empty() {
                None
            } else {
                Some(e)
            }
        } else {
            None
        };
        self.snippet_cache.clear();
        self.deep_busy = true;
        std::thread::spawn(move || {
            let (hits, _how) = search::run(&sessions, &q, mode);
            let _ = tx.send(DeepResult { generation, hits });
        });
    }

    pub fn absorb_deep(&mut self) {
        let mut newest: Option<DeepResult> = None;
        while let Ok(r) = self.deep_rx.try_recv() {
            if r.generation == self.deep_generation {
                newest = Some(r);
            }
        }
        if let Some(r) = newest {
            let n = r.hits.len();
            // If the answer was inside a subagent, show it rather than hiding
            // the match behind a collapsed parent.
            let mut reveal: HashSet<String> = HashSet::new();
            for s in &self.all {
                if s.is_subagent && r.hits.contains_key(&s.path.to_string_lossy().to_string()) {
                    if let Some(p) = &s.parent {
                        reveal.insert(p.clone());
                    }
                }
            }
            self.deep_parent_hits = reveal.clone();
            if !reveal.is_empty() {
                self.show_subagents = true;
                self.expanded.extend(reveal);
            }
            self.deep_hits = Some(r.hits);
            self.deep_busy = false;
            self.status = format!(
                "{n} session(s) match “{}” in {}",
                self.deep,
                self.deep_mode.label()
            );
            self.rebuild();
        }
    }

    /// Session ids the next tag operation applies to: the whole selection if
    /// there is one, otherwise just the row under the cursor.
    fn tag_targets(&self) -> Vec<String> {
        if !self.selected.is_empty() {
            return self
                .all
                .iter()
                .filter(|s| {
                    self.selected
                        .contains(&s.path.to_string_lossy().to_string())
                })
                .map(|s| s.id.clone())
                .collect();
        }
        self.current().map(|s| s.id.clone()).into_iter().collect()
    }

    fn commit_tag_add(&mut self) {
        let raw = self.input.trim().to_string();

        // `old>new` renames a tag everywhere rather than tagging anything.
        if let Some((from, to)) = raw.split_once('>') {
            let n = self.meta.rename_tag(from, to);
            let _ = self.meta.save();
            if self.tag_filter.as_deref() == Some(crate::meta::normalize_tag(from).as_str()) {
                self.tag_filter = Some(crate::meta::normalize_tag(to));
            }
            self.status = format!(
                "renamed #{} to #{} on {n} session(s)",
                from.trim(),
                to.trim()
            );
            self.input.clear();
            self.input_mode = InputMode::Normal;
            self.apply_overlay();
            self.rebuild();
            return;
        }

        let targets = self.tag_targets();
        if targets.is_empty() {
            self.input.clear();
            self.input_mode = InputMode::Normal;
            return;
        }
        let many = targets.len();
        if let Some(t) = raw.strip_prefix('-') {
            for id in &targets {
                self.meta.remove_tag(id, t);
            }
            self.status = format!("removed #{t} from {many} session(s)");
        } else if !raw.is_empty() {
            for id in &targets {
                for t in raw.split(|c: char| c == ',' || c.is_whitespace()) {
                    if !t.is_empty() {
                        self.meta.add_tag(id, t);
                    }
                }
            }
            self.status = if many == 1 {
                format!("tagged {raw}")
            } else {
                format!("tagged {many} sessions {raw}")
            };
        }
        let _ = self.meta.save();
        self.input.clear();
        self.input_mode = InputMode::Normal;
        self.apply_overlay();
        self.rebuild();
    }

    fn commit_note(&mut self) {
        let Some(i) = self.current_idx() else { return };
        let id = self.all[i].id.clone();
        self.meta.set_note(&id, &self.input);
        let _ = self.meta.save();
        self.all[i].note = self
            .meta
            .get(&id)
            .map(|e| e.note.clone())
            .unwrap_or_default();
        self.status = "note saved".into();
        self.input.clear();
        self.input_mode = InputMode::Normal;
    }

    fn commit_tag_filter(&mut self) {
        let t = crate::meta::normalize_tag(&self.input);
        self.tag_filter = if t.is_empty() { None } else { Some(t) };
        self.status = match &self.tag_filter {
            Some(t) => format!("filtering tag #{t}"),
            None => "tag filter cleared".into(),
        };
        self.input.clear();
        self.input_mode = InputMode::Normal;
        self.rebuild();
    }

    /// Tag completions for the current input prefix.
    pub fn tag_completions(&self) -> Vec<String> {
        let pfx = crate::meta::normalize_tag(&self.input);
        self.meta
            .all_tags()
            .into_iter()
            .filter(|(t, _)| pfx.is_empty() || t.starts_with(&pfx))
            .take(8)
            .map(|(t, n)| format!("{t} ({n})"))
            .collect()
    }

    /// Every command the interface offers, in one place. Keys and mouse clicks
    /// both come through here.
    pub fn do_action(&mut self, a: Action) {
        match a {
            Action::Resume => self.resume(Target::Here),
            Action::View => self.open_viewer(),
            Action::Tmux => self.resume(Target::Tmux),
            Action::NewWindow => self.resume(Target::Window),
            Action::Filter => self.input_mode = InputMode::Fuzzy,
            Action::Search => self.input_mode = InputMode::Deep,
            Action::Favorite => self.toggle_favorite(),
            Action::Tag => {
                self.input.clear();
                self.input_mode = InputMode::TagAdd;
            }
            Action::TagFilter => {
                self.input = self.tag_filter.clone().unwrap_or_default();
                self.input_mode = InputMode::TagFilter;
            }
            Action::CycleSort => {
                self.sort = self.sort.next();
                self.status = format!("sort: {}", self.sort.label());
                self.rebuild();
                self.goto_top();
            }
            Action::GroupByDir => {
                self.group_by_dir = !self.group_by_dir;
                self.status = if self.group_by_dir {
                    "grouped by directory".into()
                } else {
                    "flat list".into()
                };
                self.rebuild();
            }
            Action::CycleDate => {
                self.date = self.date.next();
                self.status = format!("dates: {}", self.date.label());
                self.rebuild();
            }
            Action::FavOnly => {
                self.fav_only = !self.fav_only;
                self.status = if self.fav_only {
                    "favourites only".into()
                } else {
                    "all sessions".into()
                };
                self.rebuild();
            }
            Action::LiveOnly => {
                if !self.live.supported {
                    // Better to say why than to show an empty list.
                    self.status = "which sessions are running can only be detected on Linux".into();
                    return;
                }
                self.live_only = !self.live_only;
                self.status = if self.live_only {
                    "running only".into()
                } else {
                    "all sessions".into()
                };
                self.rebuild();
            }
            Action::Subagents => {
                self.show_subagents = !self.show_subagents;
                self.status = if self.show_subagents {
                    "subagents shown — click ⌁ or press → to expand".into()
                } else {
                    "subagents hidden".into()
                };
                self.rebuild();
            }
            Action::Preview => {
                self.show_preview = !self.show_preview;
                self.status = if self.show_preview {
                    "preview on".into()
                } else {
                    "preview off".into()
                };
            }
            Action::Clear => {
                self.selected.clear();
                self.fuzzy.clear();
                self.deep.clear();
                self.deep_hits = None;
                self.deep_parent_hits.clear();
                self.tag_filter = None;
                self.fav_only = false;
                self.live_only = false;
                self.date = DateRange::All;
                self.status = "filters cleared".into();
                self.rebuild();
            }
            Action::Help => {
                self.help_page = HelpPage::Guide;
                self.help_scroll = 0;
                self.input_mode = InputMode::Help;
            }
            Action::Quit => self.quit = true,
        }
    }

    /// Load the current session's conversation for reading.
    fn open_viewer(&mut self) {
        let Some(i) = self.current_idx() else { return };
        // 8 MB covers almost every session whole; the biggest here is 400 MB,
        // where the recent end is what you want anyway.
        let (turns, more) = preview::load_turns(&self.all[i], 8 << 20, 400);
        if turns.is_empty() {
            self.status = "nothing readable in this transcript".into();
            return;
        }
        self.viewer = Some((turns, more));
        self.viewer_scroll = u16::MAX; // start at the end, then clamp on draw
        self.input_mode = InputMode::Viewer;
    }

    fn help_scroll_by(&mut self, delta: i32) {
        let max = self.help_height.saturating_sub(self.help_rows.max(1));
        let next = self.help_scroll as i32 + delta;
        self.help_scroll = next.clamp(0, max as i32) as u16;
    }

    fn viewer_scroll_by(&mut self, delta: i32) {
        let max = self.viewer_height.saturating_sub(self.viewer_page.max(1));
        let next = self.viewer_scroll as i32 + delta;
        self.viewer_scroll = next.clamp(0, max as i32) as u16;
    }

    /// Set a specific sort (clicking a column header, rather than cycling).
    pub fn set_sort(&mut self, s: Sort) {
        self.sort = s;
        self.status = format!("sort: {}", s.label());
        self.rebuild();
        self.goto_top();
    }

    // ---------------- mouse ----------------

    pub fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};

        if self.input_mode == InputMode::Viewer {
            match m.kind {
                MouseEventKind::ScrollUp => self.viewer_scroll_by(-3),
                MouseEventKind::ScrollDown => self.viewer_scroll_by(3),
                MouseEventKind::Down(_) => {
                    self.viewer = None;
                    self.input_mode = InputMode::Normal;
                }
                _ => {}
            }
            return;
        }
        if self.input_mode == InputMode::Help {
            match m.kind {
                MouseEventKind::ScrollUp => self.help_scroll_by(-3),
                MouseEventKind::ScrollDown => self.help_scroll_by(3),
                MouseEventKind::Down(_) => {
                    self.help_scroll = 0;
                    self.input_mode = InputMode::Normal;
                }
                _ => {}
            }
            return;
        }

        match m.kind {
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row, false),
            MouseEventKind::Down(MouseButton::Right) => self.click(m.column, m.row, true),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, right: bool) {
        // a column heading: sort by it
        if y == self.hits.colhead_y {
            for (x0, x1, sort) in self.hits.columns.clone() {
                if x >= x0 && x <= x1 {
                    self.set_sort(sort);
                    return;
                }
            }
            return;
        }
        // a footer hint: run it
        if y == self.hits.footer_y {
            for (x0, x1, action) in self.hits.footer.clone() {
                if x >= x0 && x <= x1 {
                    self.do_action(action);
                    return;
                }
            }
            return;
        }
        // a list row
        let l = self.hits.list;
        if y >= l.y && y < l.y + l.height && x >= l.x && x < l.x + l.width {
            let idx = self.hits.list_offset + (y - l.y) as usize;
            if idx >= self.view.len() {
                return;
            }
            if !self.view[idx].selectable() {
                return; // a date band or folder heading
            }
            let same_row = self.cursor == idx;
            self.cursor = idx;
            self.status.clear();

            if right {
                self.toggle_favorite();
                return;
            }
            // Clicking the ⌁ count expands that session's subagents.
            if let Some(i) = self.current_idx() {
                if self.all[i].subagent_count > 0 && !self.all[i].is_subagent {
                    let (sx0, sx1) = self.hits_sub_span();
                    if x >= sx0 && x <= sx1 {
                        let open = self.expanded.contains(&self.all[i].id);
                        self.toggle_expand(!open);
                        return;
                    }
                }
            }
            // Second click on the same row within the double-click window
            // resumes it; a first click just moves the cursor.
            let now = std::time::Instant::now();
            let dbl = matches!(self.last_click, Some((t, r))
                if r == idx && same_row && now.duration_since(t).as_millis() < 450);
            self.last_click = Some((now, idx));
            if dbl {
                self.last_click = None;
                self.do_action(Action::Resume);
            }
        }
    }

    /// x-range of the subagent-count cell, filled in by the renderer.
    fn hits_sub_span(&self) -> (u16, u16) {
        self.sub_span
    }

    // ---------------- key handling ----------------

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);

        if ctrl && matches!(k.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }

        // ---- text entry modes ----
        match self.input_mode {
            InputMode::Normal => {}
            InputMode::Help => {
                // Paged and scrollable, so it can say more than a key list.
                match k.code {
                    KeyCode::Tab
                    | KeyCode::Right
                    | KeyCode::Left
                    | KeyCode::Char('l')
                    | KeyCode::Char('h') => {
                        self.help_page = self.help_page.next();
                        self.help_scroll = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => self.help_scroll_by(1),
                    KeyCode::Up | KeyCode::Char('k') => self.help_scroll_by(-1),
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        self.help_scroll_by(self.help_rows as i32)
                    }
                    KeyCode::PageUp => self.help_scroll_by(-(self.help_rows as i32)),
                    KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => self.help_scroll_by(i32::MAX / 2),
                    _ => {
                        self.help_scroll = 0;
                        self.input_mode = InputMode::Normal;
                    }
                }
                return;
            }
            InputMode::Viewer => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => {
                        self.viewer = None;
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Up | KeyCode::Char('k') => self.viewer_scroll_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.viewer_scroll_by(1),
                    KeyCode::PageUp => self.viewer_scroll_by(-(self.viewer_page as i32)),
                    KeyCode::PageDown => self.viewer_scroll_by(self.viewer_page as i32),
                    KeyCode::Char('u') if ctrl => {
                        self.viewer_scroll_by(-(self.viewer_page as i32) / 2)
                    }
                    KeyCode::Char('d') if ctrl => {
                        self.viewer_scroll_by(self.viewer_page as i32 / 2)
                    }
                    KeyCode::Home | KeyCode::Char('g') => self.viewer_scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => self.viewer_scroll_by(i32::MAX / 2),
                    KeyCode::Enter => {
                        self.viewer = None;
                        self.input_mode = InputMode::Normal;
                        self.do_action(Action::Resume);
                    }
                    _ => {}
                }
                return;
            }
            InputMode::Fuzzy => {
                match k.code {
                    KeyCode::Esc => {
                        self.fuzzy.clear();
                        self.input_mode = InputMode::Normal;
                        self.rebuild();
                    }
                    KeyCode::Enter => self.input_mode = InputMode::Normal,
                    KeyCode::Backspace => {
                        self.fuzzy.pop();
                        self.rebuild();
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    KeyCode::Char(c) if !ctrl => {
                        self.fuzzy.push(c);
                        self.rebuild();
                    }
                    _ => {}
                }
                return;
            }
            InputMode::Deep => {
                match k.code {
                    KeyCode::Esc => {
                        self.deep.clear();
                        self.deep_hits = None;
                        self.deep_parent_hits.clear();
                        self.deep_busy = false;
                        self.input_mode = InputMode::Normal;
                        self.rebuild();
                    }
                    KeyCode::Enter => {
                        self.start_deep();
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Tab => {
                        self.deep_mode = self.deep_mode.next();
                    }
                    KeyCode::Backspace => {
                        self.deep.pop();
                    }
                    KeyCode::Char(c) if !ctrl => self.deep.push(c),
                    _ => {}
                }
                return;
            }
            InputMode::TagAdd | InputMode::TagFilter | InputMode::Note => {
                match k.code {
                    KeyCode::Esc => {
                        self.input.clear();
                        self.input_mode = InputMode::Normal;
                    }
                    KeyCode::Enter => match self.input_mode {
                        InputMode::TagAdd => self.commit_tag_add(),
                        InputMode::TagFilter => self.commit_tag_filter(),
                        InputMode::Note => self.commit_note(),
                        _ => {}
                    },
                    KeyCode::Tab => {
                        if let Some(first) = self.tag_completions().first() {
                            if let Some(name) = first.split(' ').next() {
                                self.input = name.to_string();
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char(c) if !ctrl => self.input.push(c),
                    _ => {}
                }
                return;
            }
        }

        // ---- normal mode ----
        self.status.clear();
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.do_action(Action::Quit),

            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Char('d') if ctrl => self.move_by(10),
            KeyCode::Char('u') if ctrl => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.goto_top(),
            KeyCode::End | KeyCode::Char('G') => self.goto_bottom(),

            KeyCode::Enter => self.do_action(if alt {
                Action::NewWindow
            } else {
                Action::Resume
            }),
            KeyCode::Char('n') if ctrl => self.do_action(Action::NewWindow),
            KeyCode::Char('t') if ctrl => self.do_action(Action::Tmux),

            KeyCode::Char(' ') => self.toggle_select(),
            KeyCode::Char('f') if ctrl => self.do_action(Action::Search),
            KeyCode::Char('f') => self.do_action(Action::Favorite),
            KeyCode::Char('t') => self.do_action(Action::Tag),
            KeyCode::Char('T') => self.do_action(Action::TagFilter),
            KeyCode::Char('N') => {
                self.input = self.current().map(|s| s.note.clone()).unwrap_or_default();
                self.input_mode = InputMode::Note;
            }
            KeyCode::Char('/') => self.do_action(Action::Filter),
            KeyCode::Char('F') => self.do_action(Action::Search),
            KeyCode::Char('m') => {
                self.deep_mode = self.deep_mode.next();
                if !self.deep.trim().is_empty() {
                    self.start_deep();
                }
                self.status = format!("search mode: {}", self.deep_mode.label());
            }

            KeyCode::Char('s') => self.do_action(Action::CycleSort),
            KeyCode::Char('o') => self.do_action(Action::GroupByDir),
            KeyCode::Char('D') => self.do_action(Action::CycleDate),
            KeyCode::Char('*') => self.do_action(Action::FavOnly),
            KeyCode::Char('L') => self.do_action(Action::LiveOnly),
            KeyCode::Char('a') => self.do_action(Action::Subagents),
            KeyCode::Char('p') => self.do_action(Action::Preview),
            KeyCode::Char('v') => self.do_action(Action::View),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.toggle_expand(true),
            KeyCode::Left | KeyCode::Char('h') => self.toggle_expand(false),

            KeyCode::Char('M') => {
                self.mouse_on = !self.mouse_on;
                self.mouse_toggled = true;
                self.status = if self.mouse_on {
                    "mouse on".into()
                } else {
                    "mouse off — terminal text selection works again".into()
                };
            }
            KeyCode::Char('c') => self.do_action(Action::Clear),
            KeyCode::Char('R') | KeyCode::F(5) => {
                self.want_refresh = true;
                self.status = "reindexing…".into();
            }
            KeyCode::Char('?') => self.do_action(Action::Help),
            _ => {}
        }
    }
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use crate::live::LiveMap;
    use crate::model::Session;
    use std::path::PathBuf;

    pub fn session(id: &str, title: &str, cwd: &str, age_days: i64) -> Session {
        let now = chrono::Utc::now().timestamp();
        Session {
            id: id.into(),
            path: PathBuf::from(format!("/p/{id}.jsonl")),
            cwd: cwd.into(),
            ai_title: title.into(),
            last_prompt: format!("last thing said in {title}"),
            model: "claude-opus-5".into(),
            permission_mode: "bypassPermissions".into(),
            size: 1_000 * (age_days as u64 + 1),
            mtime: now - age_days * 86_400,
            first_ts: now - age_days * 86_400 - 600,
            last_ts: now - age_days * 86_400,
            entries: 10 * (age_days as u32 + 1),
            user_msgs: 5,
            assistant_msgs: 5,
            in_tokens: 1_000,
            out_tokens: 2_000,
            cache_read: 3_000_000,
            ..Default::default()
        }
    }

    pub fn subagent(id: &str, parent: &str) -> Session {
        let mut s = session(id, "", "/home/u/proj", 1);
        s.is_subagent = true;
        s.parent = Some(parent.into());
        s.first_prompt = format!("subagent work for {parent}");
        s
    }

    /// A corpus with a bit of everything the interface has to cope with.
    pub fn corpus() -> Vec<Session> {
        let mut v = vec![
            session("aaaaaaaa-1", "today's work", "/home/u", 0),
            session("bbbbbbbb-2", "yesterday's thing", "/home/u/proj", 1),
            session("cccccccc-3", "last week", "/home/u/proj", 5),
            session("dddddddd-4", "last month", "/home/u/other", 20),
            session("eeeeeeee-5", "ancient", "/home/u", 300),
            session("ffffffff-6", "", "", 2), // no title, no cwd
        ];
        v[2].tags = vec!["eft".into()];
        v[3].cwd_missing = true;
        v.push(subagent("agent-a1", "aaaaaaaa-1"));
        v.push(subagent("agent-a2", "aaaaaaaa-1"));
        v
    }

    pub fn app() -> App {
        // Favourites and tags belong to the overlay, not the scanned session:
        // apply_overlay rewrites those fields from Meta every time.
        let mut meta = crate::meta::Meta::default();
        meta.toggle_favorite("aaaaaaaa-1");
        meta.add_tag("cccccccc-3", "eft");
        app_with(meta, true)
    }

    pub fn app_with(meta: crate::meta::Meta, restore_model: bool) -> App {
        let live = LiveMap {
            by_id: Default::default(),
            by_cwd: Default::default(),
            count: 0,
            supported: true,
        };
        App::new(corpus(), meta, live, restore_model)
    }
}

#[cfg(test)]
mod logic_tests {
    use super::fixtures::*;
    use super::*;

    /// The cursor must always be on something you can act on.
    fn assert_cursor_valid(a: &App) {
        if a.item_count() == 0 {
            return;
        }
        let row = a.view.get(a.cursor);
        assert!(
            row.map(|r| r.selectable()).unwrap_or(false),
            "cursor landed on {:?} (index {} of {})",
            row,
            a.cursor,
            a.view.len()
        );
    }

    #[test]
    fn subagents_are_children_not_entries() {
        let a = app();
        assert_eq!(a.item_count(), 6, "the two subagents are not top-level");
        assert_cursor_valid(&a);
    }

    #[test]
    fn favourites_float_to_the_top() {
        let a = app();
        let first = a.current().unwrap();
        assert!(first.favorite, "got {:?}", first.title());
    }

    #[test]
    fn every_sort_leaves_the_cursor_somewhere_valid() {
        let mut a = app();
        for _ in 0..8 {
            a.do_action(Action::CycleSort);
            assert_cursor_valid(&a);
            assert!(a.item_count() > 0);
        }
    }

    #[test]
    fn sorting_by_tokens_orders_by_tokens() {
        let mut a = app();
        a.all[2].cache_read = 9_000_000_000;
        a.set_sort(Sort::Tokens);
        // favourites still float, so check the rest is descending
        let got: Vec<u64> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(a.all[*i].total_tokens()),
                _ => None,
            })
            .skip(1)
            .collect();
        let mut sorted = got.clone();
        sorted.sort_unstable_by(|x, y| y.cmp(x));
        assert_eq!(got, sorted);
    }

    #[test]
    fn date_bands_appear_only_under_recency() {
        let mut a = app();
        let bands = |a: &App| {
            a.view
                .iter()
                .filter(|r| matches!(r, Row::Divider(_)))
                .count()
        };
        assert!(bands(&a) > 1, "recency view is banded");
        a.set_sort(Sort::Size);
        assert_eq!(bands(&a), 0, "bands mean nothing outside time order");
        a.do_action(Action::GroupByDir);
        assert_eq!(bands(&a), 0, "grouped mode has its own headings");
    }

    #[test]
    fn grouping_makes_one_heading_per_directory() {
        let mut a = app();
        a.do_action(Action::GroupByDir);
        let mut heads: Vec<String> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Header(d, _) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let before = heads.len();
        heads.sort();
        heads.dedup();
        assert_eq!(before, heads.len(), "a folder appeared twice: {heads:?}");
        assert_cursor_valid(&a);
    }

    #[test]
    fn filtering_narrows_and_clearing_restores() {
        let mut a = app();
        let all = a.item_count();
        a.fuzzy = "ancient".into();
        a.rebuild();
        assert_eq!(a.item_count(), 1);
        assert_cursor_valid(&a);
        a.do_action(Action::Clear);
        assert_eq!(a.item_count(), all);
    }

    #[test]
    fn a_filter_matching_nothing_is_survivable() {
        let mut a = app();
        a.fuzzy = "zzzzzzzzzzzz-no-such-thing".into();
        a.rebuild();
        assert_eq!(a.item_count(), 0);
        assert!(a.current().is_none());
        // and none of these may panic with an empty view
        a.do_action(Action::CycleSort);
        a.do_action(Action::Favorite);
        a.do_action(Action::Resume);
        a.preview(3);
    }

    #[test]
    fn sessions_are_findable_by_id() {
        let mut a = app();
        a.fuzzy = "eeeeeeee".into();
        a.rebuild();
        assert_eq!(a.item_count(), 1);
        assert_eq!(a.current().unwrap().title(), "ancient");
    }

    #[test]
    fn favourites_only_shows_just_those() {
        let mut a = app();
        a.do_action(Action::FavOnly);
        assert_eq!(a.item_count(), 1);
        assert!(a.current().unwrap().favorite);
    }

    #[test]
    fn date_range_excludes_the_old() {
        let mut a = app();
        a.date = DateRange::Days(7);
        a.rebuild();
        let titles: Vec<&str> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(a.all[*i].title()),
                _ => None,
            })
            .collect();
        assert!(!titles.contains(&"ancient"), "{titles:?}");
        assert!(titles.contains(&"today's work"));
    }

    #[test]
    fn expanding_reveals_subagents_and_collapsing_hides_them() {
        let mut a = app();
        a.do_action(Action::Subagents);
        let subs = |a: &App| a.view.iter().filter(|r| matches!(r, Row::Sub(_))).count();
        assert_eq!(subs(&a), 0, "collapsed to begin with");
        a.expanded.insert("aaaaaaaa-1".into());
        a.rebuild();
        assert_eq!(subs(&a), 2);
        assert_cursor_valid(&a);
        a.expanded.clear();
        a.rebuild();
        assert_eq!(subs(&a), 0);
    }

    #[test]
    fn tagging_applies_to_a_selection_and_not_beyond_it() {
        let mut a = app();
        let first = a.all[0].path.to_string_lossy().to_string();
        let second = a.all[1].path.to_string_lossy().to_string();
        a.selected.insert(first);
        a.selected.insert(second);
        assert_eq!(a.tag_targets().len(), 2);
        a.selected.clear();
        assert_eq!(
            a.tag_targets().len(),
            1,
            "no selection means the cursor row"
        );
    }

    #[test]
    fn resuming_something_already_running_is_refused() {
        let mut a = app();
        a.all[0].live_exact = true;
        a.all[0].live_pid = Some(4242);
        a.apply_overlay();
        a.all[0].live_exact = true;
        a.all[0].live_pid = Some(4242);
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(0)))
            .unwrap_or(0);
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none(), "should not have resumed");
        assert!(a.status.contains("4242"), "status was {:?}", a.status);
    }

    #[test]
    fn a_tmux_session_redirects_you_to_ctrl_t() {
        let mut a = app();
        a.all[0].has_tmux = true;
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(0)))
            .unwrap_or(0);
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none());
        assert!(a.status.contains("ctrl+t"), "status was {:?}", a.status);
    }

    #[test]
    fn resuming_in_tmux_is_never_refused() {
        let mut a = app();
        a.all[0].has_tmux = true;
        a.all[0].live_exact = true;
        a.do_action(Action::Tmux);
        assert!(
            a.outcome.is_some(),
            "tmux attach is the right move, not a clash"
        );
    }

    #[test]
    fn a_resume_target_carries_what_the_shell_needs() {
        let mut a = app();
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("no outcome");
        };
        let t = &targets[0];
        assert!(!t.id.is_empty());
        assert_eq!(t.model, "claude-opus-5");
        assert_eq!(t.perms, "bypassPermissions");
    }

    #[test]
    fn no_model_restoration_means_no_model_flag() {
        let mut a = app_with(crate::meta::Meta::default(), false);
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("no outcome");
        };
        assert!(targets[0].model.is_empty());
    }

    #[test]
    fn moving_stays_in_bounds_however_hard_you_push() {
        let mut a = app();
        for _ in 0..200 {
            a.on_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Down,
            ));
        }
        assert_cursor_valid(&a);
        for _ in 0..200 {
            a.on_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Up,
            ));
        }
        assert_cursor_valid(&a);
        let first_selectable = a.view.iter().position(|r| r.selectable()).unwrap();
        assert_eq!(
            a.cursor, first_selectable,
            "the top of the list, skipping the date band above it"
        );
    }

    #[test]
    fn corpus_tokens_counts_everything_including_subagents() {
        let a = app();
        let expected: u64 = a.all.iter().map(|s| s.total_tokens()).sum();
        assert_eq!(a.corpus_tokens, expected);
        assert!(a.corpus_tokens > 0);
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::fixtures::*;
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    fn at(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }
    fn click(x: u16, y: u16) -> MouseEvent {
        at(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    /// Screen row of the nth selectable entry. Hardcoding a row number lands
    /// on a date band as soon as the fixture shifts.
    fn row_y(a: &App, nth: usize) -> u16 {
        let idx = a
            .view
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selectable())
            .map(|(i, _)| i)
            .nth(nth)
            .expect("that many rows");
        a.hits.list.y + idx as u16
    }

    /// Stand in for what the renderer would have recorded.
    fn laid_out(a: &mut App) {
        a.hits.list = Rect::new(0, 3, 120, 10);
        a.hits.list_offset = 0;
        a.hits.colhead_y = 2;
        a.hits.footer_y = 20;
        a.hits.columns = vec![(8, 12, Sort::Recency), (20, 26, Sort::Title)];
        a.hits.footer = vec![(2, 10, Action::Resume), (12, 20, Action::Help)];
        a.sub_span = (35, 40);
    }

    #[test]
    fn clicking_a_row_moves_the_cursor_there() {
        let mut a = app();
        laid_out(&mut a);
        let y = row_y(&a, 2);
        a.on_mouse(click(60, y));
        assert_eq!(a.cursor as u16 + a.hits.list.y, y);
        assert!(a.view[a.cursor].selectable());
    }

    #[test]
    fn clicking_a_date_band_does_nothing() {
        let mut a = app();
        laid_out(&mut a);
        let band = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Divider(_)))
            .expect("a band");
        let before = a.cursor;
        a.on_mouse(click(60, 3 + band as u16));
        assert_eq!(a.cursor, before, "bands are not selectable");
    }

    #[test]
    fn clicking_below_the_list_is_ignored() {
        let mut a = app();
        laid_out(&mut a);
        let before = a.cursor;
        a.on_mouse(click(60, 19));
        assert_eq!(a.cursor, before);
    }

    #[test]
    fn a_column_heading_sorts_by_that_column() {
        let mut a = app();
        laid_out(&mut a);
        a.on_mouse(click(22, 2));
        assert_eq!(a.sort, Sort::Title);
    }

    #[test]
    fn a_footer_hint_only_fires_on_the_footer_row() {
        // The bug this guards: the footer used to claim any click sharing its
        // column, so clicking a list row silently did something else.
        let mut a = app();
        laid_out(&mut a);
        a.on_mouse(click(14, 6));
        assert_ne!(a.input_mode, InputMode::Help, "a list click opened help");

        a.on_mouse(click(14, 20));
        assert_eq!(
            a.input_mode,
            InputMode::Help,
            "the footer hint did not fire"
        );
    }

    #[test]
    fn right_click_toggles_the_favourite_under_it() {
        let mut a = app();
        laid_out(&mut a);
        let y = row_y(&a, 1); // not the one already favourited
        a.on_mouse(click(60, y));
        let id = a.current().unwrap().id.clone();
        let before = a.meta.get(&id).map(|e| e.favorite).unwrap_or(false);
        a.on_mouse(at(MouseEventKind::Down(MouseButton::Right), 60, y));
        let after = a.meta.get(&id).map(|e| e.favorite).unwrap_or(false);
        assert_ne!(before, after, "right-click did not toggle {id}");
    }

    #[test]
    fn the_wheel_moves_through_the_list() {
        let mut a = app();
        laid_out(&mut a);
        let before = a.cursor;
        a.on_mouse(at(MouseEventKind::ScrollDown, 60, 6));
        assert!(a.cursor > before);
        a.on_mouse(at(MouseEventKind::ScrollUp, 60, 6));
        a.on_mouse(at(MouseEventKind::ScrollUp, 60, 6));
        assert!(a.cursor <= before);
    }

    #[test]
    fn two_clicks_on_one_row_resume_it() {
        let mut a = app();
        laid_out(&mut a);
        let y = row_y(&a, 1);
        a.on_mouse(click(60, y));
        assert!(a.outcome.is_none(), "one click only selects");
        a.on_mouse(click(60, y));
        assert!(a.outcome.is_some(), "the second click should resume");
    }

    #[test]
    fn clicking_two_different_rows_is_not_a_double_click() {
        let mut a = app();
        laid_out(&mut a);
        a.on_mouse(click(60, row_y(&a, 1)));
        a.on_mouse(click(60, row_y(&a, 2)));
        assert!(a.outcome.is_none(), "different rows must not resume");
    }

    #[test]
    fn clicking_the_subagent_count_expands_it() {
        let mut a = app();
        a.do_action(Action::Subagents);
        a.all[0].subagent_count = 2;
        laid_out(&mut a);
        let row = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(0)))
            .expect("the parent row");
        a.on_mouse(click(37, 3 + row as u16));
        assert!(
            a.expanded.contains("aaaaaaaa-1"),
            "the ⌁ cell did not expand"
        );
    }

    #[test]
    fn a_click_closes_the_help_rather_than_acting_on_the_list() {
        let mut a = app();
        laid_out(&mut a);
        a.input_mode = InputMode::Help;
        a.on_mouse(click(60, 6));
        assert_eq!(a.input_mode, InputMode::Normal);
        assert!(a.outcome.is_none());
    }

    #[test]
    fn the_wheel_scrolls_the_viewer_not_the_list() {
        let mut a = app();
        laid_out(&mut a);
        a.input_mode = InputMode::Viewer;
        a.viewer_height = 100;
        a.viewer_page = 10;
        let cursor = a.cursor;
        a.on_mouse(at(MouseEventKind::ScrollDown, 60, 6));
        assert_eq!(a.cursor, cursor, "the list must not move");
        assert!(a.viewer_scroll > 0);
    }
}

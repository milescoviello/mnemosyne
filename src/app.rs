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
    /// Naming the tmux session a chat is about to run in. Blank means the
    /// generated `mn-<id>`, which is what it has always been.
    TmuxName,
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

/// A tag the list gives sessions itself, which nobody may give one by hand.
fn is_automatic(tag: &str) -> bool {
    tag.starts_with(crate::wsx::TAG_PREFIX)
}

/// Why an automatic tag was not stored, and what to do instead.
fn automatic_note(tags: &[String]) -> String {
    format!(
        "#{} is automatic — every session in a wsx workspace has its repo's; T shows them",
        tags.join(" #")
    )
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
    /// A new terminal window, with the session running under tmux inside it.
    WindowTmux,
    /// Reopen the sessions that were running before the machine rebooted.
    Reopen,
    /// Put the reopen offer away without acting on it.
    DismissReopen,
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
    /// Screen row of the reopen offer, and the spans within it.
    pub banner_y: Option<u16>,
    pub banner: Vec<(u16, u16, Action)>,
}

#[derive(Clone, Debug)]
pub struct ResumeTarget {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub perms: String,
    pub title: String,
    /// Why it lands somewhere other than where it ran; empty when it does
    /// not.
    pub note: String,
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
    /// A window *and* a tmux session: the work runs under tmux, and a terminal
    /// window is opened attached to it. Closing the window then leaves the
    /// session running instead of killing it, which is what you want from
    /// something restored automatically after a reboot.
    WindowTmux,
}

impl Target {
    pub fn tag(self) -> &'static str {
        match self {
            Target::Here => "here",
            Target::Window => "window",
            Target::Tmux => "tmux",
            Target::WindowTmux => "wintmux",
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

/// What wsx has to say about resuming a session here.
enum Claim {
    /// The conversation a live workspace's agent is on, or will carry on.
    Agent(crate::wsx::Jump),
    /// Another conversation from a live workspace's worktree, which a jump
    /// would not show. Named by the workspace, with why not.
    Not { label: String, why: &'static str },
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
    /// A rescan is running behind the list, which is showing cached rows in
    /// the meantime.
    pub indexing: bool,
    /// Sessions to open in their own window, handed to the shell as they are
    /// chosen. Opening a window elsewhere is no reason to close the picker,
    /// so these are streamed out and the browser stays where it is.
    pub to_open: Vec<(Target, Vec<ResumeTarget>)>,
    /// Everything handed over during this run. They will be running moments
    /// from now and nothing else is watching, so they belong in the record
    /// of what was open -- which is what a reboot is put back from.
    pub launched: Vec<ResumeTarget>,
    /// Live wsx workspaces to hand back to wsx, which already runs their
    /// agent. Carried out by main between frames, like `to_open`.
    pub to_jump: Vec<crate::wsx::Jump>,
    /// Said on stderr once the browser has closed. A choice that ends it
    /// takes the status line with it, so anything the shell should see
    /// before it resumes goes here as well.
    pub notes: Vec<String>,
    /// Whether changes are written to disk.
    ///
    /// False in tests, and for one reason: without it `cargo test` overwrote
    /// the real `meta.json` in whatever home it ran in, replacing a person's
    /// favourites, tags and notes with the fixture's. It did that for days
    /// before anyone noticed, because the tests all passed.
    pub persist: bool,
    /// A tmux resume waiting on its name, and the name once it is given.
    /// Empty means "use the generated one".
    pending_tmux: Option<Target>,
    pub tmux_name: String,
    /// Sessions that were running before the last reboot and are not running
    /// now. Empty in the normal case; when it is not, the offer to reopen
    /// them sits above the list until it is taken or waved away.
    pub reopen: Vec<crate::workspace::Entry>,
    /// What wsx has to say about the folders sessions ran in.
    pub wsx: crate::wsx::State,
    /// Whether the real wsx is asked again when the list is reloaded. Off
    /// unless main turns it on, so nothing built for a test ever reaches it:
    /// the same binary can switch a running wsx to another workspace, or open
    /// a terminal on the desktop of whoever runs the suite.
    pub ask_wsx: bool,

    /// Keyed on the size as well as the path, so a session that has grown
    /// since is read again rather than shown as it was the first time.
    preview_cache: HashMap<(String, u64), Vec<Turn>>,
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
            indexing: false,
            to_open: Vec::new(),
            launched: Vec::new(),
            to_jump: Vec::new(),
            notes: Vec::new(),
            persist: true,
            pending_tmux: None,
            tmux_name: String::new(),
            reopen: Vec::new(),
            wsx: crate::wsx::State::default(),
            ask_wsx: false,
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
        let tmux = crate::live::tmux_by_session_id();
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
            match tmux.get(&s.id) {
                Some(name) => {
                    s.has_tmux = true;
                    s.tmux_session = name.clone();
                }
                None => {
                    s.has_tmux = false;
                    s.tmux_session.clear();
                }
            }
            s.cwd_missing = !s.cwd.is_empty() && !dir_exists.get(&s.cwd).copied().unwrap_or(true);
            s.live_pid = None;
            s.live_exact = false;
            s.live_in_wsx = false;
            if let Some(p) = self.live.by_id.get(&s.id) {
                s.live_pid = Some(p.pid);
                s.live_exact = true;
                s.live_in_wsx = p.under_wsx;
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
                    self.all[i].live_in_wsx = p.under_wsx;
                }
            }
        }
        self.apply_wsx();
    }

    /// Name the wsx workspace each session ran in, once per folder.
    fn apply_wsx(&mut self) {
        let wsx = &self.wsx;
        let mut seen: HashMap<String, Option<crate::wsx::Place>> = HashMap::new();
        for s in &mut self.all {
            s.wsx = if s.cwd.is_empty() {
                None
            } else {
                seen.entry(s.cwd.clone())
                    .or_insert_with(|| wsx.place(&s.cwd))
                    .clone()
            };
        }
    }

    /// Take in what wsx says, and redraw the list in its light.
    pub fn set_wsx(&mut self, state: crate::wsx::State) {
        self.wsx = state;
        self.apply_wsx();
        self.rebuild();
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
            // wsx/<repo> is stored nowhere. It is read off the folder, so
            // that is what it is matched against.
            let has = if t.starts_with(crate::wsx::TAG_PREFIX) {
                s.wsx.as_ref().is_some_and(|w| w.tag() == *t)
            } else {
                s.tags.iter().any(|x| x == t)
            };
            if !has {
                return false;
            }
        }
        match self.date {
            DateRange::All => {}
            DateRange::Today => {
                // The same rule the heading uses. A rolling 24 hours put
                // sessions from yesterday afternoon under a "yesterday"
                // band while the "today" filter still showed them: two
                // things labelled today, disagreeing on screen.
                if date_band(s.mtime) != "today" {
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
            // `--resume` line and land on that session. A wsx workspace is
            // in twice over: by the name the list shows, and by its repo's
            // tag, so `/wsx/os-dev` finds what `T` would.
            let wsx = s
                .wsx
                .as_ref()
                .map(|w| format!("{} {}", w.label(), w.tag()))
                .unwrap_or_default();
            let hay = format!(
                "{} {} {} {} {} {} {}",
                s.title(),
                crate::model::short_cwd(&s.cwd),
                wsx,
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
        let hay = format!("{} {}", s.title(), s.folder());
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
                let d = self.all[i].folder();
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
            //
            // Favourites are pinned above everything regardless of age, so
            // they are their own band: without that the run of pinned rows
            // cuts across the dates and you get "today, yesterday, today,
            // yesterday" as the list crosses back into time order.
            let banded = self.sort == Sort::Recency;
            let mut band = String::new();
            for &i in &idx {
                if banded {
                    let b = if self.all[i].favorite {
                        "favourites".to_string()
                    } else {
                        date_band(self.all[i].mtime).to_string()
                    };
                    if b != band {
                        rows.push(Row::Divider(b.clone()));
                        band = b;
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

    /// Open every parent that has children.
    ///
    /// The interface expands one at a time, which is right when you are
    /// reading it. A flat dump has no such gesture, so "reveal subagents"
    /// there can only mean all of them.
    pub fn expand_all(&mut self) {
        let ids: Vec<String> = self
            .all
            .iter()
            .filter(|s| !s.is_subagent && s.subagent_count > 0)
            .map(|s| s.id.clone())
            .collect();
        self.expanded.extend(ids);
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
        // `get`, not an index. The view holds positions into `all`, and the
        // two are briefly out of step whenever a finished rescan replaces
        // the sessions: `rebuild` asks what the cursor is on before it
        // rebuilds, so a shorter list panicked with an index out of bounds
        // rather than simply having nothing there.
        self.current_idx().and_then(|i| self.all.get(i))
    }

    /// Sessions we could actually point at a running process.
    ///
    /// Not the same as the number of claude processes: a plain `claude` with
    /// no `--resume` cannot be tied to a transcript. Counting processes here
    /// meant the header could say "4 live" with nothing in the list marked,
    /// which reads as a bug.
    /// What the list is showing, truthfully.
    ///
    /// Turning one filter off used to announce "all sessions" whatever else
    /// was still on -- a tag filter narrowing it to one, say. This names
    /// what is still filtering, so the message matches the list under it.
    pub fn view_note(&self) -> String {
        let shown = self.item_count();
        let mut on: Vec<String> = Vec::new();
        if !self.fuzzy.trim().is_empty() {
            on.push(format!("/{}", self.fuzzy.trim()));
        }
        if self.deep_hits.is_some() {
            on.push(format!("search “{}”", self.deep.trim()));
        }
        if let Some(t) = &self.tag_filter {
            on.push(format!("#{t}"));
        }
        if self.fav_only {
            on.push("favourites".into());
        }
        if self.live_only {
            on.push("running".into());
        }
        if self.date != DateRange::All {
            on.push(self.date.label());
        }
        if on.is_empty() {
            format!("all {shown} sessions")
        } else {
            format!("{shown} shown — still filtered by {}", on.join(", "))
        }
    }

    /// After a reindex: what the index holds, and what of it is on screen.
    pub fn reindex_message(&self) -> String {
        let total = self.all.iter().filter(|s| !s.is_subagent).count();
        let shown = self.item_count();
        if shown == total {
            format!("reindexed — {total} sessions")
        } else {
            format!("reindexed — {total} sessions, {shown} shown by the current filters")
        }
    }

    /// Favourites among the sessions that exist, which is what the list
    /// marks. The overlay outlives transcripts, so counting its entries
    /// showed a star total nothing on screen accounted for.
    pub fn favourites_shown(&self) -> usize {
        self.all
            .iter()
            .filter(|s| s.favorite && !s.is_subagent)
            .count()
    }

    pub fn live_shown(&self) -> usize {
        self.all
            .iter()
            .filter(|s| s.is_live() && !s.is_subagent)
            .count()
    }

    pub fn item_count(&self) -> usize {
        self.view.iter().filter(|r| r.selectable()).count()
    }

    pub fn preview(&mut self, want: usize) -> Vec<Turn> {
        let Some(i) = self.current_idx() else {
            return Vec::new();
        };
        // The file's size now, so a session that grows while you watch is
        // read again -- a stat, where reading the tail every frame would not
        // be.
        let path = &self.all[i].path;
        let size = std::fs::metadata(path)
            .map(|m| m.len())
            .unwrap_or(self.all[i].size);
        let key = (path.to_string_lossy().to_string(), size);
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
        let needle = self.deep.trim().to_string();
        let text = crate::index::Index::open()
            .ok()
            .and_then(|i| i.excerpt_for(&path, &needle))?;
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
        if self.persist {
            let _ = self.meta.save();
        }
        self.all[i].favorite = now;
        self.status = if now {
            "★ favourited".into()
        } else {
            "unfavourited".into()
        };
        self.rebuild();
    }

    /// Is this session one of the ones the offer is proposing to reopen?
    pub fn is_offered(&self, id: &str) -> bool {
        !self.reopen.is_empty() && self.reopen.iter().any(|e| e.id == id)
    }

    /// Take one session out of the offer, leaving the rest of it standing.
    ///
    /// This is what `space` does on a row that is in the offer, so narrowing
    /// it down uses the key you already use for picking things. It stays out
    /// of the ordinary selection set deliberately: pre-selecting three rows
    /// would quietly redefine what `enter` does, and resuming one session
    /// would open three.
    fn drop_from_offer(&mut self, id: &str) {
        let before = self.reopen.len();
        self.reopen.retain(|e| e.id != id);
        if self.reopen.len() == before {
            return;
        }
        if self.reopen.is_empty() {
            // Taken apart one at a time until nothing is left, which is the
            // same answer as waving it away.
            if self.persist {
                crate::workspace::dismiss_previous();
            }
            self.status = "nothing left to reopen — mn --reopen brings it back".into();
        } else {
            self.status = format!("{} left to reopen", self.reopen.len());
        }
        self.move_by(1);
    }

    fn toggle_select(&mut self) {
        let Some(i) = self.current_idx() else { return };
        let key = self.all[i].path.to_string_lossy().to_string();
        // On a row the offer is proposing, space narrows the offer rather
        // than starting an unrelated selection.
        let id = self.all[i].id.clone();
        if self.is_offered(&id) {
            self.drop_from_offer(&id);
            return;
        }
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
        // Opening one shows subagents again if `a` had hidden them. Closing
        // one must not: it turned them back on under every other parent
        // still marked open.
        if open {
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

    /// The sessions that are open right now, in the form the reopen record
    /// wants them. This is deliberately the same set the list marks with a
    /// `●`: whatever the header claims is live is exactly what gets written
    /// down, so the two can never disagree.
    pub fn open_sessions(&self) -> Vec<crate::workspace::Entry> {
        let mut out = Vec::new();
        for s in &self.all {
            if s.is_subagent || (s.live_pid.is_none() && !s.has_tmux) {
                continue;
            }
            if s.cwd.is_empty() {
                continue;
            }
            // wsx puts its own agents back when it starts. Reopening one
            // here as well would be the second client it is careful to avoid.
            if s.live_in_wsx && !s.has_tmux {
                continue;
            }
            // Where it would be reopened, which for an archived workspace is
            // not the worktree it started in: that has gone, and a reboot
            // offer skips any folder that has.
            out.push(crate::workspace::Entry {
                id: s.id.clone(),
                cwd: s.resumes_elsewhere().unwrap_or(&s.cwd).to_string(),
                model: s.model.clone(),
                perms: s.permission_mode.clone(),
                title: s.title().to_string(),
            });
        }
        out
    }

    /// Fold in a finished rescan without moving the reader.
    ///
    /// The rows are replaced underneath whatever is on screen, and they are
    /// sorted by recency, so holding the cursor at the same index would put
    /// it on a different session almost every time. It follows the session
    /// it was on instead.
    pub fn absorb_rescan(&mut self, mut fresh: Vec<Session>) {
        let was = self.current().map(|s| s.id.clone());
        fresh.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        // Drop the old view before the sessions it points into.
        self.view.clear();
        self.cursor = 0;
        self.all = fresh;
        self.live = crate::live::live_map();
        // A rescan is when new workspaces' sessions turn up, and archived
        // ones' worktrees go, so it is when wsx is asked again.
        if self.ask_wsx {
            self.wsx = crate::wsx::load();
        }
        self.recompute_totals();
        self.apply_overlay();
        self.rebuild();
        if let Some(id) = was {
            self.focus_id(&id);
        }
    }

    /// Put the cursor back on a session, if it is still in view.
    pub fn focus_id(&mut self, id: &str) {
        if let Some(i) = self.view.iter().position(|r| match r {
            Row::Item(i) | Row::Sub(i) => self.all[*i].id == id,
            _ => false,
        }) {
            self.cursor = i;
        }
    }

    /// Load the offer, if a reboot left one outstanding.
    pub fn load_reopen(&mut self) {
        let w = crate::workspace::load();
        let running = self.running_ids();
        self.reopen = crate::workspace::pending(&w, &crate::workspace::boot_id(), false, &|id| {
            running.contains(id)
        });
    }

    /// Sessions known to be running: matched by id, or waiting in tmux.
    ///
    /// Not a match by folder. That is a guess, and a fresh `claude` started
    /// after a reboot in a folder a pre-reboot session ran in took that
    /// session for running -- it vanished from the offer, and taking the
    /// offer lost it for good.
    pub fn running_ids(&self) -> HashSet<String> {
        self.all
            .iter()
            .filter(|s| s.live_exact || s.has_tmux)
            .map(|s| s.id.clone())
            .collect()
    }

    fn reopen_previous(&mut self) {
        if self.reopen.is_empty() {
            return;
        }
        // The offer was worked out when the browser started. Since then one
        // may have been opened from the list, or started somewhere else, and
        // reopening that puts a second client on it.
        let running = self.running_ids();
        let busy = |id: &str| running.contains(id) || self.launched.iter().any(|t| t.id == id);
        let targets: Vec<ResumeTarget> = self
            .reopen
            .iter()
            .filter(|e| !busy(&e.id))
            .map(|e| ResumeTarget {
                id: e.id.clone(),
                cwd: e.cwd.clone(),
                model: if self.restore_model {
                    e.model.clone()
                } else {
                    String::new()
                },
                perms: e.perms.clone(),
                title: e.title.clone(),
                note: String::new(),
            })
            .collect();
        // Same as any other window: hand them over and stay open, so the
        // list is still there when they appear.
        self.launched.extend(targets.iter().cloned());
        // Each under its own generated name. The name typed for the last
        // `W` is still here, and every one of these went out under it --
        // `eft-work-2`, `eft-work-3` -- none of them the session you named.
        self.tmux_name.clear();
        let open_already = self.reopen.len() - targets.len();
        self.status = match (targets.len(), open_already) {
            (0, _) => "every one of them is open already".into(),
            (_, 0) => "reopening them in their own windows".into(),
            (n, k) => format!("reopening {n} in their own windows — {k} open already"),
        };
        if !targets.is_empty() {
            self.to_open.push((Target::WindowTmux, targets));
        }
        self.reopen.clear();
        if self.persist {
            crate::workspace::clear_previous();
        }
        self.rebuild();
    }

    pub fn targets(&self) -> Vec<ResumeTarget> {
        let mk = |s: &Session| {
            // Every route the shell takes starts with a cd, and all but
            // resuming here skip a folder that is gone -- so landing an
            // archived workspace in its repo is all a matter of this.
            let elsewhere = s.resumes_elsewhere();
            ResumeTarget {
                id: if s.is_subagent {
                    s.parent.clone().unwrap_or_else(|| s.id.clone())
                } else {
                    s.id.clone()
                },
                cwd: elsewhere.unwrap_or(&s.cwd).to_string(),
                model: if self.restore_model {
                    s.model.clone()
                } else {
                    String::new()
                },
                perms: s.permission_mode.clone(),
                title: s.title().to_string(),
                note: match (elsewhere, &s.wsx) {
                    (Some(c), Some(w)) => format!(
                        "{} was archived — resuming in {}",
                        w.label(),
                        crate::model::short_cwd(c)
                    ),
                    _ => String::new(),
                },
            }
        };
        let picks = self.picks();
        if picks.is_empty() {
            return self.current().map(mk).into_iter().collect();
        }
        let mut out: Vec<ResumeTarget> = Vec::new();
        for i in picks {
            // A subagent resumes its parent, so a parent picked along with
            // its children is one session, not a window each on it.
            let t = mk(&self.all[i]);
            if !out.iter().any(|o| o.id == t.id) {
                out.push(t);
            }
        }
        out
    }

    /// Everything picked with space, as indices into `all`: the ones on
    /// screen first, in list order, then any a filter is hiding.
    ///
    /// The hidden ones count. They are in the header's "picked" and `t`
    /// tags them; enter used to skip them, and when none were on screen it
    /// resumed the row under the cursor instead -- a session nobody picked.
    fn picks(&self) -> Vec<usize> {
        if self.selected.is_empty() {
            return Vec::new();
        }
        let picked = |i: usize| {
            self.selected
                .contains(&self.all[i].path.to_string_lossy().to_string())
        };
        let mut out: Vec<usize> = self
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) | Row::Sub(i) if picked(*i) => Some(*i),
                _ => None,
            })
            .collect();
        let shown: HashSet<usize> = out.iter().copied().collect();
        out.extend((0..self.all.len()).filter(|i| !shown.contains(i) && picked(*i)));
        out
    }

    /// Ask what to call the tmux session, then resume into it.
    ///
    /// `mn-026bcdb5` tells you nothing in `tmux ls`. Naming it is one enter
    /// away, and an empty answer keeps the generated name.
    fn ask_tmux_name(&mut self, target: Target) {
        if self.targets().is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        // Already running somewhere: go there instead of asking what to call
        // a second one.
        if let Some(s) = self.current().map(|s| self.resumed(s)) {
            if s.has_tmux && self.picks().is_empty() {
                let name = s.tmux_session.clone();
                self.pending_tmux = Some(target);
                self.tmux_name = name;
                self.finish_tmux();
                return;
            }
        }
        self.pending_tmux = Some(target);
        self.input.clear();
        self.input_mode = InputMode::TmuxName;
    }

    fn commit_tmux_name(&mut self) {
        self.tmux_name = crate::live::clean_tmux_name(&self.input);
        self.input.clear();
        self.input_mode = InputMode::Normal;
        self.finish_tmux();
    }

    fn finish_tmux(&mut self) {
        let Some(target) = self.pending_tmux.take() else {
            return;
        };
        self.resume(target);
    }

    /// The session resuming a row resumes: a subagent's is its parent's.
    fn resumed<'a>(&'a self, s: &'a Session) -> &'a Session {
        if !s.is_subagent {
            return s;
        }
        s.parent
            .as_deref()
            .and_then(|p| self.all.iter().find(|o| !o.is_subagent && o.id == p))
            .unwrap_or(s)
    }

    /// Whether a session belongs to wsx rather than to this terminal.
    ///
    /// wsx keeps a live workspace's agents running, each on a conversation of
    /// its own, and puts them back on those same conversations when it starts
    /// again -- by id where it has recorded one, otherwise with
    /// `claude --continue`, which carries on the newest conversation in the
    /// worktree. So:
    ///
    /// - a conversation wsx is running now is wsx's to bring back;
    /// - with an agent running in the worktree, any other conversation there
    ///   is not the one a jump would show;
    /// - with none running, the newest is the best guess at what wsx will
    ///   carry on, and an older one is not it -- resuming it here would also
    ///   make it the newest, and so the next `--continue`.
    ///
    /// A session started further down the worktree is none of these: no
    /// agent runs there, and `--continue` never reaches it.
    fn wsx_claim(&self, s: &Session) -> Option<Claim> {
        let w = s.wsx.as_ref()?;
        let crate::wsx::Status::Live { worktree } = &w.status else {
            return None;
        };
        let jump = || {
            Claim::Agent(crate::wsx::Jump {
                repo: w.repo.clone(),
                slug: w.slug.clone(),
                worktree: worktree.clone(),
            })
        };
        if s.live_in_wsx {
            return Some(jump());
        }
        if !w.rest.is_empty() {
            return None;
        }
        let at_top = |o: &&Session| {
            !o.is_subagent
                && o.wsx
                    .as_ref()
                    .is_some_and(|ow| ow.rest.is_empty() && ow.status == w.status)
        };
        if self.all.iter().filter(at_top).any(|o| o.live_in_wsx) {
            return Some(Claim::Not {
                label: w.label(),
                why: "another",
            });
        }
        // The same tie-break as the running-process guess: first of the
        // newest.
        let newest =
            self.all
                .iter()
                .filter(at_top)
                .fold(None::<&Session>, |best, o| match best {
                    Some(b) if b.mtime >= o.mtime => Some(b),
                    _ => Some(o),
                })?;
        Some(if newest.id == s.id {
            jump()
        } else {
            Claim::Not {
                label: w.label(),
                why: "a newer",
            }
        })
    }

    /// Carry out the jumps enter queued, once the loop has a moment.
    ///
    /// `fresh` is wsx asked again. The row was drawn from what it said
    /// earlier, and a workspace archived since is not there to go to. `run`
    /// does the jump, handed in so that nothing here starts a process.
    pub fn finish_jumps(
        &mut self,
        fresh: crate::wsx::State,
        run: impl Fn(&crate::wsx::Jump) -> Result<(), String>,
    ) {
        let jumps = std::mem::take(&mut self.to_jump);
        if jumps.is_empty() {
            return;
        }
        if !fresh.available {
            // Not an answer, so nothing is concluded from it.
            self.status = format!(
                "wsx did not answer, so nothing switched to {} — enter tries again",
                jumps[0].label()
            );
            return;
        }
        for j in &jumps {
            // Found by its worktree and sent under its name now, which a
            // rename in the meantime has changed.
            let now = fresh.workspaces.iter().find(|w| w.path == j.worktree);
            self.status = match now {
                Some(w) => {
                    let now = crate::wsx::Jump {
                        repo: w.repo.clone(),
                        slug: w.slug.clone(),
                        worktree: w.path.clone(),
                    };
                    match run(&now) {
                        Ok(()) => format!("switched to {} in wsx", now.label()),
                        Err(e) => format!("wsx could not switch to {}: {e}", now.label()),
                    }
                }
                None => {
                    let label = j.label();
                    if std::path::Path::new(&j.worktree).is_dir() {
                        format!(
                            "{label} was archived just now, its worktree kept — enter again resumes it there"
                        )
                    } else {
                        let then = match fresh.place(&j.worktree).map(|p| p.status) {
                            Some(crate::wsx::Status::Archived { checkout: Some(c) }) => {
                                format!("in {}", crate::model::short_cwd(&c))
                            }
                            _ => "wherever you are".into(),
                        };
                        format!("{label} was archived just now — enter again resumes it {then}")
                    }
                }
            };
        }
        // The whole overlay, not just the names: archiving deletes the
        // worktree, and whether a folder is gone is part of what decides
        // where a session resumes.
        self.wsx = fresh;
        self.apply_overlay();
        self.rebuild();
    }

    fn resume(&mut self, target: Target) {
        let mut targets = self.targets();
        if targets.is_empty() {
            self.status = "nothing selected".into();
            return;
        }
        let picked = !self.picks().is_empty();
        // Several at once cannot all land here, so they become windows, and
        // one already running -- by wsx or anyone else -- is left out of them
        // rather than opened a second time, as enter on its own row would
        // refuse it. The explicit routes are taken at their word.
        if target == Target::Here && picked {
            let mut held: Vec<String> = Vec::new();
            let mut running = 0usize;
            targets.retain(|t| {
                let Some(s) = self.all.iter().find(|o| !o.is_subagent && o.id == t.id) else {
                    return true;
                };
                let busy = s.live_exact || s.has_tmux || self.launched.iter().any(|l| l.id == s.id);
                match self.wsx_claim(s) {
                    // wsx's own agent is wsx's, whatever else is true of it
                    Some(Claim::Agent(j)) if s.live_in_wsx => held.push(j.label()),
                    _ if busy => running += 1,
                    Some(Claim::Agent(j)) => held.push(j.label()),
                    Some(Claim::Not { label, .. }) => held.push(label),
                    None => return true,
                }
                false
            });
            held.sort();
            held.dedup();
            let mut why: Vec<String> = Vec::new();
            if !held.is_empty() {
                why.push(format!("{}: live in wsx", held.join(", ")));
            }
            if running > 0 {
                why.push(format!("{running} already running"));
            }
            if !why.is_empty() {
                let note = format!(
                    "left out {} — ctrl+n opens {} anyway",
                    why.join("; "),
                    if held.len() + running == 1 {
                        "it"
                    } else {
                        "them"
                    }
                );
                self.status = note.clone();
                if targets.is_empty() {
                    return;
                }
                self.notes.push(note);
            }
        }
        let claim = if target == Target::Here && !picked {
            self.current()
                .map(|s| self.resumed(s))
                .and_then(|s| self.wsx_claim(s))
        } else {
            None
        };
        // wsx running it outright comes before the guard below, which would
        // otherwise read an agent wsx resumed by id as somebody's process and
        // refuse it.
        let wsx_runs_it = self
            .current()
            .map(|s| self.resumed(s))
            .is_some_and(|s| s.live_in_wsx);
        if let (Some(Claim::Agent(j)), true) = (&claim, wsx_runs_it) {
            self.status = format!("switching to {} in wsx…", j.label());
            self.to_jump.push(j.clone());
            return;
        }
        // Guard against silently starting a second client on a transcript that
        // already has one. Tmux is exempt: attaching to the existing session is
        // exactly the right move there, and is what "resume" should mean.
        if target == Target::Here {
            // The session that would be resumed, not the row: a subagent is
            // never running itself, so asking it let enter on one start a
            // second client on the parent it resumes.
            if let Some(s) = self.current().map(|s| self.resumed(s)) {
                // A waiting tmux session is as strong a signal as an exact pid
                // match: resuming here would fork a second client instead of
                // picking up where that one left off.
                // `launched` covers what this run has just opened. The
                // other two read the process table, which is polled every
                // few seconds -- opening a window and pressing enter beats
                // that poll, and a guard you can outrun is not a guard.
                let just_opened = self.launched.iter().any(|t| t.id == s.id);
                if (s.live_exact || s.has_tmux || just_opened) && !picked {
                    self.status = if just_opened && !s.has_tmux && !s.live_exact {
                        "already opened in a window just now".to_string()
                    } else if s.has_tmux {
                        format!(
                            "{} is already running in tmux — ctrl+t attaches to it",
                            s.tmux_session
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
            // Nothing running it, but the workspace is live, and wsx will
            // put its agent back on a conversation of its choosing.
            match claim {
                Some(Claim::Agent(j)) => {
                    self.status = format!("switching to {} in wsx…", j.label());
                    self.to_jump.push(j);
                    return;
                }
                Some(Claim::Not { label, why }) => {
                    self.status = format!(
                        "{label} is live in wsx on {why} session — ctrl+n opens this one anyway"
                    );
                    return;
                }
                None => {}
            }
        }
        // A window of its own does not need this one: hand it to the shell
        // and carry on browsing. Only landing *here*, or attaching in this
        // terminal, requires the picker to get out of the way.
        if matches!(target, Target::Window | Target::WindowTmux) {
            let n = targets.len();
            let what = if n == 1 {
                targets[0].title.clone()
            } else {
                format!("{n} sessions")
            };
            let moved = targets.iter().filter(|t| !t.note.is_empty()).count();
            self.status = if n == 1 && moved == 1 {
                format!("{}, in a new window", targets[0].note)
            } else if moved > 0 {
                format!(
                    "opening {what} in new windows — {moved} from archived wsx workspaces, in their repos' checkouts"
                )
            } else {
                format!("opening {what} in a new window")
            };
            self.launched.extend(targets.iter().cloned());
            self.to_open.push((target, targets));
            self.selected.clear();
            self.rebuild();
            return;
        }
        // This closes the browser, and the status line with it.
        self.notes.extend(
            targets
                .iter()
                .filter(|t| !t.note.is_empty())
                .map(|t| t.note.clone()),
        );
        self.outcome = Some(Outcome::Resume { targets, target });
        self.quit = true;
    }

    fn start_deep(&mut self) {
        let q = self.deep.trim().to_string();
        // Bump first, and for an empty query too. The generation is what
        // says which answer belongs to what you asked; leaving it alone
        // when the box is cleared means a search already in flight still
        // counts as current, and its hits land on a screen you cleared.
        self.deep_generation += 1;
        if q.is_empty() {
            self.deep_hits = None;
            self.deep_parent_hits.clear();
            self.deep_busy = false;
            self.rebuild();
            return;
        }
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
            self.rebuild();
            // Count the list, not the search. With a tag filter or a date
            // range also on, the two differ, and saying "3 match" over a
            // list of one leaves you unable to tell which number is wrong.
            let shown = self.item_count();
            self.status = if shown == n {
                format!(
                    "{shown} session(s) match “{}” in {}",
                    self.deep,
                    self.deep_mode.label()
                )
            } else {
                format!(
                    "{shown} of {n} matching “{}” shown — other filters are on",
                    self.deep
                )
            };
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
            if is_automatic(&crate::meta::normalize_tag(to)) {
                self.status = automatic_note(&[crate::meta::normalize_tag(to)]);
                self.input.clear();
                self.input_mode = InputMode::Normal;
                return;
            }
            let n = self.meta.rename_tag(from, to);
            if self.persist {
                let _ = self.meta.save();
            }
            // Only when something was renamed. `eft>` moved the filter to
            // no tag at all, and renaming nothing followed the name anyway,
            // leaving an empty list under a status saying nothing happened.
            if n > 0
                && self.tag_filter.as_deref() == Some(crate::meta::normalize_tag(from).as_str())
            {
                self.tag_filter = Some(crate::meta::normalize_tag(to));
            }
            // Say what happened. "renamed #keeper to # on 0 session(s)" is
            // three quarters of a success report for something that did not
            // occur.
            let (f, t) = (
                crate::meta::normalize_tag(from),
                crate::meta::normalize_tag(to),
            );
            self.status = if f.is_empty() || t.is_empty() {
                "a rename wants both halves: old>new".to_string()
            } else if f == t {
                format!("#{f} is already its own name")
            } else if n == 0 {
                format!("nothing is tagged #{f}")
            } else {
                format!("renamed #{f} to #{t} on {n} session(s)")
            };
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
        // One list, split the same way whichever direction it goes. Adding
        // took "eft rig" as two tags while removing took it as one called
        // "eft-rig", so `-eft rig` asked for something nothing had -- and
        // said it had removed them.
        let mut names: Vec<String> = raw
            .trim_start_matches('-')
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(crate::meta::normalize_tag)
            .filter(|t| !t.is_empty())
            .collect();
        // Removing one is allowed -- a tag of that name stored before they
        // were automatic has to be possible to clear -- but never adding one.
        let refused: Vec<String> = if raw.starts_with('-') {
            Vec::new()
        } else {
            let (auto, given): (Vec<String>, Vec<String>) =
                names.into_iter().partition(|t| is_automatic(t));
            names = given;
            auto
        };

        if raw.starts_with('-') {
            let mut gone = 0usize;
            for id in &targets {
                for t in &names {
                    if self
                        .meta
                        .get(id)
                        .is_some_and(|e| e.tags.iter().any(|x| x == t))
                    {
                        self.meta.remove_tag(id, t);
                        gone += 1;
                    }
                }
            }
            // Say what happened, not what was asked for.
            self.status = match gone {
                0 => format!(
                    "nothing to remove: no session here has #{}",
                    names.join(" #")
                ),
                n => format!("removed #{} ({n} in all)", names.join(" #")),
            };
        } else if !names.is_empty() {
            for id in &targets {
                for t in &names {
                    self.meta.add_tag(id, t);
                }
            }
            let what = names.join(" #");
            self.status = if many == 1 {
                format!("tagged #{what}")
            } else {
                format!("tagged {many} sessions #{what}")
            };
            if !refused.is_empty() {
                self.status = format!(
                    "{} — #{} is automatic, so it was left off",
                    self.status,
                    refused.join(" #")
                );
            }
        } else if !refused.is_empty() {
            self.status = automatic_note(&refused);
        } else if !raw.is_empty() {
            self.status = format!("{raw:?} leaves nothing a tag can be made of");
        }
        if self.persist {
            let _ = self.meta.save();
        }
        self.input.clear();
        self.input_mode = InputMode::Normal;
        self.apply_overlay();
        self.rebuild();
    }

    fn commit_note(&mut self) {
        let Some(i) = self.current_idx() else {
            // Nothing to put it on; enter still has to close the prompt.
            self.input.clear();
            self.input_mode = InputMode::Normal;
            return;
        };
        let id = self.all[i].id.clone();
        let had = !self.all[i].note.is_empty();
        self.meta.set_note(&id, &self.input);
        if self.persist {
            let _ = self.meta.save();
        }
        self.all[i].note = self
            .meta
            .get(&id)
            .map(|e| e.note.clone())
            .unwrap_or_default();
        // An empty note clears it; "note saved" said the opposite.
        self.status = match (had, self.all[i].note.is_empty()) {
            (_, false) => "note saved".into(),
            (true, true) => "note removed".into(),
            (false, true) => "no note to save".into(),
        };
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

    /// Where the tag being typed starts: after the last separator, and past
    /// the `-` that makes the first one a removal. Tab completes only this,
    /// so `-ef`, `rig ef` and `old>ne` complete their last word -- matched
    /// against the whole input, they never could, and a match replaced
    /// everything typed before it.
    fn tag_word_start(&self) -> usize {
        let start = self
            .input
            .char_indices()
            .rev()
            .find(|(_, c)| *c == ',' || *c == '>' || c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if start == 0 && self.input.starts_with('-') {
            1
        } else {
            start
        }
    }

    /// Tag completions for the word being typed.
    ///
    /// Showing only one tag also offers the automatic ones, counted from the
    /// sessions that carry them. Tagging does not: it would refuse them.
    pub fn tag_completions(&self) -> Vec<String> {
        let pfx = crate::meta::normalize_tag(&self.input[self.tag_word_start()..]);
        let mut tags = self.meta.all_tags();
        if self.input_mode == InputMode::TagFilter {
            let mut auto: HashMap<String, usize> = HashMap::new();
            for s in self.all.iter().filter(|s| !s.is_subagent) {
                if let Some(w) = &s.wsx {
                    *auto.entry(w.tag()).or_insert(0) += 1;
                }
            }
            tags.extend(auto);
            tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        }
        tags.into_iter()
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
            Action::Reopen => self.reopen_previous(),
            Action::DismissReopen => {
                let n = self.reopen.len();
                self.reopen.clear();
                if self.persist {
                    crate::workspace::dismiss_previous();
                }
                self.status = format!("left {n} closed — mn --reopen still brings them back");
            }
            Action::View => self.open_viewer(),
            Action::Tmux => self.ask_tmux_name(Target::Tmux),
            Action::NewWindow => self.resume(Target::Window),
            Action::WindowTmux => self.ask_tmux_name(Target::WindowTmux),
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
                self.rebuild();
                self.status = if self.fav_only {
                    format!("favourites only — {}", self.view_note())
                } else {
                    self.view_note()
                };
            }
            Action::LiveOnly => {
                if !self.live.supported {
                    // Better to say why than to show an empty list.
                    self.status = "which sessions are running can only be detected on Linux".into();
                    return;
                }
                self.live_only = !self.live_only;
                self.rebuild();
                self.status = if self.live_only {
                    format!("running only — {}", self.view_note())
                } else {
                    self.view_note()
                };
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
                // Same reason as clearing the box: a search still running
                // would otherwise count as current and put its hits back.
                self.deep_generation += 1;
                self.deep_busy = false;
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
        // A prompt acts on the row it was opened over, and the keyboard
        // cannot leave that row while it is open. The wheel and a click
        // could, so a note, a tag or a tmux name went to wherever they left
        // the cursor when you pressed enter.
        if matches!(
            self.input_mode,
            InputMode::TagAdd | InputMode::TagFilter | InputMode::Note | InputMode::TmuxName
        ) {
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
        // the reopen offer: its two words are buttons
        if Some(y) == self.hits.banner_y {
            for (x0, x1, action) in self.hits.banner.clone() {
                if x >= x0 && x <= x1 {
                    self.do_action(action);
                    return;
                }
            }
            return;
        }
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
                        // Open is what is on screen: marked open, but with
                        // `a` hiding them, a click has to show them.
                        let open = self.show_subagents && self.expanded.contains(&self.all[i].id);
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
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

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
                        // As clearing the box does: a search still running
                        // would otherwise land on the list you backed out of.
                        self.deep_generation += 1;
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
            InputMode::TagAdd | InputMode::TagFilter | InputMode::Note | InputMode::TmuxName => {
                match k.code {
                    KeyCode::Esc => {
                        self.input.clear();
                        self.input_mode = InputMode::Normal;
                        // Backing out of the name prompt backs out of the
                        // resume as well, rather than leaving one armed.
                        self.pending_tmux = None;
                    }
                    KeyCode::Enter => match self.input_mode {
                        InputMode::TagAdd => self.commit_tag_add(),
                        InputMode::TagFilter => self.commit_tag_filter(),
                        InputMode::Note => self.commit_note(),
                        InputMode::TmuxName => self.commit_tmux_name(),
                        _ => {}
                    },
                    // A note or a tmux name is not a tag; tab there put one
                    // in place of whatever you had typed.
                    KeyCode::Tab
                        if matches!(self.input_mode, InputMode::TagAdd | InputMode::TagFilter) =>
                    {
                        if let Some(first) = self.tag_completions().first() {
                            if let Some(name) = first.split(' ').next() {
                                let name = name.to_string();
                                self.input.truncate(self.tag_word_start());
                                self.input.push_str(&name);
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
            // ctrl+shift+t only arrives as its own key where the terminal
            // says so -- inside tmux that needs `extended-keys on`, and
            // without it this is indistinguishable from ctrl+t. `W` does the
            // same thing and always gets through.
            KeyCode::Char('T') if ctrl => self.do_action(Action::WindowTmux),
            KeyCode::Char('t') if ctrl && shift => self.do_action(Action::WindowTmux),
            KeyCode::Char('t') if ctrl => self.do_action(Action::Tmux),
            KeyCode::Char('W') => self.do_action(Action::WindowTmux),

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
            // Both are inert unless the offer is actually showing: a stray
            // `r` should never be able to open a pile of windows.
            KeyCode::Char('r') if !self.reopen.is_empty() => self.do_action(Action::Reopen),
            KeyCode::Char('x') if !self.reopen.is_empty() => self.do_action(Action::DismissReopen),
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

    /// Where the fixtures' wsx keeps its worktrees.
    pub const WSX_ROOT: &str = "/home/u/.local/state/wsx/worktrees";

    /// A corpus with a bit of everything the interface has to cope with.
    pub fn corpus() -> Vec<Session> {
        let mut v = vec![
            session("aaaaaaaa-1", "today's work", "/home/u", 0),
            session("bbbbbbbb-2", "yesterday's thing", "/home/u/proj", 1),
            session("cccccccc-3", "last week", "/home/u/proj", 5),
            session("dddddddd-4", "last month", "/home/u/other", 20),
            session("eeeeeeee-5", "ancient", "/home/u", 300),
            session("ffffffff-6", "", "", 2), // no title, no cwd
            // Two wsx workspaces. Their names steer clear of what the
            // fuzzy-filter tests hunt for: a worktree path is long enough to
            // spell "ancient" by accident, and every `e` in it counts
            // towards matching "eeeeeeee".
            session(
                "gggggggg-7",
                "paging on x86",
                &format!("{WSX_ROOT}/OS-DEV/shy-daffodil"),
                3,
            ),
            session(
                "hhhhhhhh-8",
                "a GPT disk tool",
                &format!("{WSX_ROOT}/OS-DEV/gdisk-app"),
                8,
            ),
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

    /// Every fixture is non-persisting. This is not a detail: the suite used
    /// to overwrite the real `meta.json` of whoever ran it.
    pub fn app_with(meta: crate::meta::Meta, restore_model: bool) -> App {
        let live = LiveMap {
            by_id: Default::default(),
            by_cwd: Default::default(),
            count: 0,
            supported: true,
        };
        let mut a = App::new(corpus(), meta, live, restore_model);
        a.persist = false;
        // Handed in, not asked for: the root is not read from $HOME, so a
        // label does not depend on whose machine the suite runs on, and the
        // list is written here rather than got from a wsx that could do
        // things on this desktop. shy-daffodil is live; gdisk-app is not in
        // the list, so it has been archived.
        a.set_wsx(crate::wsx::State {
            root: Some(WSX_ROOT.into()),
            available: true,
            workspaces: vec![crate::wsx::Workspace {
                repo: "OS-DEV".into(),
                slug: "shy-daffodil".into(),
                branch: "u/shy-daffodil".into(),
                path: format!("{WSX_ROOT}/OS-DEV/shy-daffodil"),
            }],
            repos: format!("{:<20} {}\n", "OS-DEV", "/home/u/OS-DEV"),
        });
        a
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
    fn the_filter_matches_every_field_it_claims_to() {
        // "/ narrows by title, folder, branch, tag or id" -- each of those
        // is a separate haystack entry, and dropping one would go unnoticed
        // because the others still work.
        let mut a = app();
        a.all[0].git_branch = "feature/oled-panel".into();
        a.all[0].last_prompt = "the distinctive trailing thing".into();
        a.meta.add_tag("aaaaaaaa-1", "hardware");
        a.apply_overlay();

        let id = a.all[0].id.clone();
        for (what, needle) in [
            ("title", "today's work"),
            ("folder", "/home/u"),
            ("branch", "oled-panel"),
            ("tag", "hardware"),
            ("last prompt", "distinctive trailing"),
            ("id", id.as_str()),
            ("title, differently cased", "TODAY'S WORK"),
        ] {
            a.fuzzy = needle.to_string();
            a.rebuild();
            let found = a.view.iter().any(|r| match r {
                Row::Item(i) => a.all[*i].id == id,
                _ => false,
            });
            assert!(found, "filtering by {what} ({needle:?}) did not find it");
        }

        a.fuzzy = "zzzz-definitely-not-there".into();
        a.rebuild();
        assert_eq!(
            a.item_count(),
            0,
            "a filter matching nothing matched something"
        );
    }

    fn deliver_hits(a: &mut App, paths: &[&str]) {
        let mut hits = HashMap::new();
        for p in paths {
            hits.insert((*p).to_string(), "…".to_string());
        }
        a.deep_tx
            .send(DeepResult {
                generation: a.deep_generation,
                hits,
            })
            .unwrap();
        a.absorb_deep();
    }

    #[test]
    fn clearing_a_note_says_it_was_cleared() {
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();

        a.input = "remember the raidz2".into();
        a.commit_note();
        assert_eq!(a.status, "note saved");
        assert_eq!(a.current().unwrap().note, "remember the raidz2");

        a.input.clear();
        a.commit_note();
        assert!(a.current().unwrap().note.is_empty());
        assert_eq!(a.status, "note removed", "it said {:?}", a.status);

        a.input = "   ".into();
        a.commit_note();
        assert_ne!(a.status, "note saved", "nothing was saved");
    }

    #[test]
    fn turning_one_filter_off_does_not_claim_they_are_all_off() {
        // Toggling favourites-only off said "all sessions" while a tag
        // filter was still narrowing the list to one.
        let mut a = app();
        a.tag_filter = Some("eft".into());
        a.rebuild();
        a.do_action(Action::FavOnly); // on
        a.do_action(Action::FavOnly); // off again
        assert!(
            !a.status.contains("all sessions"),
            "claimed everything is shown while #eft is still filtering: {:?}",
            a.status
        );
        assert!(
            a.status.contains(&a.item_count().to_string()),
            "should say how many are shown: {:?}",
            a.status
        );

        // with genuinely nothing else on, "all" is true and may be said
        let mut a = app();
        a.do_action(Action::LiveOnly);
        a.do_action(Action::LiveOnly);
        assert!(a.status.contains("all"), "{:?}", a.status);
    }

    #[test]
    fn the_reindex_message_counts_the_index_not_the_filter() {
        // After R it said "reindexed — 1 sessions" because one row was
        // visible, when the index held hundreds.
        let mut a = app();
        a.fuzzy = "ancient".into();
        a.rebuild();
        assert_eq!(a.item_count(), 1);
        let msg = a.reindex_message();
        let total = a.all.iter().filter(|s| !s.is_subagent).count();
        assert!(
            msg.contains(&total.to_string()),
            "reported the filter, not the index: {msg:?}"
        );
    }

    #[test]
    fn the_match_count_describes_the_list_you_are_looking_at() {
        // The count came from the search, the list came from the search
        // *and* every other filter. With a tag filter or a date range also
        // on, the header claimed matches the list did not show, and there
        // was no way to tell which number was wrong.
        let mut a = app();
        a.deep = "thing".into();
        a.start_deep();
        deliver_hits(
            &mut a,
            &[
                "/p/aaaaaaaa-1.jsonl",
                "/p/bbbbbbbb-2.jsonl",
                "/p/cccccccc-3.jsonl",
            ],
        );
        let count_in = |s: &str| {
            s.split_whitespace()
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .expect("a count")
        };
        assert_eq!(
            count_in(&a.status),
            a.item_count(),
            "unfiltered, these must agree"
        );

        // the realistic order: a filter is already on when you search
        let mut a = app();
        a.fav_only = true;
        a.deep = "thing".into();
        a.start_deep();
        deliver_hits(
            &mut a,
            &[
                "/p/aaaaaaaa-1.jsonl",
                "/p/bbbbbbbb-2.jsonl",
                "/p/cccccccc-3.jsonl",
            ],
        );
        assert_eq!(
            count_in(&a.status),
            a.item_count(),
            "the header promised matches the list does not show: {:?}",
            a.status
        );
        assert!(
            a.status.contains("other filters"),
            "it should say why the numbers differ: {:?}",
            a.status
        );
    }

    #[test]
    fn a_cleared_search_stays_cleared_when_its_answer_turns_up() {
        // The search runs on a thread and is matched to the query by a
        // generation number. Clearing did not bump it, so a query already
        // in flight still counted as current: clear the box and the hits
        // reappear a moment later, from a search you abandoned.
        let mut a = app();
        a.deep = "zpool".into();
        a.start_deep();
        let stale = a.deep_generation;

        a.deep.clear();
        a.start_deep();
        assert!(a.deep_hits.is_none(), "clearing should drop the hits");

        // the abandoned search finishes now
        let mut hits = HashMap::new();
        hits.insert("/p/aaaaaaaa-1.jsonl".to_string(), "…zpool…".to_string());
        a.deep_tx
            .send(DeepResult {
                generation: stale,
                hits,
            })
            .unwrap();
        a.absorb_deep();

        assert!(
            a.deep_hits.is_none(),
            "an abandoned search put its results back on screen"
        );
    }

    #[test]
    fn clearing_the_filters_also_abandons_a_running_search() {
        let mut a = app();
        a.deep = "zpool".into();
        a.start_deep();
        let stale = a.deep_generation;
        a.do_action(Action::Clear);

        let mut hits = HashMap::new();
        hits.insert("/p/aaaaaaaa-1.jsonl".to_string(), "…".to_string());
        a.deep_tx
            .send(DeepResult {
                generation: stale,
                hits,
            })
            .unwrap();
        a.absorb_deep();
        assert!(a.deep_hits.is_none(), "c cleared it and it came back");
        assert!(!a.deep_busy);
    }

    #[test]
    fn backing_out_of_the_search_box_abandons_a_running_search() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        a.deep = "zpool".into();
        // what start_deep does, without the thread
        a.deep_generation += 1;
        a.deep_busy = true;
        let stale = a.deep_generation;
        a.on_key(KeyEvent::from(KeyCode::Char('F')));
        a.on_key(KeyEvent::from(KeyCode::Esc));

        let mut hits = HashMap::new();
        hits.insert("/p/aaaaaaaa-1.jsonl".to_string(), "…".to_string());
        a.deep_tx
            .send(DeepResult {
                generation: stale,
                hits,
            })
            .unwrap();
        a.absorb_deep();
        assert!(
            a.deep_hits.is_none(),
            "Esc cleared the box and the search came back: {:?}",
            a.status
        );
    }

    #[test]
    fn a_later_search_wins_over_an_earlier_one() {
        let mut a = app();
        a.deep = "first".into();
        a.start_deep();
        let first = a.deep_generation;
        a.deep = "second".into();
        a.start_deep();

        let mut hits = HashMap::new();
        hits.insert("/p/aaaaaaaa-1.jsonl".to_string(), "stale".to_string());
        a.deep_tx
            .send(DeepResult {
                generation: first,
                hits,
            })
            .unwrap();
        a.absorb_deep();
        assert!(
            a.deep_hits.is_none() || a.deep_hits.as_ref().unwrap().is_empty(),
            "results from the previous query were shown"
        );
    }

    #[test]
    fn every_sort_actually_sorts_by_what_it_says() {
        // A comparator pointing at the wrong field, or the wrong way round,
        // looks plausible on screen: the list is still in *an* order.
        let mut a = app();
        a.show_subagents = false;
        for _ in 0..8 {
            let sort = a.sort;
            a.rebuild();
            let seen: Vec<&Session> = a
                .view
                .iter()
                .filter_map(|r| match r {
                    Row::Item(i) => Some(&a.all[*i]),
                    _ => None,
                })
                .collect();

            // favourites float to the top, so check the order within each
            // group rather than across the boundary
            for group in seen.chunk_by(|x, y| x.favorite == y.favorite) {
                for pair in group.windows(2) {
                    let (x, y) = (pair[0], pair[1]);
                    let ok = match sort {
                        Sort::Recency => x.mtime >= y.mtime,
                        Sort::Size => x.size >= y.size,
                        Sort::Entries => x.entries >= y.entries,
                        Sort::Duration => x.duration_secs() >= y.duration_secs(),
                        Sort::Title => x.title().to_lowercase() <= y.title().to_lowercase(),
                        Sort::Folder => x.cwd <= y.cwd,
                        Sort::Tokens => x.total_tokens() >= y.total_tokens(),
                    };
                    assert!(
                        ok,
                        "{:?}: {:?} came before {:?}",
                        sort,
                        x.title(),
                        y.title()
                    );
                }
            }
            a.do_action(Action::CycleSort);
        }
    }

    #[test]
    fn a_flat_listing_can_actually_include_subagents() {
        // --list and --json have no way to expand a parent, so revealing
        // subagents there has to mean showing them. The flag claimed to
        // "start with subagent transcripts revealed" and changed nothing:
        // 348 rows with it, 348 without, while --stats counted 755.
        let mut a = app();
        a.show_subagents = true;
        a.expand_all();
        a.rebuild();
        let subs = a.view.iter().filter(|r| matches!(r, Row::Sub(_))).count();
        assert!(subs > 0, "revealing subagents revealed none");
        assert_eq!(subs, a.all.iter().filter(|s| s.is_subagent).count());
    }

    #[test]
    fn subagents_are_children_not_entries() {
        let a = app();
        assert_eq!(a.item_count(), 8, "the two subagents are not top-level");
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
    fn favourites_get_their_own_band_rather_than_breaking_the_dates() {
        // Pinned rows sit above everything regardless of age. Banding them by
        // date made the list read "today, yesterday, today, yesterday" as it
        // crossed back into time order.
        let mut m = crate::meta::Meta::default();
        m.toggle_favorite("aaaaaaaa-1"); // today
        m.toggle_favorite("eeeeeeee-5"); // 300 days old
        let a = app_with(m, true);
        let bands: Vec<String> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Divider(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(bands.first().map(String::as_str), Some("favourites"));
        let mut uniq = bands.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(bands.len(), uniq.len(), "a band repeated: {bands:?}");
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
    fn a_wsx_workspace_is_grouped_under_its_name() {
        // Every workspace is its own folder under one long shared prefix, so
        // grouping by the path made headings that differed only past the
        // point where anyone reads.
        let mut a = app();
        a.do_action(Action::GroupByDir);
        let heads: Vec<String> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Header(d, _) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert!(
            heads.iter().any(|h| h == "OS-DEV/shy-daffodil"),
            "{heads:?}"
        );
        assert!(heads.iter().any(|h| h == "OS-DEV/gdisk-app"), "{heads:?}");
        assert!(
            !heads.iter().any(|h| h.contains(".local/state")),
            "a heading still spells out the worktree: {heads:?}"
        );
    }

    fn shown_ids(a: &App) -> Vec<String> {
        let mut v: Vec<String> = a
            .view
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(a.all[*i].id.clone()),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn one_repos_workspaces_are_one_tag_away() {
        // Every workspace is a folder of its own, so no folder filter could
        // pull one project's sessions together. The repo's tag does, without
        // anyone having tagged anything.
        let mut a = app();
        a.do_action(Action::TagFilter);
        a.input = "wsx/OS-DEV".into(); // typed as the repo is written
        a.commit_tag_filter();
        assert_eq!(a.tag_filter.as_deref(), Some("wsx/os-dev"));
        assert_eq!(shown_ids(&a), vec!["gggggggg-7", "hhhhhhhh-8"]);

        a.input = "wsx/nothing-by-that-name".into();
        a.commit_tag_filter();
        assert_eq!(a.item_count(), 0);

        // an ordinary tag still means what it did
        a.input = "eft".into();
        a.commit_tag_filter();
        assert_eq!(shown_ids(&a), vec!["cccccccc-3"]);
    }

    #[test]
    fn the_filter_finds_a_workspace_by_the_name_the_list_shows() {
        let mut a = app();
        for needle in ["OS-DEV/shy", "shy-daffodil", "wsx/os-dev"] {
            a.fuzzy = needle.into();
            a.rebuild();
            let ids = shown_ids(&a);
            assert!(
                ids.contains(&"gggggggg-7".to_string()),
                "{needle:?} found {ids:?}"
            );
        }
        a.fuzzy = "wsx/os-dev".into();
        a.rebuild();
        assert!(
            shown_ids(&a)
                .iter()
                .all(|id| id == "gggggggg-7" || id == "hhhhhhhh-8"),
            "{:?}",
            shown_ids(&a)
        );
    }

    #[test]
    fn showing_one_tag_offers_the_automatic_ones_and_tagging_does_not() {
        let mut a = app();
        a.input_mode = InputMode::TagFilter;
        a.input = "ws".into();
        assert_eq!(a.tag_completions(), vec!["wsx/os-dev (2)"]);
        a.input.clear();
        assert!(
            a.tag_completions().contains(&"wsx/os-dev (2)".to_string()),
            "{:?}",
            a.tag_completions()
        );

        // tagging would refuse it, so it is not offered there
        a.input_mode = InputMode::TagAdd;
        a.input = "ws".into();
        assert!(a.tag_completions().is_empty(), "{:?}", a.tag_completions());
    }

    #[test]
    fn tab_completes_the_tag_being_typed_not_the_whole_prompt() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        for (typed, completed) in [
            ("ef", "eft"),
            ("-ef", "-eft"),
            ("rig ef", "rig eft"),
            ("rig,ef", "rig,eft"),
            ("eft>ef", "eft>eft"),
        ] {
            a.input_mode = InputMode::TagAdd;
            a.input = typed.into();
            a.on_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(a.input, completed, "from {typed:?}");
        }
    }

    #[test]
    fn tab_in_a_note_or_a_tmux_name_is_not_tag_completion() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        for mode in [InputMode::Note, InputMode::TmuxName] {
            a.input_mode = mode;
            a.input.clear();
            a.on_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(a.input, "", "{mode:?} took a tag for its text");
        }
    }

    #[test]
    fn enter_closes_the_note_prompt_with_nothing_to_put_it_on() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        a.fuzzy = "zzzz-nothing-matches".into();
        a.rebuild();
        a.on_key(KeyEvent::from(KeyCode::Char('N')));
        a.on_key(KeyEvent::from(KeyCode::Char('x')));
        a.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(a.input_mode, InputMode::Normal);
    }

    #[test]
    fn an_automatic_tag_cannot_be_given_by_hand() {
        // Stored, it would go on disagreeing with the folder it came from:
        // a session tagged wsx/os-dev by hand is in no workspace at all.
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();

        type_tag(&mut a, "wsx/os-dev");
        assert!(tags_of(&a, &id).is_empty(), "{:?}", tags_of(&a, &id));
        assert!(a.status.contains("automatic"), "{:?}", a.status);

        type_tag(&mut a, "rig WSX/Other");
        assert_eq!(tags_of(&a, &id), vec!["rig"], "the ordinary one is kept");
        assert!(
            a.status.contains("tagged #rig") && a.status.contains("wsx/other"),
            "it should say what it did and what it left off: {:?}",
            a.status
        );

        type_tag(&mut a, "rig>wsx/os-dev");
        assert_eq!(
            tags_of(&a, &id),
            vec!["rig"],
            "renamed into an automatic tag"
        );
        assert!(a.status.contains("automatic"), "{:?}", a.status);

        assert!(
            a.meta
                .all_tags()
                .iter()
                .all(|(t, _)| !t.starts_with(crate::wsx::TAG_PREFIX)),
            "one reached meta.json: {:?}",
            a.meta.all_tags()
        );
    }

    #[test]
    fn a_session_that_did_not_run_in_wsx_has_no_workspace() {
        let a = app();
        for s in &a.all {
            let under = s.cwd.starts_with(&format!("{WSX_ROOT}/"));
            assert_eq!(s.wsx.is_some(), under, "{:?}", s.cwd);
        }
        let s = a.all.iter().find(|s| s.id == "gggggggg-7").unwrap();
        assert_eq!(s.folder(), "OS-DEV/shy-daffodil");
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
    fn the_preview_follows_a_session_that_grew() {
        // Remembered by path alone, the rail showed a live session as it was
        // the first time the cursor landed on it, however far it had got
        // since and however many rescans had seen it.
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("live.jsonl");
        let said = |role: &str, t: &str| {
            format!(
                r#"{{"parentUuid":"p","message":{{"role":"{role}","content":[{{"type":"text","text":"{t}"}}]}},"type":"{role}"}}"#
            ) + "\n"
        };
        std::fs::write(&path, said("user", "first question")).unwrap();
        let mut a = app();
        let i = a.current_idx().unwrap();
        a.all[i].path = path.clone();
        a.all[i].size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(a.preview(8).len(), 1);

        let mut more = std::fs::read_to_string(&path).unwrap();
        more.push_str(&said("assistant", "an answer"));
        more.push_str(&said("user", "second question"));
        std::fs::write(&path, more).unwrap();
        // what a rescan tells it
        a.all[i].size = std::fs::metadata(&path).unwrap().len();
        let turns = a.preview(8);
        assert_eq!(
            turns.len(),
            3,
            "{:?}",
            turns.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
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
    fn the_today_filter_and_the_today_band_mean_the_same_day() {
        // The band is calendar-based -- "today" is this date, in local time
        // -- while the filter was a rolling 24 hours. At nine in the
        // morning, a session from yesterday afternoon sat under a
        // "yesterday" heading and was still shown by the "today" filter.
        use chrono::{Local, TimeZone};
        let midnight = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|d| Local.from_local_datetime(&d).single())
            .expect("local midnight")
            .timestamp();

        // just before and just after local midnight, plus a rolling-24h
        // point that is calendar-yesterday
        for (label, t) in [
            ("a minute into today", midnight + 60),
            ("a minute before midnight", midnight - 60),
            ("late yesterday", midnight - 3600),
            ("earlier today", Local::now().timestamp() - 5),
        ] {
            let mut a = app();
            a.all[0].mtime = t;
            a.date = DateRange::Today;
            a.rebuild();
            let shown = a.view.iter().any(|r| matches!(r, Row::Item(0)));
            let banded_today = date_band(t) == "today";
            assert_eq!(
                shown,
                banded_today,
                "{label}: the filter says {shown} and the band says {:?}",
                date_band(t)
            );
        }
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

    fn tags_of(a: &App, id: &str) -> Vec<String> {
        a.meta.get(id).map(|e| e.tags.clone()).unwrap_or_default()
    }

    fn type_tag(a: &mut App, text: &str) {
        a.input = text.into();
        a.commit_tag_add();
    }

    #[test]
    fn removing_several_tags_works_the_way_adding_them_does() {
        // "eft rig" adds two tags. "-eft rig" looked like it removed the
        // same two and instead asked for one tag called "eft-rig", which
        // nothing has -- while the status line said it had removed them.
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();

        type_tag(&mut a, "eft rig, spare");
        assert_eq!(tags_of(&a, &id), vec!["eft", "rig", "spare"]);

        type_tag(&mut a, "-eft rig");
        assert_eq!(
            tags_of(&a, &id),
            vec!["spare"],
            "removing two at once left them behind"
        );
    }

    #[test]
    fn renaming_onto_an_existing_tag_merges_rather_than_duplicating() {
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();
        type_tag(&mut a, "draft final");
        type_tag(&mut a, "draft>final");
        assert_eq!(
            tags_of(&a, &id),
            vec!["final"],
            "a tag ended up on the session twice"
        );
    }

    #[test]
    fn a_rename_that_cannot_happen_says_so() {
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();
        type_tag(&mut a, "keeper");

        // no new name at all
        type_tag(&mut a, "keeper>");
        assert_eq!(tags_of(&a, &id), vec!["keeper"], "the tag was lost");
        assert!(
            !a.status.contains("renamed #keeper to #"),
            "claimed a rename with nothing to rename to: {:?}",
            a.status
        );

        // a name nothing carries
        type_tag(&mut a, "nosuchtag>other");
        assert!(
            !a.status.contains("renamed #nosuchtag to #other on 0"),
            "reads as success: {:?}",
            a.status
        );
    }

    #[test]
    fn a_rename_that_renamed_nothing_leaves_the_tag_filter_alone() {
        let mut a = app();
        a.tag_filter = Some("eft".into());
        a.rebuild();
        let shown = a.item_count();
        assert!(shown > 0);

        type_tag(&mut a, "eft>");
        assert_eq!(a.tag_filter.as_deref(), Some("eft"), "{:?}", a.status);

        // automatic, so never stored, so nothing to rename
        a.tag_filter = Some("wsx/os-dev".into());
        a.rebuild();
        type_tag(&mut a, "wsx/os-dev>osdev");
        assert_eq!(
            a.tag_filter.as_deref(),
            Some("wsx/os-dev"),
            "{:?}",
            a.status
        );

        // and one that did rename follows the new name
        a.tag_filter = Some("eft".into());
        a.rebuild();
        type_tag(&mut a, "eft>tarkov");
        assert_eq!(a.tag_filter.as_deref(), Some("tarkov"));
        assert_eq!(a.item_count(), shown);
    }

    #[test]
    fn a_tag_operation_that_changed_nothing_does_not_claim_otherwise() {
        let mut a = app();
        let id = a.all[0].id.clone();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap();

        type_tag(&mut a, "-never-had-this");
        assert!(
            !a.status.contains("removed #never-had-this from 1"),
            "said it removed a tag that was not there: {:?}",
            a.status
        );

        // a name that survives nothing of itself is not a tag
        type_tag(&mut a, "!!!");
        assert!(tags_of(&a, &id).is_empty(), "{:?}", tags_of(&a, &id));
        assert!(
            !a.status.starts_with("tagged !!!"),
            "claimed to tag with nothing: {:?}",
            a.status
        );
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

    /// The cursor on the first of aaaaaaaa-1's subagents, shown.
    fn on_a_subagent_of_a(a: &mut App) {
        a.show_subagents = true;
        a.expanded.insert("aaaaaaaa-1".into());
        a.rebuild();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Sub(_)))
            .expect("a subagent row");
    }

    #[test]
    fn a_subagent_of_something_running_is_refused_like_its_parent() {
        // Enter on a subagent resumes its parent. The guard asked the
        // subagent whether it was running, and a subagent never is, so the
        // parent was resumed here a second time.
        let mut a = app();
        a.all[0].live_exact = true;
        a.all[0].live_pid = Some(4242);
        on_a_subagent_of_a(&mut a);
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none(), "resumed: {:?}", a.outcome);
        assert!(a.status.contains("4242"), "status was {:?}", a.status);

        let mut a = app();
        a.all[0].has_tmux = true;
        a.all[0].tmux_session = "mine".into();
        on_a_subagent_of_a(&mut a);
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none(), "resumed: {:?}", a.outcome);
        assert!(a.status.contains("ctrl+t"), "status was {:?}", a.status);
    }

    #[test]
    fn a_subagent_of_something_in_tmux_goes_to_that_tmux() {
        let mut a = app();
        a.all[0].has_tmux = true;
        a.all[0].tmux_session = "some-name-i-chose".into();
        on_a_subagent_of_a(&mut a);
        a.do_action(Action::Tmux);
        assert_eq!(a.input_mode, InputMode::Normal, "asked for a new name");
        assert_eq!(a.tmux_name, "some-name-i-chose");
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

    fn offer(a: &mut App, ids: &[&str]) {
        a.reopen = ids
            .iter()
            .map(|id| crate::workspace::Entry {
                id: (*id).to_string(),
                cwd: "/home/u".into(),
                model: "claude-opus-5".into(),
                perms: "bypassPermissions".into(),
                title: format!("work in {id}"),
            })
            .collect();
    }

    fn press(a: &mut App, c: char) {
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }

    #[test]
    fn taking_the_offer_reopens_every_one_of_them() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1", "bbbbbbbb-2"]);
        press(&mut a, 'r');
        let (target, targets) = a.to_open.first().expect("the offer did nothing");
        assert_eq!(targets.len(), 2, "only part of the set came back");
        assert_eq!(
            *target,
            Target::WindowTmux,
            "restored sessions need tmux under them, or closing the window kills them"
        );
        // the recorded permission mode has to survive, or a restored session
        // comes back asking about every edit
        assert_eq!(targets[0].perms, "bypassPermissions");
        assert!(!a.quit, "they open in their own windows; the picker stays");
    }

    #[test]
    fn taking_the_offer_skips_what_has_been_opened_since() {
        // Worked out once, when the browser started: a session opened from
        // the list since, or started somewhere else, was reopened again.
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1", "bbbbbbbb-2", "cccccccc-3"]);
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "aaaaaaaa-1"))
            .unwrap();
        a.do_action(Action::NewWindow);
        let b = a.all.iter().position(|s| s.id == "bbbbbbbb-2").unwrap();
        a.all[b].live_exact = true;
        a.all[b].live_pid = Some(4242);
        a.to_open.clear();

        press(&mut a, 'r');
        let (_, targets) = a.to_open.first().expect("the offer did nothing");
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["cccccccc-3"]);
        assert!(a.status.contains("2 open already"), "{:?}", a.status);
    }

    #[test]
    fn a_guess_by_folder_is_not_counted_as_running() {
        let mut a = app();
        let b = a.all.iter().position(|s| s.id == "bbbbbbbb-2").unwrap();
        a.all[b].live_pid = Some(4242); // matched by folder only
        a.all[b].live_exact = false;
        assert!(!a.running_ids().contains("bbbbbbbb-2"));
        a.all[b].live_exact = true;
        assert!(a.running_ids().contains("bbbbbbbb-2"));
    }

    #[test]
    fn the_offer_does_not_reuse_the_last_tmux_name_typed() {
        let mut a = app();
        press(&mut a, 'W');
        for c in "eft work".chars() {
            press(&mut a, c);
        }
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        assert_eq!(a.tmux_name, "eft-work");
        a.to_open.clear(); // the loop has handed that one to the shell

        offer(&mut a, &["bbbbbbbb-2", "cccccccc-3"]);
        press(&mut a, 'r');
        assert_eq!(a.to_open.len(), 1);
        assert!(
            a.tmux_name.is_empty(),
            "reopened under the name typed for another: {:?}",
            a.tmux_name
        );
    }

    #[test]
    fn space_takes_one_session_out_of_the_offer() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1", "bbbbbbbb-2"]);
        // put the cursor on the first offered session
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "aaaaaaaa-1"))
            .unwrap();
        press(&mut a, ' ');

        assert_eq!(a.reopen.len(), 1, "space did not narrow the offer");
        assert_eq!(a.reopen[0].id, "bbbbbbbb-2", "dropped the wrong one");
        assert!(
            a.selected.is_empty(),
            "narrowing the offer must not start an unrelated selection"
        );

        press(&mut a, 'r');
        let (_, targets) = a.to_open.first().expect("nothing reopened");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "bbbbbbbb-2");
    }

    #[test]
    fn space_on_a_row_outside_the_offer_still_selects_it() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1"]);
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "cccccccc-3"))
            .unwrap();
        press(&mut a, ' ');
        assert_eq!(a.selected.len(), 1, "ordinary selection stopped working");
        assert_eq!(a.reopen.len(), 1, "the offer should be untouched");
    }

    #[test]
    fn dropping_the_last_one_is_the_same_as_declining() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1"]);
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "aaaaaaaa-1"))
            .unwrap();
        press(&mut a, ' ');
        assert!(a.reopen.is_empty());
        assert!(
            a.status.contains("--reopen"),
            "should say how to get it back: {}",
            a.status
        );
        // and the offer is gone, so r does nothing again
        press(&mut a, 'r');
        assert!(a.outcome.is_none() && a.to_open.is_empty());
    }

    #[test]
    fn opening_a_window_leaves_the_picker_open() {
        // A window of its own does not need this one. Closing the browser
        // every time meant you could not open a second session without
        // starting over, and it looked like the tool had crashed.
        let mut a = app();
        a.do_action(Action::NewWindow);
        assert!(!a.quit, "the picker closed to open a window elsewhere");
        assert!(
            a.outcome.is_none(),
            "it should be streamed, not held to exit"
        );
        assert_eq!(a.to_open.len(), 1);
        assert_eq!(a.to_open[0].0, Target::Window);

        // and again, so several can be opened in one sitting
        a.move_by(1);
        a.do_action(Action::NewWindow);
        assert!(!a.quit);
        assert_eq!(a.to_open.len(), 2, "the second one did not queue");
    }

    #[test]
    fn the_rescan_landing_does_not_move_you() {
        // The rescan now finishes behind the list, so the rows are replaced
        // under you while you are reading them. Keeping the cursor on the
        // same *index* would move you to a different session whenever the
        // order changed, which is most of the time -- it sorts by recency.
        let mut a = app();
        a.move_by(2);
        let was = a.current().map(|s| s.id.clone()).expect("no row");

        // what the background rescan does: newer data, different order
        let mut fresh = corpus();
        fresh.reverse();
        fresh[0].mtime += 10_000;
        a.absorb_rescan(fresh);

        assert_eq!(
            a.current().map(|s| s.id.clone()),
            Some(was),
            "the cursor jumped to another session when the index landed"
        );
        assert_cursor_valid(&a);
    }

    #[test]
    fn a_rescan_that_removes_rows_cannot_strand_the_cursor() {
        let mut a = app();
        a.goto_bottom();
        a.absorb_rescan(vec![session("only-one", "all that is left", "/home/u", 0)]);
        assert_cursor_valid(&a);
        assert_eq!(a.item_count(), 1);
    }

    #[test]
    fn a_session_just_opened_elsewhere_is_not_resumed_here_as_well() {
        // The guard against a second client on one transcript reads the
        // process table, which is polled every few seconds. Opening a
        // window and pressing enter on the same row beats that poll, and
        // the whole point of the guard is that it should not be beatable.
        let mut a = app();
        a.do_action(Action::NewWindow);
        assert_eq!(a.launched.len(), 1);
        let opened = a.launched[0].id.clone();
        assert_eq!(a.current().map(|s| s.id.clone()), Some(opened));

        a.do_action(Action::Resume);
        assert!(
            !a.quit,
            "it resumed here on top of the window it just opened"
        );
        assert!(
            a.status.contains("already"),
            "it should say why, got {:?}",
            a.status
        );
    }

    #[test]
    fn a_window_opened_mid_session_is_still_recorded() {
        // The record of what was open is what a reboot is put back from.
        // Once windows streamed out instead of ending the run, they stopped
        // appearing in the final outcome -- and so stopped being recorded,
        // which would have quietly lost exactly the sessions you opened.
        let mut a = app();
        a.do_action(Action::NewWindow);
        a.move_by(1);
        a.do_action(Action::NewWindow);
        assert_eq!(a.launched.len(), 2, "opened windows were not remembered");
        assert!(
            a.launched
                .iter()
                .all(|t| !t.id.is_empty() && !t.cwd.is_empty()),
            "a record with no id or folder cannot be reopened"
        );
    }

    #[test]
    fn taking_the_reboot_offer_is_remembered_too() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1", "bbbbbbbb-2"]);
        press(&mut a, 'r');
        assert_eq!(a.launched.len(), 2);
    }

    #[test]
    fn landing_here_still_closes_it() {
        // Resuming in this terminal does need the picker gone: the shell has
        // to cd and hand the terminal over.
        let mut a = app();
        a.do_action(Action::Resume);
        assert!(a.quit);
        assert!(a.outcome.is_some());
        assert!(a.to_open.is_empty());
    }

    #[test]
    fn w_opens_a_window_with_tmux_under_it() {
        let mut a = app();
        press(&mut a, 'W');
        // it asks what to call the tmux session first
        assert_eq!(a.input_mode, InputMode::TmuxName);
        assert!(a.outcome.is_none(), "resumed before asking");

        a.input = "eft work!".into();
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        let (target, _) = a.to_open.first().expect("nothing was opened");
        assert_eq!(*target, Target::WindowTmux);
        assert!(
            !a.quit,
            "a window of its own is no reason to close the picker"
        );
        assert_eq!(
            a.tmux_name, "eft-work",
            "the name was not made safe for tmux"
        );
    }

    #[test]
    fn an_empty_name_keeps_the_generated_one() {
        let mut a = app();
        a.do_action(Action::Tmux);
        assert_eq!(a.input_mode, InputMode::TmuxName);
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        assert!(a.tmux_name.is_empty(), "should fall back to mn-<id>");
        assert!(matches!(
            a.outcome,
            Some(Outcome::Resume {
                target: Target::Tmux,
                ..
            })
        ));
    }

    #[test]
    fn backing_out_of_the_name_prompt_cancels_the_resume() {
        let mut a = app();
        a.do_action(Action::Tmux);
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Esc,
        ));
        assert_eq!(a.input_mode, InputMode::Normal);
        assert!(a.outcome.is_none(), "escape left a resume armed");
        // and the next enter must resume here, not into the abandoned tmux
        a.do_action(Action::Resume);
        assert!(matches!(
            a.outcome,
            Some(Outcome::Resume {
                target: Target::Here,
                ..
            })
        ));
    }

    #[test]
    fn a_chat_already_in_tmux_is_not_asked_about_again() {
        // It is already running somewhere; the question is which session to
        // go to, not what to call a second one.
        let mut a = app();
        a.all[0].has_tmux = true;
        a.all[0].tmux_session = "some-name-i-chose".into();
        a.rebuild();
        a.do_action(Action::Tmux);
        assert_eq!(
            a.input_mode,
            InputMode::Normal,
            "asked about an existing one"
        );
        assert_eq!(a.tmux_name, "some-name-i-chose");
    }

    fn on(a: &mut App, id: &str) {
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == id))
            .unwrap_or_else(|| panic!("{id} is not in view"));
    }

    fn shy_daffodil() -> crate::wsx::Jump {
        crate::wsx::Jump {
            repo: "OS-DEV".into(),
            slug: "shy-daffodil".into(),
            worktree: format!("{WSX_ROOT}/OS-DEV/shy-daffodil"),
        }
    }

    /// Another session in a fixture workspace's folder, and the list redone.
    fn add_session(a: &mut App, id: &str, cwd: &str, age_days: i64) {
        a.all.push(session(id, "another look at it", cwd, age_days));
        a.apply_overlay();
        a.rebuild();
    }

    #[test]
    fn enter_on_a_live_workspace_goes_to_wsx_instead() {
        // wsx is already running this workspace's agent. Resuming here
        // started a second claude beside it, on the same conversation.
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert_eq!(a.to_jump, vec![shy_daffodil()]);
        assert!(a.outcome.is_none(), "resumed here as well");
        assert!(!a.quit, "the picker stays up, as it does for a window");
        assert!(
            a.to_open.is_empty() && a.launched.is_empty(),
            "wsx runs it, so it is not ours to record"
        );
    }

    #[test]
    fn an_older_conversation_in_a_live_workspace_is_refused_with_a_way_round() {
        let mut a = app();
        add_session(
            &mut a,
            "iiiiiiii-9",
            &format!("{WSX_ROOT}/OS-DEV/shy-daffodil"),
            6,
        );
        on(&mut a, "iiiiiiii-9");
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty(), "wsx would show the newer one");
        assert!(a.outcome.is_none());
        assert!(
            a.status
                .contains("OS-DEV/shy-daffodil is live in wsx on a newer session")
                && a.status.contains("ctrl+n"),
            "{:?}",
            a.status
        );
        // and the way round works
        a.do_action(Action::NewWindow);
        assert_eq!(a.to_open.len(), 1);
        assert_eq!(a.to_open[0].1[0].id, "iiiiiiii-9");

        // the newer one is still the one enter hands to wsx
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert_eq!(a.to_jump, vec![shy_daffodil()]);
    }

    #[test]
    fn the_explicit_ways_of_opening_one_are_taken_at_their_word() {
        // As for a session already running: ctrl+n, ctrl+t and W say where
        // it should go, and enter is the only one that guesses.
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::NewWindow);
        assert_eq!(a.to_open.len(), 1, "ctrl+n");
        assert!(a.to_jump.is_empty());

        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Tmux);
        a.commit_tmux_name();
        assert!(
            matches!(
                a.outcome,
                Some(Outcome::Resume {
                    target: Target::Tmux,
                    ..
                })
            ),
            "ctrl+t"
        );
        assert!(a.to_jump.is_empty());
    }

    #[test]
    fn a_selection_leaves_out_what_wsx_is_running() {
        let mut a = app();
        a.selected.insert("/p/gggggggg-7.jsonl".into());
        a.selected.insert("/p/aaaaaaaa-1.jsonl".into());
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("the rest of the selection was not resumed");
        };
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["aaaaaaaa-1"]);
        assert!(a.to_jump.is_empty(), "a selection is not a jump");
        assert!(
            a.notes
                .iter()
                .any(|n| n.contains("left out OS-DEV/shy-daffodil") && n.contains("ctrl+n")),
            "the browser closes, so it has to be said after: {:?}",
            a.notes
        );
    }

    fn resumed_ids(a: &App) -> Vec<String> {
        match &a.outcome {
            Some(Outcome::Resume { targets, .. }) => targets.iter().map(|t| t.id.clone()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn enter_opens_what_is_picked_even_when_a_filter_hides_it() {
        // Two picked, then a filter that shows neither. The header still
        // said "2 picked" and `t` still tagged both, but enter resumed the
        // row under the cursor -- a session nobody had picked.
        let mut a = app();
        a.selected.insert("/p/bbbbbbbb-2.jsonl".into());
        a.selected.insert("/p/cccccccc-3.jsonl".into());
        a.fuzzy = "ancient".into();
        a.rebuild();
        a.do_action(Action::Resume);
        assert_eq!(resumed_ids(&a), vec!["bbbbbbbb-2", "cccccccc-3"]);
    }

    #[test]
    fn a_picked_session_already_running_is_left_out() {
        // Enter on its row refuses it. Picked, it went through unguarded:
        // one pick resumed here beside the one already running.
        let mut a = app();
        a.all[0].live_exact = true;
        a.all[0].live_pid = Some(4242);
        a.selected.insert("/p/aaaaaaaa-1.jsonl".into());
        on(&mut a, "bbbbbbbb-2");
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none(), "resumed: {:?}", a.outcome);
        assert!(a.status.contains("already running"), "{:?}", a.status);

        a.selected.insert("/p/bbbbbbbb-2.jsonl".into());
        a.do_action(Action::Resume);
        assert_eq!(resumed_ids(&a), vec!["bbbbbbbb-2"]);
        assert!(
            a.notes.iter().any(|n| n.contains("1 already running")),
            "{:?}",
            a.notes
        );
    }

    #[test]
    fn a_hidden_pick_does_not_let_a_running_row_through() {
        let mut a = app();
        a.all[0].live_exact = true;
        a.all[0].live_pid = Some(4242);
        a.selected.insert("/p/bbbbbbbb-2.jsonl".into());
        a.fuzzy = "today's work".into();
        a.rebuild();
        on(&mut a, "aaaaaaaa-1");
        a.do_action(Action::Resume);
        assert_eq!(resumed_ids(&a), vec!["bbbbbbbb-2"]);
    }

    #[test]
    fn a_parent_picked_with_its_subagents_is_opened_once() {
        // Each subagent resumes its parent, so three picks were three
        // windows on one transcript.
        let mut a = app();
        a.show_subagents = true;
        a.expanded.insert("aaaaaaaa-1".into());
        a.rebuild();
        for p in [
            "/p/aaaaaaaa-1.jsonl",
            "/p/agent-a1.jsonl",
            "/p/agent-a2.jsonl",
        ] {
            a.selected.insert(p.into());
        }
        a.do_action(Action::NewWindow);
        let (_, targets) = a.to_open.first().expect("opened");
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["aaaaaaaa-1"]);
    }

    #[test]
    fn a_selection_of_nothing_but_wsxs_resumes_nothing() {
        let mut a = app();
        a.selected.insert("/p/gggggggg-7.jsonl".into());
        a.do_action(Action::Resume);
        assert!(a.outcome.is_none() && !a.quit);
        assert!(a.status.contains("OS-DEV/shy-daffodil"), "{:?}", a.status);
    }

    #[test]
    fn a_session_further_down_a_live_worktree_resumes_as_usual() {
        // `claude --continue` in the worktree never reaches it.
        let mut a = app();
        add_session(
            &mut a,
            "iiiiiiii-9",
            &format!("{WSX_ROOT}/OS-DEV/shy-daffodil/kernel"),
            0,
        );
        on(&mut a, "iiiiiiii-9");
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty());
        assert!(matches!(
            a.outcome,
            Some(Outcome::Resume {
                target: Target::Here,
                ..
            })
        ));
    }

    #[test]
    fn without_wsx_a_workspace_resumes_the_way_it_always_did() {
        let mut a = app();
        a.set_wsx(crate::wsx::State {
            root: Some(WSX_ROOT.into()),
            ..Default::default()
        });
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty());
        assert!(a.outcome.is_some() && a.quit);
    }

    #[test]
    fn something_already_running_is_refused_before_wsx_is_considered() {
        // `claude --resume <id>` running in the worktree is somebody's -- mn
        // opened it, most likely -- and a jump would not reach it.
        let mut a = app();
        on(&mut a, "gggggggg-7");
        let i = a.current_idx().unwrap();
        a.all[i].live_exact = true;
        a.all[i].live_pid = Some(4242);
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty());
        assert!(a.status.contains("4242"), "{:?}", a.status);
    }

    /// Claude processes running, as the process scan would report them.
    fn running(a: &mut App, procs: Vec<crate::live::Proc>) {
        let (mut by_id, mut by_cwd) = (HashMap::new(), HashMap::new());
        for p in procs {
            match p.resume_id.clone() {
                Some(id) => by_id.insert(id, p),
                None => by_cwd.insert(p.cwd.clone(), p),
            };
        }
        a.live = LiveMap {
            count: by_id.len() + by_cwd.len(),
            by_id,
            by_cwd,
            supported: true,
        };
        a.apply_overlay();
        a.rebuild();
    }

    fn claude(pid: i32, resume: Option<&str>, cwd: &str, by_wsx: bool) -> crate::live::Proc {
        crate::live::Proc {
            pid,
            cwd: cwd.into(),
            resume_id: resume.map(str::to_string),
            model: None,
            under_wsx: by_wsx,
        }
    }

    #[test]
    fn an_agent_wsx_resumed_by_id_is_switched_to_rather_than_refused() {
        // wsx puts an agent back on its own conversation with `--resume
        // <id>`, which the process scan matches exactly -- and the guard
        // against a second client read that as somebody's running session
        // and refused, so enter never reached wsx at all.
        let mut a = app();
        let tree = format!("{WSX_ROOT}/OS-DEV/shy-daffodil");
        running(&mut a, vec![claude(14939, Some("gggggggg-7"), &tree, true)]);
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert_eq!(a.to_jump, vec![shy_daffodil()], "{:?}", a.status);
        assert!(!a.status.contains("already running"), "{:?}", a.status);
    }

    #[test]
    fn a_plain_claude_wsx_started_is_its_agent_too() {
        // A fresh agent, or one carried on with `--continue`: no id on the
        // command line, so only its folder says which session it is.
        let mut a = app();
        let tree = format!("{WSX_ROOT}/OS-DEV/shy-daffodil");
        running(&mut a, vec![claude(2794, None, &tree, true)]);
        let s = a.all.iter().find(|s| s.id == "gggggggg-7").unwrap();
        assert!(s.live_in_wsx && !s.live_exact);
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert_eq!(a.to_jump, vec![shy_daffodil()]);
    }

    #[test]
    fn each_of_several_agents_is_switched_to_on_its_own_conversation() {
        // A workspace can run more than one agent. The second is on an older
        // conversation, and it is still wsx's -- while the newest, which no
        // agent is running, is not what a jump would show.
        let mut a = app();
        let tree = format!("{WSX_ROOT}/OS-DEV/shy-daffodil");
        add_session(&mut a, "iiiiiiii-9", &tree, 6);
        running(&mut a, vec![claude(1168, Some("iiiiiiii-9"), &tree, true)]);
        on(&mut a, "iiiiiiii-9");
        a.do_action(Action::Resume);
        assert_eq!(a.to_jump, vec![shy_daffodil()], "{:?}", a.status);

        a.to_jump.clear();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty());
        assert!(
            a.status.contains("live in wsx on another session") && a.status.contains("ctrl+n"),
            "{:?}",
            a.status
        );
    }

    #[test]
    fn a_claude_started_some_other_way_is_still_refused_as_running() {
        // `claude --resume` in the worktree that wsx did not start -- a
        // window mn opened, say. A jump would not reach it.
        let mut a = app();
        let tree = format!("{WSX_ROOT}/OS-DEV/shy-daffodil");
        running(&mut a, vec![claude(4242, Some("gggggggg-7"), &tree, false)]);
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        assert!(a.to_jump.is_empty());
        assert!(
            a.status.contains("already running as pid 4242"),
            "{:?}",
            a.status
        );
    }

    #[test]
    fn wsxs_agents_are_left_out_of_the_reboot_record() {
        // wsx puts them back itself when it starts. Offering them again
        // after a reboot would open each a second time.
        let mut a = app();
        let tree = format!("{WSX_ROOT}/OS-DEV/shy-daffodil");
        running(
            &mut a,
            vec![
                claude(14939, Some("gggggggg-7"), &tree, true),
                claude(4242, Some("aaaaaaaa-1"), "/home/u", false),
            ],
        );
        let ids: Vec<String> = a.open_sessions().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["aaaaaaaa-1"]);
    }

    #[test]
    fn a_jump_asks_wsx_again_and_goes_by_the_name_it_has_now() {
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        // renamed while the list was up: same worktree, new slug
        let mut fresh = a.wsx.clone();
        fresh.workspaces[0].slug = "page-tables".into();
        let sent = std::cell::RefCell::new(Vec::new());
        a.finish_jumps(fresh, |j| {
            sent.borrow_mut().push(j.label());
            Ok(())
        });
        assert_eq!(*sent.borrow(), vec!["OS-DEV/page-tables"]);
        assert_eq!(a.status, "switched to OS-DEV/page-tables in wsx");
        assert!(a.to_jump.is_empty(), "a jump is made once");
        let s = a.all.iter().find(|s| s.id == "gggggggg-7").unwrap();
        assert_eq!(s.folder(), "OS-DEV/page-tables", "and the list says so too");
    }

    #[test]
    fn a_workspace_archived_in_the_meantime_is_not_jumped_to() {
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        let mut fresh = a.wsx.clone();
        fresh.workspaces.clear();
        a.finish_jumps(fresh, |_| panic!("jumped to a workspace that is gone"));
        assert_eq!(
            a.status,
            "OS-DEV/shy-daffodil was archived just now — enter again resumes it in /home/u/OS-DEV"
        );
        // and enter again does what it said
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("did not resume");
        };
        assert_eq!(targets[0].cwd, "/home/u/OS-DEV");
    }

    #[test]
    fn a_wsx_that_does_not_answer_the_second_time_changes_nothing() {
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        a.finish_jumps(crate::wsx::State::default(), |_| {
            panic!("jumped without knowing it was still there")
        });
        assert!(a.status.contains("did not answer"), "{:?}", a.status);
        assert!(a.wsx.available, "a silence is not news that it is gone");
    }

    #[test]
    fn a_jump_wsx_refuses_says_why() {
        let mut a = app();
        on(&mut a, "gggggggg-7");
        a.do_action(Action::Resume);
        let fresh = a.wsx.clone();
        a.finish_jumps(fresh, |_| Err("no workspace named shy-daffodil".into()));
        assert!(
            a.status.contains("could not switch") && a.status.contains("no workspace named"),
            "{:?}",
            a.status
        );
    }

    #[test]
    fn an_archived_workspace_resumes_in_its_repos_checkout() {
        // Archiving deleted the worktree. Resuming used to start wherever
        // you were standing, when the repo it came from was right there.
        let mut a = app();
        on(&mut a, "hhhhhhhh-8");
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("did not resume");
        };
        assert_eq!(targets[0].cwd, "/home/u/OS-DEV");
        // the browser closes, so the shell is told on the way out
        assert_eq!(
            a.notes,
            vec!["OS-DEV/gdisk-app was archived — resuming in /home/u/OS-DEV"]
        );
    }

    #[test]
    fn every_way_of_opening_an_archived_workspace_lands_in_the_checkout() {
        // A window, and tmux behind one, skip a folder that is gone. The
        // shell does the cd for all of them, so the plan's folder is
        // what decides it.
        for action in [Action::NewWindow, Action::WindowTmux, Action::Tmux] {
            let mut a = app();
            on(&mut a, "hhhhhhhh-8");
            a.do_action(action);
            if a.input_mode == InputMode::TmuxName {
                a.commit_tmux_name();
            }
            let cwd = match (&a.outcome, a.to_open.first()) {
                (Some(Outcome::Resume { targets, .. }), _) => targets[0].cwd.clone(),
                (None, Some((_, targets))) => targets[0].cwd.clone(),
                _ => panic!("{action:?} opened nothing"),
            };
            assert_eq!(cwd, "/home/u/OS-DEV", "{action:?}");
        }
        // one that stays in the browser says so there
        let mut a = app();
        on(&mut a, "hhhhhhhh-8");
        a.do_action(Action::NewWindow);
        assert_eq!(
            a.status,
            "OS-DEV/gdisk-app was archived — resuming in /home/u/OS-DEV, in a new window"
        );
        assert!(
            a.notes.is_empty(),
            "nothing closed, so nothing to say after"
        );
    }

    #[test]
    fn an_archived_workspace_whose_repo_wsx_no_longer_knows_resumes_as_before() {
        let mut a = app();
        let mut forgot = a.wsx.clone();
        forgot.repos.clear();
        a.set_wsx(forgot);
        on(&mut a, "hhhhhhhh-8");
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("did not resume");
        };
        assert_eq!(
            targets[0].cwd,
            format!("{WSX_ROOT}/OS-DEV/gdisk-app"),
            "the shell says the folder is gone and resumes where you are, as it did"
        );
        assert!(a.notes.is_empty(), "{:?}", a.notes);
    }

    #[test]
    fn a_worktree_archived_and_kept_is_where_it_resumes() {
        // `wsx workspace archive --keep-worktree`: archived, and the folder
        // still there with the branch in it.
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().into_owned();
        let kept = format!("{root}/OS-DEV/kept");
        std::fs::create_dir_all(&kept).unwrap();
        let mut a = app();
        let mut st = a.wsx.clone();
        st.root = Some(root);
        a.set_wsx(st);
        add_session(&mut a, "iiiiiiii-9", &kept, 0);
        let s = a.all.iter().find(|s| s.id == "iiiiiiii-9").unwrap();
        assert!(matches!(
            s.wsx.as_ref().unwrap().status,
            crate::wsx::Status::Archived { .. }
        ));
        on(&mut a, "iiiiiiii-9");
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("did not resume");
        };
        assert_eq!(targets[0].cwd, kept);
        assert!(a.notes.is_empty());
    }

    #[test]
    fn another_machines_workspace_is_left_alone() {
        // Synced from elsewhere: this wsx never had it, so its absence from
        // the list says nothing, and there is no checkout to go to.
        let mut a = app();
        add_session(
            &mut a,
            "iiiiiiii-9",
            "/Users/u/.local/state/wsx/worktrees/OS-DEV/gdisk-app",
            0,
        );
        on(&mut a, "iiiiiiii-9");
        a.do_action(Action::Resume);
        let Some(Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("did not resume");
        };
        assert_eq!(
            targets[0].cwd,
            "/Users/u/.local/state/wsx/worktrees/OS-DEV/gdisk-app"
        );
    }

    #[test]
    fn an_archived_workspace_is_recorded_where_it_would_reopen() {
        // A reboot offer skips any folder that is gone, so a session
        // recorded under its deleted worktree would never be offered back.
        let mut a = app();
        let i = a.all.iter().position(|s| s.id == "hhhhhhhh-8").unwrap();
        a.all[i].live_pid = Some(1);
        let e = a
            .open_sessions()
            .into_iter()
            .find(|e| e.id == "hhhhhhhh-8")
            .unwrap();
        assert_eq!(e.cwd, "/home/u/OS-DEV");
    }

    #[test]
    fn nothing_built_for_a_test_asks_the_real_wsx() {
        // `wsx waybar jump` switches whatever wsx is running on this desktop,
        // or opens a terminal on it. A rescan asks wsx again, so the flag
        // that allows that has to be off in everything the suite builds.
        let mut a = app();
        assert!(!a.ask_wsx);
        let before = a.wsx.workspaces.clone();
        a.absorb_rescan(corpus());
        assert_eq!(
            a.wsx.workspaces, before,
            "a rescan replaced what was handed in"
        );
    }

    #[test]
    fn no_fixture_may_ever_write_to_the_real_home() {
        // This suite overwrote the real meta.json of whoever ran it for
        // several days: favourites, tags and notes replaced by the
        // fixture's, and every test still passed. Nothing here writes.
        assert!(!app().persist);
        assert!(!app_with(crate::meta::Meta::default(), true).persist);
    }

    #[test]
    fn r_does_nothing_at_all_when_there_is_no_offer() {
        // `r` is a letter people will hit by accident. With nothing on offer
        // it must not open anything.
        let mut a = app();
        assert!(a.reopen.is_empty());
        press(&mut a, 'r');
        assert!(
            a.outcome.is_none() && a.to_open.is_empty(),
            "a stray keystroke opened windows"
        );
        assert!(!a.quit);
    }

    #[test]
    fn waving_the_offer_away_takes_it_off_screen() {
        let mut a = app();
        offer(&mut a, &["aaaaaaaa-1"]);
        press(&mut a, 'x');
        assert!(a.reopen.is_empty(), "the offer stayed up");
        assert!(
            a.outcome.is_none() && a.to_open.is_empty(),
            "dismissing must not open anything"
        );
        assert!(
            a.status.contains("--reopen"),
            "dismissing should say how to change your mind: {}",
            a.status
        );
    }

    #[test]
    fn what_gets_recorded_is_exactly_what_the_list_marks_as_live() {
        let mut a = app();
        assert!(a.open_sessions().is_empty());

        a.all[0].live_pid = Some(1);
        a.all[1].has_tmux = true;
        // a subagent is not something you can resume on its own
        let sub = a.all.iter().position(|s| s.is_subagent).unwrap();
        a.all[sub].live_pid = Some(3);
        // and neither is a session with nowhere to open
        let nowhere = a.all.iter().position(|s| s.cwd.is_empty()).unwrap();
        a.all[nowhere].live_pid = Some(4);

        let ids: Vec<String> = a.open_sessions().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["aaaaaaaa-1", "bbbbbbbb-2"], "recorded: {ids:?}");
    }

    #[test]
    fn a_recorded_session_carries_what_reopening_it_needs() {
        let mut a = app();
        a.all[0].live_pid = Some(1);
        let e = a.open_sessions().remove(0);
        assert!(!e.cwd.is_empty() && !e.id.is_empty());
        assert_eq!(e.model, "claude-opus-5");
        assert_eq!(e.perms, "bypassPermissions");
        assert_eq!(e.title, "today's work");
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
    fn the_star_count_matches_the_starred_rows() {
        // meta.json outlives the transcripts it refers to, so counting its
        // entries put a number in the header that no row accounted for.
        let mut meta = crate::meta::Meta::default();
        meta.toggle_favorite("aaaaaaaa-1"); // a session that exists
        meta.toggle_favorite("long-gone-session"); // one that does not
        let a = app_with(meta, true);

        let starred = a
            .all
            .iter()
            .filter(|s| s.favorite && !s.is_subagent)
            .count();
        assert_eq!(starred, 1, "fixture should have exactly one real favourite");
        assert_eq!(
            a.favourites_shown(),
            starred,
            "the header promised a star the list does not have"
        );
    }

    #[test]
    fn the_live_count_matches_what_is_marked() {
        let mut a = app();
        assert_eq!(a.live_shown(), 0);
        a.all[0].live_pid = Some(1);
        a.all[1].live_pid = Some(2);
        assert_eq!(a.live_shown(), 2);
        let marked = a
            .all
            .iter()
            .filter(|s| s.is_live() && !s.is_subagent)
            .count();
        assert_eq!(
            a.live_shown(),
            marked,
            "the header must agree with the rows"
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

    fn subagent_rows(a: &App) -> usize {
        a.view.iter().filter(|r| matches!(r, Row::Sub(_))).count()
    }

    #[test]
    fn the_subagent_count_shows_them_even_after_a_hid_them() {
        // Open, then `a` to hide every subagent. The parent was still
        // marked open, so clicking its count closed it and showed nothing,
        // where → would have shown them.
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        laid_out(&mut a);
        let parent = |a: &App| {
            a.view
                .iter()
                .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "aaaaaaaa-1"))
                .unwrap()
        };
        a.cursor = parent(&a);
        a.on_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(subagent_rows(&a), 2);
        a.on_key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(subagent_rows(&a), 0);

        let row = parent(&a);
        a.on_mouse(click(37, 3 + row as u16));
        assert_eq!(subagent_rows(&a), 2, "{:?}", a.status);
    }

    #[test]
    fn closing_one_parent_does_not_show_every_other_one() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        a.cursor = a
            .view
            .iter()
            .position(|r| matches!(r, Row::Item(i) if a.all[*i].id == "aaaaaaaa-1"))
            .unwrap();
        a.on_key(KeyEvent::from(KeyCode::Right));
        a.on_key(KeyEvent::from(KeyCode::Char('a')));
        a.on_key(KeyEvent::from(KeyCode::Left));
        assert!(!a.show_subagents, "← turned subagents back on");
        assert_eq!(subagent_rows(&a), 0);
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

    #[test]
    fn a_note_goes_on_the_row_it_was_opened_for_whatever_the_mouse_does() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        laid_out(&mut a);
        let was = a.current().unwrap().id.clone();
        a.on_key(KeyEvent::from(KeyCode::Char('N')));
        for c in "mine".chars() {
            a.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        a.on_mouse(at(MouseEventKind::ScrollDown, 60, 6));
        a.on_mouse(click(60, row_y(&a, 4)));
        a.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(a.meta.get(&was).map(|e| e.note.as_str()), Some("mine"));
        assert_eq!(
            a.all.iter().filter(|s| !s.note.is_empty()).count(),
            1,
            "the note went somewhere else as well"
        );
    }

    #[test]
    fn a_tmux_name_prompt_resumes_the_row_it_was_opened_for() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        laid_out(&mut a);
        let was = a.current().unwrap().id.clone();
        a.on_key(KeyEvent::from(KeyCode::Char('W')));
        assert_eq!(a.input_mode, InputMode::TmuxName);
        a.on_mouse(at(MouseEventKind::ScrollDown, 60, 6));
        a.on_key(KeyEvent::from(KeyCode::Enter));
        let (_, targets) = a.to_open.first().expect("opened");
        assert_eq!(targets[0].id, was);
    }
}

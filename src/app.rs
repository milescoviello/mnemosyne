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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    Normal,
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
    Resume { targets: Vec<ResumeTarget>, target: Target },
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

    pub status: String,
    pub outcome: Option<Outcome>,
    pub quit: bool,
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
            status: String::new(),
            outcome: None,
            quit: false,
            restore_model,
            want_refresh: false,
            preview_cache: HashMap::new(),
            matcher: Matcher::new(Config::DEFAULT),
            deep_tx,
            deep_rx,
        };
        all.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        app.all = all;
        app.apply_overlay();
        app.rebuild();
        app
    }

    /// Fold favourites/tags/notes and live-process state onto the sessions.
    pub fn apply_overlay(&mut self) {
        let tmux = crate::live::tmux_sessions();
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
            if !hits.contains_key(&s.path.to_string_lossy().to_string()) {
                return false;
            }
        }
        if !self.fuzzy.trim().is_empty() {
            let hay = format!(
                "{} {} {} {} {}",
                s.title(),
                crate::model::short_cwd(&s.cwd),
                s.git_branch,
                s.tags.join(" "),
                s.last_prompt
            );
            let pat = Pattern::parse(self.fuzzy.trim(), CaseMatching::Ignore, Normalization::Smart);
            let mut cbuf = Vec::new();
            if pat.score(Utf32Str::new(&hay, &mut cbuf), &mut self.matcher).is_none() {
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
        let pat = Pattern::parse(self.fuzzy.trim(), CaseMatching::Ignore, Normalization::Smart);
        let mut cbuf = Vec::new();
        pat.score(Utf32Str::new(&hay, &mut cbuf), &mut self.matcher).unwrap_or(0)
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
                    Sort::Folder => x
                        .cwd
                        .cmp(&y.cwd)
                        .then_with(|| y.mtime.cmp(&x.mtime)),
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
                Row::Item(i) | Row::Sub(i) => {
                    self.all[*i].path.to_string_lossy() == p.as_str()
                }
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
            .filter(|&j| self.all[j].is_subagent && self.all[j].parent.as_deref() == Some(pid.as_str()))
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
        let Some(i) = self.current_idx() else { return Vec::new() };
        let key = self.all[i].path.to_string_lossy().to_string();
        if let Some(v) = self.preview_cache.get(&key) {
            return v.clone();
        }
        let turns = preview::tail_turns(&self.all[i], want);
        self.preview_cache.insert(key, turns.clone());
        turns
    }

    pub fn deep_snippet(&self) -> Option<&String> {
        let s = self.current()?;
        self.deep_hits
            .as_ref()?
            .get(&s.path.to_string_lossy().to_string())
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
        self.status = if now { "★ favourited".into() } else { "unfavourited".into() };
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
            model: if self.restore_model { s.model.clone() } else { String::new() },
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
            self.deep_busy = false;
            self.rebuild();
            return;
        }
        self.deep_generation += 1;
        let generation = self.deep_generation;
        let mode = self.deep_mode;
        let tx = self.deep_tx.clone();
        let sessions: Vec<Session> = self
            .all
            .iter()
            .filter(|s| self.show_subagents || !s.is_subagent)
            .cloned()
            .collect();
        self.deep_busy = true;
        std::thread::spawn(move || {
            let hits = search::run(&sessions, &q, mode);
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
            self.deep_hits = Some(r.hits);
            self.deep_busy = false;
            self.status = format!("{n} session(s) match “{}” in {}", self.deep, self.deep_mode.label());
            self.rebuild();
        }
    }

    fn commit_tag_add(&mut self) {
        let raw = self.input.trim().to_string();
        let Some(i) = self.current_idx() else { return };
        let id = self.all[i].id.clone();
        if let Some(t) = raw.strip_prefix('-') {
            self.meta.remove_tag(&id, t);
            self.status = format!("removed tag {t}");
        } else if !raw.is_empty() {
            for t in raw.split(|c: char| c == ',' || c.is_whitespace()) {
                if !t.is_empty() {
                    self.meta.add_tag(&id, t);
                }
            }
            self.status = format!("tagged {raw}");
        }
        let _ = self.meta.save();
        let e = self.meta.get(&id).cloned().unwrap_or_default();
        self.all[i].tags = e.tags;
        self.all[i].favorite = e.favorite;
        self.input.clear();
        self.input_mode = InputMode::Normal;
        self.rebuild();
    }

    fn commit_note(&mut self) {
        let Some(i) = self.current_idx() else { return };
        let id = self.all[i].id.clone();
        self.meta.set_note(&id, &self.input);
        let _ = self.meta.save();
        self.all[i].note = self.meta.get(&id).map(|e| e.note.clone()).unwrap_or_default();
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
                self.input_mode = InputMode::Normal;
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
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,

            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Char('d') if ctrl => self.move_by(10),
            KeyCode::Char('u') if ctrl => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.goto_top(),
            KeyCode::End | KeyCode::Char('G') => self.goto_bottom(),

            KeyCode::Enter => self.resume(if alt { Target::Window } else { Target::Here }),
            KeyCode::Char('n') if ctrl => self.resume(Target::Window),
            KeyCode::Char('t') if ctrl => self.resume(Target::Tmux),

            KeyCode::Char(' ') => self.toggle_select(),
            KeyCode::Char('f') if ctrl => self.input_mode = InputMode::Deep,
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('t') => {
                self.input.clear();
                self.input_mode = InputMode::TagAdd;
            }
            KeyCode::Char('T') => {
                self.input = self.tag_filter.clone().unwrap_or_default();
                self.input_mode = InputMode::TagFilter;
            }
            KeyCode::Char('N') => {
                self.input = self.current().map(|s| s.note.clone()).unwrap_or_default();
                self.input_mode = InputMode::Note;
            }
            KeyCode::Char('/') => self.input_mode = InputMode::Fuzzy,
            KeyCode::Char('F') => self.input_mode = InputMode::Deep,
            KeyCode::Char('m') => {
                self.deep_mode = self.deep_mode.next();
                if !self.deep.trim().is_empty() {
                    self.start_deep();
                }
                self.status = format!("search mode: {}", self.deep_mode.label());
            }

            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.status = format!("sort: {}", self.sort.label());
                self.rebuild();
                // Keeping the cursor on the same session across a re-sort
                // scrolls you into the middle of the new order, which reads
                // like the sort did not work. Show the top instead.
                self.goto_top();
            }
            KeyCode::Char('o') => {
                self.group_by_dir = !self.group_by_dir;
                self.status = if self.group_by_dir {
                    "grouped by directory".into()
                } else {
                    "flat list".into()
                };
                self.rebuild();
            }
            KeyCode::Char('D') => {
                self.date = self.date.next();
                self.status = format!("dates: {}", self.date.label());
                self.rebuild();
            }
            KeyCode::Char('*') => {
                self.fav_only = !self.fav_only;
                self.status = if self.fav_only { "favourites only".into() } else { "all sessions".into() };
                self.rebuild();
            }
            KeyCode::Char('L') => {
                self.live_only = !self.live_only;
                self.status = if self.live_only { "running sessions only".into() } else { "all sessions".into() };
                self.rebuild();
            }
            KeyCode::Char('a') => {
                self.show_subagents = !self.show_subagents;
                self.status = if self.show_subagents {
                    "subagents available — use → to expand".into()
                } else {
                    "subagents hidden".into()
                };
                self.rebuild();
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.toggle_expand(true),
            KeyCode::Left | KeyCode::Char('h') => self.toggle_expand(false),

            KeyCode::Char('c') => {
                self.selected.clear();
                self.fuzzy.clear();
                self.deep.clear();
                self.deep_hits = None;
                self.tag_filter = None;
                self.fav_only = false;
                self.live_only = false;
                self.date = DateRange::All;
                self.status = "filters cleared".into();
                self.rebuild();
            }
            KeyCode::Char('p') => {
                self.show_preview = !self.show_preview;
                self.status = if self.show_preview {
                    "preview on".into()
                } else {
                    "preview off".into()
                };
            }
            KeyCode::Char('R') | KeyCode::F(5) => {
                self.want_refresh = true;
                self.status = "reindexing…".into();
            }
            KeyCode::Char('?') => self.input_mode = InputMode::Help,
            _ => {}
        }
    }
}

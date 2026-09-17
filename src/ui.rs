//! Rendering.
//!
//! Visual rules, in order of importance:
//!
//! 1. No borders. Structure comes from alignment and whitespace, not boxes.
//! 2. One accent colour. Everything structural is dim grey; colour means
//!    something (favourite, running, tag) rather than decorating.
//! 3. One footer line. The full key list lives behind `?` instead of being
//!    permanently on screen.
//! 4. Columns line up, and the header row is computed from the same widths as
//!    the rows so they can never drift apart.

use crate::app::{App, InputMode, Row};
use crate::model::{compact_count, fit, human_dur, human_size, reltime, short_cwd};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Cyan;
const CHROME: Color = Color::DarkGray;
const FAV: Color = Color::Yellow;
const LIVE: Color = Color::Green;
const TAG: Color = Color::Magenta;
const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;

const PAD: &str = "  ";
const CURSOR: &str = "  \u{2192} ";
const NOCURSOR: &str = "    ";

/// Column widths, derived once per frame so the header and the rows agree.
struct Cols {
    folder: usize,
    sub: usize,
    title: usize,
    model: usize,
    msgs: usize,
    tags: usize,
}

impl Cols {
    fn new(width: usize) -> Cols {
        let folder = if width >= 150 {
            20
        } else if width >= 120 {
            16
        } else {
            12
        };
        let sub = 4;
        let model = if width >= 150 { 12 } else if width >= 110 { 10 } else { 0 };
        let msgs = 6;
        // tags trail the right edge, so they need reserved room or they fall
        // off the screen along with their header
        let tags = if width >= 140 { 16 } else { 0 };
        // 4 cursor + 2 marker + 4 age + 2 gap
        let fixed = 4 + 2 + 4 + 2 + folder + 1 + sub + model + msgs + tags;
        let title = width.saturating_sub(fixed).max(16);
        Cols { folder, sub, title, model, msgs, tags }
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_input = app.input_mode != InputMode::Normal && app.input_mode != InputMode::Help;
    let rail = if app.show_preview && area.height >= 18 { 6 } else { 0 };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                             // title bar
            Constraint::Length(1),                             // breathing room
            Constraint::Length(1),                             // column header
            Constraint::Min(3),                                // list
            Constraint::Length(rail),                          // preview rail
            Constraint::Length(if show_input { 2 } else { 1 }), // input / spacer
            Constraint::Length(1),                             // footer
        ])
        .split(area);

    let cols = Cols::new(area.width as usize);

    draw_titlebar(f, app, rows[0]);
    draw_colheader(f, &cols, rows[2]);
    draw_list(f, app, &cols, rows[3]);
    if rail > 0 {
        draw_rail(f, app, rows[4]);
    }
    if show_input {
        draw_input(f, app, rows[5]);
    }
    draw_footer(f, app, rows[6]);

    if app.input_mode == InputMode::Help {
        draw_help(f, area);
    }
}

/// ` mnemosyne                    328 sessions · ★12 · 6 live · recency`
fn draw_titlebar(f: &mut Frame, app: &App, area: Rect) {
    let left = vec![
        Span::raw(PAD),
        Span::styled(
            "mnemosyne",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ];

    let mut bits: Vec<Span> = Vec::new();
    let dot = || Span::styled(" · ", Style::default().fg(CHROME));

    bits.push(Span::styled(
        format!("{} sessions", app.item_count()),
        Style::default().fg(TEXT),
    ));
    let favs = app.meta.favorite_count();
    if favs > 0 {
        bits.push(dot());
        bits.push(Span::styled(format!("★{favs}"), Style::default().fg(FAV)));
    }
    if app.live.count > 0 {
        bits.push(dot());
        bits.push(Span::styled(
            format!("●{} live", app.live.count),
            Style::default().fg(LIVE),
        ));
    }
    bits.push(dot());
    bits.push(Span::styled(app.sort.label(), Style::default().fg(CHROME)));

    // only states that are actually on get named, so the bar stays quiet
    if app.group_by_dir {
        bits.push(dot());
        bits.push(Span::styled("grouped", Style::default().fg(ACCENT)));
    }
    if app.date != crate::model::DateRange::All {
        bits.push(dot());
        bits.push(Span::styled(app.date.label(), Style::default().fg(ACCENT)));
    }
    if app.fav_only {
        bits.push(dot());
        bits.push(Span::styled("★ only", Style::default().fg(FAV)));
    }
    if app.live_only {
        bits.push(dot());
        bits.push(Span::styled("live only", Style::default().fg(LIVE)));
    }
    if let Some(t) = &app.tag_filter {
        bits.push(dot());
        bits.push(Span::styled(format!("#{t}"), Style::default().fg(TAG)));
    }
    if !app.fuzzy.trim().is_empty() {
        bits.push(dot());
        bits.push(Span::styled(format!("/{}", app.fuzzy), Style::default().fg(ACCENT)));
    }
    if app.deep_busy {
        bits.push(dot());
        bits.push(Span::styled("searching…", Style::default().fg(ACCENT)));
    } else if app.deep_hits.is_some() {
        bits.push(dot());
        bits.push(Span::styled(
            format!("{} “{}”", app.deep_mode.label(), app.deep),
            Style::default().fg(ACCENT),
        ));
    }
    if !app.selected.is_empty() {
        bits.push(dot());
        bits.push(Span::styled(
            format!("{} selected", app.selected.len()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }

    let lw: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let rw: usize = bits.iter().map(|s| s.content.chars().count()).sum();
    let gap = (area.width as usize).saturating_sub(lw + rw + 2);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(bits);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_colheader(f: &mut Frame, c: &Cols, area: Rect) {
    let st = Style::default().fg(CHROME);
    let mut s = String::new();
    s.push_str(NOCURSOR);
    s.push_str("  ");
    s.push_str(&format!("{:>4}", "AGE"));
    s.push_str("  ");
    s.push_str(&format!("{:<w$}", "FOLDER", w = c.folder));
    s.push(' ');
    s.push_str(&" ".repeat(c.sub));
    s.push_str(&format!("{:<w$}", "TITLE", w = c.title));
    if c.model > 0 {
        s.push_str(&format!("{:<w$}", "MODEL", w = c.model));
    }
    s.push_str(&format!("{:>w$}", "MSGS", w = c.msgs));
    if c.tags > 0 {
        s.push_str("  TAGS");
    }
    f.render_widget(Paragraph::new(Span::styled(s, st)), area);
}

fn draw_list(f: &mut Frame, app: &mut App, c: &Cols, area: Rect) {
    let items: Vec<ListItem> = app
        .view
        .iter()
        .map(|r| match r {
            Row::Header(dir, n) => ListItem::new(Line::from(vec![
                Span::raw(NOCURSOR),
                Span::styled(
                    format!("{dir}  "),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{n}"), Style::default().fg(CHROME)),
            ])),
            Row::Item(i) | Row::Sub(i) => {
                let s = &app.all[*i];
                let is_sub = matches!(r, Row::Sub(_));
                let key = s.path.to_string_lossy().to_string();
                let mut sp: Vec<Span> = Vec::new();

                // one marker column: selection wins, then favourite, then live
                let (glyph, gstyle) = if app.selected.contains(&key) {
                    ("✓", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                } else if s.favorite {
                    ("★", Style::default().fg(FAV))
                } else if s.is_live() {
                    (
                        if s.live_exact { "●" } else { "◌" },
                        Style::default().fg(LIVE),
                    )
                } else {
                    (" ", Style::default())
                };
                sp.push(Span::styled(glyph, gstyle));
                sp.push(Span::raw(" "));

                sp.push(Span::styled(
                    format!("{:>4}", reltime(s.mtime)),
                    Style::default().fg(CHROME),
                ));
                sp.push(Span::raw("  "));

                if is_sub {
                    sp.push(Span::styled(
                        format!("{:<w$}", "└ subagent", w = c.folder),
                        Style::default().fg(CHROME),
                    ));
                } else {
                    let folder = if s.cwd.is_empty() {
                        "—".to_string()
                    } else {
                        short_cwd(&s.cwd)
                    };
                    sp.push(Span::styled(
                        format!("{:<w$}", fit(&folder, c.folder), w = c.folder),
                        Style::default().fg(Color::Blue),
                    ));
                }
                sp.push(Span::raw(" "));

                if s.subagent_count > 0 && !is_sub {
                    sp.push(Span::styled(
                        format!("{:<w$}", format!("⌁{}", s.subagent_count), w = c.sub),
                        Style::default().fg(CHROME),
                    ));
                } else {
                    sp.push(Span::raw(" ".repeat(c.sub)));
                }

                let tstyle = if s.is_live() {
                    Style::default().fg(BRIGHT)
                } else {
                    Style::default().fg(TEXT)
                };
                sp.push(Span::styled(
                    format!("{:<w$}", fit(s.title(), c.title), w = c.title),
                    tstyle,
                ));

                if c.model > 0 {
                    sp.push(Span::styled(
                        format!("{:<w$}", fit(s.model_short(), c.model - 1), w = c.model),
                        Style::default().fg(CHROME),
                    ));
                }
                sp.push(Span::styled(
                    format!("{:>w$}", compact_count(s.entries), w = c.msgs),
                    Style::default().fg(CHROME),
                ));

                for t in &s.tags {
                    sp.push(Span::styled(format!("  #{t}"), Style::default().fg(TAG)));
                }
                ListItem::new(Line::from(sp))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default())
        .highlight_symbol(CURSOR)
        .highlight_style(Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD));

    let mut st = ListState::default();
    st.select(Some(app.cursor));
    f.render_stateful_widget(list, area, &mut st);
}

/// The bottom rail: what this session was, and where you left off.
fn draw_rail(f: &mut Frame, app: &mut App, area: Rect) {
    let turns = app.preview(8);
    let snippet = app.deep_snippet().cloned();
    let width = area.width as usize;

    let Some(s) = app.current().cloned() else {
        let l = Line::from(vec![
            Span::raw(PAD),
            Span::styled(
                "nothing matches these filters — press c to clear them",
                Style::default().fg(CHROME),
            ),
        ]);
        f.render_widget(Paragraph::new(Text::from(vec![Line::raw(""), l])), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{}{}", PAD, "─".repeat(width.saturating_sub(4))),
        Style::default().fg(CHROME),
    )));

    // headline: title on the left, hard facts on the right
    let mut facts: Vec<String> = vec![short_cwd(&s.cwd)];
    if !s.git_branch.is_empty() {
        facts.push(s.git_branch.clone());
    }
    facts.push(human_size(s.size));
    facts.push(format!("{} entries", s.entries));
    if s.duration_secs() > 0 {
        facts.push(human_dur(s.duration_secs()));
    }
    if let Some(pid) = s.live_pid {
        facts.push(format!(
            "{} {pid}",
            if s.live_exact { "running" } else { "likely running" }
        ));
    }
    let fact_str = facts.join(" · ");
    let title = fit(s.title(), width.saturating_sub(fact_str.chars().count() + 8));
    let gap = width
        .saturating_sub(title.chars().count() + fact_str.chars().count() + 4)
        .max(2);
    lines.push(Line::from(vec![
        Span::raw(PAD),
        Span::styled(title, Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled(fact_str, Style::default().fg(CHROME)),
    ]));

    if !s.tags.is_empty() || !s.note.is_empty() {
        let mut sp = vec![Span::raw(PAD)];
        for t in &s.tags {
            sp.push(Span::styled(format!("#{t} "), Style::default().fg(TAG)));
        }
        if !s.note.is_empty() {
            sp.push(Span::styled(
                fit(&s.note, width.saturating_sub(30)),
                Style::default().fg(FAV).add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(sp));
    } else {
        lines.push(Line::raw(""));
    }

    let label = |t: &str| {
        Span::styled(
            format!("{:<10}", t),
            Style::default().fg(ACCENT),
        )
    };
    let body = width.saturating_sub(14);

    // Three body lines, and no line repeating another. `last_prompt` is
    // usually literally the final user turn, so showing both wastes a line
    // and reads as clutter.
    const BODY: usize = 3;
    let mut body_lines: Vec<Line> = Vec::new();
    // Compare by characters, not bytes: slicing a &str at an arbitrary byte
    // offset panics when it lands inside a multi-byte character, and prompts
    // contain plenty of non-ASCII.
    let same = |a: &str, b: &str| {
        let mut ai = a.chars().flat_map(|c| c.to_lowercase());
        let mut bi = b.chars().flat_map(|c| c.to_lowercase());
        let mut seen = 0;
        loop {
            match (ai.next(), bi.next()) {
                (Some(x), Some(y)) if x == y => seen += 1,
                (None, None) => return seen > 0,
                _ => return seen >= 48,
            }
            if seen >= 48 {
                return true;
            }
        }
    };

    if let Some(sn) = snippet {
        body_lines.push(Line::from(vec![
            Span::raw(PAD),
            label("match"),
            Span::styled(fit(&sn, body), Style::default().fg(BRIGHT)),
        ]));
    }
    if !s.last_prompt.is_empty() {
        body_lines.push(Line::from(vec![
            Span::raw(PAD),
            label("left off"),
            Span::styled(fit(&s.last_prompt, body), Style::default().fg(BRIGHT)),
        ]));
    }
    // A turn that is nothing but tool invocations ("[Bash] [Read]") tells you
    // nothing about the conversation, so it does not earn a rail line.
    let only_tools = |t: &str| {
        let trimmed = t.trim();
        !trimmed.is_empty()
            && trimmed
                .split_whitespace()
                .all(|w| w.starts_with('[') && w.ends_with(']'))
    };
    for t in turns.iter().rev() {
        if body_lines.len() >= BODY {
            break;
        }
        if t.role == "you" && same(&t.text, &s.last_prompt) {
            continue;
        }
        if only_tools(&t.text) {
            continue;
        }
        body_lines.insert(
            if body_lines.is_empty() { 0 } else { body_lines.len() },
            Line::from(vec![
                Span::raw(PAD),
                Span::styled(
                    format!("{:<10}", t.role),
                    Style::default().fg(if t.role == "you" { ACCENT } else { FAV }),
                ),
                Span::styled(fit(&t.text, body), Style::default().fg(TEXT)),
            ]),
        );
    }
    body_lines.truncate(BODY);
    lines.extend(body_lines);

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let (label, value, hint) = match app.input_mode {
        InputMode::Fuzzy => ("filter", app.fuzzy.clone(), "enter keeps it · esc clears".into()),
        InputMode::Deep => (
            "search in conversations",
            app.deep.clone(),
            format!("enter runs it · tab switches mode (now {})", app.deep_mode.label()),
        ),
        InputMode::TagAdd => (
            "tag",
            app.input.clone(),
            format!("enter adds · -name removes · tab completes    {}", app.tag_completions().join("   ")),
        ),
        InputMode::TagFilter => (
            "show only tag",
            app.input.clone(),
            format!("enter applies · empty clears    {}", app.tag_completions().join("   ")),
        ),
        InputMode::Note => ("note", app.input.clone(), "enter saves".into()),
        _ => ("", String::new(), String::new()),
    };
    let line = Line::from(vec![
        Span::raw(PAD),
        Span::styled(format!("{label} "), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(value, Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled("▏", Style::default().fg(ACCENT)),
        Span::styled(format!("   {hint}"), Style::default().fg(CHROME)),
    ]);
    f.render_widget(Paragraph::new(Text::from(vec![Line::raw(""), line])), area);
}

/// One line. Transient messages take it over; otherwise it shows the keys.
fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let pos = {
        let n = app.item_count();
        let at = app
            .view
            .iter()
            .take(app.cursor + 1)
            .filter(|r| matches!(r, Row::Item(_) | Row::Sub(_)))
            .count();
        format!("{at}/{n}")
    };

    let mut spans: Vec<Span> = vec![Span::raw(PAD)];
    if app.status.is_empty() {
        let k = |s: &'static str| Span::styled(s, Style::default().fg(ACCENT));
        let d = |s: &'static str| Span::styled(s, Style::default().fg(CHROME));
        // Drop hints rather than let them collide with the position counter.
        let budget = area.width as usize;
        let mut hints: Vec<(&'static str, &'static str)> = vec![
            ("↑↓", " move   "),
            ("enter", " resume   "),
            ("/", " filter   "),
            ("F", " search   "),
            ("f", " ★   "),
            ("t", " tag   "),
            ("s", " sort   "),
            ("?", " keys"),
        ];
        loop {
            let w: usize = hints
                .iter()
                .map(|(a, b)| a.chars().count() + b.chars().count())
                .sum();
            if w + pos.chars().count() + 6 <= budget || hints.len() <= 2 {
                break;
            }
            // remove the second-to-last hint, always keeping "? keys"
            hints.remove(hints.len() - 2);
        }
        for (a, b) in hints {
            spans.push(k(a));
            spans.push(d(b));
        }
    } else {
        spans.push(Span::styled(
            fit(&app.status, (area.width as usize).saturating_sub(pos.chars().count() + 6)),
            Style::default().fg(FAV),
        ));
    }

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let gap = (area.width as usize).saturating_sub(used + pos.chars().count() + 2);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(pos, Style::default().fg(CHROME)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str, &str)] = &[
        ("↑ ↓", "k j", "move the cursor"),
        ("pgup pgdn", "ctrl+u ctrl+d", "jump ten rows"),
        ("home end", "g G", "first / last session"),
        ("", "", ""),
        ("enter", "", "resume here: cd to its folder and reattach"),
        ("ctrl+n", "alt+enter", "resume in a new terminal window"),
        ("space", "", "select several, then enter reopens them all"),
        ("", "", ""),
        ("/", "", "filter by title, folder, branch or tag"),
        ("F", "ctrl+f", "search inside the conversations themselves"),
        ("m", "", "switch search: content / file touched / tool used"),
        ("", "", ""),
        ("f", "", "favourite — favourites always sort to the top"),
        ("t", "", "add a tag (type -name to remove one)"),
        ("T", "", "show only one tag"),
        ("N", "", "attach a private note"),
        ("", "", ""),
        ("s", "", "cycle sort: recency, size, entries, duration, title, folder"),
        ("o", "", "group the list by directory"),
        ("D", "", "cycle date range: today, 7d, 30d, 90d, any"),
        ("*", "", "favourites only"),
        ("L", "", "only sessions running right now"),
        ("", "", ""),
        ("a", "", "reveal subagent transcripts"),
        ("→ ←", "l h  tab", "expand / collapse a session's subagents"),
        ("p", "", "show or hide the preview rail"),
        ("R", "f5", "reindex"),
        ("c", "", "clear every filter and selection"),
        ("q esc", "ctrl+c", "quit without resuming"),
    ];
    let w = 76u16.min(area.width);
    let h = (rows.len() as u16 + 4).min(area.height);
    let r = centered(area, w, h);
    f.render_widget(Clear, r);

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (plain, vim, desc) in rows {
        if plain.is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{plain:<11}"), Style::default().fg(ACCENT)),
            Span::styled(format!("{vim:<14}"), Style::default().fg(CHROME)),
            Span::styled(*desc, Style::default().fg(TEXT)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "the middle column is a vim-style alias — you never need it",
            Style::default().fg(CHROME).add_modifier(Modifier::ITALIC),
        ),
    ]));

    let p = Paragraph::new(Text::from(lines))
        .block(Block::default())
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });
    f.render_widget(p, r);
}

//! Rendering. Two panes plus a persistent footer that always spells out the
//! plain keys, so the interface is discoverable without reading a manual.

use crate::app::{App, InputMode, Row};
use crate::model::{human_dur, human_size, reltime, short_cwd};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const FAV: Color = Color::Yellow;
const LIVE: Color = Color::Green;
const TAG: Color = Color::Magenta;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_input = app.input_mode != InputMode::Normal && app.input_mode != InputMode::Help;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                                  // header
            Constraint::Min(5),                                     // body
            Constraint::Length(if show_input { 1 } else { 0 }),      // input
            Constraint::Length(1),                                  // status
            Constraint::Length(2),                                  // footer
        ])
        .split(area);

    draw_header(f, app, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[1]);

    draw_list(f, app, body[0]);
    draw_preview(f, app, body[1]);

    if show_input {
        draw_input(f, app, chunks[2]);
    }
    draw_status(f, app, chunks[3]);
    draw_footer(f, app, chunks[4]);

    if app.input_mode == InputMode::Help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(" mnemosyne ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(format!("{}", app.item_count()), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("/{} sessions", app.all.iter().filter(|s| !s.is_subagent).count()), Style::default().fg(DIM)),
    ];
    let favs = app.meta.favorite_count();
    if favs > 0 {
        spans.push(Span::styled(format!("  ★{favs}"), Style::default().fg(FAV)));
    }
    if app.live.count > 0 {
        spans.push(Span::styled(format!("  ●{} live", app.live.count), Style::default().fg(LIVE)));
    }

    // active filters as chips, so state is never invisible
    let mut chips: Vec<String> = Vec::new();
    chips.push(format!("sort:{}", app.sort.label()));
    if app.group_by_dir {
        chips.push("grouped".into());
    }
    if app.date != crate::model::DateRange::All {
        chips.push(app.date.label());
    }
    if app.fav_only {
        chips.push("★only".into());
    }
    if app.live_only {
        chips.push("live only".into());
    }
    if let Some(t) = &app.tag_filter {
        chips.push(format!("#{t}"));
    }
    if !app.fuzzy.trim().is_empty() {
        chips.push(format!("/{}", app.fuzzy));
    }
    if app.deep_hits.is_some() || app.deep_busy {
        chips.push(format!("{}:{}", app.deep_mode.label(), app.deep));
    }
    if !app.selected.is_empty() {
        chips.push(format!("{} selected", app.selected.len()));
    }
    let chip_text = format!("[{}] ", chips.join("] ["));
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(used + chip_text.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(chip_text, Style::default().fg(ACCENT)));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let folder_w = if inner_w > 90 { 24 } else { 16 };

    let rows: Vec<ListItem> = app
        .view
        .iter()
        .map(|r| match r {
            Row::Header(dir, n) => ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{dir} "),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("({n})"), Style::default().fg(DIM)),
            ])),
            Row::Item(i) | Row::Sub(i) => {
                let s = &app.all[*i];
                let sub = matches!(r, Row::Sub(_));
                let key = s.path.to_string_lossy().to_string();
                let mut sp: Vec<Span> = Vec::new();

                sp.push(Span::styled(
                    if app.selected.contains(&key) { "✓" } else { " " },
                    Style::default().fg(ACCENT),
                ));
                sp.push(Span::styled(
                    if s.favorite { "★" } else { " " },
                    Style::default().fg(FAV),
                ));
                if s.is_live() {
                    sp.push(Span::styled(
                        if s.live_exact { "●" } else { "◌" },
                        Style::default().fg(LIVE),
                    ));
                } else {
                    sp.push(Span::raw(" "));
                }

                if sub {
                    sp.push(Span::styled("  └ ", Style::default().fg(DIM)));
                } else if s.subagent_count > 0 && app.show_subagents {
                    let open = app.expanded.contains(&s.id);
                    sp.push(Span::styled(
                        if open { " ▾ " } else { " ▸ " },
                        Style::default().fg(DIM),
                    ));
                } else {
                    sp.push(Span::raw(" "));
                }

                sp.push(Span::styled(
                    format!("{:>4} ", reltime(s.mtime)),
                    Style::default().fg(DIM),
                ));
                if !sub {
                    sp.push(Span::styled(
                        format!("{:<w$.w$} ", short_cwd(&s.cwd), w = folder_w),
                        Style::default().fg(Color::Blue),
                    ));
                }
                sp.push(Span::raw(crate::scan::squash(s.title(), 200)));
                if s.subagent_count > 0 && !sub {
                    sp.push(Span::styled(
                        format!("  ⌁{}", s.subagent_count),
                        Style::default().fg(DIM),
                    ));
                }
                for t in &s.tags {
                    sp.push(Span::styled(format!(" #{t}"), Style::default().fg(TAG)));
                }
                ListItem::new(Line::from(sp))
            }
        })
        .collect();

    let title = if app.deep_busy {
        " sessions — searching… ".to_string()
    } else {
        " sessions ".to_string()
    };
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Span::styled(title, Style::default().fg(ACCENT))),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(30, 50, 70))
                .add_modifier(Modifier::BOLD),
        );

    let mut st = ListState::default();
    st.select(Some(app.cursor));
    f.render_stateful_widget(list, area, &mut st);
}

fn kv<'a>(k: &'a str, v: String, c: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{k:<9}"), Style::default().fg(DIM)),
        Span::styled(v, Style::default().fg(c)),
    ])
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let turns = app.preview(8);
    let snippet = app.deep_snippet().cloned();
    let Some(s) = app.current().cloned() else {
        let p = Paragraph::new("no sessions match the current filters\n\npress c to clear filters")
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(DIM)))
            .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        crate::scan::squash(s.title(), 300),
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));
    lines.push(kv("folder", short_cwd(&s.cwd), Color::Blue));
    if !s.git_branch.is_empty() {
        lines.push(kv("branch", s.git_branch.clone(), Color::Green));
    }
    if !s.model.is_empty() {
        lines.push(kv("model", s.model_short().to_string(), Color::Magenta));
    }
    lines.push(kv(
        "size",
        format!(
            "{}  ·  {} entries  ·  {}↑ {}↓",
            human_size(s.size), s.entries, s.user_msgs, s.assistant_msgs
        ),
        Color::Gray,
    ));
    lines.push(kv(
        "when",
        format!("{} ago  ·  lasted {}", reltime(s.mtime), human_dur(s.duration_secs())),
        Color::Gray,
    ));
    if !s.permission_mode.is_empty() {
        lines.push(kv("perms", s.permission_mode.clone(), Color::Gray));
    }
    if s.subagent_count > 0 {
        lines.push(kv("subagents", format!("{} (→ to expand)", s.subagent_count), Color::Gray));
    }
    if let Some(pid) = s.live_pid {
        lines.push(kv(
            "running",
            if s.live_exact {
                format!("yes — pid {pid}")
            } else {
                format!("probably — pid {pid} in this folder")
            },
            LIVE,
        ));
    }
    if !s.tags.is_empty() {
        lines.push(Line::from(
            s.tags
                .iter()
                .map(|t| Span::styled(format!("#{t} "), Style::default().fg(TAG)))
                .collect::<Vec<_>>(),
        ));
    }
    if !s.note.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("note: {}", s.note),
            Style::default().fg(FAV).add_modifier(Modifier::ITALIC),
        )));
    }
    if let Some(snip) = snippet {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "match",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(snip, Style::default().fg(Color::White))));
    }
    if !s.last_prompt.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("left off ", Style::default().fg(DIM)),
            Span::styled(
                crate::scan::squash(&s.last_prompt, 300),
                Style::default().fg(Color::White),
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "─ conversation tail ─",
        Style::default().fg(DIM),
    )));
    if turns.is_empty() {
        lines.push(Line::from(Span::styled("(no readable messages)", Style::default().fg(DIM))));
    }
    for t in turns {
        let (tag, col) = if t.role == "you" {
            ("you", ACCENT)
        } else {
            ("claude", FAV)
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{tag}: "), Style::default().fg(col).add_modifier(Modifier::BOLD)),
            Span::raw(t.text),
        ]));
    }

    let p = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Span::styled(" preview ", Style::default().fg(ACCENT))),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let (label, value, hint) = match app.input_mode {
        InputMode::Fuzzy => ("filter", app.fuzzy.clone(), "type to narrow · enter keeps it · esc clears".to_string()),
        InputMode::Deep => (
            "search in conversations",
            app.deep.clone(),
            format!("enter runs it · tab switches mode (now: {})", app.deep_mode.label()),
        ),
        InputMode::TagAdd => (
            "tag",
            app.input.clone(),
            format!("enter adds · -name removes · tab completes   {}", app.tag_completions().join("  ")),
        ),
        InputMode::TagFilter => (
            "show only tag",
            app.input.clone(),
            format!("enter applies · empty clears   {}", app.tag_completions().join("  ")),
        ),
        InputMode::Note => ("note", app.input.clone(), "enter saves".to_string()),
        _ => ("", String::new(), String::new()),
    };
    let line = Line::from(vec![
        Span::styled(format!(" {label}: "), Style::default().fg(Color::Black).bg(ACCENT)),
        Span::styled(format!(" {value}"), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("▏", Style::default().fg(ACCENT)),
        Span::styled(format!("   {hint}"), Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let txt = if app.status.is_empty() {
        match app.current() {
            Some(s) => format!(" {}", s.path.to_string_lossy()),
            None => String::new(),
        }
    } else {
        format!(" {}", app.status)
    };
    let style = if app.status.is_empty() {
        Style::default().fg(DIM)
    } else {
        Style::default().fg(FAV)
    };
    f.render_widget(Paragraph::new(Span::styled(txt, style)), area);
}

fn draw_footer(f: &mut Frame, _app: &App, area: Rect) {
    let k = |s: &'static str| Span::styled(s, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let d = |s: &'static str| Span::styled(s, Style::default().fg(DIM));
    let l1 = Line::from(vec![
        d(" "), k("↑↓"), d(" move  "),
        k("enter"), d(" resume  "),
        k("ctrl+n"), d(" new window  "),
        k("space"), d(" select  "),
        k("/"), d(" filter  "),
        k("F"), d(" search inside  "),
        k("f"), d(" ★  "),
        k("t"), d(" tag"),
    ]);
    let l2 = Line::from(vec![
        d(" "), k("s"), d(" sort  "),
        k("o"), d(" group by folder  "),
        k("D"), d(" dates  "),
        k("T"), d(" by tag  "),
        k("*"), d(" ★ only  "),
        k("L"), d(" live only  "),
        k("→"), d(" subagents  "),
        k("c"), d(" clear  "),
        k("?"), d(" help  "),
        k("q"), d(" quit"),
    ]);
    f.render_widget(Paragraph::new(Text::from(vec![l1, l2])), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: &[(&str, &str, &str)] = &[
        ("↑ ↓", "k j", "move the cursor"),
        ("pgup pgdn", "ctrl+u ctrl+d", "jump ten rows"),
        ("home end", "g G", "first / last session"),
        ("enter", "", "resume here: cd to its folder and reattach"),
        ("ctrl+n", "alt+enter", "resume in a new terminal window"),
        ("space", "", "select several, then enter reopens them all"),
        ("/", "", "filter by title, folder, branch or tag"),
        ("F", "ctrl+f", "search inside the conversations themselves"),
        ("m", "", "switch search: content / file touched / tool used"),
        ("f", "", "favourite — favourites always sort to the top"),
        ("t", "", "add a tag (type -name to remove one)"),
        ("T", "", "show only one tag"),
        ("N", "", "attach a private note to a session"),
        ("s", "", "cycle sort: recency, size, entries, duration, title, folder"),
        ("o", "", "group the list by directory"),
        ("D", "", "cycle date range: today, 7d, 30d, 90d, any"),
        ("*", "", "favourites only"),
        ("L", "", "only sessions running right now"),
        ("a", "", "reveal subagent transcripts"),
        ("→ ←", "l h  tab", "expand / collapse a session's subagents"),
        ("c", "", "clear every filter and selection"),
        ("q esc", "ctrl+c", "quit without resuming"),
    ];
    let w = 78u16.min(area.width);
    let h = (rows.len() as u16 + 4).min(area.height);
    let r = centered(area, w, h);
    f.render_widget(Clear, r);

    let mut lines: Vec<Line> = Vec::new();
    for (plain, vim, desc) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!(" {plain:<11}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{vim:<14}"), Style::default().fg(DIM)),
            Span::raw(*desc),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " the middle column is the vim-style alias — you never need it",
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
    )));

    let p = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(" keys — any key closes ", Style::default().fg(ACCENT)))
            .title_alignment(Alignment::Center),
    );
    f.render_widget(p, r);
}

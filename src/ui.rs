//! Rendering.
//!
//! The look has one idea behind it: **the pool**. Mnemosyne is the spring of
//! memory, so the list is a water surface and older sessions sink. That gives
//! the interface its own visual language rather than a borrowed one:
//!
//! * A depth gutter runs down the left edge, coloured on the water ramp by how
//!   old each session is — recent ones are pale foam at the surface, old ones
//!   fade into deep indigo. Age becomes something you see rather than read.
//! * Date bands are drawn as ripples, not rules.
//! * The wordmark is lit letter by letter along the same ramp.
//! * Every glyph of chrome comes from the same small water alphabet.
//!
//! Everything clickable records its screen span into `app.hits` as it draws, so
//! the mouse handler hit-tests against what was actually rendered.

use crate::app::{Action, App, InputMode, Row};
use crate::art;
use crate::model::{compact_count, fit, human_dur, human_size, reltime, short_cwd, Sort};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

const CHROME: Color = Color::DarkGray;
const FAV: Color = Color::Yellow;
const LIVE: Color = Color::Green;
const TAG: Color = Color::Magenta;
/// A path that has since been deleted.
const GONE: Color = Color::Rgb(150, 84, 84);
const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;
/// The selected row's band: a shallow pool lit from below.
const BAND: Color = Color::Rgb(18, 42, 58);
/// Overlay panels sit on still, deep water so the list cannot bleed through.
const PANEL: Color = Color::Rgb(9, 17, 28);

const MARGIN: usize = 2;
/// margin + gutter + gap + cursor + gap + marker + gap
const PREFIX: usize = MARGIN + 1 + 1 + 1 + 1 + 1 + 1;

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// How near the surface a session sits, 1.0 = just now, 0.0 = long sunk.
/// Logarithmic, because the interesting differences are all in the first week.
fn depth(mtime: i64) -> f64 {
    let age = (chrono::Utc::now().timestamp() - mtime).max(0) as f64 / 86_400.0;
    let t = (1.0 + age).ln() / (1.0 + 400.0f64).ln();
    (1.0 - t).clamp(0.0, 1.0)
}

struct Cols {
    folder: usize,
    sub: usize,
    title: usize,
    preview: usize,
    model: usize,
    msgs: usize,
    tags: usize,
}

const TITLE_MAX: usize = 52;
// The depth gutter widened the row prefix, which pushed the cue column's
// threshold past a 150-column terminal. 20 is still enough to be worth a
// column and brings it back.
const PREVIEW_MIN: usize = 20;

impl Cols {
    fn new(width: usize) -> Cols {
        let folder = if width >= 150 {
            20
        } else if width >= 120 {
            16
        } else {
            12
        };
        let sub = 5;
        let model = if width >= 150 {
            12
        } else if width >= 110 {
            10
        } else {
            0
        };
        let msgs = 6;
        let tags = if width >= 140 { 16 } else { 0 };
        let fixed = PREFIX + 4 + 2 + folder + 1 + sub + model + msgs + tags;
        let avail = width.saturating_sub(fixed).max(16);
        let (title, preview) = if avail >= TITLE_MAX + PREVIEW_MIN + 2 {
            (TITLE_MAX, avail - TITLE_MAX - 2)
        } else {
            (avail, 0)
        };
        Cols {
            folder,
            sub,
            title,
            preview,
            model,
            msgs,
            tags,
        }
    }
}

/// Wrap to `width` on word boundaries, hard-breaking anything longer than a
/// line. ratatui's own wrapping cannot hang-indent continuations, and a reply
/// that wraps back to column zero is hard to read against the speaker labels.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut len = 0usize;
    for word in text.split_whitespace() {
        let wl = word.chars().count();
        if wl > width {
            if len > 0 {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                chunk.push(ch);
                if chunk.chars().count() == width {
                    out.push(std::mem::take(&mut chunk));
                }
            }
            if !chunk.is_empty() {
                line = chunk;
                len = line.chars().count();
            }
            continue;
        }
        if len > 0 && len + 1 + wl > width {
            out.push(std::mem::take(&mut line));
            len = 0;
        }
        if len > 0 {
            line.push(' ');
            len += 1;
        }
        line.push_str(word);
        len += wl;
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Character-wise shared-prefix test. Byte slicing panics mid-character and
/// prompts are full of non-ASCII.
fn same_prefix(a: &str, b: &str, want: usize) -> bool {
    let mut ai = a.trim().chars().flat_map(|c| c.to_lowercase());
    let mut bi = b.trim().chars().flat_map(|c| c.to_lowercase());
    let mut seen = 0usize;
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) if x == y => {
                seen += 1;
                if seen >= want {
                    return true;
                }
            }
            (None, None) => return seen > 0,
            _ => return false,
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_input = app.input_mode != InputMode::Normal && app.input_mode != InputMode::Help;
    let rail = if app.show_preview && area.height >= 18 {
        7
    } else {
        0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                              // wordmark
            Constraint::Length(1),                              // air
            Constraint::Length(1),                              // column heads
            Constraint::Min(3),                                 // the pool
            Constraint::Length(rail),                           // rail
            Constraint::Length(if show_input { 2 } else { 1 }), // input / air
            Constraint::Length(1),                              // footer
        ])
        .split(area);

    let cols = Cols::new(area.width as usize);

    draw_wordmark(f, app, rows[0]);
    draw_colheads(f, app, &cols, rows[2]);
    draw_pool(f, app, &cols, rows[3]);
    if rail > 0 {
        draw_rail(f, app, rows[4], cols.preview == 0);
    }
    if show_input {
        draw_input(f, app, rows[5]);
    }
    draw_footer(f, app, rows[6]);

    if app.input_mode == InputMode::Help {
        draw_help(f, area);
    }
    if app.input_mode == InputMode::Viewer {
        draw_viewer(f, app, area);
    }
}

/// Full-screen read of a session, so you can see what happened without
/// resuming it and changing it.
fn draw_viewer(f: &mut Frame, app: &mut App, area: Rect) {
    let Some((turns, more)) = app.viewer.clone() else {
        return;
    };
    let title = app
        .current()
        .map(|s| s.title().to_string())
        .unwrap_or_default();
    let body_w = (area.width as usize).saturating_sub(MARGIN * 2 + 10);

    let mut lines: Vec<Line> = Vec::new();
    if more {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(
                "…earlier of this conversation not shown",
                Style::default().fg(CHROME).add_modifier(Modifier::ITALIC),
            ),
        ]));
        lines.push(Line::raw(""));
    }
    // A run of turns that are nothing but tool calls is a wall of "[Bash]"
    // that tells you nothing. Collapse each run into one line that still says
    // which tools ran, and how many times.
    let only_tools = |t: &str| {
        let t = t.trim();
        !t.is_empty()
            && t.split_whitespace()
                .all(|w| w.starts_with('[') && w.ends_with(']'))
    };
    let mut i = 0;
    while i < turns.len() {
        if only_tools(&turns[i].text) {
            let start = i;
            let mut names: Vec<String> = Vec::new();
            while i < turns.len() && only_tools(&turns[i].text) {
                for w in turns[i].text.split_whitespace() {
                    let n = w.trim_matches(|c| c == '[' || c == ']').to_string();
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                i += 1;
            }
            let calls = i - start;
            let summary = if calls == 1 {
                format!("ran {}", names.join(", "))
            } else {
                format!("ran {} · {calls} calls", names.join(", "))
            };
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled("        ", Style::default()),
                Span::styled(
                    summary,
                    Style::default().fg(CHROME).add_modifier(Modifier::ITALIC),
                ),
            ]));
            lines.push(Line::raw(""));
            continue;
        }
        let t = &turns[i];
        let (label, colour) = if t.role == "you" {
            ("you", rgb(art::ramp(0.85)))
        } else {
            ("claude", FAV)
        };
        for (n, chunk) in wrap_words(&t.text, body_w).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                if n == 0 {
                    Span::styled(
                        format!("{label:<8}"),
                        Style::default().fg(colour).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("        ")
                },
                Span::styled(chunk, Style::default().fg(TEXT)),
            ]));
        }
        lines.push(Line::raw(""));
        i += 1;
    }

    // Lines are pre-wrapped, so the height is exact and scrolling can stop
    // precisely at the bottom.
    let page = area.height.saturating_sub(3);
    app.viewer_height = lines.len() as u16;
    app.viewer_page = page;
    if app.viewer_scroll > app.viewer_height.saturating_sub(page.max(1)) {
        app.viewer_scroll = app.viewer_height.saturating_sub(page.max(1));
    }

    f.render_widget(Clear, area);

    let head = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled("⌇ ", Style::default().fg(rgb(art::ramp(0.6)))),
                Span::styled(
                    fit(&title, area.width as usize - 12),
                    Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
                ),
            ]),
            ripple_line(area.width as usize, MARGIN, 1.7),
        ]))
        .block(Block::default().style(Style::default().bg(PANEL))),
        head[0],
    );

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((app.viewer_scroll, 0))
            .block(Block::default().style(Style::default().bg(PANEL))),
        head[1],
    );

    let pct = if app.viewer_height > page {
        (app.viewer_scroll as f64 / (app.viewer_height - page) as f64 * 100.0).round() as u32
    } else {
        100
    };
    let k = |t: &'static str| Span::styled(t, Style::default().fg(rgb(art::ramp(0.85))));
    let d = |t: &'static str| Span::styled(t, Style::default().fg(CHROME));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            k("↑↓"),
            d(" scroll   "),
            k("↵"),
            d(" resume this one   "),
            k("esc"),
            d(" back   "),
            Span::styled(
                format!("{} turns · {pct}%", turns.len()),
                Style::default().fg(CHROME),
            ),
        ]))
        .block(Block::default().style(Style::default().bg(PANEL))),
        head[2],
    );
}

/// `⌇ m n e m o s y n e` — the name lit along the water ramp.
fn draw_wordmark(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled("⌇ ", Style::default().fg(rgb(art::ramp(0.55)))),
    ];
    let letters: Vec<char> = art::WORD.chars().collect();
    for (i, ch) in letters.iter().enumerate() {
        let p = 0.30 + (i as f64 / (letters.len() - 1) as f64) * 0.70;
        spans.push(Span::styled(
            format!("{ch}"),
            Style::default()
                .fg(rgb(art::ramp(p)))
                .add_modifier(Modifier::BOLD),
        ));
    }

    let dot = || Span::styled(" · ", Style::default().fg(CHROME));
    let mut right: Vec<Span> = vec![Span::styled(
        format!("{} sessions", app.item_count()),
        Style::default().fg(TEXT),
    )];
    let favs = app.meta.favorite_count();
    if favs > 0 {
        right.push(dot());
        right.push(Span::styled(format!("★{favs}"), Style::default().fg(FAV)));
    }
    if app.live.count > 0 {
        right.push(dot());
        right.push(Span::styled(
            format!("●{} live", app.live.count),
            Style::default().fg(LIVE),
        ));
    }
    right.push(dot());
    right.push(Span::styled(app.sort.label(), Style::default().fg(CHROME)));
    for (on, label, col) in [
        (
            app.group_by_dir,
            "grouped".to_string(),
            rgb(art::ramp(0.75)),
        ),
        (
            app.date != crate::model::DateRange::All,
            app.date.label(),
            rgb(art::ramp(0.75)),
        ),
        (app.fav_only, "★ only".to_string(), FAV),
        (app.live_only, "live only".to_string(), LIVE),
        (
            app.tag_filter.is_some(),
            format!("#{}", app.tag_filter.clone().unwrap_or_default()),
            TAG,
        ),
        (
            !app.fuzzy.trim().is_empty(),
            format!("/{}", app.fuzzy),
            rgb(art::ramp(0.85)),
        ),
        (
            app.deep_busy,
            "searching…".to_string(),
            rgb(art::ramp(0.85)),
        ),
        (
            !app.deep_busy && app.deep_hits.is_some(),
            format!("{} “{}”", app.deep_mode.label(), app.deep),
            rgb(art::ramp(0.85)),
        ),
        (
            !app.selected.is_empty(),
            format!("{} picked", app.selected.len()),
            rgb(art::ramp(0.9)),
        ),
        (!app.mouse_on, "mouse off".to_string(), CHROME),
    ] {
        if on {
            right.push(dot());
            right.push(Span::styled(label, Style::default().fg(col)));
        }
    }

    let lw: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let rw: usize = right.iter().map(|s| s.content.chars().count()).sum();
    spans.push(Span::raw(
        " ".repeat((area.width as usize).saturating_sub(lw + rw + MARGIN)),
    ));
    spans.extend(right);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Column headings, and the clickable spans that sort by them.
fn draw_colheads(f: &mut Frame, app: &mut App, c: &Cols, area: Rect) {
    app.hits.columns.clear();
    app.hits.colhead_y = area.y;
    let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(PREFIX))];
    let mut x = area.x + PREFIX as u16;

    let head = |spans: &mut Vec<Span>,
                x: &mut u16,
                text: String,
                w: usize,
                sort: Option<Sort>,
                hits: &mut Vec<(u16, u16, Sort)>| {
        if w == 0 {
            return;
        }
        if let Some(s) = sort {
            hits.push((*x, *x + text.trim().chars().count() as u16, s));
        }
        *x += w as u16;
        spans.push(Span::styled(text, Style::default().fg(CHROME)));
    };

    let hits = &mut app.hits.columns;
    head(
        &mut spans,
        &mut x,
        format!("{:>4}  ", "AGE"),
        6,
        Some(Sort::Recency),
        hits,
    );
    head(
        &mut spans,
        &mut x,
        format!("{:<w$} ", "FOLDER", w = c.folder),
        c.folder + 1,
        Some(Sort::Folder),
        hits,
    );
    head(&mut spans, &mut x, " ".repeat(c.sub), c.sub, None, hits);
    head(
        &mut spans,
        &mut x,
        format!("{:<w$}", "TITLE", w = c.title),
        c.title,
        Some(Sort::Title),
        hits,
    );
    if c.preview > 0 {
        head(
            &mut spans,
            &mut x,
            format!("  {:<w$}", "LEFT OFF", w = c.preview),
            c.preview + 2,
            None,
            hits,
        );
    }
    head(
        &mut spans,
        &mut x,
        format!("{:<w$}", "MODEL", w = c.model),
        c.model,
        None,
        hits,
    );
    head(
        &mut spans,
        &mut x,
        format!("{:>w$}", "MSGS", w = c.msgs),
        c.msgs,
        Some(Sort::Entries),
        hits,
    );
    if c.tags > 0 {
        head(&mut spans, &mut x, "  TAGS".to_string(), 6, None, hits);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_pool(f: &mut Frame, app: &mut App, c: &Cols, area: Rect) {
    let width = area.width as usize;
    let cursor = app.cursor;

    // where the subagent cell sits, so a click on ⌁ can expand it
    let sub_x = area.x + (PREFIX + 4 + 2 + c.folder + 1) as u16;
    app.sub_span = (sub_x, sub_x + c.sub as u16);

    let items: Vec<ListItem> = app
        .view
        .iter()
        .enumerate()
        .map(|(row_i, r)| match r {
            // a ripple across the surface, with the band name riding it
            Row::Divider(label) => {
                let text = format!("{label}  ");
                let used = MARGIN + 2 + text.chars().count();
                let mut sp = vec![
                    Span::raw(" ".repeat(MARGIN)),
                    Span::styled(
                        "≈ ",
                        Style::default().fg(rgb(art::sink(art::ramp(0.5), 0.3))),
                    ),
                    Span::styled(
                        text,
                        Style::default()
                            .fg(rgb(art::ramp(0.55)))
                            .add_modifier(Modifier::ITALIC),
                    ),
                ];
                sp.extend(ripple_rule(width.saturating_sub(used), 0, 0.0));
                ListItem::new(Line::from(sp))
            }
            Row::Header(dir, n) => ListItem::new(Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled("▌ ", Style::default().fg(rgb(art::ramp(0.7)))),
                Span::styled(
                    format!("{dir}  "),
                    Style::default()
                        .fg(rgb(art::ramp(0.8)))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{n}"), Style::default().fg(CHROME)),
            ])),
            Row::Item(i) | Row::Sub(i) => {
                let s = &app.all[*i];
                let is_sub = matches!(r, Row::Sub(_));
                let key = s.path.to_string_lossy().to_string();
                let d = depth(s.mtime);
                let mut sp: Vec<Span> = Vec::new();

                // the depth gutter: how far this session has sunk
                sp.push(Span::raw(" ".repeat(MARGIN)));
                sp.push(Span::styled(
                    if is_sub { "│" } else { "▌" },
                    Style::default().fg(rgb(art::sink(
                        art::ramp(0.25 + d * 0.75),
                        if is_sub { 0.5 } else { 0.0 },
                    ))),
                ));
                sp.push(Span::raw(" "));

                sp.push(Span::styled(
                    if row_i == cursor { "❯" } else { " " },
                    Style::default()
                        .fg(rgb(art::ramp(1.0)))
                        .add_modifier(Modifier::BOLD),
                ));
                sp.push(Span::raw(" "));

                let (glyph, gstyle) = if app.selected.contains(&key) {
                    (
                        "◆",
                        Style::default()
                            .fg(rgb(art::ramp(0.9)))
                            .add_modifier(Modifier::BOLD),
                    )
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

                // age reads on the same ramp, floored so it stays legible
                sp.push(Span::styled(
                    format!("{:>4}", reltime(s.mtime)),
                    Style::default().fg(rgb(art::ramp(0.30 + d * 0.5))),
                ));
                sp.push(Span::raw("  "));

                if is_sub {
                    sp.push(Span::styled(
                        format!("{:<w$} ", "└ subagent", w = c.folder),
                        Style::default().fg(CHROME),
                    ));
                } else {
                    let folder = if s.cwd.is_empty() {
                        "—".to_string()
                    } else {
                        short_cwd(&s.cwd)
                    };
                    // A directory that no longer exists is worth seeing here
                    // rather than discovering at resume time.
                    let fstyle = if s.cwd_missing {
                        Style::default().fg(GONE)
                    } else {
                        Style::default().fg(rgb(art::ramp(0.45 + d * 0.2)))
                    };
                    sp.push(Span::styled(
                        format!("{:<w$} ", fit(&folder, c.folder), w = c.folder),
                        fstyle,
                    ));
                }

                if s.subagent_count > 0 && !is_sub {
                    sp.push(Span::styled(
                        format!(
                            "{:<w$}",
                            fit(&format!("⌁{}", s.subagent_count), c.sub - 1),
                            w = c.sub
                        ),
                        Style::default().fg(rgb(art::ramp(0.5))),
                    ));
                } else {
                    sp.push(Span::raw(" ".repeat(c.sub)));
                }

                sp.push(Span::styled(
                    format!("{:<w$}", fit(s.title(), c.title), w = c.title),
                    Style::default()
                        .fg(if s.is_live() { BRIGHT } else { TEXT })
                        .add_modifier(if row_i == cursor {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ));

                if c.preview > 0 {
                    let mut cue = if !s.last_prompt.is_empty() {
                        s.last_prompt.as_str()
                    } else {
                        s.first_prompt.as_str()
                    };
                    if same_prefix(cue, s.title(), 24) {
                        cue = "";
                    }
                    sp.push(Span::raw("  "));
                    sp.push(Span::styled(
                        format!(
                            "{:<w$}",
                            fit(cue, c.preview.saturating_sub(2)),
                            w = c.preview
                        ),
                        Style::default().fg(CHROME),
                    ));
                }
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
        .highlight_style(Style::default().bg(BAND));

    app.list_state.select(Some(app.cursor));
    f.render_stateful_widget(list, area, &mut app.list_state);

    app.hits.list = area;
    app.hits.list_offset = app.list_state.offset();
}

/// A rule made of water. Only the crests print, and they thin out toward the
/// right so the line dissolves instead of shouting across the whole screen.
fn ripple_rule(width: usize, indent: usize, phase: f64) -> Vec<Span<'static>> {
    let span = (width as f64 * 0.55).max(24.0);
    let mut sp = vec![Span::raw(" ".repeat(indent))];
    for i in 0..width.saturating_sub(indent * 2) {
        let fade = (1.0 - i as f64 / span).clamp(0.0, 1.0);
        let (ch, inten) = art::ripple_at(i, phase, 0.0);
        let v = inten * fade;
        if v < 0.42 {
            sp.push(Span::raw(" "));
        } else {
            sp.push(Span::styled(
                ch.to_string(),
                Style::default().fg(rgb(art::sink(art::ramp(0.18 + v * 0.22), 0.45))),
            ));
        }
    }
    sp
}

fn ripple_line(width: usize, indent: usize, phase: f64) -> Line<'static> {
    Line::from(ripple_rule(width, indent, phase))
}

fn draw_rail(f: &mut Frame, app: &mut App, area: Rect, show_cue: bool) {
    let turns = app.preview(8);
    let snippet = app.deep_snippet().cloned();
    let width = area.width as usize;

    let Some(s) = app.current().cloned() else {
        let mut lines = vec![ripple_line(width, MARGIN, 1.7), Line::raw("")];
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(
                "still water — nothing matches. ",
                Style::default().fg(CHROME),
            ),
            Span::styled("c", Style::default().fg(rgb(art::ramp(0.85)))),
            Span::styled(" clears the filters", Style::default().fg(CHROME)),
        ]));
        f.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    };

    let mut lines: Vec<Line> = vec![ripple_line(width, MARGIN, 1.7)];

    let mut facts: Vec<String> = vec![if s.cwd_missing {
        format!("{} (gone)", short_cwd(&s.cwd))
    } else {
        short_cwd(&s.cwd)
    }];
    if !s.git_branch.is_empty() {
        facts.push(s.git_branch.clone());
    }
    facts.push(human_size(s.size));
    facts.push(format!("{} entries", s.entries));
    if s.duration_secs() > 0 {
        facts.push(human_dur(s.duration_secs()));
    }
    if !s.permission_mode.is_empty() && s.permission_mode != "default" {
        facts.push(match s.permission_mode.as_str() {
            "bypassPermissions" => "bypass".into(),
            o => o.to_string(),
        });
    }
    if let Some(pid) = s.live_pid {
        facts.push(format!(
            "{} {pid}",
            if s.live_exact {
                "running"
            } else {
                "likely running"
            }
        ));
    }
    if s.has_tmux {
        facts.push(format!("tmux {}", crate::live::tmux_name(&s.id)));
    }
    let fact_str = facts.join(" · ");
    let title = fit(
        s.title(),
        width.saturating_sub(fact_str.chars().count() + MARGIN * 3),
    );
    let gap = width
        .saturating_sub(title.chars().count() + fact_str.chars().count() + MARGIN * 2)
        .max(2);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled(
            title,
            Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(fact_str, Style::default().fg(CHROME)),
    ]));

    if !s.tags.is_empty() || !s.note.is_empty() {
        let mut sp = vec![Span::raw(" ".repeat(MARGIN))];
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
            Style::default().fg(rgb(art::ramp(0.72))),
        )
    };
    let body = width.saturating_sub(14);
    const BODY: usize = 3;
    let mut body_lines: Vec<Line> = Vec::new();
    if let Some(sn) = snippet {
        body_lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            label("match"),
            Span::styled(fit(&sn, body), Style::default().fg(BRIGHT)),
        ]));
    }
    if show_cue && !s.last_prompt.is_empty() {
        body_lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            label("left off"),
            Span::styled(fit(&s.last_prompt, body), Style::default().fg(BRIGHT)),
        ]));
    }
    let only_tools = |t: &str| {
        let t = t.trim();
        !t.is_empty()
            && t.split_whitespace()
                .all(|w| w.starts_with('[') && w.ends_with(']'))
    };
    for t in turns.iter().rev() {
        if body_lines.len() >= BODY {
            break;
        }
        if t.role == "you" && same_prefix(&t.text, &s.last_prompt, 48) {
            continue;
        }
        if only_tools(&t.text) {
            continue;
        }
        body_lines.insert(
            body_lines.len(),
            Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled(
                    format!("{:<10}", t.role),
                    Style::default().fg(if t.role == "you" {
                        rgb(art::ramp(0.8))
                    } else {
                        FAV
                    }),
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
        InputMode::Fuzzy => (
            "filter",
            app.fuzzy.clone(),
            "enter keeps it · esc clears".into(),
        ),
        InputMode::Deep => (
            "search in conversations",
            app.deep.clone(),
            format!(
                "enter runs it · tab switches mode (now {})",
                app.deep_mode.label()
            ),
        ),
        InputMode::TagAdd => (
            "tag",
            app.input.clone(),
            format!(
                "enter adds · -name removes · tab completes    {}",
                app.tag_completions().join("   ")
            ),
        ),
        InputMode::TagFilter => (
            "show only tag",
            app.input.clone(),
            format!(
                "enter applies · empty clears    {}",
                app.tag_completions().join("   ")
            ),
        ),
        InputMode::Note => ("note", app.input.clone(), "enter saves".into()),
        _ => ("", String::new(), String::new()),
    };
    let line = Line::from(vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled("⌇ ", Style::default().fg(rgb(art::ramp(0.6)))),
        Span::styled(
            format!("{label} "),
            Style::default()
                .fg(rgb(art::ramp(0.85)))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            value,
            Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::default().fg(rgb(art::ramp(1.0)))),
        Span::styled(format!("   {hint}"), Style::default().fg(CHROME)),
    ]);
    f.render_widget(Paragraph::new(Text::from(vec![Line::raw(""), line])), area);
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let n = app.item_count();
    let at = app
        .view
        .iter()
        .take(app.cursor + 1)
        .filter(|r| r.selectable())
        .count();
    let pos = format!("{at}/{n}");

    app.hits.footer.clear();
    app.hits.footer_y = area.y;
    let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(MARGIN))];
    let mut x = area.x + MARGIN as u16;

    if app.status.is_empty() {
        let budget = area.width as usize;
        let mut hints: Vec<(&'static str, &'static str, Option<Action>)> = vec![
            ("↑↓", " move   ", None),
            ("↵", " resume   ", Some(Action::Resume)),
            ("/", " filter   ", Some(Action::Filter)),
            ("F", " search   ", Some(Action::Search)),
            ("^t", " tmux   ", Some(Action::Tmux)),
            ("f", " ★   ", Some(Action::Favorite)),
            ("t", " tag   ", Some(Action::Tag)),
            ("s", " sort   ", Some(Action::CycleSort)),
            ("?", " keys", Some(Action::Help)),
        ];
        loop {
            let w: usize = hints
                .iter()
                .map(|(a, b, _)| a.chars().count() + b.chars().count())
                .sum();
            if w + pos.chars().count() + 6 <= budget || hints.len() <= 2 {
                break;
            }
            hints.remove(hints.len() - 2);
        }
        for (k, d, act) in hints {
            let kw = k.chars().count() as u16;
            if let Some(a) = act {
                app.hits
                    .footer
                    .push((x, x + kw + d.trim_end().chars().count() as u16, a));
            }
            spans.push(Span::styled(k, Style::default().fg(rgb(art::ramp(0.85)))));
            spans.push(Span::styled(d, Style::default().fg(CHROME)));
            x += kw + d.chars().count() as u16;
        }
    } else {
        spans.push(Span::styled(
            fit(
                &app.status,
                (area.width as usize).saturating_sub(pos.chars().count() + 6),
            ),
            Style::default().fg(FAV),
        ));
    }

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    spans.push(Span::raw(" ".repeat(
        (area.width as usize).saturating_sub(used + pos.chars().count() + MARGIN),
    )));
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
        ("click", "", "select · click again to resume"),
        ("right-click", "", "favourite it"),
        ("wheel", "", "scroll the pool"),
        ("v", "", "read the conversation without resuming it"),
        ("click a heading", "", "sort by that column"),
        ("click ⌁n", "", "open that session's subagents"),
        ("M", "", "mouse off, so the terminal can select text again"),
        ("", "", ""),
        ("↑ ↓", "k j", "move"),
        ("pgup pgdn", "^u ^d", "jump ten"),
        ("home end", "g G", "first · last"),
        ("", "", ""),
        ("enter", "", "resume here: cd to its folder and reattach"),
        ("ctrl+n", "alt+enter", "resume in a new terminal window"),
        (
            "ctrl+t",
            "",
            "resume in tmux — attaches if one is already waiting",
        ),
        ("space", "", "pick several, then enter reopens them all"),
        ("", "", ""),
        ("/", "", "filter titles, folders, branches, tags"),
        ("F", "^f", "search inside the conversations"),
        ("m", "", "search mode: content · file touched · tool used"),
        ("", "", ""),
        ("f", "", "favourite — favourites float to the surface"),
        (
            "t",
            "",
            "tag — applies to the whole selection; -name removes, old>new renames",
        ),
        ("T", "", "show one tag only"),
        ("N", "", "private note"),
        ("", "", ""),
        (
            "s",
            "",
            "sort: recency · size · entries · duration · title · folder",
        ),
        ("o", "", "group by directory"),
        ("D", "", "date range"),
        ("*", "", "favourites only"),
        ("L", "", "running only"),
        ("a", "", "reveal subagents"),
        ("→ ←", "l h", "expand · collapse subagents"),
        ("p", "", "preview rail"),
        ("R", "f5", "reindex"),
        ("c", "", "clear everything"),
        ("q esc", "^c", "quit"),
    ];

    let mark = art::mark_for(area.width.saturating_sub(6) as usize);
    let art_w = mark.as_ref().map(|m| m.width).unwrap_or(0) as u16;
    let art_h = mark.as_ref().map(|m| m.height()).unwrap_or(0);
    let show_art = mark.is_some() && area.height as usize >= rows.len() + art_h + 6;
    // A centred panel narrower than the terminal leaves the list showing down
    // both sides, which looks like a rendering fault rather than an overlay.
    // Help is a screenful of content, so it takes the screen.
    let (w, h) = if show_art {
        (area.width, area.height)
    } else {
        (
            80u16.min(area.width),
            (rows.len() as u16 + 4).min(area.height),
        )
    };
    let r = centered(area, w, h);
    f.render_widget(Clear, r);

    let mut lines: Vec<Line> = vec![Line::raw("")];
    if show_art {
        let m = mark.as_ref().unwrap();
        let indent = (w as usize).saturating_sub(m.width) / 2;
        for (row, chars) in m.rows.iter().enumerate() {
            let mut sp: Vec<Span> = vec![Span::raw(" ".repeat(indent))];
            let glyphs: Vec<Span> = chars
                .iter()
                .enumerate()
                .map(|(x, ch)| {
                    if *ch == ' ' {
                        return Span::raw(" ");
                    }
                    let c = art::column_color(x, art_w as usize, -99.0, row, art_h);
                    Span::styled(ch.to_string(), Style::default().fg(rgb(c)))
                })
                .collect();
            sp.extend(glyphs);
            lines.push(Line::from(sp));
        }
        lines.push(ripple_line(w as usize, indent.max(2), 1.7));
        lines.push(Line::raw(""));
    }
    for (plain, alt, desc) in rows {
        if plain.is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{plain:<16}"),
                Style::default().fg(rgb(art::ramp(0.85))),
            ),
            Span::styled(format!("{alt:<11}"), Style::default().fg(CHROME)),
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

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            // A filled panel, because a centred overlay narrower than the
            // terminal otherwise shows the list either side of it.
            .block(Block::default().style(Style::default().bg(PANEL))),
        r,
    );
}

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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

/// Resolved once from the config, so the palette can follow a desktop theme
/// without a rebuild. Missing keys keep the values the tool shipped with.
struct Theme {
    chrome: Color,
    fav: Color,
    live: Color,
    tag: Color,
    text: Color,
    bright: Color,
    gone: Color,
    band: Color,
    panel: Color,
}

static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

fn th() -> &'static Theme {
    THEME.get_or_init(|| Theme::from_config(&crate::config::Config::load()))
}

impl Theme {
    fn from_config(c: &crate::config::Config) -> Theme {
        let pick = |given: &Option<String>, fallback: Color| -> Color {
            given
                .as_deref()
                .and_then(crate::config::parse_color)
                .map(|(r, g, b)| Color::Rgb(r, g, b))
                .unwrap_or(fallback)
        };
        Theme {
            chrome: pick(&c.chrome, Color::DarkGray),
            fav: pick(&c.favorite, Color::Yellow),
            live: pick(&c.live, Color::Green),
            tag: pick(&c.tag, Color::Magenta),
            text: pick(&c.text, Color::Gray),
            bright: pick(&c.bright, Color::White),
            gone: pick(&c.gone, Color::Rgb(150, 84, 84)),
            band: pick(&c.band, Color::Rgb(18, 42, 58)),
            panel: pick(&c.panel, Color::Rgb(9, 17, 28)),
        }
    }
}

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
    tokens: usize,
    tags: usize,
}

const TITLE_MAX: usize = 52;
// The depth gutter widened the row prefix, which pushed the cue column's
// threshold past a 150-column terminal. 20 is still enough to be worth a
// column and brings it back.
const PREVIEW_MIN: usize = 20;

impl Cols {
    fn new(width: usize) -> Cols {
        // Each column has to earn its place. Below these widths there is not
        // enough room for the table to be worth more than the title, so they
        // are dropped rather than allowed to overflow.
        let folder = if width >= 150 {
            20
        } else if width >= 120 {
            16
        } else if width >= 56 {
            12
        } else {
            0
        };
        let sub = if width >= 50 { 5 } else { 0 };
        let model = if width >= 150 {
            12
        } else if width >= 110 {
            10
        } else {
            0
        };
        let msgs = if width >= 44 { 6 } else { 0 };
        let tokens = if width >= 168 { 8 } else { 0 };
        let tags = if width >= 140 { 16 } else { 0 };
        let gap = if folder > 0 { 1 } else { 0 };
        let fixed = PREFIX + 4 + 2 + folder + gap + sub + model + msgs + tokens + tags;
        let avail = width.saturating_sub(fixed).max(8);
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
            tokens,
            tags,
        }
    }
}

/// Wrap to `width` on word boundaries, hard-breaking anything longer than a
/// line. ratatui's own wrapping cannot hang-indent continuations, and a reply
/// that wraps back to column zero is hard to read against the speaker labels.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    // Honour the width asked for. Clamping it upward produced lines wider
    // than the caller had room for, which is the one thing wrapping is for.
    let width = width.max(1);
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
    // The blank line under the wordmark is where the reopen offer goes, so
    // an offer never pushes the list around: it fills air that was there
    // anyway, and the rows below it do not move.
    app.hits.banner.clear();
    app.hits.banner_y = None;
    if !app.reopen.is_empty() {
        draw_reopen(f, app, rows[1]);
    }
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
        draw_help(f, app, area);
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
                Style::default()
                    .fg(th().chrome)
                    .add_modifier(Modifier::ITALIC),
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
                    Style::default()
                        .fg(th().chrome)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            lines.push(Line::raw(""));
            continue;
        }
        let t = &turns[i];
        let (label, colour) = if t.role == "you" {
            ("you", rgb(art::ramp(0.85)))
        } else {
            ("claude", th().fav)
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
                Span::styled(chunk, Style::default().fg(th().text)),
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
                Span::styled("≈ ", Style::default().fg(rgb(art::ramp(0.6)))),
                Span::styled(
                    fit(&title, area.width as usize - 12),
                    Style::default()
                        .fg(th().bright)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            ripple_line(area.width as usize, MARGIN, 1.7),
        ]))
        .block(Block::default().style(Style::default().bg(th().panel))),
        head[0],
    );

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((app.viewer_scroll, 0))
            .block(Block::default().style(Style::default().bg(th().panel))),
        head[1],
    );

    let pct = if app.viewer_height > page {
        (app.viewer_scroll as f64 / (app.viewer_height - page) as f64 * 100.0).round() as u32
    } else {
        100
    };
    let k = |t: &'static str| Span::styled(t, Style::default().fg(rgb(art::ramp(0.85))));
    let d = |t: &'static str| Span::styled(t, Style::default().fg(th().chrome));
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
                Style::default().fg(th().chrome),
            ),
        ]))
        .block(Block::default().style(Style::default().bg(th().panel))),
        head[2],
    );
}

/// The offer to put back what a reboot took away.
///
/// Only ever an offer. Restoring on login without being asked would mean a
/// pile of terminal windows and a Claude process each, before you had said
/// you wanted any of them.
fn draw_reopen(f: &mut Frame, app: &mut App, area: Rect) {
    let n = app.reopen.len();
    let what = if n == 1 {
        "1 session was".to_string()
    } else {
        format!("{n} sessions were")
    };
    // Naming one of them is what makes the offer legible: a bare count could
    // mean anything, and you cannot tell whether you want it back.
    let first = app.reopen[0].title.trim().to_string();
    let head = if first.is_empty() || n > 1 {
        format!("{what} open before the reboot")
    } else {
        format!("{what} open before the reboot: {first}")
    };

    let key = Style::default()
        .fg(rgb(art::ramp(0.95)))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(th().chrome);

    let mut spans = vec![
        Span::raw(" ".repeat(MARGIN)),
        // Hollow, not solid: `●` means running *now* everywhere else in this
        // interface, and these are exactly the sessions that are not.
        Span::styled("◌ ", Style::default().fg(th().live)),
        Span::styled(head.clone(), Style::default().fg(th().text)),
        Span::raw("   "),
    ];
    let mut x = area.x + MARGIN as u16 + 2 + head.chars().count() as u16 + 3;

    app.hits.banner_y = Some(area.y);
    for (k, label, action) in [
        ("r", " reopen   ", Action::Reopen),
        ("x", " not now", Action::DismissReopen),
    ] {
        let w = (k.chars().count() + label.chars().count()) as u16;
        // The whole phrase is the target, not just the letter: a one-column
        // click target is not a click target.
        app.hits.banner.push((x, x + w.saturating_sub(1), action));
        spans.push(Span::styled(k, key));
        spans.push(Span::styled(label, dim));
        x += w;
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `≈ m n e m o s y n e` — the name lit along the water ramp.
fn draw_wordmark(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled("≈ ", Style::default().fg(rgb(art::ramp(0.55)))),
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

    // The right side is built as separate pieces so it can be thinned rather
    // than truncated: a narrow terminal drops whole facts, worst-first,
    // instead of cutting one in half.
    let mut segs: Vec<(u8, Vec<Span>)> = Vec::new();
    let plain = |t: String, c: Color| vec![Span::styled(t, Style::default().fg(c))];

    // priority 0 is kept longest
    segs.push((
        0,
        plain(format!("{} sessions", app.item_count()), th().text),
    ));
    if app.corpus_tokens > 0 {
        segs.push((
            4,
            plain(
                format!("{} tokens", crate::model::human_count(app.corpus_tokens)),
                th().chrome,
            ),
        ));
    }
    let favs = app.meta.favorite_count();
    if favs > 0 {
        segs.push((3, plain(format!("★{favs}"), th().fav)));
    }
    let live = app.live_shown();
    if live > 0 {
        segs.push((2, plain(format!("●{live} live"), th().live)));
    }
    segs.push((5, plain(app.sort.label().to_string(), th().chrome)));
    if let Some(found) = &app.update_notice {
        // Near the front, because it is the one thing on screen that asks
        // something of you.
        let (text, colour) = match found {
            crate::update::Found::Installed(v) => (
                format!("v{v} installed — restart to update"),
                rgb(art::ramp(0.95)),
            ),
            crate::update::Found::Available(v) => {
                (format!("v{v} available — mn --update"), th().fav)
            }
        };
        segs.push((1, plain(text, colour)));
    }

    // Anything that explains why the list looks the way it does stays near
    // the front: without it the view is inexplicable.
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
        (app.fav_only, "★ only".to_string(), th().fav),
        (app.live_only, "live only".to_string(), th().live),
        (
            app.tag_filter.is_some(),
            format!("#{}", app.tag_filter.clone().unwrap_or_default()),
            th().tag,
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
        (!app.mouse_on, "mouse off".to_string(), th().chrome),
    ] {
        if on {
            segs.push((1, plain(label, col)));
        }
    }

    let lw: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let width = area.width as usize;
    let seg_len = |v: &Vec<Span>| v.iter().map(|s| s.content.chars().count()).sum::<usize>();
    // drop the least important piece until what remains fits with a gap
    loop {
        let joined: usize =
            segs.iter().map(|(_, v)| seg_len(v)).sum::<usize>() + 3 * segs.len().saturating_sub(1);
        if lw + joined + MARGIN + 2 <= width || segs.is_empty() {
            break;
        }
        let worst = segs
            .iter()
            .enumerate()
            .max_by_key(|(i, (pri, _))| (*pri, *i))
            .map(|(i, _)| i)
            .unwrap();
        segs.remove(worst);
    }

    let mut right: Vec<Span> = Vec::new();
    for (i, (_, v)) in segs.into_iter().enumerate() {
        if i > 0 {
            right.push(Span::styled(" · ", Style::default().fg(th().chrome)));
        }
        right.extend(v);
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
        spans.push(Span::styled(text, Style::default().fg(th().chrome)));
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
    if c.folder > 0 {
        head(
            &mut spans,
            &mut x,
            format!("{:<w$} ", "FOLDER", w = c.folder),
            c.folder + 1,
            Some(Sort::Folder),
            hits,
        );
    }
    if c.sub > 0 {
        head(&mut spans, &mut x, " ".repeat(c.sub), c.sub, None, hits);
    }
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
    if c.msgs > 0 {
        head(
            &mut spans,
            &mut x,
            format!("{:>w$}", "MSGS", w = c.msgs),
            c.msgs,
            Some(Sort::Entries),
            hits,
        );
    }
    if c.tokens > 0 {
        head(
            &mut spans,
            &mut x,
            format!("{:>w$}", "TOKENS", w = c.tokens),
            c.tokens,
            Some(Sort::Tokens),
            hits,
        );
    }
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
                Span::styled(format!("{n}"), Style::default().fg(th().chrome)),
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
                    ("★", Style::default().fg(th().fav))
                } else if s.is_live() {
                    (
                        if s.live_exact { "●" } else { "◌" },
                        Style::default().fg(th().live),
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

                if c.folder == 0 {
                    // dropped on a narrow terminal
                } else if is_sub {
                    sp.push(Span::styled(
                        format!("{:<w$} ", "└ subagent", w = c.folder),
                        Style::default().fg(th().chrome),
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
                        Style::default().fg(th().gone)
                    } else {
                        Style::default().fg(rgb(art::ramp(0.45 + d * 0.2)))
                    };
                    sp.push(Span::styled(
                        format!("{:<w$} ", fit(&folder, c.folder), w = c.folder),
                        fstyle,
                    ));
                }

                if c.sub == 0 {
                    // dropped on a narrow terminal
                } else if s.subagent_count > 0 && !is_sub {
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
                        .fg(if s.is_live() { th().bright } else { th().text })
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
                        Style::default().fg(th().chrome),
                    ));
                }
                if c.model > 0 {
                    sp.push(Span::styled(
                        format!("{:<w$}", fit(s.model_short(), c.model - 1), w = c.model),
                        Style::default().fg(th().chrome),
                    ));
                }
                if c.msgs > 0 {
                    sp.push(Span::styled(
                        format!("{:>w$}", compact_count(s.entries), w = c.msgs),
                        Style::default().fg(th().chrome),
                    ));
                }
                if c.tokens > 0 {
                    // Token totals run into the billions, well past u32;
                    // clamping to it made every large session read 4295.0m.
                    let t = s.total_tokens();
                    sp.push(Span::styled(
                        format!(
                            "{:>w$}",
                            if t == 0 {
                                String::new()
                            } else {
                                crate::model::human_count(t)
                            },
                            w = c.tokens
                        ),
                        Style::default().fg(th().chrome),
                    ));
                }
                for t in &s.tags {
                    sp.push(Span::styled(
                        format!("  #{t}"),
                        Style::default().fg(th().tag),
                    ));
                }
                ListItem::new(Line::from(sp))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default())
        .highlight_style(Style::default().bg(th().band));

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
    let snippet = app.deep_snippet();
    let width = area.width as usize;

    let Some(s) = app.current().cloned() else {
        let mut lines = vec![ripple_line(width, MARGIN, 1.7), Line::raw("")];
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(
                "still water — nothing matches. ",
                Style::default().fg(th().chrome),
            ),
            Span::styled("c", Style::default().fg(rgb(art::ramp(0.85)))),
            Span::styled(" clears the filters", Style::default().fg(th().chrome)),
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
    if s.total_tokens() > 0 {
        facts.push(format!(
            "{} tokens",
            crate::model::human_count(s.total_tokens())
        ));
    }
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
            Style::default()
                .fg(th().bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(fact_str, Style::default().fg(th().chrome)),
    ]));

    if !s.tags.is_empty() || !s.note.is_empty() {
        let mut sp = vec![Span::raw(" ".repeat(MARGIN))];
        for t in &s.tags {
            sp.push(Span::styled(
                format!("#{t} "),
                Style::default().fg(th().tag),
            ));
        }
        if !s.note.is_empty() {
            sp.push(Span::styled(
                fit(&s.note, width.saturating_sub(30)),
                Style::default().fg(th().fav).add_modifier(Modifier::ITALIC),
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
            Span::styled(fit(&sn, body), Style::default().fg(th().bright)),
        ]));
    }
    if show_cue && !s.last_prompt.is_empty() {
        body_lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            label("left off"),
            Span::styled(fit(&s.last_prompt, body), Style::default().fg(th().bright)),
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
                        th().fav
                    }),
                ),
                Span::styled(fit(&t.text, body), Style::default().fg(th().text)),
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
        Span::styled("≈ ", Style::default().fg(rgb(art::ramp(0.6)))),
        Span::styled(
            format!("{label} "),
            Style::default()
                .fg(rgb(art::ramp(0.85)))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            value,
            Style::default()
                .fg(th().bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::default().fg(rgb(art::ramp(1.0)))),
        Span::styled(format!("   {hint}"), Style::default().fg(th().chrome)),
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
            ("?", " help", Some(Action::Help)),
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
            spans.push(Span::styled(d, Style::default().fg(th().chrome)));
            x += kw + d.chars().count() as u16;
        }
    } else {
        spans.push(Span::styled(
            fit(
                &app.status,
                (area.width as usize).saturating_sub(pos.chars().count() + 6),
            ),
            Style::default().fg(th().fav),
        ));
    }

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    spans.push(Span::raw(" ".repeat(
        (area.width as usize).saturating_sub(used + pos.chars().count() + MARGIN),
    )));
    spans.push(Span::styled(pos, Style::default().fg(th().chrome)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One row of the help screen.
enum H {
    /// A section heading.
    Head(&'static str),
    /// A key or gesture, its vim alias, and what it does.
    Key(&'static str, &'static str, &'static str),
    /// A glyph from the interface, and what it means.
    Mark(&'static str, &'static str),
    /// Prose.
    Say(&'static str),
    Gap,
}

/// What the thing is and how you drive it, rather than a list of keys.
fn guide() -> Vec<H> {
    use H::*;
    vec![
        Say("Every Claude Code session you have ever run, from every folder, in one list."),
        Say("Claude keeps them all; it just cannot show you across directories. This can."),
        Gap,
        Head("finding one"),
        Key("/", "", "narrow the list by title, folder, branch, tag or id"),
        Key("F", "^f", "search inside the conversations themselves"),
        Key("m", "", "switch what that searches: what was said, a file it edited, a tool it ran, or everything"),
        Say("Content search is indexed and effectively instant. \"everything\" also reads tool"),
        Say("output, which is slower but misses nothing."),
        Gap,
        Head("opening one"),
        Key("enter", "", "resume it: cd to its folder and pick up where you left off"),
        Key("v", "", "read it first, without resuming or changing it"),
        Key("ctrl+t", "", "resume inside tmux, attaching if a session is already waiting"),
        Key("ctrl+n", "alt+enter", "resume in a new terminal window"),
        Key("space", "", "choose several, then enter reopens them all at once"),
        Say("It comes back with the model and the permission mode it started under."),
        Gap,
        Head("after a reboot"),
        Say("Which sessions are open is written down every time this runs. When a reboot"),
        Say("has taken them, the line under the title offers them back."),
        Key("r", "", "reopen them — a window each, running under tmux"),
        Key("x", "", "leave them closed"),
        Say("Nothing reopens on its own: that would be a pile of windows you never asked"),
        Say("for. Waved it away and changed your mind? mn --reopen still does it."),
        Gap,
        Head("keeping track"),
        Key("f", "", "favourite — favourites float to the top"),
        Key("t", "", "tag it, or tag everything you have selected"),
        Key("T", "", "show one tag only"),
        Key("N", "", "attach a private note"),
        Say("Tags are the useful axis: most sessions share a working directory, so the"),
        Say("folder column rarely tells them apart."),
        Gap,
        Head("reading the list"),
        Mark("▌", "how long ago — bright at the surface, fading as a session sinks"),
        Mark("❯", "where you are"),
        Mark("★", "favourite"),
        Mark("●", "running right now   ◌ probably running, matched only by folder"),
        Mark("◆", "picked, for acting on several at once"),
        Mark("⌁27", "has 27 subagents — → opens them, or click the count"),
        Mark("│", "a subagent, hanging off its parent"),
        Gap,
        Say("LEFT OFF is the last thing you said in that session — usually the fastest"),
        Say("way to recognise one. A folder in red no longer exists; resuming still"),
        Say("works, it just starts wherever you are."),
        Gap,
        Head("if something looks wrong"),
        Key("R", "f5", "reread the transcripts"),
        Key("c", "", "clear every filter and selection"),
        Say("The index is a cache and is rebuilt whenever it has to be; deleting"),
        Say("~/.claude/mnemosyne/index.db is always safe. Your favourites, tags and"),
        Say("notes live beside it in meta.json, which keeps three generations."),
    ]
}

fn keys() -> Vec<H> {
    use H::*;
    vec![
        Head("mouse"),
        Key("click", "", "select · click again to resume"),
        Key("right-click", "", "favourite it"),
        Key("wheel", "", "scroll"),
        Key("click a heading", "", "sort by that column"),
        Key("click ⌁n", "", "open that session's subagents"),
        Key("M", "", "mouse off, so the terminal can select text again"),
        Gap,
        Head("moving"),
        Key("↑ ↓", "k j", "move"),
        Key("pgup pgdn", "^u ^d", "jump ten"),
        Key("home end", "g G", "first · last"),
        Gap,
        Head("opening"),
        Key("enter", "", "resume here: cd to its folder and reattach"),
        Key("ctrl+n", "alt+enter", "resume in a new terminal window"),
        Key(
            "ctrl+t",
            "",
            "resume in tmux — attaches if one is already waiting",
        ),
        Key("v", "", "read the conversation without resuming it"),
        Key("space", "", "pick several, then enter reopens them all"),
        Gap,
        Head("searching"),
        Key("/", "", "filter titles, folders, branches, tags, ids"),
        Key("F", "^f", "search inside the conversations"),
        Key(
            "m",
            "",
            "search mode: content · file touched · tool used · everything",
        ),
        Gap,
        Head("marking"),
        Key("f", "", "favourite"),
        Key(
            "t",
            "",
            "tag — applies to the selection; -name removes, old>new renames",
        ),
        Key("T", "", "show one tag only"),
        Key("N", "", "private note"),
        Gap,
        Head("arranging"),
        Key(
            "s",
            "",
            "sort: recency · size · entries · duration · title · folder · tokens",
        ),
        Key("o", "", "group by directory"),
        Key("D", "", "date range"),
        Key("*", "", "favourites only"),
        Key("L", "", "running only"),
        Key("a", "", "reveal subagents"),
        Key("→ ←", "l h", "expand · collapse subagents"),
        Key("p", "", "preview rail"),
        Gap,
        Head("after a reboot"),
        Key(
            "r",
            "",
            "reopen what was open before it — window each, under tmux",
        ),
        Key("x", "", "leave them closed (mn --reopen still works)"),
        Say("Both only do anything while the offer is showing."),
        Gap,
        Head("other"),
        Key("R", "f5", "reindex"),
        Key("c", "", "clear everything"),
        Key("q esc", "^c", "quit"),
        Gap,
        Say("The middle column is a vim-style alias. You never need it."),
    ]
}

fn draw_help(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = match app.help_page {
        crate::app::HelpPage::Guide => guide(),
        crate::app::HelpPage::Keys => keys(),
    };

    let mark = art::mark_for(area.width.saturating_sub(6) as usize);
    let art_h = mark.as_ref().map(|m| m.height()).unwrap_or(0);
    // The wordmark is decoration; the text is the point. It only appears when
    // there is room for it on top of everything else.
    let show_art = mark.is_some() && area.height as usize >= rows.len() + art_h + 8;

    f.render_widget(Clear, area);
    let body_w = (area.width as usize).saturating_sub(MARGIN * 2);

    // Key column, narrowed when there is little room to spare.
    let kw: usize = if body_w > 70 { 16 } else { 12 };

    let mut lines: Vec<Line> = Vec::new();
    if show_art {
        let m = mark.as_ref().unwrap();
        let indent = (area.width as usize).saturating_sub(m.width) / 2;
        for (row, chars) in m.rows.iter().enumerate() {
            let mut sp: Vec<Span> = vec![Span::raw(" ".repeat(indent))];
            sp.extend(chars.iter().enumerate().map(|(x, ch)| {
                if *ch == ' ' {
                    return Span::raw(" ");
                }
                Span::styled(
                    ch.to_string(),
                    Style::default().fg(rgb(art::column_color(x, m.width, -99.0, row, art_h))),
                )
            }));
            lines.push(Line::from(sp));
        }
        lines.push(ripple_line(area.width as usize, indent.max(2), 1.7));
    }
    lines.push(Line::raw(""));

    for r in &rows {
        match r {
            H::Gap => lines.push(Line::raw("")),
            H::Head(t) => lines.push(Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled(
                    t.to_string(),
                    Style::default()
                        .fg(rgb(art::ramp(0.8)))
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            H::Say(t) => {
                for chunk in wrap_words(t, body_w.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(MARGIN)),
                        Span::styled(chunk, Style::default().fg(th().text)),
                    ]));
                }
            }
            H::Key(k, alt, desc) => {
                // Descriptions wrap under themselves rather than running off
                // the edge; on a narrow terminal the alias column goes first.
                let aw = if body_w > 58 { 11 } else { 0 };
                let head = kw + aw;
                for (n, chunk) in wrap_words(desc, body_w.saturating_sub(head + 2))
                    .into_iter()
                    .enumerate()
                {
                    let mut sp = vec![Span::raw(" ".repeat(MARGIN + 1))];
                    if n == 0 {
                        sp.push(Span::styled(
                            format!("{k:<kw$}"),
                            Style::default().fg(rgb(art::ramp(0.9))),
                        ));
                        if aw > 0 {
                            sp.push(Span::styled(
                                format!("{alt:<aw$}"),
                                Style::default().fg(th().chrome),
                            ));
                        }
                    } else {
                        sp.push(Span::raw(" ".repeat(head)));
                    }
                    sp.push(Span::styled(chunk, Style::default().fg(th().text)));
                    lines.push(Line::from(sp));
                }
            }
            H::Mark(g, desc) => {
                for (n, chunk) in wrap_words(desc, body_w.saturating_sub(kw + 2))
                    .into_iter()
                    .enumerate()
                {
                    let mut sp = vec![Span::raw(" ".repeat(MARGIN + 1))];
                    if n == 0 {
                        sp.push(Span::styled(
                            format!("{g:<kw$}"),
                            Style::default()
                                .fg(rgb(art::ramp(1.0)))
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        sp.push(Span::raw(" ".repeat(kw)));
                    }
                    sp.push(Span::styled(chunk, Style::default().fg(th().text)));
                    lines.push(Line::from(sp));
                }
            }
        }
    }

    let chrome = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    app.help_height = lines.len() as u16;
    app.help_rows = chrome[0].height;
    if app.help_scroll > app.help_height.saturating_sub(app.help_rows.max(1)) {
        app.help_scroll = app.help_height.saturating_sub(app.help_rows.max(1));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((app.help_scroll, 0))
            .block(Block::default().style(Style::default().bg(th().panel))),
        chrome[0],
    );

    // tabs, so the other page is discoverable rather than a secret
    let tab = |p: crate::app::HelpPage| {
        let on = p == app.help_page;
        Span::styled(
            format!(" {} ", p.title()),
            if on {
                Style::default()
                    .fg(Color::Black)
                    .bg(rgb(art::ramp(0.85)))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th().chrome)
            },
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            tab(crate::app::HelpPage::Guide),
            Span::raw(" "),
            tab(crate::app::HelpPage::Keys),
        ]))
        .block(Block::default().style(Style::default().bg(th().panel))),
        chrome[1],
    );

    let k = |t: &'static str| Span::styled(t, Style::default().fg(rgb(art::ramp(0.85))));
    let d = |t: &'static str| Span::styled(t, Style::default().fg(th().chrome));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            k("tab"),
            d(" other page   "),
            k("↑↓"),
            d(" scroll   "),
            k("esc"),
            d(" back"),
        ]))
        .block(Block::default().style(Style::default().bg(th().panel))),
        chrome[2],
    );
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::fixtures::app;
    use crate::app::{App, HelpPage, InputMode};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render once and hand back the screen as text rows.
    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// Sizes worth caring about, from silly to wide.
    fn sizes() -> Vec<(u16, u16)> {
        let mut v = Vec::new();
        for w in [
            20u16, 24, 32, 40, 44, 50, 55, 58, 64, 70, 80, 90, 100, 110, 120, 140, 150, 164, 178,
            200, 240,
        ] {
            for h in [4u16, 6, 8, 10, 14, 20, 26, 34, 50] {
                v.push((w, h));
            }
        }
        v
    }

    #[test]
    fn renders_at_every_size_without_panicking() {
        // Layout arithmetic is where this has broken before, always at a size
        // nobody tried by hand.
        let mut a = app();
        for (w, h) in sizes() {
            let _ = render(&mut a, w, h);
        }
    }

    #[test]
    fn the_wordmark_never_collides_with_the_counts() {
        // The header used to read "mnemosyne331 sessions" once the token
        // total made the right-hand side too long to fit.
        let mut a = app();
        for (w, h) in sizes() {
            if h < 4 {
                continue;
            }
            let rows = render(&mut a, w, h);
            let top = &rows[0];
            if let Some(i) = top.find("mnemosyne") {
                let after = &top[i + "mnemosyne".len()..];
                assert!(
                    after.is_empty() || after.starts_with(' '),
                    "{w}x{h}: header ran together: {top:?}"
                );
            }
        }
    }

    #[test]
    fn every_mode_renders_at_every_size() {
        for (w, h) in sizes() {
            let mut a = app();
            // grouped, filtered, searched, selected — all at once
            a.do_action(crate::app::Action::GroupByDir);
            a.fuzzy = "e".into();
            a.deep = "thing".into();
            a.deep_hits = Some(Default::default());
            a.selected.insert("/p/aaaaaaaa-1.jsonl".into());
            a.rebuild();
            let _ = render(&mut a, w, h);

            for mode in [
                InputMode::Fuzzy,
                InputMode::Deep,
                InputMode::TagAdd,
                InputMode::TagFilter,
                InputMode::Note,
                InputMode::Help,
            ] {
                a.input_mode = mode;
                a.input = "some typed text".into();
                let _ = render(&mut a, w, h);
            }
            a.input_mode = InputMode::Help;
            a.help_page = HelpPage::Keys;
            let _ = render(&mut a, w, h);
            a.input_mode = InputMode::Normal;

            // and with the reopen offer up, which draws into the one blank
            // line in the layout
            offer(&mut a, 3);
            let _ = render(&mut a, w, h);
        }
    }

    /// An outstanding offer of `n` sessions to put back.
    fn offer(a: &mut App, n: usize) {
        a.reopen = (0..n)
            .map(|i| crate::workspace::Entry {
                id: format!("session-{i}"),
                cwd: "/home/u".into(),
                title: format!("some work {i}"),
                ..Default::default()
            })
            .collect();
    }

    #[test]
    fn the_offer_says_what_it_is_offering() {
        let mut a = app();
        offer(&mut a, 3);
        let rows = render(&mut a, 120, 30);
        let line = rows
            .iter()
            .find(|r| r.contains("before the reboot"))
            .unwrap_or_else(|| panic!("the offer never appeared: {rows:#?}"));
        assert!(line.contains("3 sessions were"), "no count: {line:?}");
        assert!(line.contains("reopen"), "no way to accept: {line:?}");
        assert!(line.contains("not now"), "no way to decline: {line:?}");
    }

    #[test]
    fn a_single_session_is_named_rather_than_counted() {
        let mut a = app();
        offer(&mut a, 1);
        let rows = render(&mut a, 120, 30);
        let line = rows
            .iter()
            .find(|r| r.contains("before the reboot"))
            .unwrap();
        assert!(line.contains("1 session was"), "{line:?}");
        assert!(
            line.contains("some work 0"),
            "one session should be named, not just counted: {line:?}"
        );
    }

    #[test]
    fn nothing_is_drawn_there_when_there_is_no_offer() {
        let mut a = app();
        let rows = render(&mut a, 120, 30);
        assert!(!rows.iter().any(|r| r.contains("before the reboot")));
        assert!(
            rows[1].trim().is_empty(),
            "the line under the wordmark should stay empty: {:?}",
            rows[1]
        );
    }

    #[test]
    fn the_offer_can_be_clicked() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        // Where the words land depends on how the header was drawn, so the
        // coordinates come from the render rather than from a guess.
        let mut a = app();
        offer(&mut a, 2);
        let _ = render(&mut a, 120, 30);
        let y = a
            .hits
            .banner_y
            .expect("the offer registered no click target");
        let (x0, _, _) = a.hits.banner[0];

        a.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x0,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let Some(crate::app::Outcome::Resume { targets, .. }) = &a.outcome else {
            panic!("clicking reopen did nothing");
        };
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn declining_can_be_clicked_too() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut a = app();
        offer(&mut a, 2);
        let _ = render(&mut a, 120, 30);
        let y = a.hits.banner_y.unwrap();
        let (x0, _, _) = a.hits.banner[1];

        a.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x0,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(a.reopen.is_empty(), "the offer stayed up");
        assert!(a.outcome.is_none(), "declining opened something");
    }

    #[test]
    fn a_click_beside_the_offer_does_not_open_anything() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut a = app();
        offer(&mut a, 2);
        let _ = render(&mut a, 120, 30);
        let y = a.hits.banner_y.unwrap();
        a.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(a.outcome.is_none(), "the margin acted as a button");
        assert_eq!(a.reopen.len(), 2);
    }

    #[test]
    fn the_viewer_renders_and_scrolls_within_bounds() {
        let mut a = app();
        a.viewer = Some((
            (0..60)
                .map(|i| crate::preview::Turn {
                    role: if i % 2 == 0 { "you" } else { "claude" },
                    text: format!("turn {i} ").repeat(20),
                })
                .collect(),
            true,
        ));
        a.input_mode = InputMode::Viewer;
        for (w, h) in sizes() {
            if h < 6 {
                continue;
            }
            let _ = render(&mut a, w, h);
            assert!(
                a.viewer_scroll <= a.viewer_height,
                "{w}x{h}: scrolled past the end"
            );
        }
    }

    #[test]
    fn help_scroll_is_clamped_to_its_content() {
        let mut a = app();
        a.input_mode = InputMode::Help;
        a.help_scroll = u16::MAX;
        for (w, h) in sizes() {
            let _ = render(&mut a, w, h);
            assert!(
                a.help_scroll <= a.help_height,
                "{w}x{h}: help scrolled into nothing"
            );
        }
    }

    #[test]
    fn an_empty_list_still_draws() {
        let mut a = app();
        a.fuzzy = "no-such-session-anywhere".into();
        a.rebuild();
        assert_eq!(a.item_count(), 0);
        for (w, h) in sizes() {
            let rows = render(&mut a, w, h);
            assert_eq!(rows.len(), h as usize);
        }
    }

    #[test]
    fn a_wide_terminal_shows_the_columns_it_promises() {
        let mut a = app();
        let rows = render(&mut a, 178, 24);
        let head = rows
            .iter()
            .find(|r| r.contains("AGE"))
            .expect("column head");
        for col in [
            "AGE", "FOLDER", "TITLE", "LEFT OFF", "MODEL", "MSGS", "TOKENS", "TAGS",
        ] {
            assert!(head.contains(col), "missing {col} in {head:?}");
        }
    }

    #[test]
    fn columns_are_dropped_in_order_as_it_narrows() {
        let mut a = app();
        let has = |a: &mut App, w: u16, col: &str| {
            render(a, w, 20)
                .iter()
                .any(|r| r.contains("AGE") && r.contains(col))
        };
        assert!(has(&mut a, 178, "TOKENS"));
        assert!(!has(&mut a, 120, "TOKENS"), "tokens go before tags");
        assert!(has(&mut a, 150, "TAGS"));
        assert!(!has(&mut a, 100, "TAGS"));
        assert!(has(&mut a, 120, "MODEL"));
        assert!(!has(&mut a, 90, "MODEL"));
        // the ones that always survive
        for w in [60u16, 80, 100, 140, 200] {
            assert!(has(&mut a, w, "TITLE"), "TITLE missing at {w}");
            assert!(has(&mut a, w, "AGE"), "AGE missing at {w}");
        }
    }

    #[test]
    fn wrapping_never_loses_or_splits_a_word() {
        for width in [8usize, 12, 20, 40, 80] {
            let text = "the quick brown fox jumps over the lazy dog";
            let out = wrap_words(text, width);
            for line in &out {
                assert!(line.chars().count() <= width, "{width}: {line:?}");
            }
            assert_eq!(
                out.join(" ").split_whitespace().collect::<Vec<_>>(),
                text.split_whitespace().collect::<Vec<_>>(),
                "words changed at width {width}"
            );
        }
    }

    #[test]
    fn wrapping_hard_breaks_something_longer_than_a_line() {
        let out = wrap_words("supercalifragilistic", 6);
        assert!(out.len() > 1);
        assert!(out.iter().all(|l| l.chars().count() <= 6));
        assert_eq!(out.concat(), "supercalifragilistic");
    }

    #[test]
    fn wrapping_copes_with_nothing() {
        assert_eq!(wrap_words("", 10), vec![String::new()]);
        assert_eq!(wrap_words("   ", 10), vec![String::new()]);
    }

    #[test]
    fn shared_prefix_compares_by_character() {
        assert!(same_prefix("hello there", "hello there", 5));
        assert!(!same_prefix("hello", "goodbye", 3));
        // multi-byte input must not panic or mis-slice
        assert!(same_prefix("héllo wörld ≈≈", "héllo wörld ≈≈", 4));
        assert!(!same_prefix("héllo", "hello", 3));
    }

    #[test]
    fn column_widths_always_fit_the_terminal() {
        // 44 is the narrowest width the table still claims to be a table;
        // below that the columns are shed and only the title remains.
        for w in 44usize..=300 {
            let c = Cols::new(w);
            let gap = if c.folder > 0 { 1 } else { 0 };
            let used = PREFIX
                + 4
                + 2
                + c.folder
                + gap
                + c.sub
                + c.title
                + if c.preview > 0 { c.preview + 2 } else { 0 }
                + c.model
                + c.msgs
                + c.tokens
                + c.tags;
            assert!(used <= w, "width {w}: columns want {used}");
        }
    }
}

#[cfg(test)]
mod glyph_tests {
    /// Every non-ASCII character the interface is allowed to draw.
    ///
    /// Each one has been checked against the fonts a plain `monospace`
    /// actually resolves to, and against fontconfig's fallback chain. The
    /// wordmark prefix used to be U+2307, which nothing in that chain
    /// carries — it rendered as a box and there was no way to tell from a
    /// text capture, because the codepoint survives whether or not the font
    /// can draw it.
    const ALLOWED: &str = "▌❯★●◌◆⌁│└≈~-─—…·“”↵→←↑↓█░▏ ";

    fn ui_source_glyphs() -> Vec<char> {
        let src = include_str!("ui.rs");
        // stop before this module so the allowlist does not test itself
        let body = src.split("mod glyph_tests").next().unwrap();
        let mut out: Vec<char> = body
            .chars()
            .filter(|c| !c.is_ascii() && !c.is_alphabetic())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    #[test]
    fn the_interface_only_uses_glyphs_fonts_actually_have() {
        let unknown: Vec<char> = ui_source_glyphs()
            .into_iter()
            .filter(|c| !ALLOWED.contains(*c))
            .collect();
        assert!(
            unknown.is_empty(),
            "unvetted glyphs {unknown:?} — check them against `fc-match -s monospace` \
             before using them, or they will render as boxes"
        );
    }

    #[test]
    fn the_retired_glyph_is_gone() {
        assert!(
            !include_str!("ui.rs").contains('\u{2307}'),
            "U+2307 is back"
        );
    }
}

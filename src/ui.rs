//! Rendering.
//!
//! The look has one idea behind it: **the gold leaf**. The Orphic tablets
//! were leaves of gold that told the dead to drink from Mnemosyne's spring
//! and remember, so the interface is struck from one:
//!
//! * An edge of gold runs down the left, coloured on the ramp by how old each
//!   session is — fresh leaf for today, tarnished bronze for last year. Age
//!   becomes something you see rather than read.
//! * The name in the header is a chip of the same gold the opening screen
//!   cuts it into.
//! * Beside the gold, three colours that each mean one thing: lapis for your
//!   own marks, cypress for anything alive, cinnabar for anything lost.
//! * Rules are dotted and fade out rather than cross the screen. There are
//!   no borders; structure comes from alignment.
//!
//! Everything clickable records its screen span into `app.hits` as it draws, so
//! the mouse handler hit-tests against what was actually rendered.

use crate::app::{Action, App, InputMode, Row};
use crate::art;
use crate::model::{
    compact_count, fit, human_dur, human_size, pad_fit, reltime, short_cwd, width, Sort,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

/// Resolved once from the config, so the palette can follow a desktop theme
/// without a rebuild. Missing keys keep the values the tool shipped with.
///
/// The defaults are the gold leaf's: gold for the record and its age, and
/// three colours beside it that each mean one thing. Lapis is your own ink
/// -- tags, notes, what you said. Cypress is alive: running now, or a wsx
/// workspace still there to go to. Cinnabar, the red ochre painted into
/// Greek inscriptions, is what has been lost. Everything else is stone.
struct Theme {
    accent: Color,
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
    THEME.get_or_init(|| Theme::from_config(crate::config::get()))
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
            accent: pick(&c.accent, Color::Rgb(134, 176, 142)), // cypress
            chrome: pick(&c.chrome, Color::Rgb(125, 114, 99)),  // worn stone
            fav: pick(&c.favorite, Color::Rgb(245, 197, 66)),   // a gold star
            live: pick(&c.live, Color::Rgb(134, 176, 142)),     // cypress
            tag: pick(&c.tag, Color::Rgb(138, 162, 230)),       // lapis
            text: pick(&c.text, Color::Rgb(203, 194, 176)),     // limestone
            bright: pick(&c.bright, Color::Rgb(245, 239, 227)), // marble
            gone: pick(&c.gone, Color::Rgb(208, 101, 75)),      // cinnabar
            band: pick(&c.band, Color::Rgb(43, 34, 21)),
            panel: pick(&c.panel, Color::Rgb(18, 14, 10)),
        }
    }
}

const MARGIN: usize = 2;
/// margin + gutter + gap + cursor + gap + marker + gap
const PREFIX: usize = MARGIN + 1 + 1 + 1 + 1 + 1 + 1;

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// How fresh a session's gold is, 1.0 = just now, 0.0 = long tarnished.
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
    /// `wsx` says whether any row carries the wsx mark, which gets a few
    /// cells of its own rather than taking them from the name after it.
    fn new(width: usize, wsx: bool) -> Cols {
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
        let folder = if wsx && folder > 0 {
            folder + crate::model::width(crate::wsx::MARK)
        } else {
            folder
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
fn wrap_words(text: &str, wrap_at: usize) -> Vec<String> {
    // Honour the width asked for. Clamping it upward produced lines wider
    // than the caller had room for, which is the one thing wrapping is for.
    let wrap_at = wrap_at.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut len = 0usize;
    for word in text.split_whitespace() {
        let wl = width(word);
        if wl > wrap_at {
            if len > 0 {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            // A character goes on the next line if it would not fit on
            // this one. Checked after adding it instead, a two-column
            // character one column short of the edge went one past it.
            let mut chunk = String::new();
            let mut used = 0usize;
            for ch in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + cw > wrap_at && !chunk.is_empty() {
                    out.push(std::mem::take(&mut chunk));
                    used = 0;
                }
                chunk.push(ch);
                used += cw;
            }
            if !chunk.is_empty() {
                line = chunk;
                len = width(&line);
            }
            continue;
        }
        if len > 0 && len + 1 + wl > wrap_at {
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
            Constraint::Min(3),                                 // the list
            Constraint::Length(rail),                           // rail
            Constraint::Length(if show_input { 2 } else { 1 }), // input / air
            Constraint::Length(1),                              // footer
        ])
        .split(area);

    let cols = Cols::new(area.width as usize, app.has_wsx());

    draw_wordmark(f, app, rows[0]);
    // The blank line under the wordmark is where the reopen offer goes, so
    // an offer never pushes the list around: it fills air that was there
    // anyway, and the rows below it do not move.
    app.hits.banner.clear();
    app.hits.banner_y = None;
    // Only where there is a row for it. On a terminal a few lines high the
    // layout gives some rows no height at all, and one recorded there sat
    // on top of another: with the offer up, clicking a column heading hit
    // the Reopen button nobody could see, and opened a window for each.
    if !app.reopen.is_empty() && rows[1].height > 0 {
        draw_reopen(f, app, rows[1]);
    }
    draw_colheads(f, app, &cols, rows[2]);
    draw_list(f, app, &cols, rows[3]);
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
        let (label, colour) = speaker(t.role);
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
    // Scrolling is counted in u16, so no more lines than that can be
    // reached. Counted as they were, the height wrapped: 400 turns at
    // sixteen columns is 93,600 lines, recorded as 28,064, and the viewer
    // opened part way and could never reach the end. It opens at the end,
    // so the newest are the ones to keep.
    if lines.len() > u16::MAX as usize {
        lines.drain(..lines.len() - u16::MAX as usize);
    }
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
                Span::styled("▸ ", Style::default().fg(rgb(art::ramp(0.6)))),
                Span::styled(
                    fit(&title, (area.width as usize).saturating_sub(12)),
                    Style::default()
                        .fg(th().bright)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            rule_line(area.width as usize, MARGIN),
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
            d(if app.enter_jumps().is_some() {
                " switch to it in wsx   "
            } else {
                " resume this one   "
            }),
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
/// One word in the offer: its key, its label, and what clicking it does —
/// `None` for a hint that is there to be read rather than pressed.
type Button = (&'static str, &'static str, Option<Action>);

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

    // What to say, wordiest first. A title can be 160 characters long, which
    // used to run the whole line off the screen and take both buttons with
    // it -- the offer was still there, and there was no longer any visible
    // way to accept it. The buttons are what must survive, so the wording
    // gives way to them rather than the other way round.
    let mut says = Vec::new();
    if n == 1 && !first.is_empty() {
        says.push(format!("{what} open before the reboot: {first}"));
    }
    says.push(format!("{what} open before the reboot"));
    says.push(format!("{n} open before the reboot"));
    says.push(format!("reopen {n}?"));

    // Likewise the buttons themselves: words if they fit, letters if not.
    // The hint about space is how you find out the offer can be narrowed at
    // all; the marked rows below are the other half of that. It is the first
    // thing dropped when the line gets tight.
    let wordy: &[Button] = &[
        ("r", " reopen   ", Some(Action::Reopen)),
        ("x", " not now   ", Some(Action::DismissReopen)),
        ("space", " drops one", None),
    ];
    let plain: &[Button] = &[
        ("r", " reopen   ", Some(Action::Reopen)),
        ("x", " not now", Some(Action::DismissReopen)),
    ];
    let terse: &[Button] = &[
        ("r", " reopen ", Some(Action::Reopen)),
        ("x", " no", Some(Action::DismissReopen)),
    ];

    let room = area.width as usize;
    let fixed = MARGIN + 2 + 3; // margin, marker, the gap before the buttons
    let width_of = |b: &[Button]| -> usize { b.iter().map(|(k, l, _)| width(k) + width(l)).sum() };

    let mut chosen: Option<(String, &[Button])> = None;
    for buttons in [wordy, plain, terse] {
        let budget = room.saturating_sub(fixed + width_of(buttons));
        if width(&says[0]) <= budget {
            chosen = Some((says[0].clone(), buttons));
            break;
        }
        // The wordiest line is the one carrying the title, and half a title
        // still tells you which session this is. Cut it rather than drop it,
        // so long as enough of it survives to be worth reading. This has to
        // be tried before the shorter wordings, or the first one that merely
        // fits always wins and the title is never shown at all.
        if says.len() > 1 && budget > width(&says[1]) + 8 {
            chosen = Some((crate::model::fit(&says[0], budget), buttons));
            break;
        }
        if let Some(say) = says.iter().find(|s| width(s) <= budget) {
            chosen = Some((say.clone(), buttons));
            break;
        }
    }

    // Narrower than even "reopen 3?" plus two letters: say the least that
    // still leaves something to press.
    let (head, buttons) = chosen.unwrap_or_else(|| {
        let budget = room.saturating_sub(fixed + width_of(terse));
        (crate::model::fit(says.last().unwrap(), budget), terse)
    });

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
    let mut x = area.x + MARGIN as u16 + 2 + width(&head) as u16 + 3;

    app.hits.banner_y = Some(area.y);
    for (k, label, action) in buttons {
        let w = (width(k) + width(label)) as u16;
        // The whole phrase is the target, not just the letter: a one-column
        // click target is not a click target. Anything that would land past
        // the edge of the screen is not one either, so it is not registered.
        if let Some(a) = action {
            if x + w <= area.x + area.width {
                app.hits.banner.push((x, x + w.saturating_sub(1), *a));
            }
        }
        spans.push(Span::styled(*k, key));
        spans.push(Span::styled(*label, dim));
        x += w;
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The tablet in miniature: the name cut into a chip of gold, lit from the
/// left the way the big one is.
fn chip() -> Vec<Span<'static>> {
    let letters: Vec<char> = art::GREEK.chars().collect();
    let n = letters.len() + 1;
    let gold = |i: usize| rgb(art::ramp(0.86 - 0.22 * i as f64 / n as f64));
    let mut spans = vec![Span::styled("▐", Style::default().fg(gold(0)))];
    for (i, ch) in letters.iter().enumerate() {
        spans.push(Span::styled(
            ch.to_string(),
            Style::default()
                .fg(rgb(art::ramp(0.2)))
                .bg(gold(i + 1))
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("▌", Style::default().fg(gold(n))));
    spans
}

/// Who said a turn, and in what: your words in your own ink, Claude's in
/// the gold of the record.
fn speaker(role: &str) -> (&'static str, Color) {
    if role == "you" {
        ("you", th().tag)
    } else {
        ("claude", rgb(art::ramp(0.72)))
    }
}

/// The name, and on the right what the list is showing.
fn draw_wordmark(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ".repeat(MARGIN))];
    spans.extend(chip());

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
    let favs = app.favourites_shown();
    if favs > 0 {
        segs.push((3, plain(format!("★{favs}"), th().fav)));
    }
    if app.indexing {
        // The list is showing cached rows while the rescan catches up. Say
        // so, quietly, rather than letting numbers change with no reason.
        segs.push((1, plain("indexing…".to_string(), th().chrome)));
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

    let lw: usize = spans.iter().map(|s| width(&s.content)).sum();
    let width = area.width as usize;
    let seg_len = |v: &Vec<Span>| {
        v.iter()
            .map(|s| crate::model::width(&s.content))
            .sum::<usize>()
    };
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

    let lw: usize = spans.iter().map(|s| crate::model::width(&s.content)).sum();
    let rw: usize = right.iter().map(|s| crate::model::width(&s.content)).sum();
    spans.push(Span::raw(
        " ".repeat((area.width as usize).saturating_sub(lw + rw + MARGIN)),
    ));
    spans.extend(right);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Column headings, and the clickable spans that sort by them.
fn draw_colheads(f: &mut Frame, app: &mut App, c: &Cols, area: Rect) {
    app.hits.columns.clear();
    // no row to click on (u16::MAX is never one)
    app.hits.colhead_y = if area.height > 0 { area.y } else { u16::MAX };
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
        // Where the word is, not where its padding starts: a right-aligned
        // heading's span began in the blanks before it, and MSGS and TOKENS
        // could not be clicked on their last letters.
        if let (Some(s), Some(lw)) = (sort, width(text.trim()).checked_sub(1)) {
            let x0 = *x + (width(&text) - width(text.trim_start())) as u16;
            hits.push((x0, x0 + lw as u16, s));
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

fn draw_list(f: &mut Frame, app: &mut App, c: &Cols, area: Rect) {
    let width = area.width as usize;
    let cursor = app.cursor;

    // Where the subagent cell sits, so a click on ⌁ can expand it: after
    // the age, and after the folder and its space when there is a folder.
    // The span is inclusive, so it ends on the cell's last column -- one
    // more was the title's first letter, which toggled subagents instead
    // of selecting. With no cell drawn there is nothing to click.
    let sub_x = area.x + (PREFIX + 6 + if c.folder > 0 { c.folder + 1 } else { 0 }) as u16;
    app.sub_span = match c.sub {
        0 => (1, 0),
        n => (sub_x, sub_x + n as u16 - 1),
    };

    // Only the rows on screen are built. Every one of them was, every frame
    // -- two thousand rows, clipped, measured and styled to show forty --
    // which was nearly all of what drawing a frame cost.
    let height = area.height as usize;
    let first = visible_from(app.list_state.offset(), cursor, app.view.len(), height);
    let items: Vec<ListItem> = app
        .view
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(row_i, r)| match r {
            // a dotted rule, with the band's name set into it
            Row::Divider(label) => {
                let text = format!("{label} ");
                let used = MARGIN + 2 + crate::model::width(&text);
                let mut sp = vec![
                    Span::raw(" ".repeat(MARGIN)),
                    Span::styled("┄ ", Style::default().fg(rgb(rule_colour(0.0)))),
                    Span::styled(text, Style::default().fg(rgb(art::ramp(0.62)))),
                ];
                sp.extend(rule(width.saturating_sub(used), 0));
                ListItem::new(Line::from(sp))
            }
            Row::Header(dir, n) => {
                // A folder heading is a path, so it never starts with the
                // mark; a workspace's always does.
                let (mark, name) = match dir.strip_prefix(crate::wsx::MARK) {
                    Some(name) => (crate::wsx::MARK, name),
                    None => ("", dir.as_str()),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(" ".repeat(MARGIN)),
                    Span::styled("▌ ", Style::default().fg(rgb(art::ramp(0.7)))),
                    Span::styled(mark, Style::default().fg(th().chrome)),
                    Span::styled(
                        format!("{name}  "),
                        Style::default()
                            .fg(rgb(art::ramp(0.8)))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{n}"), Style::default().fg(th().chrome)),
                ]))
            }
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
                } else if app.is_offered(&s.id) {
                    // In the offer above: hollow, because it is not running
                    // now, and in the offer's own colour so it cannot be
                    // read as the "probably running" marker it shares a
                    // glyph with. Nothing can be both -- the offer skips
                    // anything already up.
                    (
                        "◌",
                        Style::default()
                            .fg(rgb(art::ramp(0.95)))
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
                        format!("{} ", pad_fit("└ subagent", c.folder)),
                        Style::default().fg(th().chrome),
                    ));
                } else {
                    // The mark first, dim, and the name in what is left.
                    let mark = if s.wsx.is_some() && !s.cwd.is_empty() {
                        crate::wsx::MARK
                    } else {
                        ""
                    };
                    let room = c.folder.saturating_sub(crate::model::width(mark));
                    let folder = if s.cwd.is_empty() {
                        "—".to_string()
                    } else if let Some(w) = &s.wsx {
                        w.fit(room)
                    } else {
                        short_cwd(&s.cwd)
                    };
                    // A directory that no longer exists is worth seeing here
                    // rather than discovering at resume time. A live wsx
                    // workspace is lit instead: it is somewhere to go.
                    //
                    // An archived one is dimmed rather than drawn as gone:
                    // its worktree went on purpose, and there is somewhere
                    // for it to resume that is not wherever you are.
                    let status = s.wsx.as_ref().map(|w| &w.status);
                    let fstyle = if matches!(status, Some(crate::wsx::Status::Live { .. })) {
                        Style::default().fg(th().accent)
                    } else if matches!(status, Some(crate::wsx::Status::Archived { .. })) {
                        Style::default().fg(th().chrome)
                    } else if s.cwd_missing {
                        Style::default().fg(th().gone)
                    } else {
                        Style::default().fg(rgb(art::ramp(0.45 + d * 0.2)))
                    };
                    sp.push(Span::styled(mark, Style::default().fg(th().chrome)));
                    sp.push(Span::styled(format!("{} ", pad_fit(&folder, room)), fstyle));
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
                    pad_fit(s.title(), c.title),
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
                    // Padded by columns, not characters: a prompt in CJK
                    // came out half as wide as its column, and every
                    // column after it moved left to fill the gap.
                    sp.push(Span::styled(
                        pad_fit(&fit(cue, c.preview.saturating_sub(2)), c.preview),
                        Style::default().fg(th().chrome),
                    ));
                }
                if c.model > 0 {
                    sp.push(Span::styled(
                        pad_fit(s.model_short(), c.model - 1) + " ",
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

    let mut window = ratatui::widgets::ListState::default().with_selected(
        app.cursor
            .checked_sub(first)
            .filter(|_| !app.view.is_empty()),
    );
    f.render_stateful_widget(list, area, &mut window);
    app.list_state = ratatui::widgets::ListState::default()
        .with_offset(first)
        .with_selected(Some(app.cursor));

    app.hits.list = area;
    app.hits.list_offset = first;
}

/// The first row on screen, scrolled the way ratatui's list scrolls its
/// one-line rows: from where it was, just far enough to keep the selected
/// row in view.
fn visible_from(offset: usize, selected: usize, len: usize, height: usize) -> usize {
    if len == 0 || height == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    let mut first = offset.min(len - 1);
    if selected >= first + height {
        first = selected + 1 - height;
    }
    first.min(selected)
}

/// How far along a rule has faded, 0 at its start, 1 where it is gone.
fn rule_colour(fade: f64) -> (u8, u8, u8) {
    art::sink(art::ramp(0.42), 0.35 + fade * 0.6)
}

/// A dotted rule that fades out toward the right, so it separates without
/// running a hard line across the whole screen.
fn rule(width: usize, indent: usize) -> Vec<Span<'static>> {
    let len = width.saturating_sub(indent * 2);
    let span = (len as f64 * 0.6).max(24.0);
    let mut sp = vec![Span::raw(" ".repeat(indent))];
    for i in 0..len {
        let fade = i as f64 / span;
        if fade >= 1.0 {
            break;
        }
        sp.push(Span::styled(
            "┄",
            Style::default().fg(rgb(rule_colour(fade))),
        ));
    }
    sp
}

fn rule_line(width: usize, indent: usize) -> Line<'static> {
    Line::from(rule(width, indent))
}

fn draw_rail(f: &mut Frame, app: &mut App, area: Rect, show_cue: bool) {
    let turns = app.preview(8);
    let snippet = app.deep_snippet();
    let width = area.width as usize;

    let Some(s) = app.current().cloned() else {
        let mut lines = vec![rule_line(width, MARGIN), Line::raw("")];
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled("nothing matches — ", Style::default().fg(th().chrome)),
            Span::styled("c", Style::default().fg(rgb(art::ramp(0.85)))),
            Span::styled(" clears the filters", Style::default().fg(th().chrome)),
        ]));
        f.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    };

    let mut lines: Vec<Line> = vec![rule_line(width, MARGIN)];

    // What wsx says about a workspace is the word on it. A live one's
    // enter goes through wsx rather than into the folder, and an archived
    // one's says where it goes instead.
    let known = s
        .wsx
        .as_ref()
        .is_some_and(|w| !matches!(w.status, crate::wsx::Status::Unknown));
    let mut facts: Vec<String> = vec![if s.cwd_missing && !known {
        format!("{} (gone)", s.folder())
    } else {
        s.folder()
    }];
    if let Some(w) = &s.wsx {
        match &w.status {
            crate::wsx::Status::Live { .. } => facts.push("live".into()),
            crate::wsx::Status::Archived { checkout } => {
                facts.push("archived".into());
                if s.cwd_missing {
                    facts.push(match checkout {
                        Some(c) => format!("resumes in {}", short_cwd(c)),
                        None => "resumes where you are".into(),
                    });
                }
            }
            crate::wsx::Status::Unknown => {}
        }
    }
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
            "{}{} {pid}",
            if s.live_exact {
                "running"
            } else {
                "likely running"
            },
            if s.live_in_wsx { " in wsx" } else { "" }
        ));
    }
    if s.has_tmux {
        facts.push(format!("tmux {}", crate::live::tmux_name(&s.id)));
    }
    let fact_str = facts.join(" · ");
    let title = fit(
        s.title(),
        width.saturating_sub(crate::model::width(&fact_str) + MARGIN * 3),
    );
    let gap = width
        .saturating_sub(crate::model::width(&title) + crate::model::width(&fact_str) + MARGIN * 2)
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
                Style::default().fg(th().tag).add_modifier(Modifier::ITALIC),
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
        let (who, colour) = speaker(t.role);
        body_lines.insert(
            body_lines.len(),
            Line::from(vec![
                Span::raw(" ".repeat(MARGIN)),
                Span::styled(format!("{who:<10}"), Style::default().fg(colour)),
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
        InputMode::TmuxName => (
            "name this tmux session",
            app.input.clone(),
            format!(
                "enter · leave it empty for {}",
                app.current()
                    .map(|s| crate::live::tmux_name(&s.id))
                    .unwrap_or_else(|| "mn-…".into())
            ),
        ),
        _ => ("", String::new(), String::new()),
    };
    let line = Line::from(vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled("▸ ", Style::default().fg(rgb(art::ramp(0.6)))),
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
    app.hits.footer_y = if area.height > 0 { area.y } else { u16::MAX };
    let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(MARGIN))];
    let mut x = area.x + MARGIN as u16;

    if app.status.is_empty() {
        let budget = area.width as usize;
        // On a workspace wsx is running, enter goes there instead, and the
        // hint says so before you press it rather than after.
        let enter = if app.enter_jumps().is_some() {
            " switch to wsx   "
        } else {
            " resume   "
        };
        let mut hints: Vec<(&'static str, &'static str, Option<Action>)> = vec![
            ("↑↓", " move   ", None),
            ("↵", enter, Some(Action::Resume)),
            ("/", " filter   ", Some(Action::Filter)),
            ("F", " search   ", Some(Action::Search)),
            ("^t", " tmux   ", Some(Action::Tmux)),
            ("f", " ★   ", Some(Action::Favorite)),
            ("t", " tag   ", Some(Action::Tag)),
            ("s", " sort   ", Some(Action::CycleSort)),
            ("?", " help", Some(Action::Help)),
        ];
        loop {
            let w: usize = hints.iter().map(|(a, b, _)| width(a) + width(b)).sum();
            if w + width(&pos) + 6 <= budget || hints.len() <= 2 {
                break;
            }
            hints.remove(hints.len() - 2);
        }
        for (k, d, act) in hints {
            let kw = width(k) as u16;
            if let Some(a) = act {
                app.hits
                    .footer
                    .push((x, x + kw + width(d.trim_end()) as u16, a));
            }
            spans.push(Span::styled(k, Style::default().fg(rgb(art::ramp(0.85)))));
            spans.push(Span::styled(d, Style::default().fg(th().chrome)));
            x += kw + width(d) as u16;
        }
    } else {
        spans.push(Span::styled(
            fit(
                &app.status,
                (area.width as usize).saturating_sub(width(&pos) + 6),
            ),
            Style::default().fg(th().fav),
        ));
    }

    let used: usize = spans.iter().map(|s| width(&s.content)).sum();
    spans.push(Span::raw(" ".repeat(
        (area.width as usize).saturating_sub(used + width(&pos) + MARGIN),
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
        Key("W", "ctrl+shift+t", "both: a new terminal window with tmux inside it"),
        Say("The last one is the durable option — close the window and the session keeps"),
        Say("running. Either tmux route asks what to call the session first; leave it"),
        Say("empty for the generated name."),
        Key("space", "", "choose several, then enter reopens them all at once"),
        Say("It comes back with the model and the permission mode it started under."),
        Say("A live wsx workspace is already running in wsx, so enter switches to it there"),
        Say("instead, and the browser stays up. ctrl+n opens it in a window anyway."),
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
        Key("T", "", "show one tag only — wsx/<repo> is one wsx repo's workspaces"),
        Key("N", "", "attach a private note"),
        Say("Tags are the useful axis: most sessions share a working directory, so the"),
        Say("folder column rarely tells them apart."),
        Gap,
        Head("reading the list"),
        Mark("▌", "how long ago — fresh gold today, tarnishing as a session ages"),
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
        Say("A wsx workspace is marked wsx and named repo/workspace: lit while wsx lists"),
        Say("it, dimmed once it is archived, when it resumes in the repo's checkout instead."),
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
        Key("click", "", "select · double-click to resume"),
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
        Say("On a live wsx workspace, enter switches to it in wsx instead."),
        Key("ctrl+n", "alt+enter", "resume in a new terminal window"),
        Key(
            "ctrl+t",
            "",
            "resume in tmux — attaches if one is already waiting",
        ),
        Key(
            "W",
            "ctrl+shift+t",
            "new terminal window with tmux inside it — closing the window leaves it running",
        ),
        Say("ctrl+shift+t only arrives as its own key where the terminal says so; inside"),
        Say("tmux that needs `set -s extended-keys on`. W always works."),
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
        Key("T", "", "show one tag only · wsx/<repo> for a wsx repo"),
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

    let tablet = art::tablet_for(area.width.saturating_sub(6) as usize);
    let art_h = tablet.as_ref().map(|t| t.height()).unwrap_or(0);
    // The tablet is decoration; the text is the point. It only appears when
    // there is room for it on top of everything else.
    let show_art = tablet.is_some() && area.height as usize >= rows.len() + art_h + 8;

    f.render_widget(Clear, area);
    let body_w = (area.width as usize).saturating_sub(MARGIN * 2);

    // Key column, narrowed when there is little room to spare -- but never
    // below its longest key and a space. A fixed width ran the longest into
    // their descriptions: `click a headingsort by that column` on a narrow
    // terminal, and `ctrl+shift+tboth: a new terminal window` at 80 and 120.
    let longest = |pick: fn(&H) -> Option<&str>| {
        rows.iter()
            .filter_map(pick)
            .map(crate::model::width)
            .max()
            .unwrap_or(0)
    };
    let key_w = longest(|r| match r {
        H::Key(k, _, _) | H::Mark(k, _) => Some(k),
        _ => None,
    });
    let alt_w = longest(|r| match r {
        H::Key(_, a, _) => Some(a),
        _ => None,
    });
    let kw: usize = (key_w + 1).max(if body_w > 70 { 16 } else { 12 });

    let mut lines: Vec<Line> = Vec::new();
    if show_art {
        let t = tablet.as_ref().unwrap();
        let indent = (area.width as usize).saturating_sub(t.width) / 2;
        lines.push(Line::raw(""));
        for line in t.lines(&art::Light::STILL) {
            let mut sp: Vec<Span> = vec![Span::raw(" ".repeat(indent))];
            sp.extend(line.spans);
            lines.push(Line::from(sp));
        }
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
                let aw = if body_w > 58 { (alt_w + 1).max(11) } else { 0 };
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
    use crate::app::fixtures::{app, app_with};
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
                // A wide character occupies two cells: the glyph goes in the
                // first and the second is left as a blank that the backend
                // skips when it flushes. Reading every cell would count that
                // blank and make a correct row look one column too wide per
                // wide character, so step over it the way the terminal does.
                let mut out = String::new();
                let mut x = 0u16;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    out.push_str(sym);
                    x += (crate::model::width(sym) as u16).max(1);
                }
                out
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
    fn the_example_config_ships_the_default_colours() {
        // It says deleting a line brings back the default, which is only
        // true while the two agree.
        let example: crate::config::Config = toml::from_str(crate::config::EXAMPLE).unwrap();
        let (a, b) = (
            Theme::from_config(&example),
            Theme::from_config(&Default::default()),
        );
        for (name, x, y) in [
            ("accent", a.accent, b.accent),
            ("chrome", a.chrome, b.chrome),
            ("favorite", a.fav, b.fav),
            ("live", a.live, b.live),
            ("tag", a.tag, b.tag),
            ("text", a.text, b.text),
            ("bright", a.bright, b.bright),
            ("gone", a.gone, b.gone),
            ("band", a.band, b.band),
            ("panel", a.panel, b.panel),
        ] {
            assert_eq!(x, y, "{name} in the example is not the default");
        }
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
            let name = format!("{}▌", art::GREEK);
            // Wherever there is a header at all it has room for the name, so
            // the name must be there: a test that only looks when it finds
            // it passes on a header that lost it. A terminal a few rows high
            // gives the header no row.
            let Some(i) = top.find(&name) else {
                assert!(h < 10, "{w}x{h}: no name in the header: {top:?}");
                continue;
            };
            let after = &top[i + name.len()..];
            assert!(
                after.is_empty() || after.starts_with(' '),
                "{w}x{h}: header ran together: {top:?}"
            );
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
            // and with the rescan still running, which adds a chip to the
            // header that has to fit like every other one
            a.indexing = true;
            let _ = render(&mut a, w, h);
            a.indexing = false;
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
    fn a_note_or_tag_cannot_smuggle_escapes_onto_the_screen() {
        // Titles are cleaned by the scanner, but notes and tags come from
        // meta.json -- a file you can edit by hand and that gets synced
        // between machines. Anything drawn from it reaches the terminal, so
        // an escape sequence in a note would be a file deciding what your
        // screen does.
        let mut meta = crate::meta::Meta::default();
        meta.set_note("aaaaaaaa-1", "danger \x1b[31m red \x07 bell");
        meta.add_tag("aaaaaaaa-1", "ok-tag");
        let mut a = app_with(meta, true);
        a.show_preview = true;
        a.rebuild();

        for (w, h) in sizes() {
            if h < 18 {
                continue;
            }
            for line in render(&mut a, w, h) {
                assert!(
                    !line.contains('\u{1b}') && !line.contains('\u{7}'),
                    "{w}x{h}: an escape from meta.json reached the screen: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_wide_title_does_not_break_the_columns() {
        // CJK and emoji take two cells each. The layout counted characters,
        // so a row containing them was padded too narrow and everything to
        // its right slid left -- and since the row is clipped at the edge,
        // the last column simply fell off. The row never looks too long;
        // the columns just stop lining up. So that is what is asserted.
        let mut a = app();
        a.all[0].ai_title = "日本語のタイトル".into(); // 16 columns, 8 characters
        a.all[1].ai_title = "an ascii title".into();
        a.rebuild();

        // wide enough that MODEL is drawn (see Cols::new)
        for w in [120u16, 150, 178, 200] {
            let rows = render(&mut a, w, 24);
            let col_of = |needle: &str, line: &str| -> Option<usize> {
                line.find(needle).map(|b| crate::model::width(&line[..b]))
            };
            let wide = rows
                .iter()
                .find(|l| l.contains("日本語"))
                .expect("no wide row");
            let ascii = rows
                .iter()
                .find(|l| l.contains("an ascii title"))
                .expect("no ascii row");
            let (a_col, w_col) = (col_of("opus-5", ascii), col_of("opus-5", wide));
            assert!(
                a_col.is_some() && w_col.is_some(),
                "{w}: MODEL fell off:\n{wide}\n{ascii}"
            );
            assert_eq!(
                a_col, w_col,
                "{w}: the MODEL column moved when the title had wide characters\n  wide : {wide:?}\n  ascii: {ascii:?}"
            );
        }
    }

    #[test]
    fn a_long_title_never_pushes_the_buttons_off_the_screen() {
        // A title can be 160 characters. It used to take the whole line and
        // both buttons with it: the offer was still up, and there was no
        // longer any visible way to take it.
        let mut a = app();
        offer(&mut a, 1);
        a.reopen[0].title = "Work out why the nightly backup job silently drops the last \
             three datasets when the pool is more than eighty per cent full"
            .into();

        for (w, h) in sizes() {
            if h < 4 {
                continue;
            }
            let rows = render(&mut a, w, h);
            for (x0, x1, _) in &a.hits.banner {
                assert!(
                    *x1 < w,
                    "{w}x{h}: a click target at {x0}..{x1} is off the screen"
                );
            }
            // Where the offer landed depends on how the layout squeezed;
            // at four rows high there may be no room for it at all.
            let Some(y) = a.hits.banner_y else { continue };
            let Some(line) = rows.get(y as usize) else {
                continue;
            };
            if !line.contains("reopen") && !line.contains("open before") {
                continue; // squeezed out entirely, which is not this test
            }
            // wide enough to say anything at all: it must still be possible
            // to accept the offer by clicking
            if w >= 40 {
                assert_eq!(a.hits.banner.len(), 2, "{w}x{h}: lost a button: {line:?}");
                assert!(
                    line.contains('r') && line.contains('x'),
                    "{w}x{h}: no visible way to answer: {line:?}"
                );
            }
        }
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
        let (_, targets) = a.to_open.first().expect("clicking reopen did nothing");
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
        assert!(
            a.outcome.is_none() && a.to_open.is_empty(),
            "declining opened something"
        );
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
        assert!(
            a.outcome.is_none() && a.to_open.is_empty(),
            "the margin acted as a button"
        );
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
    fn the_viewer_draws_on_a_terminal_narrower_than_its_margins() {
        let mut a = app();
        a.viewer = Some((
            vec![crate::preview::Turn {
                role: "you",
                text: "a question".into(),
            }],
            false,
        ));
        a.input_mode = InputMode::Viewer;
        for w in 1..=14 {
            let _ = render(&mut a, w, 12);
        }
    }

    #[test]
    fn a_viewer_too_long_to_count_still_opens_on_its_last_turn() {
        let mut a = app();
        a.viewer = Some((
            (0..400)
                .map(|i| crate::preview::Turn {
                    role: if i % 2 == 0 { "you" } else { "claude" },
                    text: format!("turn {i:03} ").repeat(120),
                })
                .collect(),
            false,
        ));
        a.input_mode = InputMode::Viewer;
        a.viewer_scroll = u16::MAX;
        let rows = render(&mut a, 18, 20);
        assert!(
            rows.iter().any(|r| r.contains("399")),
            "opened somewhere other than the end: {rows:?}"
        );
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
    fn a_key_in_the_help_never_runs_into_what_it_does() {
        // `ctrl+shift+tboth: a new terminal window` at 80 and 120, and
        // `click a headingsort by that column` on a narrow terminal.
        let mut a = app();
        a.input_mode = InputMode::Help;
        for page in [crate::app::HelpPage::Guide, crate::app::HelpPage::Keys] {
            a.help_page = page;
            for w in [50u16, 60, 64, 76, 80, 120, 178] {
                a.help_scroll = 0;
                let rows = render(&mut a, w, 200);
                let all = rows.join("\n");
                for (key, desc) in [
                    ("ctrl+shift+t", "both"),
                    ("click a heading", "sort"),
                    ("click ⌁n", "open"),
                ] {
                    if let Some(row) = rows.iter().find(|r| r.contains(key)) {
                        assert!(!row.contains(&format!("{key}{desc}")), "{w}: {row:?}");
                    }
                }
                assert!(!all.contains("headingsort"), "{w}");
            }
        }
    }

    #[test]
    fn a_prompt_in_cjk_keeps_the_columns_after_it_in_place() {
        // LEFT OFF was padded by characters, so a CJK prompt came out half
        // as wide as its column and MODEL moved left to fill the gap.
        let mut a = app();
        let at = |a: &mut App, w: u16| -> (usize, usize) {
            let rows = render(a, w, 30);
            let find = |needle: &str| {
                rows.iter()
                    .find(|r| r.contains(needle))
                    .and_then(|r| {
                        let i = r.find("opus-5")?;
                        Some(crate::model::width(&r[..i]))
                    })
                    .unwrap_or(0)
            };
            (find("yesterday's thing"), find("last week"))
        };
        for w in [130u16, 150, 178] {
            let (plain, _) = at(&mut a, w);
            let i = a.all.iter().position(|s| s.id == "cccccccc-3").unwrap();
            a.all[i].last_prompt = "ソフトウェアの設計について話しましょう".into();
            a.rebuild();
            let (_, cjk) = at(&mut a, w);
            assert!(plain > 0, "{w}: no MODEL column to measure");
            assert_eq!(cjk, plain, "{w}: MODEL moved");
            a.all[i].last_prompt = "back to plain".into();
            a.rebuild();
        }
    }

    #[test]
    fn what_is_clicked_is_what_was_drawn_there() {
        let mut a = app();
        let col = |row: &str, needle: &str| {
            row.find(needle)
                .map(|i| crate::model::width(&row[..i]) as u16)
        };
        for w in [40u16, 52, 60, 90, 130, 178] {
            let rows = render(&mut a, w, 30);
            // The ⌁2 cell, and only it.
            let (sx0, sx1) = a.sub_span;
            match rows.iter().find_map(|r| col(r, "⌁2").map(|x| (r, x))) {
                Some((row, x)) => {
                    assert!(sx0 <= x && x < sx1, "{w}: ⌁2 at {x}, span {sx0}..={sx1}");
                    let title = col(row, "today's work").unwrap();
                    assert!(sx1 < title, "{w}: the span reaches the title at {title}");
                }
                None => assert!(sx0 > sx1, "{w}: nothing drawn, yet {sx0}..={sx1} clicks"),
            }
            // Every sortable heading, to its last letter.
            let head = &rows[a.hits.colhead_y as usize];
            for label in ["AGE", "FOLDER", "TITLE", "MSGS", "TOKENS"] {
                let Some(x) = col(head, label) else { continue };
                let last = x + label.len() as u16 - 1;
                assert!(
                    a.hits
                        .columns
                        .iter()
                        .any(|(x0, x1, _)| *x0 <= x && last <= *x1),
                    "{w}: {label} at {x}..={last}, spans {:?}",
                    a.hits.columns
                );
            }
        }
    }

    #[test]
    fn on_a_short_terminal_no_two_things_are_clicked_on_one_row() {
        let mut a = app();
        a.reopen = vec![crate::workspace::Entry {
            id: "bbbbbbbb-2".into(),
            cwd: "/home/u/proj".into(),
            ..Default::default()
        }];
        for h in 1..=8u16 {
            for w in [60u16, 120, 178] {
                let _ = render(&mut a, w, h);
                let l = a.hits.list;
                let list = l.y..l.y + l.height;
                let mut rows: Vec<u16> = vec![a.hits.colhead_y, a.hits.footer_y];
                rows.extend(a.hits.banner_y);
                rows.retain(|y| *y != u16::MAX);
                for y in &rows {
                    assert!(*y < h, "{w}x{h}: a click row {y} past the bottom");
                    assert!(!list.contains(y), "{w}x{h}: row {y} is also a list row");
                }
                let mut seen = rows.clone();
                seen.sort();
                seen.dedup();
                assert_eq!(seen.len(), rows.len(), "{w}x{h}: {rows:?}");
            }
        }
    }

    #[test]
    fn drawing_only_what_shows_scrolls_as_the_whole_list_did() {
        // ratatui's list, given every row, against the window worked out
        // here, over a cursor wandering up and down and a list that shrinks.
        use ratatui::widgets::{List, ListItem, ListState};
        let mut seed: u64 = 7;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        let (mut theirs, mut ours) = (ListState::default(), 0usize);
        let mut len = 200;
        let mut cursor = 0usize;
        for step in 0..600 {
            match next(4) {
                0 => cursor = cursor.saturating_sub(next(15)),
                1 => cursor = (cursor + next(15)).min(len - 1),
                2 => cursor = next(len),
                _ => {
                    len = 20 + next(200);
                    cursor = cursor.min(len - 1);
                }
            }
            let h = 5 + next(30) as u16;
            let items: Vec<ListItem> = (0..len).map(|i| ListItem::new(i.to_string())).collect();
            let mut term = Terminal::new(TestBackend::new(20, h)).unwrap();
            theirs.select(Some(cursor));
            term.draw(|f| f.render_stateful_widget(List::new(items), f.area(), &mut theirs))
                .unwrap();
            ours = visible_from(ours, cursor, len, h as usize);
            assert_eq!(
                ours,
                theirs.offset(),
                "step {step}: cursor {cursor} of {len}, height {h}"
            );
        }
    }

    #[test]
    fn a_long_run_of_wide_characters_wraps_inside_its_width() {
        for w in 2..12 {
            for line in wrap_words(&"日本語".repeat(20), w) {
                assert!(width(&line) <= w, "{w}: {line:?} is {}", width(&line));
            }
            for line in wrap_words(&format!("a{}", "語".repeat(15)), w) {
                assert!(width(&line) <= w, "{w}: {line:?}");
            }
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
    fn a_wsx_workspace_is_named_rather_than_spelled_out() {
        // The folder column is at most twenty cells, and every worktree
        // starts with the same thirty-odd characters of state directory.
        // The mark gets cells of its own, so it costs the name nothing it
        // had before.
        let mut a = app();
        for (w, want) in [
            (178u16, "wsx OS-DEV/shy-daffodil"),
            (150, "wsx OS-DEV/shy-daffodil"),
            (120, "wsx OS…/shy-daffodil"),
            (90, "wsx shy-daffodil"),
            (60, "wsx shy-daffodil"),
        ] {
            let rows = render(&mut a, w, 30);
            let row = rows
                .iter()
                .find(|r| r.contains("paging on x86"))
                .unwrap_or_else(|| panic!("{w}: no wsx row"));
            assert!(row.contains(want), "{w}: wanted {want:?} in {row:?}");
            assert!(!row.contains(".local"), "{w}: {row:?}");
        }
    }

    #[test]
    fn the_footer_says_enter_switches_to_wsx_where_it_does() {
        let mut a = app();
        let footer = |a: &mut App, id: &str| {
            a.focus_id(id);
            a.status.clear();
            render(a, 178, 30).last().cloned().unwrap()
        };
        // live, and nothing else running it
        let live = footer(&mut a, "gggggggg-7");
        assert!(live.contains("↵ switch to wsx"), "{live:?}");
        // an ordinary folder, and an archived workspace, resume here
        for id in ["aaaaaaaa-1", "hhhhhhhh-8"] {
            let row = footer(&mut a, id);
            assert!(row.contains("↵ resume"), "{id}: {row:?}");
            assert!(!row.contains("wsx"), "{id}: {row:?}");
        }
        // and the conversation viewer's enter says the same
        a.focus_id("gggggggg-7");
        a.viewer = Some((
            vec![crate::preview::Turn {
                role: "you",
                text: "a turn".into(),
            }],
            false,
        ));
        a.input_mode = InputMode::Viewer;
        let rows = render(&mut a, 178, 30);
        assert!(
            rows.iter().any(|r| r.contains("↵ switch to it in wsx")),
            "{rows:#?}"
        );
    }

    /// The rail's first line, with the cursor on session `id`.
    fn rail_for(a: &mut App, id: &str) -> String {
        a.show_preview = true;
        a.focus_id(id);
        let rows = render(a, 178, 30);
        let title = a.current().unwrap().title().to_string();
        rows.iter()
            .rev()
            .find(|r| r.contains(&title) && r.contains(" · "))
            .unwrap_or_else(|| panic!("no rail for {id}: {rows:#?}"))
            .clone()
    }

    #[test]
    fn every_wsx_row_is_marked_as_wsx_and_no_other_row_is() {
        // Only the rail used to say so, and only for the row under the
        // cursor; the list itself showed a name that could have been any
        // folder's.
        let mut a = app();
        for w in [60u16, 90, 120, 150, 178, 240] {
            let rows = render(&mut a, w, 30);
            for (title, wsx) in [
                ("paging on x86", true),
                ("a GPT disk tool", true),
                ("yesterday's thing", false),
            ] {
                let row = rows
                    .iter()
                    .find(|r| r.contains(title))
                    .unwrap_or_else(|| panic!("{w}: no row for {title:?}"));
                assert_eq!(row.contains(" wsx "), wsx, "{w}: {row:?}");
            }
        }
    }

    #[test]
    fn a_workspace_heading_is_marked_too() {
        let mut a = app();
        a.do_action(crate::app::Action::GroupByDir);
        let rows = render(&mut a, 178, 40);
        assert!(
            rows.iter().any(|r| r.contains("▌ wsx OS-DEV/shy-daffodil")),
            "{rows:#?}"
        );
    }

    #[test]
    fn the_rail_names_the_workspace_and_says_what_became_of_it() {
        let mut a = app();
        let live = rail_for(&mut a, "gggggggg-7");
        assert!(live.contains("wsx OS-DEV/shy-daffodil · live"), "{live:?}");
        // gdisk-app is not in what wsx listed
        let archived = rail_for(&mut a, "hhhhhhhh-8");
        assert!(
            archived.contains("wsx OS-DEV/gdisk-app · archived · resumes in /home/u/OS-DEV"),
            "{archived:?}"
        );
        assert!(!archived.contains("gone"), "{archived:?}");

        let mut forgot = a.wsx.clone();
        forgot.repos.clear();
        a.set_wsx(forgot);
        let nowhere = rail_for(&mut a, "hhhhhhhh-8");
        assert!(
            nowhere.contains("wsx OS-DEV/gdisk-app · archived · resumes where you are"),
            "{nowhere:?}"
        );
    }

    #[test]
    fn the_rail_says_when_wsx_is_the_one_running_it() {
        let mut a = app();
        let tree = format!("{}/OS-DEV/shy-daffodil", crate::app::fixtures::WSX_ROOT);
        let mut by_id = std::collections::HashMap::new();
        by_id.insert(
            "gggggggg-7".to_string(),
            crate::live::Proc {
                pid: 14939,
                cwd: tree,
                resume_id: Some("gggggggg-7".into()),
                model: None,
                under_wsx: true,
            },
        );
        a.live = crate::live::LiveMap {
            by_id,
            by_cwd: Default::default(),
            count: 1,
            supported: true,
        };
        a.apply_overlay();
        let rail = rail_for(&mut a, "gggggggg-7");
        assert!(rail.contains("running in wsx 14939"), "{rail:?}");
    }

    /// The colour a piece of text on screen was drawn in.
    fn colour_of(a: &mut App, w: u16, h: u16, needle: &str) -> Color {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, a)).unwrap();
        let rows = render(a, w, h);
        let buf = term.backend().buffer().clone();
        for (y, row) in rows.iter().enumerate() {
            if let Some(b) = row.find(needle) {
                let x = crate::model::width(&row[..b]) as u16;
                return buf[(x, y as u16)].fg;
            }
        }
        panic!("{needle:?} is not on screen: {rows:#?}");
    }

    #[test]
    fn an_archived_workspace_is_dimmed_not_drawn_as_gone() {
        // Its worktree was deleted on purpose, and it has somewhere better
        // to resume than wherever you are: not the red of a folder lost.
        let mut a = app();
        let archived = colour_of(&mut a, 178, 30, "OS-DEV/gdisk-app");
        assert_eq!(archived, th().chrome);
        assert_ne!(archived, th().gone);
        assert_eq!(
            colour_of(&mut a, 178, 30, "OS-DEV/shy-daffodil"),
            th().accent
        );
        // and a plain folder that has gone is still red
        assert_eq!(colour_of(&mut a, 178, 30, "/home/u/other"), th().gone);
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
        assert!(same_prefix("héllo wörld ┄┄", "héllo wörld ┄┄", 4));
        assert!(!same_prefix("héllo", "hello", 3));
    }

    #[test]
    fn column_widths_always_fit_the_terminal() {
        // 44 is the narrowest width the table still claims to be a table;
        // below that the columns are shed and only the title remains.
        for (w, wsx) in (44usize..=300).flat_map(|w| [(w, false), (w, true)]) {
            let c = Cols::new(w, wsx);
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
            assert!(used <= w, "width {w}, wsx {wsx}: columns want {used}");
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
    const ALLOWED: &str = "▌▐▸┄❯★●◌◆⌁│└─—…·“”↵→←↑↓▏ ";

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

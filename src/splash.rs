//! Opening animation.
//!
//! The name resolves out of noise while the index is actually being built, so
//! the animation is doing real work rather than stalling on purpose: it runs
//! until indexing finishes or a floor of ~1s has passed, whichever is later.
//! Any keypress skips straight to the list, and the keypress is swallowed
//! rather than leaking into the browser underneath.

use crate::index::Progress;
use crate::model::human_size;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{backend::Backend, Terminal};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const WORD: &str = "mnemosyne";
const GLYPHS: &[char] = &[
    '#', '%', '&', '@', '$', '*', '?', '/', '\\', '=', '+', '~', '<', '>', '0', '1', '4', '7',
    'x', 'z', 'k', 'q', 'w', 'm', 'n', 'e', 's', 'y', 'o',
];

const ACCENT: Color = Color::Cyan;
const CHROME: Color = Color::DarkGray;
const BRIGHT: Color = Color::White;

/// Minimum time on screen, so the reveal is actually visible on a warm index.
const FLOOR: Duration = Duration::from_millis(1050);
const FRAME: Duration = Duration::from_millis(33);
/// When each letter stops scrambling.
const LOCK_STEP: Duration = Duration::from_millis(62);
const LOCK_START: Duration = Duration::from_millis(140);

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

/// Returns true if the user skipped.
pub fn run<B: Backend>(term: &mut Terminal<B>, p: &Progress) -> Result<bool> {
    let start = Instant::now();
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut skipped = false;
    let letters: Vec<char> = WORD.chars().collect();
    // The bar is fed from real progress while work remains and from the reveal
    // once it is done; a high-water mark stops it from ever stepping backwards
    // when the source changes under it.
    let mut bar_high = 0.0f64;
    // Once real counts have been on screen, don't fall back to "recalling…".
    let mut showed_counts = false;

    loop {
        let elapsed = start.elapsed();
        let done = p.done.load(Ordering::Relaxed);
        let total = p.total.load(Ordering::Relaxed);
        let bytes = p.bytes.load(Ordering::Relaxed);
        let finished = p.finished.load(Ordering::Relaxed);

        // how many letters have settled
        let settled = if elapsed < LOCK_START {
            0
        } else {
            ((elapsed - LOCK_START).as_millis() / LOCK_STEP.as_millis()) as usize
        };

        term.draw(|f| {
            let area = f.area();
            let w = 54u16.min(area.width);
            let r = centered(area, w, 9);

            // the word: settled letters in place, the rest still churning
            let mut spans: Vec<Span> = Vec::new();
            for (i, ch) in letters.iter().enumerate() {
                let (c, st) = if i < settled {
                    (
                        *ch,
                        Style::default()
                            .fg(BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    let g = GLYPHS[(lcg(&mut rng) as usize) % GLYPHS.len()];
                    (g, Style::default().fg(CHROME))
                };
                spans.push(Span::styled(format!("{c} "), st));
            }

            // the rule under the name draws itself left to right
            let rule_w = letters.len() * 2 - 1;
            let grown = (settled * 2).min(rule_w);
            let rule = format!(
                "{}{}",
                "─".repeat(grown),
                " ".repeat(rule_w.saturating_sub(grown))
            );

            // On a warm index the work is over before the first frame, so a
            // real-progress bar would just be full from the start. When there
            // is genuine work left the bar reports it; otherwise the bar fills
            // with the reveal and the counts below state the actual facts.
            let reveal = (settled as f64 / letters.len() as f64).min(1.0);
            let candidate = if finished {
                reveal
            } else if total > 0 {
                done as f64 / total as f64
            } else {
                0.0
            };
            bar_high = bar_high.max(candidate);
            let pct = bar_high;
            let bar_w = 22usize;
            let filled = (pct * bar_w as f64).round() as usize;
            let bar = format!(
                "{}{}",
                "█".repeat(filled.min(bar_w)),
                "░".repeat(bar_w.saturating_sub(filled))
            );

            let status = if !finished && total > 0 {
                showed_counts = true;
                format!("recalling {done} of {total}")
            } else if finished && (showed_counts || settled >= letters.len()) {
                format!("{total} transcripts · {}", human_size(bytes))
            } else if total > 0 || finished {
                "recalling…".to_string()
            } else {
                "looking for transcripts".to_string()
            };

            let lines = vec![
                Line::from(spans),
                Line::from(Span::styled(rule, Style::default().fg(ACCENT))),
                Line::raw(""),
                Line::from(Span::styled(status, Style::default().fg(CHROME))),
                Line::raw(""),
                Line::from(Span::styled(
                    bar,
                    Style::default().fg(if finished { ACCENT } else { Color::Blue }),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "any key to skip",
                    Style::default().fg(CHROME).add_modifier(Modifier::DIM),
                )),
            ];
            f.render_widget(
                Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
                r,
            );
        })?;

        // a keypress skips, and is consumed here so it cannot act on the list
        if event::poll(FRAME)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    skipped = true;
                    if k.code == KeyCode::Char('c')
                        && k.modifiers.contains(event::KeyModifiers::CONTROL)
                    {
                        return Ok(true);
                    }
                    break;
                }
                _ => {}
            }
        }

        if finished && elapsed >= FLOOR && settled >= letters.len() {
            // brief beat on the completed frame so it does not just vanish
            std::thread::sleep(Duration::from_millis(140));
            break;
        }
        // never hang here if indexing somehow stalls
        if elapsed > Duration::from_secs(30) {
            break;
        }
    }
    Ok(skipped)
}

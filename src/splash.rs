//! Opening animation.
//!
//! The wordmark rises out of a rippling pool -- Mnemosyne is the spring of
//! memory, the counter-pool to Lethe -- lit by a gradient that runs from deep
//! water to pale foam, with a shimmer band that leads the reveal and then
//! keeps sweeping.
//!
//! It is covering real work: the index builds on a background thread while
//! this runs, and the bar reports genuine progress whenever there is any left
//! to report. Any keypress skips, and is swallowed so it cannot act on the
//! list underneath.

use crate::art;
use crate::index::Progress;
use crate::model::human_size;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{backend::Backend, Terminal};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How long the animation lingers once there is nothing left to wait for.
///
/// Two speeds, because the animation exists to cover real work. A cold index
/// genuinely takes a second or so and the full reveal fits inside it. A warm
/// one is done in about twenty milliseconds, and holding a screen for one and
/// a half seconds over eighty milliseconds of work is just a delay wearing a
/// costume — so it plays a brief version instead.
/// Floors, from the config. Its defaults are the values these always were.
fn floors() -> (Duration, Duration) {
    let c = crate::config::Config::load();
    (
        Duration::from_millis(c.splash.floor_cold_ms),
        Duration::from_millis(c.splash.floor_warm_ms),
    )
}
const FRAME: Duration = Duration::from_millis(28);
const REVEAL_COLD_MS: f64 = 780.0;
const REVEAL_WARM_MS: f64 = 300.0;
const SWEEP_MS: f64 = 1500.0;

const BAR_W: usize = 48;
const RIPPLE_ROWS: usize = 2;

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

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

/// A gradient progress bar with eighth-cell resolution.
fn bar_line(pct: f64) -> Line<'static> {
    let exact = pct.clamp(0.0, 1.0) * BAR_W as f64;
    let full = exact.floor() as usize;
    let frac = exact - full as f64;
    let mut spans: Vec<Span> = Vec::with_capacity(BAR_W);
    for x in 0..BAR_W {
        let p = x as f64 / (BAR_W - 1) as f64;
        if x < full {
            spans.push(Span::styled("█", Style::default().fg(rgb(art::ramp(p)))));
        } else if x == full && frac > 0.08 {
            let i = ((frac * art::PARTIALS.len() as f64) as usize).min(art::PARTIALS.len() - 1);
            spans.push(Span::styled(
                art::PARTIALS[i].to_string(),
                Style::default().fg(rgb(art::ramp(p))),
            ));
        } else {
            spans.push(Span::styled(
                "░",
                Style::default().fg(rgb(art::sink(art::ramp(p), 0.72))),
            ));
        }
    }
    Line::from(spans)
}

/// Returns true if the user skipped.
pub fn run<B: Backend>(term: &mut Terminal<B>, p: &Progress) -> Result<bool> {
    let start = Instant::now();
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut skipped = false;
    let mut bar_high = 0.0f64;
    let mut showed_counts = false;
    // Resolved once we know whether there was real work to cover. Deciding on
    // the first frame does not work: the frame happens before even a warm
    // refresh has finished, so everything looked cold.
    let mut speed: Option<(Duration, f64)> = None;
    let (floor_cold, floor_warm) = floors();
    /// Finishing inside this means the index was already warm.
    const WARM_IF_DONE_BY_MS: f64 = 300.0;

    // Pick the largest wordmark this terminal can hold; None means fall back
    // to the letter reveal.
    let term_w = term.size().map(|s| s.width as usize).unwrap_or(80);
    let mark = art::mark_for(term_w.saturating_sub(4));
    let mark_w = mark
        .as_ref()
        .map(|m| m.width)
        .unwrap_or(art::WORD.chars().count() * 2);
    let mark_h = mark.as_ref().map(|m| m.height()).unwrap_or(1);

    loop {
        let elapsed = start.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        let done = p.done.load(Ordering::Relaxed);
        let total = p.total.load(Ordering::Relaxed);
        let bytes = p.bytes.load(Ordering::Relaxed);
        let finished = p.finished.load(Ordering::Relaxed);

        if speed.is_none() {
            if finished && ms <= WARM_IF_DONE_BY_MS {
                speed = Some((floor_warm, REVEAL_WARM_MS));
            } else if ms > WARM_IF_DONE_BY_MS {
                speed = Some((floor_cold, REVEAL_COLD_MS));
            }
        }
        // Until it is known, animate at the slower pace: switching from slow
        // to fast nudges the reveal forward a little, which reads fine, while
        // the other way round would make it appear to stall.
        let (floor, reveal_ms) = speed.unwrap_or((floor_cold, REVEAL_COLD_MS));
        let rev = (ms / reveal_ms).min(1.0);
        let complete = rev >= 1.0;

        // the shimmer leads the reveal, then keeps sweeping across
        let band = if !complete {
            rev * mark_w as f64
        } else {
            let t = ((ms - reveal_ms) / SWEEP_MS).fract();
            t * (mark_w as f64 + 30.0) - 15.0
        };

        let candidate = if finished {
            rev
        } else if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        bar_high = bar_high.max(candidate);

        let status = if !finished && total > 0 {
            showed_counts = true;
            format!("recalling {done} of {total}")
        } else if finished && (showed_counts || complete) {
            format!("{total} transcripts · {}", human_size(bytes))
        } else {
            "recalling…".to_string()
        };

        term.draw(|f| {
            let area = f.area();
            let big = mark.is_some() && area.width as usize >= mark_w + 4 && area.height >= 16;

            let mut lines: Vec<Line> = Vec::new();

            if big {
                let m = mark.as_ref().unwrap();
                let revealed = (rev * mark_w as f64) as usize;
                for (r, row) in m.rows.iter().enumerate() {
                    let mut spans: Vec<Span> = Vec::with_capacity(mark_w);
                    for (x, ch) in row.iter().enumerate() {
                        if x < revealed {
                            // Blank cells of the art stay blank; only inked
                            // ones take colour, so the tone does the drawing.
                            if *ch == ' ' {
                                spans.push(Span::raw(" "));
                            } else {
                                let c = art::column_color(x, mark_w, band, r, mark_h);
                                spans.push(Span::styled(
                                    ch.to_string(),
                                    Style::default().fg(rgb(c)),
                                ));
                            }
                        } else {
                            // Not yet surfaced. Only the crests show, so this
                            // reads as open water instead of a wall of glyphs.
                            let (w, i) = art::ripple_at(x, ms / 240.0, r as f64 * 1.9);
                            // Only the highest crests, and dim: a dense field
                            // here fights the art instead of framing it.
                            if i > 0.93 {
                                let c = art::sink(art::ramp(0.34), 0.68);
                                spans
                                    .push(Span::styled(w.to_string(), Style::default().fg(rgb(c))));
                            } else {
                                spans.push(Span::raw(" "));
                            }
                        }
                    }
                    lines.push(Line::from(spans));
                }
            } else {
                // compact fallback: spaced letters resolving out of noise
                let letters: Vec<char> = art::WORD.chars().collect();
                let settled = (rev * letters.len() as f64) as usize;
                let mut spans: Vec<Span> = Vec::new();
                for (i, ch) in letters.iter().enumerate() {
                    if i < settled {
                        let c = art::column_color(i * 2, letters.len() * 2, band / 6.0, 0, 1);
                        spans.push(Span::styled(
                            format!("{ch} "),
                            Style::default().fg(rgb(c)).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        let g = art::WAVES_FALLBACK
                            [(lcg(&mut rng) as usize) % art::WAVES_FALLBACK.len()];
                        spans.push(Span::styled(
                            format!("{g} "),
                            Style::default().fg(rgb(art::sink(art::ramp(0.3), 0.55))),
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }

            // the pool the name rose out of
            for r in 0..RIPPLE_ROWS {
                let w = if big {
                    mark_w
                } else {
                    art::WORD.chars().count() * 2
                };
                let cells = art::ripple(w, ms / 230.0 + r as f64 * 0.8, r as f64 * 2.1);
                let fade = 0.35 + r as f64 * 0.3;
                let spans: Vec<Span> = cells
                    .into_iter()
                    .map(|(ch, i)| {
                        if i < 0.58 {
                            return Span::raw(" ");
                        }
                        let c = art::sink(art::ramp(0.16 + i * 0.34), fade);
                        Span::styled(ch.to_string(), Style::default().fg(rgb(c)))
                    })
                    .collect();
                lines.push(Line::from(spans));
            }

            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                status.clone(),
                Style::default().fg(rgb(art::ramp(0.62))),
            )));
            lines.push(Line::raw(""));
            lines.push(bar_line(bar_high));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "any key to skip",
                Style::default()
                    .fg(rgb(art::sink(art::ramp(0.4), 0.55)))
                    .add_modifier(Modifier::DIM),
            )));

            let h = lines.len() as u16;
            let w = if big { mark_w as u16 + 2 } else { 44 };
            let r = centered(area, w.min(area.width), h);
            f.render_widget(
                Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
                r,
            );
        })?;

        if event::poll(FRAME)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    skipped = true;
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        return Ok(true);
                    }
                    break;
                }
            }
        }

        if finished && elapsed >= floor && complete {
            std::thread::sleep(Duration::from_millis(160));
            break;
        }
        if elapsed > Duration::from_secs(30) {
            break;
        }
    }
    Ok(skipped)
}

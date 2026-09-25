//! Opening animation.
//!
//! The name is cut into a leaf of gold, letter by letter, left to right,
//! with the point of the tool still bright where it last cut. Once it is
//! done a glint crosses the leaf, and goes on crossing it for as long as the
//! animation stays up.
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
use ratatui::style::{Modifier, Style};
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
    let c = crate::config::get();
    (
        Duration::from_millis(c.splash.floor_cold_ms),
        Duration::from_millis(c.splash.floor_warm_ms),
    )
}
const FRAME: Duration = Duration::from_millis(28);
const REVEAL_COLD_MS: f64 = 780.0;
const REVEAL_WARM_MS: f64 = 300.0;
/// One glint and the pause after it.
const SWEEP_MS: f64 = 1800.0;
/// The share of each sweep the glint spends crossing; the rest is rest.
const CROSSING: f64 = 0.6;

const BAR_W: usize = 48;

use art::rgb;

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

/// Progress as a gold thread drawn along a tarnished one, in half-cell
/// steps.
fn bar_line(pct: f64) -> Line<'static> {
    let exact = pct.clamp(0.0, 1.0) * BAR_W as f64;
    let full = exact.floor() as usize;
    let half = exact - full as f64 >= 0.5;
    let mut spans: Vec<Span> = Vec::with_capacity(BAR_W);
    for x in 0..BAR_W {
        let p = x as f64 / (BAR_W - 1) as f64;
        let gold = Style::default().fg(rgb(art::ramp(0.5 + p * 0.45)));
        if x < full {
            spans.push(Span::styled("━", gold));
        } else if x == full && half {
            spans.push(Span::styled("╸", gold));
        } else {
            spans.push(Span::styled(
                "─",
                Style::default().fg(rgb(art::sink(art::ramp(0.35), 0.45))),
            ));
        }
    }
    Line::from(spans)
}

/// How the opening animation ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum End {
    /// It ran its course, or the work finished and it stepped aside.
    Done,
    /// A key was pressed once there was nothing left to wait for.
    Skipped,
    /// ctrl+c. The caller must not wait for the scan; it should leave.
    Aborted,
}

/// Returns how it ended.
///
/// `must_wait` says there is nothing cached to show yet, so the list cannot
/// appear until the scan does. When it is false the animation plays its own
/// short course and hands over; the scan carries on behind the list, which
/// is the difference between a start that takes a moment and one that takes
/// twelve seconds re-reading every transcript.
pub fn run<B: Backend>(term: &mut Terminal<B>, p: &Progress, must_wait: bool) -> Result<End> {
    let start = Instant::now();
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut ended = End::Done;
    let mut bar_high = 0.0f64;
    let mut showed_counts = false;
    // Resolved once we know whether there was real work to cover. Deciding on
    // the first frame does not work: the frame happens before even a warm
    // refresh has finished, so everything looked cold.
    let mut speed: Option<(Duration, f64)> = None;
    let (floor_cold, floor_warm) = floors();
    /// Finishing inside this means the index was already warm.
    const WARM_IF_DONE_BY_MS: f64 = 300.0;

    // Pick the largest tablet this terminal can hold; None means fall back
    // to the letter reveal.
    let term_w = term.size().map(|s| s.width as usize).unwrap_or(80);
    let tablet = art::tablet_for(term_w.saturating_sub(4));
    let letters: Vec<char> = art::GREEK.chars().collect();
    let mark_w = tablet
        .as_ref()
        .map(|t| t.width)
        .unwrap_or(letters.len() * 2);
    let mark_h = tablet.as_ref().map(|t| t.height()).unwrap_or(1);

    loop {
        let elapsed = start.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        let done = p.done.load(Ordering::Relaxed);
        let total = p.total.load(Ordering::Relaxed);
        let bytes = p.bytes.load(Ordering::Relaxed);
        let finished = p.finished.load(Ordering::Relaxed);

        if speed.is_none() {
            // The slow pace exists to cover a scan nobody can skip. When the
            // list is already there to fall back on, there is nothing to
            // cover and the animation has no business holding it back.
            if !must_wait || (finished && ms <= WARM_IF_DONE_BY_MS) {
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

        // The point of the tool is the light while the letters are cut;
        // after that, a glint every so often.
        let light = if !complete {
            art::Light {
                cut_to: rev * mark_w as f64,
                glint: None,
            }
        } else {
            let t = ((ms - reveal_ms) / SWEEP_MS).fract() / CROSSING;
            art::Light {
                cut_to: f64::INFINITY,
                glint: (t <= 1.0).then_some(t * (mark_w as f64 + 24.0) - 12.0),
            }
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

        // Waiting is only mandatory when there is nothing to fall back to.
        let blocked = must_wait && !finished;
        // Skipping is only offered once there is nothing left to wait for.
        // The animation used to say "any key to skip" while the index was
        // still being built, and pressing one dropped you onto a frozen
        // screen until the scan finished -- the tool looked hung at exactly
        // the moment it was working hardest. A long wait only happens on a
        // cold cache, which is once after an update, so say that too.
        let working = blocked;
        let hint = if !working {
            "any key to skip"
        } else if elapsed >= Duration::from_secs(4) {
            "reading every transcript — this happens once after an update · ctrl+c to leave"
        } else {
            "ctrl+c to leave"
        };

        term.draw(|f| {
            let area = f.area();
            let big = tablet.is_some()
                && area.width as usize >= mark_w + 4
                && area.height as usize >= mark_h + 8;

            let mut lines: Vec<Line> = Vec::new();

            if big {
                lines.extend(tablet.as_ref().unwrap().lines(&light));
            } else {
                // Compact fallback: the name settling, letter by letter, out
                // of the rest of the alphabet it is written in.
                let settled = (rev * letters.len() as f64) as usize;
                let mut spans: Vec<Span> = Vec::new();
                for (i, ch) in letters.iter().enumerate() {
                    if i < settled {
                        let p = 0.6 + i as f64 / letters.len() as f64 * 0.4;
                        spans.push(Span::styled(
                            format!("{ch} "),
                            Style::default()
                                .fg(rgb(art::ramp(p)))
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        let g = art::NOISE[(lcg(&mut rng) as usize) % art::NOISE.len()];
                        spans.push(Span::styled(
                            format!("{g} "),
                            Style::default().fg(rgb(art::sink(art::ramp(0.4), 0.5))),
                        ));
                    }
                }
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
            // Quiet, but there to be read: at the old strength it all but
            // vanished against the gold above it.
            lines.push(Line::from(Span::styled(
                fitting_hint(hint, area.width),
                Style::default().fg(rgb(art::sink(art::ramp(0.4), 0.3))),
            )));

            let h = lines.len() as u16;
            let w = box_width(
                if big { mark_w } else { 42 },
                fitting_hint(hint, area.width),
            );
            let r = centered(area, w.min(area.width), h);
            f.render_widget(
                Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
                r,
            );
        })?;

        if event::poll(FRAME)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        return Ok(End::Aborted);
                    }
                    // While the scan is still running there is nothing to
                    // skip to, so the keypress is swallowed rather than
                    // dropping you onto a screen that cannot change yet.
                    if !working {
                        ended = End::Skipped;
                        break;
                    }
                }
            }
        }

        if (finished || !must_wait) && elapsed >= floor && complete {
            std::thread::sleep(Duration::from_millis(160));
            break;
        }
        if gives_up(elapsed, must_wait, finished) {
            break;
        }
    }
    Ok(ended)
}

/// Whether the animation stops waiting, done or not.
///
/// Never while the list has nothing to show without the scan. Stopping then
/// handed over to a join on the scan thread, with no one reading keys: the
/// screen froze on "ctrl+c to leave" and ctrl+c did nothing until the scan
/// was over. The refresh says when it has finished however it ends, so
/// this cannot wait on nothing.
fn gives_up(elapsed: Duration, must_wait: bool, finished: bool) -> bool {
    elapsed > Duration::from_secs(30) && (!must_wait || finished)
}

/// The hint, or its short form where the long one does not fit.
fn fitting_hint(hint: &'static str, area_w: u16) -> &'static str {
    if crate::model::width(hint) + 2 > area_w as usize {
        "ctrl+c to leave"
    } else {
        hint
    }
}

/// Wide enough for everything in the box. It was the wordmark's width, or
/// 44 without one: narrower than the 48-cell bar, which never looked full,
/// and than the long hint, which lost its ending -- at 80x24, the part
/// saying how to leave.
fn box_width(mark_w: usize, hint: &str) -> u16 {
    (mark_w + 2)
        .max(BAR_W + 2)
        .max(crate::model::width(hint) + 2) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cold_start_never_stops_waiting_on_its_own() {
        let late = Duration::from_secs(45);
        assert!(
            !gives_up(late, true, false),
            "handed over to a frozen screen"
        );
        assert!(gives_up(late, true, true));
        assert!(gives_up(late, false, false));
        assert!(!gives_up(Duration::from_secs(5), false, false));
    }

    #[test]
    fn the_box_holds_the_bar_and_the_hint() {
        let long = "reading every transcript — this happens once after an update · ctrl+c to leave";
        assert!(box_width(42, long) as usize >= crate::model::width(long));
        assert!(box_width(42, "ctrl+c to leave") as usize >= BAR_W);
        // and on a terminal the long one does not fit, the short one
        assert_eq!(fitting_hint(long, 60), "ctrl+c to leave");
        assert_eq!(fitting_hint(long, 120), long);
    }
}

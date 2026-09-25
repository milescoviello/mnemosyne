//! The mark, and the gold it is drawn in.
//!
//! The mark is a meander -- the Greek key -- drawn as a single square
//! spiral: one unbroken path that turns inward, the way back through the
//! maze. The Orphic gold tablets told the dead which spring to drink from to
//! remember, and the gold is theirs.
//!
//! It is a symbol, not a word. The name was drawn here twice before, cut
//! into a slab of gold and then as gilded block letters, and both were
//! worse than no name at all.
//!
//! Nothing is baked. The spiral is walked here, a cell of path at a time,
//! and each cell drawn as a square of pixels. A terminal cell is about twice
//! as tall as it is wide, so half a cell (▀ over ▄) is close to a square
//! pixel, and a cell holds two colours: the top half as foreground, the
//! bottom as background. At the large size every path cell is two pixels a
//! side, so each stroke is whole cells, and a whole cell is drawn as its
//! background rather than as █: a glyph does not always reach the top and
//! bottom of its cell, and a column of them showed a hairline at every row.
//!
//! The light is worked out per pixel and per frame, because it moves: while
//! the index builds the gold runs along the path from its outer end to the
//! centre, and then a glint crosses it. The same gold ramp dyes everything
//! else in the interface, from the age of a session to the keys in the
//! footer, so a palette set in the config regilds the mark too.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// A square spiral `n` cells a side: its cells in order from the outer end
/// to the centre, a cell of gap between each turn and the next. `n` is one
/// more than a multiple of four, which is what lands it on the centre.
///
/// Walked by arm length -- three sides the full width, then two of each
/// length, two shorter each time -- so every turn joins the next. Stepping
/// the arms in from the edges instead left each ring two cells short of the
/// one outside it: broken squares that only looked like a spiral.
fn spiral(n: usize) -> Vec<(usize, usize)> {
    let n = n as isize;
    let mut arms = vec![n - 1; 3];
    let mut len = n - 3;
    while len > 0 {
        arms.extend([len, len]);
        len -= 2;
    }
    let (mut x, mut y) = (0isize, 0isize);
    let mut path = vec![(0, 0)];
    for (k, arm) in arms.into_iter().enumerate() {
        let (dx, dy) = [(1, 0), (0, 1), (-1, 0), (0, -1)][k % 4];
        for _ in 0..arm {
            x += dx;
            y += dy;
            path.push((x as usize, y as usize));
        }
    }
    path
}

pub struct Mark {
    /// Where each pixel falls along the path, or None off it.
    px: Vec<Vec<Option<usize>>>,
    /// Cells of path, end to end.
    len: usize,
    /// Pixels to a path cell, each way.
    scale: usize,
    /// Width in columns, which is also width in pixels.
    pub width: usize,
}

/// How a mark is toned: where on the ramp the head and foot of it sit.
#[derive(Clone, Copy)]
struct Tone {
    foot: f64,
    head: f64,
}

/// The terminal's.
const GOLD: Tone = Tone {
    foot: 0.58,
    head: 0.9,
};

impl Mark {
    /// A spiral `n` cells a side, each cell `scale` pixels square.
    fn new(n: usize, scale: usize) -> Mark {
        let side = n * scale;
        // half blocks pair the rows, so round the height up to even
        let mut px = vec![vec![None; side]; side + side % 2];
        let path = spiral(n);
        for (i, (cx, cy)) in path.iter().enumerate() {
            for dy in 0..scale {
                for dx in 0..scale {
                    px[cy * scale + dy][cx * scale + dx] = Some(i);
                }
            }
        }
        Mark {
            px,
            len: path.len(),
            scale,
            width: side,
        }
    }

    /// Height in terminal rows: two pixels to a row.
    pub fn height(&self) -> usize {
        self.px.len() / 2
    }

    fn at(&self, x: usize, y: usize) -> Option<usize> {
        self.px.get(y).and_then(|r| r.get(x)).copied().flatten()
    }

    /// How far a glint reaches either side of its centre: a quarter of the
    /// mark, so it reads as a glint at any size rather than a wash.
    fn reach(&self) -> f64 {
        (self.width as f64 * 0.22).clamp(3.0, 8.0)
    }

    /// Where a glint's centre starts and ends to cross the mark whole: clear
    /// of it at both ends, so it neither appears on it nor vanishes there.
    pub fn glint_path(&self) -> (f64, f64) {
        let lean = self.px.len().saturating_sub(1) as f64 * SLANT;
        (-self.reach(), self.width as f64 + lean + self.reach())
    }

    /// The colour of one pixel, or None off the path.
    fn colour(&self, x: usize, y: usize, light: &Light) -> Option<Rgb> {
        self.toned(x, y, light, GOLD)
    }

    fn toned(&self, x: usize, y: usize, light: &Light, tone: Tone) -> Option<Rgb> {
        let i = self.at(x, y)?;
        // How far along the path the gold has run, in cells. Past it the
        // path is already there in dim bronze, so the mark is whole from
        // the first frame and the gold is plainly running along it.
        let front = light.traced.clamp(0.0, 1.0) * self.len as f64;
        if i as f64 >= front {
            return Some(sink(ramp(0.35), 0.6));
        }
        // Bright at the head of the mark, deeper gold at its foot. Lit a
        // path cell at a time, not a pixel: the two pixels of a doubled
        // cell came out two colours, and the cell half one and half the
        // other where it should be solid. A lit top edge on each stroke
        // went too, because it set every corner off as a darker square.
        let top = y - y % self.scale;
        let rows = (self.px.len() / self.scale).max(2) as f64;
        let up = 1.0 - (y / self.scale) as f64 / (rows - 1.0);
        let mut c = ramp(tone.foot + up * (tone.head - tone.foot));
        // the last few cells run still bright from the brush
        let behind = front - i as f64;
        if behind < 4.0 && light.traced < 1.0 {
            c = lift(c, (1.0 - behind / 4.0) * 0.6);
        }
        if let Some(g) = light.glint {
            let d = (x as f64 + top as f64 * SLANT - g).abs();
            if d < self.reach() {
                c = lift(c, (1.0 - d / self.reach()).powi(2) * 0.6);
            }
        }
        Some(c)
    }

    /// The mark as terminal rows, lit as `light` says. Off the path nothing
    /// is drawn, so the terminal's background shows through.
    pub fn lines(&self, light: &Light) -> Vec<Line<'static>> {
        (0..self.height())
            .map(|row| {
                let spans: Vec<Span<'static>> = (0..self.width)
                    .map(|x| {
                        let top = self.colour(x, row * 2, light);
                        let bottom = self.colour(x, row * 2 + 1, light);
                        match (top, bottom) {
                            (None, None) => Span::raw(" "),
                            (Some(t), None) => Span::styled("▀", Style::default().fg(rgb(t))),
                            (None, Some(b)) => Span::styled("▄", Style::default().fg(rgb(b))),
                            (Some(t), Some(b)) if t == b => {
                                Span::styled(" ", Style::default().bg(rgb(t)))
                            }
                            (Some(t), Some(b)) => {
                                Span::styled("▀", Style::default().fg(rgb(t)).bg(rgb(b)))
                            }
                        }
                    })
                    .collect();
                Line::from(spans)
            })
            .collect()
    }
}

/// How far a glint leans: this many columns left for each pixel down.
const SLANT: f64 = 0.6;

/// How the mark is lit on one frame.
#[derive(Clone, Copy, Debug)]
pub struct Light {
    /// How much of the path, 0 to 1, the gold has run along.
    pub traced: f64,
    /// Where the glint is, in columns, if there is one.
    pub glint: Option<f64>,
}

impl Light {
    /// Gilded end to end, and at rest.
    pub const STILL: Light = Light {
        traced: 1.0,
        glint: None,
    };
}

/// The sizes the mark comes in, largest first: cells a side, and pixels to
/// a cell.
const SIZES: [(usize, usize); 3] = [(13, 2), (13, 1), (9, 1)];

/// The mark in miniature, for the corner of the list: a spiral five cells a
/// side, the smallest that still turns in on itself, three rows tall.
pub fn corner() -> Mark {
    Mark::new(5, 1)
}

/// The largest mark that fits in `width` columns and `height` rows, or None
/// when even the smallest would not.
pub fn mark_for(width: usize, height: usize) -> Option<Mark> {
    SIZES
        .iter()
        .map(|&(n, scale)| Mark::new(n, scale))
        .find(|m| m.width <= width && m.height() <= height)
}

// ---------------------------------------------------------------- the gold

type Rgb = (u8, u8, u8);

pub fn rgb((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// Tarnished to fresh. Read as age everywhere a session's age is drawn: the
/// newest are bright leaf, the oldest have gone to bronze.
const RAMP: &[Rgb] = &[
    (43, 33, 23),    // soot
    (95, 69, 36),    // bronze
    (154, 116, 56),  // tarnish
    (212, 169, 79),  // gold
    (239, 207, 122), // leaf
    (251, 239, 196), // electrum
];

fn lerp(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let f = |x: u8, y: u8| {
        (x as f64 + (y as f64 - x as f64) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// The ramp actually in use. Set once from the config, if it supplies one.
static RAMP_OVERRIDE: std::sync::OnceLock<Vec<Rgb>> = std::sync::OnceLock::new();

/// Install a palette from the config. Ignored if fewer than two stops, since
/// a gradient needs somewhere to go.
pub fn set_ramp(stops: Vec<Rgb>) {
    if stops.len() >= 2 {
        let _ = RAMP_OVERRIDE.set(stops);
    }
}

fn stops() -> &'static [Rgb] {
    RAMP_OVERRIDE.get().map(|v| v.as_slice()).unwrap_or(RAMP)
}

/// Sample the ramp at `p` in 0..=1.
pub fn ramp(p: f64) -> Rgb {
    let ramp = stops();
    let p = p.clamp(0.0, 1.0);
    let span = (ramp.len() - 1) as f64;
    let x = p * span;
    let i = x.floor() as usize;
    if i >= ramp.len() - 1 {
        return ramp[ramp.len() - 1];
    }
    lerp(ramp[i], ramp[i + 1], x - i as f64)
}

/// Brighten toward white by `amount` (0..=1).
pub fn lift(c: Rgb, amount: f64) -> Rgb {
    lerp(c, (255, 252, 240), amount.clamp(0.0, 1.0))
}

/// Dim toward the background, for anything meant to recede.
pub fn sink(c: Rgb, amount: f64) -> Rgb {
    lerp(c, (14, 11, 8), amount.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The logo in the README, for a page that is light: the terminal's gold
    /// all but vanishes on white, so it is bronze there, with no lit edge.
    const BRONZE: Tone = Tone {
        foot: 0.22,
        head: 0.52,
    };

    /// The logo as an SVG, a unit to a pixel. One path per colour, each run
    /// of it a rectangle.
    fn svg(m: &Mark, tone: Tone) -> String {
        let px = 8;
        let h = m.px.len();
        let mut runs: std::collections::BTreeMap<Rgb, String> = Default::default();
        for y in 0..h {
            let mut x = 0;
            while x < m.width {
                let Some(c) = m.toned(x, y, &Light::STILL, tone) else {
                    x += 1;
                    continue;
                };
                let mut n = 1;
                while x + n < m.width && m.toned(x + n, y, &Light::STILL, tone) == Some(c) {
                    n += 1;
                }
                runs.entry(c)
                    .or_default()
                    .push_str(&format!("M{x} {y}h{n}v1h-{n}z"));
                x += n;
            }
        }
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" \
             width=\"{pw}\" height=\"{ph}\" shape-rendering=\"crispEdges\">\n\
             <title>mnemosyne</title>\n",
            w = m.width,
            pw = m.width * px,
            ph = h * px,
        );
        for ((r, g, b), d) in runs {
            out.push_str(&format!(
                "<path fill=\"#{r:02x}{g:02x}{b:02x}\" d=\"{d}\"/>\n"
            ));
        }
        out.push_str("</svg>\n");
        out
    }

    #[test]
    fn the_logos_are_what_the_terminal_draws() {
        // Drawn from the same mark, so the README cannot show something the
        // terminal does not. `MNEMOSYNE_WRITE_LOGOS=1 cargo test logos`
        // redraws them after a change.
        let m = Mark::new(13, 2);
        for (file, tone) in [
            ("docs/logo-dark.svg", GOLD),
            ("docs/logo-light.svg", BRONZE),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
            let want = svg(&m, tone);
            if std::env::var_os("MNEMOSYNE_WRITE_LOGOS").is_some() {
                std::fs::write(&path, &want).unwrap();
            }
            let have = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                have == want,
                "{file} is not what the terminal draws: MNEMOSYNE_WRITE_LOGOS=1 cargo test logos"
            );
        }
    }

    #[test]
    fn the_example_config_ships_the_default_ramp() {
        // It says deleting a line brings back the default, which is only
        // true while the two agree.
        let c: crate::config::Config = toml::from_str(crate::config::EXAMPLE).unwrap();
        let stops: Vec<Rgb> = c
            .ramp
            .unwrap()
            .iter()
            .filter_map(|s| crate::config::parse_color(s))
            .collect();
        assert_eq!(stops, RAMP);
    }

    #[test]
    fn the_spiral_is_one_path_that_never_touches_itself() {
        for n in [5, 9, 13] {
            let path = spiral(n);
            let set: std::collections::HashSet<_> = path.iter().copied().collect();
            assert_eq!(set.len(), path.len(), "{n}: a cell twice");
            for w in path.windows(2) {
                let (a, b) = (w[0], w[1]);
                assert_eq!(a.0.abs_diff(b.0) + a.1.abs_diff(b.1), 1, "{n}: a jump");
            }
            // Each cell touches only the ones before and after it: a turn
            // running into the one outside it would close the maze.
            for (i, &(x, y)) in path.iter().enumerate() {
                let touching = path
                    .iter()
                    .enumerate()
                    .filter(|(_, &(u, v))| x.abs_diff(u) + y.abs_diff(v) == 1)
                    .count();
                let ends = (i == 0 || i == path.len() - 1) as usize;
                assert_eq!(touching, 2 - ends, "{n}: {x},{y} touches another turn");
            }
            // it winds in to the centre
            assert_eq!(*path.last().unwrap(), (n / 2, n / 2));
        }
    }

    #[test]
    fn the_largest_that_fits_is_chosen() {
        assert_eq!(mark_for(200, 60).unwrap().width, 26);
        assert_eq!(mark_for(200, 10).unwrap().width, 13);
        assert_eq!(mark_for(20, 60).unwrap().width, 13);
        assert_eq!(mark_for(10, 60).unwrap().width, 9);
        assert!(mark_for(8, 60).is_none());
        assert!(mark_for(200, 4).is_none());
    }

    #[test]
    fn the_large_mark_is_whole_cells_of_background() {
        // Two pixels a side, so no half block; and each cell its background
        // colour rather than a █, which leaves a hairline at every row
        // where the glyph stops short of the cell.
        let m = mark_for(200, 60).unwrap();
        let mut solid = 0;
        for line in m.lines(&Light::STILL) {
            for s in &line.spans {
                assert_eq!(s.content, " ");
                solid += s.style.bg.is_some() as usize;
            }
        }
        assert!(solid > 0);
    }

    #[test]
    fn it_looks_square() {
        // A cell is about twice as tall as it is wide.
        for (w, h) in [(200, 60), (200, 10), (10, 60)] {
            let m = mark_for(w, h).unwrap();
            assert!(
                m.width.abs_diff(m.height() * 2) <= 1,
                "{} x {}",
                m.width,
                m.height()
            );
        }
    }

    #[test]
    fn the_gold_runs_from_the_outer_end_to_the_centre() {
        let m = Mark::new(13, 1);
        let half = Light {
            traced: 0.5,
            glint: None,
        };
        let bare = Some(sink(ramp(0.35), 0.6));
        // the outer end is gilded half way, the centre not yet
        assert_ne!(m.colour(0, 0, &half), bare);
        assert_eq!(m.colour(6, 6, &half), bare);
        assert_ne!(m.colour(6, 6, &Light::STILL), bare);
    }

    #[test]
    fn a_glint_starts_and_ends_clear_of_the_mark() {
        for (w, h) in [(200, 60), (200, 10), (10, 60)] {
            let m = mark_for(w, h).unwrap();
            let (from, to) = m.glint_path();
            for g in [from, to] {
                let lit = Light {
                    traced: 1.0,
                    glint: Some(g),
                };
                assert!(
                    (0..m.px.len()).all(|y| (0..m.width)
                        .all(|x| m.colour(x, y, &lit) == m.colour(x, y, &Light::STILL))),
                    "{}: the glint at {g} is still on the mark",
                    m.width
                );
            }
        }
    }

    #[test]
    fn nothing_is_drawn_off_the_path() {
        // The terminal's background has to show through: a colour there
        // would paint a slab behind the mark.
        for (w, h) in [(200, 60), (200, 10), (10, 60)] {
            let m = mark_for(w, h).unwrap();
            for (row, line) in m.lines(&Light::STILL).iter().enumerate() {
                for (x, s) in line.spans.iter().enumerate() {
                    let on = m.at(x, row * 2).is_some() || m.at(x, row * 2 + 1).is_some();
                    let painted = s.style.bg.is_some() || s.content != " ";
                    assert_eq!(painted, on, "{} at {x},{row}", m.width);
                }
            }
        }
    }

    #[test]
    fn every_row_draws_exactly_its_width() {
        for (w, h) in [(200, 60), (200, 10), (10, 60)] {
            let m = mark_for(w, h).unwrap();
            for line in m.lines(&Light::STILL) {
                let cells: usize = line
                    .spans
                    .iter()
                    .map(|s| crate::model::width(&s.content))
                    .sum();
                assert_eq!(cells, m.width);
            }
        }
    }
}

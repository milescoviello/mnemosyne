//! The tablet, and the gold it is cut from.
//!
//! The Orphic gold tablets were thin leaves of gold buried with the dead,
//! telling them which spring to drink from in the underworld: not Lethe,
//! which makes you forget, but Mnemosyne, which lets you remember. So the
//! name is cut into a leaf of gold, in Greek capitals -- ΜΝΗΜΟΣΥΝΗ -- above
//! the first words of the tablet from Hipponion, "this is the work of
//! Memory".
//!
//! The letters are set by hand on a pixel grid, by `tools/gen-tablet.py`, and
//! baked in. A terminal cell is about twice as tall as it is wide, so each
//! half of a cell (▀ over ▄) is close to a square pixel, and a cell can hold
//! two colours: the top half as foreground, the bottom as background. Eight
//! pixels of letter is four rows. A rasteriser smears every diagonal at that
//! size; a hand-set pixel does not.
//!
//! Only the geometry is baked. The light is worked out here, per pixel and
//! per frame, because it moves: the letters are cut left to right while the
//! index builds, and then a glint crosses the leaf. The same gold ramp dyes
//! everything else in the interface, from the age of a session to the keys
//! in the footer, so a palette set in the config recolours the tablet too.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub const INSCRIPTION: &str = "ΜΝΑΜΟΣΥΝΑΣ ΤΟΔΕ ΕΡΓΟΝ";

/// 89 columns by 8 rows.
pub const TABLET_WIDE: [&str; 16] = [
    " ggggggggggggggggggggggggggggfgggggggggggggggggggggggggggggfgggggggggggg ggggggggggggg   ",
    "gggggggggggggggggggggggggggggfgggggggggggggggggggggggggggggfggggggggggggggggggggggggggg  ",
    "gggggggggggggggggggggggggggggfgggggggggggggggggggggggggggggfgggggggggggggggggggggggggggg ",
    "ggggg#ggggg#gg#ggggg#gg#ggggg#gg#ggggg#gggg###gggg#######gg#ggggg#gg#ggggg#gg#ggggg#ggg  ",
    "ggggg##ggg##gg##gggg#gg#ggggg#gg##ggg##ggg#ggg#gggg#gggg#ggf#ggg#ggg##gggg#gg#ggggg#gggg ",
    "ggggg#g#g#g#gg#g#ggg#gg#ggggg#gg#g#g#g#gg#ggggg#gggg#ggggggfg#g#gggg#g#ggg#gg#ggggg#gggg ",
    "ggggg#gg#gg#gg#gg#gg#gg#######gg#gg#gg#gg#ggggg#ggggg#gggggfgg#ggggg#gg#gg#gg#######ggggg",
    "ggggg#ggggg#gg#ggg#g#gg#ggggg#gg#ggggg#gg#ggggg#ggggg#gggggfgg#ggggg#ggg#g#gg#ggggg#ggggg",
    "ggggg#ggggg#gg#gggg##gg#ggggg#gg#ggggg#gg#ggggg#gggg#ggggggfgg#ggggg#gggg##gg#ggggg#gggg ",
    "ggggg#ggggg#gg#ggggg#gg#ggggg#gg#ggggg#ggg#ggg#gggg#gggg#ggfgg#ggggg#ggggg#gg#ggggg#ggggg",
    "ggggg#ggggg#gg#ggggg#gg#ggggg#gg#ggggg#gggg###gggg#######ggfgg#ggggg#ggggg#gg#ggggg#ggggg",
    "gggggggggggggggggggggggggggggfgggggggggggggggggggggggggggggfggggggggggggggggggggggggggg  ",
    "gggggggggggggggggggggggggggggfggggtttttttttttttttttttttggggfgggggggggggggggggggggggggggg ",
    "gggggggggggggggggggggggggggggfggggtttttttttttttttttttttggggfggggggggggggggggggggggggggggg",
    "gggggggggggggggggggggggggggggfgggggggggggggggggggggggggggggfgggggggggggggggggggggggggggg ",
    " ggggggggggggggggg ggggggggggfgggggggggggggggggggggggggg  gfgggggggggggggggggggggggggggg ",
];

/// 79 columns by 8 rows.
pub const TABLET_MED: [&str; 16] = [
    " gggggggggggggggggggggggggfgggggggggggggggggggggggggfgggggggggg gggggggggggg   ",
    "ggggggggggggggggggggggggggfgggggggggggggggggggggggggfgggggggggggggggggggggggg  ",
    "ggggggggggggggggggggggggggfgggggggggggggggggggggggggfggggggggggggggggggggggggg ",
    "gggg#ggggg#g#ggggg#g#ggggg#g#ggggg#ggg###ggg#######g#ggggg#g#ggggg#g#ggggg#gg  ",
    "gggg##ggg##g##gggg#g#ggggg#g##ggg##gg#ggg#ggg#gggg#gf#ggg#gg##gggg#g#ggggg#ggg ",
    "gggg#g#g#g#g#g#ggg#g#ggggg#g#g#g#g#g#ggggg#ggg#gggggfg#g#ggg#g#ggg#g#ggggg#ggg ",
    "gggg#gg#gg#g#gg#gg#g#######g#gg#gg#g#ggggg#gggg#ggggfgg#gggg#gg#gg#g#######gggg",
    "gggg#ggggg#g#ggg#g#g#ggggg#g#ggggg#g#ggggg#gggg#ggggfgg#gggg#ggg#g#g#ggggg#gggg",
    "gggg#ggggg#g#gggg##g#ggggg#g#ggggg#g#ggggg#ggg#gggggfgg#gggg#gggg##g#ggggg#ggg ",
    "gggg#ggggg#g#ggggg#g#ggggg#g#ggggg#gg#ggg#ggg#gggg#gfgg#gggg#ggggg#g#ggggg#gggg",
    "gggg#ggggg#g#ggggg#g#ggggg#g#ggggg#ggg###ggg#######gfgg#gggg#ggggg#g#ggggg#gggg",
    "ggggggggggggggggggggggggggfgggggggggggggggggggggggggfgggggggggggggggggggggggg  ",
    "ggggggggggggggggggggggggggfggtttttttttttttttttttttggfggggggggggggggggggggggggg ",
    "ggggggggggggggggggggggggggfggtttttttttttttttttttttggfgggggggggggggggggggggggggg",
    "ggggggggggggggggggggggggggfgggggggggggggggggggggggggfggggggggggggggggggggggggg ",
    " ggggggggggggggg gggggggggfgggggggggggggggggggggg  gfggggggggggggggggggggggggg ",
];

/// 59 columns by 7 rows.
pub const TABLET_SMALL: [&str; 14] = [
    " ggggggggggggggggggfgggggggggggggggggggfggggggg gggggggg   ",
    "gggggggggggggggggggfgggggggggggggggggggfggggggggggggggggg  ",
    "ggg#ggg#g#ggg#g#ggg#g#ggg#gg###gg#####g#ggg#g#ggg#g#ggg#gg ",
    "ggg##g##g##gg#g#ggg#g##g##g#ggg#gg#gg#gf#g#gg##gg#g#ggg#g  ",
    "ggg#g#g#g#g#g#g#####g#g#g#g#ggg#ggg#gggfg#ggg#g#g#g#####gg ",
    "ggg#ggg#g#gg##g#ggg#g#ggg#g#ggg#ggg#gggfg#ggg#gg##g#ggg#gg ",
    "ggg#ggg#g#ggg#g#ggg#g#ggg#g#ggg#gg#gg#gfg#ggg#ggg#g#ggg#ggg",
    "ggg#ggg#g#ggg#g#ggg#g#ggg#gg###gg#####gfg#ggg#ggg#g#ggg#ggg",
    "gggggggggggggggggggfgggggggggggggggggggfgggggggggggggggggg ",
    "gggggggggggggggggggfgggggggggggggggggggfggggggggggggggggggg",
    "gggggggggggggggggggtttttttttttttttttttttggggggggggggggggggg",
    "gggggggggggggggggggtttttttttttttttttttttggggggggggggggggg  ",
    "gggggggggggggggggggfgggggggggggggggggggfgggggggggggggggggg ",
    " ggggggggggg ggggggfggggggggggggggggg  fgggggggggggggggggg ",
];

/// The name as the tablet spells it.
pub const GREEK: &str = "ΜΝΗΜΟΣΥΝΗ";

/// What one pixel of the leaf is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Px {
    /// Off the edge of the leaf, where the terminal shows through.
    Bare,
    Gold,
    /// Where it was folded to be carried, and flattened out again.
    Fold,
    /// Part of a letter.
    Cut,
    /// Under the line of text, which is set in the terminal's own type.
    Text,
}

pub struct Tablet {
    px: Vec<Vec<Px>>,
    /// Width in columns, which is also width in pixels.
    pub width: usize,
}

impl Tablet {
    fn from(src: &[&str]) -> Tablet {
        let px: Vec<Vec<Px>> = src
            .iter()
            .map(|r| {
                r.chars()
                    .map(|c| match c {
                        'g' => Px::Gold,
                        'f' => Px::Fold,
                        '#' => Px::Cut,
                        't' => Px::Text,
                        _ => Px::Bare,
                    })
                    .collect()
            })
            .collect();
        let width = px.first().map(|r| r.len()).unwrap_or(0);
        Tablet { px, width }
    }

    /// Height in terminal rows: two pixels to a row.
    pub fn height(&self) -> usize {
        self.px.len() / 2
    }

    fn at(&self, x: isize, y: isize) -> Px {
        if x < 0 || y < 0 {
            return Px::Bare;
        }
        self.px
            .get(y as usize)
            .and_then(|r| r.get(x as usize))
            .copied()
            .unwrap_or(Px::Bare)
    }

    /// The colour of one pixel, or None where the leaf is not.
    fn colour(&self, x: usize, y: usize, light: &Light) -> Option<Rgb> {
        let (xi, yi) = (x as isize, y as isize);
        let here = self.at(xi, yi);
        if here == Px::Bare {
            return None;
        }
        let cut = |px: Px, x: usize| px == Px::Cut && (x as f64) < light.cut_to;
        if cut(here, x) {
            // The last few columns cut are still bright from the point
            // that cut them.
            let behind = light.cut_to - x as f64;
            let groove = ramp(0.2);
            return Some(if behind < 3.0 && light.cut_to < self.width as f64 {
                lerp(groove, lift(ramp(1.0), 0.5), 1.0 - behind / 3.0)
            } else {
                groove
            });
        }

        // Light from the upper left, and a broad sheen across it: metal
        // reads as metal by its highlights, not by its hue.
        let w = self.width.max(1) as f64;
        let h = self.px.len().max(1) as f64;
        let u = x as f64 / w * 0.7 + y as f64 / h * 0.3;
        let sheen = (1.0 - (u - 0.3).abs() / 0.22).max(0.0).powi(2);
        // hammered, not polished: a little grain that never moves
        let grain =
            ((x as u64).wrapping_mul(73_856_093) ^ (y as u64).wrapping_mul(19_349_663)) % 1000;
        let grain = grain as f64 / 1000.0 - 0.5;
        let mut c = lift(ramp(0.52 + (1.0 - u) * 0.3 + grain * 0.04), sheen * 0.22);

        if here == Px::Fold {
            c = darken(c, 0.16);
        } else if self.at(xi - 1, yi) == Px::Fold {
            // the far side of a crease catches the light
            c = lift(c, 0.10);
        }
        let bare = |dx: isize, dy: isize| self.at(xi + dx, yi + dy) == Px::Bare;
        if bare(1, 0) || bare(-1, 0) || bare(0, 1) || bare(0, -1) {
            c = darken(c, 0.2);
        }
        // the lower lip of a groove, lit
        if y > 0 && cut(self.at(xi, yi - 1), x) {
            c = lift(c, 0.16);
        }
        if let Some(g) = light.glint {
            let d = (x as f64 + y as f64 * 0.6 - g).abs();
            if d < 9.0 {
                c = lift(c, (1.0 - d / 9.0).powi(2) * 0.55);
            }
        }
        Some(c)
    }

    /// The leaf as terminal rows, lit as `light` says.
    pub fn lines(&self, light: &Light) -> Vec<Line<'static>> {
        let text: Vec<char> = INSCRIPTION.chars().collect();
        (0..self.height())
            .map(|row| {
                let (y0, y1) = (row * 2, row * 2 + 1);
                // where the line of text starts on this row, if it is on it
                let text_x = self.px[y0].iter().position(|p| *p == Px::Text);
                let spans: Vec<Span<'static>> = (0..self.width)
                    .map(|x| {
                        let top = self.colour(x, y0, light);
                        let bottom = self.colour(x, y1, light);
                        if let (Some(t0), Some(t), Some(b)) = (text_x, top, bottom) {
                            let i = x.wrapping_sub(t0);
                            if self.px[y0][x] == Px::Text && i < text.len() {
                                let bg = lerp(t, b, 0.5);
                                // cut as the pass over the letters above reaches it
                                let ch = if (x as f64) < light.cut_to {
                                    text[i]
                                } else {
                                    ' '
                                };
                                return Span::styled(
                                    ch.to_string(),
                                    Style::default().fg(rgb(ramp(0.2))).bg(rgb(bg)),
                                );
                            }
                        }
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

/// How the leaf is lit on one frame.
#[derive(Clone, Copy, Debug)]
pub struct Light {
    /// How many columns of the inscription have been cut. Anything past
    /// this is still smooth gold.
    pub cut_to: f64,
    /// Where the glint is, in columns, if there is one.
    pub glint: Option<f64>,
}

impl Light {
    /// Fully cut and at rest.
    pub const STILL: Light = Light {
        cut_to: f64::INFINITY,
        glint: None,
    };
}

/// The largest tablet that fits in `width` columns, or None when even the
/// smallest would not.
pub fn tablet_for(width: usize) -> Option<Tablet> {
    for art in [&TABLET_WIDE[..], &TABLET_MED[..], &TABLET_SMALL[..]] {
        if width >= art[0].chars().count() {
            return Some(Tablet::from(art));
        }
    }
    None
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

fn darken(c: Rgb, amount: f64) -> Rgb {
    lerp(c, (0, 0, 0), amount.clamp(0.0, 1.0))
}

/// Dim toward the background, for anything meant to recede.
pub fn sink(c: Rgb, amount: f64) -> Rgb {
    lerp(c, (14, 11, 8), amount.clamp(0.0, 1.0))
}

/// Letters for the compact reveal to settle out of: the rest of the
/// alphabet the name is cut in.
pub const NOISE: &[char] = &[
    'Α', 'Β', 'Γ', 'Δ', 'Ε', 'Ζ', 'Θ', 'Ι', 'Κ', 'Λ', 'Ξ', 'Π', 'Ρ', 'Τ', 'Φ', 'Χ', 'Ψ', 'Ω',
];

#[cfg(test)]
mod tests {
    use super::*;

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
    fn every_tablet_is_whole_rows_of_one_width() {
        for art in [&TABLET_WIDE[..], &TABLET_MED[..], &TABLET_SMALL[..]] {
            assert_eq!(art.len() % 2, 0, "half blocks pair the rows");
            let w = art[0].chars().count();
            assert!(art.iter().all(|r| r.chars().count() == w));
        }
    }

    #[test]
    fn the_line_of_text_fits_its_place_on_every_tablet() {
        for art in [&TABLET_WIDE[..], &TABLET_MED[..], &TABLET_SMALL[..]] {
            let t = Tablet::from(art);
            let rows: Vec<usize> = (0..t.px.len())
                .filter(|y| t.px[*y].contains(&Px::Text))
                .collect();
            // one whole terminal row, starting on its top half
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0] % 2, 0);
            let n = t.px[rows[0]].iter().filter(|p| **p == Px::Text).count();
            assert_eq!(n, INSCRIPTION.chars().count());
        }
    }

    #[test]
    fn the_largest_that_fits_is_chosen() {
        assert_eq!(
            tablet_for(200).unwrap().width,
            TABLET_WIDE[0].chars().count()
        );
        assert_eq!(tablet_for(80).unwrap().width, TABLET_MED[0].chars().count());
        assert_eq!(
            tablet_for(60).unwrap().width,
            TABLET_SMALL[0].chars().count()
        );
        assert!(tablet_for(40).is_none());
    }

    #[test]
    fn uncut_letters_are_plain_gold() {
        let t = tablet_for(200).unwrap();
        let blank = Light {
            cut_to: 0.0,
            glint: None,
        };
        let (x, y) = (0..t.px.len())
            .flat_map(|y| (0..t.width).map(move |x| (x, y)))
            .find(|(x, y)| t.px[*y][*x] == Px::Cut)
            .unwrap();
        assert_ne!(t.colour(x, y, &blank), Some(ramp(0.2)));
        assert_eq!(t.colour(x, y, &Light::STILL), Some(ramp(0.2)));
    }

    #[test]
    fn every_row_draws_exactly_its_width() {
        for w in [60, 80, 200] {
            let t = tablet_for(w).unwrap();
            for line in t.lines(&Light::STILL) {
                let cells: usize = line
                    .spans
                    .iter()
                    .map(|s| crate::model::width(&s.content))
                    .sum();
                assert_eq!(cells, t.width);
            }
        }
    }
}

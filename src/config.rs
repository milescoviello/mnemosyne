//! User configuration.
//!
//! Optional: with no file at all the defaults are what the tool has always
//! used. It exists so the palette can be matched to a desktop theme, and the
//! opening animation turned down, without recompiling.
//!
//! Lives next to the rest of the state at `~/.claude/mnemosyne/config.toml`.
//! `mnemosyne --write-config` drops a commented copy of the defaults there.

use serde::{Deserialize, Serialize};

pub const EXAMPLE: &str = r##"# mnemosyne configuration.
# Delete any line to go back to its default.

# The gold ramp, tarnished to fresh. Every gradient in the interface samples
# it: the age of each session, the tablet on the opening screen, the progress
# bar. Two or more stops.
ramp = ["#2b2117", "#5f4524", "#9a7438", "#d4a94f", "#efcf7a", "#fbefc4"]

# Fixed colours. Any of "#rrggbb", or a name: black red green yellow blue
# magenta cyan white, gray, and the bright- variants.
accent = "#86b08e"     # a wsx workspace that is still there
chrome = "#7d7263"     # labels, hints, anything structural
favorite = "#f5c542"
live = "#86b08e"       # running now
tag = "#8aa2e6"        # tags, notes, and what you said
text = "#cbc2b0"       # session titles
bright = "#f5efe3"     # the row you are on
gone = "#d0654b"       # a directory that no longer exists

# Row background for the selected line, and for overlay panels.
band = "#2b2215"
panel = "#120e0a"

[splash]
enabled = true
# Play it on every start, not only when there is indexing to cover. Off, a
# start with the index already built goes straight to the list.
warm = false
# Milliseconds the animation lingers once the index is ready. The warm value
# applies when there was no real work to cover.
floor_cold_ms = 1500
floor_warm_ms = 420

[update]
# Check GitHub for a newer release and install it in the background. The
# running session is never swapped out; a new version takes effect next start.
auto = true
# Hours between checks. 0 is every start, which is the default; nothing waits
# on the check, so the only cost of asking is the request itself.
check_every_hours = 0

[start]
mouse = true           # false is the same as --no-mouse
preview = true         # the bottom rail
subagents = false      # show the ⌁ counts from the start, so → can
                       # expand a session's children without pressing `a` first
"##;

fn d_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Splash {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Play it even when there is no indexing to cover.
    pub warm: bool,
    pub floor_cold_ms: u64,
    pub floor_warm_ms: u64,
}

impl Default for Splash {
    fn default() -> Self {
        Splash {
            enabled: true,
            warm: false,
            floor_cold_ms: 1500,
            floor_warm_ms: 420,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Update {
    #[serde(default = "d_true")]
    pub auto: bool,
    pub check_every_hours: u64,
}

impl Default for Update {
    fn default() -> Self {
        Update {
            auto: true,
            check_every_hours: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Start {
    pub mouse: bool,
    pub preview: bool,
    pub subagents: bool,
}

impl Default for Start {
    fn default() -> Self {
        Start {
            mouse: true,
            preview: true,
            subagents: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub ramp: Option<Vec<String>>,
    pub accent: Option<String>,
    pub chrome: Option<String>,
    pub favorite: Option<String>,
    pub live: Option<String>,
    pub tag: Option<String>,
    pub text: Option<String>,
    pub bright: Option<String>,
    pub gone: Option<String>,
    pub band: Option<String>,
    pub panel: Option<String>,
    pub splash: Splash,
    pub start: Start,
    pub update: Update,
}

pub fn path() -> std::path::PathBuf {
    crate::index::state_dir().join("config.toml")
}

/// The config, read once.
///
/// The browser, the theme and the splash each used to read it for
/// themselves, and a complaint about the file came out once for each --
/// two of them from inside the alternate screen, drawn over the interface
/// where nothing ever cleared them. `main` asks first, before the screen is
/// taken, so whatever it has to say is said on the terminal you can read.
pub fn get() -> &'static Config {
    static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    CONFIG.get_or_init(Config::load)
}

impl Config {
    /// Read the config, or the defaults. What cannot be used is reported and
    /// left out: losing your colours is not worth refusing to start.
    fn load() -> Config {
        let p = path();
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Config::default();
        };
        let (c, problems) = Config::parse(&raw);
        for e in problems {
            eprintln!("mnemosyne: ignoring {e} in {}", p.display());
        }
        c
    }

    /// Everything in `raw` that can be used, and what could not.
    ///
    /// One key at a time. A single bad value used to throw the whole file
    /// away -- so `mouse = "no"` switched `[update] auto = false` back on,
    /// and the next start replaced the binary it was there to keep.
    pub fn parse(raw: &str) -> (Config, Vec<String>) {
        let mut t: toml::Table = match toml::from_str(raw) {
            Ok(t) => t,
            Err(e) => return (Config::default(), vec![e.to_string()]),
        };
        let mut problems = Vec::new();
        for (name, keep) in [
            ("splash", usable::<Splash> as Usable),
            ("start", usable::<Start>),
            ("update", usable::<Update>),
        ] {
            if let Some(toml::Value::Table(sec)) = t.get(name) {
                let kept = keep(sec, &format!("{name}."), &mut problems);
                t.insert(name.into(), toml::Value::Table(kept));
            }
        }
        let t = usable::<Config>(&t, "", &mut problems);
        let c = toml::Value::Table(t).try_into().unwrap_or_default();
        (c, problems)
    }
}

type Usable = fn(&toml::Table, &str, &mut Vec<String>) -> toml::Table;

/// The keys of `t` that `T` accepts on their own, with the rest reported.
fn usable<T: serde::de::DeserializeOwned>(
    t: &toml::Table,
    at: &str,
    problems: &mut Vec<String>,
) -> toml::Table {
    let mut kept = toml::Table::new();
    for (k, v) in t {
        let one = toml::Table::from_iter([(k.clone(), v.clone())]);
        match toml::Value::Table(one).try_into::<T>() {
            Ok(_) => {
                kept.insert(k.clone(), v.clone());
            }
            Err(e) => problems.push(format!("{at}{k} ({})", e.message())),
        }
    }
    kept
}

/// Parse "#rrggbb" or a colour name into RGB.
pub fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        // ASCII first: six bytes can be fewer characters, and slicing at two
        // bytes then cut one in half -- `#aébcd` ended the program.
        if hex.len() == 6 && hex.is_ascii() {
            let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            return Some((c(0)?, c(2)?, c(4)?));
        }
        return None;
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "red" => (205, 49, 49),
        "green" => (13, 188, 121),
        "yellow" => (229, 229, 16),
        "blue" => (36, 114, 200),
        "magenta" => (188, 63, 188),
        "cyan" => (17, 168, 205),
        "white" => (229, 229, 229),
        "gray" | "grey" => (118, 118, 118),
        "bright-black" => (102, 102, 102),
        "bright-red" => (241, 76, 76),
        "bright-green" => (35, 209, 139),
        "bright-yellow" => (245, 245, 67),
        "bright-blue" => (59, 142, 234),
        "bright-magenta" => (214, 112, 214),
        "bright-cyan" => (41, 184, 219),
        "bright-white" => (255, 255, 255),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses_into_the_defaults_shape() {
        let c: Config = toml::from_str(EXAMPLE).expect("shipped example must parse");
        assert_eq!(c.ramp.as_ref().unwrap().len(), 6);
        assert!(c.splash.enabled);
        assert!(c.update.auto);
        assert_eq!(c.update.check_every_hours, 0, "every start by default");
        assert_eq!(c.splash.floor_warm_ms, 420);
        assert!(c.start.mouse);
    }

    #[test]
    fn an_empty_config_is_valid_and_means_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.ramp.is_none());
        assert_eq!(c.splash.floor_cold_ms, 1500);
        assert!(c.start.preview);
        assert!(c.update.auto, "updates are on unless turned off");
    }

    #[test]
    fn partial_configs_keep_the_rest_of_the_defaults() {
        let c: Config =
            toml::from_str("accent = \"#ff0000\"\n[splash]\nenabled = false\n").unwrap();
        assert_eq!(c.accent.as_deref(), Some("#ff0000"));
        assert!(!c.splash.enabled);
        assert_eq!(c.splash.floor_cold_ms, 1500, "untouched keys stay default");
        assert!(c.start.mouse);
    }

    #[test]
    fn one_bad_value_costs_only_itself() {
        let (c, problems) = Config::parse(
            "accent = 7\n[start]\nmouse = \"no\"\npreview = false\n[update]\nauto = false\n",
        );
        assert!(!c.update.auto, "a typo elsewhere turned updates back on");
        assert!(!c.start.preview, "the good key beside the bad one was lost");
        assert!(c.start.mouse, "the bad one falls back to its default");
        assert!(c.accent.is_none());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().any(|p| p.starts_with("start.mouse")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_is_reported_and_defaulted() {
        let (c, problems) = Config::parse("this is = = not toml");
        assert!(c.update.auto);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_colour_with_a_wide_character_is_refused_not_a_crash() {
        assert_eq!(parse_color("#aébcd"), None);
        assert_eq!(parse_color("#１２"), None);
    }

    #[test]
    fn colours_parse_from_hex_and_names() {
        assert_eq!(parse_color("#0e2042"), Some((14, 32, 66)));
        assert_eq!(parse_color("cyan"), Some((17, 168, 205)));
        assert_eq!(parse_color("  yellow "), Some((229, 229, 16)));
        assert_eq!(parse_color("#xyzxyz"), None);
        assert_eq!(parse_color("puce"), None);
    }
}

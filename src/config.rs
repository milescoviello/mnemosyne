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

# The water ramp, deep to pale. Every gradient in the interface samples it:
# the depth gutter, the wordmark, the progress bar. Two or more stops.
ramp = ["#0e2042", "#154884", "#1a7aa8", "#26b2b0", "#6ce2d6", "#e2f8f6"]

# Fixed colours. Any of "#rrggbb", or a name: black red green yellow blue
# magenta cyan white, gray, and the bright- variants.
accent = "cyan"
chrome = "gray"        # borders, labels, anything structural
favorite = "yellow"
live = "green"
tag = "magenta"
text = "gray"          # session titles
bright = "white"       # the row you are on
gone = "#965454"       # a directory that no longer exists

# Row background for the selected line, and for overlay panels.
band = "#162a3a"
panel = "#09111c"

[splash]
enabled = true
# Milliseconds the animation lingers once the index is ready. The warm value
# applies when there was no real work to cover.
floor_cold_ms = 1500
floor_warm_ms = 420

[update]
# Check GitHub for a newer release and install it in the background. The
# running session is never swapped out; a new version takes effect next start.
auto = true
check_every_hours = 24

[start]
mouse = true           # false is the same as --no-mouse
preview = true         # the bottom rail
subagents = false      # reveal subagent transcripts immediately
"##;

fn d_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Splash {
    #[serde(default = "d_true")]
    pub enabled: bool,
    pub floor_cold_ms: u64,
    pub floor_warm_ms: u64,
}

impl Default for Splash {
    fn default() -> Self {
        Splash {
            enabled: true,
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
            check_every_hours: 24,
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

impl Config {
    /// Read the config, or the defaults. A malformed file is reported once
    /// and then ignored: losing your colours is not worth refusing to start.
    pub fn load() -> Config {
        let p = path();
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Config::default();
        };
        match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("mnemosyne: ignoring {}: {e}", p.display());
                Config::default()
            }
        }
    }
}

/// Parse "#rrggbb" or a colour name into RGB.
pub fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
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
        assert_eq!(c.update.check_every_hours, 24);
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
    fn colours_parse_from_hex_and_names() {
        assert_eq!(parse_color("#0e2042"), Some((14, 32, 66)));
        assert_eq!(parse_color("cyan"), Some((17, 168, 205)));
        assert_eq!(parse_color("  yellow "), Some((229, 229, 16)));
        assert_eq!(parse_color("#xyzxyz"), None);
        assert_eq!(parse_color("puce"), None);
    }
}

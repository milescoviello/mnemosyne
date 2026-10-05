//! The `/` filter: which sessions a few typed words pick out, and in what
//! order.
//!
//! It was one fuzzy match over every field joined into a single line --
//! title, folder, branch, tags, the last prompt and the id -- and a fuzzy
//! match only asks that the letters turn up in order, however far apart.
//! Over a long last prompt and a hex id they nearly always do: `/nvenc`
//! kept 123 of 436 sessions here when not one of them said "nvenc"
//! anywhere it looked, and `/ci` kept 343.
//!
//! So each word is matched against each field on its own, the way that
//! field can sensibly be matched, and has to be found inside one of them:
//!
//! - titles, folders, branches and tags are short and named by people, so
//!   they match fuzzily -- but only when the letters sit close together,
//!   and a word of three letters or fewer has to be there as written;
//! - the last prompt and your note are prose, where letters in order mean
//!   nothing, so a word has to be in them as written;
//! - an id matches only from the start, or as a long run of it, which is
//!   what pasting one from a log looks like.
//!
//! The fzf marks still work, since they are explicit: `'exact`, `^start`,
//! `end$` and `!not`.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

/// What a piece of a session is, which decides how loosely a word may
/// match it and how much a match there counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Title,
    Folder,
    Tags,
    Branch,
    Prompt,
    /// The note you attached with `N`.
    Note,
    Id,
}

impl Field {
    /// A match in the title says the most about what a session was;
    /// one in the last prompt, which is a single line of a long
    /// conversation, the least.
    fn weight(self) -> u32 {
        match self {
            Field::Title => 4,
            Field::Folder | Field::Tags | Field::Note => 3,
            Field::Branch | Field::Id => 2,
            Field::Prompt => 1,
        }
    }
}

/// A word of the filter, with what it takes to be found as written.
struct Word {
    atom: Atom,
    /// The same needle as a plain substring, for the fields where letters
    /// strewn in order are not a match.
    literal: Atom,
    /// How many characters it has.
    len: usize,
    /// Whether it could be part of an id: hex digits and hyphens.
    hexish: bool,
}

pub struct Filter {
    words: Vec<Word>,
}

impl Filter {
    pub fn new(query: &str) -> Filter {
        let words = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart)
            .atoms
            .into_iter()
            .map(|atom| {
                let needle: String = atom.needle_text().chars().collect();
                let literal = Atom::new(
                    &needle,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Substring,
                    false,
                );
                Word {
                    len: needle.chars().count(),
                    hexish: needle.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                    atom,
                    literal,
                }
            })
            .collect();
        Filter { words }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// How well a session's fields answer the filter, or `None` when they
    /// do not: every word must be found inside one field, and no word
    /// marked `!` may be found in any.
    pub fn score(&self, fields: &[(Field, &str)], m: &mut Matcher) -> Option<u32> {
        let mut buf = Vec::new();
        let mut idx = Vec::new();
        let mut total = 0u32;
        for w in &self.words {
            let mut best: Option<u32> = None;
            for &(field, text) in fields {
                if text.is_empty() {
                    continue;
                }
                let hay = chars(text, &mut buf);
                if let Some(s) = w.found(field, hay, m, &mut idx) {
                    let s = s as u32 * field.weight();
                    best = Some(best.map_or(s, |b| b.max(s)));
                }
            }
            if w.atom.negative {
                if best.is_some() {
                    return None;
                }
            } else {
                total += best?;
            }
        }
        Some(total)
    }

    /// Which characters of `text` the filter matched, by position, to draw
    /// them out. Asked of the text as it is drawn, cut to its column, so
    /// what is marked is what is there.
    pub fn positions(&self, field: Field, text: &str, m: &mut Matcher) -> Vec<usize> {
        if self.words.is_empty() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let mut idx = Vec::new();
        let mut out: Vec<u32> = Vec::new();
        for w in self.words.iter().filter(|w| !w.atom.negative) {
            if w.found(field, chars(text, &mut buf), m, &mut idx).is_some() {
                out.extend(idx.iter().copied());
            }
        }
        out.sort_unstable();
        out.dedup();
        out.into_iter().map(|i| i as usize).collect()
    }
}

impl Word {
    /// Whether this word is in `hay`, read as a `field`, and how well.
    /// Fills `idx` with the positions it matched at.
    fn found(
        &self,
        field: Field,
        hay: Utf32Str,
        m: &mut Matcher,
        idx: &mut Vec<u32>,
    ) -> Option<u16> {
        idx.clear();
        if field == Field::Id {
            // From the start, or a run long enough not to turn up by chance
            // in thirty-two hex digits: `cafe` is in one id in sixty-five
            // thousand, `bad` in one in a hundred.
            if !self.hexish || self.atom.negative {
                return None;
            }
            let s = self.literal.indices(hay, m, idx)?;
            let from_start = idx.first() == Some(&0);
            return ((from_start && self.len >= 4) || self.len >= 8).then_some(s);
        }
        if self.atom.negative {
            // `!word` keeps out what has it as written, wherever it is.
            return self.atom.score(hay, m).is_none().then_some(0);
        }
        if self.atom.kind != AtomKind::Fuzzy {
            return self.atom.indices(hay, m, idx);
        }
        if matches!(field, Field::Prompt | Field::Note) || self.len <= 3 {
            let s = self.literal.indices(hay, m, idx)?;
            // Two letters as written are in most sentences; at the start of
            // a word they are what was meant -- `ci`, not "decision".
            if self.len <= 2 && !starts_word(hay, idx.first().copied()) {
                return self.first_word_start(hay, m, idx);
            }
            return Some(s);
        }
        let s = self.atom.indices(hay, m, idx)?;
        // The word abbreviated, and no further: letters left out here and
        // there, `redsgn` for redesign, or the starts of words, `mnbh` for
        // mn-bug-hunt. `rust` is not in "ruleset", nor `fish` in
        // "refurbished".
        if tight(idx, self.len) || by_word_starts(hay, idx, self.len) {
            return Some(s);
        }
        // The letters are too far apart, but the word may still be in
        // there as written.
        self.literal.indices(hay, m, idx)
    }

    /// The first place a short word starts a word of `hay`, as written.
    fn first_word_start(&self, hay: Utf32Str, m: &mut Matcher, idx: &mut Vec<u32>) -> Option<u16> {
        let n = hay.len();
        for from in 1..n.saturating_sub(self.len.saturating_sub(1)) {
            if !starts_word(hay, Some(from as u32)) {
                continue;
            }
            idx.clear();
            // Only as much as the word is long: searched to the end from
            // every word, a long note cost the square of its length.
            let here = hay.slice(from..from + self.len);
            if let Some(s) = self.literal.indices(here, m, idx) {
                if idx.first() == Some(&0) {
                    for i in idx.iter_mut() {
                        *i += from as u32;
                    }
                    return Some(s);
                }
            }
        }
        idx.clear();
        None
    }
}

/// `text` as the matcher reads it, one character to a position.
///
/// nucleo's own conversion counts grapheme clusters, and where those all
/// begin with ASCII but the text is not ASCII -- an accent written as its
/// own mark -- it counts bytes. The list draws by character, so a title
/// with an emoji or an accent in it had the letters after it marked one or
/// two places along: `⚙️ zpool rebuild` marked " zpoo".
fn chars<'a>(text: &'a str, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    if text.is_ascii() {
        return Utf32Str::Ascii(text.as_bytes());
    }
    buf.clear();
    buf.extend(text.chars());
    Utf32Str::Unicode(buf)
}

/// Whether the matched letters leave out no more than half as many again.
fn tight(idx: &[u32], len: usize) -> bool {
    match (idx.first(), idx.last()) {
        (Some(a), Some(b)) => ((b - a + 1) as usize) <= len + len / 2,
        _ => false,
    }
}

/// Whether every run of matched letters begins a word, and the words are
/// near each other: `fish` is not "Wi**Fi** and wireless **sh**ell".
fn by_word_starts(hay: Utf32Str, idx: &[u32], len: usize) -> bool {
    let near = match (idx.first(), idx.last()) {
        (Some(a), Some(b)) => ((b - a + 1) as usize) <= len * 3,
        _ => false,
    };
    near && idx
        .iter()
        .enumerate()
        .all(|(k, &at)| (k > 0 && idx[k - 1] + 1 == at) || starts_word(hay, Some(at)))
}

/// Whether position `at` of `hay` begins a word.
fn starts_word(hay: Utf32Str, at: Option<u32>) -> bool {
    let Some(at) = at else { return false };
    if at == 0 {
        return true;
    }
    let before = hay.get(at - 1);
    let here = hay.get(at);
    !before.is_alphanumeric() || (before.is_lowercase() && here.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo_matcher::Config;

    fn finds(query: &str, fields: &[(Field, &str)]) -> bool {
        Filter::new(query)
            .score(fields, &mut Matcher::new(Config::DEFAULT))
            .is_some()
    }

    #[test]
    fn a_word_is_not_spelt_out_of_letters_strewn_across_fields() {
        // n from the title, v from the folder, e, n and c from the prompt
        let s = [
            (Field::Title, "Tune the network"),
            (Field::Folder, "~/dev"),
            (Field::Prompt, "then check the logs once more"),
            (Field::Id, "c3b6e0c7-bc4c-4946-b8c9-d11160d7765e"),
        ];
        assert!(!finds("nvenc", &s));
        assert!(finds("network", &s));
    }

    #[test]
    fn letters_far_apart_in_one_title_are_not_a_match() {
        let t = [(Field::Title, "Unveil the newest fancy cat")];
        assert!(!finds("nvenc", &t), "n…v…e…n…c across a sentence");
        assert!(finds("fancy", &t));
        // where they start the words, they spell what was meant
        assert!(finds("nvenc", &[(Field::Title, "New video encoder")]));
    }

    #[test]
    fn a_word_abbreviated_still_finds_its_title() {
        assert!(finds(
            "mnbh",
            &[(Field::Folder, "wsx mnemosyne/mn-bug-hunt")]
        ));
        assert!(finds("redsgn", &[(Field::Title, "Gold leaf redesign")]));
    }

    #[test]
    fn a_word_is_not_found_inside_a_longer_one_by_its_letters() {
        assert!(!finds("rust", &[(Field::Title, "VM hardening ruleset")]));
        assert!(!finds("fish", &[(Field::Title, "a refurbished switch")]));
        assert!(!finds(
            "fish",
            &[(Field::Title, "Configure WiFi and wireless shell")]
        ));
        // but as written, it is
        assert!(finds("rust", &[(Field::Title, "a matter of trust")]));
    }

    #[test]
    fn the_last_prompt_has_to_say_the_word() {
        let p = [(Field::Prompt, "no, verify each change in context first")];
        assert!(!finds("nvenc", &p));
        assert!(finds("verify", &p));
        assert!(finds("CONTEXT", &p), "case does not matter");
    }

    #[test]
    fn two_letters_match_where_a_word_starts() {
        let t = [(Field::Title, "Make the decision about CI runners")];
        assert!(finds("ci", &t));
        assert!(!finds("ci", &[(Field::Title, "A specific decision")]));
        // and the place it starts is what is drawn out
        let at = Filter::new("ci").positions(
            Field::Title,
            "Make the decision about CI runners",
            &mut Matcher::new(Config::DEFAULT),
        );
        assert_eq!(at, vec![24, 25]);
    }

    #[test]
    fn what_is_marked_is_counted_by_character() {
        let at = |q: &str, t: &str| -> String {
            let f = Filter::new(q);
            let marks = f.positions(Field::Title, t, &mut Matcher::new(Config::DEFAULT));
            t.chars()
                .enumerate()
                .filter(|(i, _)| marks.contains(i))
                .map(|(_, c)| c)
                .collect()
        };
        assert_eq!(at("zpool", "⚙\u{fe0f} zpool rebuild"), "zpool");
        assert_eq!(at("zpool", "e\u{301}te zpool"), "zpool");
        assert_eq!(at("zpool", "👨\u{200d}👩\u{200d}👧 zpool"), "zpool");
    }

    #[test]
    fn three_letters_must_be_there_as_written() {
        assert!(finds("zfs", &[(Field::Title, "Scrub the ZFS pool")]));
        assert!(!finds("zfs", &[(Field::Title, "zones for system")]));
    }

    #[test]
    fn an_id_matches_from_its_start_or_as_a_long_run() {
        let id = [(Field::Id, "c3b6e0c7-bc4c-4946-b8c9-d11160d7765e")];
        assert!(finds("c3b6", &id));
        assert!(finds("c3b6e0c7-bc4c-4946-b8c9-d11160d7765e", &id));
        assert!(finds("d11160d7", &id));
        assert!(!finds("bc4c", &id), "a short run from the middle");
        assert!(!finds("c3b", &id), "too short to mean this one");
        assert!(!finds("bead", &id));
    }

    #[test]
    fn every_word_has_to_be_found_but_each_in_any_field() {
        let s = [
            (Field::Title, "Scrub the ZFS pool"),
            (Field::Folder, "~/homelab"),
        ];
        assert!(finds("zfs homelab", &s));
        assert!(!finds("zfs kernel", &s));
    }

    #[test]
    fn the_fzf_marks_still_work() {
        let s = [
            (Field::Title, "Scrub the ZFS pool"),
            (Field::Folder, "~/homelab"),
        ];
        assert!(!finds("zfs !homelab", &s), "!word keeps it out");
        assert!(finds("zfs !kernel", &s));
        assert!(finds("^scrub", &s));
        assert!(!finds("^pool", &s));
        assert!(finds("pool$", &s));
        assert!(finds("'zfs", &s));
    }

    #[test]
    fn a_title_match_outranks_one_in_the_prompt() {
        let f = Filter::new("kernel");
        let mut m = Matcher::new(Config::DEFAULT);
        let title = f.score(&[(Field::Title, "Kernel config")], &mut m).unwrap();
        let prompt = f
            .score(&[(Field::Prompt, "rebuild the kernel")], &mut m)
            .unwrap();
        assert!(title > prompt, "{title} vs {prompt}");
    }

    #[test]
    fn an_empty_filter_is_empty() {
        assert!(Filter::new("   ").is_empty());
        assert!(!Filter::new("x").is_empty());
    }
}

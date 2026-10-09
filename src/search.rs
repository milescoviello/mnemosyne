//! Deep search across transcript bodies.
//!
//! Fuzzy matching on titles is done in memory by the UI; this module is for the
//! expensive kind -- "which session did I fix the NVENC thing in" -- which has
//! to read the actual conversations. A parallel byte scan over the corpus lands
//! around two tenths of a second, so it runs on a background thread with the
//! results folded in when they arrive.

use crate::model::{Harness, Session};
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Anything, anywhere in the conversation.
    Content,
    /// Files the session actually read or edited (tool inputs, not mentions).
    File,
    /// Sessions that invoked a given tool.
    Tool,
    /// Every byte of the conversation, including tool output. Slower, and the
    /// only mode that reads the ~98% of a transcript the index leaves out.
    Everything,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Content => "content",
            Mode::File => "file touched",
            Mode::Tool => "tool used",
            Mode::Everything => "everything, slow",
        }
    }
    pub fn next(self) -> Mode {
        match self {
            Mode::Content => Mode::File,
            Mode::File => Mode::Tool,
            Mode::Tool => Mode::Everything,
            Mode::Everything => Mode::Content,
        }
    }
}

/// A session a search found: why, and how well.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    /// The words around the match. Empty for an indexed hit until the row
    /// is looked at, when it is cut from the stored prose.
    pub excerpt: String,
    /// How well it answers the question, higher better. Zero where there
    /// is no telling -- the exhaustive scan stops at the first match -- and
    /// then the list's own order decides.
    pub score: f64,
}

impl Hit {
    pub fn new(excerpt: String) -> Hit {
        Hit {
            excerpt,
            score: 0.0,
        }
    }
}

/// Path to hit, for what a search found.
pub type Hits = HashMap<String, Hit>;

/// ASCII case-insensitive substring search, leftmost match first. `needle`
/// must already be lowercase.
///
/// Candidates come from memchr on both cases of one byte of the needle,
/// which keeps this close to a plain memmem without lowercasing the
/// haystack. The byte is the needle's rarest, not its first: anchored on
/// the first, `checkpatch` stopped at every `c` in 2.8GB of transcripts.
pub(crate) fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let k = rarest(needle);
    let lo = needle[k];
    let up = lo.to_ascii_uppercase();
    // where the anchor byte can sit for a match to fit
    let (mut from, end) = (k, hay.len() - needle.len() + k + 1);
    while from < end {
        let rel = if lo == up {
            memchr::memchr(lo, &hay[from..end])?
        } else {
            memchr::memchr2(lo, up, &hay[from..end])?
        };
        let at = from + rel - k;
        if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        from += rel + 1;
    }
    None
}

/// The position of the byte in `needle` least likely to turn up by chance
/// in a transcript: English letters by how rarely they are used, anything
/// outside ASCII rarer still, and spaces and JSON's punctuation -- on every
/// line -- the least useful of all.
fn rarest(needle: &[u8]) -> usize {
    const COMMON_FIRST: &[u8] = b"etaoinshrdlcumwfgypbvkjxqz";
    let rarity = |b: u8| -> i32 {
        match b {
            b' ' | b'"' | b':' | b',' | b'{' | b'}' | b'\\' => 0,
            b'a'..=b'z' => 10 + COMMON_FIRST.iter().position(|c| *c == b).unwrap_or(0) as i32,
            b'0'..=b'9' => 30,
            0x80.. => 40,
            _ => 20,
        }
    };
    (0..needle.len())
        .max_by_key(|&i| (rarity(needle[i]), std::cmp::Reverse(i)))
        .unwrap_or(0)
}

/// Turn a raw JSON line into something readable around the hit.
fn snippet(line: &[u8], at: usize) -> String {
    const PAD: usize = 90;
    let mut lo = at.saturating_sub(PAD);
    let mut hi = (at + PAD).min(line.len());
    // Whole characters only. Cut mid-way, the pieces decode as U+FFFD, and a
    // hit in Japanese text came out as `…��本語…`.
    let continues = |i: usize| i < line.len() && line[i] & 0xC0 == 0x80;
    while lo < at && continues(lo) {
        lo += 1;
    }
    while continues(hi) {
        hi += 1;
    }
    let raw = String::from_utf8_lossy(&line[lo..hi]);
    let cleaned: String = raw
        .replace("\\n", " ")
        .replace("\\\"", "\"")
        .replace("\\t", " ");
    let mut out = crate::scan::squash(&cleaned, 200);
    if lo > 0 {
        out.insert(0, '…');
    }
    if hi < line.len() {
        out.push('…');
    }
    out
}

/// Spans of injected context inside a line, as byte ranges.
///
/// Every session gets the user's memory index and other `<system-reminder>`
/// blocks pasted into its context. Those are not things that happened in the
/// conversation, so a naive substring search reports every session that ever
/// ran as a match for any word in them. Matches landing inside these spans are
/// ignored.
fn injected_spans(line: &[u8]) -> Vec<(usize, usize)> {
    const OPEN: &[u8] = b"<system-reminder>";
    const CLOSE: &[u8] = b"</system-reminder>";
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = memmem::find(&line[from..], OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let end = match memmem::find(&line[after..], CLOSE) {
            Some(r) => after + r + CLOSE.len(),
            None => line.len(),
        };
        spans.push((start, end));
        from = end;
    }
    spans
}

/// Is this line something that was actually said, rather than machinery?
///
/// Uses the same classification verified in `scan`: metadata lines begin
/// literally with `{"type":"`, and real turns carry `"role":"user"` or
/// `"role":"assistant"`. This is what keeps `attachment` lines -- which is
/// where the injected memory index lives -- out of content search.
#[cfg(test)]
fn is_conversation(line: &[u8]) -> bool {
    is_conversation_of(Harness::Claude, line)
}

/// The same, for whichever agent wrote the line.
///
/// pi's and omp's lines all begin `{"type":"` -- a message is
/// `{"type":"message"` -- so Claude's test turned every one of them away.
/// What a tool handed back is left out, as Claude's tool results are. A
/// Codex line is a message to or from the model, or a command it ran, and
/// not the AGENTS.md and environment it sends along as messages of their
/// own.
fn is_conversation_of(h: Harness, line: &[u8]) -> bool {
    let f = &line[..line.len().min(1 << 16)];
    let has = |needle: &[u8]| memmem::find(f, needle).is_some();
    match h {
        Harness::Claude => claude_conversation(line),
        Harness::Pi | Harness::Omp => {
            line.starts_with(b"{\"type\":\"message\"")
                && matches!(
                    crate::pi::role_of(f),
                    Some("user" | "assistant" | "bashExecution")
                )
        }
        Harness::Codex => {
            if has(br#""payload":{"type":"function_call""#) {
                return true;
            }
            if !has(br#""payload":{"type":"message""#) {
                return false;
            }
            if has(br#""role":"assistant""#) {
                return true;
            }
            has(br#""role":"user""#)
                && !has(br#""text":"<"#)
                && !has(br##""text":"# AGENTS.md instructions"##)
        }
        // Rows of a database, read as they are: see `search_hermes`.
        Harness::Hermes => true,
    }
}

fn claude_conversation(line: &[u8]) -> bool {
    if line.starts_with(b"{\"type\":\"") {
        return false;
    }
    // The same predicate the indexer uses. These were two separate copies of
    // the idea and they drifted: the index harvested injected memory blobs
    // that the scan skipped, so one query gave two answers.
    if crate::scan::is_injected_meta(line) {
        return false;
    }
    memmem::find(line, b"\"role\":\"user\"").is_some()
        || memmem::find(line, b"\"role\":\"assistant\"").is_some()
}

/// Is the match sitting inside a base64 blob rather than prose?
///
/// Pasted images and binary tool payloads arrive as long unbroken base64, and
/// its alphabet happily spells short words like "nvenc" by chance. So a match
/// that is part of an unbroken run of base64 characters a hundred long is
/// taken for one.
///
/// It was a window of seventy bytes either side with no space or
/// punctuation in it -- which near either end of a blob reached the quote or
/// the space outside it, and let `c++` and `xYz` match inside images. And
/// nothing outside ASCII is base64, so Japanese and Chinese, which go a long
/// way without an ASCII space, are never taken for it.
fn looks_like_blob(line: &[u8], at: usize, len: usize) -> bool {
    const RUN: usize = 100;
    let b64 = |c: &u8| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=');
    let end = (at + len).min(line.len());
    if !line[at..end].iter().all(b64) {
        return false;
    }
    let before = line[..at]
        .iter()
        .rev()
        .take(RUN)
        .take_while(|c| b64(c))
        .count();
    let after = line[end..].iter().take(RUN).take_while(|c| b64(c)).count();
    before + (end - at) + after >= RUN
}

/// Is a match inside the value of one of the transcript's own bookkeeping
/// fields -- the folder, the branch, the version, ids and types?
///
/// Every line carries them, so when the index had no answer and the scan
/// ran, `main` matched every session on a branch called main, `2.1.0` every
/// session from that version, and any word in a folder's path everything
/// run there -- each with raw JSON for an excerpt.
fn in_bookkeeping(line: &[u8], at: usize) -> bool {
    const KEYS: &[&[u8]] = &[
        b"cwd",
        b"gitBranch",
        b"version",
        b"userType",
        b"entrypoint",
        b"sessionId",
        b"session_id",
        b"uuid",
        b"parentUuid",
        b"leafUuid",
        b"promptId",
        b"requestId",
        b"timestamp",
        b"agentId",
        b"messageId",
        b"sourceToolAssistantUUID",
        b"tool_use_id",
        b"toolUseID",
        b"id",
        b"type",
        b"role",
        b"model",
        b"stop_reason",
        b"permissionMode",
        b"origin",
        b"promptSource",
        b"subtype",
        b"slug",
    ];
    const REACH: usize = 512;
    let back = &line[at.saturating_sub(REACH)..at];
    let Some(q) = back.iter().rposition(|c| *c == b'"') else {
        return false;
    };
    let open = at - back.len() + q;
    // `"key":"` just before the value's opening quote
    if open < 3 || line[open - 1] != b':' || line[open - 2] != b'"' || line[open - 3] == b'\\' {
        return false;
    }
    let key_end = open - 2;
    let key_back = &line[key_end.saturating_sub(40)..key_end];
    let Some(k) = key_back.iter().rposition(|c| *c == b'"') else {
        return false;
    };
    let key = &key_back[k + 1..];
    KEYS.contains(&key)
}

/// Is a match inside one of the JSON's own keys rather than a value?
///
/// The keys are the transcript's scaffolding -- `parentUuid`, `role`,
/// `timestamp` -- and every line has them, so a search for one that the
/// index had no answer for fell back to this and matched every session.
///
/// Keys are short, so it looks no further than that either side: a common
/// word in a long value would otherwise walk back to the value's start for
/// every one of its matches.
fn in_key(line: &[u8], at: usize, len: usize) -> bool {
    const KEY_MAX: usize = 64;
    let back = &line[at.saturating_sub(KEY_MAX)..at];
    let Some(open) = back.iter().rposition(|c| *c == b'"') else {
        return false;
    };
    let open = at - back.len() + open;
    let ahead = &line[at + len..(at + len + KEY_MAX).min(line.len())];
    let Some(rel) = memchr::memchr(b'"', ahead) else {
        return false;
    };
    let close = at + len + rel;
    // a quote with a backslash before it is prose inside a value
    let escaped = |i: usize| i > 0 && line[i - 1] == b'\\';
    !escaped(open)
        && !escaped(close)
        && line.get(close + 1) == Some(&b':')
        && line[open + 1..close]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

/// One line as the scan searches it: as written, and lowercased so the
/// needle can be found with a plain SIMD memmem. That is twice as fast as a
/// case-insensitive search, which stops at every place the needle might
/// start. Lowercasing ASCII moves no byte, so a position in one is the same
/// position in the other.
struct Line<'a> {
    raw: &'a [u8],
    low: &'a [u8],
}

/// First occurrence of `needle` that is real conversation, not injected context.
fn content_hit(l: &Line, needle: &memmem::Finder, h: Harness) -> Option<usize> {
    let line = l.raw;
    let first = needle.find(l.low)?;
    if !is_conversation_of(h, line) {
        return None;
    }
    let len = needle.needle().len();
    let spans = injected_spans(line);
    let bad = |at: usize| {
        spans.iter().any(|(a, b)| at >= *a && at < *b)
            || looks_like_blob(line, at, len)
            || in_key(line, at, len)
            || in_bookkeeping(line, at)
    };
    if !bad(first) {
        return Some(first);
    }
    let mut at = first;
    loop {
        at = at + 1 + needle.find(&l.low[at + 1..])?;
        if !bad(at) {
            return Some(at);
        }
    }
}

/// Does this line record a tool actually touching `needle` as a path?
///
/// Only the values are lowercased, not the line: the keys are found with a
/// plain memmem over it as written, and a path is short.
fn file_hit(line: &[u8], needle: &memmem::Finder) -> Option<usize> {
    for key in [
        &b"\"file_path\":\""[..],
        &b"\"notebook_path\":\""[..],
        &b"\"path\":\""[..],
    ] {
        let mut from = 0;
        while let Some(rel) = memmem::find(&line[from..], key) {
            let vs = from + rel + key.len();
            if let Some(end) = memchr::memchr(b'"', &line[vs..]) {
                if needle
                    .find(&line[vs..vs + end].to_ascii_lowercase())
                    .is_some()
                {
                    return Some(vs);
                }
                from = vs + end;
            } else {
                break;
            }
        }
    }
    None
}

fn tool_hit(line: &[u8], needle: &memmem::Finder) -> Option<usize> {
    // tool_use blocks look like {"type":"tool_use","id":...,"name":"Edit",...}
    let mut from = 0;
    let key = &b"\"name\":\""[..];
    let has_tool = memmem::find(line, b"\"tool_use\"").is_some();
    if !has_tool {
        return None;
    }
    while let Some(rel) = memmem::find(&line[from..], key) {
        let vs = from + rel + key.len();
        if let Some(end) = memchr::memchr(b'"', &line[vs..]) {
            if needle
                .find(&line[vs..vs + end].to_ascii_lowercase())
                .is_some()
            {
                return Some(vs);
            }
            from = vs + end;
        } else {
            break;
        }
    }
    None
}

/// One thing a content search looks for, as it was typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// A word, found wherever a word starts with it, or with its root.
    Word(String),
    /// What was put in quotes, found as written: together, in order.
    Phrase(String),
}

impl Term {
    pub fn text(&self) -> &str {
        match self {
            Term::Word(w) | Term::Phrase(w) => w,
        }
    }

    /// Whether the index has anything to match it by: FTS5 refuses a
    /// phrase its tokenizer makes nothing of.
    fn tokens(&self) -> bool {
        self.text().chars().any(char::is_alphanumeric)
    }

    /// This term as FTS5 reads it. Quotes are doubled, so nothing typed can
    /// be read as syntax.
    fn fts(&self) -> String {
        let quoted = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        match self {
            // The trailing `*` makes the last word a prefix, so "connection
            // pool" finds "connection pooling" and "page fault" "page
            // faults", as the single words always did.
            Term::Phrase(p) => format!("{}*", quoted(&p.to_lowercase())),
            Term::Word(w) => {
                let w = w.to_lowercase();
                // `a*` is every word with an a in front, which is most of
                // them, and costs FTS5 four tenths of a second to gather.
                let star = if w.chars().count() >= 3 { "*" } else { "" };
                match root(&w) {
                    Some(r) => format!("({}{star} OR {}*)", quoted(&w), quoted(&r)),
                    None => format!("{}{star}", quoted(&w)),
                }
            }
        }
    }
}

/// A content search, read the way it is meant: every word, in any order.
///
/// Several words were one phrase, so they had to sit side by side in the
/// order typed. Given two words of a session's own title, the index found
/// that session one time in four -- the words were in it, just not next to
/// each other -- and given the whole title, one time in eleven. Put in
/// quotes, words are still a phrase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
}

impl Query {
    pub fn parse(query: &str) -> Query {
        let is_quote = |c: char| matches!(c, '"' | '“' | '”');
        let mut terms = Vec::new();
        let mut rest = query;
        loop {
            let (words, quoted) = match rest.find(is_quote) {
                Some(i) => (&rest[..i], Some(&rest[i..])),
                None => (rest, None),
            };
            terms.extend(words.split_whitespace().map(|w| Term::Word(w.to_string())));
            let Some(quoted) = quoted else { break };
            let inside = quoted.trim_start_matches(is_quote);
            let (inside, after) = match inside.find(is_quote) {
                Some(j) => (&inside[..j], inside[j..].trim_start_matches(is_quote)),
                None => (inside, ""),
            };
            let phrase = inside.split_whitespace().collect::<Vec<_>>().join(" ");
            if !phrase.is_empty() {
                terms.push(Term::Phrase(phrase));
            }
            rest = after;
        }
        // Nothing to look for in a word that is all sentence punctuation.
        // One of symbols is kept -- `->`, `::` -- and is found as written:
        // dropped, `C++ ->` lost its arrow, and `->` alone had nothing left
        // to ask the prose for and read every transcript instead.
        terms.retain(|t| t.text().chars().any(|c| c.is_alphanumeric() || telling(c)));
        Query { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The FTS5 expression asking for any of the terms.
    pub fn fts_any(&self) -> String {
        self.terms
            .iter()
            .filter(|t| t.tokens())
            .map(Term::fts)
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    /// The FTS5 expression asking for every term.
    pub fn fts(&self) -> String {
        self.terms
            .iter()
            .filter(|t| t.tokens())
            .map(Term::fts)
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// Each term written out, for looking for literally: as typed, and
    /// lowercased in full when that is different. A capital outside ASCII
    /// has a lowercase that folding ASCII cannot reach.
    pub fn literals(&self) -> Vec<Vec<String>> {
        self.terms
            .iter()
            .map(|t| {
                let typed = t.text().to_string();
                let lower = typed.to_lowercase();
                if lower == typed.to_ascii_lowercase() {
                    vec![typed]
                } else {
                    vec![typed, lower]
                }
            })
            .collect()
    }
}

/// The FTS5 expression for what was typed: every word, in any order.
#[cfg(test)]
pub fn fts_expr(query: &str) -> String {
    Query::parse(query).fts()
}

/// The start a word's other forms share, when it has one worth asking for:
/// `install` for "installation", `debug` for "debugging", `queri` for
/// "query".
///
/// A session's title is often put in other words than were used in it --
/// "installation" where the conversation said "install", "optimization"
/// where it said "optimize" -- and asked for such a word, the search found
/// nothing. Stemming the whole index finds them but buries the sessions that
/// used the very word typed under the ones that used its relatives; asking
/// for the word *or* its root keeps those first, since they match twice.
///
/// Only plain English words are cut, and never shorter than four letters:
/// `fix` would be every fixture.
fn root(word: &str) -> Option<String> {
    if !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    // Longest first. What each leaves behind is a start the other forms
    // share, not a word: `optimiz` covers optimize, optimized, optimizing.
    const CUTS: &[(&str, &str)] = &[
        ("izations", "iz"),
        ("ization", "iz"),
        ("ations", ""),
        ("ation", ""),
        ("nesses", ""),
        ("ments", ""),
        ("ities", ""),
        ("ness", ""),
        ("ment", ""),
        ("ings", ""),
        ("ity", ""),
        ("ies", "y"),
        ("ied", "y"),
        ("ing", ""),
        ("ed", ""),
        ("ly", ""),
        ("es", ""),
        ("s", ""),
        ("e", ""),
        ("y", "i"),
    ];
    for (cut, put) in CUTS {
        let Some(stem) = word.strip_suffix(cut) else {
            continue;
        };
        let ok = match *cut {
            // `-es` is its own ending only after a hiss: patches, boxes.
            // Otherwise it is an `e` and an `s`, which the next rules take.
            "es" => ["ch", "sh", "ss", "x", "z"]
                .iter()
                .any(|h| stem.ends_with(h)),
            // status, analysis and class are not plurals
            "s" => !["s", "u", "i"].iter().any(|h| stem.ends_with(h)),
            // a vowel before it: key, play
            "y" => !stem.ends_with(['a', 'e', 'i', 'o', 'u']),
            _ => true,
        };
        if !ok {
            continue;
        }
        if stem.len() < 4 {
            return None;
        }
        let mut r = format!("{stem}{put}");
        // debugging, stopped: the consonant doubled for the ending
        let b = r.as_bytes();
        if put.is_empty()
            && r.len() >= 5
            && b[b.len() - 1] == b[b.len() - 2]
            && !b"aeioulsz".contains(&b[b.len() - 1])
        {
            r.pop();
        }
        return (r != word).then_some(r);
    }
    None
}

/// Parents of the subagents that matched.
///
/// A subagent is not something you resume on its own; the session that
/// spawned it is. The browser reveals the parent when the answer was found
/// in one of its children, and the command line did not -- so the same
/// query gave two different answers depending on where you asked it.
pub fn parents_of_hits<V>(
    sessions: &[crate::model::Session],
    hits: &HashMap<String, V>,
) -> std::collections::HashSet<String> {
    sessions
        .iter()
        .filter(|s| s.is_subagent && hits.contains_key(&s.path.to_string_lossy().to_string()))
        .filter_map(|s| s.parent.clone())
        .collect()
}

/// How well each session answered, by id: its own hit, or the best of its
/// subagents', whichever is better. A subagent is listed under its session,
/// so its match is what puts that session where it goes.
pub fn session_scores<'a>(
    sessions: &'a [crate::model::Session],
    hits: &Hits,
) -> HashMap<&'a str, f64> {
    let mut best: HashMap<&str, f64> = HashMap::new();
    for s in sessions {
        let Some(h) = hits.get(s.path.to_string_lossy().as_ref()) else {
            continue;
        };
        let id = match (&s.parent, s.is_subagent) {
            (Some(p), true) => p.as_str(),
            _ => s.id.as_str(),
        };
        let e = best.entry(id).or_insert(h.score);
        *e = e.max(h.score);
        // an orphan is listed as itself
        if s.is_subagent {
            let e = best.entry(s.id.as_str()).or_insert(h.score);
            *e = e.max(h.score);
        }
    }
    best
}

/// A readable excerpt of indexed prose around the first mention.
///
/// FTS5 has `snippet()` for this, and it costs about 80ms per document --
/// nothing for a normal search, and eighty-five seconds for a word that
/// appears in every transcript. Cutting the window out of the stored text
/// does the same job for the whole corpus in under a second.
pub fn excerpt(text: &str, needle: &str) -> String {
    excerpt_by(text, &Spotter::new(needle))
}

/// An excerpt of `text` around where `spotter` finds the most.
pub fn excerpt_by(text: &str, spotter: &Spotter) -> String {
    const PAD: usize = 90;
    let hay = text.as_bytes();
    let at = spotter.best(text);
    let Some(at) = at else {
        // Nothing of the query is in the text: a prefix match on a longer
        // word. The opening line still says what the session was about.
        return crate::scan::squash(text, 160);
    };
    let lo = at.saturating_sub(PAD);
    let hi = (at + PAD).min(hay.len());
    // never split a character in half
    let lo = (lo..=at).find(|&i| text.is_char_boundary(i)).unwrap_or(at);
    let hi = (hi..=hay.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(hay.len());
    let mut out = crate::scan::squash(&text[lo..hi], 200);
    if lo > 0 {
        out.insert(0, '…');
    }
    if hi < hay.len() {
        out.push('…');
    }
    out
}

/// What an excerpt looks for, term by term: the ways each can be written.
///
/// Folded the way `find_ci` folds, ASCII only -- a Unicode lowercase turned
/// `Москве` into `москве`, which then never matched the text it was typed
/// from -- and lowercased in full as well when that is different. A word is
/// looked for as the index took it, without the punctuation around it:
/// `pool*` is there as `pooling`, `«bonjour»` as `« bonjour »`; and by its
/// root, since that is how the index may have found it.
fn spot_needles(needle: &str) -> Vec<Vec<String>> {
    let trim = |w: &str| w.trim_matches(|c| PROSE.contains(c)).to_string();
    let mut out: Vec<Vec<String>> = Vec::new();
    for t in Query::parse(needle).terms {
        let typed = trim(t.text());
        let mut ways = vec![typed.to_ascii_lowercase()];
        let full = typed.to_lowercase();
        if full != ways[0] {
            ways.push(full);
        }
        match &t {
            Term::Word(w) => ways.extend(root(&w.to_lowercase())),
            // FTS splits on punctuation, so "page fault" matches a
            // transcript that only ever wrote "page-fault": the phrase is
            // not in it as written, and its first word is far more use
            // than the opening line.
            Term::Phrase(p) => {
                if let Some(first) = p.split_whitespace().next() {
                    let first = trim(first).to_ascii_lowercase();
                    if !ways.contains(&first) {
                        ways.push(first);
                    }
                }
            }
        }
        ways.retain(|w| !w.is_empty());
        if !ways.is_empty() {
            out.push(ways);
        }
    }
    out
}

/// Where a query's terms are in a text: what an excerpt is cut around, and
/// what the viewer goes to and draws out.
#[derive(Clone, Debug, Default)]
pub struct Spotter {
    terms: Vec<Vec<String>>,
    /// Found inside words too, as the scan finds them. Otherwise a word is
    /// found where a word starts, as the index matched it.
    anywhere: bool,
}

/// One place a term was found: its bytes, and which term it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub start: usize,
    pub end: usize,
    pub term: usize,
}

impl Spotter {
    pub fn new(query: &str) -> Spotter {
        Spotter {
            terms: spot_needles(query),
            anywhere: false,
        }
    }

    /// One that finds a word inside another, for what the scan found: it
    /// matched `pool` in "zpool", and would otherwise be shown nothing.
    pub fn anywhere(query: &str) -> Spotter {
        Spotter {
            anywhere: true,
            ..Spotter::new(query)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Only the terms `text` does not have.
    pub fn missing_from(&self, text: &str) -> Spotter {
        Spotter {
            terms: (0..self.terms.len())
                .filter(|&t| self.places(text, t, 1).is_empty())
                .map(|t| self.terms[t].clone())
                .collect(),
            anywhere: self.anywhere,
        }
    }

    /// How many terms there are to find.
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// Where term `t` is in `text`, as byte ranges, up to `most` of them.
    ///
    /// A word is found only where a word starts -- "pool", not "spool" --
    /// as the index matched it. That is decided by the word, not by the
    /// text: decided by whether this piece of text had it at a word start
    /// anywhere, a line of the viewer with only "zpool" in it marked the
    /// pool inside it. What starts with a symbol, `c++` or `__init__`, and
    /// a script written without spaces, which has no word starts to go by,
    /// are found wherever they are.
    fn places(&self, text: &str, t: usize, most: usize) -> Vec<(usize, usize)> {
        // Only ASCII is folded by `find_ci`. For the rest the text is
        // lowercased whole, when that leaves every byte where it was --
        // Cyrillic and Greek do -- so `москве` finds "Москве".
        let folded = (!self.terms[t].iter().all(|w| w.is_ascii()))
            .then(|| text.to_lowercase())
            .filter(|l| l.len() == text.len());
        let hay = folded.as_deref().unwrap_or(text).as_bytes();
        let mut at: Vec<(usize, usize)> = Vec::new();
        for w in &self.terms[t] {
            let starts = !self.anywhere
                && w.chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() && !unspaced(c));
            let mut from = 0;
            while at.len() < most && from < hay.len() {
                let Some(rel) = find_ci(&hay[from..], w.as_bytes()) else {
                    break;
                };
                let p = from + rel;
                from = p + 1;
                let e = p + w.len();
                if !text.is_char_boundary(p) || !text.is_char_boundary(e) {
                    continue;
                }
                if starts
                    && text[..p]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric)
                {
                    continue;
                }
                at.push((p, e));
            }
        }
        at.sort_unstable();
        at
    }

    /// Every place in `text` a term is, in order. A word is marked to its
    /// end, since the index matched the word: `pool` marks all of
    /// "pooling".
    pub fn spots(&self, text: &str) -> Vec<Spot> {
        const MOST: usize = 4096;
        let mut out: Vec<Spot> = Vec::new();
        for t in 0..self.terms.len() {
            for (p, e) in self.places(text, t, MOST) {
                // Only ASCII is folded, so a match begins and ends where
                // a character does; and carries on to the end of its word.
                let word_end = text[e..]
                    .char_indices()
                    .find(|(_, c)| !c.is_alphanumeric())
                    .map_or(text.len(), |(i, _)| e + i);
                out.push(Spot {
                    start: p,
                    end: word_end,
                    term: t,
                });
            }
        }
        out.sort_unstable();
        out.dedup_by(|b, a| a.start == b.start);
        out
    }

    /// Where to centre an excerpt: where the most of the terms are close
    /// together, the first of those that have as many.
    ///
    /// The first mention of the first word was where it went, and with
    /// several words that was usually somewhere only that one word was:
    /// "zfs snapshot" showed a line about zfs and nothing about a snapshot.
    ///
    /// Anchored on the term that is said least often, whose every place is
    /// looked at, and each looked around for the others. Gathered in
    /// order and capped instead, a word like "the" ran out of places in
    /// the first few pages of a long session, and the excerpt for `the
    /// zpool` came from where the zpool was not.
    pub fn best(&self, text: &str) -> Option<usize> {
        const NEAR: usize = 80;
        const MOST: usize = 4096;
        let per: Vec<Vec<(usize, usize)>> = (0..self.terms.len())
            .map(|t| self.places(text, t, MOST))
            .collect();
        let anchor = (0..per.len())
            .filter(|&t| !per[t].is_empty())
            .min_by_key(|&t| per[t].len())?;
        let boundary = |mut i: usize| {
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let mut best = (0usize, per[anchor][0].0);
        for &(p, _) in &per[anchor] {
            let around =
                &text[boundary(p.saturating_sub(NEAR))..boundary((p + NEAR).min(text.len()))];
            let held = 1
                + (0..per.len())
                    .filter(|&t| t != anchor && !self.places(around, t, 1).is_empty())
                    .count();
            if held > best.0 {
                best = (held, p);
                if held == per.len() {
                    break;
                }
            }
        }
        Some(best.1)
    }

    /// How many of the terms `text` has.
    pub fn count(&self, text: &str) -> usize {
        (0..self.terms.len())
            .filter(|&t| !self.places(text, t, 1).is_empty())
            .count()
    }
}

/// Scan one file for every group of needles, returning a readable hit.
///
/// A group is one term written each way it might be; the file matches once
/// every group has turned up somewhere that counts, not necessarily on the
/// same line. The excerpt is from where the first of them did.
fn search_file(
    path: &std::path::Path,
    h: Harness,
    groups: &[Vec<memmem::Finder>],
    mode: Mode,
) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut rdr = BufReader::with_capacity(1 << 18, f);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 14);
    let mut seen = Seen::new(groups.len());
    loop {
        buf.clear();
        let n = rdr.read_until(b'\n', &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(done) = seen.line(&buf[..n], h, groups, mode) {
            return Some(done);
        }
        if buf.capacity() > (1 << 20) {
            buf = Vec::with_capacity(1 << 14);
            seen.low = Vec::with_capacity(1 << 14);
        }
    }
}

/// Which groups a session has shown so far, line by line, and where the
/// first of them was.
struct Seen {
    found: Vec<bool>,
    first: Option<String>,
    low: Vec<u8>,
}

impl Seen {
    fn new(groups: usize) -> Seen {
        Seen {
            found: vec![false; groups],
            first: None,
            low: Vec::with_capacity(1 << 14),
        }
    }

    /// Look through one more line: the excerpt once every group has been
    /// seen, `None` until then.
    fn line(
        &mut self,
        line: &[u8],
        h: Harness,
        groups: &[Vec<memmem::Finder>],
        mode: Mode,
    ) -> Option<String> {
        if matches!(mode, Mode::Content | Mode::Everything) {
            self.low.clear();
            self.low.extend(line.iter().map(u8::to_ascii_lowercase));
        }
        for (g, ways) in groups.iter().enumerate() {
            if self.found[g] {
                continue;
            }
            let hit = ways.iter().find_map(|needle| match mode {
                Mode::Content | Mode::Everything => content_hit(
                    &Line {
                        raw: line,
                        low: &self.low,
                    },
                    needle,
                    h,
                ),
                Mode::File => file_hit(line, needle),
                Mode::Tool => tool_hit(line, needle),
            });
            if let Some(at) = hit {
                self.found[g] = true;
                self.first.get_or_insert_with(|| snippet(line, at));
            }
        }
        if self.found.iter().all(|f| *f) {
            self.first.take()
        } else {
            None
        }
    }
}

/// Scan a Hermes session: its messages are rows in a database, not lines of
/// a file. What you and Hermes said is searched; a tool's result is not,
/// and nor are the tool-call records the other modes look through.
fn search_hermes(s: &Session, groups: &[Vec<memmem::Finder>], mode: Mode) -> Option<String> {
    if !matches!(mode, Mode::Content | Mode::Everything) {
        return None;
    }
    let (db, id) = crate::hermes::split_key(&s.path.to_string_lossy())?;
    let c = crate::hermes::open(&db)?;
    let mut st = c
        .prepare(
            "SELECT content FROM messages WHERE session_id = ?1
             AND role IN ('user', 'assistant') ORDER BY id",
        )
        .ok()?;
    let rows = st.query_map([id], |r| r.get::<_, Option<String>>(0)).ok()?;
    let mut seen = Seen::new(groups.len());
    for content in rows.flatten().flatten() {
        if let Some(done) = seen.line(content.as_bytes(), Harness::Hermes, groups, mode) {
            return Some(done);
        }
    }
    None
}

/// Search every session in parallel, for the ones every group is in.
fn brute(sessions: &[Session], groups: &[Vec<Vec<u8>>], mode: Mode) -> Hits {
    if groups.is_empty() {
        return HashMap::new();
    }
    let finders: Vec<Vec<memmem::Finder>> = groups
        .iter()
        .map(|ways| ways.iter().map(memmem::Finder::new).collect())
        .collect();
    sessions
        .par_iter()
        .filter_map(|s| {
            match s.harness {
                Harness::Hermes => search_hermes(s, &finders, mode),
                h => search_file(&s.path, h, &finders, mode),
            }
            .map(|snip| (s.path.to_string_lossy().to_string(), Hit::new(snip)))
        })
        .collect()
}

/// What the scan looks for: each term of a content search, or the whole of
/// a file or tool search, which name one thing. Each is written out the
/// ways it might be on disk.
fn scan_groups(query: &str, mode: Mode) -> Vec<Vec<Vec<u8>>> {
    let q = Query::parse(query);
    let whole = || {
        let typed = query.trim().to_string();
        let lower = typed.to_lowercase();
        if lower == typed.to_ascii_lowercase() {
            vec![vec![typed]]
        } else {
            vec![vec![typed, lower]]
        }
    };
    let literals = match mode {
        Mode::Content | Mode::Everything if !q.is_empty() => q.literals(),
        _ => whole(),
    };
    literals
        .iter()
        .map(|ways| {
            let mut out: Vec<Vec<u8>> = Vec::new();
            for w in ways {
                let n = scan_needle(w);
                if !n.is_empty() && !out.contains(&n) {
                    out.push(n);
                }
            }
            out
        })
        .filter(|ways| !ways.is_empty())
        .collect()
}

/// How a set of results was produced, so the interface can say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    Indexed,
    Scanned,
    /// Nothing had every word, so these have some: the most of them first.
    Partial,
}

/// Find sessions matching `query`.
///
/// Content searches go through the full-text index, which covers the ~2% of
/// the corpus that is actually prose and answers in milliseconds instead of
/// re-reading gigabytes. The index tokenises, so it cannot match the middle of
/// a word; when it finds nothing we fall back to the exhaustive scan rather
/// than claiming there is nothing there, and when that finds nothing either,
/// to the sessions with some of the words. File and tool searches always
/// scan, because they query structure rather than prose.
pub fn run(sessions: &[Session], query: &str, mode: Mode) -> (Hits, How) {
    if query.trim().is_empty() {
        return (HashMap::new(), How::Indexed);
    }
    let q = Query::parse(query);
    let indexed = mode == Mode::Content && !q.is_empty();
    let idx = indexed.then(crate::index::Index::open).and_then(Result::ok);
    // Every session searched, with what it is called: a session whose title
    // says what was asked is likelier to be the one meant.
    let titles: HashMap<String, &str> = sessions
        .iter()
        .map(|s| (s.path.to_string_lossy().to_string(), s.title()))
        .collect();
    let spotter = Spotter::new(query);
    let lift = |path: &str, score: f64| titled(&spotter, titles.get(path).copied(), score);
    if let Some(idx) = &idx {
        // What the tokenizer would lose is looked for as written, in the
        // same prose: a tenth of the corpus rather than all of it, and it
        // comes with its excerpts. Everything else goes to the index, whose
        // excerpts are filled in one row at a time, on demand.
        let found = if beyond_tokens(query) {
            idx.prose_containing(&q.literals(), query.trim())
        } else {
            idx.search_text(&q.fts()).map(|found| {
                found
                    .into_iter()
                    .map(|(p, score)| {
                        let hit = Hit {
                            excerpt: String::new(),
                            score,
                        };
                        (p, hit)
                    })
                    .collect()
            })
        };
        if let Ok(found) = found {
            let kept: Hits = found
                .into_iter()
                .filter(|(p, _)| titles.contains_key(p))
                .map(|(p, mut hit)| {
                    hit.score = lift(&p, hit.score);
                    (p, hit)
                })
                .collect();
            if !kept.is_empty() {
                return (kept, How::Indexed);
            }
        }
    }
    let hits = brute(sessions, &scan_groups(query, mode), mode);
    if !hits.is_empty() || q.terms.len() < 2 || beyond_tokens(query) {
        return (hits, How::Scanned);
    }
    match &idx {
        Some(idx) => {
            let some = some_of(idx, &q, |p| titles.contains_key(p), lift);
            let how = if some.is_empty() {
                How::Scanned
            } else {
                How::Partial
            };
            (some, how)
        }
        None => (hits, How::Scanned),
    }
}

/// The sessions with some of the terms, the most of them first and then by
/// how well they have them.
///
/// Three words of which no session has all three found nothing at all,
/// which said less than it knew: the one that has two of them is very
/// often the one meant, the third word being what it was called in
/// somebody's memory rather than in the session.
fn some_of(
    idx: &crate::index::Index,
    q: &Query,
    known: impl Fn(&str) -> bool,
    lift: impl Fn(&str, f64) -> f64,
) -> Hits {
    let mut tally: HashMap<String, (usize, f64)> = HashMap::new();
    for t in &q.terms {
        let Ok(found) = idx.search_text(&t.fts()) else {
            continue;
        };
        for (p, score) in found {
            if known(&p) {
                let e = tally.entry(p).or_default();
                e.0 += 1;
                e.1 += score;
            }
        }
    }
    tally
        .into_iter()
        .map(|(p, (n, score))| {
            // bm25 is a few units at most; a term more outweighs any of it
            let hit = Hit {
                excerpt: String::new(),
                score: n as f64 * 1e6 + lift(&p, score),
            };
            (p, hit)
        })
        .collect()
}

/// A score lifted by how much of the query the session's title says: up to
/// twice as good when it says all of it.
///
/// The index holds what was said, and not what the session is called. A
/// session titled "Hyprland installation" came sixth for `hyprland
/// installation`, under long ones that said "installation" forty times and
/// "hyprland" once. Asked for two words of what was first asked in a
/// session, it came first 64% of the time with this, and 44% without; three
/// times as good when the title says it all did no better than twice.
fn titled(spotter: &Spotter, title: Option<&str>, score: f64) -> f64 {
    match (title, spotter.len()) {
        (Some(title), n) if n > 0 => score * (1.0 + spotter.count(title) as f64 / n as f64),
        _ => score,
    }
}

/// Would the full-text index lose what this query is about?
///
/// unicode61 splits on anything that is not a letter or digit, and does not
/// split scripts written without spaces at all. So `c++` became a prefix
/// search for "c" and matched nearly every session -- and since the index
/// had an answer, the scan that would have been right never ran -- while a
/// Japanese word from the middle of a sentence was inside one long token
/// nobody would type, and matched nothing. Sentence punctuation at a word's
/// edge is not what a question is about, so `why is it slow?` still goes to
/// the index.
///
/// Anything but a letter or digit counts -- `_`, `£`, `€` as much as `+`
/// -- except what belongs to the sentence rather than the word: `*`, the
/// index's own prefix search, and quotes and brackets in any script. Those
/// sent `pool*` and `«bonjour»` to an exact match that found nothing.
///
/// Asked of the words as the query is read, quotes taken off: asked of the
/// query as typed, `"c++"` had a quote at each edge, which is punctuation,
/// and went to the index as a search for "c".
fn beyond_tokens(query: &str) -> bool {
    let edge =
        |w: &str| w.chars().next().is_some_and(telling) || w.chars().last().is_some_and(telling);
    Query::parse(query)
        .terms
        .iter()
        .any(|t| t.text().split_whitespace().any(edge))
        || query.chars().any(unspaced)
}

/// Not a letter or a digit, nor a mark that belongs to the sentence.
fn telling(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !PROSE.contains(c)
}

/// Marks at a word's edge that are the sentence's, not the word's.
const PROSE: &str =
    ".,;:!?\"'()[]{}*«»‹›“”„‟‘’‚‛¿¡…–—·「」『』（）［］｛｝【】〈〉《》、。，．：；！？";

/// Letters of a script written without spaces between words: Chinese,
/// Japanese, Thai, Lao, Burmese, Khmer.
fn unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF
        | 0x3040..=0x30FF | 0x31F0..=0x31FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// The query as the scan looks for it in a raw transcript line.
///
/// Folded the way `find_ci` compares, ASCII only -- lowercased in full, a
/// word with a non-ASCII capital could not match even typed exactly as it
/// was written -- and escaped the way JSON writes it, since the scan reads
/// the JSON and not the text: a `"` or `\` in the query is `\"` or `\\` on
/// disk, and without this `say "hello` or `C:\Users` never matched.
fn scan_needle(query: &str) -> Vec<u8> {
    let q = query.trim().to_ascii_lowercase();
    let quoted = serde_json::to_string(&q).unwrap_or_default();
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or("")
        .as_bytes()
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hit run on one line, the way the scan runs it.
    fn content(line: &[u8], needle: &[u8]) -> Option<usize> {
        let low = line.to_ascii_lowercase();
        content_hit(
            &Line {
                raw: line,
                low: &low,
            },
            &memmem::Finder::new(needle),
            Harness::Claude,
        )
    }
    fn hit(
        f: fn(&[u8], &memmem::Finder) -> Option<usize>,
        line: &[u8],
        needle: &[u8],
    ) -> Option<usize> {
        f(line, &memmem::Finder::new(needle))
    }

    // A user turn carrying an injected memory block, exactly as Claude Code
    // writes it: the reminder is inside the message content.
    const WITH_REMINDER: &str = concat!(
        r#"{"parentUuid":"a","isSidechain":false,"message":{"role":"user","content":"#,
        r#"[{"type":"text","text":"<system-reminder>NVENC needs USE=cuda</system-reminder>"#,
        r#" please check the disk"}]},"type":"user","uuid":"b"}"#
    );
    const PLAIN_USER: &str = r#"{"parentUuid":"a","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"fix the NVENC build"}]},"type":"user","uuid":"b"}"#;
    // `attachment` records are where the memory index actually lands.
    const ATTACHMENT: &str = r#"{"parentUuid":"a","attachment":{"x":1},"rendered":"NVENC and friends","type":"attachment"}"#;

    /// One transcript on disk, and whether the scan finds `query` in it.
    fn scan_finds(lines: &[&str], query: &str, mode: Mode) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let needles = scan_groups(query, mode);
        let groups: Vec<Vec<memmem::Finder>> = needles
            .iter()
            .map(|ways| ways.iter().map(memmem::Finder::new).collect())
            .collect();
        search_file(&path, Harness::Claude, &groups, mode)
    }

    /// The same, for another agent's file.
    fn scan_finds_in(h: crate::model::Harness, lines: &[&str], query: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let s = Session {
            harness: h,
            path,
            ..Default::default()
        };
        !brute(&[s], &scan_groups(query, Mode::Content), Mode::Content).is_empty()
    }

    #[test]
    fn the_scan_reads_what_pi_and_omp_said() {
        // Where the index cannot answer -- `c++` is not a word to it -- the
        // scan does, and pi's lines all begin as Claude's metadata does.
        use crate::model::Harness;
        let said = r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-01T09:00:00Z","message":{"role":"user","content":"port the c++ templates"}}"#;
        let tool = r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-01T09:00:01Z","message":{"role":"toolResult","toolCallId":"x","toolName":"bash","content":[{"type":"text","text":"g++ -std=c++20 src/zebra.cpp"}],"isError":false}}"#;
        for h in [Harness::Pi, Harness::Omp] {
            assert!(scan_finds_in(h, &[said, tool], "c++"), "{h:?}");
            assert!(
                !scan_finds_in(h, &[said, tool], "zebra.cpp"),
                "a tool's output, {h:?}"
            );
        }
    }

    #[test]
    fn the_scan_reads_what_codex_was_asked_but_not_what_it_sent_along() {
        use crate::model::Harness;
        let sent = r##"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /w <INSTRUCTIONS>use c++17</INSTRUCTIONS>"}]}}"##;
        let asked = r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"why is the c++ build slow"}]}}"#;
        let ran = r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ninja -C out/zebra\"}","call_id":"c"}}"#;
        let out = r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"g++ quokka.o"}}"#;
        assert!(scan_finds_in(Harness::Codex, &[sent, asked], "c++"));
        assert!(
            !scan_finds_in(Harness::Codex, &[sent], "c++17"),
            "AGENTS.md was searched"
        );
        assert!(
            scan_finds_in(Harness::Codex, &[ran], "out/zebra"),
            "a command it ran"
        );
        assert!(
            !scan_finds_in(Harness::Codex, &[out], "quokka.o"),
            "a command's output"
        );
    }

    #[test]
    fn the_scan_reads_hermes_from_its_database() {
        use crate::hermes::tests::{add_session, make_db, say};
        use crate::model::Harness;
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = make_db(&db, false);
        add_session(&c, "h1", "cli", Some("/w"));
        say(&c, "h1", "user", "the c++ build broke", 1.0);
        say(&c, "h1", "tool", "zebra.cpp:3 error", 2.0);
        let s = Session {
            harness: Harness::Hermes,
            id: "h1".into(),
            path: crate::hermes::key(&db, "h1").into(),
            ..Default::default()
        };
        let find = |q: &str| {
            !brute(
                std::slice::from_ref(&s),
                &scan_groups(q, Mode::Content),
                Mode::Content,
            )
            .is_empty()
        };
        assert!(find("c++"));
        assert!(!find("zebra.cpp"), "a tool's output");
    }

    fn user_said(text: &str) -> String {
        let text = serde_json::to_string(text).unwrap();
        format!(
            r#"{{"parentUuid":"a","message":{{"role":"user","content":[{{"type":"text","text":{text}}}]}},"type":"user"}}"#
        )
    }

    #[test]
    fn a_word_typed_as_it_was_written_is_found_whatever_its_script() {
        let line = user_said("встреча в Москве завтра");
        assert!(scan_finds(&[&line], "Москве", Mode::Everything).is_some());
        let e = excerpt("встреча в Москве завтра", "Москве");
        assert!(e.contains("Москве"), "{e:?}");
    }

    #[test]
    fn a_capital_outside_ascii_still_finds_its_lowercase() {
        let line = user_said("встреча в москве завтра");
        assert!(run_scan(&[&line], "Москве").is_some(), "the scan missed it");
        let e = excerpt("встреча в москве завтра", "Москве");
        assert!(e.contains("москве"), "{e:?}");
    }

    /// `run`'s exhaustive path over one transcript, every needle and all.
    fn run_scan(lines: &[&str], query: &str) -> Option<String> {
        scan_finds(lines, query, Mode::Everything)
    }

    #[test]
    fn a_word_from_the_middle_of_a_japanese_sentence_is_found() {
        let line = user_said(&format!(
            "{}テキスト{}",
            "日本語のながい".repeat(8),
            "ですね".repeat(8)
        ));
        let hit = scan_finds(&[&line], "テキスト", Mode::Everything).expect("taken for base64");
        assert!(!hit.contains('\u{fffd}'), "{hit:?}");
    }

    #[test]
    fn a_rare_anchor_still_finds_the_leftmost_match() {
        let hay = b"xx CheckPatch and checkpatch, then CHECKPATCH";
        assert_eq!(find_ci(hay, b"checkpatch"), Some(3));
        assert_eq!(find_ci(b"check patch", b"checkpatch"), None);
        assert_eq!(find_ci(b"kkkkk", b"kk"), Some(0));
        assert_eq!(find_ci(b"ab", b"abc"), None);
        assert_eq!(find_ci(b"zzz qz", b"qz"), Some(4));
        assert_eq!(
            find_ci("日本語テキスト".as_bytes(), "テキスト".as_bytes()),
            Some(9)
        );
        // every position, against the obvious search
        let text = b"The quick brown fox jumps over the lazy dog; THE END";
        for n in 1..6 {
            for i in 0..text.len() - n {
                let needle = text[i..i + n].to_ascii_lowercase();
                let want =
                    (0..=text.len() - n).find(|&j| text[j..j + n].eq_ignore_ascii_case(&needle));
                assert_eq!(
                    find_ci(text, &needle),
                    want,
                    "{:?}",
                    String::from_utf8_lossy(&needle)
                );
            }
        }
    }

    #[test]
    fn what_the_tokenizer_would_lose_is_looked_for_as_written() {
        for q in [
            "c++",
            "C#",
            "-v",
            "$HOME",
            "~/.bashrc",
            "テキスト",
            "设计",
            "templates in c++",
            // unicode61 splits on `_` and drops currency signs, so these
            // became `init*`, `id*` and `5*` and matched nearly everything
            "__init__",
            "_id",
            "£5",
            "€100",
            // in quotes it is the same word
            "\"c++\"",
            "“c++ templates”",
            "->",
            "C++ ->",
        ] {
            assert!(beyond_tokens(q), "{q:?} went to the tokenizer");
        }
        for q in [
            "pool",
            "page fault",
            "why is it slow?",
            "foo()",
            "src/main.rs",
            "Москве",
            "e.g.",
            "pool*",
            "«bonjour»",
            "“quoted”",
            "¿qué?",
            "etc…",
        ] {
            assert!(!beyond_tokens(q), "{q:?} skipped the index");
        }
    }

    #[test]
    fn the_transcript_s_own_keys_are_not_a_match() {
        let line = user_said("who holds the admin role here");
        for key in ["parentuuid", "parentUuid", "type"] {
            assert!(
                scan_finds(&[&line], key, Mode::Everything).is_none(),
                "{key} matched a key"
            );
        }
        // but the same word said in the conversation is
        assert!(scan_finds(&[&line], "role", Mode::Everything).is_some());
        let quoted = user_said(r#"it printed "role": "admin" twice"#);
        assert!(scan_finds(&[&quoted], "admin", Mode::Everything).is_some());
    }

    #[test]
    fn a_quote_or_a_backslash_in_the_query_can_match() {
        let line = user_said(r#"then say "hello there" from C:\Users\me"#);
        assert!(scan_finds(&[&line], r#"say "hello"#, Mode::Everything).is_some());
        assert!(scan_finds(&[&line], r"C:\Users", Mode::Everything).is_some());
        // a file search names one thing, spaces and all
        let edit = r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/tmp/my notes.txt"}}]}}"#;
        assert!(scan_finds(&[edit], "my notes.txt", Mode::File).is_some());
        assert!(scan_finds(&[edit], "notes my", Mode::File).is_none());
    }

    #[test]
    fn the_scan_wants_every_word_but_not_on_one_line() {
        let a = user_said("the zpool will not import");
        let b = r#"{"message":{"role":"assistant","content":[{"type":"text","text":"try it with the -f flag"}]},"type":"assistant"}"#;
        let hit = scan_finds(&[&a, b], "zpool flag", Mode::Everything).expect("both are there");
        assert!(hit.contains("zpool"), "{hit:?}");
        assert!(scan_finds(&[&a, b], "zpool raidz", Mode::Everything).is_none());
    }

    #[test]
    fn fts_expressions_cannot_be_broken_by_input() {
        // one word gets a prefix match, so "nvenc" still finds "nvenc's"
        assert_eq!(fts_expr("nvenc"), r#""nvenc"*"#);
        // several words are each looked for, in any order
        assert_eq!(fts_expr("page fault"), r#""page"* AND "fault"*"#);
        assert_eq!(fts_expr("  page   fault  "), r#""page"* AND "fault"*"#);
        // and in quotes, as a phrase
        assert_eq!(fts_expr(r#""page fault""#), r#""page fault"*"#);
        assert_eq!(
            fts_expr(r#"kernel “page  fault” boot"#),
            r#""kernel"* AND "page fault"* AND "boot"*"#
        );
        // an unclosed quote runs to the end
        assert_eq!(fts_expr(r#"say "hi there"#), r#""say"* AND "hi there"*"#);
        // operators are inert inside a quoted term
        assert_eq!(fts_expr("a OR b"), r#""a" AND "or" AND "b""#);
        // what has nothing to look for is dropped, not sent as `""`
        assert_eq!(fts_expr("? —"), "");
        // a word of symbols is kept for the prose, but is not FTS5's to ask
        assert_eq!(Query::parse("C++ ->").literals().len(), 2);
        assert_eq!(fts_expr("->"), "");
        assert_eq!(fts_expr(r#""""#), "");
        assert_eq!(fts_expr(""), "");
        assert_eq!(fts_expr("   "), "");
    }

    #[test]
    fn a_word_is_also_asked_for_by_its_root() {
        for (word, want) in [
            ("installation", Some("install")),
            ("optimization", Some("optimiz")),
            ("debugging", Some("debug")),
            ("stopped", Some("stop")),
            ("reboots", Some("reboot")),
            ("patches", Some("patch")),
            ("files", Some("file")),
            ("queries", Some("query")),
            ("query", Some("queri")),
            ("properly", Some("proper")),
            ("configure", Some("configur")),
            ("deployment", Some("deploy")),
            // not plurals
            ("status", None),
            ("analysis", None),
            ("class", None),
            // too short to be worth it: `fix` is every fixture
            ("fixed", None),
            ("thing", None),
            ("key", None),
            // not plain English words
            ("src/main.rs", None),
            ("nvenc", None),
            ("Москве", None),
        ] {
            assert_eq!(root(word).as_deref(), want, "{word}");
        }
        assert_eq!(
            fts_expr("installation"),
            r#"("installation"* OR "install"*)"#
        );
        // a short word is matched whole: `a*` is most of the language
        assert_eq!(fts_expr("is it"), r#""is" AND "it""#);
    }

    #[test]
    fn a_session_called_what_was_asked_for_counts_for_more() {
        // "Hyprland installation" came sixth for `hyprland installation`,
        // under long sessions that said "installation" forty times.
        let f = Spotter::new("hyprland installation");
        let t = |title| titled(&f, Some(title), 10.0);
        assert_eq!(t("Hyprland installation"), 20.0);
        assert_eq!(
            t("Install hyprland"),
            20.0,
            "by its root, as the index matched it"
        );
        assert_eq!(t("Hyprland window rules"), 15.0);
        assert_eq!(t("EFT offline backend"), 10.0);
        assert_eq!(titled(&f, None, 10.0), 10.0);
        // a long session that mentions it in passing no longer outranks
        // the one named for it
        assert!(t("Hyprland installation") * 9.46 / 10.0 > t("EFT offline backend") * 10.76 / 10.0);
    }

    #[test]
    fn with_nothing_that_has_every_word_the_most_of_them_come_first() {
        let d = tempfile::tempdir().unwrap();
        let mut idx = crate::index::Index::open_at(&d.path().join("i.db")).unwrap();
        idx.store_text(&[
            ("/two".into(), "the zfs pool needed a snapshot".into()),
            ("/one".into(), "zfs zfs zfs zfs zfs, all day".into()),
            ("/none".into(), "nothing to see".into()),
        ])
        .unwrap();
        let q = Query::parse("zfs snapshot rollback");
        assert!(
            idx.search_text(&q.fts()).unwrap().is_empty(),
            "one has all three"
        );
        let some = some_of(&idx, &q, |_| true, |_, s| s);
        let mut order: Vec<(&String, &Hit)> = some.iter().collect();
        order.sort_by(|a, b| b.1.score.total_cmp(&a.1.score));
        let order: Vec<&str> = order.iter().map(|(p, _)| p.as_str()).collect();
        // two words beat one said five times
        assert_eq!(order, ["/two", "/one"]);
    }

    #[test]
    fn a_match_inside_a_subagent_points_at_its_parent() {
        // You cannot resume a subagent; the session that spawned it is the
        // thing to open. The browser knew that and the command line did
        // not, so `--search zpool` missed a session whose only mention was
        // inside one of its children.
        use crate::app::fixtures::corpus;
        let all = corpus();
        let sub = all
            .iter()
            .find(|s| s.is_subagent)
            .expect("fixture has subagents");
        let mut hits = HashMap::new();
        hits.insert(sub.path.to_string_lossy().to_string(), "…".to_string());

        let parents = parents_of_hits(&all, &hits);
        assert_eq!(parents.len(), 1);
        assert!(parents.contains(sub.parent.as_deref().unwrap()));

        // a hit on a top-level session contributes no parent
        let top = all.iter().find(|s| !s.is_subagent).unwrap();
        let mut hits = HashMap::new();
        hits.insert(top.path.to_string_lossy().to_string(), "…".to_string());
        assert!(parents_of_hits(&all, &hits).is_empty());
    }

    #[test]
    fn an_excerpt_shows_the_term_in_its_surroundings() {
        let text = "a ".repeat(200) + "the zpool is degraded" + &" b".repeat(200);
        let e = excerpt(&text, "zpool");
        assert!(e.contains("zpool"), "{e:?}");
        assert!(e.contains('…'), "cut from the middle, so it should say so");
        assert!(e.chars().count() <= 210, "{} chars", e.chars().count());
    }

    #[test]
    fn an_excerpt_never_splits_a_character() {
        // The window is measured in bytes; the text is not.
        let text = format!(
            "{}日本語のながいテキスト{}",
            "あ".repeat(100),
            "い".repeat(100)
        );
        let e = excerpt(&text, "ながい");
        assert!(e.contains("ながい"), "{e:?}");
    }

    #[test]
    fn a_phrase_written_with_punctuation_still_lands_on_the_word() {
        // FTS splits on punctuation, so "page fault" matches text that only
        // ever says "page-fault". The excerpt should still show it rather
        // than falling back to the opening line.
        let text = "x ".repeat(200) + "a burst of page-faults under load" + &" y".repeat(200);
        let e = excerpt(&text, "page fault");
        assert!(e.contains("page-fault"), "{e:?}");
    }

    #[test]
    fn an_excerpt_lands_on_the_word_inside_the_punctuation() {
        // `pool*` and `«bonjour»` go to the index, which finds the word;
        // the excerpt looked for the punctuation too and showed the
        // opening line instead.
        let text = "x ".repeat(200) + "we tuned the pooling today" + &" y".repeat(200);
        let e = excerpt(&text, "pool*");
        assert!(e.contains("pooling"), "{e:?}");
        let text = "x ".repeat(200) + "il a dit « bonjour » et puis" + &" y".repeat(200);
        let e = excerpt(&text, "«bonjour»");
        assert!(e.contains("bonjour"), "{e:?}");
    }

    #[test]
    fn a_prefix_match_without_the_literal_word_still_shows_something() {
        // "connection pool" matches "connection pooling", so the query
        // itself need not appear anywhere in the text.
        let e = excerpt("we fixed the connection pooling at last", "connection pool");
        assert!(!e.is_empty());
        assert!(e.contains("pooling"), "{e:?}");
    }

    #[test]
    fn an_excerpt_shows_where_the_words_are_together() {
        // The first mention of the first word was where it went, which
        // showed a line about zfs and nothing about a snapshot.
        let text = "zfs is fine. ".repeat(30)
            + "then the zfs snapshot was rolled back"
            + &" and more".repeat(30);
        let e = excerpt(&text, "snapshot zfs");
        assert!(e.contains("zfs snapshot"), "{e:?}");
        // a word found where a word starts, as the index matched it
        let text = "the spool filled up. ".repeat(20) + "then the pool was resized";
        let e = excerpt(&text, "pool");
        assert!(e.contains("the pool was"), "{e:?}");
        // and by its root, as the index may have found it
        let text = "x ".repeat(200) + "we install hyprland next" + &" y".repeat(200);
        let e = excerpt(&text, "installation");
        assert!(e.contains("install hyprland"), "{e:?}");
    }

    #[test]
    fn an_excerpt_finds_a_rare_word_past_a_common_one_said_thousands_of_times() {
        let text = "the cat sat. ".repeat(5000) + "then the zpool degraded";
        let e = excerpt(&text, "the zpool");
        assert!(e.contains("zpool"), "{e:?}");
    }

    #[test]
    fn a_word_inside_another_is_not_marked_however_short_the_text() {
        let f = Spotter::new("pool");
        assert!(f.spots("the zpool is full").is_empty());
        assert_eq!(f.count("the zpool is full"), 0);
        assert_eq!(f.count("the pool is full"), 1);
        // but the scan's finds are, since it found them there
        assert_eq!(Spotter::anywhere("pool").count("the zpool is full"), 1);
        // and what starts with a symbol, or in Japanese, is found anywhere
        assert_eq!(Spotter::new("++").count("in c++ it is"), 1);
        assert_eq!(Spotter::new("テキスト").count("日本語のテキストです"), 1);
        assert_eq!(Spotter::new("москве").count("в Москве"), 1);
    }

    #[test]
    fn an_excerpt_of_nothing_is_empty_not_a_panic() {
        assert_eq!(excerpt("", "zpool"), "");
    }

    #[test]
    fn search_modes_cycle_and_include_the_exhaustive_one() {
        let mut m = Mode::Content;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(m.label());
            m = m.next();
        }
        assert_eq!(m, Mode::Content, "four modes, four steps");
        assert!(seen.contains(&"everything, slow"));
    }

    #[test]
    fn ci_search_finds_either_case() {
        assert_eq!(find_ci(b"hello WORLD", b"world"), Some(6));
        assert_eq!(find_ci(b"hello world", b"world"), Some(6));
        assert_eq!(find_ci(b"hello", b"world"), None);
        // must not run past the end when the needle is longer
        assert_eq!(find_ci(b"ab", b"abc"), None);
    }

    #[test]
    fn only_real_conversation_is_searched() {
        assert!(is_conversation(PLAIN_USER.as_bytes()));
        // metadata lines begin literally with {"type":"
        assert!(!is_conversation(
            br#"{"type":"ai-title","aiTitle":"NVENC work"}"#
        ));
        // attachments carry the injected memory index and are not conversation
        assert!(!is_conversation(ATTACHMENT.as_bytes()));
    }

    #[test]
    fn injected_context_does_not_match() {
        // The word only appears inside a <system-reminder>, so this session
        // did not actually discuss it. Matching here is what made a query
        // return every session that had ever run.
        assert_eq!(content(WITH_REMINDER.as_bytes(), b"nvenc"), None);
        // but text outside the reminder in the same line still matches
        assert!(content(WITH_REMINDER.as_bytes(), b"disk").is_some());
        // and a plain mention matches normally
        assert!(content(PLAIN_USER.as_bytes(), b"nvenc").is_some());
    }

    fn wrap(text: &str) -> String {
        format!(
            r#"{{"message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}},"type":"user"}}"#
        )
    }

    #[test]
    fn base64_blobs_do_not_match() {
        // A pasted image is thousands of unbroken characters, and base64's
        // alphabet spells short words by chance. Prose has whitespace; a blob
        // does not, which is the tell.
        let pad = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5YWJjZGVm".repeat(4);
        let blob = wrap(&format!("{pad}nvenc{pad}"));
        assert_eq!(content(blob.as_bytes(), b"nvenc"), None);
    }

    #[test]
    fn short_base64_run_is_not_treated_as_a_blob() {
        // A run shorter than a blob is let through: a chance hit in a few
        // dozen characters is rare, and rejecting it would risk discarding
        // real prose.
        let short = wrap("QUJDnvencREVG");
        assert!(content(short.as_bytes(), b"nvenc").is_some());
    }

    #[test]
    fn a_match_near_either_end_of_a_blob_is_still_the_blob() {
        let pad = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5YWJjZGVm".repeat(4);
        for blob in [
            wrap(&format!("see this: {pad}xYz{}", &pad[..20])),
            wrap(&format!("{}xYz{pad} was the image", &pad[..20])),
        ] {
            assert_eq!(content(blob.as_bytes(), b"xyz"), None, "{}", &blob[..80]);
        }
    }

    #[test]
    fn the_transcript_s_bookkeeping_is_not_a_match() {
        let line = r#"{"parentUuid":"p","message":{"role":"user","content":[{"type":"text","text":"a question"}]},"type":"user","cwd":"/home/u/scratchpad/proj","version":"2.1.0","gitBranch":"main"}"#;
        for q in ["main", "2.1.0", "scratchpad", "user"] {
            assert_eq!(content(line.as_bytes(), q.as_bytes()), None, "{q} matched");
        }
        let said = r#"{"message":{"role":"user","content":[{"type":"text","text":"merge it into main"}]},"type":"user","gitBranch":"main"}"#;
        assert!(
            content(said.as_bytes(), b"main").is_some(),
            "said in the conversation"
        );
    }

    #[test]
    fn file_search_matches_tool_paths_not_mentions() {
        let edit = br#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/etc/portage/make.conf"}}]}}"#;
        assert!(hit(file_hit, edit, b"make.conf").is_some());
        // a mere mention in prose is not a file the session touched
        let chat = br#"{"message":{"role":"user","content":[{"type":"text","text":"what is in make.conf"}]}}"#;
        assert_eq!(hit(file_hit, chat, b"make.conf"), None);
    }

    #[test]
    fn tool_search_needs_a_tool_use_block() {
        let used = br#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"WebSearch","input":{}}]}}"#;
        assert!(hit(tool_hit, used, b"websearch").is_some());
        let named_only =
            br#"{"message":{"role":"user","content":[{"type":"text","text":"use WebSearch"}]}}"#;
        assert_eq!(hit(tool_hit, named_only, b"websearch"), None);
    }
}

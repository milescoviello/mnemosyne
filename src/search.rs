//! Deep search across transcript bodies.
//!
//! Fuzzy matching on titles is done in memory by the UI; this module is for the
//! expensive kind -- "which session did I fix the NVENC thing in" -- which has
//! to read the actual conversations. A parallel byte scan over the corpus lands
//! around two tenths of a second, so it runs on a background thread with the
//! results folded in when they arrive.

use crate::model::Session;
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

/// ASCII case-insensitive substring search. `needle` must already be lowercase.
/// Candidate positions come from memchr on both cases of the first byte, which
/// keeps this close to a plain memmem while avoiding lowercasing the haystack.
pub(crate) fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let lo = needle[0];
    let up = lo.to_ascii_uppercase();
    let mut start = 0usize;
    let limit = hay.len() - needle.len() + 1;
    while start < limit {
        let rel = if lo == up {
            memchr::memchr(lo, &hay[start..limit])?
        } else {
            memchr::memchr2(lo, up, &hay[start..limit])?
        };
        let at = start + rel;
        if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        start = at + 1;
    }
    None
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
fn is_conversation(line: &[u8]) -> bool {
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
/// its alphabet happily spells short words like "nvenc" by chance. Prose has
/// spaces; a blob does not, so a wide window with no whitespace in it is the
/// tell.
fn looks_like_blob(line: &[u8], at: usize) -> bool {
    const W: usize = 70;
    let lo = at.saturating_sub(W);
    let hi = (at + W).min(line.len());
    if hi - lo < W {
        return false;
    }
    // Nor anything outside ASCII, which base64 never is. Japanese and
    // Chinese go a long way without an ASCII space or full stop -- their
    // punctuation is full-width -- and a word from the middle of a long
    // sentence was rejected as a blob, so neither engine could find it.
    !line[lo..hi]
        .iter()
        .any(|c| matches!(c, b' ' | b'\\' | b'\t' | b'>' | b',' | b'.' | b';') || *c >= 0x80)
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

/// First occurrence of `needle` that is real conversation, not injected context.
fn content_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    if !is_conversation(line) {
        return None;
    }
    let first = find_ci(line, needle)?;
    let spans = injected_spans(line);
    let bad = |at: usize| {
        spans.iter().any(|(a, b)| at >= *a && at < *b)
            || looks_like_blob(line, at)
            || in_key(line, at, needle.len())
    };
    if !bad(first) {
        return Some(first);
    }
    let mut at = first;
    loop {
        let next_rel = find_ci(&line[at + 1..], needle)?;
        at = at + 1 + next_rel;
        if !bad(at) {
            return Some(at);
        }
    }
}

/// Does this line record a tool actually touching `needle` as a path?
fn file_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    for key in [
        &b"\"file_path\":\""[..],
        &b"\"notebook_path\":\""[..],
        &b"\"path\":\""[..],
    ] {
        let mut from = 0;
        while let Some(rel) = memmem::find(&line[from..], key) {
            let vs = from + rel + key.len();
            if let Some(end) = memchr::memchr(b'"', &line[vs..]) {
                let val = &line[vs..vs + end];
                if find_ci(val, needle).is_some() {
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

fn tool_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
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
            let val = &line[vs..vs + end];
            if find_ci(val, needle).is_some() {
                return Some(vs);
            }
            from = vs + end;
        } else {
            break;
        }
    }
    None
}

/// Turn a user's words into an FTS5 expression they cannot break.
///
/// Several words are treated as a phrase, which is what a substring search
/// meant before; a single word gets a prefix match so "nvenc" still finds
/// "nvenc's". Quotes are doubled so no input can be read as syntax.
pub fn fts_expr(query: &str) -> String {
    let cleaned: Vec<String> = query
        .split_whitespace()
        .map(|w| w.replace('"', "\"\""))
        .filter(|w| !w.is_empty())
        .collect();
    match cleaned.len() {
        0 => String::new(),
        // The trailing `*` makes the last word a prefix, so "pool" finds
        // "pooling". A phrase gets the same treatment: without it,
        // "connection pool" missed "connection pooling" and "page fault"
        // missed "page faults", while the single-word form of either would
        // have found them. One rule, not two.
        1 => format!("\"{}\"*", cleaned[0]),
        _ => format!("\"{}\"*", cleaned.join(" ")),
    }
}

/// Parents of the subagents that matched.
///
/// A subagent is not something you resume on its own; the session that
/// spawned it is. The browser reveals the parent when the answer was found
/// in one of its children, and the command line did not -- so the same
/// query gave two different answers depending on where you asked it.
pub fn parents_of_hits(
    sessions: &[crate::model::Session],
    hits: &HashMap<String, String>,
) -> std::collections::HashSet<String> {
    sessions
        .iter()
        .filter(|s| s.is_subagent && hits.contains_key(&s.path.to_string_lossy().to_string()))
        .filter_map(|s| s.parent.clone())
        .collect()
}

/// A readable excerpt of indexed prose around the first mention.
///
/// FTS5 has `snippet()` for this, and it costs about 80ms per document --
/// nothing for a normal search, and eighty-five seconds for a word that
/// appears in every transcript. Cutting the window out of the stored text
/// does the same job for the whole corpus in under a second.
pub fn excerpt(text: &str, needle: &str) -> String {
    const PAD: usize = 90;
    let hay = text.as_bytes();
    // The whole phrase first, then its words. FTS tokenises on punctuation,
    // so "page fault" matches a transcript that only ever wrote
    // "page-fault" -- the query never appears in it literally, and landing
    // on the first word is far more use than the opening line.
    // Folded the way `find_ci` folds, ASCII only: a Unicode lowercase
    // turned `Москве` into `москве`, which then never matched the text it
    // was typed from.
    let lowered = needle.trim().to_ascii_lowercase();
    let at = find_ci(hay, lowered.as_bytes()).or_else(|| {
        lowered
            .split_whitespace()
            .find_map(|w| find_ci(hay, w.as_bytes()))
    });
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

/// Scan one file, returning the first readable hit.
fn search_file(path: &std::path::Path, needle: &[u8], mode: Mode) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut rdr = BufReader::with_capacity(1 << 18, f);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 14);
    loop {
        buf.clear();
        let n = rdr.read_until(b'\n', &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        let line = &buf[..n];
        let hit = match mode {
            Mode::Content | Mode::Everything => content_hit(line, needle),
            Mode::File => file_hit(line, needle),
            Mode::Tool => tool_hit(line, needle),
        };
        if let Some(at) = hit {
            return Some(snippet(line, at));
        }
        if buf.capacity() > (1 << 20) {
            buf = Vec::with_capacity(1 << 14);
        }
    }
}

/// Search every session in parallel. Returns path -> snippet for hits only.
fn brute(sessions: &[Session], needle: &[u8], mode: Mode) -> HashMap<String, String> {
    sessions
        .par_iter()
        .filter_map(|s| {
            search_file(&s.path, needle, mode)
                .map(|snip| (s.path.to_string_lossy().to_string(), snip))
        })
        .collect()
}

/// How a set of results was produced, so the interface can say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    Indexed,
    Scanned,
}

/// Find sessions matching `query`.
///
/// Content searches go through the full-text index, which covers the ~2% of
/// the corpus that is actually prose and answers in milliseconds instead of
/// re-reading gigabytes. The index tokenises, so it cannot match the middle of
/// a word; when it finds nothing we fall back to the exhaustive scan rather
/// than claiming there is nothing there. File and tool searches always scan,
/// because they query structure rather than prose.
pub fn run(sessions: &[Session], query: &str, mode: Mode) -> (HashMap<String, String>, How) {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (HashMap::new(), How::Indexed);
    }

    if mode == Mode::Content {
        let expr = fts_expr(&needle);
        if !expr.is_empty() {
            if let Ok(idx) = crate::index::Index::open() {
                // What the tokenizer would lose is looked for as written, in
                // the same prose: a tenth of the corpus rather than all of
                // it, and it comes with its excerpts. Everything else goes
                // to the index, whose excerpts are filled in one row at a
                // time, on demand.
                let found = if beyond_tokens(query) {
                    idx.prose_containing(query.trim())
                } else {
                    idx.search_text(&expr)
                        .map(|paths| paths.into_iter().map(|p| (p, String::new())).collect())
                };
                if let Ok(found) = found {
                    let known: std::collections::HashSet<String> = sessions
                        .iter()
                        .map(|s| s.path.to_string_lossy().to_string())
                        .collect();
                    let kept: HashMap<String, String> = found
                        .into_iter()
                        .filter(|(p, _)| known.contains(p))
                        .collect();
                    if !kept.is_empty() {
                        return (kept, How::Indexed);
                    }
                }
            }
        }
    }
    (brute(sessions, &scan_needle(query), mode), How::Scanned)
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
fn beyond_tokens(query: &str) -> bool {
    let telling = |c: char| !c.is_alphanumeric() && !".,;:!?\"'()[]{}".contains(c);
    query
        .split_whitespace()
        .any(|w| w.chars().next().is_some_and(telling) || w.chars().last().is_some_and(telling))
        || query.chars().any(unspaced)
}

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
        search_file(&path, &scan_needle(query), mode)
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
    }

    #[test]
    fn fts_expressions_cannot_be_broken_by_input() {
        // one word gets a prefix match, so "nvenc" still finds "nvenc's"
        assert_eq!(fts_expr("nvenc"), r#""nvenc"*"#);
        // several words become a phrase, which is what substring meant
        assert_eq!(fts_expr("page fault"), r#""page fault"*"#);
        assert_eq!(fts_expr("  page   fault  "), r#""page fault"*"#);
        // quotes are doubled so nothing can be read as FTS syntax
        assert_eq!(fts_expr(r#"say "hi""#), r#""say ""hi"""*"#);
        // operators are inert inside a quoted term
        assert_eq!(fts_expr("a OR b"), r#""a OR b"*"#);
        assert_eq!(fts_expr(""), "");
        assert_eq!(fts_expr("   "), "");
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
    fn a_prefix_match_without_the_literal_word_still_shows_something() {
        // "connection pool" matches "connection pooling", so the query
        // itself need not appear anywhere in the text.
        let e = excerpt("we fixed the connection pooling at last", "connection pool");
        assert!(!e.is_empty());
        assert!(e.contains("pooling"), "{e:?}");
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
        assert_eq!(content_hit(WITH_REMINDER.as_bytes(), b"nvenc"), None);
        // but text outside the reminder in the same line still matches
        assert!(content_hit(WITH_REMINDER.as_bytes(), b"disk").is_some());
        // and a plain mention matches normally
        assert!(content_hit(PLAIN_USER.as_bytes(), b"nvenc").is_some());
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
        assert_eq!(content_hit(blob.as_bytes(), b"nvenc"), None);
    }

    #[test]
    fn short_base64_run_is_not_treated_as_a_blob() {
        // Documented limit: the test needs a clear window either side, so a
        // run shorter than that is allowed through. Harmless -- a chance hit
        // in a few dozen characters is rare, and rejecting it would risk
        // discarding real prose.
        let short = wrap("QUJDnvencREVG");
        assert!(content_hit(short.as_bytes(), b"nvenc").is_some());
    }

    #[test]
    fn file_search_matches_tool_paths_not_mentions() {
        let edit = br#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/etc/portage/make.conf"}}]}}"#;
        assert!(file_hit(edit, b"make.conf").is_some());
        // a mere mention in prose is not a file the session touched
        let chat = br#"{"message":{"role":"user","content":[{"type":"text","text":"what is in make.conf"}]}}"#;
        assert_eq!(file_hit(chat, b"make.conf"), None);
    }

    #[test]
    fn tool_search_needs_a_tool_use_block() {
        let used = br#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"WebSearch","input":{}}]}}"#;
        assert!(tool_hit(used, b"websearch").is_some());
        let named_only =
            br#"{"message":{"role":"user","content":[{"type":"text","text":"use WebSearch"}]}}"#;
        assert_eq!(tool_hit(named_only, b"websearch"), None);
    }
}

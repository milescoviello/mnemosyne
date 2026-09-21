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
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
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
    let lo = at.saturating_sub(PAD);
    let hi = (at + PAD).min(line.len());
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
    !line[lo..hi]
        .iter()
        .any(|c| matches!(c, b' ' | b'\\' | b'\t' | b'>' | b',' | b'.' | b';'))
}

/// First occurrence of `needle` that is real conversation, not injected context.
fn content_hit(line: &[u8], needle: &[u8]) -> Option<usize> {
    if !is_conversation(line) {
        return None;
    }
    let first = find_ci(line, needle)?;
    let spans = injected_spans(line);
    let bad =
        |at: usize| spans.iter().any(|(a, b)| at >= *a && at < *b) || looks_like_blob(line, at);
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
        1 => format!("\"{}\"*", cleaned[0]),
        _ => format!("\"{}\"", cleaned.join(" ")),
    }
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
                if let Ok(paths) = idx.search_text(&expr) {
                    let known: std::collections::HashSet<String> = sessions
                        .iter()
                        .map(|s| s.path.to_string_lossy().to_string())
                        .collect();
                    // Excerpts are filled in one row at a time, on demand.
                    let kept: HashMap<String, String> = paths
                        .into_iter()
                        .filter(|p| known.contains(p))
                        .map(|p| (p, String::new()))
                        .collect();
                    if !kept.is_empty() {
                        return (kept, How::Indexed);
                    }
                }
            }
        }
    }
    (brute(sessions, needle.as_bytes(), mode), How::Scanned)
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

    #[test]
    fn fts_expressions_cannot_be_broken_by_input() {
        // one word gets a prefix match, so "nvenc" still finds "nvenc's"
        assert_eq!(fts_expr("nvenc"), r#""nvenc"*"#);
        // several words become a phrase, which is what substring meant
        assert_eq!(fts_expr("page fault"), r#""page fault""#);
        assert_eq!(fts_expr("  page   fault  "), r#""page fault""#);
        // quotes are doubled so nothing can be read as FTS syntax
        assert_eq!(fts_expr(r#"say "hi""#), r#""say ""hi""""#);
        // operators are inert inside a quoted term
        assert_eq!(fts_expr("a OR b"), r#""a OR b""#);
        assert_eq!(fts_expr(""), "");
        assert_eq!(fts_expr("   "), "");
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

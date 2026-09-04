//! Fenced `usine-*` blocks: the machine-readable payload an agent appends to its
//! reply, and the one convention every agent-I/O protocol in this module shares.
//!
//! Each protocol names its own tag — `usine-questions` (plan), `usine-review`
//! (review/triage), `usine-commit`, `usine-handoff`, `usine-fixes`,
//! `usine-plan`, `usine-findings` — and the agent emits it as
//! ```` ```<tag> ```` … ```` ``` ````. [`find`] pulls the payload out for parsing;
//! [`strip`] removes the whole block for display, since it's addressed to the
//! executor, not to the user. [`parse`] does both at once and reports a block
//! whose JSON is garbled instead of silently dropping it.
//!
//! The scan is line-wise and CommonMark-shaped: an opening fence is a line
//! indented at most 3 spaces made of at least three backticks (or tildes)
//! followed by the tag, and it closes on a later line of the same fence
//! character, at least as long. That matters because payloads increasingly
//! carry prose, and prose carries code fences: a naive "first ``` after the
//! tag" scan truncates the payload and destroys the whole block.
//!
//! Everything here is tolerant by design: a missing block yields `None` /
//! [`Block::Absent`] / the text unchanged, so a run whose agent forgot the
//! block degrades to the caller's default rather than failing.
//!
//! Payload convention: every block carries a `"v": <n>` schema version, absent
//! meaning v1, so a shape can grow without minting a new tag.

use serde::de::DeserializeOwned;

/// One complete block: its trimmed payload, plus the byte span (from the start
/// of the opening fence's line to the end of the closing fence's line, newline
/// excluded) that [`strip`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found<'a> {
    pub payload: &'a str,
    pub start: usize,
    pub end: usize,
}

/// The outcome of parsing a tagged block: parsed, present but unparseable, or
/// simply not there. The middle case is the one worth surfacing — the agent
/// tried to tell us something and garbled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block<T> {
    Ok(T),
    Malformed,
    Absent,
}

impl<T> Block<T> {
    /// The value, dropping the distinction between malformed and absent — for
    /// callers that only have a default to fall back to.
    pub fn ok(self) -> Option<T> {
        match self {
            Block::Ok(value) => Some(value),
            _ => None,
        }
    }

    /// The agent emitted a block of this tag but its JSON didn't parse.
    pub fn is_malformed(&self) -> bool {
        matches!(self, Block::Malformed)
    }
}

/// The payload inside the first block tagged `tag` (given without backticks),
/// trimmed. `None` when there's no opening fence or no closing one.
pub fn find<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    candidates(text, tag).first().map(|found| found.payload)
}

/// Every way the first block tagged `tag` could close, nearest close first.
///
/// More than one entry means the payload itself contains a fence the agent
/// didn't lengthen the opener for. CommonMark says the first close wins, and
/// [`find`] obeys that; [`parse`] walks the list so such a block can still be
/// rescued when a later close is the one that yields valid JSON.
pub fn candidates<'a>(text: &'a str, tag: &str) -> Vec<Found<'a>> {
    let Some(open) = opener(text, tag) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut offset = open.body;
    for line in text[open.body..].split_inclusive('\n') {
        let bare = bare(line);
        match fence_run(bare, open.fence) {
            Some(run) if run >= open.width && bare[run..].trim().is_empty() => {
                found.push(Found {
                    payload: text[open.body..offset].trim(),
                    start: open.start,
                    end: offset + bare.len(),
                });
            }
            _ => {}
        }
        offset += line.len();
    }
    found
}

/// `text` with the first block tagged `tag` removed, trimmed. Unchanged when
/// there's no such block. An unterminated block takes the rest of the text with
/// it — the fence swallowed everything after it anyway.
pub fn strip(text: &str, tag: &str) -> String {
    match candidates(text, tag).first() {
        Some(found) => cut(text, found.start, found.end),
        None => match opener(text, tag) {
            Some(open) => cut(text, open.start, text.len()),
            None => text.to_string(),
        },
    }
}

/// Parse the first block tagged `tag` as `T`, and return the text with the
/// block that actually parsed removed — the display prose, since the block is
/// addressed to the executor.
///
/// A [`Block::Malformed`] block is stripped too: garbage must never leak into a
/// transcript or a downstream prompt, but the caller can tell the user
/// something was dropped.
pub fn parse<T: DeserializeOwned>(text: &str, tag: &str) -> (Block<T>, String) {
    let found = candidates(text, tag);
    let Some(first) = found.first() else {
        return (Block::Absent, text.to_string());
    };
    for candidate in &found {
        if let Ok(value) = serde_json::from_str::<T>(candidate.payload) {
            return (Block::Ok(value), cut(text, candidate.start, candidate.end));
        }
    }
    (Block::Malformed, cut(text, first.start, first.end))
}

/// An opening fence: where its line starts, where the payload starts (past the
/// line's newline), and the fence character and width a close must match.
struct Opener {
    start: usize,
    body: usize,
    fence: char,
    width: usize,
}

/// The first line that opens a block tagged `tag`.
fn opener(text: &str, tag: &str) -> Option<Opener> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let bare = bare(line);
        for fence in ['`', '~'] {
            match fence_run(bare, fence) {
                Some(width) if bare[width..].trim() == tag => {
                    return Some(Opener {
                        start: offset,
                        body: offset + line.len(),
                        fence,
                        width,
                    });
                }
                _ => {}
            }
        }
        offset += line.len();
    }
    None
}

/// The line without its trailing newline. Byte offsets stay valid: the caller
/// only ever adds `bare.len()` to the line's own start.
fn bare(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// The width of a fence run of `fence` at the start of `bare` (CommonMark
/// allows up to 3 spaces of indent), or `None` when the line doesn't open or
/// close a fence. The returned width is a byte offset into `bare`, indent
/// included, so `bare[width..]` is the info string / trailing text.
fn fence_run(bare: &str, fence: char) -> Option<usize> {
    let indent = bare.len() - bare.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let run = bare[indent..].chars().take_while(|c| *c == fence).count();
    (run >= 3).then_some(indent + run)
}

/// `text` with `start..end` removed, trimmed.
fn cut(text: &str, start: usize, end: usize) -> String {
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_strips_a_tagged_block() {
        let text = "Prose.\n\n```usine-x\n[1, 2]\n```\n\nMore.";
        assert_eq!(find(text, "usine-x"), Some("[1, 2]"));
        assert_eq!(strip(text, "usine-x"), "Prose.\n\n\n\nMore.");
    }

    #[test]
    fn a_missing_or_unterminated_block_degrades_gracefully() {
        assert_eq!(find("no block", "usine-x"), None);
        assert_eq!(strip("no block", "usine-x"), "no block");
        // Opened but never closed: nothing to parse, and the fence eats the rest.
        assert_eq!(find("a\n```usine-x\n{", "usine-x"), None);
        assert_eq!(strip("a\n```usine-x\n{", "usine-x"), "a");
    }

    #[test]
    fn tags_dont_collide_with_each_other() {
        let text = "```usine-handoff\n{}\n```\n```usine-commit\nfeat: x\n```";
        assert_eq!(find(text, "usine-commit"), Some("feat: x"));
        assert_eq!(find(text, "usine-handoff"), Some("{}"));
        assert_eq!(
            strip(text, "usine-handoff"),
            "```usine-commit\nfeat: x\n```"
        );
    }

    #[test]
    fn a_nested_fence_no_longer_destroys_the_payload() {
        // The agent quoted a code fence inside a JSON string without lengthening
        // its own opener. CommonMark closes at the inner fence, so the first
        // candidate is truncated garbage — parse walks on to the one that works.
        let text = "Done.\n\n```usine-x\n{\"summary\": \"run ```cargo test```\"}\n```\n";
        let (parsed, rest) = parse::<serde_json::Value>(text, "usine-x");
        assert_eq!(
            parsed.ok().unwrap()["summary"],
            serde_json::json!("run ```cargo test```")
        );
        assert_eq!(rest, "Done.");
    }

    #[test]
    fn a_longer_or_tilde_opener_closes_on_its_own_kind() {
        // Four backticks: an inner three-backtick fence is payload, not a close.
        let text = "````usine-x\n[\"```rust\", \"```\"]\n````\nAfter.";
        assert_eq!(find(text, "usine-x"), Some("[\"```rust\", \"```\"]"));
        assert_eq!(strip(text, "usine-x"), "After.");

        let text = "~~~usine-x\n{\"a\": 1}\n~~~\nAfter.";
        assert_eq!(find(text, "usine-x"), Some("{\"a\": 1}"));
        assert_eq!(strip(text, "usine-x"), "After.");
    }

    #[test]
    fn an_indented_fence_is_still_a_block() {
        let text = "Prose.\n\n  ```usine-x\n  [1]\n  ```\n\nMore.";
        assert_eq!(find(text, "usine-x"), Some("[1]"));
        assert_eq!(strip(text, "usine-x"), "Prose.\n\n\n\nMore.");
        // Four spaces is an indented code block, not a fence.
        assert_eq!(find("    ```usine-x\n[1]\n    ```", "usine-x"), None);
    }

    #[test]
    fn parse_reports_malformed_absent_and_ok() {
        let (parsed, rest) = parse::<Vec<u8>>("A.\n```usine-x\nnot json\n```\nB.", "usine-x");
        assert_eq!(parsed, Block::Malformed);
        assert!(parsed.is_malformed());
        assert_eq!(rest, "A.\n\nB.", "garbage never survives into display");

        let (parsed, rest) = parse::<Vec<u8>>("A.", "usine-x");
        assert_eq!(parsed, Block::Absent);
        assert_eq!(rest, "A.");

        // Unterminated: not a complete block, so nothing is claimed or dropped.
        let (parsed, rest) = parse::<Vec<u8>>("A.\n```usine-x\n[1]", "usine-x");
        assert_eq!(parsed, Block::Absent);
        assert_eq!(rest, "A.\n```usine-x\n[1]");

        let (parsed, rest) = parse::<Vec<u8>>("A.\n```usine-x\n[1]\n```", "usine-x");
        assert_eq!(parsed.ok(), Some(vec![1]));
        assert_eq!(rest, "A.");
    }
}

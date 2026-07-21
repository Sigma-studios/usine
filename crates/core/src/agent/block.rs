//! Fenced `usine-*` blocks: the machine-readable payload an agent appends to its
//! reply, and the one convention every agent-I/O protocol in this module shares.
//!
//! Each protocol names its own tag — `usine-questions` (plan), `usine-review`
//! (review/triage), `usine-commit`, `usine-handoff` — and the agent emits it as
//! ```` ```<tag> ```` … ```` ``` ````. [`find`] pulls the payload out for parsing;
//! [`strip`] removes the whole block for display, since it's addressed to the
//! executor, not to the user.
//!
//! Both are tolerant by design: a missing block yields `None` / the text
//! unchanged, so a run whose agent forgot the block degrades to the caller's
//! default rather than failing.

/// The payload inside the first block tagged `tag` (given without backticks),
/// trimmed. `None` when there's no opening fence or no closing one.
pub fn find<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let (_, _, body) = locate(text, tag)?;
    Some(body)
}

/// `text` with the first block tagged `tag` removed, trimmed. Unchanged when
/// there's no such block. An unterminated block takes the rest of the text with
/// it — the fence swallowed everything after it anyway.
pub fn strip(text: &str, tag: &str) -> String {
    let open = format!("```{tag}");
    let Some(start) = text.find(&open) else {
        return text.to_string();
    };
    let after = &text[start + open.len()..];
    let end = match after.find("```") {
        Some(close) => start + open.len() + close + 3, // include the closing fence
        None => text.len(),
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out.trim().to_string()
}

/// `(start of the opening fence, end past the closing fence, trimmed payload)`.
fn locate<'a>(text: &'a str, tag: &str) -> Option<(usize, usize, &'a str)> {
    let open = format!("```{tag}");
    let start = text.find(&open)?;
    let after = &text[start + open.len()..];
    let close = after.find("```")?;
    Some((start, start + open.len() + close + 3, after[..close].trim()))
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
}

//! Commit-message generation for write runs.
//!
//! After an implement/fix run finishes, the executor commits the worktree.
//! Rather than a repeated, generic `usine: <title>` on every run, the agent is
//! asked to emit a one-line (optionally multi-line) message in a fenced
//! ` ```usine-commit ` block that follows the *repository's own* commit
//! conventions. The executor parses that block and uses it, falling back to the
//! title-based default when the agent didn't provide one.

use crate::agent::block;

const TAG: &str = "usine-commit";

/// Appended to implement/fix runs: ask the agent to author the commit message in
/// the project's own style. Kept provider-agnostic and self-contained.
pub const COMMIT_MESSAGE_INSTRUCTION: &str = "\
When you have finished making changes, write the commit message for this change. Match THIS \
repository's existing commit conventions — infer them from the recent history (`git log --oneline`) \
and any CONTRIBUTING or commit guidelines. If the history shows no clear convention, default to a \
Conventional-Commits-style type prefix such as `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, or \
`test:` (with an optional scope, e.g. `fix(auth): …`), picking the one that fits this change. \
NEVER prefix the message with `usine:`. \
Describe what THIS specific change does; do NOT reuse a generic or previous message. Emit it at the \
END of your reply, as a fenced code block tagged `usine-commit` containing only the message \
— a concise subject line, optionally followed by a blank line and a short body. Do NOT stage, \
commit, or push anything yourself; the message is all that's needed.";

/// Extract the agent's commit message from a fenced `usine-commit` block, if one
/// is present and non-empty. Returns `None` — so the caller uses its default —
/// when the block is missing or empty.
pub fn parse_commit_message(text: &str) -> Option<String> {
    block::find(text, TAG)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// The reply text with the `usine-commit` block removed, for display in the
/// transcript / recap (the block is machine-facing, not for the user to read).
pub fn strip_commit_block(text: &str) -> String {
    block::strip(text, TAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_block_for_display() {
        let text = "All fixed.\n\n```usine-commit\nfix: thing\n```";
        assert_eq!(strip_commit_block(text), "All fixed.");
        // No block → unchanged.
        assert_eq!(strip_commit_block("just prose"), "just prose");
    }

    #[test]
    fn parses_a_tagged_message() {
        let text = "Done.\n\n```usine-commit\nfeat(licensees): add filter side panel\n```";
        assert_eq!(
            parse_commit_message(text).as_deref(),
            Some("feat(licensees): add filter side panel")
        );
    }

    #[test]
    fn keeps_subject_and_body() {
        let text = "```usine-commit\nfix: guard empty verdicts\n\nSkip the picker when nothing is worth fixing.\n```";
        let msg = parse_commit_message(text).unwrap();
        assert!(msg.starts_with("fix: guard empty verdicts"));
        assert!(msg.contains("Skip the picker"));
    }

    #[test]
    fn missing_or_empty_block_is_none() {
        assert_eq!(parse_commit_message("no block here"), None);
        assert_eq!(parse_commit_message("```usine-commit\n\n```"), None);
    }
}

//! Syntax highlighting for diff lines.
//!
//! A single [`Highlighter`] (syntax set + theme) is built once behind a
//! `LazyLock` and reused for every diff. Only the theme's *foreground* colors are
//! emitted; the diff panel supplies its own line/gutter chrome (add/remove washes)
//! via CSS.
//!
//! v1 highlights each physical line independently (a fresh `HighlightLines` per
//! line). This is correct within a line but loses multi-line context — a block
//! comment or template literal spanning lines only highlights its first line —
//! because a hunk interleaves old-side and new-side lines that don't form one
//! coherent text. The emitted [`Token`] type does not change under the eventual
//! upgrade (highlight the whole old/new blobs once, index by line number).

use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::lang::Lang;
use super::Token;

/// Lines longer than this aren't tokenized (minified/generated files): fancy-regex
/// can be pathologically slow on very long lines. They render as one plain token.
const MAX_LINE_LEN: usize = 2_000;

/// Which bundled theme's foregrounds to emit. Only foreground colors are used, so
/// the theme's own background is irrelevant. Swappable in one place.
const THEME: &str = "base16-ocean.dark";

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

static HIGHLIGHTER: LazyLock<Highlighter> = LazyLock::new(Highlighter::load);

impl Highlighter {
    /// The process-wide highlighter, built on first use.
    pub fn global() -> &'static Highlighter {
        &HIGHLIGHTER
    }

    fn load() -> Self {
        // `two-face` bundles syntect's defaults plus TypeScript/TSX (and more) as
        // a prebuilt dump for the fancy-regex backend — no asset files to ship,
        // no `onig`. The `_newlines` variant expects lines to keep their '\n'.
        let syntaxes = two_face::syntax::extra_newlines();
        let theme = ThemeSet::load_defaults()
            .themes
            .remove(THEME)
            .expect("base16-ocean.dark ships with syntect's default themes");
        Highlighter { syntaxes, theme }
    }

    /// Tokenize one physical diff line. `content` should keep its trailing '\n'
    /// (git2 line content does) for correct tokenization; the newline is stripped
    /// from the returned tokens. A `None` language, an unresolved syntax, or an
    /// over-long line yields a single plain token (`color: None`).
    pub fn line_tokens(&self, lang: Option<Lang>, content: &str) -> Vec<Token> {
        let syntax = lang
            .filter(|_| content.len() <= MAX_LINE_LEN)
            .and_then(|l| self.syntaxes.find_syntax_by_extension(l.syntect_ext()));
        let Some(syntax) = syntax else {
            return vec![plain(content)];
        };
        let mut h = HighlightLines::new(syntax, &self.theme);
        match h.highlight_line(content, &self.syntaxes) {
            Ok(ranges) => finish(
                ranges
                    .into_iter()
                    .map(|(style, text)| Token {
                        text: text.to_string(),
                        color: Some(hex(style.foreground)),
                    })
                    .collect(),
            ),
            Err(_) => vec![plain(content)],
        }
    }
}

/// A single unhighlighted token, trailing newline removed.
fn plain(content: &str) -> Token {
    Token {
        text: content.trim_end_matches(['\n', '\r']).to_string(),
        color: None,
    }
}

/// Strip the trailing newline that lived in the last token, dropping it if it
/// becomes empty (so a code line doesn't end with a stray blank run).
fn finish(mut tokens: Vec<Token>) -> Vec<Token> {
    if let Some(last) = tokens.last_mut() {
        while last.text.ends_with('\n') || last.text.ends_with('\r') {
            last.text.pop();
        }
        if last.text.is_empty() {
            tokens.pop();
        }
    }
    tokens
}

/// Format a theme color as a CSS `#rrggbb` string (alpha dropped — the panel
/// controls opacity via its own line backgrounds).
fn hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_pads_each_channel() {
        assert_eq!(
            hex(Color {
                r: 0,
                g: 15,
                b: 255,
                a: 255
            }),
            "#000fff"
        );
    }

    #[test]
    fn unsupported_language_yields_one_plain_token() {
        let toks = Highlighter::global().line_tokens(None, "let x = 1;\n");
        assert_eq!(
            toks,
            vec![Token {
                text: "let x = 1;".into(),
                color: None
            }]
        );
    }

    #[test]
    fn over_long_line_is_not_highlighted() {
        let long = format!("{}\n", "a".repeat(MAX_LINE_LEN + 1));
        let toks = Highlighter::global().line_tokens(Some(Lang::Rust), &long);
        assert_eq!(toks.len(), 1);
        assert!(toks[0].color.is_none());
    }

    #[test]
    fn rust_line_is_highlighted_into_colored_runs() {
        let toks = Highlighter::global().line_tokens(Some(Lang::Rust), "fn main() {}\n");
        // Multiple runs, at least one carrying a color, and no trailing newline.
        assert!(toks.len() > 1, "expected tokenized runs, got {toks:?}");
        assert!(toks.iter().any(|t| t.color.is_some()));
        assert!(!toks.iter().any(|t| t.text.contains('\n')));
    }
}

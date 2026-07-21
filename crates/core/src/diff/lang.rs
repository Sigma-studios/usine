//! The language registry for diff highlighting.
//!
//! This is the one place to extend to support a new language. The registry
//! deliberately *gates* which languages get highlighted — even though the
//! underlying syntax set (via `two-face`) recognizes many more, we only
//! highlight the ones listed here so the diff view stays predictable.

use std::path::Path;

/// A language the diff viewer highlights today.
///
/// ADD A LANGUAGE: add a variant here, its extension token in [`Lang::syntect_ext`],
/// and the file extensions that map to it in [`lang_for_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
}

impl Lang {
    /// The file-extension token used to resolve a syntax with
    /// `SyntaxSet::find_syntax_by_extension`. These match extensions declared by
    /// syntect's defaults and the `two-face` bundle.
    pub fn syntect_ext(self) -> &'static str {
        match self {
            Lang::Rust => "rs",
            Lang::JavaScript => "js",
            Lang::Jsx => "jsx",
            Lang::TypeScript => "ts",
            Lang::Tsx => "tsx",
        }
    }
}

/// Map a path's extension to a supported language. `None` → render plain (no
/// highlighting). ADD A LANGUAGE: one arm here.
pub fn lang_for_path(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "js" | "mjs" | "cjs" => Lang::JavaScript,
        "jsx" => Lang::Jsx,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn known_extensions_map_to_languages() {
        assert_eq!(lang_for_path(Path::new("src/main.rs")), Some(Lang::Rust));
        assert_eq!(
            lang_for_path(Path::new("a/b/app.js")),
            Some(Lang::JavaScript)
        );
        assert_eq!(lang_for_path(Path::new("mod.mjs")), Some(Lang::JavaScript));
        assert_eq!(lang_for_path(Path::new("View.jsx")), Some(Lang::Jsx));
        assert_eq!(lang_for_path(Path::new("api.ts")), Some(Lang::TypeScript));
        assert_eq!(lang_for_path(Path::new("Page.tsx")), Some(Lang::Tsx));
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(lang_for_path(Path::new("MAIN.RS")), Some(Lang::Rust));
    }

    #[test]
    fn unknown_or_missing_extension_is_none() {
        assert_eq!(lang_for_path(Path::new("go.mod")), None);
        assert_eq!(lang_for_path(Path::new("data.json")), None);
        assert_eq!(lang_for_path(Path::new("Makefile")), None);
    }
}

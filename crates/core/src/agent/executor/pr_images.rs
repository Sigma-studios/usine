//! Screenshots pasted into the PR description. The form inserts a placeholder
//! link — `![pasted-N](usine-image:<id>)`, `<id>` being the 8-hex prefix of the
//! card attachment holding the image — and `create_pr` swaps each placeholder
//! URL for the one the forge hosted the image at. Pure string work, so the
//! whole round-trip is unit-tested here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// The URL scheme of a pasted-image placeholder.
pub const PLACEHOLDER_SCHEME: &str = "usine-image:";

/// The unique placeholder ids in `body`, in order of first appearance. Only a
/// well-formed id (8 lowercase hex chars) counts, so prose that merely mentions the
/// scheme is left alone.
pub fn find_placeholders(body: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (at, _) in body.match_indices(PLACEHOLDER_SCHEME) {
        let rest = &body[at + PLACEHOLDER_SCHEME.len()..];
        let id: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        if is_placeholder_id(&id) && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// Whether `id` has the shape of an attachment prefix: 8 lowercase hex chars
/// (the form a UUID prints in).
pub fn is_placeholder_id(id: &str) -> bool {
    id.len() == 8 && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The attachment file each id names (the one whose file name starts with
/// `<id>-`), in the order of `ids`. An id with no attachment means the image
/// was removed from the card after it was pasted.
pub fn resolve(ids: &[String], attachments: &[PathBuf]) -> Result<Vec<PathBuf>> {
    ids.iter()
        .map(|id| {
            let prefix = format!("{id}-");
            attachments
                .iter()
                .find(|p| file_name(p).starts_with(&prefix))
                .cloned()
                .ok_or_else(|| {
                    CoreError::other(format!(
                        "image `{id}` was removed from the card; delete its reference \
                         from the description"
                    ))
                })
        })
        .collect()
}

/// `body` with every placeholder URL replaced by its hosted URL in `urls`
/// (keyed by id). The alt text is untouched; an id missing from `urls` stays
/// as it was.
pub fn rewrite(body: &str, urls: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find(PLACEHOLDER_SCHEME) {
        out.push_str(&rest[..at]);
        let after = &rest[at + PLACEHOLDER_SCHEME.len()..];
        let id: String = after.chars().take_while(char::is_ascii_hexdigit).collect();
        match urls.get(&id).filter(|_| is_placeholder_id(&id)) {
            Some(url) => {
                out.push_str(url);
                rest = &after[id.len()..];
            }
            None => {
                out.push_str(PLACEHOLDER_SCHEME);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unique_well_formed_ids_in_order() {
        let body = "a ![pasted-1](usine-image:0a1b2c3d) b ![x](usine-image:ffffffff)\n\
                    again ![pasted-1](usine-image:0a1b2c3d) bad usine-image:xyz";
        assert_eq!(find_placeholders(body), vec!["0a1b2c3d", "ffffffff"]);
    }

    #[test]
    fn a_body_without_placeholders_has_none() {
        assert!(find_placeholders("Fixes the thing.\n\n![shot](https://x/y.png)").is_empty());
        assert!(find_placeholders("").is_empty());
    }

    #[test]
    fn a_too_long_hex_run_is_not_an_id() {
        assert!(find_placeholders("usine-image:0a1b2c3d4").is_empty());
    }

    #[test]
    fn resolves_ids_to_attachments_by_prefix() {
        let atts = vec![
            PathBuf::from("/a/11111111-notes.txt"),
            PathBuf::from("/a/0a1b2c3d-pasted-2.png"),
        ];
        let ids = vec!["0a1b2c3d".to_string()];
        assert_eq!(
            resolve(&ids, &atts).unwrap(),
            vec![PathBuf::from("/a/0a1b2c3d-pasted-2.png")]
        );
    }

    #[test]
    fn a_removed_attachment_is_an_error_naming_the_id() {
        let err = resolve(&["deadbeef".to_string()], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("deadbeef") && err.contains("removed"), "{err}");
    }

    #[test]
    fn rewrite_swaps_every_occurrence_and_keeps_alt_text() {
        let body = "![pasted-1](usine-image:0a1b2c3d)\n![again](usine-image:0a1b2c3d)";
        let urls = HashMap::from([("0a1b2c3d".to_string(), "https://h/i.png".to_string())]);
        assert_eq!(
            rewrite(body, &urls),
            "![pasted-1](https://h/i.png)\n![again](https://h/i.png)"
        );
    }

    #[test]
    fn rewrite_leaves_unknown_and_malformed_placeholders_alone() {
        let body = "usine-image:12345678 and usine-image:zz é";
        assert_eq!(rewrite(body, &HashMap::new()), body);
    }
}

//! Small reusable form widgets shared by the card config form and the global
//! settings panel.

use dioxus::prelude::*;
use usine_core::{supported_efforts, Effort, ModelSpec, Provider, SEVERITY_LEVELS};

/// Selectable model ids per provider.
pub(crate) fn models_for(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["opus", "sonnet", "haiku", "fable", "claude-fable-5-1"],
        // Models available through Codex with ChatGPT authentication, newest
        // first. Older entries remain selectable for plan-dependent access.
        Provider::Codex => &[
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-5.4",
        ],
    }
}

/// Display text for a model id. Ids that are already friendly (the Claude
/// aliases, the Codex ids) print as-is.
pub(crate) fn model_label(model: &str) -> &str {
    match model {
        "claude-fable-5-1" => "fable 5.1",
        other => other,
    }
}

pub(crate) fn provider_value(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

pub(crate) fn parse_provider(s: &str) -> Provider {
    match s {
        "codex" => Provider::Codex,
        _ => Provider::Claude,
    }
}

pub(crate) fn parse_effort(s: &str) -> Effort {
    match s {
        "low" => Effort::Low,
        "high" => Effort::High,
        "xhigh" => Effort::XHigh,
        "max" => Effort::Max,
        "ultra" => Effort::Ultra,
        _ => Effort::Medium,
    }
}

/// The criticality of a drafted review comment, as an editable pill. The
/// maintainer owns this rating — it is published to the contributor alongside
/// the comment, so they can correct one they disagree with, or pick `\u{2014}`
/// to clear it and post the comment untagged.
///
/// One component for both validation surfaces (the detail panel's list and the
/// diff viewer's inline thread) so the two never drift apart.
#[component]
pub(crate) fn SeverityPicker(severity: String, on_change: EventHandler<String>) -> Element {
    let class = if severity.is_empty() {
        "sev".to_string()
    } else {
        format!("sev sev-{severity}")
    };
    rsx! {
        select {
            class: "{class}",
            value: "{severity}",
            title: "Criticality published with this comment",
            "aria-label": "Criticality",
            onchange: move |e: Event<FormData>| on_change.call(e.value()),
            option { value: "", selected: severity.is_empty(), "\u{2014}" }
            for level in SEVERITY_LEVELS.iter() {
                option { value: "{level}", selected: severity.as_str() == *level, "{level}" }
            }
        }
    }
}

/// A model dropdown + effort dropdown. Emits the updated [`ModelSpec`] on change.
#[component]
pub(crate) fn ModelEffortPicker(
    provider: Provider,
    spec: ModelSpec,
    on_change: EventHandler<ModelSpec>,
) -> Element {
    let models = models_for(provider);
    let model = spec.model.clone();
    let effort = spec.effort;

    rsx! {
        div { class: "row",
            select {
                value: "{model}",
                onchange: {
                    let spec = spec.clone();
                    // Switching models can strip the current effort (e.g. leaving a
                    // non-max Codex model on `xhigh`), so clamp it to the new model.
                    move |e: Event<FormData>| {
                        let model = e.value();
                        let effort = spec.effort.clamp_to(supported_efforts(provider, &model));
                        on_change.call(ModelSpec { model, effort })
                    }
                },
                for m in models.iter() {
                    option { value: "{m}", selected: model.as_str() == *m, "{model_label(m)}" }
                }
            }
            select {
                value: effort.label(),
                onchange: {
                    let spec = spec.clone();
                    move |e: Event<FormData>| on_change.call(ModelSpec { model: spec.model.clone(), effort: parse_effort(&e.value()) })
                },
                for ef in supported_efforts(provider, &model).iter().copied() {
                    option { value: ef.label(), selected: ef == effort, "{ef.label()}" }
                }
            }
        }
    }
}

/// A [`ModelEffortPicker`] with a leading "inherit" option that clears the
/// override, emitting `None`. Used for the review phase, which falls back to the
/// implement spec when unset. The effort dropdown is hidden while inheriting —
/// there's no independent effort to show, it comes from whatever is inherited.
#[component]
pub(crate) fn OptionalModelEffortPicker(
    provider: Provider,
    spec: Option<ModelSpec>,
    /// Label for the inherit option, e.g. "Same as implement".
    inherit_label: String,
    on_change: EventHandler<Option<ModelSpec>>,
) -> Element {
    let models = models_for(provider);
    let model = spec.as_ref().map(|s| s.model.clone()).unwrap_or_default();
    let effort = spec.as_ref().map(|s| s.effort);

    rsx! {
        div { class: "row",
            select {
                value: "{model}",
                onchange: {
                    let spec = spec.clone();
                    move |e: Event<FormData>| {
                        let model = e.value();
                        if model.is_empty() {
                            on_change.call(None);
                            return;
                        }
                        // Coming from inherit there's no effort to carry over, so
                        // seed Medium — the one tier every model offers. Clamp
                        // regardless, for the same reason the picker above does.
                        let effort = spec
                            .as_ref()
                            .map(|s| s.effort)
                            .unwrap_or(Effort::Medium)
                            .clamp_to(supported_efforts(provider, &model));
                        on_change.call(Some(ModelSpec { model, effort }))
                    }
                },
                option { value: "", selected: model.is_empty(), "{inherit_label}" }
                for m in models.iter() {
                    option { value: "{m}", selected: model.as_str() == *m, "{model_label(m)}" }
                }
            }
            if let Some(effort) = effort {
                select {
                    value: effort.label(),
                    onchange: {
                        let model = model.clone();
                        move |e: Event<FormData>| on_change.call(Some(ModelSpec { model: model.clone(), effort: parse_effort(&e.value()) }))
                    },
                    for ef in supported_efforts(provider, &model).iter().copied() {
                        option { value: ef.label(), selected: ef == effort, "{ef.label()}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_picker_lists_the_gpt_5_6_family() {
        let models = models_for(Provider::Codex);
        assert!(models.contains(&"gpt-5.6-sol"));
        assert!(models.contains(&"gpt-5.6-terra"));
        assert!(models.contains(&"gpt-5.6-luna"));
    }

    #[test]
    fn claude_picker_pins_fable_5_1_while_keeping_the_alias() {
        let models = models_for(Provider::Claude);
        assert!(models.contains(&"claude-fable-5-1"));
        // Dropping the bare alias would strand cards already configured on it:
        // the select would render blank and silently switch model on next edit.
        assert!(models.contains(&"fable"));
    }

    #[test]
    fn only_the_pinned_ids_get_a_friendlier_label() {
        assert_eq!(model_label("claude-fable-5-1"), "fable 5.1");
        assert_eq!(model_label("opus"), "opus");
        assert_eq!(model_label("gpt-5.6-sol"), "gpt-5.6-sol");
    }

    #[test]
    fn ultra_effort_is_parsed() {
        assert_eq!(parse_effort("ultra"), Effort::Ultra);
    }
}

/// A tab strip scoped to one artifact section (the hand-off, the plan), reusing
/// the settings dialog's tab look.
///
/// Section-scoped on purpose: the detail panel is itself a scroll surface with
/// anchored actions, so a panel-wide tab bar would hide the primary button and
/// fight `focus_section`. The selection signal belongs to the calling section,
/// never to the panel — the panel body remounts on every state change.
#[component]
pub(crate) fn ArtifactTabs(
    labels: Vec<String>,
    active: usize,
    onselect: EventHandler<usize>,
) -> Element {
    rsx! {
        div { class: "settings-tabs artifact-tabs",
            for (i, label) in labels.iter().enumerate() {
                button {
                    key: "{i}",
                    class: if i == active { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| onselect.call(i),
                    "{label}"
                }
            }
        }
    }
}

/// A block of agent prose (a summary, a plan, a conclusion), rendered as
/// markdown-lite: headings, bullets, numbered steps, inline code, fenced code,
/// and `path:line` references turned into chips.
///
/// Every artifact goes through this one component, so the whole app's prose
/// reads the same and improves in one place. Agent text is always built into
/// rsx *elements* — never inner HTML: it quotes repo content into the webview
/// that hosts the Dioxus bridge.
///
/// `on_path` makes the `path:line` chips clickable; without it they are just
/// legible. The caller decides what a path means (a diff to open, a file to
/// edit), because that differs by panel.
#[component]
pub(crate) fn ArtifactText(text: String, on_path: Option<EventHandler<String>>) -> Element {
    rsx! {
        div { class: "artifact-box",
            for (i, block) in md_blocks(&text).into_iter().enumerate() {
                match block {
                    MdBlock::Heading(level, line) => rsx! {
                        div { key: "{i}", class: "md-h md-h{level}", MdLine { line, on_path } }
                    },
                    MdBlock::Code(code) => rsx! {
                        pre { key: "{i}", class: "md-code", "{code}" }
                    },
                    MdBlock::Bullets(items) => rsx! {
                        ul { key: "{i}", class: "md-list",
                            for (j, item) in items.into_iter().enumerate() {
                                li { key: "{j}", MdLine { line: item, on_path } }
                            }
                        }
                    },
                    MdBlock::Ordered(items) => rsx! {
                        ol { key: "{i}", class: "md-list",
                            for (j, item) in items.into_iter().enumerate() {
                                li { key: "{j}", MdLine { line: item, on_path } }
                            }
                        }
                    },
                    MdBlock::Para(lines) => rsx! {
                        p { key: "{i}", class: "md-p",
                            for (j, line) in lines.into_iter().enumerate() {
                                span { key: "{j}",
                                    // A hard break, not a paragraph: agents
                                    // write one-line-per-change lists inside a
                                    // single paragraph and mean them to stack.
                                    if j > 0 {
                                        br {}
                                    }
                                    MdLine { line, on_path }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// One line's worth of inline spans.
#[component]
fn MdLine(line: String, on_path: Option<EventHandler<String>>) -> Element {
    rsx! {
        for (i, span) in md_inline(&line).into_iter().enumerate() {
            match span {
                MdInline::Text(t) => rsx! { span { key: "{i}", "{t}" } },
                MdInline::Code(t) => rsx! { code { key: "{i}", class: "md-code-inline", "{t}" } },
                MdInline::Strong(t) => rsx! { strong { key: "{i}", "{t}" } },
                MdInline::Path(t) => match on_path {
                    Some(handler) => rsx! {
                        button {
                            key: "{i}",
                            class: "md-path link",
                            onclick: move |_| handler.call(t.clone()),
                            "{t}"
                        }
                    },
                    None => rsx! { span { key: "{i}", class: "md-path", "{t}" } },
                },
            }
        }
    }
}

/// The block structure this renderer understands. Deliberately small: agent
/// prose is headings, bullets, steps and code, and anything richer degrades to
/// a paragraph rather than to markup the reader has to decode.
#[derive(Debug, PartialEq, Eq)]
enum MdBlock {
    Heading(u8, String),
    Para(Vec<String>),
    Bullets(Vec<String>),
    Ordered(Vec<String>),
    Code(String),
}

/// Split prose into blocks. Unterminated fences run to the end of the text —
/// the agent opened a code block and never closed it, and showing the rest as
/// code beats showing the fence.
fn md_blocks(text: &str) -> Vec<MdBlock> {
    let mut out = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut bullets: Vec<String> = Vec::new();
    let mut ordered: Vec<String> = Vec::new();
    let mut code: Option<Vec<String>> = None;

    macro_rules! flush {
        () => {
            if !para.is_empty() {
                out.push(MdBlock::Para(std::mem::take(&mut para)));
            }
            if !bullets.is_empty() {
                out.push(MdBlock::Bullets(std::mem::take(&mut bullets)));
            }
            if !ordered.is_empty() {
                out.push(MdBlock::Ordered(std::mem::take(&mut ordered)));
            }
        };
    }

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if let Some(body) = &mut code {
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                out.push(MdBlock::Code(body.join("\n")));
                code = None;
            } else {
                body.push(line.to_string());
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush!();
            code = Some(Vec::new());
            continue;
        }
        if trimmed.is_empty() {
            flush!();
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            flush!();
            out.push(MdBlock::Heading(
                level,
                trimmed[level as usize..].trim().to_string(),
            ));
            continue;
        }
        if let Some(item) = bullet_item(trimmed) {
            if !para.is_empty() || !ordered.is_empty() {
                flush!();
            }
            bullets.push(item);
            continue;
        }
        if let Some(item) = ordered_item(trimmed) {
            if !para.is_empty() || !bullets.is_empty() {
                flush!();
            }
            ordered.push(item);
            continue;
        }
        // A continuation line under a list item belongs to it, not to a new
        // paragraph — agents wrap long bullets.
        if raw.starts_with("  ") {
            if let Some(last) = bullets.last_mut().or_else(|| ordered.last_mut()) {
                last.push(' ');
                last.push_str(trimmed);
                continue;
            }
        }
        if !bullets.is_empty() || !ordered.is_empty() {
            flush!();
        }
        para.push(line.trim_start().to_string());
    }
    if let Some(body) = code {
        out.push(MdBlock::Code(body.join("\n")));
    }
    flush!();
    out
}

/// `#` through `######`, returning the level (which is also the byte offset of
/// the text after the hashes).
fn heading_level(line: &str) -> Option<u8> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&hashes) && line[hashes..].starts_with(' ')).then_some(hashes as u8)
}

/// The text of a `- ` / `* ` bullet.
fn bullet_item(line: &str) -> Option<String> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .map(|s| s.trim().to_string())
}

/// The text of a `1. ` numbered item.
fn ordered_item(line: &str) -> Option<String> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..]
        .strip_prefix(". ")
        .map(|s| s.trim().to_string())
}

/// One inline run.
#[derive(Debug, PartialEq, Eq)]
enum MdInline {
    Text(String),
    Code(String),
    Strong(String),
    Path(String),
}

/// Split a line into inline runs: backticked code, `**bold**`, and file
/// references. Anything unmatched — including a stray backtick or asterisk —
/// stays literal text.
fn md_inline(line: &str) -> Vec<MdInline> {
    let mut out = Vec::new();
    let mut rest = line;
    let mut plain = String::new();

    while !rest.is_empty() {
        let delim = ["`", "**"]
            .into_iter()
            .filter_map(|d| rest.find(d).map(|i| (i, d)))
            .min();
        let Some((at, delim)) = delim else { break };
        let after = &rest[at + delim.len()..];
        let Some(end) = after.find(delim) else {
            // Unclosed: the delimiter is literal.
            plain.push_str(&rest[..at + delim.len()]);
            rest = after;
            continue;
        };
        plain.push_str(&rest[..at]);
        push_plain(&mut out, &mut plain);
        let inner = after[..end].to_string();
        out.push(if delim == "`" {
            MdInline::Code(inner)
        } else {
            MdInline::Strong(inner)
        });
        rest = &after[end + delim.len()..];
    }
    plain.push_str(rest);
    push_plain(&mut out, &mut plain);
    out
}

/// Flush accumulated plain text, pulling `path:line` references out of it.
fn push_plain(out: &mut Vec<MdInline>, plain: &mut String) {
    if plain.is_empty() {
        return;
    }
    let text = std::mem::take(plain);
    let mut pending = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        let trailing = word.len() - word.trim_end().len();
        let core = &word[..word.len() - trailing];
        let lead = core.len() - core.trim_start_matches(['(', '[', '`']).len();
        let inner = core[lead..].trim_end_matches([',', '.', ')', ']', ';', ':', '`']);
        if is_path_ref(inner) {
            pending.push_str(&core[..lead]);
            if !pending.is_empty() {
                out.push(MdInline::Text(std::mem::take(&mut pending)));
            }
            out.push(MdInline::Path(inner.to_string()));
            pending.push_str(&core[lead + inner.len()..]);
            pending.push_str(&word[word.len() - trailing..]);
        } else {
            pending.push_str(word);
        }
    }
    if !pending.is_empty() {
        out.push(MdInline::Text(pending));
    }
}

/// A path reference minus its trailing `:42`, when it carries one, plus whether
/// it did. Agents write both forms interchangeably.
fn split_line_suffix(word: &str) -> (&str, bool) {
    match word.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => (p, true),
        _ => (word, false),
    }
}

/// Whether a path written by an agent names the file at `actual`. Tolerant of a
/// shortened or extra-prefixed path and of a trailing `:line` — agents write
/// repo-relative paths but sometimes shorten or prefix them, and a near miss
/// should join (and scroll) rather than read as a phantom file.
pub fn same_path(claimed: &str, actual: &str) -> bool {
    let (claimed, _) = split_line_suffix(claimed.trim().trim_start_matches("./"));
    !claimed.is_empty()
        && (claimed == actual
            || actual.ends_with(&format!("/{claimed}"))
            || claimed.ends_with(&format!("/{actual}")))
}

/// Whether a word looks like a repo file reference — `src/cache.rs`,
/// `crates/app/src/style.css:504`. Conservative on purpose: turning ordinary
/// prose ("e.g.", "3.5") into chips would be worse than missing a reference.
fn is_path_ref(word: &str) -> bool {
    let (path, line) = split_line_suffix(word);
    if path.len() < 4 || path.ends_with('/') {
        return false;
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
    {
        return false;
    }
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return false;
    };
    // An extension, and either a directory separator or a line number — a bare
    // `foo.bar` in a sentence is not a file reference.
    !stem.is_empty()
        && (1..=6).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && (path.contains('/') || line)
}

#[cfg(test)]
mod md_tests {
    use super::*;

    #[test]
    fn matches_agent_paths_against_diff_paths() {
        assert!(same_path("crates/app/src/lib.rs", "crates/app/src/lib.rs"));
        // Shortened, extra-prefixed, `./`-prefixed, and `:line`-suffixed forms
        // all name the same file — the diff dialog and the Changes tab must
        // agree about that, which is why they share this one matcher.
        assert!(same_path("src/lib.rs", "crates/app/src/lib.rs"));
        assert!(same_path(
            "./crates/app/src/lib.rs:42",
            "crates/app/src/lib.rs"
        ));
        assert!(same_path(
            "repo/crates/app/src/lib.rs",
            "crates/app/src/lib.rs"
        ));
        assert!(!same_path("", "crates/app/src/lib.rs"));
        assert!(!same_path("other/lib.rs", "crates/app/src/lib.rs"));
    }

    #[test]
    fn splits_headings_lists_and_code() {
        let text = "## Plan\n\nDo it.\nAnd then some.\n\n- one\n- two\n\n1. first\n2. second\n\n```rust\nlet x = 1;\n```";
        let blocks = md_blocks(text);
        assert_eq!(blocks[0], MdBlock::Heading(2, "Plan".into()));
        assert_eq!(
            blocks[1],
            MdBlock::Para(vec!["Do it.".into(), "And then some.".into()])
        );
        assert_eq!(
            blocks[2],
            MdBlock::Bullets(vec!["one".into(), "two".into()])
        );
        assert_eq!(
            blocks[3],
            MdBlock::Ordered(vec!["first".into(), "second".into()])
        );
        assert_eq!(blocks[4], MdBlock::Code("let x = 1;".into()));
    }

    #[test]
    fn a_wrapped_bullet_stays_one_item() {
        let blocks = md_blocks("- a long bullet\n  that wrapped\n- another");
        assert_eq!(
            blocks[0],
            MdBlock::Bullets(vec!["a long bullet that wrapped".into(), "another".into()])
        );
    }

    #[test]
    fn an_unterminated_fence_shows_as_code_not_as_a_fence() {
        assert_eq!(
            md_blocks("```\nstuff"),
            vec![MdBlock::Code("stuff".into())],
            "the reader should never see a bare ```"
        );
    }

    #[test]
    fn inline_code_and_bold_split_out() {
        assert_eq!(
            md_inline("call `foo()` and **stop**"),
            vec![
                MdInline::Text("call ".into()),
                MdInline::Code("foo()".into()),
                MdInline::Text(" and ".into()),
                MdInline::Strong("stop".into()),
            ]
        );
        // An unclosed delimiter is literal, not a swallowed rest-of-line.
        assert_eq!(
            md_inline("a ` b"),
            vec![MdInline::Text("a ` b".into())],
            "an unclosed backtick must not eat the line"
        );
    }

    #[test]
    fn file_references_become_chips_and_prose_does_not() {
        assert_eq!(
            md_inline("see src/cache.rs:42, it grows"),
            vec![
                MdInline::Text("see ".into()),
                MdInline::Path("src/cache.rs:42".into()),
                MdInline::Text(", it grows".into()),
            ]
        );
        for prose in ["e.g. this", "about 3.5 times", "Node.js is fine"] {
            assert!(
                md_inline(prose)
                    .iter()
                    .all(|s| !matches!(s, MdInline::Path(_))),
                "{prose} must stay prose"
            );
        }
    }
}

#[cfg(test)]
mod md_fuzz_tests {
    use super::*;

    /// Byte-offset arithmetic over agent prose must never panic: the text is
    /// arbitrary UTF-8 (em dashes, accents, CJK) and full of stray delimiters.
    #[test]
    fn non_ascii_and_adversarial_prose_never_panics() {
        let cases = [
            "— em dash — and `côté.rs:12` here",
            "日本語 src/ファイル.rs:3 と **太字**",
            "```\nunclosed café\n",
            "**",
            "`",
            "*.*",
            "a/b.rs:",
            ":::",
            "(src/a.rs:1)",
            "[src/a.rs:1],",
            "naïve `**` mix ** `",
            "####### too many hashes",
            "1. ok\n2.no space\n   deep indent",
            "",
            "\u{200b}zero width",
        ];
        for case in cases {
            let blocks = md_blocks(case);
            for block in &blocks {
                match block {
                    MdBlock::Heading(_, l) | MdBlock::Code(l) => {
                        let _ = md_inline(l);
                    }
                    MdBlock::Para(v) | MdBlock::Bullets(v) | MdBlock::Ordered(v) => {
                        for l in v {
                            let _ = md_inline(l);
                        }
                    }
                }
            }
            // Inline parsing must also survive the raw line.
            let _ = md_inline(case);
        }
    }

    /// Whatever the inline splitter does, it must not lose or invent text:
    /// concatenating the runs back returns the input minus its delimiters.
    #[test]
    fn inline_runs_preserve_every_character() {
        for case in [
            "plain text",
            "a `b` c",
            "x **y** z",
            "src/a.rs:1 tail",
            "— é 日本",
        ] {
            let joined: String = md_inline(case)
                .into_iter()
                .map(|s| match s {
                    MdInline::Text(t)
                    | MdInline::Code(t)
                    | MdInline::Strong(t)
                    | MdInline::Path(t) => t,
                })
                .collect();
            let stripped: String = case.replace("**", "").replace('`', "");
            assert_eq!(joined, stripped, "input: {case}");
        }
    }
}

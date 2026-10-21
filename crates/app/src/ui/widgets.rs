//! Small reusable form widgets shared by the card config form and the global
//! settings panel.

use dioxus::prelude::*;
use usine_core::{supported_efforts, Effort, ModelSpec, Provider, SEVERITY_LEVELS};

/// Selectable model ids per provider.
pub(crate) fn models_for(provider: Provider) -> &'static [&'static str] {
    match provider {
        // The bare aliases track whatever Claude Code points them at today
        // (`opus` resolves to Opus 5.5, `fable` to Fable 5.1); the pinned ids
        // beside them stay on that exact version when the alias moves on.
        Provider::Claude => &[
            "opus",
            "claude-opus-5-5",
            "claude-opus-5",
            "sonnet",
            "haiku",
            "fable",
            "claude-fable-5-1",
        ],
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
        "claude-opus-5-5" => "opus 5.5",
        "claude-opus-5" => "opus 5",
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
    fn claude_picker_pins_versions_while_keeping_the_aliases() {
        let models = models_for(Provider::Claude);
        assert!(models.contains(&"claude-fable-5-1"));
        // Opus 5.5 is what the `opus` alias resolves to today; pinning it lets a
        // card stay on this version once the alias moves to the next Opus, and
        // `claude-opus-5` keeps the previous one reachable.
        assert!(models.contains(&"claude-opus-5-5"));
        assert!(models.contains(&"claude-opus-5"));
        // Dropping the bare aliases would strand cards already configured on
        // them: the select would render blank and silently switch model on next
        // edit.
        assert!(models.contains(&"fable"));
        assert!(models.contains(&"opus"));
    }

    #[test]
    fn only_the_pinned_ids_get_a_friendlier_label() {
        assert_eq!(model_label("claude-fable-5-1"), "fable 5.1");
        assert_eq!(model_label("claude-opus-5-5"), "opus 5.5");
        assert_eq!(model_label("claude-opus-5"), "opus 5");
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
pub(crate) fn ArtifactText(
    text: String,
    on_path: Option<EventHandler<String>>,
    /// Lay the blocks out without the boxed background — for prose that sits
    /// inside a box of its own (the amber question boxes).
    bare: Option<bool>,
) -> Element {
    let class = if bare.unwrap_or(false) {
        "md-bare"
    } else {
        "artifact-box"
    };
    rsx! {
        div { class,
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
                                match task_item(&item) {
                                    Some((done, rest)) => rsx! {
                                        li { key: "{j}", class: "md-task",
                                            span { class: "md-check", if done { "☑" } else { "☐" } }
                                            MdLine { line: rest.to_string(), on_path }
                                        }
                                    },
                                    None => rsx! {
                                        li { key: "{j}", MdLine { line: item.clone(), on_path } }
                                    },
                                }
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
                    MdBlock::Quote(lines) => rsx! {
                        div { key: "{i}", class: "md-quote",
                            for (j, line) in lines.into_iter().enumerate() {
                                div { key: "{j}", class: "md-quote-line", MdLine { line, on_path } }
                            }
                        }
                    },
                    MdBlock::Rule => rsx! {
                        hr { key: "{i}", class: "md-hr" }
                    },
                    MdBlock::Table { align, header, rows } => rsx! {
                        // Its own horizontal scroller: a wide table must not
                        // widen the panel it sits in.
                        div { key: "{i}", class: "md-table-wrap",
                            table { class: "md-table",
                                thead {
                                    tr {
                                        for (j, h) in header.into_iter().enumerate() {
                                            th { key: "{j}", style: align[j].css(), MdLine { line: h, on_path } }
                                        }
                                    }
                                }
                                tbody {
                                    for (r, row) in rows.into_iter().enumerate() {
                                        tr { key: "{r}",
                                            for (j, c) in row.into_iter().enumerate() {
                                                td { key: "{j}", style: align[j].css(), MdLine { line: c, on_path } }
                                            }
                                        }
                                    }
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
                MdInline::Em(t) => rsx! { em { key: "{i}", "{t}" } },
                MdInline::Strike(t) => rsx! { del { key: "{i}", "{t}" } },
                MdInline::Link { label, url } => rsx! {
                    button {
                        key: "{i}",
                        class: "md-link",
                        title: "{url}",
                        onclick: {
                            let url = url.clone();
                            move |_| crate::ui::sessions::open_url(&url)
                        },
                        "{label}"
                    }
                },
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
/// prose is headings, bullets, steps, code and the odd table, and anything
/// richer degrades to a paragraph rather than to markup the reader has to
/// decode.
#[derive(Debug, PartialEq, Eq)]
enum MdBlock {
    Heading(u8, String),
    Para(Vec<String>),
    Bullets(Vec<String>),
    Ordered(Vec<String>),
    Code(String),
    Quote(Vec<String>),
    Rule,
    /// Every row is padded or cut to the header's width, so `align`, `header`
    /// and each row index alike.
    Table {
        align: Vec<Align>,
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

/// A table column's alignment, from the colons of its separator cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

impl Align {
    fn css(self) -> &'static str {
        match self {
            Align::None | Align::Left => "",
            Align::Center => "text-align: center",
            Align::Right => "text-align: right",
        }
    }
}

/// Split prose into blocks. Unterminated fences run to the end of the text —
/// the agent opened a code block and never closed it, and showing the rest as
/// code beats showing the fence.
fn md_blocks(text: &str) -> Vec<MdBlock> {
    let mut out = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut bullets: Vec<String> = Vec::new();
    let mut ordered: Vec<String> = Vec::new();
    let mut quote: Vec<String> = Vec::new();
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
            if !quote.is_empty() {
                out.push(MdBlock::Quote(std::mem::take(&mut quote)));
            }
        };
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let raw = lines[idx];
        idx += 1;
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
        // A table needs its separator row: a `|` in prose is just a `|`.
        if trimmed.contains('|') {
            let header = split_row(trimmed);
            if let Some(align) = lines
                .get(idx)
                .and_then(|next| parse_sep(next.trim()))
                .filter(|a| a.len() == header.len())
            {
                flush!();
                idx += 1;
                let mut rows = Vec::new();
                while let Some(next) = lines.get(idx).map(|l| l.trim()) {
                    if next.is_empty() || !next.contains('|') {
                        break;
                    }
                    let mut row = split_row(next);
                    row.resize(header.len(), String::new());
                    rows.push(row);
                    idx += 1;
                }
                out.push(MdBlock::Table {
                    align,
                    header,
                    rows,
                });
                continue;
            }
        }
        if let Some(level) = heading_level(trimmed) {
            flush!();
            out.push(MdBlock::Heading(
                level,
                trimmed[level as usize..].trim().to_string(),
            ));
            continue;
        }
        // Before bullets, so `* * *` is a rule and not an item. A `---` under a
        // paragraph is a rule too: setext headings are not supported.
        if is_rule(trimmed) {
            flush!();
            out.push(MdBlock::Rule);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            if quote.is_empty() {
                flush!();
            }
            quote.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            continue;
        }
        if let Some(item) = bullet_item(trimmed) {
            if !para.is_empty() || !ordered.is_empty() || !quote.is_empty() {
                flush!();
            }
            bullets.push(item);
            continue;
        }
        if let Some(item) = ordered_item(trimmed) {
            if !para.is_empty() || !bullets.is_empty() || !quote.is_empty() {
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
        if !bullets.is_empty() || !ordered.is_empty() || !quote.is_empty() {
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

/// The cells of a table row: outer pipes optional, `\|` a literal pipe, and a
/// pipe inside a code span part of the code (agents put `a|b` in tables).
fn split_row(line: &str) -> Vec<String> {
    let mut s = line.trim();
    s = s.strip_prefix('|').unwrap_or(s);
    if s.ends_with('|') && !s.ends_with("\\|") {
        s = &s[..s.len() - 1];
    }
    // An unclosed backtick would swallow every later pipe, so a row whose
    // backticks don't pair up splits on every pipe instead.
    let code_aware = s.matches('`').count() % 2 == 0;
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '`' if code_aware => {
                in_code = !in_code;
                cur.push(c);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// A separator row (`|:--|:-:|--:|`), as its columns' alignments.
fn parse_sep(line: &str) -> Option<Vec<Align>> {
    if line.is_empty() {
        return None;
    }
    split_row(line)
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.len() > 1 && cell.ends_with(':');
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|c| c == '-')).then_some(
                match (left, right) {
                    (true, true) => Align::Center,
                    (true, false) => Align::Left,
                    (false, true) => Align::Right,
                    (false, false) => Align::None,
                },
            )
        })
        .collect()
}

/// `---`, `***`, `___`, spaces allowed between.
fn is_rule(line: &str) -> bool {
    let marks: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3
        && ['-', '*', '_'].contains(&marks[0])
        && marks.iter().all(|c| *c == marks[0])
}

/// `#` through `######`, returning the level (which is also the byte offset of
/// the text after the hashes).
fn heading_level(line: &str) -> Option<u8> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&hashes) && line[hashes..].starts_with(' ')).then_some(hashes as u8)
}

/// The text of a `- ` / `* ` / `+ ` bullet.
fn bullet_item(line: &str) -> Option<String> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
        .map(|s| s.trim().to_string())
}

/// A `[ ]` / `[x]` checklist item: whether it's ticked, and its text.
fn task_item(item: &str) -> Option<(bool, &str)> {
    let rest = item
        .strip_prefix("[ ]")
        .map(|r| (false, r))
        .or_else(|| item.strip_prefix("[x]").map(|r| (true, r)))
        .or_else(|| item.strip_prefix("[X]").map(|r| (true, r)))?;
    (rest.1.is_empty() || rest.1.starts_with(' ')).then(|| (rest.0, rest.1.trim_start()))
}

/// The text of a `1. ` / `1) ` numbered item.
fn ordered_item(line: &str) -> Option<String> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..]
        .strip_prefix(". ")
        .or_else(|| line[digits..].strip_prefix(") "))
        .map(|s| s.trim().to_string())
}

/// One inline run.
#[derive(Debug, PartialEq, Eq)]
enum MdInline {
    Text(String),
    Code(String),
    Strong(String),
    Em(String),
    Strike(String),
    /// Only for `http(s)` targets — anything else has nowhere to open.
    Link { label: String, url: String },
    Path(String),
}

/// Split a line into inline runs: backticked code, `**bold**`, `*em*` /
/// `_em_`, `~~strike~~`, `[links](https://…)` and file references. Anything
/// unmatched — including a stray backtick or asterisk — stays literal text.
fn md_inline(line: &str) -> Vec<MdInline> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < line.len() {
        let rest = &line[i..];
        let prev = line[..i].chars().next_back();
        // Paired delimiters, longest first so `**` never reads as two `*`.
        let mut matched = false;
        for (delim, kind) in [("`", 0), ("**", 1), ("~~", 2)] {
            if !rest.starts_with(delim) {
                continue;
            }
            let after = &rest[delim.len()..];
            let close = if kind == 0 {
                after.find(delim)
            } else {
                find_outside_code(after, 0, delim)
            };
            match close {
                Some(end) if kind == 0 || end > 0 => {
                    push_plain(&mut out, &mut plain);
                    let inner = after[..end].to_string();
                    out.push(match kind {
                        0 => MdInline::Code(inner),
                        1 => MdInline::Strong(inner),
                        _ => MdInline::Strike(inner),
                    });
                    i += delim.len() * 2 + end;
                }
                // Unclosed: the delimiter is literal.
                _ => {
                    plain.push_str(delim);
                    i += delim.len();
                }
            }
            matched = true;
            break;
        }
        if matched {
            continue;
        }
        if let Some((inner, used)) = emphasis(rest, prev) {
            push_plain(&mut out, &mut plain);
            out.push(MdInline::Em(inner.to_string()));
            i += used;
            continue;
        }
        if let Some((label, url, used)) = link(rest) {
            if url.starts_with("http://") || url.starts_with("https://") {
                push_plain(&mut out, &mut plain);
                out.push(MdInline::Link {
                    label: label.to_string(),
                    url: url.to_string(),
                });
            } else {
                // A relative target has nowhere to open; its label still gets
                // the path-chip treatment.
                plain.push_str(label);
            }
            i += used;
            continue;
        }
        let ch = rest.chars().next().unwrap_or_default();
        plain.push(ch);
        i += ch.len_utf8();
    }
    push_plain(&mut out, &mut plain);
    out
}

/// `*em*` or `_em_` at the start of `rest`: its text and the bytes consumed.
/// The text may not start or end with a space (`a * b * c` is arithmetic), and
/// `_` only counts at word boundaries (`snake_case_name`, `__init__`).
fn emphasis(rest: &str, prev: Option<char>) -> Option<(&str, usize)> {
    let d = rest.chars().next().filter(|c| *c == '*' || *c == '_')?;
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if d == '_' && prev.is_some_and(word) {
        return None;
    }
    let after = &rest[1..];
    let first = after.chars().next()?;
    if first.is_whitespace() || first == d {
        return None;
    }
    let mut from = 0;
    let delim = if d == '*' { "*" } else { "_" };
    while let Some(end) = find_outside_code(after, from, delim) {
        let inner = &after[..end];
        let next = after[end + 1..].chars().next();
        // A `*` that's half of a `**` run (`*args and **kwargs`) isn't a closer.
        let closes = !inner.ends_with(char::is_whitespace)
            && !inner.ends_with(d)
            && next != Some(d)
            && (d == '*' || !next.is_some_and(word));
        if !inner.is_empty() && closes {
            return Some((inner, end + 2));
        }
        from = end + 1;
    }
    None
}

/// The first `delim` in `s` at or after byte `from` that isn't inside a closed
/// backtick code span — markers in code are code, never a closer.
fn find_outside_code(s: &str, mut from: usize, delim: &str) -> Option<usize> {
    loop {
        let hit = from + s[from..].find(delim)?;
        let Some(tick) = s[from..hit].find('`').map(|t| from + t) else {
            return Some(hit);
        };
        match s[tick + 1..].find('`') {
            Some(close) => from = tick + 1 + close + 1,
            // An unclosed backtick is literal.
            None => return Some(hit),
        }
    }
}

/// `[label](url)` at the start of `rest`: label, url, and the bytes consumed.
fn link(rest: &str) -> Option<(&str, &str, usize)> {
    let body = rest.strip_prefix('[')?;
    let close = body.find(']')?;
    let label = &body[..close];
    let target = body[close + 1..].strip_prefix('(')?;
    // Balanced parens may sit inside the URL (`wiki/Foo_(bar)`).
    let mut depth = 0usize;
    let end = target.char_indices().find_map(|(j, c)| match c {
        '(' => {
            depth += 1;
            None
        }
        ')' if depth == 0 => Some(j),
        ')' => {
            depth -= 1;
            None
        }
        _ => None,
    })?;
    let url = &target[..end];
    (!label.is_empty() && !url.is_empty() && !url.contains(char::is_whitespace))
        .then_some((label, url, 1 + close + 2 + end + 1))
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
mod md_table_tests {
    use super::*;

    fn cells(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_table_between_paragraphs_parses_with_alignment() {
        let text = "Before.\n| Name | Mid | Count |\n|:-----|:---:|------:|\n| a | b | 1 |\n| c | d | 22 |\nAfter.";
        assert_eq!(
            md_blocks(text),
            vec![
                MdBlock::Para(vec!["Before.".into()]),
                MdBlock::Table {
                    align: vec![Align::Left, Align::Center, Align::Right],
                    header: cells(&["Name", "Mid", "Count"]),
                    rows: vec![cells(&["a", "b", "1"]), cells(&["c", "d", "22"])],
                },
                MdBlock::Para(vec!["After.".into()]),
            ]
        );
    }

    #[test]
    fn pipes_without_a_separator_stay_prose() {
        assert_eq!(
            md_blocks("a | b\nc | d"),
            vec![MdBlock::Para(vec!["a | b".into(), "c | d".into()])]
        );
        // A separator whose width disagrees with the header is not one.
        assert!(matches!(md_blocks("a | b\n---")[0], MdBlock::Para(_)));
    }

    #[test]
    fn escaped_and_code_pipes_do_not_split_cells() {
        let blocks = md_blocks("| x | y |\n|---|---|\n| a \\| b | `c|d` |");
        let MdBlock::Table { rows, .. } = &blocks[0] else {
            panic!("expected a table, got {blocks:?}")
        };
        assert_eq!(rows[0], cells(&["a | b", "`c|d`"]));
    }

    #[test]
    fn ragged_rows_are_padded_and_cut_to_the_header() {
        let blocks = md_blocks("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |");
        let MdBlock::Table { rows, align, .. } = &blocks[0] else {
            panic!("expected a table")
        };
        assert_eq!(align, &vec![Align::None, Align::None]);
        assert_eq!(rows[0], cells(&["1", ""]));
        assert_eq!(rows[1], cells(&["1", "2"]));
    }

    #[test]
    fn a_table_without_outer_pipes_still_parses() {
        assert_eq!(
            md_blocks("a | b\n--|--\n1 | 2"),
            vec![MdBlock::Table {
                align: vec![Align::None, Align::None],
                header: cells(&["a", "b"]),
                rows: vec![cells(&["1", "2"])],
            }]
        );
    }

    #[test]
    fn a_table_in_a_fence_is_code() {
        assert_eq!(
            md_blocks("```\n| a |\n|---|\n```"),
            vec![MdBlock::Code("| a |\n|---|".into())]
        );
    }

    #[test]
    fn rules_quotes_and_list_styles() {
        for rule in ["---", "***", "* * *", "___"] {
            assert_eq!(md_blocks(rule), vec![MdBlock::Rule], "{rule}");
        }
        assert_eq!(
            md_blocks("- item\n* other"),
            vec![MdBlock::Bullets(vec!["item".into(), "other".into()])]
        );
        assert_eq!(
            md_blocks("Para\n---\nmore"),
            vec![
                MdBlock::Para(vec!["Para".into()]),
                MdBlock::Rule,
                MdBlock::Para(vec!["more".into()]),
            ]
        );
        assert_eq!(
            md_blocks("> one\n>two\n>\nafter"),
            vec![
                MdBlock::Quote(vec!["one".into(), "two".into(), "".into()]),
                MdBlock::Para(vec!["after".into()]),
            ]
        );
        assert_eq!(md_blocks("+ x"), vec![MdBlock::Bullets(vec!["x".into()])]);
        assert_eq!(md_blocks("1) x"), vec![MdBlock::Ordered(vec!["x".into()])]);
        assert_eq!(task_item("[ ] a"), Some((false, "a")));
        assert_eq!(task_item("[x] b"), Some((true, "b")));
        assert_eq!(task_item("[X] c"), Some((true, "c")));
        assert_eq!(task_item("[link](x)"), None);
        assert_eq!(task_item("plain"), None);
    }

    #[test]
    fn emphasis_strike_and_links() {
        assert_eq!(
            md_inline("an *em* and _em_ and ~~gone~~"),
            vec![
                MdInline::Text("an ".into()),
                MdInline::Em("em".into()),
                MdInline::Text(" and ".into()),
                MdInline::Em("em".into()),
                MdInline::Text(" and ".into()),
                MdInline::Strike("gone".into()),
            ]
        );
        for literal in ["snake_case_name", "a * b * c", "__init__", "x_1 and y_2"] {
            assert_eq!(
                md_inline(literal),
                vec![MdInline::Text(literal.into())],
                "{literal} must stay as written"
            );
        }
        assert_eq!(
            md_inline("see [docs](https://example.com) now"),
            vec![
                MdInline::Text("see ".into()),
                MdInline::Link {
                    label: "docs".into(),
                    url: "https://example.com".into()
                },
                MdInline::Text(" now".into()),
            ]
        );
        // A relative target has nowhere to open: the label, as text — and a
        // path-shaped label still becomes a chip.
        assert_eq!(md_inline("[foo](./foo)"), vec![MdInline::Text("foo".into())]);
        assert_eq!(
            md_inline("[src/a.rs:3](src/a.rs#L3)"),
            vec![MdInline::Path("src/a.rs:3".into())]
        );
        // Markers inside code are code.
        assert_eq!(
            md_inline("`*a* _b_ ~~c~~ [d](https://e)`"),
            vec![MdInline::Code("*a* _b_ ~~c~~ [d](https://e)".into())]
        );
        // Bold still wins over emphasis.
        assert_eq!(md_inline("**b**"), vec![MdInline::Strong("b".into())]);
        // Half of a `**` run never closes an `*`.
        for literal in ["accepts *args and **kwargs", "int *p and **pp"] {
            assert_eq!(
                md_inline(literal),
                vec![MdInline::Text(literal.into())],
                "{literal} must stay as written"
            );
        }
        // A closer inside a code span doesn't count; the span stays whole.
        assert_eq!(
            md_inline("compute n*m, see `a*b`"),
            vec![
                MdInline::Text("compute n*m, see ".into()),
                MdInline::Code("a*b".into()),
            ]
        );
        assert_eq!(
            md_inline("x ~~y `a~~b`"),
            vec![MdInline::Text("x ~~y ".into()), MdInline::Code("a~~b".into())]
        );
        // But code may sit inside bold.
        assert_eq!(
            md_inline("**fix `a` now**"),
            vec![MdInline::Strong("fix `a` now".into())]
        );
        // Balanced parens stay in the URL.
        assert_eq!(
            md_inline("[x](https://en.wikipedia.org/wiki/Foo_(bar)) ok"),
            vec![
                MdInline::Link {
                    label: "x".into(),
                    url: "https://en.wikipedia.org/wiki/Foo_(bar)".into()
                },
                MdInline::Text(" ok".into()),
            ]
        );
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
            "| é | 日本 |\n|:-:|--:|\n| `a|b | \\|",
            "|\n|-|\n|",
            "| a |\n|---|",
            "> — quote\n>",
            "* * *\n- - -\n_ _",
            "[é](https://ü) [](x) [a]( ) [b](",
            "_日本_ *é* ~~ü~~ ~~ _ *",
            "- [ ]\n- [x]日本",
            "1) ok\n2)no",
        ];
        for case in cases {
            let blocks = md_blocks(case);
            for block in &blocks {
                match block {
                    MdBlock::Heading(_, l) | MdBlock::Code(l) => {
                        let _ = md_inline(l);
                    }
                    MdBlock::Para(v)
                    | MdBlock::Bullets(v)
                    | MdBlock::Ordered(v)
                    | MdBlock::Quote(v) => {
                        for l in v {
                            let _ = md_inline(l);
                            let _ = task_item(l);
                        }
                    }
                    MdBlock::Table {
                        align,
                        header,
                        rows,
                    } => {
                        assert_eq!(align.len(), header.len());
                        for l in header.iter().chain(rows.iter().flatten()) {
                            let _ = md_inline(l);
                        }
                        assert!(rows.iter().all(|r| r.len() == header.len()));
                    }
                    MdBlock::Rule => {}
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
            "a *b* c",
            "a ~~b~~ c",
            "a _b_ c",
        ] {
            let joined: String = md_inline(case)
                .into_iter()
                .map(|s| match s {
                    MdInline::Text(t)
                    | MdInline::Code(t)
                    | MdInline::Strong(t)
                    | MdInline::Em(t)
                    | MdInline::Strike(t)
                    | MdInline::Path(t) => t,
                    MdInline::Link { label, .. } => label,
                })
                .collect();
            let stripped: String = case
                .replace("**", "")
                .replace("~~", "")
                .replace(['`', '*', '_'], "");
            assert_eq!(joined, stripped, "input: {case}");
        }
    }
}

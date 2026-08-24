//! Debug-only load generator and typing probe for the dropped-keystroke race.
//!
//! Controlled text inputs (`value: "{signal}"`) round-trip every keystroke
//! through the async IPC to the Rust main thread, and the re-rendered value
//! comes back as a DOM patch the interpreter applies unconditionally
//! (`core.js`: `case "value": … if (node.value !== value) node.value = value`).
//! When the round trip outruns the typing interval, a late patch rewinds the
//! field and erases whatever was typed meanwhile.
//!
//! This module reproduces that numerically so a fix can be proven rather than
//! asserted:
//!
//! - `USINE_STRESS=1` — the load generator. Mints a synthetic card parked at
//!   the merge gate (the state whose panel renders both the Agent Chat box and
//!   the transcript), selects it, and floods `state.transcripts` through the
//!   real `apply_event` reducer plus periodic `CardUpdated` churn, so the whole
//!   render path — `TranscriptView`, its autoscroll eval, the board — is on the
//!   hot path exactly as it is under five concurrent runs.
//! - `USINE_STRESS_TYPE=1` — the probe. Drives the chat textarea from JS at a
//!   fixed interval, appending one character of an 80-char sentinel at a time
//!   and dispatching a bubbling `InputEvent`. That traverses the same
//!   listener → IPC → signal → patch path as a real keystroke and is clobbered
//!   by a stale patch identically. After a quiescence window it reports the
//!   surviving DOM value, which is compared against the sentinel and against
//!   the Rust-side draft signal.
//!
//! Everything is `cfg(debug_assertions)` and inert unless the env vars are set,
//! so it stays in the tree as a regression check. Point `USINE_DATA_DIR` at a
//! throwaway directory when running it.

use std::time::Duration;

use dioxus::prelude::*;
use usine_core::{Card, CardConfig, CardState, ExecutorEvent, ExecutorEventKind};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;

/// The typed sentinel: 80 ASCII characters with no repeated adjacent character,
/// so a surviving value can be matched against it unambiguously.
const SENTINEL: &str =
    "the-quick-brown-fox-jumps-over-the-lazy-dog-0123456789-abcdefghijklmnopqrstuvwxy";

/// The DOM id given to the chat textarea so the probe can find it.
pub const CHAT_INPUT_ID: &str = "chat-input";

thread_local! {
    static LAST_CHAT_INPUT: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

/// Record the value the chat box's `oninput` handler received. This is what
/// closes the loop on a fix: a field that keeps its text only because the Rust
/// side never processed the events would look identical in the DOM but have
/// delivered nothing.
pub fn record_chat_input(v: &str) {
    LAST_CHAT_INPUT.with(|c| {
        let mut c = c.borrow_mut();
        c.clear();
        c.push_str(v);
    });
}

fn last_chat_input() -> String {
    LAST_CHAT_INPUT.with(|c| c.borrow().clone())
}

fn flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Whether the load generator is armed.
pub fn enabled() -> bool {
    flag("USINE_STRESS")
}

/// Fix A: the chat box renders uncontrolled (`initial_value` plus a generation
/// key for programmatic resets) instead of controlled (`value`). On by default;
/// `USINE_STRESS_NO_FIX_A=1` restores the pre-fix rendering so the harness can
/// measure both configurations in one session and prove it still detects the
/// bug. The other converted fields have no toggle — the probe doesn't type into
/// them, and a second rendering path is only worth keeping where it is measured.
pub fn fix_a() -> bool {
    !flag("USINE_STRESS_NO_FIX_A")
}

/// Fix B: how many transcript lines `TranscriptView` renders, newest last.
/// `USINE_STRESS_NO_FIX_B=1` gives `0`, meaning "all of them, unkeyed" — the
/// pre-fix rendering.
pub fn transcript_cap() -> usize {
    if flag("USINE_STRESS_NO_FIX_B") {
        0
    } else {
        num("USINE_TRANSCRIPT_CAP", 500) as usize
    }
}

/// Install the harness. A no-op unless `USINE_STRESS=1`.
pub fn use_stress(state: AppState) {
    use_future(move || async move {
        if !enabled() {
            return;
        }
        run(state).await;
    });
}

/// How long the sentinel should take to type at `gap` ms per character.
fn typed_span(gap: u64) -> u64 {
    gap * (SENTINEL.chars().count() as u64 - 1)
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn synthetic_card(project_id: Uuid, n: usize) -> Card {
    let mut card = Card::new(
        project_id,
        format!("stress card {n}"),
        "load-generator card",
        CardConfig::default(),
    );
    card.state = CardState::ReadyToMerge;
    card
}

async fn run(state: AppState) {
    let tick_ms = num("USINE_STRESS_TICK_MS", 15);
    let batch = num("USINE_STRESS_BATCH", 5) as usize;
    let line_chars = num("USINE_STRESS_LINE_CHARS", 100) as usize;
    let prefill = num("USINE_STRESS_PREFILL", 0) as usize;
    let churn = num("USINE_STRESS_CHURN", 4) as usize;
    let trials = num("USINE_STRESS_TRIALS", 5) as usize;
    let type_ms = num("USINE_STRESS_TYPE_MS", 30);
    let settle_ms = num("USINE_STRESS_SETTLE_MS", 1500);
    let quiet_ms = num("USINE_STRESS_QUIET_MS", 2000);

    // Let the first render, the store load and the event drain settle.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let project_id = state
        .projects
        .read()
        .first()
        .map(|p| p.id)
        .unwrap_or_else(Uuid::new_v4);

    // The card under test plus siblings whose `CardUpdated` echoes churn the
    // board, the way concurrent runs do.
    let target = synthetic_card(project_id, 0);
    let target_id = target.id;
    let siblings: Vec<Card> = (1..=churn).map(|n| synthetic_card(project_id, n)).collect();
    for c in std::iter::once(target.clone()).chain(siblings.iter().cloned()) {
        state.apply_event(ExecutorEvent {
            card_id: c.id,
            kind: ExecutorEventKind::CardUpdated(Box::new(c)),
        });
    }
    state.select_card(Some(target_id));

    let line: String = "x".repeat(line_chars.saturating_sub(20));
    // Pre-grow the transcript so the per-line render cost starts out at the
    // size a long run reaches, rather than climbing there during the probe.
    if prefill > 0 {
        let ts = now_ms();
        let mut transcripts = state.transcripts;
        let mut w = transcripts.write();
        let v = w.entry(target_id).or_default();
        for i in 0..prefill {
            v.push((ts, format!("prefill {i:06} {line}")));
        }
    }

    println!(
        "[stress] armed: tick={tick_ms}ms batch={batch} line_chars={line_chars} \
         prefill={prefill} churn={churn} fix_a={} transcript_cap={} type_gap={type_ms}ms",
        fix_a(),
        transcript_cap(),
    );

    // The functional checks normally want a quiet app. `USINE_STRESS_CHECK_LOAD=1`
    // runs them under the flood instead, which is how the effect-starvation
    // finding is turned into a user-visible one: with the render loop saturated
    // Dioxus never reaches its queued effects, the draft store stops mirroring,
    // and a remount has nothing to restore from.
    let checks_under_load = flag("USINE_STRESS_CHECK_LOAD");
    if flag("USINE_STRESS_CHECK") && !checks_under_load {
        run_checks(state, target, 1).await;
        return;
    }

    // The flood: one task, the real reducer, forever.
    let flood_line = line.clone();
    let flood_siblings = siblings.clone();
    let flood_target = target.clone();
    spawn(async move {
        let mut n: u64 = 0;
        loop {
            let ts = now_ms();
            for i in 0..batch {
                state.apply_event(ExecutorEvent {
                    card_id: target_id,
                    kind: ExecutorEventKind::Transcript {
                        ts,
                        line: format!("run{i} {n:08} {flood_line}"),
                    },
                });
            }
            // Board churn: sibling cards every tick, the selected card less
            // often (its own panel re-renders, as a cost update would).
            if churn > 0 && n.is_multiple_of(2) {
                for c in flood_siblings.iter() {
                    let mut c = c.clone();
                    c.updated_at = ts;
                    state.apply_event(ExecutorEvent {
                        card_id: c.id,
                        kind: ExecutorEventKind::CardUpdated(Box::new(c)),
                    });
                }
            }
            // The loaded checks drive the target card's state themselves; re-applying
            // the card here would revert each transition before the panel renders it.
            if churn > 0 && !checks_under_load && n.is_multiple_of(8) {
                let mut c = flood_target.clone();
                c.updated_at = ts;
                state.apply_event(ExecutorEvent {
                    card_id: c.id,
                    kind: ExecutorEventKind::CardUpdated(Box::new(c)),
                });
            }
            n += 1;
            tokio::time::sleep(Duration::from_millis(tick_ms)).await;
        }
    });

    if flag("USINE_STRESS_CHECK") {
        // Same checks, flood running. Every settle window is stretched so a
        // failure means the behaviour is genuinely broken, not merely late.
        run_checks(state, target, 3).await;
        return;
    }

    if !flag("USINE_STRESS_TYPE") {
        return;
    }

    // Give the flood a moment to build up a transcript before probing.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut results: Vec<Trial> = Vec::new();
    for i in 1..=trials {
        // Every trial starts from the same transcript length, so a later trial
        // isn't measured under a heavier load than an earlier one.
        {
            let mut transcripts = state.transcripts;
            let mut w = transcripts.write();
            if let Some(v) = w.get_mut(&target_id) {
                v.truncate(prefill);
            }
        }
        let mirrors_before = drafts::MIRROR_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let t = trial(state, target_id, type_ms, settle_ms, quiet_ms).await;
        let mirrors =
            drafts::MIRROR_CALLS.load(std::sync::atomic::Ordering::Relaxed) - mirrors_before;
        println!(
            "[stress] trial {i}/{trials}: typed={} dom={} dom_match={} signal_match={} \
             draft_match={} subsequence={} dropped={} draft_mirrors={mirrors} \
             typing_ms={}",
            t.typed,
            t.dom_len,
            t.dom_match,
            t.signal_match,
            t.draft_match,
            t.subsequence,
            t.dropped,
            t.typing_ms
        );
        if !t.dom_match || !t.signal_match {
            println!("[stress]   dom    = {:?}", t.dom);
            println!("[stress]   signal = {:?}", t.signal);
            println!("[stress]   draft  = {:?}", t.draft);
        }
        results.push(t);
    }

    let lines: usize = state
        .transcripts
        .read()
        .get(&target_id)
        .map(|v| v.len())
        .unwrap_or(0);
    let dropping = results.iter().filter(|t| t.dropped > 0).count();
    let total: usize = results.iter().map(|t| t.dropped).sum();
    let signal_ok = results.iter().filter(|t| t.signal_match).count();
    println!(
        "[stress] SUMMARY fix_a={} transcript_cap={} trials={} trials_with_drops={} \
         total_dropped={} per_trial={:?} signal_intact={signal_ok}/{} \
         transcript_lines={lines}",
        fix_a(),
        transcript_cap(),
        results.len(),
        dropping,
        total,
        results.iter().map(|t| t.dropped).collect::<Vec<_>>(),
        results.len(),
    );
    println!(
        "[stress] TYPING nominal={}ms actual={:?}",
        typed_span(type_ms),
        results.iter().map(|t| t.typing_ms).collect::<Vec<_>>(),
    );
    std::process::exit(0);
}

struct Trial {
    typed: usize,
    /// How long the 80 keystrokes actually took. At the nominal gap this is
    /// ~`80 * gap`; well above that means the webview was starved and the
    /// trial says little about the race.
    typing_ms: u64,
    /// What the field still showed after the quiescence window.
    dom: String,
    /// What the last `oninput` handler on the Rust side received.
    signal: String,
    /// What the global draft store had mirrored.
    draft: String,
    dom_len: usize,
    dom_match: bool,
    signal_match: bool,
    draft_match: bool,
    subsequence: bool,
    dropped: usize,
}

/// The JS half of the probe. Placeholders are substituted rather than
/// `format!`-ed so the braces stay readable.
const TYPE_JS: &str = r#"
const SENT = "__SENT__";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const el = document.getElementById("__ID__");
if (!el) {
  dioxus.send("__NOELEM__");
} else {
  el.focus();
  // Clear whatever the previous trial left, and let the clear round-trip so
  // the Rust-side signal agrees with the DOM before the run starts.
  el.value = "";
  el.dispatchEvent(new InputEvent("input", { bubbles: true }));
  await sleep(__SETTLE__);
  el.value = "";
  // Keystrokes are scheduled against an absolute timeline, not chained
  // setTimeouts: a person types at a steady rate no matter how busy the
  // machine is, and a typist that slows down in step with the renderer would
  // hide the very race this measures.
  const t0 = performance.now();
  for (let i = 0; i < SENT.length; i++) {
    const wait = t0 + i * __GAP__ - performance.now();
    if (wait > 0) await sleep(wait);
    el.value = el.value + SENT[i];
    el.dispatchEvent(new InputEvent("input", { bubbles: true }));
  }
  const typing_ms = Math.round(performance.now() - t0);
  await sleep(__QUIET__);
  dioxus.send(typing_ms + ":" + el.value);
}
"#;

async fn trial(
    _state: AppState,
    card_id: Uuid,
    gap_ms: u64,
    settle_ms: u64,
    quiet_ms: u64,
) -> Trial {
    let js = TYPE_JS
        .replace("__SENT__", SENTINEL)
        .replace("__ID__", CHAT_INPUT_ID)
        .replace("__SETTLE__", &settle_ms.to_string())
        .replace("__GAP__", &gap_ms.to_string())
        .replace("__QUIET__", &quiet_ms.to_string());
    let mut ev = dioxus::document::eval(&js);
    let raw: String = match ev.recv().await {
        Ok(v) => v,
        Err(e) => {
            println!("[stress] eval channel error: {e}");
            String::new()
        }
    };
    // "<elapsed ms>:<surviving value>" — the elapsed time says whether the
    // simulated typist actually kept its cadence, which is what makes the
    // drop count meaningful.
    let (typing_ms, dom) = match raw.split_once(':') {
        Some((ms, v)) => (ms.parse::<u64>().unwrap_or(0), v.to_string()),
        None => (0, raw),
    };
    if dom == "__NOELEM__" {
        println!(
            "[stress] FATAL: no #{CHAT_INPUT_ID} in the DOM — the card panel is not \
             showing the Agent Chat box."
        );
        std::process::exit(2);
    }
    let draft = drafts::peek(card_id, "chat").unwrap_or_default();
    let signal = last_chat_input();
    let typed = SENTINEL.chars().count();
    let dom_len = dom.chars().count();
    Trial {
        typed,
        typing_ms,
        dom_match: dom == SENTINEL,
        signal_match: signal == SENTINEL,
        draft_match: draft == SENTINEL,
        subsequence: is_subsequence(&dom, SENTINEL),
        dropped: typed.saturating_sub(dom_len),
        dom_len,
        dom,
        signal,
        draft,
    }
}

/// Every surviving character must still appear, in order, in the sentinel — a
/// rewind-then-append leaves a subsequence, so anything else means the probe
/// itself is misbehaving rather than the field dropping input.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut h = haystack.chars();
    needle.chars().all(|c| h.any(|x| x == c))
}

// ---------------------------------------------------------------------------
// Regression checks (`USINE_STRESS_CHECK=1`)
//
// Uncontrolled fields trade one hazard for another: a re-render can no longer
// rewind the box, but a *programmatic* reset can no longer reach it either.
// These drive the real UI through the behaviours that depend on a reset
// landing — sending, remounting, reseeding — and report what the DOM actually
// held afterwards, so the trade is verified rather than assumed.
// ---------------------------------------------------------------------------

/// Run `js` (an async function body with `dioxus` in scope) and return whatever
/// it passes to `dioxus.send`.
async fn eval_str(js: &str) -> String {
    let mut ev = dioxus::document::eval(js);
    match ev.recv::<String>().await {
        Ok(v) => v,
        Err(e) => format!("<eval error: {e}>"),
    }
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

const JS_SLEEP: &str = "const sleep = (ms) => new Promise((r) => setTimeout(r, ms));";

fn report(name: &str, ok: bool, detail: &str) -> bool {
    println!(
        "[check] {:<28} {}  {detail}",
        name,
        if ok { "PASS" } else { "FAIL" }
    );
    ok
}

/// Put the card into `state`, then let the panel settle.
async fn set_state(app: AppState, card: &Card, state: CardState, settle: u64) {
    let mut c = card.clone();
    c.state = state;
    c.updated_at = now_ms();
    app.apply_event(ExecutorEvent {
        card_id: c.id,
        kind: ExecutorEventKind::CardUpdated(Box::new(c)),
    });
    pause(settle).await;
}

fn intervention(q: &str) -> CardState {
    CardState::Implementing(usine_core::RunSub::Intervention(usine_core::Intervention {
        request_id: "stress".into(),
        question: q.into(),
        options: Vec::new(),
    }))
}

async fn type_into(id: &str, text: &str, slow: u64) -> String {
    eval_str(&format!(
        r#"{JS_SLEEP}
const el = document.getElementById("{id}");
if (!el) {{ dioxus.send("missing:{id}"); }} else {{
  el.focus();
  el.value = {text:?};
  el.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
  await sleep({settle});
  dioxus.send("ok");
}}"#,
        settle = 400 * slow
    ))
    .await
}

/// `=<value>` on success, with the element's `defaultValue` appended when the
/// two disagree — the difference between "the seed never arrived" and "the seed
/// arrived but something overwrote the field" is the whole diagnosis here.
async fn read_value(id: &str) -> String {
    eval_str(&format!(
        r#"const el = document.getElementById("{id}");
if (!el) {{ dioxus.send("missing:{id}"); }} else {{
  dioxus.send("=" + el.value + (el.value === el.defaultValue ? "" : " default=" + JSON.stringify(el.defaultValue)));
}}"#
    ))
    .await
}

/// Strip the ` default=…` diagnostic tail [`read_value`] appends when a field's
/// value and `defaultValue` disagree — expected for a controlled field.
fn value_of(s: &str) -> &str {
    s.split(" default=").next().unwrap_or(s)
}

async fn run_checks(app: AppState, card: Card, slow: u64) {
    let id = card.id;
    let mut all_ok = true;
    // The panel was only just selected; let it render before poking at it.
    pause(1500 * slow).await;
    println!("[check] --- regression checks (fix_a={}) ---", fix_a());

    // 1. Send clears the box, twice in a row. This is the hazard Fix A
    //    introduces: `defaultValue` cannot clear a dirtied field, so the clear
    //    only lands if the generation bump actually remounts the element.
    let out = eval_str(&format!(
        r#"{JS_SLEEP}
const results = [];
for (const msg of ["first message", "second message"]) {{
  let el = document.getElementById("chat-input");
  if (!el) {{ dioxus.send("missing:chat-input"); return; }}
  el.focus();
  el.value = msg;
  el.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
  await sleep(400);
  const typed = document.getElementById("chat-input").value;
  const btn = [...document.querySelectorAll("button")]
      .find((b) => b.textContent.trim() === "Ask questions");
  if (!btn) {{ dioxus.send("missing:ask-button"); return; }}
  btn.click();
  await sleep(700);
  const after = document.getElementById("chat-input").value;
  results.push(JSON.stringify(typed) + "->" + JSON.stringify(after));
}}
dioxus.send(results.join(" , "));"#
    ))
    .await;
    all_ok &= report(
        "send clears the box x2",
        out == r#""first message"->"" , "second message"->"""#,
        &out,
    );

    // 2. A draft outlives a deselect and a state-change remount.
    let _ = type_into("chat-input", "draft-survives", slow).await;
    app.select_card(None);
    pause(500 * slow).await;
    app.select_card(Some(id));
    pause(800 * slow).await;
    let after_reselect = read_value("chat-input").await;
    all_ok &= report(
        "draft survives reselect",
        value_of(&after_reselect) == "=draft-survives",
        &after_reselect,
    );

    // Leaving and re-entering the state remounts the whole panel.
    set_state(app, &card, CardState::Done, 600 * slow).await;
    set_state(app, &card, CardState::ReadyToMerge, 900 * slow).await;
    let after_remount = read_value("chat-input").await;
    all_ok &= report(
        "draft survives state remount",
        value_of(&after_remount) == "=draft-survives",
        &after_remount,
    );

    // Clean up so the leftover draft can't confuse a later check.
    let out = eval_str(
        r#"const el = document.getElementById("chat-input");
if (el) { el.value = ""; el.dispatchEvent(new InputEvent("input", { bubbles: true })); }
dioxus.send("ok");"#,
    )
    .await;
    let _ = out;
    pause(400 * slow).await;

    // 3. The intervention answer follows `use_draft_of`'s origin rule: the same
    //    question restores the half-typed answer, a *new* question reseeds. With
    //    an uncontrolled field that swap only happens if the remount is real.
    set_state(app, &card, intervention("Question one?"), 800 * slow).await;
    let typed = type_into("intervention-answer", "answer-to-q1", slow).await;
    all_ok &= report("intervention field present", typed == "ok", &typed);

    set_state(
        app,
        &card,
        CardState::Implementing(usine_core::RunSub::Running),
        500 * slow,
    )
    .await;
    set_state(app, &card, intervention("Question one?"), 900 * slow).await;
    let same_q = read_value("intervention-answer").await;
    all_ok &= report(
        "same question restores answer",
        value_of(&same_q) == "=answer-to-q1",
        &same_q,
    );

    set_state(
        app,
        &card,
        CardState::Implementing(usine_core::RunSub::Running),
        500 * slow,
    )
    .await;
    set_state(
        app,
        &card,
        intervention("A different question?"),
        900 * slow,
    )
    .await;
    let new_q = read_value("intervention-answer").await;
    all_ok &= report(
        "new question reseeds blank",
        value_of(&new_q) == "=",
        &new_q,
    );

    set_state(app, &card, CardState::ReadyToMerge, 700 * slow).await;

    // 4. The search box still filters the board live, per keystroke.
    let out = eval_str(&format!(
        r#"{JS_SLEEP}
const count = () => document.querySelectorAll(".board .card").length;
document.dispatchEvent(new KeyboardEvent("keydown", {{
  key: "f", metaKey: true, ctrlKey: false, bubbles: true,
}}));
await sleep(700);
const box = document.getElementById("card-search-input");
if (!box) {{ dioxus.send("missing:card-search-input"); return; }}
const before = count();
const term = "stress card 3";
box.focus();
for (let i = 0; i < term.length; i++) {{
  box.value = term.slice(0, i + 1);
  box.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
  await sleep(60);
}}
await sleep(600);
const filtered = count();
const shown = box.value;
box.value = "";
box.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
await sleep(600);
const restored = count();
dioxus.send(before + "/" + filtered + "/" + restored + " typed=" + JSON.stringify(shown));"#
    ))
    .await;
    let parts: Vec<&str> = out.split(['/', ' ']).collect();
    let ok = parts.len() >= 3
        && parts[0].parse::<usize>().ok() > parts[1].parse::<usize>().ok()
        && parts[1] == "1"
        && parts[0] == parts[2]
        && out.contains(r#"typed="stress card 3""#);
    all_ok &= report("search filters live", ok, &out);

    println!(
        "[check] --- {} ---",
        if all_ok { "ALL PASS" } else { "FAILURES" }
    );
    std::process::exit(if all_ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_is_80_chars_with_no_adjacent_repeats() {
        let chars: Vec<char> = SENTINEL.chars().collect();
        assert_eq!(chars.len(), 80);
        assert!(chars.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn subsequence_detects_a_rewind() {
        // A patch rewound to "the-qu" and typing carried on from there.
        assert!(is_subsequence("the-quown-fox", SENTINEL));
        assert!(is_subsequence(SENTINEL, SENTINEL));
        assert!(!is_subsequence("zzz", SENTINEL));
    }
}

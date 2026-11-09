//! The implementation hand-off: what the agent tells the human who reviews it.
//!
//! An implement run ends with the work committed in the card's worktree and the
//! card parked at the awaiting-review gate. The user arrives there with no idea
//! how the run went — whether the agent hit something surprising, made a call
//! that could have gone the other way, or left part of the task undone.
//!
//! So the run is asked to append a fenced ` ```usine-handoff ` JSON block: a
//! recap of the work done, the files it touched, any open questions, the risks
//! it took, and the things worth testing by hand — that last list written
//! against the card's own seeded preview environment, so the reviewer can run
//! it as written (see [`handoff_instruction`]).
//! [`parse_handoff`] pulls it out; the executor stores it on the card and the
//! detail panel renders it beside the self-review and PR buttons. Same
//! prompt-and-parse shape as [`crate::agent::commit`] and [`crate::agent::review`],
//! and just as tolerant: a run that emits no block simply has no hand-off.
//!
//! # Schema v2
//!
//! v1 was `{summary, questions[], tests[]}` with `tests` a list of strings, the
//! per-change lines buried in `summary`'s prose and a `[verified]` string prefix
//! standing in for a field. v2 promotes each of those to real data —
//! `changes[]`, `risks[]`, `tests[].verified` — so the panel can tab them apart
//! instead of printing one blob.
//!
//! v1 stays readable forever: every field is `#[serde(default)]` and
//! [`TestItem`] deserializes from a bare string, which matters twice over —
//! for an agent that emits the old shape, and for the v1 hand-offs already
//! serialized into `CardReviewRecord`.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::agent::block;

const TAG: &str = "usine-handoff";

/// The pre-revision, tests-only spelling of [`REVISION_TAG`]. No prompt asks
/// for it any more, but a run launched by an older build and finished by this
/// one may still send it, so it is still parsed (as a tests-only revision)
/// and stripped.
const TESTS_TAG: &str = "usine-tests";

/// A later run's revision of the hand-off's "Test it", "Risks" and "Open
/// questions" — see [`revision_instruction`] and [`revise_handoff`].
const REVISION_TAG: &str = "usine-handoff-revision";

/// The schema version this build emits and assumes when a payload omits `v`.
const VERSION: u8 = 2;

/// Appended to implement runs (not to fix runs, which report through their own
/// recap — see [`crate::agent::fixes`]). Kept provider-agnostic and
/// self-contained.
///
/// `local_app` says whether the project can actually run its app — the same
/// condition that gates [`crate::agent::testing::testing_instruction`]. When it
/// can, the `tests` list is demanded against the card's own seeded preview:
/// concrete routes, real seeded accounts, seeded records. When it can't (a
/// library, a project with no `run_script`), the generic bullet stands, because
/// there is no environment to point the reviewer at.
///
/// `scenario` says whether the card may carry a scenario script (see
/// [`crate::agent::testing::ScenarioSupport`]): when it can, checklist records
/// may come from it as well as from the seed.
pub fn handoff_instruction(local_app: bool, scenario: bool) -> String {
    let tests = tests_guidance(local_app, scenario);
    format!(
        "{HANDOFF_HEADER}- `tests`: {tests} Use an empty array if the change has no observable \
behaviour.\n{HANDOFF_TAIL}"
    )
}

/// How to write the `tests` checklist, shared by the hand-off and the change
/// recap's `usine-tests` block so the two never drift apart.
fn tests_guidance(local_app: bool, scenario: bool) -> &'static str {
    match (local_app, scenario) {
        (true, true) => TESTS_SCENARIO,
        (true, false) => TESTS_SEEDED,
        (false, _) => TESTS_GENERIC,
    }
}

const HANDOFF_HEADER: &str = "\
When you have finished making changes, hand the work off to the human who will review it. Emit a \
fenced code block tagged `usine-handoff` containing a JSON object shaped like \
{\"v\": 2, \"summary\": \"<what was done>\", \"changes\": [{\"path\": \"<file>\", \"what\": \
\"<what changed there>\", \"kind\": \"feat|fix|test|docs|chore\"}], \"tests\": [{\"scenario\": \
\"<what to do>\", \"expect\": \"<what should happen>\", \"verified\": false}], \"risks\": \
[\"<what could bite>\"], \"questions\": [\"<open question>\"]}.\n\
- `summary`: a `TL;DR:` line followed by 1-3 bullet points (`- ` lines) giving the headline \
outcome, for someone who did not watch you work. Keep it short — the per-file detail belongs in \
`changes`.\n\
- `changes`: one entry per file you meaningfully touched, covering everything in the diff \
(features, refactors, tests, docs, config). Say plainly in `what` anything you had to work \
around or deliberately left out; if part of the task is unfinished, or you are unsure a change is \
right, say so there.\n";

/// The `tests` bullet for a project that can run its app: the reviewer has this
/// card's preview, so the checklist has to be executable in it — and only in
/// it. The environment is re-created from scratch when the reviewer restarts
/// the preview, so a scenario leaning on a row the agent hand-made during its
/// own run is dead on arrival; and preview ports are per-worktree, so a
/// hard-coded URL is too.
///
/// The seed itself is hedged: all `local_app` proves is a `run_script`, while
/// the setup script that would re-seed is auto-detected and optional — so a
/// project can perfectly well have a runnable app and no seed at all, and
/// asserting one as fact would box the agent in on a false premise about its
/// own environment. The route and no-hard-coded-URL requirements hold
/// unconditionally.
const TESTS_SEEDED: &str = "\
a hand-testing script for the reviewer, who will run this card's app locally — the same \
worktree preview you had, but restarted from scratch: the setup re-creates the environment, \
re-seeding its data if the project seeds any, so anything you created by hand during your run is \
gone. Every scenario must be executable in that local environment and nothing else — no production \
or staging data, no hand-edited database rows, and, where the project seeds data, no account or \
record that does not exist in the seed. Be concrete: name the path to open (a route like \
`/settings/billing`, never a hard-coded URL — the port is per-worktree), the account to sign in as \
with the exact credentials the repo's own seed/fixture data gives it (never \"log in as an \
admin\"), and the records to act on by the names they carry in the seed. If the seed does not \
already contain the state a scenario needs, make creating it the first steps of the scenario. \
`scenario` is what to do (sign in → route → actions), `expect` is what should happen. Most \
important first, and favour what your automated tests do not already cover: the risky paths, the \
edge cases you touched, the flows a regression would hide in. Set `verified: true` only for a \
scenario you actually ran yourself in the app, as written — it still deserves a human eye, but the \
reviewer should know it has been run once.";

/// [`TESTS_SEEDED`] for a card that may carry a scenario script
/// (`.usine-scenario.sh`, replayed after setup on every fresh preview start):
/// the checklist may lean on the records it creates as well as on the seed,
/// and adding missing state to the script beats making the reviewer build it
/// by hand. Same rules otherwise.
const TESTS_SCENARIO: &str = "\
a hand-testing script for the reviewer, who will run this card's app locally — the same \
worktree preview you had, but restarted from scratch: the setup re-creates the environment, \
re-seeding its data if the project seeds any, then replays this card's `.usine-scenario.sh` if it \
has one, so anything you created by hand during your run is gone. Every scenario must be \
executable in that local environment and nothing else — no production or staging data, no \
hand-edited database rows, and, where the project seeds data, no account or record that exists \
neither in the seed nor in this card's `.usine-scenario.sh`. Be concrete: name the path to open (a \
route like `/settings/billing`, never a hard-coded URL — the port is per-worktree), the account to \
sign in as with the exact credentials the repo's own seed/fixture data (or the scenario script) \
gives it (never \"log in as an admin\"), and the records to act on by the names they carry in the \
seed or the scenario script — say which records the script created where a scenario uses them. If \
neither already contains the state a scenario needs, add it to the scenario script; failing that, \
make creating it the first steps of the scenario. `scenario` is what to do (sign in → route → \
actions), `expect` is what should happen. Most important first, and favour what your automated \
tests do not already cover: the risky paths, the edge cases you touched, the flows a regression \
would hide in. Set `verified: true` only for a scenario you actually ran yourself in the app, as \
written — it still deserves a human eye, but the reviewer should know it has been run once.";

/// The `tests` bullet for a project with no runnable app: same checklist, no
/// environment to promise.
const TESTS_GENERIC: &str = "\
what is worth checking by hand in the running app, most important first. `scenario` is \
what to do, `expect` is what should happen. Favour what your automated tests do not already cover: \
the risky paths, the edge cases you touched, the flows a regression would hide in. Set \
`verified: true` only for a scenario you actually exercised yourself in the running app — it still \
deserves a human eye, but the reviewer should know it has been run once.";

const HANDOFF_TAIL: &str = "\
- `risks`: what could bite the reviewer or a user — a migration, a behaviour change, something \
you could not test. Empty array if none.\n\
- `questions`: what you genuinely want the author to weigh in on — a judgement call that could \
reasonably have gone the other way, an ambiguity in the task, an assumption you had to invent. Use \
an empty array if you have none; do NOT manufacture questions to fill it.\n\
Be honest and specific — this is the note a careful engineer leaves a colleague, not a sales pitch.";

/// Appended to a "Request changes" run instead of [`handoff_instruction`]: the
/// original summary keeps describing the work as a whole, and this run's
/// final message becomes its own entry in the Agent Chat log, under the
/// request it answers.
///
/// What a change can falsify — the "Test it" checklist, the risks, the open
/// questions — it is shown and may revise (see [`revision_instruction`]).
pub fn change_recap_instruction(local_app: bool, scenario: bool, current: &Handoff) -> String {
    format!(
        "{CHANGE_RECAP_HEADER}\n\n{}",
        revision_instruction(local_app, scenario, current)
    )
}

/// Appended (after [`crate::agent::fixes::FIX_RECAP_INSTRUCTION`]) to a fix
/// run — self-review fixes, validation fixes, PR-comment fixes, an update or
/// conflict resolution — on a card that already has a hand-off: those runs
/// change code too, so they may falsify it just as a requested change can.
pub fn fix_revision_instruction(local_app: bool, scenario: bool, current: &Handoff) -> String {
    format!(
        "Your fixes may have made parts of the reviewer's hand-off note for this task wrong. \
The note's summary stays as it is.\n\n{}",
        revision_instruction(local_app, scenario, current)
    )
}

/// Shows a run the hand-off's current "Test it", "Risks" and "Open questions"
/// and asks for a `usine-handoff-revision` block carrying only the lists it
/// made wrong (see [`revise_handoff`]). No block means "still accurate", so
/// none of them silently describes code the branch no longer has.
fn revision_instruction(local_app: bool, scenario: bool, current: &Handoff) -> String {
    let tests = if current.tests.is_empty() {
        "(none)\n".to_string()
    } else {
        current
            .tests
            .iter()
            // The exact JSON objects the block takes, so an unchanged item
            // (and its `verified` flag) can be copied verbatim rather than
            // reassembled from display text.
            .map(|t| format!("- {}\n", serde_json::to_string(t).unwrap_or_default()))
            .collect()
    };
    let list = |items: &[String]| -> String {
        if items.is_empty() {
            "(none)\n".to_string()
        } else {
            items.iter().map(|i| format!("- {i}\n")).collect()
        }
    };
    let risks = list(&current.risks);
    let questions = list(&current.questions);
    let guidance = tests_guidance(local_app, scenario);
    format!(
        "Below are the \"Test it\" checklist, the risks and the open questions of the hand-off \
note the reviewer will read. If the code changes in this run make any of them wrong, emit a \
fenced code block tagged `{REVISION_TAG}` containing a JSON object with ONLY the keys whose list \
is now wrong, each holding its COMPLETE updated list for the branch as it now stands, shaped \
like {{\"tests\": [{{\"scenario\": \"<what to do>\", \"expect\": \"<what should happen>\", \
\"verified\": false}}], \"risks\": [\"<what could bite>\"], \"questions\": [\"<open \
question>\"]}}. A missing key means that list is still accurate; an empty list means nothing is \
left in it, and wipes it. Drop a question this change answered, add a risk this change \
introduced, remove a risk it eliminated; for tests keep what still holds, rewrite what this \
change altered, add new items, drop obsolete ones. If you re-send `tests`, copy every unchanged \
item's JSON object exactly as listed, `verified` flag included; set `verified: true` on a new or changed item \
only if you exercised it against the code as it now stands. Send no block at all if everything \
listed below still holds — in particular after a change with no observable effect of its own \
(a refactor, a rename in code).\n\
Current \"Test it\" checklist:\n{tests}\
Current risks:\n{risks}\
Current open questions:\n{questions}\
Test items follow the same rules as the original hand-off's checklist — {guidance}"
    )
}

const CHANGE_RECAP_HEADER: &str = "\
Your final message is shown to the user as the recap of this change request, beside their \
request; the original summary of the task stays as it is, so do not restate it. Start with a \
`TL;DR:` line followed by 1-3 bullet points (`- ` lines) covering only what this change did. Do \
not emit a hand-off block.";

/// One file the run touched, as the agent describes it. The path is a claim,
/// not a fact: the panel cross-checks it against the computed diff and marks
/// anything that isn't actually there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Repo-relative path, as the agent wrote it.
    #[serde(default)]
    pub path: String,
    /// What changed in that file.
    #[serde(default)]
    pub what: String,
    /// `feat` / `fix` / `test` / `docs` / `chore`. Free-form: an agent that
    /// invents a word gets it rendered as-is rather than dropped.
    #[serde(default)]
    pub kind: String,
}

/// One thing worth exercising by hand.
///
/// Deserializes from either the v2 object or a bare v1 string — in which case
/// the whole string is the scenario, and a leading `[verified]` marker (the
/// convention v2 replaces with a field) is consumed rather than displayed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TestItem {
    /// What to do.
    #[serde(default)]
    pub scenario: String,
    /// What should happen. Empty for a v1 entry, which said both at once.
    #[serde(default)]
    pub expect: String,
    /// The agent claims to have exercised this itself in the running app.
    #[serde(default)]
    pub verified: bool,
}

impl<'de> Deserialize<'de> for TestItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The object form, spelled out so the string arm can't recurse into it.
        #[derive(Deserialize)]
        struct Object {
            #[serde(default)]
            scenario: String,
            #[serde(default)]
            expect: String,
            #[serde(default)]
            verified: bool,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Object(Object),
        }

        match Either::deserialize(deserializer).map_err(de::Error::custom)? {
            Either::Text(text) => Ok(TestItem::from_v1(&text)),
            Either::Object(o) => Ok(TestItem {
                scenario: o.scenario.trim().to_string(),
                expect: o.expect.trim().to_string(),
                verified: o.verified,
            }),
        }
    }
}

/// A plain checklist line, in the v1 spelling — the shape both a legacy
/// payload and a hand-written caller reach for.
impl From<&str> for TestItem {
    fn from(text: &str) -> Self {
        TestItem::from_v1(text)
    }
}

impl TestItem {
    /// A v1 checklist line, with the `[verified]` prefix migrated to the field.
    fn from_v1(text: &str) -> Self {
        let text = text.trim();
        match text.strip_prefix("[verified]") {
            Some(rest) => TestItem {
                scenario: rest.trim().to_string(),
                expect: String::new(),
                verified: true,
            },
            None => TestItem {
                scenario: text.to_string(),
                expect: String::new(),
                verified: false,
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.scenario.is_empty() && self.expect.is_empty()
    }
}

/// An implement run's hand-off to its reviewer. Every field is optional: an agent
/// with nothing to ask emits `questions: []`, and a change with no observable
/// behaviour emits `tests: []`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// Payload schema version. Absent (v1 payloads, and v1 records already on
    /// disk) reads as 1.
    #[serde(default = "v1")]
    pub v: u8,
    /// A short recap of the work done: a `TL;DR:` header with bullet points.
    /// In v1 payloads this also carries the per-change lines.
    #[serde(default)]
    pub summary: String,
    /// The files the run says it touched. Empty for v1.
    #[serde(default)]
    pub changes: Vec<Change>,
    /// Things worth testing by hand, most important first.
    #[serde(default)]
    pub tests: Vec<TestItem>,
    /// What could bite the reviewer or a user. Empty for v1.
    #[serde(default)]
    pub risks: Vec<String>,
    /// Decisions or ambiguities the agent wants the user to weigh in on.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Not part of the payload: set by [`handoff_from_reply`] when the agent
    /// emitted a `usine-handoff` block whose JSON didn't parse. The panel says
    /// so, rather than leaving the reviewer to read "no hand-off" as "the run
    /// had nothing to say".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malformed: bool,
    /// Not part of the payload: set by the executor when a later run (a
    /// requested change, a fix run) sent a `usine-handoff-revision` block that
    /// didn't parse, so the tests, risks or questions may be stale. Cleared by
    /// the next revision that applies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revision_malformed: bool,
}

fn v1() -> u8 {
    1
}

impl Default for Handoff {
    fn default() -> Self {
        Handoff {
            v: VERSION,
            summary: String::new(),
            changes: Vec::new(),
            tests: Vec::new(),
            risks: Vec::new(),
            questions: Vec::new(),
            malformed: false,
            revision_malformed: false,
        }
    }
}

impl Handoff {
    /// Nothing to show. The executor stores this as "no hand-off" rather than an
    /// empty card section.
    pub fn is_empty(&self) -> bool {
        !self.malformed
            && self.summary.trim().is_empty()
            && self.changes.is_empty()
            && self.tests.is_empty()
            && self.risks.is_empty()
            && self.questions.is_empty()
    }
}

/// Extract the agent's hand-off from its `usine-handoff` block. `None` when the
/// block is missing, malformed, or carries nothing — in every case the card ends
/// up with no hand-off rather than a broken one. Use [`handoff_block_malformed`]
/// to tell "the agent said nothing" from "the agent garbled it".
pub fn parse_handoff(text: &str) -> Option<Handoff> {
    let raw = block::parse::<Handoff>(text, TAG).0.ok()?;
    let handoff = Handoff {
        v: raw.v,
        malformed: false,
        revision_malformed: false,
        summary: raw.summary.trim().to_string(),
        changes: raw
            .changes
            .into_iter()
            .map(|c| Change {
                path: c.path.trim().to_string(),
                what: c.what.trim().to_string(),
                kind: c.kind.trim().to_string(),
            })
            .filter(|c| !(c.path.is_empty() && c.what.is_empty()))
            .collect(),
        tests: raw.tests.into_iter().filter(|t| !t.is_empty()).collect(),
        risks: trimmed(raw.risks),
        questions: trimmed(raw.questions),
    };
    (!handoff.is_empty()).then_some(handoff)
}

/// The hand-off to store for a finished implement run: the parsed one, or a
/// hand-off flagged [`Handoff::malformed`] when the agent emitted a block that
/// didn't parse, or the empty (= "none") one when it emitted no block at all.
pub fn handoff_from_reply(text: &str) -> Handoff {
    match parse_handoff(text) {
        Some(handoff) => handoff,
        None if handoff_block_malformed(text) => Handoff {
            malformed: true,
            ..Handoff::default()
        },
        None => Handoff::default(),
    }
}

/// True when the reply carries a `usine-handoff` block whose JSON doesn't parse
/// — the agent tried to hand off and garbled it. [`parse_handoff`] yields
/// nothing then; the panel uses this to say the note was dropped rather than
/// leaving the reviewer to assume the run had nothing to say.
pub fn handoff_block_malformed(text: &str) -> bool {
    block::parse::<Handoff>(text, TAG).0.is_malformed()
}

/// The reply with the `usine-handoff` block removed, for the transcript (the
/// block is rendered as its own panel section, not as agent prose).
pub fn strip_handoff_block(text: &str) -> String {
    block::strip(text, TAG)
}

/// The payload of a legacy `usine-tests` block. `tests` is required: an empty
/// list wipes the checklist, so only an explicit `"tests": []` may say so — a
/// missing or misspelled key makes the block malformed and the list stands.
#[derive(Deserialize)]
struct TestsBlock {
    tests: Vec<TestItem>,
}

/// The payload of a `usine-handoff-revision` block. Each key is optional — a
/// missing one leaves that list alone — but an unknown one (a typo like
/// `"risk"`) makes the block malformed, so the user is told, rather than the
/// revision being silently dropped. `v` is tolerated like on every block.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionBlock {
    #[serde(default)]
    #[allow(dead_code)]
    v: Option<u32>,
    #[serde(default)]
    tests: Option<Vec<TestItem>>,
    #[serde(default)]
    risks: Option<Vec<String>>,
    #[serde(default)]
    questions: Option<Vec<String>>,
}

/// What a later run revised in the hand-off. `None` fields are left as
/// stored; `Some(vec![])` is a real answer: nothing is left in that list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffRevision {
    pub tests: Option<Vec<TestItem>>,
    pub risks: Option<Vec<String>>,
    pub questions: Option<Vec<String>>,
}

impl HandoffRevision {
    fn is_empty(&self) -> bool {
        self.tests.is_none() && self.risks.is_none() && self.questions.is_none()
    }
}

fn clean_tests(tests: Vec<TestItem>) -> Vec<TestItem> {
    tests.into_iter().filter(|t| !t.is_empty()).collect()
}

/// The revision a run sent in its `usine-handoff-revision` block — or, from a
/// run launched by an older build, in a legacy `usine-tests` block (a
/// tests-only revision). `None` when there is no usable block, or the block
/// revises nothing (`{}`): either way the stored hand-off stands.
pub fn parse_revision_block(text: &str) -> Option<HandoffRevision> {
    if let Some(raw) = block::parse::<RevisionBlock>(text, REVISION_TAG).0.ok() {
        let revision = HandoffRevision {
            tests: raw.tests.map(clean_tests),
            risks: raw.risks.map(trimmed),
            questions: raw.questions.map(trimmed),
        };
        if !revision.is_empty() {
            return Some(revision);
        }
    }
    let raw = block::parse::<TestsBlock>(text, TESTS_TAG).0.ok()?;
    Some(HandoffRevision {
        tests: Some(clean_tests(raw.tests)),
        ..HandoffRevision::default()
    })
}

/// True when the reply carries a revision block (either spelling) that
/// doesn't parse — the agent tried to revise the hand-off and garbled it, so
/// the stored lists were kept and may be stale.
pub fn revision_block_malformed(text: &str) -> bool {
    block::parse::<RevisionBlock>(text, REVISION_TAG)
        .0
        .is_malformed()
        || block::parse::<TestsBlock>(text, TESTS_TAG).0.is_malformed()
}

/// The reply with any revision block (either spelling) removed, for the
/// Agent Chat recap and the fixes recap.
pub fn strip_revision_block(text: &str) -> String {
    block::strip(&block::strip(text, REVISION_TAG), TESTS_TAG)
}

/// The hand-off to store after a later run: `stored` with each list the
/// reply's revision block sent replaced, the rest — `verified` flags of an
/// un-re-sent checklist included — untouched (a fresh hand-off when nothing
/// was stored). A revision that applies clears
/// [`Handoff::revision_malformed`]. `None` when the reply revises nothing —
/// nothing to write, the stored hand-off still holds.
pub fn revise_handoff(stored: Option<Handoff>, reply: &str) -> Option<Handoff> {
    let revision = parse_revision_block(reply)?;
    let mut handoff = stored.unwrap_or_default();
    if let Some(tests) = revision.tests {
        handoff.tests = tests;
    }
    if let Some(risks) = revision.risks {
        handoff.risks = risks;
    }
    if let Some(questions) = revision.questions {
        handoff.questions = questions;
    }
    handoff.revision_malformed = false;
    Some(handoff)
}

/// Trim each entry and drop the ones that were only whitespace.
fn trimmed(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_v2_handoff() {
        let text = "Done.\n\n```usine-handoff\n{\
            \"v\": 2,\
            \"summary\": \"TL;DR: added the filter panel. \",\
            \"changes\": [{\"path\":\" src/ui/filter.rs \",\"what\":\"new panel\",\"kind\":\"feat\"}, {\"path\":\"\",\"what\":\"\"}],\
            \"tests\": [{\"scenario\":\"Filter by owner\",\"expect\":\"the list narrows\",\"verified\":true}],\
            \"risks\": [\"The saved filter is not migrated\", \" \"],\
            \"questions\": [\"Should the filter persist across reloads?\"]\
            }\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.v, 2);
        assert_eq!(
            h.summary, "TL;DR: added the filter panel.",
            "summary is trimmed"
        );
        assert_eq!(h.changes.len(), 1, "the empty change entry is dropped");
        assert_eq!(h.changes[0].path, "src/ui/filter.rs", "paths are trimmed");
        assert_eq!(h.tests[0].expect, "the list narrows");
        assert!(h.tests[0].verified);
        assert_eq!(h.risks, vec!["The saved filter is not migrated"]);
        assert_eq!(h.questions.len(), 1);
    }

    #[test]
    fn a_v1_payload_still_parses_and_migrates_the_verified_prefix() {
        let text = "```usine-handoff\n{\
            \"summary\": \"Renamed a field.\",\
            \"questions\": [],\
            \"tests\": [\"[verified] Open the board — it loads\", \"Reload\", \"  \"]\
            }\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.v, 1, "a payload without `v` reads as v1");
        assert_eq!(h.tests.len(), 2, "the whitespace-only entry is dropped");
        assert_eq!(h.tests[0].scenario, "Open the board — it loads");
        assert!(h.tests[0].verified, "the [verified] prefix became a field");
        assert!(!h.tests[1].verified);
        assert!(h.changes.is_empty() && h.risks.is_empty());
    }

    #[test]
    fn a_v1_persisted_record_round_trips_through_the_v2_struct() {
        // What `CardReviewRecord.handoff` holds for a card handed off before v2.
        let stored =
            "{\"summary\":\"did the thing\",\"questions\":[\"why?\"],\"tests\":[\"click it\"]}";
        let h: Handoff = serde_json::from_str(stored).unwrap();
        assert_eq!(h.summary, "did the thing");
        assert_eq!(h.tests[0].scenario, "click it");
        // …and re-serializing it yields the v2 shape the panel reads back.
        let again: Handoff = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(again, h);
    }

    #[test]
    fn a_partial_handoff_still_parses() {
        // No questions, no tests: a change with nothing to ask and nothing to click.
        let h = parse_handoff("```usine-handoff\n{\"summary\":\"Renamed a field.\"}\n```").unwrap();
        assert_eq!(h.summary, "Renamed a field.");
        assert!(h.questions.is_empty() && h.tests.is_empty());

        // Only changes: no prose, but still worth showing.
        let h = parse_handoff(
            "```usine-handoff\n{\"changes\":[{\"path\":\"a.rs\",\"what\":\"x\"}]}\n```",
        )
        .unwrap();
        assert!(h.summary.is_empty());
        assert_eq!(h.changes[0].path, "a.rs");
    }

    #[test]
    fn missing_malformed_or_empty_yields_none() {
        assert_eq!(parse_handoff("no block"), None);
        assert_eq!(parse_handoff("```usine-handoff\nnot json\n```"), None);
        assert_eq!(parse_handoff("```usine-handoff\n{}\n```"), None);
        // Present but vacuous — nothing to render.
        assert_eq!(
            parse_handoff("```usine-handoff\n{\"summary\":\"  \",\"tests\":[\" \"]}\n```"),
            None
        );
        // Only the garbled case is worth telling the user about.
        assert!(handoff_block_malformed("```usine-handoff\nnot json\n```"));
        assert!(!handoff_block_malformed("no block"));
        assert!(!handoff_block_malformed("```usine-handoff\n{}\n```"));
    }

    #[test]
    fn a_summary_quoting_a_code_fence_survives() {
        // The regression Phase 0 fixed: the inner fence used to close the block.
        let text = "Done.\n\n```usine-handoff\n{\"summary\":\"Run ```cargo test``` first.\"}\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.summary, "Run ```cargo test``` first.");
    }

    #[test]
    fn a_garbled_block_is_stored_as_a_malformed_handoff() {
        let h = handoff_from_reply("```usine-handoff\nnot json\n```");
        assert!(h.malformed);
        assert!(!h.is_empty(), "malformed is worth a panel line of its own");
        // No block at all stays "no hand-off".
        assert!(handoff_from_reply("just prose").is_empty());
        // It survives a store round-trip, so the panel still says so after a
        // restart — but it never clutters an ordinary hand-off's JSON.
        let back: Handoff = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert!(back.malformed);
        let ordinary = parse_handoff("```usine-handoff\n{\"summary\":\"x\"}\n```").unwrap();
        assert!(!serde_json::to_string(&ordinary)
            .unwrap()
            .contains("malformed"));
    }

    #[test]
    fn strips_the_block_but_leaves_the_commit_one() {
        let text = "All done.\n\n```usine-handoff\n{\"summary\":\"x\"}\n```\n\n```usine-commit\nfeat: x\n```";
        let stripped = strip_handoff_block(text);
        assert!(!stripped.contains("usine-handoff"));
        assert!(
            stripped.contains("```usine-commit"),
            "the commit block is another parser's to remove"
        );
    }

    #[test]
    fn the_change_recap_instruction_never_asks_for_a_hand_off_block() {
        for local_app in [true, false] {
            let s = change_recap_instruction(local_app, false, &Handoff::default());
            assert!(!s.contains("`usine-handoff`"), "local_app={local_app}");
            assert!(s.contains("TL;DR"));
            assert!(s.contains(REVISION_TAG));
            assert!(!s.contains(TESTS_TAG), "the legacy tag is no longer asked for");
            assert_eq!(s.matches("(none)").count(), 3, "tests, risks, questions");
        }
    }

    fn stored_handoff() -> Handoff {
        Handoff {
            summary: "TL;DR: form".into(),
            changes: vec![Change {
                path: "a.rs".into(),
                what: "x".into(),
                kind: "feat".into(),
            }],
            tests: vec![
                TestItem {
                    scenario: "Click Submit".into(),
                    expect: "the form saves".into(),
                    verified: true,
                },
                TestItem::from("Reload the board"),
            ],
            risks: vec!["The default changed".into()],
            questions: vec!["Keep the old label?".into()],
            ..Handoff::default()
        }
    }

    #[test]
    fn the_revision_prompt_lists_tests_risks_and_questions() {
        let current = stored_handoff();
        for s in [
            change_recap_instruction(true, false, &current),
            fix_revision_instruction(true, false, &current),
        ] {
            // The same JSON objects the block takes, ready to copy verbatim.
            assert!(s.contains(
                "- {\"scenario\":\"Click Submit\",\"expect\":\"the form saves\",\"verified\":true}\n"
            ));
            assert!(s.contains(
                "- {\"scenario\":\"Reload the board\",\"expect\":\"\",\"verified\":false}\n"
            ));
            assert!(s.contains("Current risks:\n- The default changed\n"));
            assert!(s.contains("Current open questions:\n- Keep the old label?\n"));
            assert!(!s.contains("(none)"));
            assert!(s.contains("ONLY the keys"));
            assert!(s.contains("`verified` flag included"));
            assert!(s.contains("Drop a question this change answered"));
            assert!(!s.contains("`usine-handoff`"), "never a full hand-off");
            // The same writing rules as the hand-off's checklist.
            assert!(s.contains("never a hard-coded URL"));
        }
        assert!(!change_recap_instruction(false, false, &current).contains("hard-coded URL"));
        assert!(fix_revision_instruction(true, false, &current).contains("Your fixes may have"));
    }

    #[test]
    fn parses_each_revision_key_alone_or_together() {
        let block = |json: &str| format!("TL;DR: x.\n\n```{REVISION_TAG}\n{json}\n```");
        let r = parse_revision_block(&block(r#"{"risks": [" r1 ", " "]}"#)).unwrap();
        assert_eq!(r.risks, Some(vec!["r1".to_string()]));
        assert_eq!((r.tests, r.questions), (None, None));

        let r = parse_revision_block(&block(r#"{"questions": []}"#)).unwrap();
        assert_eq!(r.questions, Some(vec![]), "an empty list is an answer");

        let r = parse_revision_block(&block(
            r#"{"v":1,"tests":[{"scenario":"Click Save","expect":"saved"}," "],"risks":[],"questions":["q"]}"#,
        ))
        .unwrap();
        assert_eq!(r.tests.unwrap().len(), 1, "the blank test is dropped");
        assert_eq!(r.risks, Some(vec![]));
        assert_eq!(r.questions, Some(vec!["q".to_string()]));

        assert_eq!(parse_revision_block(&block("{}")), None, "{{}} revises nothing");
        assert!(!revision_block_malformed(&block("{}")));
        assert_eq!(parse_revision_block("no block"), None);
        assert!(!revision_block_malformed("no block"));
        for garbled in ["not json", r#"{"risk": ["typo"]}"#, r#"["r"]"#, r#"{"risks": "r"}"#] {
            assert_eq!(parse_revision_block(&block(garbled)), None, "{garbled}");
            assert!(revision_block_malformed(&block(garbled)), "{garbled}");
        }
    }

    #[test]
    fn a_legacy_tests_block_is_a_tests_only_revision() {
        let text = "TL;DR: renamed.\n\n```usine-tests\n{\"tests\": [\
            {\"scenario\":\"Click Save\",\"expect\":\"saved\"},\
            \"[verified] Reload\", \" \"]}\n```";
        let r = parse_revision_block(text).unwrap();
        let tests = r.tests.unwrap();
        assert_eq!(tests.len(), 2, "the blank entry is dropped");
        assert_eq!(tests[1].scenario, "Reload");
        assert!(tests[1].verified);
        assert_eq!((r.risks, r.questions), (None, None));
        assert!(revision_block_malformed("```usine-tests\n{}\n```"));
        assert_eq!(
            parse_revision_block("```usine-tests\n{\"tests\": []}\n```")
                .unwrap()
                .tests,
            Some(vec![])
        );
    }

    #[test]
    fn strips_both_revision_spellings() {
        let text = format!(
            "TL;DR: x.\n\n```{REVISION_TAG}\n{{\"risks\":[]}}\n```\n\n```usine-tests\n{{\"tests\":[]}}\n```"
        );
        assert_eq!(strip_revision_block(&text), "TL;DR: x.");
    }

    #[test]
    fn revising_replaces_only_the_lists_it_sends() {
        let stored = stored_handoff();
        let reply = format!(
            "```{REVISION_TAG}\n{{\"risks\": [\"Needs a migration\"], \"questions\": []}}\n```"
        );
        let revised = revise_handoff(Some(stored.clone()), &reply).unwrap();
        assert_eq!(revised.risks, vec!["Needs a migration"]);
        assert!(revised.questions.is_empty());
        assert_eq!(
            revised.tests, stored.tests,
            "un-re-sent tests keep their verified flags"
        );
        assert!(revised.tests[0].verified);
        assert_eq!(
            (revised.summary.clone(), revised.changes.clone()),
            (stored.summary.clone(), stored.changes.clone())
        );

        // A revision that applies clears an earlier garbled one's flag.
        let flagged = Handoff {
            revision_malformed: true,
            ..stored.clone()
        };
        assert!(!revise_handoff(Some(flagged), &reply).unwrap().revision_malformed);

        let fresh = revise_handoff(None, &reply).unwrap();
        assert!(fresh.summary.is_empty() && fresh.tests.is_empty());

        assert_eq!(revise_handoff(Some(stored), "TL;DR: refactor only."), None);
    }

    #[test]
    fn revision_malformed_round_trips_but_stays_out_of_ordinary_json() {
        let h = Handoff {
            revision_malformed: true,
            ..stored_handoff()
        };
        let back: Handoff = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert!(back.revision_malformed);
        assert!(!serde_json::to_string(&stored_handoff())
            .unwrap()
            .contains("revision_malformed"));
        assert!(
            Handoff {
                revision_malformed: true,
                ..Handoff::default()
            }
            .is_empty(),
            "the flag alone is nothing to show"
        );
    }

    #[test]
    fn the_instruction_states_the_v2_contract() {
        for local_app in [true, false] {
            let s = handoff_instruction(local_app, false);
            for field in [
                "\"v\": 2",
                "changes",
                "risks",
                "verified",
                "Use an empty array",
            ] {
                assert!(s.contains(field), "missing {field} (local_app={local_app})");
            }
        }
    }

    /// The seeded variant is the whole point of the split: it may only be
    /// promised where the reviewer actually has the card's preview to run.
    #[test]
    fn only_the_local_app_variant_demands_the_seeded_environment() {
        let seeded = handoff_instruction(true, false);
        for phrase in [
            "executable in that local environment",
            "if the project seeds any",
            "exact credentials",
            "seed/fixture data",
            "no hand-edited database rows",
            "never a hard-coded URL",
        ] {
            assert!(seeded.contains(phrase), "missing {phrase}");
        }

        let generic = handoff_instruction(false, false);
        for phrase in ["seed", "credentials", "hard-coded URL"] {
            assert!(
                !generic.contains(phrase),
                "the no-app variant must not mention {phrase}"
            );
        }
        // …but it still asks for the same checklist.
        assert!(generic.contains("worth checking by hand in the running app"));
        assert!(generic.contains("most important first"));
    }

    /// The scenario variant lets checklist records come from the card's
    /// script, and only it mentions the script at all.
    #[test]
    fn only_the_scenario_variant_mentions_the_script() {
        let scen = handoff_instruction(true, true);
        assert!(scen.contains(".usine-scenario.sh"));
        assert!(scen.contains("add it to the scenario script"));
        assert!(scen.contains("no hand-edited database rows"));
        assert!(scen.contains("never a hard-coded URL"));
        assert!(!handoff_instruction(true, false).contains(".usine-scenario.sh"));
        // No runnable app → no environment, script or not.
        assert!(!handoff_instruction(false, true).contains(".usine-scenario.sh"));
        assert!(change_recap_instruction(true, true, &Handoff::default()).contains(".usine-scenario.sh"));
        assert!(!change_recap_instruction(true, false, &Handoff::default()).contains(".usine-scenario.sh"));
    }
}

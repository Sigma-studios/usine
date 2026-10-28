//! Typed-but-unsent input that must outlive its panel.
//!
//! The detail panels are deliberately torn down on deselect and remounted on
//! every card/review state change (see `detail/mod.rs`), so any `use_signal`
//! they hold is wiped by a background poll or a finishing run. Fields whose
//! text should survive that opt into this store instead: one global map keyed
//! by (owner id, field name), with two hooks that are drop-in replacements for
//! `use_signal`.
//!
//! The rules that keep the map from ever showing stale data:
//! - **Seed rule** — a value equal to its seed is *removed* from the map, so an
//!   untouched field never shadows fresher seeds on remount. The rule runs
//!   in an effect, though, and a send usually remounts the panel before that
//!   effect gets to run — so send sites use [`clear`], never a bare reset.
//! - **Origin rule** (`use_draft_of`) — a working copy of agent output is
//!   stored alongside a fingerprint of the payload it edits; a new agent run
//!   (new verdicts, a replanned plan, a different question) reseeds instead of
//!   restoring edits that no longer apply. Same idea as `reviewdraft`'s
//!   `origin` field.
//!
//! Entries die with their owner: `forget_owner` runs on `CardRemoved` and
//! `ProjectRemoved`, and `AppState` drops a card's drafts once nothing on it
//! can be sent any more (PR created, card done).
//!
//! The map — together with `reviewdraft`'s buffer — is persisted, so drafts
//! survive a restart or a crash: every change bumps [`REVISION`], the root
//! component's [`use_autosave`] loop saves a [`snapshot`] once typing pauses
//! for about a second, and `quit_app` sends a last one. [`load`] fills the map
//! once at startup. Both rules above still hold for restored entries.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dioxus::prelude::*;
use serde::{de::DeserializeOwned, Serialize};
use usine_core::{ExecutorCommand, ExecutorHandle, StoredDraft, UiDrafts};
use uuid::Uuid;

/// Every field name a `use_draft`/`use_draft_of` call site uses. Loading maps a
/// saved name back through this list (keys hold `&'static str`), so a field
/// that's renamed or removed just drops its old draft.
const FIELDS: &[&str] = &[
    "chat",
    "session-message",
    "pr.title",
    "pr.body",
    "pr.reviewer",
    "pr.branch",
    "fixes.verdicts",
    "fixes.task",
    "fixes.note",
    "update.note",
    "plan.answers",
    "investigate.answers",
    "investigate.followup",
    "intervention.answer",
    "intervention.option",
    "review.guidance",
    "review.fixnote",
];

/// One draft slot: the card or review it belongs to, plus a per-panel field
/// name (e.g. `"chat"`, `"pr.body"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DraftKey {
    pub owner: Uuid,
    pub field: &'static str,
}

static DRAFTS: GlobalSignal<HashMap<DraftKey, String>> = Signal::global(HashMap::new);

/// Drop-in for `use_signal(String::new)` (or a seeded closure) on any text
/// input whose draft should outlive the panel. Restores on remount, mirrors on
/// every edit, and forgets itself when the text returns to `seed` — so call
/// sites that clear on send need no extra bookkeeping, and a seeded field
/// stays live-fresh until actually touched.
pub fn use_draft(
    owner: Uuid,
    field: &'static str,
    seed: impl FnOnce() -> String,
) -> Signal<String> {
    debug_assert!(FIELDS.contains(&field), "draft field {field:?} missing from FIELDS");
    let key = DraftKey { owner, field };
    let seeded = seed();
    let init = seeded.clone();
    // `peek`, not `read`: subscribing the panel to the whole map would
    // re-render every draft-bearing component on any keystroke anywhere.
    let sig = use_signal(|| restore_str(&DRAFTS.peek(), key).unwrap_or(init));
    use_effect(move || {
        let v = sig.read().clone();
        #[cfg(debug_assertions)]
        MIRROR_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(debug_assertions)]
        if mirror_held() {
            return;
        }
        if mirror_str(&mut DRAFTS.write(), key, v, &seeded) {
            bump();
        }
    });
    sig
}

/// How many times the mirror effect has run, across every draft field. The
/// keystroke-drop harness reports it: under a heavy enough render load Dioxus
/// never reaches the point where queued effects run, and the draft store stops
/// mirroring altogether.
#[cfg(debug_assertions)]
pub static MIRROR_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// While set, the mirror effects skip their write (they still subscribe). The
/// sent-draft checks hold it across a send and the remount that follows, which
/// pins the race a real send can lose — state change before the mirror effect —
/// instead of hoping the scheduler loses it.
#[cfg(debug_assertions)]
pub static HOLD_MIRROR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(debug_assertions)]
fn mirror_held() -> bool {
    HOLD_MIRROR.load(std::sync::atomic::Ordering::Relaxed)
}

/// Origin-keyed variant for working copies of agent output (fix verdicts, plan
/// answers, an intervention's typed answer). The draft is stored alongside a
/// fingerprint of `origin` and restored only while the current origin still
/// matches — a fresh agent run reseeds instead of showing stale edits. The
/// seed rule applies as in [`use_draft`].
pub fn use_draft_of<O, T>(
    owner: Uuid,
    field: &'static str,
    origin: &O,
    seed: impl FnOnce() -> T,
) -> Signal<T>
where
    O: Serialize,
    T: Serialize + DeserializeOwned + Clone + PartialEq + 'static,
{
    debug_assert!(FIELDS.contains(&field), "draft field {field:?} missing from FIELDS");
    let key = DraftKey { owner, field };
    let fp = serde_json::to_string(origin).unwrap_or_default();
    let seeded = seed();
    let init = seeded.clone();
    let init_fp = fp.clone();
    let sig = use_signal(|| restore_typed::<T>(&DRAFTS.peek(), key, &init_fp).unwrap_or(init));
    use_effect(move || {
        let v = sig.read().clone();
        #[cfg(debug_assertions)]
        if mirror_held() {
            return;
        }
        if mirror_typed(&mut DRAFTS.write(), key, &fp, &v, &seeded) {
            bump();
        }
    });
    sig
}

/// Drop one field's draft — for sends that don't clear the signal back to its
/// seed (the confirm dialog fires the command, or the seed is nonempty).
pub fn forget(owner: Uuid, field: &'static str) {
    if DRAFTS.write().remove(&DraftKey { owner, field }).is_some() {
        bump();
    }
}

/// Clear a draft at send time: reset the signal *and* drop the store entry in
/// the same handler. The mirror effect alone can't be trusted here — the send
/// usually changes the card state, which remounts the panel before the effect
/// runs, leaving the sent text to restore into the next step.
pub fn clear<T: 'static>(owner: Uuid, field: &'static str, mut sig: Signal<T>, seed: T) {
    sig.set(seed);
    forget(owner, field);
}

/// Read one field's mirrored draft without subscribing. Debug-only: the
/// keystroke-drop harness compares what the DOM kept against what the Rust
/// side actually received.
#[cfg(debug_assertions)]
pub fn peek(owner: Uuid, field: &'static str) -> Option<String> {
    DRAFTS.peek().get(&DraftKey { owner, field }).cloned()
}

/// Drop every draft belonging to `owner` — called when a card or review is
/// removed, or a card is done.
pub fn forget_owner(owner: Uuid) {
    forget_where(|k| k.owner == owner);
}

/// Drop `owner`'s drafts whose field starts with `prefix` (e.g. the `"pr."`
/// form once the PR exists).
pub fn forget_prefix(owner: Uuid, prefix: &str) {
    forget_where(|k| k.owner == owner && k.field.starts_with(prefix));
}

/// Peek first so the common nothing-to-drop case (every `CardUpdated`) doesn't
/// write the signal.
fn forget_where(doomed: impl Fn(&DraftKey) -> bool) {
    if !DRAFTS.peek().keys().any(&doomed) {
        return;
    }
    forget_in(&mut DRAFTS.write(), doomed);
    bump();
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Bumped on every change to the map or to `reviewdraft`'s buffer; the
/// autosave loop compares it against what it last saved.
static REVISION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bump() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Everything unsent, as the record the store keeps.
pub fn snapshot() -> UiDrafts {
    UiDrafts {
        entries: entries_of(&DRAFTS.peek()),
        review_edits: super::reviewdraft::snapshot(),
    }
}

/// Fill the store from the saved record — once, at startup. Drafts whose card,
/// review or session no longer exists are dropped.
pub fn load(saved: UiDrafts, owner_exists: impl Fn(Uuid) -> bool) {
    *DRAFTS.write() = map_of(saved.entries, &owner_exists);
    super::reviewdraft::load(saved.review_edits.filter(|e| owner_exists(e.review_id)));
}

/// Send the current snapshot to the executor to save. Fire-and-forget: on quit
/// the process may exit before it's written.
pub fn save_now(exec: &ExecutorHandle) {
    exec.send(ExecutorCommand::SaveUiDrafts {
        drafts: Box::new(snapshot()),
    });
}

/// Root-component loop that saves the drafts once they stop changing: each
/// tick saves if something changed since the last save and nothing changed
/// during the tick — or if a save has been pending for a while, so a long
/// stretch of typing still lands.
pub fn use_autosave(state: crate::state::AppState) {
    use_future(move || async move {
        const TICK: Duration = Duration::from_secs(1);
        const MAX_PENDING_TICKS: u32 = 5;
        let mut saved = REVISION.load(Ordering::Relaxed);
        let mut seen = saved;
        let mut pending = 0;
        loop {
            tokio::time::sleep(TICK).await;
            let now = REVISION.load(Ordering::Relaxed);
            if now == saved {
                pending = 0;
            } else {
                pending += 1;
                if now == seen || pending >= MAX_PENDING_TICKS {
                    state.send(ExecutorCommand::SaveUiDrafts {
                        drafts: Box::new(snapshot()),
                    });
                    saved = now;
                    pending = 0;
                }
            }
            seen = now;
        }
    });
}

// ---------------------------------------------------------------------------
// The map logic proper, as free functions so it's testable without a Dioxus
// runtime. The hooks above are thin wrappers.
// ---------------------------------------------------------------------------

fn restore_str(map: &HashMap<DraftKey, String>, key: DraftKey) -> Option<String> {
    map.get(&key).cloned()
}

/// Returns whether the map changed.
fn mirror_str(
    map: &mut HashMap<DraftKey, String>,
    key: DraftKey,
    value: String,
    seed: &str,
) -> bool {
    if value == seed {
        map.remove(&key).is_some()
    } else {
        map.insert(key, value.clone()).as_ref() != Some(&value)
    }
}

/// A typed draft is stored as JSON `(origin_fingerprint, value)`; restoring
/// returns the value only when the stored fingerprint matches the current one.
/// Missing or unparseable entries restore nothing.
fn restore_typed<T: DeserializeOwned>(
    map: &HashMap<DraftKey, String>,
    key: DraftKey,
    origin_fp: &str,
) -> Option<T> {
    let raw = map.get(&key)?;
    let (fp, value): (String, T) = serde_json::from_str(raw).ok()?;
    (fp == origin_fp).then_some(value)
}

fn mirror_typed<T: Serialize + PartialEq>(
    map: &mut HashMap<DraftKey, String>,
    key: DraftKey,
    origin_fp: &str,
    value: &T,
    seed: &T,
) -> bool {
    if value == seed {
        map.remove(&key).is_some()
    } else if let Ok(raw) = serde_json::to_string(&(origin_fp, value)) {
        map.insert(key, raw.clone()).as_ref() != Some(&raw)
    } else {
        false
    }
}

fn forget_in(map: &mut HashMap<DraftKey, String>, doomed: impl Fn(&DraftKey) -> bool) {
    map.retain(|k, _| !doomed(k));
}

/// The map as stored entries, sorted so an unchanged map saves identical bytes.
fn entries_of(map: &HashMap<DraftKey, String>) -> Vec<StoredDraft> {
    let mut entries: Vec<StoredDraft> = map
        .iter()
        .map(|(k, v)| StoredDraft {
            owner: k.owner,
            field: k.field.to_string(),
            value: v.clone(),
        })
        .collect();
    entries.sort_by(|a, b| (a.owner, &a.field).cmp(&(b.owner, &b.field)));
    entries
}

/// Stored entries back into a map, dropping unknown field names and entries
/// whose owner is gone.
fn map_of(
    entries: Vec<StoredDraft>,
    owner_exists: impl Fn(Uuid) -> bool,
) -> HashMap<DraftKey, String> {
    entries
        .into_iter()
        .filter(|e| owner_exists(e.owner))
        .filter_map(|e| {
            let field = FIELDS.iter().copied().find(|f| *f == e.field)?;
            Some((
                DraftKey {
                    owner: e.owner,
                    field,
                },
                e.value,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(owner: Uuid, field: &'static str) -> DraftKey {
        DraftKey { owner, field }
    }

    #[test]
    fn restore_miss_returns_none() {
        let map = HashMap::new();
        assert_eq!(restore_str(&map, key(Uuid::new_v4(), "chat")), None);
    }

    #[test]
    fn mirror_then_restore_round_trips() {
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "chat");
        mirror_str(&mut map, k, "hello".into(), "");
        assert_eq!(restore_str(&map, k), Some("hello".into()));
    }

    #[test]
    fn mirror_back_to_seed_removes_the_entry() {
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "pr.title");
        mirror_str(&mut map, k, "edited".into(), "seeded");
        assert_eq!(map.len(), 1);
        mirror_str(&mut map, k, "seeded".into(), "seeded");
        assert!(map.is_empty());
    }

    #[test]
    fn typed_restore_honors_the_origin() {
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "plan.answers");
        let answers = vec!["a".to_string(), String::new()];
        let seed = vec![String::new(), String::new()];
        mirror_typed(&mut map, k, "plan-v1", &answers, &seed);
        // Same origin: the draft comes back.
        assert_eq!(
            restore_typed::<Vec<String>>(&map, k, "plan-v1"),
            Some(answers)
        );
        // A replan changed the origin: no restore.
        assert_eq!(restore_typed::<Vec<String>>(&map, k, "plan-v2"), None);
    }

    #[test]
    fn typed_mirror_back_to_seed_removes_the_entry() {
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "fixes.verdicts");
        let seed = vec!["as-drafted".to_string()];
        mirror_typed(&mut map, k, "run-1", &vec!["edited".to_string()], &seed);
        assert_eq!(map.len(), 1);
        mirror_typed(&mut map, k, "run-1", &seed.clone(), &seed);
        assert!(map.is_empty());
    }

    #[test]
    fn late_mirror_of_the_seed_after_clear_leaves_nothing() {
        // `clear` drops the entry first; the mirror effect may still run
        // afterwards with the reset value, and must not resurrect anything.
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "chat");
        mirror_str(&mut map, k, "sent feedback".into(), "");
        map.remove(&k);
        mirror_str(&mut map, k, String::new(), "");
        assert_eq!(restore_str(&map, k), None);

        let k = key(Uuid::new_v4(), "plan.answers");
        let seed = vec![String::new(); 2];
        mirror_typed(
            &mut map,
            k,
            "plan-v1",
            &vec!["a".to_string(), String::new()],
            &seed,
        );
        map.remove(&k);
        mirror_typed(&mut map, k, "plan-v1", &seed.clone(), &seed);
        assert!(map.is_empty());
    }

    #[test]
    fn mirror_reports_whether_the_map_changed() {
        let mut map = HashMap::new();
        let k = key(Uuid::new_v4(), "chat");
        assert!(mirror_str(&mut map, k, "a".into(), ""));
        // A remount re-mirrors the same value: not a change worth saving.
        assert!(!mirror_str(&mut map, k, "a".into(), ""));
        assert!(mirror_str(&mut map, k, String::new(), ""));
        assert!(!mirror_str(&mut map, k, String::new(), ""));
    }

    #[test]
    fn stored_entries_round_trip_every_field() {
        let mut map = HashMap::new();
        let owner = Uuid::new_v4();
        for (i, f) in FIELDS.iter().enumerate() {
            map.insert(key(owner, f), format!("v{i}"));
        }
        let back = map_of(entries_of(&map), |_| true);
        assert_eq!(back, map);
    }

    #[test]
    fn loading_drops_unknown_fields_and_gone_owners() {
        let alive = Uuid::new_v4();
        let gone = Uuid::new_v4();
        let entry = |owner, field: &str| StoredDraft {
            owner,
            field: field.into(),
            value: "x".into(),
        };
        let map = map_of(
            vec![
                entry(alive, "chat"),
                entry(alive, "renamed.field"),
                entry(gone, "chat"),
            ],
            |id| id == alive,
        );
        assert_eq!(map.len(), 1);
        assert_eq!(restore_str(&map, key(alive, "chat")), Some("x".into()));
    }

    #[test]
    fn forget_owner_clears_only_that_owner() {
        let mut map = HashMap::new();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        mirror_str(&mut map, key(mine, "chat"), "a".into(), "");
        mirror_str(&mut map, key(mine, "pr.body"), "b".into(), "");
        mirror_str(&mut map, key(theirs, "chat"), "c".into(), "");
        forget_in(&mut map, |k| k.owner == mine);
        assert_eq!(map.len(), 1);
        assert_eq!(restore_str(&map, key(theirs, "chat")), Some("c".into()));
    }

    #[test]
    fn forget_by_prefix_keeps_the_owners_other_fields() {
        let mut map = HashMap::new();
        let mine = Uuid::new_v4();
        mirror_str(&mut map, key(mine, "pr.title"), "t".into(), "");
        mirror_str(&mut map, key(mine, "pr.branch"), "b".into(), "");
        mirror_str(&mut map, key(mine, "chat"), "c".into(), "");
        forget_in(&mut map, |k| k.owner == mine && k.field.starts_with("pr."));
        assert_eq!(map.len(), 1);
        assert_eq!(restore_str(&map, key(mine, "chat")), Some("c".into()));
    }
}

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
//!   untouched or just-sent field never shadows fresher seeds on remount.
//! - **Origin rule** (`use_draft_of`) — a working copy of agent output is
//!   stored alongside a fingerprint of the payload it edits; a new agent run
//!   (new verdicts, a replanned plan, a different question) reseeds instead of
//!   restoring edits that no longer apply. Same idea as `reviewdraft`'s
//!   `origin` field.
//!
//! Entries die with their owner: `forget_owner` runs on `CardRemoved` and
//! `ProjectRemoved`. In-memory only — drafts don't survive a restart.

use std::collections::HashMap;

use dioxus::prelude::*;
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;

/// One draft slot: the card or review it belongs to, plus a per-panel field
/// name (e.g. `"chat"`, `"pr.body"`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
    let key = DraftKey { owner, field };
    let seeded = seed();
    let init = seeded.clone();
    // `peek`, not `read`: subscribing the panel to the whole map would
    // re-render every draft-bearing component on any keystroke anywhere.
    let sig = use_signal(|| restore_str(&DRAFTS.peek(), key).unwrap_or(init));
    use_effect(move || {
        let v = sig.read().clone();
        mirror_str(&mut DRAFTS.write(), key, v, &seeded);
    });
    sig
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
    let key = DraftKey { owner, field };
    let fp = serde_json::to_string(origin).unwrap_or_default();
    let seeded = seed();
    let init = seeded.clone();
    let init_fp = fp.clone();
    let sig = use_signal(|| restore_typed::<T>(&DRAFTS.peek(), key, &init_fp).unwrap_or(init));
    use_effect(move || {
        let v = sig.read().clone();
        mirror_typed(&mut DRAFTS.write(), key, &fp, &v, &seeded);
    });
    sig
}

/// Drop one field's draft — for sends that don't clear the signal back to its
/// seed (the confirm dialog fires the command, or the seed is nonempty).
pub fn forget(owner: Uuid, field: &'static str) {
    DRAFTS.write().remove(&DraftKey { owner, field });
}

/// Drop every draft belonging to `owner` — called when a card or review is
/// removed.
pub fn forget_owner(owner: Uuid) {
    forget_owner_in(&mut DRAFTS.write(), owner);
}

// ---------------------------------------------------------------------------
// The map logic proper, as free functions so it's testable without a Dioxus
// runtime. The hooks above are thin wrappers.
// ---------------------------------------------------------------------------

fn restore_str(map: &HashMap<DraftKey, String>, key: DraftKey) -> Option<String> {
    map.get(&key).cloned()
}

fn mirror_str(map: &mut HashMap<DraftKey, String>, key: DraftKey, value: String, seed: &str) {
    if value == seed {
        map.remove(&key);
    } else {
        map.insert(key, value);
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
) {
    if value == seed {
        map.remove(&key);
    } else if let Ok(raw) = serde_json::to_string(&(origin_fp, value)) {
        map.insert(key, raw);
    }
}

fn forget_owner_in(map: &mut HashMap<DraftKey, String>, owner: Uuid) {
    map.retain(|k, _| k.owner != owner);
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
    fn forget_owner_clears_only_that_owner() {
        let mut map = HashMap::new();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        mirror_str(&mut map, key(mine, "chat"), "a".into(), "");
        mirror_str(&mut map, key(mine, "pr.body"), "b".into(), "");
        mirror_str(&mut map, key(theirs, "chat"), "c".into(), "");
        forget_owner_in(&mut map, mine);
        assert_eq!(map.len(), 1);
        assert_eq!(restore_str(&map, key(theirs, "chat")), Some("c".into()));
    }
}

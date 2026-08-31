//! Typed persistence via `native_db` (on the `redb` engine).
//!
//! There is **no SQL and no manual column mapping**: each aggregate is stored
//! as a typed record whose body is the domain struct itself, (de)serialized by
//! `native_model`. The database is ACID and single-file. The `Store` facade is
//! the only thing the rest of the crate sees, so the storage engine is an
//! implementation detail.
//!
//! Access is document-by-UUID (plus "list all"), matching the board's needs;
//! at this app's scale a full scan + in-Rust filter for the per-project view is
//! both simplest and plenty fast.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

use native_db::*;
use native_model::{native_model, Model};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::handoff::Handoff;
use crate::domain::config::AppSettings;
use crate::domain::model::{Card, CardAnswers, Project, QaExchange, ReviewComment, ReviewTask};
use crate::error::{CoreError, Result};

// ---------------------------------------------------------------------------
// Codec — every record body is (de)serialized as JSON.
//
// native_model's default codec is bincode: positional and *non-self-describing*
// (fields are read by byte offset, in declaration order, with no field names).
// That makes any change to a stored struct — adding a field, even one marked
// `#[serde(default)]`, or reordering an enum variant — silently corrupt every
// previously-written record: on the next load they fail to decode and are
// skipped, so the data appears to vanish. JSON stores field *names*, so
// `#[serde(default)]` genuinely keeps older records loadable, unknown fields are
// ignored, and enum variants are matched by name rather than index. The cost
// (larger rows, slower parse) is irrelevant at this app's scale.
// ---------------------------------------------------------------------------

/// Self-describing JSON codec for native_model record bodies.
struct Json;

impl<T: Serialize> native_model::Encode<T> for Json {
    type Error = serde_json::Error;
    fn encode(obj: &T) -> std::result::Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(obj)
    }
}

impl<T: for<'a> Deserialize<'a>> native_model::Decode<T> for Json {
    type Error = serde_json::Error;
    fn decode(data: Vec<u8>) -> std::result::Result<T, serde_json::Error> {
        serde_json::from_slice(&data)
    }
}

// ---------------------------------------------------------------------------
// Records — thin typed wrappers that lift the key field(s) for indexing and
// embed the domain struct as the body. Keeps the domain model storage-agnostic.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 1, version = 2, with = Json)]
#[native_db]
struct ProjectRecord {
    #[primary_key]
    id: String,
    project: Project,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 2, version = 2, with = Json)]
#[native_db]
struct CardRecord {
    #[primary_key]
    id: String,
    project_id: String,
    card: Card,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 3, version = 1, with = Json)]
#[native_db]
struct SettingsRecord {
    #[primary_key]
    id: u8,
    settings: AppSettings,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 4, version = 1, with = Json)]
#[native_db]
struct TranscriptRecord {
    #[primary_key]
    id: String,
    card_id: String,
    ts: i64,
    /// Process-monotonic insertion order, used as a tiebreaker so two lines that
    /// land in the same millisecond keep their append order instead of coming
    /// back in random (primary-key) order. `#[serde(default)]` so rows written
    /// before this field existed still deserialize.
    #[serde(default)]
    seq: i64,
    line: String,
}

/// Monotonic counter handing out [`TranscriptRecord::seq`] values. Process-global
/// (one database per process); seeded past any persisted value on `open` so it
/// stays monotonic across restarts.
static TRANSCRIPT_SEQ: AtomicI64 = AtomicI64::new(0);

/// The approved plan for a card, kept separately so resuming an implement run can
/// re-inject it (and so adding it doesn't change the `Card` record layout).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 5, version = 1, with = Json)]
#[native_db]
struct CardPlanRecord {
    #[primary_key]
    card_id: String,
    plan: String,
}

/// Per-card options that are set before a card starts. Kept in its own record
/// (not on `Card`) so adding it doesn't change the `Card` record layout, which
/// would break reading existing cards.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 6, version = 1, with = Json)]
#[native_db]
struct CardOptionsRecord {
    #[primary_key]
    card_id: String,
    /// Skip the design/plan phase and implement straight from the description.
    skip_plan: bool,
    /// Opt OUT of the automatic self-review pass that follows a finished
    /// implementation. Stored negatively so the default (auto-review ON) is the
    /// serde default, which also keeps records written before this field loadable.
    #[serde(default)]
    skip_auto_review: bool,
}

/// The managed copies of a card's attached images, kept in their own record so
/// adding it doesn't change the `Card` record layout. Paths point into the data
/// dir (see [`crate::infra::paths::attachments_dir`]), never into a project repo.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 7, version = 1, with = Json)]
#[native_db]
struct CardAttachmentsRecord {
    #[primary_key]
    card_id: String,
    paths: Vec<String>,
}

/// Per-card review state kept in its own record (not on `Card`, to avoid
/// changing its layout): the fixes recap shown on the merge step, the PR
/// comments awaiting agent triage (serialized JSON, transient between fetch and
/// the triage run's completion), and the implement run's hand-off to its reviewer.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[native_model(id = 8, version = 1, with = Json)]
#[native_db]
struct CardReviewRecord {
    #[primary_key]
    card_id: String,
    recap: String,
    pending_comments: String,
    /// Review-comment ids the last fix run addressed, stashed between applying
    /// the fixes and the run's completion so their GitHub threads can be resolved
    /// once the fix lands. `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pending_resolve: Vec<u64>,
    /// The implement run's hand-off (a serialized [`Handoff`]), shown on the
    /// awaiting-review step. Empty when the run emitted none. `#[serde(default)]`
    /// keeps older records loadable.
    #[serde(default)]
    handoff: String,
    /// Restart-log lines describing the fixes a launched fix run set out to
    /// apply, stashed until the run lands a commit — recording them up front
    /// would leave a durable "fix applied" claim behind a run that was
    /// cancelled or died. `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pending_qa: Vec<String>,
}

/// The extra context of the current investigation round: the follow-up prompt
/// built from the prior conclusion and the earlier rounds. Kept so a retry of a
/// faulted round re-launches with the same context instead of silently
/// re-answering only the original description. Absent for the initial round.
/// Its own record so adding it doesn't change the `Card` record layout.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 11, version = 1, with = Json)]
#[native_db]
struct CardInvestigationRecord {
    #[primary_key]
    card_id: String,
    extra: String,
}

/// The task-specific extra of the last launched fix run (the conflict prompt,
/// the picked review comments, a requested change, the failing-checks logs).
/// Kept so a retry of a faulted fix run can restate the task: `relaunch`
/// rebuilds the prompt from scratch, and without this the resumed agent finds
/// finished work, changes nothing, and the no-commit guard fails the run again
/// — an unwinnable retry loop. Its own record so adding it doesn't change the
/// `Card` record layout.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 13, version = 1, with = Json)]
#[native_db]
struct CardFixExtraRecord {
    #[primary_key]
    card_id: String,
    extra: String,
}

/// A pull request the user is reviewing (someone else's PR). Its own top-level
/// record — distinct from `CardRecord` — because a foreign PR has no owned
/// branch/worktree/card. `project_id` is lifted for the per-project filter.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[native_model(id = 9, version = 1, with = Json)]
#[native_db]
struct ReviewTaskRecord {
    #[primary_key]
    id: String,
    project_id: String,
    task: ReviewTask,
}

/// PR numbers the user has dismissed from a project's review board, kept so the
/// poll never re-adds them (dismissals are permanent). Keyed by project.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[native_model(id = 10, version = 1, with = Json)]
#[native_db]
struct DismissedReviewsRecord {
    #[primary_key]
    project_id: String,
    pr_numbers: Vec<u64>,
}

/// A card's Agent Chat log: `question` stashes the pending question of an
/// in-flight run, and `history` collects every exchange once answered. Its own
/// record (not a field on [`CardReviewRecord`]) so adding it doesn't change an
/// existing record's layout. `history`/`superseded` are additive
/// `#[serde(default)]` fields — no version bump, the startup
/// `canonicalize_records()` refresh rewrites old rows; `answer` is legacy, a
/// pre-history row's single answer folded into `history` on read.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[native_model(id = 12, version = 1, with = Json)]
#[native_db]
struct CardAnswerRecord {
    #[primary_key]
    card_id: String,
    #[serde(default)]
    question: String,
    answer: String,
    #[serde(default)]
    history: Vec<QaExchange>,
    #[serde(default)]
    superseded: bool,
}

impl CardAnswerRecord {
    /// The record's log, folding a legacy pre-`history` row's lone answer into
    /// a single exchange so an existing database keeps it.
    fn answers(self) -> CardAnswers {
        let exchanges = if self.history.is_empty() && !self.answer.is_empty() {
            vec![QaExchange {
                question: self.question,
                answer: self.answer,
                asked_at: 0,
            }]
        } else {
            self.history
        };
        CardAnswers {
            exchanges,
            superseded: self.superseded,
        }
    }
}

const SETTINGS_ID: u8 = 1;

/// Model registry. Must be `'static` so the [`Database`] can be `'static` and
/// thus stored in a `Clone + Send + Sync` handle.
///
/// `ProjectRecord`/`CardRecord` are declared at `version = 2` (rather than 1) so
/// they keep addressing the tables an existing dev database already wrote. The
/// pre-release v1→v2 migration code was dropped: this app has never shipped, so
/// there is nothing in the wild still on the v1 layout.
static MODELS: LazyLock<Models> = LazyLock::new(|| {
    let mut models = Models::new();
    models
        .define::<ProjectRecord>()
        .expect("define ProjectRecord");
    models.define::<CardRecord>().expect("define CardRecord");
    models
        .define::<SettingsRecord>()
        .expect("define SettingsRecord");
    models
        .define::<TranscriptRecord>()
        .expect("define TranscriptRecord");
    models
        .define::<CardPlanRecord>()
        .expect("define CardPlanRecord");
    models
        .define::<CardOptionsRecord>()
        .expect("define CardOptionsRecord");
    models
        .define::<CardAttachmentsRecord>()
        .expect("define CardAttachmentsRecord");
    models
        .define::<CardReviewRecord>()
        .expect("define CardReviewRecord");
    models
        .define::<CardInvestigationRecord>()
        .expect("define CardInvestigationRecord");
    models
        .define::<CardFixExtraRecord>()
        .expect("define CardFixExtraRecord");
    models
        .define::<ReviewTaskRecord>()
        .expect("define ReviewTaskRecord");
    models
        .define::<DismissedReviewsRecord>()
        .expect("define DismissedReviewsRecord");
    models
        .define::<CardAnswerRecord>()
        .expect("define CardAnswerRecord");
    models
});

#[derive(Clone)]
pub struct Store {
    db: Arc<Database<'static>>,
}

impl Store {
    /// Open (creating if needed) a database file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Builder::new().create(&MODELS, path)?;
        let store = Store { db: Arc::new(db) };
        store.seed_transcript_seq();
        Ok(store)
    }

    /// In-memory database (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let db = Builder::new().create_in_memory(&MODELS)?;
        Ok(Store { db: Arc::new(db) })
    }

    /// Advance the global transcript sequence past any value already on disk so
    /// new lines always sort after restored ones.
    fn seed_transcript_seq(&self) {
        let mut max = 0i64;
        if let Ok(r) = self.db.r_transaction() {
            if let Ok(scan) = r.scan().primary::<TranscriptRecord>() {
                if let Ok(all) = scan.all() {
                    for rec in all.flatten() {
                        max = max.max(rec.seq);
                    }
                }
            }
        }
        TRANSCRIPT_SEQ.fetch_max(max + 1, Ordering::Relaxed);
    }

    /// Rewrite every in-place-mutated record in canonical (decode→encode) form.
    ///
    /// native_db's `update`/`remove` re-encode the value and compare it
    /// byte-for-byte against the stored bytes; a record written before a newer
    /// `#[serde(default)]` field existed — or with a float that no longer
    /// round-trips — isn't a fixed point, so the next edit or delete fails with
    /// `IncorrectInputData`. Refreshing at startup makes each record a fixed point
    /// so later writes always land. Append-only transcripts are excluded: they're
    /// high-volume and never updated in place.
    pub fn canonicalize_records(&self) {
        self.refresh_type::<SettingsRecord>();
        self.refresh_type::<ProjectRecord>();
        self.refresh_type::<CardRecord>();
        self.refresh_type::<ReviewTaskRecord>();
        self.refresh_type::<CardPlanRecord>();
        self.refresh_type::<CardOptionsRecord>();
        self.refresh_type::<CardAttachmentsRecord>();
        self.refresh_type::<CardReviewRecord>();
        self.refresh_type::<DismissedReviewsRecord>();
        self.refresh_type::<CardAnswerRecord>();
    }

    /// Refresh one record type in its own transaction, best-effort: an undecodable
    /// record (or a commit failure) leaves that type untouched rather than
    /// blocking the rest.
    fn refresh_type<T: ToInput + std::fmt::Debug>(&self) {
        let Ok(rw) = self.db.rw_transaction() else {
            return;
        };
        if rw.refresh::<T>().is_ok() {
            let _ = rw.commit();
        }
    }

    // --- settings -------------------------------------------------------

    /// Whether a settings record has ever been written — i.e. this is NOT a
    /// first startup. [`Self::settings`] hides absence behind `Default`, so
    /// first-run detection needs this explicit probe.
    pub fn has_settings(&self) -> Result<bool> {
        let r = self.db.r_transaction()?;
        let rec: Option<SettingsRecord> = r.get().primary(SETTINGS_ID)?;
        Ok(rec.is_some())
    }

    pub fn settings(&self) -> Result<AppSettings> {
        let r = self.db.r_transaction()?;
        let rec: Option<SettingsRecord> = r.get().primary(SETTINGS_ID)?;
        Ok(rec.map(|r| r.settings).unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        let rec = SettingsRecord {
            id: SETTINGS_ID,
            settings: settings.clone(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<SettingsRecord> = rw.get().primary(SETTINGS_ID)?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    // --- projects -------------------------------------------------------

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let r = self.db.r_transaction()?;
        let mut out = Vec::new();
        for rec in r.scan().primary::<ProjectRecord>()?.all()? {
            out.push(rec?.project);
        }
        out.sort_by_key(|p| p.name.to_lowercase());
        Ok(out)
    }

    pub fn get_project(&self, id: Uuid) -> Result<Project> {
        let r = self.db.r_transaction()?;
        let rec: Option<ProjectRecord> = r.get().primary(id.to_string())?;
        rec.map(|r| r.project)
            .ok_or_else(|| CoreError::NotFound(format!("project {id}")))
    }

    pub fn upsert_project(&self, project: &Project) -> Result<()> {
        let rec = ProjectRecord {
            id: project.id.to_string(),
            project: project.clone(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<ProjectRecord> = rw.get().primary(rec.id.clone())?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    pub fn delete_project(&self, id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        // Remove the project.
        if let Some(rec) = rw.get().primary::<ProjectRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        // Cascade to its cards.
        let cards: Vec<CardRecord> = rw
            .scan()
            .primary::<CardRecord>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for card in cards.into_iter().filter(|c| c.card.project_id == id) {
            rw.remove(card)?;
        }
        // Cascade to its review tasks.
        let reviews: Vec<ReviewTaskRecord> = rw
            .scan()
            .primary::<ReviewTaskRecord>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for review in reviews.into_iter().filter(|r| r.task.project_id == id) {
            rw.remove(review)?;
        }
        // Cascade to its dismissed-review record.
        if let Some(rec) = rw.get().primary::<DismissedReviewsRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        rw.commit()?;
        Ok(())
    }

    // --- cards ----------------------------------------------------------

    pub fn list_cards(&self) -> Result<Vec<Card>> {
        let r = self.db.r_transaction()?;
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for rec in r.scan().primary::<CardRecord>()?.all()? {
            match rec {
                Ok(r) => out.push(r.card),
                // Never let a single undecodable record (e.g. a schema drift left
                // by an intermediate dev build) blank the entire board — skip it
                // and load the rest.
                Err(e) => {
                    skipped += 1;
                    tracing::warn!("skipping undecodable card record: {e}");
                }
            }
        }
        if skipped > 0 {
            tracing::warn!("{skipped} card record(s) could not be decoded and were skipped");
        }
        Ok(out)
    }

    pub fn list_cards_for_project(&self, project_id: Uuid) -> Result<Vec<Card>> {
        Ok(self
            .list_cards()?
            .into_iter()
            .filter(|c| c.project_id == project_id)
            .collect())
    }

    pub fn get_card(&self, id: Uuid) -> Result<Card> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardRecord> = r.get().primary(id.to_string())?;
        rec.map(|r| r.card)
            .ok_or_else(|| CoreError::NotFound(format!("card {id}")))
    }

    pub fn upsert_card(&self, card: &Card) -> Result<()> {
        let rec = CardRecord {
            id: card.id.to_string(),
            project_id: card.project_id.to_string(),
            card: card.clone(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<CardRecord> = rw.get().primary(rec.id.clone())?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    /// Atomically read-modify-write a card inside a single transaction. Use this
    /// instead of `get_card` + mutate + `upsert_card` whenever a card may be
    /// touched concurrently (the executor's run actors and dispatch tasks both
    /// mutate cards): the separate-transaction pattern is a lost-update race,
    /// while this serializes through one rw-transaction. Returns the new card.
    pub fn mutate_card<F>(&self, id: Uuid, f: F) -> Result<Card>
    where
        F: FnOnce(&mut Card) -> Result<()>,
    {
        let rw = self.db.rw_transaction()?;
        let old: CardRecord = rw
            .get()
            .primary(id.to_string())?
            .ok_or_else(|| CoreError::NotFound(format!("card {id}")))?;
        let mut card = old.card.clone();
        f(&mut card)?;
        let rec = CardRecord {
            id: card.id.to_string(),
            project_id: card.project_id.to_string(),
            card: card.clone(),
        };
        rw.update(old, rec)?;
        rw.commit()?;
        Ok(card)
    }

    pub fn delete_card(&self, id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        if let Some(rec) = rw.get().primary::<CardRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        let lines: Vec<TranscriptRecord> = rw
            .scan()
            .primary::<TranscriptRecord>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for line in lines.into_iter().filter(|t| t.card_id == id.to_string()) {
            rw.remove(line)?;
        }
        if let Some(rec) = rw.get().primary::<CardPlanRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        if let Some(rec) = rw.get().primary::<CardOptionsRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        if let Some(rec) = rw.get().primary::<CardAttachmentsRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        if let Some(rec) = rw.get().primary::<CardReviewRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        if let Some(rec) = rw.get().primary::<CardAnswerRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        if let Some(rec) = rw
            .get()
            .primary::<CardInvestigationRecord>(id.to_string())?
        {
            rw.remove(rec)?;
        }
        rw.commit()?;
        Ok(())
    }

    // --- plans ----------------------------------------------------------

    pub fn save_plan(&self, card_id: Uuid, plan: &str) -> Result<()> {
        let rec = CardPlanRecord {
            card_id: card_id.to_string(),
            plan: plan.to_string(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<CardPlanRecord> = rw.get().primary(rec.card_id.clone())?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    pub fn get_plan(&self, card_id: Uuid) -> Result<Option<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardPlanRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.plan))
    }

    /// Drop the card's saved plan. Used by "back to the starting block", where the
    /// plan is a run artifact of the attempt being discarded — leaving it behind
    /// would feed a stale plan to the next run's prompt. Idempotent.
    pub fn delete_plan(&self, card_id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        if let Some(rec) = rw.get().primary::<CardPlanRecord>(card_id.to_string())? {
            rw.remove(rec)?;
        }
        rw.commit()?;
        Ok(())
    }

    // --- Agent Chat answers ---------------------------------------------

    /// Stash the question a starting Agent Chat run will answer. `set_answer`
    /// completes it when the run finishes; the earlier exchanges are kept.
    pub fn set_question(&self, card_id: Uuid, question: &str) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardAnswerRecord> = rw.get().primary(card_id.to_string())?;
        let rec = CardAnswerRecord {
            card_id: card_id.to_string(),
            question: question.to_string(),
            ..old.clone().unwrap_or_default()
        };
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    /// The stashed question of the in-flight Agent Chat run.
    pub fn get_question(&self, card_id: Uuid) -> Result<Option<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardAnswerRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.question).filter(|s: &String| !s.is_empty()))
    }

    /// Append an answered exchange to the card's log, consuming the stashed
    /// question and clearing `superseded` (this answer describes current work).
    /// Unbounded: the log is only dropped by "back to start" or card removal.
    pub fn set_answer(&self, card_id: Uuid, answer: &str) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardAnswerRecord> = rw.get().primary(card_id.to_string())?;
        let mut history = old
            .as_ref()
            .map(|o| o.clone().answers().exchanges)
            .unwrap_or_default();
        history.push(QaExchange {
            question: old.as_ref().map(|o| o.question.clone()).unwrap_or_default(),
            answer: answer.to_string(),
            asked_at: crate::now_millis(),
        });
        let rec = CardAnswerRecord {
            card_id: card_id.to_string(),
            question: String::new(),
            answer: String::new(),
            history,
            superseded: false,
        };
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    /// The card's most recent answer, if it has ever answered a question.
    pub fn get_answer(&self, card_id: Uuid) -> Result<Option<String>> {
        Ok(self
            .get_answers(card_id)?
            .exchanges
            .pop()
            .map(|e| e.answer)
            .filter(|s: &String| !s.is_empty()))
    }

    /// A card's whole Agent Chat log (empty when it has never answered).
    pub fn get_answers(&self, card_id: Uuid) -> Result<CardAnswers> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardAnswerRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(CardAnswerRecord::answers).unwrap_or_default())
    }

    /// Mark the log as superseded by a write run: the exchanges stay, but the
    /// panel stops showing any of them expanded. Idempotent.
    pub fn supersede_answers(&self, card_id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        if let Some(old) = rw.get().primary::<CardAnswerRecord>(card_id.to_string())? {
            let rec = CardAnswerRecord {
                superseded: true,
                ..old.clone()
            };
            rw.update(old, rec)?;
        }
        rw.commit()?;
        Ok(())
    }

    /// Drop a card's whole Agent Chat log (used by "back to start"). Idempotent.
    pub fn delete_answer(&self, card_id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        if let Some(rec) = rw.get().primary::<CardAnswerRecord>(card_id.to_string())? {
            rw.remove(rec)?;
        }
        rw.commit()?;
        Ok(())
    }

    /// Every card's Agent Chat log, keyed by card id (loaded once at startup).
    /// Cards with nothing answered yet are skipped.
    pub fn all_answers(&self) -> Result<HashMap<Uuid, CardAnswers>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardAnswerRecord>()?.all()? {
            let rec = rec?;
            let Ok(id) = Uuid::parse_str(&rec.card_id) else {
                continue;
            };
            let answers = rec.answers();
            if answers.exchanges.is_empty() {
                continue;
            }
            out.insert(id, answers);
        }
        Ok(out)
    }

    // --- investigation context ------------------------------------------

    /// Stash (or, with `None`, clear) the current investigation round's extra
    /// context so a retry re-launches the same round (see the record's doc).
    pub fn set_investigation_extra(&self, card_id: Uuid, extra: Option<&str>) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardInvestigationRecord> = rw.get().primary(card_id.to_string())?;
        match (old, extra) {
            (old, Some(extra)) => {
                let rec = CardInvestigationRecord {
                    card_id: card_id.to_string(),
                    extra: extra.to_string(),
                };
                match old {
                    Some(old) => rw.update(old, rec)?,
                    None => rw.insert(rec)?,
                }
            }
            (Some(old), None) => rw.remove(old).map(|_| ())?,
            (None, None) => {}
        }
        rw.commit()?;
        Ok(())
    }

    pub fn get_investigation_extra(&self, card_id: Uuid) -> Result<Option<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardInvestigationRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.extra))
    }

    // --- fix-run context ------------------------------------------------

    /// Stash (or, with `None`, clear) the task-specific extra of a fix run so a
    /// retry re-launches with the same task (see the record's doc).
    pub fn set_fix_extra(&self, card_id: Uuid, extra: Option<&str>) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardFixExtraRecord> = rw.get().primary(card_id.to_string())?;
        match (old, extra) {
            (old, Some(extra)) => {
                let rec = CardFixExtraRecord {
                    card_id: card_id.to_string(),
                    extra: extra.to_string(),
                };
                match old {
                    Some(old) => rw.update(old, rec)?,
                    None => rw.insert(rec)?,
                }
            }
            (Some(old), None) => rw.remove(old).map(|_| ())?,
            (None, None) => {}
        }
        rw.commit()?;
        Ok(())
    }

    pub fn get_fix_extra(&self, card_id: Uuid) -> Result<Option<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardFixExtraRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.extra))
    }

    // --- per-card options ----------------------------------------------

    /// Whether the card should skip planning and implement directly. Defaults to
    /// `false` for cards that have never had an option set.
    pub fn get_skip_plan(&self, card_id: Uuid) -> Result<bool> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardOptionsRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.skip_plan).unwrap_or(false))
    }

    /// All stored skip-plan flags, keyed by card id. The UI loads this once at
    /// startup into a signal so it never reads the store on its own thread.
    pub fn skip_plan_flags(&self) -> Result<HashMap<Uuid, bool>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardOptionsRecord>()?.all()? {
            let rec = rec?;
            if let Ok(id) = Uuid::parse_str(&rec.card_id) {
                out.insert(id, rec.skip_plan);
            }
        }
        Ok(out)
    }

    pub fn set_skip_plan(&self, card_id: Uuid, skip_plan: bool) -> Result<()> {
        self.mutate_options(card_id, |o| o.skip_plan = skip_plan)
    }

    /// Whether the card auto-starts its self-review pass when the implementation
    /// finishes. On by default; the toggle stores the opt-out.
    pub fn get_auto_review(&self, card_id: Uuid) -> Result<bool> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardOptionsRecord> = r.get().primary(card_id.to_string())?;
        Ok(!rec.map(|r| r.skip_auto_review).unwrap_or(false))
    }

    /// All stored auto-review flags (true = auto-review on), keyed by card id.
    /// Loaded once at startup into a UI signal, like [`Self::skip_plan_flags`].
    pub fn auto_review_flags(&self) -> Result<HashMap<Uuid, bool>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardOptionsRecord>()?.all()? {
            let rec = rec?;
            if let Ok(id) = Uuid::parse_str(&rec.card_id) {
                out.insert(id, !rec.skip_auto_review);
            }
        }
        Ok(out)
    }

    pub fn set_auto_review(&self, card_id: Uuid, auto: bool) -> Result<()> {
        self.mutate_options(card_id, |o| o.skip_auto_review = !auto)
    }

    /// Read-modify-write the card's options record so setting one option can't
    /// reset the others to their defaults.
    fn mutate_options(&self, card_id: Uuid, f: impl FnOnce(&mut CardOptionsRecord)) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardOptionsRecord> = rw.get().primary(card_id.to_string())?;
        let mut rec = old.clone().unwrap_or(CardOptionsRecord {
            card_id: card_id.to_string(),
            skip_plan: false,
            skip_auto_review: false,
        });
        f(&mut rec);
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    // --- attachments ----------------------------------------------------

    pub fn get_attachments(&self, card_id: Uuid) -> Result<Vec<PathBuf>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardAttachmentsRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec
            .map(|r| r.paths.into_iter().map(PathBuf::from).collect())
            .unwrap_or_default())
    }

    pub fn set_attachments(&self, card_id: Uuid, paths: &[PathBuf]) -> Result<()> {
        let rec = CardAttachmentsRecord {
            card_id: card_id.to_string(),
            paths: paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<CardAttachmentsRecord> = rw.get().primary(rec.card_id.clone())?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    /// All cards' attachments, keyed by card id. Loaded once into a signal at
    /// startup so the UI never reads the store on its own thread.
    pub fn all_attachments(&self) -> Result<HashMap<Uuid, Vec<PathBuf>>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardAttachmentsRecord>()?.all()? {
            let rec = rec?;
            if let Ok(id) = Uuid::parse_str(&rec.card_id) {
                out.insert(id, rec.paths.into_iter().map(PathBuf::from).collect());
            }
        }
        Ok(out)
    }

    // --- PR review (recap + pending triage comments) --------------------

    /// Read-modify-write the card's review record, preserving the untouched field.
    fn mutate_review<F: FnOnce(&mut CardReviewRecord)>(&self, card_id: Uuid, f: F) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<CardReviewRecord> = rw.get().primary(card_id.to_string())?;
        let mut rec = old.clone().unwrap_or_else(|| CardReviewRecord {
            card_id: card_id.to_string(),
            ..Default::default()
        });
        f(&mut rec);
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    pub fn set_review_recap(&self, card_id: Uuid, recap: &str) -> Result<()> {
        self.mutate_review(card_id, |r| r.recap = recap.to_string())
    }

    pub fn get_review_recap(&self, card_id: Uuid) -> Result<Option<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardReviewRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.map(|r| r.recap).filter(|s: &String| !s.is_empty()))
    }

    /// All cards' fixes recaps, keyed by card id (loaded once at startup).
    pub fn all_review_recaps(&self) -> Result<HashMap<Uuid, String>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardReviewRecord>()?.all()? {
            let rec = rec?;
            if rec.recap.is_empty() {
                continue;
            }
            if let Ok(id) = Uuid::parse_str(&rec.card_id) {
                out.insert(id, rec.recap);
            }
        }
        Ok(out)
    }

    // --- implementation hand-off ----------------------------------------

    /// Store the implement run's hand-off, or clear it (an empty [`Handoff`]) so a
    /// re-implemented card never shows the previous attempt's recap.
    pub fn set_handoff(&self, card_id: Uuid, handoff: &Handoff) -> Result<()> {
        let json = if handoff.is_empty() {
            String::new()
        } else {
            serde_json::to_string(handoff).unwrap_or_default()
        };
        self.mutate_review(card_id, |r| r.handoff = json)
    }

    pub fn get_handoff(&self, card_id: Uuid) -> Result<Option<Handoff>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardReviewRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec.and_then(|r| serde_json::from_str::<Handoff>(&r.handoff).ok()))
    }

    /// All cards' hand-offs, keyed by card id (loaded once at startup).
    pub fn all_handoffs(&self) -> Result<HashMap<Uuid, Handoff>> {
        let r = self.db.r_transaction()?;
        let mut out = HashMap::new();
        for rec in r.scan().primary::<CardReviewRecord>()?.all()? {
            let rec = rec?;
            let Ok(id) = Uuid::parse_str(&rec.card_id) else {
                continue;
            };
            if let Ok(h) = serde_json::from_str::<Handoff>(&rec.handoff) {
                out.insert(id, h);
            }
        }
        Ok(out)
    }

    /// Stash the PR comments awaiting triage (so the triage run's completion can
    /// join the agent's verdicts back to them by id).
    pub fn set_pending_comments(&self, card_id: Uuid, comments: &[ReviewComment]) -> Result<()> {
        let json = serde_json::to_string(comments).unwrap_or_else(|_| "[]".into());
        self.mutate_review(card_id, |r| r.pending_comments = json)
    }

    pub fn get_pending_comments(&self, card_id: Uuid) -> Result<Vec<ReviewComment>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardReviewRecord> = r.get().primary(card_id.to_string())?;
        Ok(rec
            .and_then(|r| serde_json::from_str(&r.pending_comments).ok())
            .unwrap_or_default())
    }

    /// Stash the review-comment ids a fix run is addressing, so their GitHub
    /// threads can be resolved once the run lands (see [`Self::take_pending_resolve`]).
    pub fn set_pending_resolve(&self, card_id: Uuid, ids: &[u64]) -> Result<()> {
        self.mutate_review(card_id, |r| r.pending_resolve = ids.to_vec())
    }

    /// Read and clear the stashed comment ids to resolve. Clearing here means a
    /// resolve is attempted exactly once — a retry of the fix restashes its own set.
    pub fn take_pending_resolve(&self, card_id: Uuid) -> Result<Vec<u64>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardReviewRecord> = r.get().primary(card_id.to_string())?;
        let ids = rec.map(|r| r.pending_resolve).unwrap_or_default();
        if !ids.is_empty() {
            self.mutate_review(card_id, |r| r.pending_resolve = Vec::new())?;
        }
        Ok(ids)
    }

    /// Stash the restart-log lines a fix run will earn *if it lands* ("Fix
    /// applied per review comment: …"), so they go on the log only once the
    /// run's commit is real (see [`Self::take_pending_fix_qa`]). Overwrites the
    /// previous stash — each fix run states its own set.
    pub fn set_pending_fix_qa(&self, card_id: Uuid, entries: &[String]) -> Result<()> {
        self.mutate_review(card_id, |r| r.pending_qa = entries.to_vec())
    }

    /// Read and clear the stashed fix-run log lines. Cleared on read so they
    /// land on the log exactly once; a run that never commits leaves them for
    /// its retry (a cancel clears them explicitly).
    pub fn take_pending_fix_qa(&self, card_id: Uuid) -> Result<Vec<String>> {
        let r = self.db.r_transaction()?;
        let rec: Option<CardReviewRecord> = r.get().primary(card_id.to_string())?;
        let entries = rec.map(|r| r.pending_qa).unwrap_or_default();
        if !entries.is_empty() {
            self.mutate_review(card_id, |r| r.pending_qa = Vec::new())?;
        }
        Ok(entries)
    }

    // --- review tasks (foreign PRs under review) ------------------------

    /// All review tasks, across every project (loaded once at startup).
    pub fn list_review_tasks(&self) -> Result<Vec<ReviewTask>> {
        let r = self.db.r_transaction()?;
        let mut out = Vec::new();
        for rec in r.scan().primary::<ReviewTaskRecord>()?.all()? {
            match rec {
                Ok(r) => out.push(r.task),
                Err(e) => tracing::warn!("skipping undecodable review-task record: {e}"),
            }
        }
        Ok(out)
    }

    pub fn list_review_tasks_for_project(&self, project_id: Uuid) -> Result<Vec<ReviewTask>> {
        Ok(self
            .list_review_tasks()?
            .into_iter()
            .filter(|t| t.project_id == project_id)
            .collect())
    }

    pub fn get_review_task(&self, id: Uuid) -> Result<ReviewTask> {
        let r = self.db.r_transaction()?;
        let rec: Option<ReviewTaskRecord> = r.get().primary(id.to_string())?;
        rec.map(|r| r.task)
            .ok_or_else(|| CoreError::NotFound(format!("review task {id}")))
    }

    pub fn upsert_review_task(&self, task: &ReviewTask) -> Result<()> {
        let rec = ReviewTaskRecord {
            id: task.id.to_string(),
            project_id: task.project_id.to_string(),
            task: task.clone(),
        };
        let rw = self.db.rw_transaction()?;
        let old: Option<ReviewTaskRecord> = rw.get().primary(rec.id.clone())?;
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    /// Atomically read-modify-write a review task inside a single transaction
    /// (mirrors [`Self::mutate_card`]). Returns the new task.
    pub fn mutate_review_task<F>(&self, id: Uuid, f: F) -> Result<ReviewTask>
    where
        F: FnOnce(&mut ReviewTask) -> Result<()>,
    {
        let rw = self.db.rw_transaction()?;
        let old: ReviewTaskRecord = rw
            .get()
            .primary(id.to_string())?
            .ok_or_else(|| CoreError::NotFound(format!("review task {id}")))?;
        let mut task = old.task.clone();
        f(&mut task)?;
        let rec = ReviewTaskRecord {
            id: task.id.to_string(),
            project_id: task.project_id.to_string(),
            task: task.clone(),
        };
        rw.update(old, rec)?;
        rw.commit()?;
        Ok(task)
    }

    /// PR numbers the user has permanently dismissed from a project's review board.
    pub fn dismissed_reviews(&self, project_id: Uuid) -> Result<Vec<u64>> {
        let r = self.db.r_transaction()?;
        let rec: Option<DismissedReviewsRecord> = r.get().primary(project_id.to_string())?;
        Ok(rec.map(|r| r.pr_numbers).unwrap_or_default())
    }

    /// Record a PR number as permanently dismissed for a project (idempotent).
    pub fn add_dismissed_review(&self, project_id: Uuid, pr_number: u64) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        let old: Option<DismissedReviewsRecord> = rw.get().primary(project_id.to_string())?;
        let mut rec = old.clone().unwrap_or_else(|| DismissedReviewsRecord {
            project_id: project_id.to_string(),
            pr_numbers: Vec::new(),
        });
        if !rec.pr_numbers.contains(&pr_number) {
            rec.pr_numbers.push(pr_number);
        }
        match old {
            Some(old) => rw.update(old, rec)?,
            None => rw.insert(rec)?,
        }
        rw.commit()?;
        Ok(())
    }

    pub fn delete_review_task(&self, id: Uuid) -> Result<()> {
        let rw = self.db.rw_transaction()?;
        if let Some(rec) = rw.get().primary::<ReviewTaskRecord>(id.to_string())? {
            rw.remove(rec)?;
        }
        // Review-run activity is stored in the transcript table keyed by the task id.
        let lines: Vec<TranscriptRecord> = rw
            .scan()
            .primary::<TranscriptRecord>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for line in lines.into_iter().filter(|t| t.card_id == id.to_string()) {
            rw.remove(line)?;
        }
        rw.commit()?;
        Ok(())
    }

    // --- transcripts ----------------------------------------------------

    pub fn append_transcript(&self, card_id: Uuid, ts: i64, line: &str) -> Result<()> {
        let rec = TranscriptRecord {
            id: Uuid::new_v4().to_string(),
            card_id: card_id.to_string(),
            ts,
            seq: TRANSCRIPT_SEQ.fetch_add(1, Ordering::Relaxed),
            line: line.to_string(),
        };
        let rw = self.db.rw_transaction()?;
        rw.insert(rec)?;
        rw.commit()?;
        Ok(())
    }

    /// The stored lines with their timestamps, in append order — what the UI's
    /// activity feed needs to rebuild a card's transcript after a restart. The
    /// table has no `card_id` secondary key, so this is a full scan: call it off
    /// the async worker.
    pub fn load_transcript_entries(&self, card_id: Uuid) -> Result<Vec<(i64, String)>> {
        let r = self.db.r_transaction()?;
        let wanted = card_id.to_string();
        let mut rows: Vec<TranscriptRecord> = Vec::new();
        for rec in r.scan().primary::<TranscriptRecord>()?.all()? {
            let rec = rec?;
            if rec.card_id == wanted {
                rows.push(rec);
            }
        }
        // (ts, seq): seq breaks ties so same-millisecond lines keep append order.
        rows.sort_by_key(|t| (t.ts, t.seq));
        Ok(rows.into_iter().map(|t| (t.ts, t.line)).collect())
    }

    pub fn load_transcript(&self, card_id: Uuid) -> Result<Vec<String>> {
        Ok(self
            .load_transcript_entries(card_id)?
            .into_iter()
            .map(|(_, line)| line)
            .collect())
    }
}

/// One-off maintenance: rebuild the database at `old` into a fresh file at `new`,
/// copying every decodable record.
///
/// Re-inserting re-serializes each record with the current canonical JSON codec,
/// which normalizes any legacy value that no longer round-trips byte-for-byte —
/// notably a pre-[`crate::Cost`] `cost_usd` written as a raw `f64` (e.g.
/// `9.708580000000001`), which the current `Cost` rounds to whole cents and
/// re-emits as `9.71`. native_db's `update`/`remove` are value-matched (they
/// compare the re-encoded item to the stored bytes), so a record carrying such a
/// stale float is otherwise *frozen* — unwritable and even undeletable. Rebuilding
/// into a fresh file writes each record via `insert` (no value-match), so every
/// record lands in the canonical form and normal writes work again.
///
/// Undecodable records (e.g. a stray record left by an intermediate dev build) are
/// skipped and counted rather than aborting the whole rebuild. Returns per-table
/// `(name, copied, skipped)` counts. Run with the app CLOSED — the database allows
/// a single writer, so opening `old` here will fail while Usine holds it.
pub fn rebuild_database(old: &Path, new: &Path) -> Result<Vec<(&'static str, usize, usize)>> {
    let src = Builder::new().create(&MODELS, old)?;
    let dst = Builder::new().create(&MODELS, new)?;
    let r = src.r_transaction()?;
    let rw = dst.rw_transaction()?;
    let mut report: Vec<(&'static str, usize, usize)> = Vec::new();

    // Copy one table: scan the source, re-inserting each decodable record into the
    // destination; a record that won't decode is counted and skipped, not fatal.
    macro_rules! copy_table {
        ($ty:ty, $name:expr) => {{
            let (mut copied, mut skipped) = (0usize, 0usize);
            for rec in r.scan().primary::<$ty>()?.all()? {
                match rec {
                    Ok(rec) => {
                        rw.insert(rec)?;
                        copied += 1;
                    }
                    Err(_) => skipped += 1,
                }
            }
            report.push(($name, copied, skipped));
        }};
    }

    copy_table!(ProjectRecord, "projects");
    copy_table!(CardRecord, "cards");
    copy_table!(SettingsRecord, "settings");
    copy_table!(TranscriptRecord, "transcripts");
    copy_table!(CardPlanRecord, "plans");
    copy_table!(CardOptionsRecord, "options");
    copy_table!(CardAttachmentsRecord, "attachments");
    copy_table!(CardReviewRecord, "reviews");
    copy_table!(ReviewTaskRecord, "review_tasks");
    copy_table!(DismissedReviewsRecord, "dismissed_reviews");
    copy_table!(CardAnswerRecord, "answers");

    rw.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::{CardConfig, ProjectConfig};
    use crate::domain::model::{CardState, DesignSub};
    use std::path::PathBuf;

    #[test]
    fn rebuild_round_trips_all_records() {
        // Rebuilding an in-memory-seeded DB into a fresh file must preserve every
        // record type, and the rebuilt file must be openable and writable.
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.db");
        let out_path = dir.path().join("out.db");
        {
            let store = Store::open(&src_path).unwrap();
            let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
            store.upsert_project(&project).unwrap();
            let mut card = Card::new(project.id, "c", "d", CardConfig::default());
            card.cost = crate::Cost::from_usd(1.23);
            store.upsert_card(&card).unwrap();
            store.save_plan(card.id, "the plan").unwrap();
            store.append_transcript(card.id, 1, "hello").unwrap();
        }

        let report = super::rebuild_database(&src_path, &out_path).unwrap();
        let cards = report.iter().find(|(n, ..)| *n == "cards").unwrap();
        assert_eq!(cards.1, 1, "one card copied");
        assert_eq!(cards.2, 0, "none skipped");

        // The rebuilt DB opens, keeps the data, and accepts writes.
        let store = Store::open(&out_path).unwrap();
        let all = store.list_cards().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].cost, crate::Cost::from_usd(1.23));
        assert_eq!(
            store.get_plan(all[0].id).unwrap().as_deref(),
            Some("the plan")
        );
        assert_eq!(store.load_transcript(all[0].id).unwrap(), vec!["hello"]);
        store.upsert_card(&all[0]).unwrap();
    }

    #[test]
    fn project_and_card_round_trip() {
        let store = Store::open_in_memory().unwrap();

        let settings = store.settings().unwrap(); // defaults
        let project = Project::new(
            "demo",
            PathBuf::from("/tmp/demo"),
            crate::ProjectConfig::default(),
        );
        store.upsert_project(&project).unwrap();

        let mut card = Card::new(
            project.id,
            "Add feature",
            "Implement the thing",
            settings.new_card_config(),
        );
        store.upsert_card(&card).unwrap();

        // Mutate state and re-save (exercises the update path).
        card.state = CardState::Designing(DesignSub::Running);
        card.cost = crate::Cost::from_usd(1.25);
        card.updated_at += 1;
        store.upsert_card(&card).unwrap();

        let loaded = store.get_card(card.id).unwrap();
        assert_eq!(loaded.title, "Add feature");
        assert!(matches!(
            loaded.state,
            CardState::Designing(DesignSub::Running)
        ));
        assert_eq!(loaded.cost, crate::Cost::from_usd(1.25));

        assert_eq!(store.list_projects().unwrap().len(), 1);
        assert_eq!(store.list_cards_for_project(project.id).unwrap().len(), 1);

        store.append_transcript(card.id, 1, "hello").unwrap();
        store.append_transcript(card.id, 2, "world").unwrap();
        assert_eq!(
            store.load_transcript(card.id).unwrap(),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn plan_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        assert_eq!(store.get_plan(id).unwrap(), None);
        store.save_plan(id, "step 1; step 2").unwrap();
        assert_eq!(
            store.get_plan(id).unwrap().as_deref(),
            Some("step 1; step 2")
        );
        store.save_plan(id, "revised").unwrap();
        assert_eq!(store.get_plan(id).unwrap().as_deref(), Some("revised"));
        store.delete_plan(id).unwrap();
        assert_eq!(store.get_plan(id).unwrap(), None);
        // Deleting a plan that isn't there is a no-op, not an error.
        store.delete_plan(id).unwrap();
    }

    #[test]
    fn mutate_card_is_atomic_read_modify_write() {
        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
        store.upsert_project(&project).unwrap();
        let card = Card::new(project.id, "c", "d", CardConfig::default());
        store.upsert_card(&card).unwrap();

        let updated = store
            .mutate_card(card.id, |c| {
                c.cost += crate::Cost::from_usd(0.5);
                Ok(())
            })
            .unwrap();
        assert_eq!(updated.cost, crate::Cost::from_usd(0.5));
        assert_eq!(
            store.get_card(card.id).unwrap().cost,
            crate::Cost::from_usd(0.5)
        );

        // A missing card is a NotFound error, not a silent insert.
        assert!(store.mutate_card(Uuid::new_v4(), |_| Ok(())).is_err());
    }

    #[test]
    fn canonicalize_preserves_data_and_keeps_records_updatable() {
        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
        store.upsert_project(&project).unwrap();
        let card = Card::new(project.id, "c", "d", CardConfig::default());
        store.upsert_card(&card).unwrap();

        store.canonicalize_records();

        // Data survives the refresh...
        assert_eq!(store.list_projects().unwrap().len(), 1);
        assert_eq!(store.get_card(card.id).unwrap().title, "c");
        // ...and the update path (which byte-matches the stored record) still lands.
        let mut edited = project.clone();
        edited.config.reviewer = Some("octocat".into());
        store.upsert_project(&edited).unwrap();
        assert_eq!(
            store
                .get_project(project.id)
                .unwrap()
                .config
                .reviewer
                .as_deref(),
            Some("octocat")
        );
    }

    #[test]
    fn transcript_same_ts_keeps_append_order() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        // Identical timestamps: the seq tiebreaker must preserve append order.
        for line in ["a", "b", "c", "d"] {
            store.append_transcript(id, 42, line).unwrap();
        }
        assert_eq!(store.load_transcript(id).unwrap(), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn transcript_entries_carry_timestamps_in_order() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        // Written out of order: the feed must still read chronologically.
        store.append_transcript(id, 20, "second").unwrap();
        store.append_transcript(id, 10, "first").unwrap();
        assert_eq!(
            store.load_transcript_entries(id).unwrap(),
            vec![(10, "first".to_string()), (20, "second".to_string())]
        );
        // An entity that never ran (or whose rows were deleted) reads empty
        // rather than erroring — the UI asks for every selected card.
        assert!(store
            .load_transcript_entries(Uuid::new_v4())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deleting_project_removes_its_cards() {
        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
        store.upsert_project(&project).unwrap();
        let card = Card::new(project.id, "c", "d", CardConfig::default());
        store.upsert_card(&card).unwrap();

        store.delete_project(project.id).unwrap();
        assert!(store.list_projects().unwrap().is_empty());
        assert!(store.list_cards().unwrap().is_empty());
    }

    #[test]
    fn review_task_round_trip_and_project_cascade() {
        use crate::domain::model::{DraftComment, ReviewEvent, ReviewStatus, ReviewTask};

        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
        store.upsert_project(&project).unwrap();

        let task = ReviewTask::new(
            project.id,
            42,
            "Add feature",
            "octocat",
            "http://x",
            "feat",
            "main",
        );
        store.upsert_review_task(&task).unwrap();

        // Read back by id and by project filter.
        assert_eq!(store.get_review_task(task.id).unwrap().pr_number, 42);
        assert_eq!(
            store
                .list_review_tasks_for_project(project.id)
                .unwrap()
                .len(),
            1
        );

        // Atomic mutate advances the status (exercises the update path).
        let updated = store
            .mutate_review_task(task.id, |t| {
                t.status = ReviewStatus::AwaitingValidation {
                    drafts: vec![DraftComment {
                        path: "a.rs".into(),
                        line: Some(3),
                        body: "nit".into(),
                        severity: "low".into(),
                        selected: true,
                    }],
                    summary: "looks good".into(),
                    event: ReviewEvent::Comment,
                };
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            updated.status,
            ReviewStatus::AwaitingValidation { .. }
        ));

        // Deleting the project cascades to its review tasks.
        store.delete_project(project.id).unwrap();
        assert!(store.list_review_tasks().unwrap().is_empty());
    }
}

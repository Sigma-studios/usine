//! The pure card lifecycle state machine.
//!
//! [`transition`] is a total, side-effect-free function shared by both the
//! executor (which drives real runs) and the UI (which renders the result), so
//! the two can never disagree about which moves are legal. All effects —
//! spawning a run, creating a worktree, calling the forge — are the executor's
//! responsibility, keyed off the *resulting* state.

use crate::domain::model::{
    CardState, DesignSub, FixVerdict, Intervention, PrReviewSub, ReviewSub, RunSub,
};
use crate::error::{CoreError, Result};

/// How many validate-command runs the gate's auto fix-loop gets before parking
/// the card for the user. Attempt `n` failing with `n >= MAX` parks; the user
/// can then grant one more fix cycle at a time, re-run the whole gate, or skip.
pub const MAX_VALIDATION_ATTEMPTS: u32 = 3;

/// Every legal stimulus that can move a card. Variants prefixed `Agent`/
/// `Comments` are agent-driven; the rest are user-triggered.
#[derive(Debug, Clone)]
pub enum Transition {
    // --- user-triggered ---
    StartPlan,
    StartImplement,
    /// Start (or, from `Concluded`, follow up on) a read-only investigation run.
    StartInvestigate,
    AnswerIntervention,
    ApprovePlan,
    RejectPlan,
    RequestChanges,
    /// Kick off the self-review pass over the committed diff.
    StartSelfReview,
    /// Apply the selected self-review fixes.
    ApplySelfFixes,
    /// Skip the self-review pass entirely and go straight to the PR.
    SkipReview,
    /// From the self-review fix picker, skip applying fixes and go to the PR.
    SkipToPr,
    /// Enter the validation gate (or re-run it from the parked failure,
    /// resetting the attempt budget). Applied by the executor whenever a card
    /// reaches `ReadyForPr` with a validate command configured, and by the
    /// user's "Run validation again".
    StartValidation,
    /// From the parked validation failure, grant one more agent fix cycle.
    FixValidation,
    /// From the parked validation failure, give up on the gate and open the PR
    /// anyway.
    SkipValidation,
    CreatePr,
    FetchComments,
    SelectFixes,
    /// Send another free-form change to the agent on a card whose PR is already
    /// open. From `ReadyToMerge` it loops back through applying fixes; from
    /// `PrReview(Idle)` (the freshly-opened PR, before any comment triage) it
    /// applies the change in place and returns to `Idle`.
    RequestPostPrChange,
    Merge,
    Cancel,
    Retry,
    /// User-triggered "do-over": send the card back to the starting block from
    /// wherever it is, to re-run the task from a (possibly amended) prompt.
    ResetToStart,
    /// User-triggered: mark the card finished from wherever it is, without going
    /// through the PR/merge flow.
    MarkDone,
    /// Ask the agent a read-only question while the work is parked. Wraps the
    /// current state in `Answering` so the question run has a state of its own
    /// (a crash/retry re-runs the question, not the parked phase).
    AskQuestion {
        question: String,
    },
    // --- agent-driven ---
    AgentNeedsInput(Intervention),
    AgentPlanReady {
        plan: String,
    },
    /// The investigation run finished with its conclusion.
    AgentConcluded {
        conclusion: String,
    },
    AgentImplementDone,
    /// The self-review agent produced its verdicts on the committed diff.
    SelfReviewReady {
        verdicts: Vec<FixVerdict>,
    },
    /// The self-review fix run finished.
    SelfFixesDone,
    /// The validate command exited 0.
    ValidationPassed,
    /// The validate command exited non-zero; carries the capped output tail.
    ValidationFailed {
        output: String,
    },
    /// The validation-fix agent run finished (committed); re-run the check.
    ValidationFixDone,
    CommentsFetched {
        verdicts: Vec<FixVerdict>,
    },
    AgentFixesDone,
    /// The PR gate has nothing left to wait for, so the card can skip the
    /// comment-triage chain and go straight to the merge gate: either a reviewer
    /// approved with nothing left to triage, or no reviewer was ever assigned
    /// (so no approval will ever come). Fired by the review poll (and its manual
    /// ↻ twin, and — for the no-reviewer case — PR creation itself) off
    /// [`Card::approval_clears_merge`](crate::domain::model::Card::approval_clears_merge)
    /// and
    /// [`Card::no_reviewer_clears_merge`](crate::domain::model::Card::no_reviewer_clears_merge),
    /// which own the "is it really clear?" rules.
    ReviewApproved,
    /// The reconciliation found the card's PR merged on GitHub directly while
    /// its review hadn't finished (from `ReadyToMerge` — review passed — the
    /// existing `Merge` edge to `Done` is used instead).
    PrMergedExternally,
    /// The reconciliation found the card's PR closed on GitHub without merging.
    PrClosedExternally,
    AgentError {
        message: String,
    },
    /// The question run finished (its answer recorded elsewhere); unwrap
    /// `Answering` back to the state the question was asked from.
    QuestionAnswered,
}

/// Compute the next state, or [`CoreError::IllegalTransition`] if the move is
/// not allowed from the current state.
pub fn transition(state: &CardState, t: Transition) -> Result<CardState> {
    use CardState as S;
    use Transition as T;

    let next = match (state, t) {
        // Starting block → designing, or straight to implementing ("no plan").
        (S::StartingBlock, T::StartPlan) => S::Designing(DesignSub::Running),
        (S::StartingBlock, T::StartImplement) => S::Implementing(RunSub::Running),

        // Designing
        (S::Designing(DesignSub::Running), T::AgentNeedsInput(i)) => {
            S::Designing(DesignSub::Intervention(i))
        }
        (S::Designing(DesignSub::Intervention(_)), T::AnswerIntervention) => {
            S::Designing(DesignSub::Running)
        }
        (S::Designing(DesignSub::Running), T::AgentPlanReady { plan }) => {
            S::Designing(DesignSub::AwaitingApproval { plan })
        }
        // A run can also reach a terminal result directly from an intervention
        // (e.g. an interactive provider that answers its own question, or a
        // late terminal event racing the park) — accept it rather than wedging.
        (S::Designing(DesignSub::Intervention(_)), T::AgentPlanReady { plan }) => {
            S::Designing(DesignSub::AwaitingApproval { plan })
        }
        (S::Designing(DesignSub::AwaitingApproval { .. }), T::ApprovePlan) => {
            S::Implementing(RunSub::Running)
        }
        (S::Designing(DesignSub::AwaitingApproval { .. }), T::RejectPlan) => {
            S::Designing(DesignSub::Running)
        }

        // Investigating (the read-only "investigate only" cards). Starts from
        // the starting block; a follow-up round re-enters from the conclusion.
        (S::StartingBlock, T::StartInvestigate) => S::Investigating(RunSub::Running),
        (S::Concluded { .. }, T::StartInvestigate) => S::Investigating(RunSub::Running),
        (S::Investigating(RunSub::Running), T::AgentNeedsInput(i)) => {
            S::Investigating(RunSub::Intervention(i))
        }
        (S::Investigating(RunSub::Intervention(_)), T::AnswerIntervention) => {
            S::Investigating(RunSub::Running)
        }
        (S::Investigating(RunSub::Running), T::AgentConcluded { conclusion }) => {
            S::Concluded { conclusion }
        }
        // As in the other phases: tolerate a terminal racing a parked question.
        (S::Investigating(RunSub::Intervention(_)), T::AgentConcluded { conclusion }) => {
            S::Concluded { conclusion }
        }

        // Implementing
        (S::Implementing(RunSub::Running), T::AgentNeedsInput(i)) => {
            S::Implementing(RunSub::Intervention(i))
        }
        (S::Implementing(RunSub::Intervention(_)), T::AnswerIntervention) => {
            S::Implementing(RunSub::Running)
        }
        (S::Implementing(RunSub::Running), T::AgentImplementDone) => {
            S::AwaitingReview(ReviewSub::ReadyForReview)
        }
        // As above: tolerate a terminal arriving while parked on a question.
        (S::Implementing(RunSub::Intervention(_)), T::AgentImplementDone) => {
            S::AwaitingReview(ReviewSub::ReadyForReview)
        }

        // Awaiting review: the finished work sits committed in the card's own
        // worktree. The user reviews it there (editor / preview app), then runs a
        // self-review pass or opens the PR — nothing enters the main working copy.
        // Before reviewing, a card can still be bounced back to the agent to revise.
        (S::AwaitingReview(ReviewSub::ReadyForReview), T::RequestChanges) => {
            S::Implementing(RunSub::Running)
        }
        // Self-review pass, or skip straight to the PR.
        (S::AwaitingReview(ReviewSub::ReadyForReview), T::StartSelfReview) => {
            S::AwaitingReview(ReviewSub::Reviewing)
        }
        (S::AwaitingReview(ReviewSub::ReadyForReview), T::SkipReview) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        // No findings at all → skip the picker and go straight to the PR. Any
        // feedback (even not-worth-fixing nits) still parks on the picker so the
        // user sees it before opening the PR.
        (S::AwaitingReview(ReviewSub::Reviewing), T::SelfReviewReady { verdicts }) => {
            if verdicts.is_empty() {
                S::AwaitingReview(ReviewSub::ReadyForPr)
            } else {
                S::AwaitingReview(ReviewSub::SelectingFixes { verdicts })
            }
        }
        (S::AwaitingReview(ReviewSub::SelectingFixes { .. }), T::ApplySelfFixes) => {
            S::AwaitingReview(ReviewSub::ApplyingFixes)
        }
        (S::AwaitingReview(ReviewSub::SelectingFixes { .. }), T::SkipToPr) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        // Re-run the self-review pass from the picker (e.g. to refresh the analysis).
        (S::AwaitingReview(ReviewSub::SelectingFixes { .. }), T::StartSelfReview) => {
            S::AwaitingReview(ReviewSub::Reviewing)
        }
        // The picker is where an auto-reviewed implementation first parks, so it
        // must also offer the wholesale bounce back to the agent that the
        // `ReadyForReview` gate offers.
        (S::AwaitingReview(ReviewSub::SelectingFixes { .. }), T::RequestChanges) => {
            S::Implementing(RunSub::Running)
        }
        // Self-review is a single pass: once fixes are applied, advance to the PR
        // rather than looping back for another review.
        (S::AwaitingReview(ReviewSub::ApplyingFixes), T::SelfFixesDone) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        // Even once the work is cleared for a PR, the user can still bounce it back
        // to the agent to revise (same as from `ReadyForReview`).
        (S::AwaitingReview(ReviewSub::ReadyForPr), T::RequestChanges) => {
            S::Implementing(RunSub::Running)
        }
        // Ready for the PR: open it (executor renames the branch + pushes first).
        (S::AwaitingReview(ReviewSub::ReadyForPr), T::CreatePr) => S::PrReview(PrReviewSub::Idle),

        // Validation gate. The executor applies `StartValidation` right after
        // any edge that lands on `ReadyForPr` when the project has a validate
        // command, so `ReadyForPr` means "cleared for PR: validated, skipped,
        // or nothing to validate". A failure inside the attempt budget loops
        // through an agent fix run and back to the check — the re-review loop
        // self-review deliberately lacks; an exhausted budget parks the card.
        (
            S::AwaitingReview(ReviewSub::ReadyForPr | ReviewSub::ValidationFailed { .. }),
            T::StartValidation,
        ) => S::AwaitingReview(ReviewSub::Validating { attempt: 1 }),
        (S::AwaitingReview(ReviewSub::Validating { .. }), T::ValidationPassed) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        (S::AwaitingReview(ReviewSub::Validating { attempt }), T::ValidationFailed { output }) => {
            if *attempt < MAX_VALIDATION_ATTEMPTS {
                S::AwaitingReview(ReviewSub::FixingValidation {
                    attempt: *attempt,
                    output,
                })
            } else {
                S::AwaitingReview(ReviewSub::ValidationFailed {
                    attempt: *attempt,
                    output,
                })
            }
        }
        (S::AwaitingReview(ReviewSub::FixingValidation { attempt, .. }), T::ValidationFixDone) => {
            S::AwaitingReview(ReviewSub::Validating {
                attempt: attempt + 1,
            })
        }
        // Parked options: one more fix cycle (a further failure parks again,
        // since the attempt stays past the budget), or open the PR anyway.
        (S::AwaitingReview(ReviewSub::ValidationFailed { attempt, output }), T::FixValidation) => {
            S::AwaitingReview(ReviewSub::FixingValidation {
                attempt: *attempt,
                output: output.clone(),
            })
        }
        (S::AwaitingReview(ReviewSub::ValidationFailed { .. }), T::SkipValidation) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        // The parked failure can also bounce the work back to the agent
        // wholesale, same as the other awaiting-review parks.
        (S::AwaitingReview(ReviewSub::ValidationFailed { .. }), T::RequestChanges) => {
            S::Implementing(RunSub::Running)
        }

        // PR review
        (S::PrReview(PrReviewSub::Idle), T::FetchComments) => {
            S::PrReview(PrReviewSub::FetchingComments)
        }
        // Re-run triage from the picker (e.g. after new review comments arrive).
        (S::PrReview(PrReviewSub::SelectingFixes { .. }), T::FetchComments) => {
            S::PrReview(PrReviewSub::FetchingComments)
        }
        (S::PrReview(PrReviewSub::FetchingComments), T::CommentsFetched { verdicts }) => {
            S::PrReview(PrReviewSub::SelectingFixes { verdicts })
        }
        (S::PrReview(PrReviewSub::SelectingFixes { .. }), T::SelectFixes) => {
            S::PrReview(PrReviewSub::ApplyingFixes)
        }
        (S::PrReview(PrReviewSub::ApplyingFixes), T::AgentFixesDone) => S::ReadyToMerge,

        // An approval with no comments to triage skips the chain above — there is
        // no comment to fetch, nothing to select, and no fix run to finish, so
        // without this edge the card would sit in `Idle` forever. Only legal from
        // `Idle`: mid-triage the chain is already carrying the card to merge, and
        // a late poll tick must not yank it out from under a running fix.
        (S::PrReview(PrReviewSub::Idle), T::ReviewApproved) => S::ReadyToMerge,

        // A card whose PR is already open can be reprompted directly from the PR
        // gate — before any reviewer-comment triage — to tweak the branch. The fix
        // runs, commits + pushes (updating the open PR), then returns to `Idle` so
        // the card stays in the PR-review gate rather than jumping to merge.
        (S::PrReview(PrReviewSub::Idle), T::RequestPostPrChange) => {
            S::PrReview(PrReviewSub::ApplyingChange)
        }
        (S::PrReview(PrReviewSub::ApplyingChange), T::AgentFixesDone) => {
            S::PrReview(PrReviewSub::Idle)
        }

        // Ready to merge → request another change (loops back), or done.
        (S::ReadyToMerge, T::RequestPostPrChange) => S::PrReview(PrReviewSub::ApplyingFixes),
        // Reviewer comments can land *after* the triage chain already carried
        // the card to the merge gate (a follow-up on a reply, a fresh thread on
        // the fixed code). Reevaluating re-enters the same triage chain, which
        // loops back here through `ApplyingFixes`. The UI offers this only when
        // some thread still awaits an answer (`Card::unanswered_count`).
        (S::ReadyToMerge, T::FetchComments) => S::PrReview(PrReviewSub::FetchingComments),
        (S::ReadyToMerge, T::Merge) => S::Done,

        // The PR left GitHub out from under the card. Merged while comments
        // were still pending → "merged without review"; a `ReadyToMerge` card
        // merged externally goes to `Done` via the `Merge` edge above instead —
        // its review completed. Closed without merging parks either gate on the
        // same column, flagged as closed. Only legal from the two parked PR
        // gates: a mid-triage or mid-fix card is left for the run to finish.
        (S::PrReview(PrReviewSub::Idle), T::PrMergedExternally) => {
            S::MergedWithoutReview { merged: true }
        }
        (S::PrReview(PrReviewSub::Idle) | S::ReadyToMerge, T::PrClosedExternally) => {
            S::MergedWithoutReview { merged: false }
        }

        // Questions: from any parked hand-off state the user can ask the agent
        // a read-only question. The current state is wrapped, not replaced, so
        // the answer (or a cancel, or a crash + retry) restores it exactly.
        (s, T::AskQuestion { question })
            if matches!(
                s,
                S::Designing(DesignSub::AwaitingApproval { .. })
                    | S::AwaitingReview(
                        ReviewSub::ReadyForReview
                            | ReviewSub::ReadyForPr
                            | ReviewSub::ValidationFailed { .. }
                    )
                    | S::PrReview(PrReviewSub::Idle)
                    | S::ReadyToMerge
            ) =>
        {
            S::Answering {
                previous: Box::new(s.clone()),
                question,
            }
        }
        (S::Answering { previous, .. }, T::QuestionAnswered) => (**previous).clone(),
        // Cancelling a question restores the exact state it was asked from.
        (S::Answering { previous, .. }, T::Cancel) => (**previous).clone(),

        // Any active run can fault.
        (s, T::AgentError { message }) if s.is_running() => S::Failed {
            previous: Box::new(s.clone()),
            message,
        },

        // Recovery
        (S::Failed { previous, .. }, T::Retry) => (**previous).clone(),
        // A do-over is legal from any state — the executor cancels any live run
        // and clears the card's execution artifacts before re-running.
        (_, T::ResetToStart) => S::StartingBlock,
        // Marking done is likewise legal from anywhere (the executor cancels any
        // live run first); already-done is idempotent.
        (_, T::MarkDone) => S::Done,
        (S::Designing(_), T::Cancel) => S::StartingBlock,
        (S::Investigating(_), T::Cancel) => S::StartingBlock,
        (S::Implementing(_), T::Cancel) => S::StartingBlock,
        (S::AwaitingReview(ReviewSub::Reviewing), T::Cancel) => {
            S::AwaitingReview(ReviewSub::ReadyForReview)
        }
        (S::AwaitingReview(ReviewSub::ApplyingFixes), T::Cancel) => {
            S::AwaitingReview(ReviewSub::ReadyForReview)
        }
        // Cancelling the check means "skip it for now" — the PR form is
        // immediately available. Cancelling the fix run parks with the failing
        // output (and its options) intact.
        (S::AwaitingReview(ReviewSub::Validating { .. }), T::Cancel) => {
            S::AwaitingReview(ReviewSub::ReadyForPr)
        }
        (S::AwaitingReview(ReviewSub::FixingValidation { attempt, output }), T::Cancel) => {
            S::AwaitingReview(ReviewSub::ValidationFailed {
                attempt: *attempt,
                output: output.clone(),
            })
        }
        (S::PrReview(PrReviewSub::FetchingComments), T::Cancel) => S::PrReview(PrReviewSub::Idle),
        (S::PrReview(PrReviewSub::ApplyingFixes), T::Cancel) => S::PrReview(PrReviewSub::Idle),
        (S::PrReview(PrReviewSub::ApplyingChange), T::Cancel) => S::PrReview(PrReviewSub::Idle),

        (s, other) => {
            return Err(CoreError::IllegalTransition(format!(
                "{:?} cannot accept {:?}",
                s, other
            )))
        }
    };
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        CardState, DesignSub, Intervention, PrReviewSub, ReviewComment, ReviewSub, RunSub,
    };

    fn intervention() -> Intervention {
        Intervention {
            request_id: "req-1".into(),
            question: "Which database?".into(),
            options: vec!["sqlite".into(), "postgres".into()],
        }
    }

    fn verdicts() -> Vec<FixVerdict> {
        vec![FixVerdict {
            comment: ReviewComment {
                id: 1,
                author: "octocat".into(),
                path: "src/lib.rs".into(),
                line: Some(10),
                body: "nit".into(),
            },
            worth_fixing: true,
            severity: "high".into(),
            rationale: "valid".into(),
            selected: false,
            reply: String::new(),
        }]
    }

    /// Drive a card through the entire happy path and assert each landing state.
    #[test]
    fn full_happy_path() {
        let s = CardState::StartingBlock;
        let s = transition(&s, Transition::StartPlan).unwrap();
        assert!(matches!(s, CardState::Designing(DesignSub::Running)));

        let s = transition(&s, Transition::AgentNeedsInput(intervention())).unwrap();
        assert!(s.needs_intervention());
        assert_eq!(s.intervention().unwrap().request_id, "req-1");

        let s = transition(&s, Transition::AnswerIntervention).unwrap();
        assert!(matches!(s, CardState::Designing(DesignSub::Running)));

        let s = transition(
            &s,
            Transition::AgentPlanReady {
                plan: "do the thing".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            s,
            CardState::Designing(DesignSub::AwaitingApproval { .. })
        ));

        let s = transition(&s, Transition::ApprovePlan).unwrap();
        assert!(matches!(s, CardState::Implementing(RunSub::Running)));

        let s = transition(&s, Transition::AgentImplementDone).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ReadyForReview)
        ));

        // Pre-PR gate: the work is committed in the worktree; run the self-review
        // pass over the committed diff, then open the PR. No checkout to the main repo.
        let s = transition(&s, Transition::StartSelfReview).unwrap();
        assert!(matches!(s, CardState::AwaitingReview(ReviewSub::Reviewing)));

        let s = transition(
            &s,
            Transition::SelfReviewReady {
                verdicts: verdicts(),
            },
        )
        .unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        ));

        let s = transition(&s, Transition::ApplySelfFixes).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ApplyingFixes)
        ));

        // Self-review is a single pass: applying fixes advances straight to the PR.
        let s = transition(&s, Transition::SelfFixesDone).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ReadyForPr)
        ));

        let s = transition(&s, Transition::CreatePr).unwrap();
        assert!(matches!(s, CardState::PrReview(PrReviewSub::Idle)));

        let s = transition(&s, Transition::FetchComments).unwrap();
        assert!(matches!(
            s,
            CardState::PrReview(PrReviewSub::FetchingComments)
        ));

        let s = transition(
            &s,
            Transition::CommentsFetched {
                verdicts: verdicts(),
            },
        )
        .unwrap();
        assert!(matches!(
            s,
            CardState::PrReview(PrReviewSub::SelectingFixes { .. })
        ));

        let s = transition(&s, Transition::SelectFixes).unwrap();
        assert!(matches!(s, CardState::PrReview(PrReviewSub::ApplyingFixes)));

        let s = transition(&s, Transition::AgentFixesDone).unwrap();
        assert!(matches!(s, CardState::ReadyToMerge));

        let s = transition(&s, Transition::Merge).unwrap();
        assert!(matches!(s, CardState::Done));
    }

    #[test]
    fn the_fix_picker_can_send_the_work_back_to_the_agent() {
        // With the self-review auto-started, the picker is where a finished
        // implementation first parks — it must offer the same wholesale bounce
        // back to implementing that `ReadyForReview` does.
        let s = CardState::AwaitingReview(ReviewSub::SelectingFixes {
            verdicts: verdicts(),
        });
        let s = transition(&s, Transition::RequestChanges).unwrap();
        assert!(matches!(s, CardState::Implementing(RunSub::Running)));
    }

    #[test]
    fn mark_done_is_legal_from_anywhere() {
        for s in [
            CardState::Designing(DesignSub::Running),
            CardState::Implementing(RunSub::Intervention(intervention())),
            CardState::AwaitingReview(ReviewSub::ReadyForReview),
            CardState::PrReview(PrReviewSub::Idle),
            CardState::ReadyToMerge,
            CardState::Done,
        ] {
            let done = transition(&s, Transition::MarkDone).unwrap();
            assert!(matches!(done, CardState::Done), "{s:?} → done");
        }
    }

    #[test]
    fn error_then_retry_returns_to_previous() {
        let running = CardState::Implementing(RunSub::Running);
        let failed = transition(
            &running,
            Transition::AgentError {
                message: "boom".into(),
            },
        )
        .unwrap();
        assert!(failed.is_failed());
        // Column stays put so the failed card is visible where it was.
        assert_eq!(failed.column(), running.column());

        let recovered = transition(&failed, Transition::Retry).unwrap();
        assert_eq!(recovered, running);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        // Can't merge from the starting block.
        assert!(transition(&CardState::StartingBlock, Transition::Merge).is_err());
        // Can't approve a plan that isn't ready.
        assert!(transition(
            &CardState::Designing(DesignSub::Running),
            Transition::ApprovePlan
        )
        .is_err());
        // A non-running state can't fault.
        assert!(transition(
            &CardState::AwaitingReview(ReviewSub::ReadyForReview),
            Transition::AgentError {
                message: "x".into()
            }
        )
        .is_err());
    }

    #[test]
    fn interrupted_running_states_mark_failed_and_resume() {
        // Startup reconciliation marks any running card as interrupted (Failed),
        // and Resume (Retry) returns it to the exact running state to re-launch.
        for running in [
            CardState::Designing(DesignSub::Running),
            CardState::Implementing(RunSub::Running),
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            CardState::PrReview(PrReviewSub::ApplyingChange),
        ] {
            assert!(running.is_running());
            let failed = transition(
                &running,
                Transition::AgentError {
                    message: "Interrupted".into(),
                },
            )
            .unwrap();
            assert!(failed.is_failed());
            let resumed = transition(&failed, Transition::Retry).unwrap();
            assert_eq!(resumed, running);
        }
    }

    #[test]
    fn validation_gate_happy_path() {
        let s = CardState::AwaitingReview(ReviewSub::ReadyForPr);
        let s = transition(&s, Transition::StartValidation).unwrap();
        assert_eq!(
            s,
            CardState::AwaitingReview(ReviewSub::Validating { attempt: 1 })
        );
        let s = transition(&s, Transition::ValidationPassed).unwrap();
        assert_eq!(s, CardState::AwaitingReview(ReviewSub::ReadyForPr));
        let s = transition(&s, Transition::CreatePr).unwrap();
        assert!(matches!(s, CardState::PrReview(PrReviewSub::Idle)));
    }

    #[test]
    fn validation_fix_loop_bounds_attempts() {
        let mut s = transition(
            &CardState::AwaitingReview(ReviewSub::ReadyForPr),
            Transition::StartValidation,
        )
        .unwrap();
        // Every failure inside the budget loops through a fix run and back to
        // the check at the next attempt.
        for attempt in 1..MAX_VALIDATION_ATTEMPTS {
            s = transition(
                &s,
                Transition::ValidationFailed {
                    output: "boom".into(),
                },
            )
            .unwrap();
            assert_eq!(
                s,
                CardState::AwaitingReview(ReviewSub::FixingValidation {
                    attempt,
                    output: "boom".into()
                })
            );
            s = transition(&s, Transition::ValidationFixDone).unwrap();
            assert_eq!(
                s,
                CardState::AwaitingReview(ReviewSub::Validating {
                    attempt: attempt + 1
                })
            );
        }
        // The last attempt failing parks the card with the output.
        s = transition(
            &s,
            Transition::ValidationFailed {
                output: "final".into(),
            },
        )
        .unwrap();
        assert_eq!(
            s,
            CardState::AwaitingReview(ReviewSub::ValidationFailed {
                attempt: MAX_VALIDATION_ATTEMPTS,
                output: "final".into()
            })
        );
    }

    #[test]
    fn parked_validation_offers_rerun_fix_skip_and_revise() {
        let parked = CardState::AwaitingReview(ReviewSub::ValidationFailed {
            attempt: MAX_VALIDATION_ATTEMPTS,
            output: "boom".into(),
        });
        // Re-running the gate resets the attempt budget.
        assert_eq!(
            transition(&parked, Transition::StartValidation).unwrap(),
            CardState::AwaitingReview(ReviewSub::Validating { attempt: 1 })
        );
        // One more fix cycle keeps the attempt count past the budget, so a
        // further failure parks again rather than looping forever.
        let s = transition(&parked, Transition::FixValidation).unwrap();
        let s = transition(&s, Transition::ValidationFixDone).unwrap();
        assert_eq!(
            s,
            CardState::AwaitingReview(ReviewSub::Validating {
                attempt: MAX_VALIDATION_ATTEMPTS + 1
            })
        );
        let s = transition(
            &s,
            Transition::ValidationFailed {
                output: "still".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ValidationFailed { attempt, .. })
                if attempt == MAX_VALIDATION_ATTEMPTS + 1
        ));
        // Skip opens the PR form; revise bounces back to implementing.
        assert_eq!(
            transition(&parked, Transition::SkipValidation).unwrap(),
            CardState::AwaitingReview(ReviewSub::ReadyForPr)
        );
        assert!(matches!(
            transition(&parked, Transition::RequestChanges).unwrap(),
            CardState::Implementing(RunSub::Running)
        ));
    }

    #[test]
    fn validation_cancel_and_failure_recovery() {
        // Cancelling the check means "skip it for now".
        let validating = CardState::AwaitingReview(ReviewSub::Validating { attempt: 2 });
        assert_eq!(
            transition(&validating, Transition::Cancel).unwrap(),
            CardState::AwaitingReview(ReviewSub::ReadyForPr)
        );
        // Cancelling the fix run parks with the output (and its options) intact.
        let fixing = CardState::AwaitingReview(ReviewSub::FixingValidation {
            attempt: 2,
            output: "boom".into(),
        });
        assert_eq!(
            transition(&fixing, Transition::Cancel).unwrap(),
            CardState::AwaitingReview(ReviewSub::ValidationFailed {
                attempt: 2,
                output: "boom".into()
            })
        );
        // Both gate running states fault and resume like any other run.
        for running in [validating, fixing] {
            assert!(running.is_running());
            let failed = transition(
                &running,
                Transition::AgentError {
                    message: "Interrupted".into(),
                },
            )
            .unwrap();
            assert!(failed.is_failed());
            assert_eq!(transition(&failed, Transition::Retry).unwrap(), running);
        }
    }

    /// Drive an investigation card through its whole lifecycle: start, park on a
    /// question, answer, conclude, follow up, re-conclude — then the exits.
    #[test]
    fn investigation_happy_path_and_follow_up_loop() {
        let s = transition(&CardState::StartingBlock, Transition::StartInvestigate).unwrap();
        assert!(matches!(s, CardState::Investigating(RunSub::Running)));

        let s = transition(&s, Transition::AgentNeedsInput(intervention())).unwrap();
        assert!(s.needs_intervention());
        let s = transition(&s, Transition::AnswerIntervention).unwrap();
        assert!(matches!(s, CardState::Investigating(RunSub::Running)));

        let s = transition(
            &s,
            Transition::AgentConcluded {
                conclusion: "verdict: the cache is unbounded".into(),
            },
        )
        .unwrap();
        assert!(matches!(s, CardState::Concluded { ref conclusion }
            if conclusion.contains("unbounded")));
        assert!(s.needs_attention());

        // Follow-up: dig deeper from the conclusion, then re-conclude.
        let s = transition(&s, Transition::StartInvestigate).unwrap();
        assert!(matches!(s, CardState::Investigating(RunSub::Running)));
        let s = transition(
            &s,
            Transition::AgentConcluded {
                conclusion: "second round".into(),
            },
        )
        .unwrap();

        // Exits: convert rides ResetToStart; MarkDone finishes the card. A
        // conclusion can never jump straight into a run — conversion goes back
        // through the starting block.
        assert!(matches!(
            transition(&s, Transition::ResetToStart).unwrap(),
            CardState::StartingBlock
        ));
        assert!(matches!(
            transition(&s, Transition::MarkDone).unwrap(),
            CardState::Done
        ));
        assert!(transition(&s, Transition::StartImplement).is_err());
        assert!(transition(&s, Transition::StartPlan).is_err());
    }

    #[test]
    fn investigation_terminal_exits_intervention_and_faults_recover() {
        // A terminal conclusion racing a parked question must not wedge.
        let parked = CardState::Investigating(RunSub::Intervention(intervention()));
        let s = transition(
            &parked,
            Transition::AgentConcluded {
                conclusion: "c".into(),
            },
        )
        .unwrap();
        assert!(matches!(s, CardState::Concluded { .. }));

        // Cancel returns to the starting block, like the other pre-work runs.
        let running = CardState::Investigating(RunSub::Running);
        assert!(matches!(
            transition(&running, Transition::Cancel).unwrap(),
            CardState::StartingBlock
        ));

        // The generic fault edge + retry cover the run like any other.
        let failed = transition(
            &running,
            Transition::AgentError {
                message: "Interrupted".into(),
            },
        )
        .unwrap();
        assert!(failed.is_failed());
        assert_eq!(transition(&failed, Transition::Retry).unwrap(), running);

        // AgentConcluded is only legal from an investigation run.
        assert!(transition(
            &CardState::Designing(DesignSub::Running),
            Transition::AgentConcluded {
                conclusion: "c".into()
            }
        )
        .is_err());
        assert!(transition(
            &CardState::StartingBlock,
            Transition::AgentConcluded {
                conclusion: "c".into()
            }
        )
        .is_err());
    }

    #[test]
    fn no_plan_starts_implementing_directly() {
        // A card marked "no plan" skips Designing entirely.
        let s = transition(&CardState::StartingBlock, Transition::StartImplement).unwrap();
        assert!(matches!(s, CardState::Implementing(RunSub::Running)));
        // ...and only from the starting block.
        assert!(transition(
            &CardState::AwaitingReview(ReviewSub::ReadyForReview),
            Transition::StartImplement
        )
        .is_err());
    }

    #[test]
    fn request_changes_returns_to_implementing() {
        // From "awaiting review", asking for changes re-enters Implementing so
        // the agent revises in the existing worktree before any PR is opened.
        let s = transition(
            &CardState::AwaitingReview(ReviewSub::ReadyForReview),
            Transition::RequestChanges,
        )
        .unwrap();
        assert!(matches!(s, CardState::Implementing(RunSub::Running)));
    }

    #[test]
    fn terminal_events_exit_intervention() {
        // A run can reach a terminal result while still parked on a question
        // (e.g. an interactive provider answering itself); it must not wedge.
        let s = CardState::Designing(DesignSub::Intervention(intervention()));
        let s = transition(&s, Transition::AgentPlanReady { plan: "p".into() }).unwrap();
        assert!(matches!(
            s,
            CardState::Designing(DesignSub::AwaitingApproval { .. })
        ));

        let s = CardState::Implementing(RunSub::Intervention(intervention()));
        let s = transition(&s, Transition::AgentImplementDone).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ReadyForReview)
        ));
    }

    #[test]
    fn reset_to_start_is_legal_from_anywhere() {
        // The do-over returns the card to the starting block regardless of where
        // it sits in the pipeline (designing, implementing, review, ready, …).
        for s in [
            CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() }),
            CardState::Implementing(RunSub::Running),
            CardState::AwaitingReview(ReviewSub::ReadyForReview),
            CardState::PrReview(PrReviewSub::Idle),
            CardState::ReadyToMerge,
            CardState::StartingBlock,
        ] {
            let reset = transition(&s, Transition::ResetToStart).unwrap();
            assert!(matches!(reset, CardState::StartingBlock), "{s:?} → start");
        }
    }

    #[test]
    fn analysis_can_rerun_from_the_fix_picker() {
        // PR triage: re-evaluate from the fix picker (e.g. new comments arrived).
        let s = CardState::PrReview(PrReviewSub::SelectingFixes {
            verdicts: verdicts(),
        });
        let s = transition(&s, Transition::FetchComments).unwrap();
        assert!(matches!(
            s,
            CardState::PrReview(PrReviewSub::FetchingComments)
        ));

        // Self-review: re-run the pass from its picker.
        let s = CardState::AwaitingReview(ReviewSub::SelectingFixes {
            verdicts: verdicts(),
        });
        let s = transition(&s, Transition::StartSelfReview).unwrap();
        assert!(matches!(s, CardState::AwaitingReview(ReviewSub::Reviewing)));
    }

    #[test]
    fn late_comments_reevaluate_from_the_merge_gate_and_loop_back() {
        // Comments that land after a triage pass already carried the card to
        // `ReadyToMerge` re-enter the same chain…
        let s = transition(&CardState::ReadyToMerge, Transition::FetchComments).unwrap();
        assert!(matches!(
            s,
            CardState::PrReview(PrReviewSub::FetchingComments)
        ));
        // …and ride it back to the merge gate.
        let s = transition(
            &s,
            Transition::CommentsFetched {
                verdicts: verdicts(),
            },
        )
        .unwrap();
        let s = transition(&s, Transition::SelectFixes).unwrap();
        let s = transition(&s, Transition::AgentFixesDone).unwrap();
        assert!(matches!(s, CardState::ReadyToMerge));
        // The merge edge is unaffected.
        assert!(matches!(
            transition(&s, Transition::Merge).unwrap(),
            CardState::Done
        ));
        // A finished card has nothing left to reevaluate.
        assert!(transition(&CardState::Done, Transition::FetchComments).is_err());
    }

    #[test]
    fn reject_plan_loops_back_to_designing() {
        let awaiting = CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() });
        let s = transition(&awaiting, Transition::RejectPlan).unwrap();
        assert!(matches!(s, CardState::Designing(DesignSub::Running)));
    }

    #[test]
    fn reprompt_from_pr_gate_applies_in_place_and_returns() {
        // A card whose PR is already open can be reprompted from the PR gate
        // (`Idle`): the change runs, then loops back to the gate — never jumping
        // to merge — so triage stays available.
        let idle = CardState::PrReview(PrReviewSub::Idle);
        let running = transition(&idle, Transition::RequestPostPrChange).unwrap();
        assert!(matches!(
            running,
            CardState::PrReview(PrReviewSub::ApplyingChange)
        ));
        assert!(running.is_running());
        let back = transition(&running, Transition::AgentFixesDone).unwrap();
        assert!(matches!(back, CardState::PrReview(PrReviewSub::Idle)));

        // Contrast: the same request from `ReadyToMerge` loops back to merge.
        let ready = CardState::ReadyToMerge;
        let running = transition(&ready, Transition::RequestPostPrChange).unwrap();
        assert!(matches!(
            running,
            CardState::PrReview(PrReviewSub::ApplyingFixes)
        ));
        let done = transition(&running, Transition::AgentFixesDone).unwrap();
        assert!(matches!(done, CardState::ReadyToMerge));
    }

    #[test]
    fn approval_with_nothing_to_triage_reaches_merge_from_the_pr_gate() {
        // The whole point: an approval carrying no comments has no triage chain
        // to ride to `ReadyToMerge`, so it gets its own edge off the gate.
        let idle = CardState::PrReview(PrReviewSub::Idle);
        let ready = transition(&idle, Transition::ReviewApproved).unwrap();
        assert!(matches!(ready, CardState::ReadyToMerge));
        // And from there the usual merge edge still applies.
        assert!(matches!(
            transition(&ready, Transition::Merge).unwrap(),
            CardState::Done
        ));

        // Illegal anywhere else — in particular, a poll tick landing mid-triage
        // must not yank the card out from under a running fix.
        for s in [
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            CardState::PrReview(PrReviewSub::ApplyingChange),
            CardState::ReadyToMerge,
            CardState::Done,
        ] {
            assert!(
                transition(&s, Transition::ReviewApproved).is_err(),
                "{s:?} should reject ReviewApproved"
            );
        }
    }

    #[test]
    fn external_pr_termination_parks_on_merged_without_review() {
        // A PR merged on GitHub while comments were still pending: the review
        // never finished, so the card parks on the new column, flagged merged.
        let idle = CardState::PrReview(PrReviewSub::Idle);
        let s = transition(&idle, Transition::PrMergedExternally).unwrap();
        assert_eq!(s, CardState::MergedWithoutReview { merged: true });

        // Closed without merging parks the same column from either gate,
        // flagged closed.
        for gate in [
            CardState::PrReview(PrReviewSub::Idle),
            CardState::ReadyToMerge,
        ] {
            let s = transition(&gate, Transition::PrClosedExternally).unwrap();
            assert_eq!(s, CardState::MergedWithoutReview { merged: false });
        }

        // A `ReadyToMerge` card merged externally completed its review — the
        // caller uses the existing `Merge` edge to `Done`, and the external
        // edge is deliberately illegal there so nothing can misroute it.
        assert!(transition(&CardState::ReadyToMerge, Transition::PrMergedExternally).is_err());

        // Mid-triage and terminal states must not be yanked by a poll tick.
        for s in [
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            CardState::PrReview(PrReviewSub::ApplyingChange),
            CardState::Done,
        ] {
            assert!(
                transition(&s, Transition::PrMergedExternally).is_err(),
                "{s:?}"
            );
            assert!(
                transition(&s, Transition::PrClosedExternally).is_err(),
                "{s:?}"
            );
        }

        // The park's exits come from the catch-all edges.
        let parked = CardState::MergedWithoutReview { merged: true };
        assert_eq!(
            transition(&parked, Transition::ResetToStart).unwrap(),
            CardState::StartingBlock
        );
        assert_eq!(
            transition(&parked, Transition::MarkDone).unwrap(),
            CardState::Done
        );
    }

    #[test]
    fn ready_for_pr_can_still_request_changes() {
        // Cleared for a PR, the user can still bounce the work back to the agent —
        // same landing state as requesting changes from `ReadyForReview`.
        let ready = CardState::AwaitingReview(ReviewSub::ReadyForPr);
        let s = transition(&ready, Transition::RequestChanges).unwrap();
        assert!(matches!(s, CardState::Implementing(RunSub::Running)));
    }

    #[test]
    fn self_review_skips_the_picker_only_when_empty() {
        // A self-review that surfaces no findings at all bypasses the fix picker
        // and goes straight to the PR — no empty fix screen to click through.
        let reviewing = CardState::AwaitingReview(ReviewSub::Reviewing);
        let s = transition(&reviewing, Transition::SelfReviewReady { verdicts: vec![] }).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::ReadyForPr)
        ));

        // Any feedback still parks on the picker so the user sees it — even a
        // not-worth-fixing nit.
        let nit = vec![FixVerdict {
            comment: ReviewComment {
                id: 1,
                author: "self-review".into(),
                path: "src/lib.rs".into(),
                line: None,
                body: "optional nit".into(),
            },
            worth_fixing: false,
            severity: "low".into(),
            rationale: "minor".into(),
            selected: false,
            reply: String::new(),
        }];
        let s = transition(&reviewing, Transition::SelfReviewReady { verdicts: nit }).unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        ));

        // A genuine worth-fixing finding likewise parks on the picker.
        let s = transition(
            &reviewing,
            Transition::SelfReviewReady {
                verdicts: verdicts(),
            },
        )
        .unwrap();
        assert!(matches!(
            s,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        ));
    }

    /// The parked hand-off states a question can be asked from.
    fn question_entry_states() -> Vec<CardState> {
        vec![
            CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() }),
            CardState::AwaitingReview(ReviewSub::ReadyForReview),
            CardState::AwaitingReview(ReviewSub::ReadyForPr),
            CardState::AwaitingReview(ReviewSub::ValidationFailed {
                attempt: 3,
                output: "boom".into(),
            }),
            CardState::PrReview(PrReviewSub::Idle),
            CardState::ReadyToMerge,
        ]
    }

    fn ask(s: &CardState) -> Result<CardState> {
        transition(
            s,
            Transition::AskQuestion {
                question: "why this approach?".into(),
            },
        )
    }

    #[test]
    fn ask_question_wraps_exactly_the_parked_handoff_states() {
        for s in question_entry_states() {
            let wrapped = ask(&s).unwrap();
            match &wrapped {
                CardState::Answering { previous, question } => {
                    assert_eq!(**previous, s, "the asked-from state rides inside");
                    assert_eq!(question, "why this approach?");
                }
                other => panic!("expected Answering, got {other:?}"),
            }
            // Wrapping, not replacing: the card stays in its column.
            assert_eq!(wrapped.column(), s.column());
        }

        // Illegal everywhere else: unstarted, running, mid-question, terminal.
        for s in [
            CardState::StartingBlock,
            CardState::Designing(DesignSub::Running),
            CardState::Investigating(RunSub::Running),
            CardState::Implementing(RunSub::Running),
            CardState::Implementing(RunSub::Intervention(intervention())),
            CardState::AwaitingReview(ReviewSub::Reviewing),
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts: vec![] }),
            CardState::AwaitingReview(ReviewSub::Validating { attempt: 1 }),
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::Concluded {
                conclusion: "c".into(),
            },
            CardState::Answering {
                previous: Box::new(CardState::ReadyToMerge),
                question: "q".into(),
            },
            CardState::Done,
        ] {
            assert!(ask(&s).is_err(), "{s:?} must not accept AskQuestion");
        }
    }

    #[test]
    fn question_answered_and_cancel_restore_the_asked_from_state() {
        for s in question_entry_states() {
            let wrapped = ask(&s).unwrap();
            let answered = transition(&wrapped, Transition::QuestionAnswered).unwrap();
            assert_eq!(answered, s, "answering must restore the exact state");
            let cancelled = transition(&wrapped, Transition::Cancel).unwrap();
            assert_eq!(cancelled, s, "cancelling must restore the exact state");
        }
        // QuestionAnswered means nothing outside a question run.
        assert!(transition(&CardState::ReadyToMerge, Transition::QuestionAnswered).is_err());
    }

    #[test]
    fn a_faulted_question_retries_as_a_question() {
        // The crash/Retry path that motivated the state: the fault wraps
        // Answering (not the borrowed phase), and Retry re-enters Answering —
        // never a write run against the parked work.
        for s in question_entry_states() {
            let wrapped = ask(&s).unwrap();
            let failed = transition(
                &wrapped,
                Transition::AgentError {
                    message: "boom".into(),
                },
            )
            .unwrap();
            match &failed {
                CardState::Failed { previous, .. } => {
                    assert_eq!(**previous, wrapped, "the fault wraps Answering")
                }
                other => panic!("expected Failed, got {other:?}"),
            }
            assert_eq!(
                failed.column(),
                s.column(),
                "column stable through the fault"
            );
            let retried = transition(&failed, Transition::Retry).unwrap();
            assert_eq!(retried, wrapped, "Retry restores the question run");
            let answered = transition(&retried, Transition::QuestionAnswered).unwrap();
            assert_eq!(answered, s);
        }
    }
}

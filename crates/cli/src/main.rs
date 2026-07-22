//! Headless test harness for usine-core.
//!
//! Modes:
//!   usine-cli                  — drive a card through the whole pipeline with the simulator
//!   usine-cli github           — live GitHub forge test (creates a throwaway repo)
//!   usine-cli real-plan <dir> <task...>  — run a real `claude` plan over <dir>

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use usine_core::{
    spawn_executor, AppSettings, Card, CardState, DesignSub, Effort, ExecutorCommand,
    ExecutorConfig, ExecutorEventKind, Forge, GhForge, GitOps, ModelSpec, PrReviewSub, Project,
    Provider, RealGit, ReviewSub, RunConfig, RunMode, SimFactory, SimForge, SimGit, Store,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,usine_core=info")
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("github") => github_smoke().await,
        Some("real-e2e") => real_e2e().await,
        Some("inspect-db") => {
            let path = PathBuf::from(args.get(1).expect("usage: usine-cli inspect-db <path>"));
            inspect_db(path)
        }
        Some("recover-db") => {
            let current = PathBuf::from(
                args.get(1)
                    .expect("usage: usine-cli recover-db <current.db> <backup.db>"),
            );
            let backup = PathBuf::from(
                args.get(2)
                    .expect("usage: usine-cli recover-db <current.db> <backup.db>"),
            );
            recover_db(current, backup)
        }
        Some("rebuild-db") => {
            let path = PathBuf::from(
                args.get(1)
                    .expect("usage: usine-cli rebuild-db <db>  (run with Usine closed)"),
            );
            rebuild_db(path)
        }
        Some("reset-to-pr-open") => {
            let path = PathBuf::from(
                args.get(1)
                    .expect("usage: usine-cli reset-to-pr-open <db> <card-id-prefix-or-title>"),
            );
            let needle = args
                .get(2)
                .expect("usage: usine-cli reset-to-pr-open <db> <card-id-prefix-or-title>");
            reset_to_pr_open(path, needle)
        }
        Some("real-plan") => {
            let dir = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            let task = if args.len() > 2 {
                args[2..].join(" ")
            } else {
                "Summarize what this project does and propose one improvement.".to_string()
            };
            real_plan(dir, task).await
        }
        _ => sim_pipeline().await,
    }
}

// ---------------------------------------------------------------------------
// Inspect / migrate a database file (opens it, which migrates old cards, then
// lists them). Run on a COPY — opening rewrites v1 cards to v2.
// ---------------------------------------------------------------------------

fn inspect_db(path: PathBuf) -> anyhow::Result<()> {
    println!("opening {}", path.display());
    let store = match Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open failed (Display): {e}");
            eprintln!("open failed (Debug):   {e:?}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                eprintln!("  caused by: {s} | {s:?}");
                src = std::error::Error::source(s);
            }
            return Err(e.into());
        }
    };
    match store.list_projects() {
        Ok(projects) => {
            println!("{} project(s):", projects.len());
            for p in &projects {
                println!(
                    "  [{}] {} — {}",
                    &p.id.to_string()[..8],
                    p.name,
                    p.path.display()
                );
            }
        }
        Err(e) => {
            eprintln!("list_projects FAILED to decode: {e}");
            eprintln!("  (project records are stored under an older layout)");
        }
    }
    let cards = store.list_cards()?;
    println!("{} card(s):", cards.len());
    for c in &cards {
        println!(
            "  [{}] {:<40} — {} (col: {:?})",
            &c.id.to_string()[..8],
            c.title,
            c.state.status_label(),
            c.state.column(),
        );
    }
    Ok(())
}

/// One-time recovery: force a single card back to `PrReview(Idle)` ("PR open"),
/// so the user can re-run the PR-comment fix flow (which now isolates work in a
/// worktree and pushes). Matches exactly one card by id-prefix or title substring;
/// refuses if zero or many match. Clears the stale worktree pointer so the next
/// fix re-establishes a fresh isolated worktree. Run with Usine closed (the DB is
/// single-writer).
fn reset_to_pr_open(path: PathBuf, needle: &str) -> anyhow::Result<()> {
    let store = match Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not open {} ({e}).", path.display());
            eprintln!("Is Usine running? Quit it first — the database allows only one writer.");
            return Err(e.into());
        }
    };
    let needle_lc = needle.to_lowercase();
    let cards = store.list_cards()?;
    let matches: Vec<_> = cards
        .iter()
        .filter(|c| {
            c.id.to_string().starts_with(needle) || c.title.to_lowercase().contains(&needle_lc)
        })
        .collect();
    if matches.is_empty() {
        anyhow::bail!("no card matches “{needle}”");
    }
    if matches.len() > 1 {
        eprintln!("“{needle}” is ambiguous — {} cards match:", matches.len());
        for c in &matches {
            eprintln!("  [{}] {}", &c.id.to_string()[..8], c.title);
        }
        anyhow::bail!("refusing to act on multiple cards — narrow the id/title");
    }
    let card = matches[0];
    println!(
        "before: [{}] {} — {} (col {:?})",
        &card.id.to_string()[..8],
        card.title,
        card.state.status_label(),
        card.state.column(),
    );
    let id = card.id;
    let updated = store.mutate_card(id, |c| {
        c.state = CardState::PrReview(PrReviewSub::Idle);
        // Drop the stale worktree pointer so the next fix re-establishes a clean,
        // isolated worktree on the branch (see Executor::ensure_pr_worktree).
        c.worktree_path = None;
        c.updated_at = usine_core::now_millis();
        Ok(())
    })?;
    println!(
        "after:  [{}] {} — {} (col {:?})",
        &updated.id.to_string()[..8],
        updated.title,
        updated.state.status_label(),
        updated.state.column(),
    );
    println!(
        "done — reopen Usine, open this card, and use “Read review comments” to redo the fixes."
    );
    Ok(())
}

/// Merge any cards present in `backup` but missing from `current` (e.g. a record
/// that drifted out of `current` and won't decode) into `current`, in the current
/// layout. Mutates `current` in place — pass a copy.
fn recover_db(current: PathBuf, backup: PathBuf) -> anyhow::Result<()> {
    let cur = Store::open(&current)?;
    let bak = Store::open(&backup)?;
    let cur_cards = cur.list_cards()?;
    let cur_ids: std::collections::HashSet<_> = cur_cards.iter().map(|c| c.id).collect();
    println!("current: {} decodable card(s)", cur_cards.len());

    let mut added = 0;
    for c in bak.list_cards()? {
        if !cur_ids.contains(&c.id) {
            // The card's original id may still exist in `current` as an
            // undecodable ("quarantined") record, so re-inserting under it would
            // collide. Give the recovered card a fresh id to sidestep that.
            let old_id = c.id;
            let mut c = c;
            c.id = uuid::Uuid::new_v4();
            println!(
                "  recovering from backup: [{}] {} (re-id {})",
                &old_id.to_string()[..8],
                c.title,
                &c.id.to_string()[..8],
            );
            cur.upsert_card(&c)?;
            if let Ok(Some(plan)) = bak.get_plan(old_id) {
                let _ = cur.save_plan(c.id, &plan);
            }
            added += 1;
        }
    }
    println!(
        "recovered {added} card(s); current now has {} card(s)",
        cur.list_cards()?.len()
    );
    Ok(())
}

/// One-off maintenance: rebuild the database in place so legacy values that no
/// longer round-trip (e.g. a pre-`Cost` `cost_usd` float like `9.708580000000001`,
/// which the current `Cost` re-emits as `9.71`) stop blocking native_db's
/// value-matched writes — the failure mode that leaves an interrupted card frozen
/// in a running state that no restart can reconcile. Backs the original up first,
/// rebuilds into a temp file, then swaps it in. Run with Usine CLOSED (the DB
/// allows a single writer).
fn rebuild_db(path: PathBuf) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("no database at {}", path.display());
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let backup = PathBuf::from(format!("{}.bak-before-rebuild-{secs}", path.display()));
    let tmp = PathBuf::from(format!("{}.rebuilt-{secs}", path.display()));
    let _ = std::fs::remove_file(&tmp);

    std::fs::copy(&path, &backup)?;
    println!("backed up → {}", backup.display());

    let report = match usine_core::rebuild_database(&path, &tmp) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("rebuild failed: {e}");
            eprintln!("Is Usine running? Quit it first — the database allows only one writer.");
            return Err(e.into());
        }
    };
    println!("rebuilt {} →", path.display());
    for (name, copied, skipped) in &report {
        let note = if *skipped > 0 {
            format!("  ({skipped} undecodable skipped)")
        } else {
            String::new()
        };
        println!("  {name:<18} {copied} copied{note}");
    }

    // Swap the rebuilt file in; the original is preserved at `backup`.
    std::fs::rename(&tmp, &path)?;
    println!(
        "\nswapped in the rebuilt database (original kept at {}).",
        backup.display()
    );
    println!("Reopen Usine: the stuck card reconciles to Failed → click Resume.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Sim pipeline (no agents, no network)
// ---------------------------------------------------------------------------

async fn sim_pipeline() -> anyhow::Result<()> {
    let store = Store::open_in_memory()?;
    let settings = AppSettings::default();
    // Use a throwaway dir, not the real cwd: parts of the executor inspect the
    // project path with real git regardless of backend, and pointing them at the
    // developer's usually-dirty usine repo would make the smoke run flaky.
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let proj_dir = std::env::temp_dir().join(format!("usine-sim-smoke-{secs}"));
    std::fs::create_dir_all(&proj_dir)?;
    let project = Project::new("smoke", proj_dir, settings.new_project_config());
    store.upsert_project(&project)?;

    let card = Card::new(
        project.id,
        "Smoke test card",
        "Run the end-to-end smoke test.",
        project.config.new_card_config(),
    );
    store.upsert_card(&card)?;
    let card_id = card.id;
    // Skip the plan phase: the simulator's sample plan always carries unanswered
    // `usine-questions` (so a sim-started card can't be approved), and this smoke
    // test is about exercising the post-implement review→PR→merge pipeline.
    store.set_skip_plan(card_id, true)?;

    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    println!("→ Start");
    exec.send(ExecutorCommand::Start { card_id });

    // Only drive on *state* changes: the executor emits `CardUpdated` for
    // field-only edits too (branch rename, worktree clear), and acting on those
    // would double-dispatch the same command.
    let mut last_label = String::new();
    // Trigger the comment triage once, from the PR gate.
    let mut comments_fetched = false;
    while let Some(evt) = rx.next().await {
        match evt.kind {
            ExecutorEventKind::Transcript { line, .. } => println!("   {line}"),
            ExecutorEventKind::Toast { severity, message } => {
                println!("   [{severity:?}] {message}")
            }
            // The PR gate takes TWO exclusive commands (mark the draft ready,
            // then triage the comments), and the dispatcher silently drops an
            // exclusive command sent while another holds the card (that's the
            // right call for a double click — see `ExecutorCommand::is_exclusive`).
            // So drive this phase serially: each `busy: false` marks a command
            // fully released, and only then is the next one sent. Reacting to
            // `CardUpdated` instead would race the sender's still-held claim.
            ExecutorEventKind::CardBusy { busy: false } => {
                if let Ok(card) = store.get_card(card_id) {
                    if matches!(card.state, CardState::PrReview(PrReviewSub::Idle)) {
                        match card.pr.as_ref().map(|p| p.state.as_str()) {
                            Some("draft") => exec.send(ExecutorCommand::MarkPrReady { card_id }),
                            Some(_) if !comments_fetched => {
                                comments_fetched = true;
                                exec.send(ExecutorCommand::FetchComments { card_id });
                            }
                            _ => {}
                        }
                    }
                }
            }
            ExecutorEventKind::Reviewers { .. } => {}
            ExecutorEventKind::CardUpdated(card) => {
                let label = card.state.status_label().to_string();
                if label == last_label {
                    continue;
                }
                last_label = label;
                println!("◆ {}", card.state.status_label());
                match &card.state {
                    CardState::Designing(DesignSub::Intervention(_)) => {
                        exec.send(ExecutorCommand::Answer {
                            card_id,
                            text: "Simplicity".into(),
                        });
                    }
                    CardState::Designing(DesignSub::AwaitingApproval { .. }) => {
                        exec.send(ExecutorCommand::ApprovePlan { card_id });
                    }
                    // The executor auto-starts the self-review pass when the
                    // implementation lands, so the gate needs no push from here;
                    // the `SelectingFixes` arm below applies all its findings.
                    CardState::AwaitingReview(ReviewSub::ReadyForReview) => {}
                    CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                        let ids = verdicts.iter().map(|v| v.comment.id).collect();
                        exec.send(ExecutorCommand::ApplySelfFixes {
                            card_id,
                            selected_comment_ids: ids,
                            note: String::new(),
                        });
                    }
                    CardState::AwaitingReview(ReviewSub::ReadyForPr) => {
                        exec.send(ExecutorCommand::CreatePr {
                            card_id,
                            branch: "usine/smoke-test".into(),
                            title: "Smoke test card".into(),
                            body: "Automated smoke test.".into(),
                            reviewer: None,
                            draft: true,
                        })
                    }
                    // The PR gate is driven from the `CardBusy` arm above: mark
                    // ready, then fetch — one exclusive command at a time.
                    CardState::PrReview(PrReviewSub::Idle) => {}
                    CardState::PrReview(PrReviewSub::SelectingFixes { verdicts }) => {
                        // Fix only the ones triaged as worth fixing; the rest get a reply.
                        let ids = verdicts
                            .iter()
                            .filter(|v| v.selected)
                            .map(|v| v.comment.id)
                            .collect();
                        exec.send(ExecutorCommand::ApplyFixes {
                            card_id,
                            selected_comment_ids: ids,
                            note: String::new(),
                        });
                    }
                    CardState::ReadyToMerge => exec.send(ExecutorCommand::Merge {
                        card_id,
                        delete_branch: true,
                    }),
                    CardState::Done => {
                        println!("✔ Done — smoke test complete");
                        break;
                    }
                    CardState::Failed { message, .. } => {
                        println!("✗ Failed: {message}");
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
            // CRUD echo events aren't produced by the CLI's sim pipeline.
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Live GitHub forge test
// ---------------------------------------------------------------------------

async fn github_smoke() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let owner = sh("gh", &["api", "user", "--jq", ".login"], &cwd)?
        .trim()
        .to_string();
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let name = format!("usine-sandbox-{secs}");
    let repo = std::env::temp_dir().join(&name);
    std::fs::create_dir_all(&repo)?;

    println!("• creating local repo at {}", repo.display());
    sh("git", &["init", "-b", "main"], &repo)?;
    std::fs::write(repo.join("README.md"), "# usine sandbox\n")?;
    sh("git", &["add", "-A"], &repo)?;
    sh("git", &["commit", "-m", "init"], &repo)?;

    println!("• creating GitHub repo {owner}/{name} (private) and pushing");
    sh(
        "gh",
        &[
            "repo",
            "create",
            &name,
            "--private",
            "--source",
            ".",
            "--push",
        ],
        &repo,
    )?;

    let git = RealGit;
    let forge = GhForge;

    let branch = "usine/test-change";
    let worktree = repo.join(".usine/worktrees/test");
    println!("• creating worktree + branch {branch}");
    git.create_worktree(&repo, branch, &worktree, "main")
        .await?;
    std::fs::write(worktree.join("CHANGE.md"), "An automated change.\n")?;
    git.commit_all(&worktree, "usine: test change").await?;
    git.push(&worktree, branch).await?;

    println!("• opening PR");
    let pr = forge
        .create_pr(
            &repo,
            "Test change from usine",
            "This PR was created by the usine GitHub integration test.",
            "main",
            branch,
            None,
            false,
        )
        .await?;
    println!("  PR #{} — {}", pr.number, pr.url);

    let comments = forge.fetch_comments(&repo, pr.number).await?;
    println!("• fetched {} review comment(s)", comments.len());

    println!("• merging PR");
    forge.merge(&repo, pr.number).await?;
    println!("  merged ✔");

    // Cleanup (best-effort: requires the delete_repo scope, which may be absent).
    match sh(
        "gh",
        &["repo", "delete", &format!("{owner}/{name}"), "--yes"],
        &repo,
    ) {
        Ok(_) => println!("• deleted GitHub repo {owner}/{name}"),
        Err(e) => println!(
            "• left repo in place (auto-delete failed: {}). Delete manually:\n    https://github.com/{owner}/{name}/settings",
            e.to_string().lines().next().unwrap_or("")
        ),
    }
    let _ = std::fs::remove_dir_all(&repo);
    println!("\n✔ GitHub integration test complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Full combined real run: claude plan + implement, real git + gh, through the
// executor. Creates a throwaway repo and stops once the PR is open.
// ---------------------------------------------------------------------------

async fn real_e2e() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let owner = sh("gh", &["api", "user", "--jq", ".login"], &cwd)?
        .trim()
        .to_string();
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let name = format!("usine-e2e-{secs}");
    let repo = std::env::temp_dir().join(&name);
    std::fs::create_dir_all(&repo)?;

    println!("• creating GitHub repo {owner}/{name}");
    sh("git", &["init", "-b", "main"], &repo)?;
    std::fs::write(repo.join("README.md"), "# usine e2e\n")?;
    sh("git", &["add", "-A"], &repo)?;
    sh("git", &["commit", "-m", "init"], &repo)?;
    sh(
        "gh",
        &[
            "repo",
            "create",
            &name,
            "--private",
            "--source",
            ".",
            "--push",
        ],
        &repo,
    )?;

    let store = Store::open_in_memory()?;
    let settings = AppSettings::default();
    let project = Project::new(name.clone(), repo.clone(), settings.new_project_config());
    store.upsert_project(&project)?;

    let mut card = Card::new(
        project.id,
        "Add a greeting file",
        "Add a file named HELLO.md containing a single friendly one-line greeting. Keep it minimal.",
        project.config.new_card_config(),
    );
    // Keep the test cheap.
    card.config.plan = ModelSpec::new("sonnet", Effort::Low);
    card.config.implement = ModelSpec::new("sonnet", Effort::Low);
    store.upsert_card(&card)?;
    let card_id = card.id;

    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(usine_core::RealFactory),
        forge: Arc::new(GhForge),
        git: Arc::new(RealGit),
    });

    println!("→ Start\n");
    exec.send(ExecutorCommand::Start { card_id });

    // Drive on state changes only (see the sim pipeline for why).
    let mut last_label = String::new();
    while let Some(evt) = rx.next().await {
        match evt.kind {
            ExecutorEventKind::Transcript { line, .. } => println!("   {line}"),
            ExecutorEventKind::Toast { severity, message } => {
                println!("   [{severity:?}] {message}")
            }
            ExecutorEventKind::Reviewers { .. } => {}
            ExecutorEventKind::CardUpdated(card) => {
                let label = card.state.status_label().to_string();
                if label == last_label {
                    continue;
                }
                last_label = label;
                println!("◆ {}", card.state.status_label());
                match &card.state {
                    CardState::Designing(DesignSub::AwaitingApproval { .. }) => {
                        println!("→ ApprovePlan");
                        exec.send(ExecutorCommand::ApprovePlan { card_id });
                    }
                    // Pre-PR review gate: the agent's work is already committed in
                    // the worktree, so just skip self-review and open the (draft) PR.
                    CardState::AwaitingReview(ReviewSub::ReadyForReview) => {
                        println!("→ SkipReview");
                        exec.send(ExecutorCommand::SkipReview { card_id });
                    }
                    // The executor auto-starts the self-review on implement-done;
                    // this run deliberately skips it (a real review run costs real
                    // tokens), so cancel — the card parks back at `ReadyForReview`
                    // and the SkipReview arm above fires on the next label change.
                    CardState::AwaitingReview(ReviewSub::Reviewing) => {
                        println!("→ Cancel (skipping the auto self-review)");
                        exec.send(ExecutorCommand::Cancel { card_id });
                    }
                    CardState::AwaitingReview(ReviewSub::ReadyForPr) => {
                        println!("→ CreatePr");
                        exec.send(ExecutorCommand::CreatePr {
                            card_id,
                            branch: "usine/greeting".into(),
                            title: "Add a greeting file".into(),
                            body: "Opened by the usine end-to-end test.".into(),
                            reviewer: None,
                            draft: true,
                        });
                    }
                    CardState::PrReview(PrReviewSub::Idle) => {
                        let url = card.pr.as_ref().map(|p| p.url.clone()).unwrap_or_default();
                        println!("\n✔ End-to-end complete — draft PR opened: {url}");
                        break;
                    }
                    CardState::Failed { message, .. } => {
                        println!("\n✗ Failed: {message}");
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    println!("(left repo in place — delete manually: https://github.com/{owner}/{name}/settings)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Real Claude plan run
// ---------------------------------------------------------------------------

async fn real_plan(dir: PathBuf, task: String) -> anyhow::Result<()> {
    use usine_core::{AgentEvent, AgentProvider, ClaudeProvider};

    println!("• running a real `claude` plan over {}\n", dir.display());
    let cfg = RunConfig {
        provider: Provider::Claude,
        project_dir: dir,
        spec: ModelSpec::new("sonnet", Effort::Medium),
        mode: RunMode::Plan,
        session_id: uuid_v4(),
        prompt: task,
        extra_prompt: None,
        resume_session: None,
        attachments: Vec::new(),
    };

    let handle = ClaudeProvider.start(cfg).await?;
    let mut events = handle.events;
    while let Some(evt) = events.next().await {
        match evt {
            AgentEvent::Started { session_id } => println!("● session {session_id}"),
            AgentEvent::Progress { text } => println!("   {text}"),
            AgentEvent::NeedsInput { question, .. } => println!("   ? {question}"),
            AgentEvent::PlanReady { plan } => {
                println!("\n=== PLAN ===\n{plan}\n============");
                break;
            }
            AgentEvent::Done {
                result, cost_usd, ..
            } => {
                println!("\n✔ done (${cost_usd:.4})\n{result}");
                break;
            }
            AgentEvent::Error { message } => {
                println!("\n✗ {message}");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn sh(cmd: &str, args: &[&str], dir: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn uuid_v4() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

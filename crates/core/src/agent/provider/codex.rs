//! Codex provider: drives `codex exec --json`.
//!
//! Relies on the user's existing `codex login` (ChatGPT OAuth) — no API key is
//! stored. Runs are one-shot and non-interactive by construction: `codex exec`
//! never prompts for approval (the `-a/--ask-for-approval` flag exists only on
//! the root `codex` command — the exec subcommand rejects it). Codex has no
//! `ExitPlanMode`, so the pump tracks the last *agent message* and reports it
//! as the run result; the executor's plan-phase fallback then uses that text
//! as the plan. A `resume_session` re-enters the thread via `codex exec
//! resume`, which inherits the original run's model, effort, and sandbox.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::agent::events::{AgentEvent, RunControl};
use crate::domain::model::{supported_efforts, Provider};
use crate::error::{CoreError, Result};

use super::stream::{codex_agent_message, parse_codex_line};
use super::{AgentProvider, RunConfig, RunHandle, RunMode};

/// Build argv for `codex exec`, excluding the prompt (appended by `start`). The
/// working directory is set on the command, so no `-C` is needed.
///
/// Always ends with `--` so everything the caller appends is read as a
/// positional. That terminator is load-bearing twice over:
///
/// * `-i/--image` is declared `<FILE>...` — *greedy* multi-value — so an
///   unterminated `-i <path>` swallows the prompt (and, on resume, the session
///   id) as further image paths. codex then finds no prompt positional, falls
///   back to stdin, and dies with "Reading prompt from stdin... No prompt
///   provided via stdin." Verified on codex 0.144.6 and 0.145.0.
/// * A prompt starting with `-` (a description opening on a markdown bullet)
///   would otherwise be parsed as a flag: "error: unexpected argument '- '".
pub fn build_args(cfg: &RunConfig) -> Vec<String> {
    // Image attachments ride as `-i` flags: unlike Claude (whose Read tool is
    // vision-capable), Codex can only see images attached to the message. Also
    // passed on resume — documented there too, and it covers a session that
    // was killed before its first turn saved anything.
    let image_args = cfg
        .attachments
        .iter()
        .filter(|p| is_image(p))
        .flat_map(|p| ["-i".to_string(), p.to_string_lossy().into_owned()]);

    // Unlike Claude Code, Codex doesn't clamp an unsupported effort — passing
    // `xhigh` to a model that lacks it can misbehave — so clamp to the model's
    // ceiling ourselves.
    let effort = cfg
        .spec
        .effort
        .clamp_to(supported_efforts(Provider::Codex, &cfg.spec.model));

    // Resume re-enters an existing thread as a fresh turn. The sandbox is
    // restored from the recorded session, but the model is NOT: verified
    // against codex 0.144.6, resuming without `-m` switches the thread to the
    // CLI's default model ("session was recorded with X but is resuming with
    // Y"), so the card's model + effort are re-pinned explicitly.
    if let Some(session) = &cfg.resume_session {
        let mut args = vec![
            "exec".to_string(),
            "resume".into(),
            "--json".into(),
            "--model".into(),
            cfg.spec.model.clone(),
            "-c".into(),
            format!("model_reasoning_effort={}", effort.codex_value()),
        ];
        args.extend(image_args);
        args.push("--".into());
        args.push(session.clone());
        return args;
    }
    let mut args = vec![
        "exec".to_string(),
        "--json".into(),
        "--model".into(),
        cfg.spec.model.clone(),
        "-c".into(),
        format!("model_reasoning_effort={}", effort.codex_value()),
    ];

    match cfg.mode {
        // Read-only phases (planning, self-review, comment triage, Q&A,
        // investigation).
        RunMode::Plan
        | RunMode::Review
        | RunMode::Triage
        | RunMode::Question
        | RunMode::Investigate => {
            args.push("--sandbox".into());
            args.push("read-only".into());
        }
        RunMode::Implement | RunMode::ApplyFixes => {
            args.push("--sandbox".into());
            args.push("workspace-write".into());
            // Claude's write phases run unsandboxed, so extend the same trust:
            // without network the agent can't fetch deps (cargo, npm, …). The
            // run is still confined to the card's isolated worktree.
            args.push("-c".into());
            args.push("sandbox_workspace_write.network_access=true".into());
            // A card worktree's real git dir lives in the MAIN repo's `.git`
            // (a worktree's `.git` is just a pointer file), outside the
            // sandbox's nominal writable roots. Codex 0.144.6 resolves this
            // itself (verified live: agent-run `git add` works either way), but
            // the explicit writable root is harmless and covers versions
            // without that handling.
            if let Some(git_dir) = linked_git_dir(&cfg.project_dir) {
                args.push("--add-dir".into());
                args.push(git_dir.to_string_lossy().into_owned());
            }
        }
    }

    args.extend(image_args);
    args.push("--".into());
    args
}

/// For a git *worktree* checkout, resolve the main repository's `.git`
/// directory from the worktree's `.git` pointer file (`gitdir: <path>`, which
/// points at `<repo>/.git/worktrees/<name>`). Returns the enclosing `.git` so
/// one `--add-dir` covers the whole git dir (objects, refs, and the per-
/// worktree subdir nested inside it). `None` for a regular checkout (`.git` is
/// a directory) or anything unparseable.
fn linked_git_dir(project_dir: &Path) -> Option<PathBuf> {
    let dotgit = project_dir.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = PathBuf::from(content.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_relative() {
        project_dir.join(gitdir)
    } else {
        gitdir
    };
    gitdir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".git"))
        .map(Path::to_path_buf)
        .or(Some(gitdir))
}

/// Whether `path` looks like an image `codex exec -i` can attach.
fn is_image(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
    })
}

pub struct CodexProvider;

#[async_trait]
impl AgentProvider for CodexProvider {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        let mut args = build_args(&cfg);
        args.push(cfg.full_prompt());

        let mut child = Command::new("codex")
            .args(&args)
            .current_dir(&cfg.project_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                CoreError::provider(format!("failed to spawn `codex` (is it installed?): {e}"))
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::provider("codex stdout unavailable"))?;
        let stderr = child.stderr.take();

        let (evt_tx, evt_rx) = mpsc::unbounded::<AgentEvent>();
        let (ctl_tx, ctl_rx) = mpsc::unbounded::<RunControl>();

        tokio::spawn(pump(child, stdout, stderr, evt_tx, ctl_rx));

        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

async fn pump(
    mut child: Child,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<RunControl>,
) {
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut buf).await;
        }
        buf
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut saw_terminal = false;
    let mut cancelled = false;
    let mut last_message = String::new();
    // See claude.rs: avoid spinning once the control channel closes.
    let mut ctl_open = true;

    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    // Track the last *agent message* from the raw line, not the
                    // Progress stream: reasoning summaries (and command lines)
                    // also flow as Progress and would clobber the result text
                    // that plan/verdict/commit parsing depends on.
                    if let Some(text) = codex_agent_message(&line) {
                        last_message = text;
                    }
                    if let Ok(events) = parse_codex_line(&line) {
                        for evt in events {
                            match evt {
                                // Codex's terminal event carries no text; substitute the
                                // last agent message so plan/result text isn't lost.
                                //
                                // Forward only the FIRST terminal event: a real
                                // failure emits both a stream-level `error` and a
                                // `turn.failed` for the same fault (seen live on
                                // 0.144.6), and a second Error/Done would hit the
                                // executor after the card already left its running
                                // state.
                                AgentEvent::Done { cost_usd, usage, .. } if !saw_terminal => {
                                    saw_terminal = true;
                                    let _ = evt_tx.unbounded_send(AgentEvent::Done {
                                        result: last_message.clone(),
                                        cost_usd,
                                        usage,
                                    });
                                }
                                AgentEvent::Error { .. } if !saw_terminal => {
                                    saw_terminal = true;
                                    let _ = evt_tx.unbounded_send(evt);
                                }
                                AgentEvent::Done { .. } | AgentEvent::Error { .. } => {}
                                other => { let _ = evt_tx.unbounded_send(other); }
                            }
                        }
                    }
                }
                _ => break,
            },
            ctl = ctl_rx.next(), if ctl_open => match ctl {
                Some(RunControl::Cancel) | Some(RunControl::Interrupt) => {
                    cancelled = true;
                    let _ = child.start_kill();
                    break;
                }
                Some(RunControl::Answer { .. }) => {}
                None => ctl_open = false,
            },
        }
    }

    let status = child.wait().await.ok();
    let stderr_text = stderr_task.await.unwrap_or_default();

    if !saw_terminal && !cancelled {
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        let detail = stderr_text.trim();
        let message = if detail.is_empty() {
            format!("codex exited with code {code} before producing a result")
        } else {
            format!("codex failed (code {code}): {detail}")
        };
        let _ = evt_tx.unbounded_send(AgentEvent::Error { message });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Effort, ModelSpec, Provider};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn cfg(mode: RunMode, effort: Effort) -> RunConfig {
        RunConfig {
            provider: Provider::Codex,
            project_dir: PathBuf::from("/repo/wt"),
            spec: ModelSpec::new("gpt-5-codex", effort),
            mode,
            session_id: Uuid::nil(),
            prompt: "do it".into(),
            extra_prompt: None,
            resume_session: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn plan_is_read_only_without_approval_flag() {
        let args = build_args(&cfg(RunMode::Plan, Effort::Medium));
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        // `-a/--ask-for-approval` only exists on the root `codex` command; the
        // exec subcommand rejects it ("unexpected argument"), so it must never
        // appear. `codex exec` is non-interactive by construction anyway.
        assert!(!args.iter().any(|a| a == "-a" || a == "--ask-for-approval"));
        assert!(args.iter().any(|a| a == "model_reasoning_effort=medium"));
        // Read-only phases must not get the write-phase trust extensions.
        assert!(!args
            .iter()
            .any(|a| a.contains("network_access") || a == "--add-dir"));
    }

    #[test]
    fn question_is_read_only() {
        let args = build_args(&cfg(RunMode::Question, Effort::Medium));
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(!args
            .iter()
            .any(|a| a.contains("network_access") || a == "--add-dir"));
    }

    #[test]
    fn investigate_is_read_only_sandboxed() {
        let args = build_args(&cfg(RunMode::Investigate, Effort::Medium));
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(!args
            .iter()
            .any(|a| a.contains("network_access") || a == "--add-dir"));
    }

    #[test]
    fn implement_is_workspace_write_with_network_and_effort_clamped() {
        // gpt-5-codex tops out at `high`, so the `max` tier clamps down to it.
        let args = build_args(&cfg(RunMode::Implement, Effort::Max));
        assert!(args
            .windows(2)
            .any(|w| w == ["--sandbox", "workspace-write"]));
        assert!(args.iter().any(|a| a == "model_reasoning_effort=high"));
        // Parity with Claude's unsandboxed write phases: deps need network.
        assert!(args
            .windows(2)
            .any(|w| w == ["-c", "sandbox_workspace_write.network_access=true"]));
    }

    #[test]
    fn resume_reenters_the_thread_and_repins_the_model() {
        let mut c = cfg(RunMode::Implement, Effort::Medium);
        c.resume_session = Some("thread-123".into());
        let args = build_args(&c);
        // Model + effort must be re-pinned: codex resume otherwise switches
        // the thread to the CLI's default model (observed on 0.144.6). The
        // sandbox is restored from the recorded session, so no --sandbox.
        assert_eq!(
            args,
            vec![
                "exec",
                "resume",
                "--json",
                "--model",
                "gpt-5-codex",
                "-c",
                "model_reasoning_effort=medium",
                "--",
                "thread-123"
            ]
        );
        assert!(!args.iter().any(|a| a == "--sandbox"));
    }

    #[test]
    fn image_attachments_ride_as_i_flags() {
        let mut c = cfg(RunMode::Implement, Effort::Medium);
        c.attachments = vec![
            PathBuf::from("/att/mock.PNG"),
            PathBuf::from("/att/notes.txt"),
            PathBuf::from("/att/shot.jpeg"),
        ];
        let args = build_args(&c);
        // Images attach via -i; non-images only travel in the prompt note.
        assert!(args.windows(2).any(|w| w == ["-i", "/att/mock.PNG"]));
        assert!(args.windows(2).any(|w| w == ["-i", "/att/shot.jpeg"]));
        assert!(!args.iter().any(|a| a == "/att/notes.txt"));
        // ...and the `--` closing the greedy image list is the LAST arg, so the
        // prompt `start` appends lands as a positional instead of being eaten as
        // one more image path (which left codex prompt-less on stdin).
        assert_eq!(args.last().map(String::as_str), Some("--"));

        // On resume the images still attach, before the terminator and the
        // trailing session id — which `-i` would otherwise swallow too.
        c.resume_session = Some("t-1".into());
        let args = build_args(&c);
        assert!(args.windows(2).any(|w| w == ["-i", "/att/mock.PNG"]));
        assert_eq!(args.last().map(String::as_str), Some("t-1"));
        assert_eq!(args[args.len() - 2], "--");
    }

    /// Every mode must end its argv with `--`: the prompt is appended by
    /// `start`, and only a terminator guarantees codex reads it as the prompt
    /// positional rather than as a flag or as trailing `-i` fodder.
    #[test]
    fn every_mode_terminates_options_before_the_prompt() {
        for mode in [
            RunMode::Plan,
            RunMode::Implement,
            RunMode::ApplyFixes,
            RunMode::Review,
            RunMode::Triage,
            RunMode::Question,
            RunMode::Investigate,
        ] {
            let args = build_args(&cfg(mode, Effort::Medium));
            assert_eq!(
                args.last().map(String::as_str),
                Some("--"),
                "{mode:?} must end with the option terminator"
            );
        }
    }

    #[test]
    fn linked_git_dir_resolves_the_main_repos_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        // A regular checkout (`.git` directory) needs no extra writable root.
        let regular = tmp.path().join("regular");
        std::fs::create_dir_all(regular.join(".git")).unwrap();
        assert_eq!(linked_git_dir(&regular), None);
        // No `.git` at all: same.
        assert_eq!(linked_git_dir(tmp.path()), None);

        // A worktree's `.git` file points into `<repo>/.git/worktrees/<name>`;
        // the enclosing `.git` is what needs to be writable.
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}/repo/.git/worktrees/wt\n", tmp.path().display()),
        )
        .unwrap();
        assert_eq!(
            linked_git_dir(&wt),
            Some(tmp.path().join("repo").join(".git"))
        );
    }

    #[test]
    fn effort_is_clamped_per_model() {
        let mk = |model: &str, effort: Effort| RunConfig {
            provider: Provider::Codex,
            project_dir: PathBuf::from("/repo/wt"),
            spec: ModelSpec::new(model, effort),
            mode: RunMode::Implement,
            session_id: Uuid::nil(),
            prompt: "do it".into(),
            extra_prompt: None,
            resume_session: None,
            attachments: Vec::new(),
        };
        let effort_arg = |args: &[String]| {
            args.iter()
                .find_map(|a| a.strip_prefix("model_reasoning_effort=").map(str::to_owned))
                .unwrap()
        };

        // The current Codex generation honors `xhigh`...
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.3-codex", Effort::XHigh))),
            "xhigh"
        );
        // ...and `max` clamps to that ceiling.
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.3-codex", Effort::Max))),
            "xhigh"
        );
        // GPT-5.6 Sol and Terra add `max` and `ultra`.
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.6-sol", Effort::Ultra))),
            "ultra"
        );
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.6-terra", Effort::Max))),
            "max"
        );
        // Luna tops out at `max`.
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.6-luna", Effort::Ultra))),
            "max"
        );
        // Legacy / unknown model ids stay clamped to `high`.
        assert_eq!(
            effort_arg(&build_args(&mk("gpt-5.1-codex", Effort::XHigh))),
            "high"
        );
    }
}

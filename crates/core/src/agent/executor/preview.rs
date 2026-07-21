//! In-worktree preview: run the project's app straight from a card's worktree so
//! the user can test the change without the code ever leaving the worktree.
//!
//! The card's worktree already holds the committed work; a preview runs the
//! project's setup script (deps, an isolated per-worktree DB, free ports) and
//! then its `run_script` as a persistent process, surfacing the
//! clickable URLs. Nothing touches the user's main checkout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::*;
use crate::domain::config::{resolve_open_command, PreviewPort, ProjectConfig};
use crate::domain::model::{PreviewStatus, PreviewUrl};

/// Conventional setup/teardown script paths auto-detected in a worktree when the
/// project config names no explicit command (matches what the daedalus skill
/// generates and what `docker-compose.worktree.yml` expects).
const SETUP_CANDIDATES: &[&str] = &["setup-worktree.sh", "scripts/setup-worktree.sh"];
const TEARDOWN_CANDIDATES: &[&str] = &["teardown-worktree.sh", "scripts/teardown-worktree.sh"];

/// How long a `SIGTERM`ed preview tree gets to exit cleanly before `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_millis(800);

/// Outcome of the preview setup step: it either ran to completion, or a
/// concurrent [`Executor::stop_preview`] reaped it mid-run.
enum SetupOutcome {
    Completed,
    Stopped,
}

impl Executor {
    /// Atomically claim an entity's preview slot: one lock acquisition checks
    /// occupancy and reserves (`pid: None`) in the same breath, so two
    /// concurrent starts — a user click racing the write-run's `EnsurePreview`,
    /// or a double click — can never both pass an occupancy check and
    /// double-launch. Returns the claim's generation, which every later
    /// process registration and cleanup is keyed on; `None` means the slot is
    /// already taken (reserved or live).
    fn claim_preview(&self, id: Uuid) -> Option<Uuid> {
        let mut map = lock(&self.previews);
        if map.contains_key(&id) {
            return None;
        }
        let generation = Uuid::new_v4();
        map.insert(
            id,
            PreviewHandle {
                generation,
                pid: None,
            },
        );
        Some(generation)
    }

    /// Drop a preview slot iff it still belongs to `generation`. A concurrent
    /// stop (which removes the slot) or a later claim (new generation) must not
    /// be clobbered by a stale claimant cleaning up after itself.
    fn release_claim(&self, id: Uuid, generation: Uuid) {
        let mut map = lock(&self.previews);
        if map.get(&id).map(|h| h.generation) == Some(generation) {
            map.remove(&id);
        }
    }

    /// Release a failed claim and report the failure on the preview status
    /// channel — the single cleanup path for every entry point's error exit,
    /// so no failure can strand a reserved slot (which would refuse all
    /// later starts as "already running").
    fn abandon_claim(&self, id: Uuid, generation: Uuid, error: &CoreError) {
        self.release_claim(id, generation);
        self.emit_preview(id, PreviewStatus::Failed(error.to_string()), Vec::new());
    }

    /// Start a card's preview: make the worktree runnable (setup script), then
    /// launch the project's app and surface its URLs.
    pub(super) async fn start_preview(&self, card_id: Uuid) -> Result<()> {
        // Refuse to stack a second process on the same worktree.
        let Some(generation) = self.claim_preview(card_id) else {
            return Err(CoreError::other(
                "A preview is already running for this card — stop or relaunch it first.",
            ));
        };
        let result = self.start_preview_claimed(card_id, generation).await;
        if let Err(e) = &result {
            self.abandon_claim(card_id, generation, e);
        }
        result
    }

    /// The claimed body of [`Self::start_preview`], split out so its every `?`
    /// funnels through the caller's one abandon-claim exit.
    async fn start_preview_claimed(&self, card_id: Uuid, generation: Uuid) -> Result<()> {
        // Rebuild the worktree if a crash/cleanup removed it (normally a no-op).
        self.ensure_branch_worktree(card_id).await?;
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let worktree = card
            .worktree_path
            .clone()
            .ok_or_else(|| CoreError::other("this card has no worktree to run the app in"))?;
        self.launch_preview(card_id, &project.config, &worktree, generation)
            .await
    }

    /// Start a preview of a PR under review — the same in-worktree app launch a
    /// card gets, pointed at the contributor's checkout. Reviewing a UI change by
    /// running it beats reading the diff and imagining it, and the review
    /// worktree is already the isolated, disposable place to do that.
    ///
    /// The checkout is materialized on demand (`force: false` reuses one that's
    /// already there), so this works straight from `To review` without having
    /// spent an agent pass on the PR first.
    pub(super) async fn start_review_preview(&self, review_id: Uuid) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        // Check the run script before claiming or doing the (slow) checkout: a
        // project with no `run_script` can never preview, and the UI hides the
        // control anyway.
        if run_command(&project.config).is_none() {
            return Err(CoreError::other(
                "No run script is configured for this project — set its run command \
                 in the project settings to enable previews.",
            ));
        }
        let Some(generation) = self.claim_preview(review_id) else {
            return Err(CoreError::other(
                "A preview is already running for this PR — stop it first.",
            ));
        };
        let result = async {
            let worktree = self.prepare_review_worktree(review_id, false).await?;
            self.launch_preview(review_id, &project.config, &worktree, generation)
                .await
        }
        .await;
        if let Err(e) = &result {
            self.abandon_claim(review_id, generation, e);
        }
        result
    }

    /// Stop a PR preview: the same kill + teardown a card's preview gets.
    pub(super) async fn stop_review_preview(&self, review_id: Uuid) -> Result<()> {
        self.kill_preview(review_id).await;
        let task = self.store.get_review_task(review_id)?;
        if let Some(wt) = task.worktree_path.as_deref() {
            clear_preview_info(wt);
        }
        if let Ok(project) = self.store.get_project(task.project_id) {
            self.teardown_preview(review_id, &project.config, task.worktree_path.as_deref())
                .await;
        }
        self.emit_preview(review_id, PreviewStatus::Stopped, Vec::new());
        Ok(())
    }

    /// Bring a card's preview up alongside a write run, so the agent can test its
    /// work against the running app (see [`crate::agent::testing`]) and the human
    /// who reviews next arrives at a warm, already-serving build. Quietly a no-op
    /// when there is nothing to do: a preview already up (or setting up) is
    /// reused, and a project with no run script — or a card with no worktree —
    /// simply has no app to run (the agent's prompt omits the testing instruction
    /// on the same condition). A failure to start surfaces as a warning toast but
    /// never fails the run: the agent just finds no app and says so in its
    /// hand-off.
    pub(super) async fn ensure_preview_for_run(&self, card_id: Uuid) -> Result<()> {
        let Some(generation) = self.claim_preview(card_id) else {
            return Ok(());
        };
        // Resolve what to launch; any "nothing to do" hands the claim back
        // without having emitted a status (the UI never saw this attempt).
        let target = (|| {
            let card = self.store.get_card(card_id).ok()?;
            let project = self.store.get_project(card.project_id).ok()?;
            run_command(&project.config)?;
            let worktree = card.worktree_path.filter(|w| w.exists())?;
            Some((project.config, worktree))
        })();
        let Some((config, worktree)) = target else {
            self.release_claim(card_id, generation);
            return Ok(());
        };
        if let Err(e) = self
            .launch_preview(card_id, &config, &worktree, generation)
            .await
        {
            self.abandon_claim(card_id, generation, &e);
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                format!("The app for in-run testing couldn't start: {e}"),
            ));
        }
        Ok(())
    }

    /// The entity-agnostic half of starting a preview: verify the project can run,
    /// run the setup script, then launch the app. `id` keys the preview slot and
    /// every event it emits — a card id or a review-task id, both of which the
    /// UI's per-id preview map handles the same way. `generation` is the caller's
    /// claim on that slot; every error return leaves cleanup (release + Failed
    /// status) to the claiming entry point.
    async fn launch_preview(
        &self,
        id: Uuid,
        config: &ProjectConfig,
        worktree: &Path,
        generation: Uuid,
    ) -> Result<()> {
        if run_command(config).is_none() {
            return Err(CoreError::other(
                "No run script is configured for this project — set its run command \
                 in the project settings to enable previews.",
            ));
        }

        self.emit_preview(id, PreviewStatus::SettingUp, Vec::new());

        // 1. Setup (deps, isolated DB, per-worktree ports). Its output streams
        //    live to the UI (not persisted — it can be voluminous). The setup
        //    child is registered on the claimed slot *while it runs*, so a hung
        //    setup can be reaped by `StopPreview` — otherwise a slow
        //    `docker compose pull` would leave a live child with no stop path.
        if let Some(cmd) = resolve_script(&config.worktree_setup_script, worktree, SETUP_CANDIDATES)
        {
            self.progress(id, "Setting up the worktree for preview…");
            match self.run_setup_script(id, worktree, &cmd, generation).await {
                Ok(SetupOutcome::Completed) => {}
                // The user stopped the preview mid-setup: `kill_preview` already
                // reaped the process and emitted nothing, so settle on Stopped.
                Ok(SetupOutcome::Stopped) => {
                    self.emit_preview(id, PreviewStatus::Stopped, Vec::new());
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }

        // 2. Launch the app as a persistent process and publish its URLs.
        self.spawn_run_process(id, config, worktree, generation)
            .await
    }

    /// Run the project's teardown script (e.g. drop the isolated Docker DB) for
    /// whatever entity owns `worktree`. Best-effort: a teardown failure must never
    /// block stopping a preview.
    async fn teardown_preview(&self, id: Uuid, config: &ProjectConfig, worktree: Option<&Path>) {
        let Some(worktree) = worktree else { return };
        if let Some(cmd) = resolve_script(
            &config.worktree_teardown_script,
            worktree,
            TEARDOWN_CANDIDATES,
        ) {
            self.progress(id, "Tearing down the preview…");
            let _ = self.run_blocking_script(id, worktree, &cmd).await;
        }
    }

    /// Open a card's working directory in the user's configured terminal or
    /// editor. Uses the card's isolated worktree when it has one, else the
    /// project's main checkout — so the action works before a worktree exists.
    ///
    /// The command comes from global settings (`{path}` → the directory); we
    /// tokenize it and spawn the program directly (no shell), detached. Errors
    /// surface as a toast rather than failing the command loop.
    pub(super) async fn open_worktree(&self, card_id: Uuid, target: OpenTarget) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let dir = card.worktree_path.clone().unwrap_or(project.path);
        self.open_dir(card_id, &dir, target).await
    }

    /// Open a PR-under-review's checkout in the terminal or editor.
    ///
    /// Unlike a card — whose worktree exists from the moment it starts — a review
    /// task may have none yet, and falling back to the project's main checkout
    /// would silently open the *wrong code* (the user's own branch, not the
    /// contributor's). So the checkout is materialized first; that is the whole
    /// point of the action.
    pub(super) async fn open_review_worktree(
        &self,
        review_id: Uuid,
        target: OpenTarget,
    ) -> Result<()> {
        let dir = self.prepare_review_worktree(review_id, false).await?;
        self.open_dir(review_id, &dir, target).await
    }

    /// Launch the configured terminal/editor on `dir`, reporting failures as
    /// toasts against `id`.
    async fn open_dir(&self, id: Uuid, dir: &Path, target: OpenTarget) -> Result<()> {
        let card_id = id;
        let settings = self.store.settings().unwrap_or_default();
        let (template, what) = match target {
            OpenTarget::Terminal => (settings.terminal_command, "terminal"),
            OpenTarget::Editor => (settings.editor_command, "editor"),
        };

        let Some(argv) = template
            .as_deref()
            .and_then(|t| resolve_open_command(t, dir))
        else {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                format!("No {what} command is set — add one in Settings (e.g. `zed {{path}}`)."),
            ));
            return Ok(());
        };

        // `resolve_open_command` never returns an empty argv.
        let (program, rest) = argv.split_first().expect("non-empty argv");
        // Detached: don't wait on the GUI app, and don't let it inherit our stdio.
        let spawned = Command::new(program)
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = spawned {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Error,
                format!("Couldn't run the {what} command `{program}`: {e}"),
            ));
        }
        Ok(())
    }

    /// Restart the app in place to pick up code changes: reap the running process
    /// (leaving the worktree's DB/infra up), then relaunch `run_script`. No setup,
    /// no teardown — a fast iteration loop.
    pub(super) async fn relaunch_preview(&self, card_id: Uuid) -> Result<()> {
        self.kill_preview(card_id).await;
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let worktree = card
            .worktree_path
            .clone()
            .ok_or_else(|| CoreError::other("this card has no worktree to run the app in"))?;
        if run_command(&project.config).is_none() {
            return Err(CoreError::other(
                "No run script is configured for this project.",
            ));
        }
        // Re-claim the slot the kill just freed. Losing this claim means another
        // start slipped in between — that start owns the slot now, so bow out.
        let Some(generation) = self.claim_preview(card_id) else {
            return Err(CoreError::other(
                "A preview is already starting for this card.",
            ));
        };
        self.emit_preview(card_id, PreviewStatus::SettingUp, Vec::new());
        let result = self
            .spawn_run_process(card_id, &project.config, &worktree, generation)
            .await;
        if let Err(e) = &result {
            self.abandon_claim(card_id, generation, e);
        }
        result
    }

    /// Stop a card's preview: kill the process tree, then run the project's
    /// teardown script (e.g. drop the isolated Docker DB). Best-effort throughout.
    pub(super) async fn stop_preview(&self, card_id: Uuid) -> Result<()> {
        self.kill_preview(card_id).await;
        let card = self.store.get_card(card_id)?;
        if let Some(wt) = card.worktree_path.as_deref() {
            clear_preview_info(wt);
        }
        if let Ok(project) = self.store.get_project(card.project_id) {
            self.teardown_preview(card_id, &project.config, card.worktree_path.as_deref())
                .await;
        }
        self.emit_preview(card_id, PreviewStatus::Stopped, Vec::new());
        Ok(())
    }

    // --- helpers -----------------------------------------------------------

    /// Spawn `run_script` as a persistent, process-group-leading child in the
    /// worktree, register it on the caller's claimed slot, stream its output
    /// live, spawn a reaper that reports an unexpected exit, and emit `Running`
    /// with the preview URLs.
    async fn spawn_run_process(
        &self,
        card_id: Uuid,
        config: &ProjectConfig,
        worktree: &Path,
        generation: Uuid,
    ) -> Result<()> {
        let run = run_command(config).expect("callers guarantee a run script");
        let mut cmd = Command::new("sh");
        cmd.arg("-lc")
            .arg(&run)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A new process group so `kill_preview` can reap the whole tree (the run
        // command plus everything it spawns: turbo, vite, nest, docker).
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| CoreError::other(format!("failed to launch the app (`{run}`): {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| CoreError::other("preview process exited before it could be tracked"))?;
        // Register the app on our claim — unless a concurrent stop removed the
        // slot while we were spawning, in which case that stop could not have
        // seen this pid: reap it ourselves and settle on Stopped.
        if !self.register_claimed_pid(card_id, generation, pid) {
            kill_group(pid).await;
            self.emit_preview(card_id, PreviewStatus::Stopped, Vec::new());
            return Ok(());
        }

        // Stream both streams live to the UI (unpersisted — dev output is huge).
        if let Some(out) = child.stdout.take() {
            self.spawn_log_reader(card_id, out);
        }
        if let Some(err) = child.stderr.take() {
            self.spawn_log_reader(card_id, err);
        }

        let urls = compute_preview_urls(worktree, &config.preview_ports);
        // Surface the URLs to an agent working in this worktree too (see
        // `agent::testing`) — written before the reaper is spawned, so an app
        // that dies instantly can't have its reaper clear the file first and
        // then be overwritten by a stale "running". The exclude guard keeps the
        // file — and the setup script's `.wt-offset` — from ever being swept
        // into a commit by a `git add -A` (finalize's auto-commit, or one the
        // agent runs itself).
        crate::infra::git::ensure_excluded(
            worktree,
            &[crate::agent::testing::PREVIEW_INFO_FILE, ".wt-offset"],
        )
        .await;
        let _ = std::fs::write(
            worktree.join(crate::agent::testing::PREVIEW_INFO_FILE),
            crate::agent::testing::preview_info_json(&urls),
        );

        // Reaper: on exit, report Stopped/Failed iff this generation still owns the
        // slot. An intentional stop/relaunch removes the entry first, so the reaper
        // stays quiet and doesn't clobber the new run's status. Either way the
        // worktree's preview-info file no longer describes a live app, so drop it
        // (a relaunch rewrites it).
        let previews = self.previews.clone();
        let evt_tx = self.evt_tx.clone();
        let wt = worktree.to_path_buf();
        tokio::spawn(async move {
            let status = child.wait().await;
            clear_preview_info(&wt);
            let mut map = lock(&previews);
            if map.get(&card_id).map(|h| h.generation) != Some(generation) {
                return;
            }
            map.remove(&card_id);
            drop(map);
            let update = match status {
                Ok(s) if s.success() => PreviewStatus::Stopped,
                Ok(s) => PreviewStatus::Failed(format!("the app exited ({s})")),
                Err(e) => PreviewStatus::Failed(format!("the app process errored: {e}")),
            };
            let _ =
                evt_tx.unbounded_send(ExecutorEvent::preview_updated(card_id, update, Vec::new()));
        });
        let addrs = urls
            .iter()
            .map(|u| format!("{}: {}", u.label, u.url))
            .collect::<Vec<_>>()
            .join("  ");
        self.progress(
            card_id,
            &if addrs.is_empty() {
                "▶ preview started".to_string()
            } else {
                format!("▶ preview started — {addrs}")
            },
        );
        self.emit_preview(card_id, PreviewStatus::Running, urls);
        Ok(())
    }

    /// Set the pid on a claimed slot iff it still belongs to `generation`.
    /// Returns whether the registration took — `false` means a concurrent
    /// stop (or a superseding claim) owns the slot and the caller must reap
    /// the process it just spawned itself.
    fn register_claimed_pid(&self, id: Uuid, generation: Uuid, pid: u32) -> bool {
        let mut map = lock(&self.previews);
        match map.get_mut(&id) {
            Some(h) if h.generation == generation => {
                h.pid = Some(pid);
                true
            }
            _ => false,
        }
    }

    /// Reap a card's running preview (if any): drop its slot first so the reaper
    /// stays quiet, then SIGTERM→SIGKILL the whole process group. A slot that is
    /// only reserved (no pid yet) has no process to kill — removing it is
    /// enough: the launching task notices its claim is gone at the next
    /// registration checkpoint and reaps whatever it spawned meanwhile.
    async fn kill_preview(&self, card_id: Uuid) {
        let handle = lock(&self.previews).remove(&card_id);
        if let Some(PreviewHandle { pid: Some(pid), .. }) = handle {
            kill_group(pid).await;
        }
    }

    /// Run the preview's setup script, registering it on the caller's claimed
    /// slot (as a process-group leader) while it runs so `kill_preview` can reap
    /// a hung setup. Returns [`SetupOutcome::Stopped`] if a concurrent stop
    /// reaped it (our claim was taken), or an error on a non-zero exit.
    async fn run_setup_script(
        &self,
        card_id: Uuid,
        worktree: &Path,
        cmd: &str,
        generation: Uuid,
    ) -> Result<SetupOutcome> {
        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(cmd)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so `kill_group` reaps the setup command's whole tree.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|e| CoreError::other(format!("failed to run `{cmd}`: {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| CoreError::other("setup process exited before it could be tracked"))?;
        // A stop that landed between the claim and this spawn removed our slot
        // without seeing this pid — reap it ourselves.
        if !self.register_claimed_pid(card_id, generation, pid) {
            kill_group(pid).await;
            return Ok(SetupOutcome::Stopped);
        }

        if let Some(out) = child.stdout.take() {
            self.spawn_log_reader(card_id, out);
        }
        if let Some(err) = child.stderr.take() {
            self.spawn_log_reader(card_id, err);
        }
        let status = child.wait().await;
        // A stop that landed while we were running took our slot (or cleared it)
        // and already reaped the process — report that rather than a spurious exit.
        if lock(&self.previews).get(&card_id).map(|h| h.generation) != Some(generation) {
            return Ok(SetupOutcome::Stopped);
        }
        match status {
            Ok(s) if s.success() => Ok(SetupOutcome::Completed),
            Ok(s) => Err(CoreError::other(format!("setup script exited with {s}"))),
            Err(e) => Err(CoreError::other(format!("setup script failed to run: {e}"))),
        }
    }

    /// Run a script to completion in the worktree, streaming its output live to
    /// the UI. Errors on a non-zero exit.
    async fn run_blocking_script(&self, card_id: Uuid, worktree: &Path, cmd: &str) -> Result<()> {
        let mut child = Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CoreError::other(format!("failed to run `{cmd}`: {e}")))?;
        if let Some(out) = child.stdout.take() {
            self.spawn_log_reader(card_id, out);
        }
        if let Some(err) = child.stderr.take() {
            self.spawn_log_reader(card_id, err);
        }
        let status = child
            .wait()
            .await
            .map_err(|e| CoreError::other(format!("script failed to run: {e}")))?;
        if !status.success() {
            return Err(CoreError::other(format!("script exited with {status}")));
        }
        Ok(())
    }

    /// Forward a child stream's lines to the UI transcript live (not persisted).
    fn spawn_log_reader<S: AsyncRead + Unpin + Send + 'static>(&self, card_id: Uuid, stream: S) {
        let evt_tx = self.evt_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ =
                    evt_tx.unbounded_send(ExecutorEvent::transcript(card_id, now_millis(), line));
            }
        });
    }

    fn emit_preview(&self, card_id: Uuid, status: PreviewStatus, urls: Vec<PreviewUrl>) {
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::preview_updated(card_id, status, urls));
    }
}

/// The project's launch command, trimmed to `None` if unset/blank. `pub(super)`
/// so `lifecycle::launch` keys the testing instruction off the same condition
/// that decides whether a preview can start at all.
pub(super) fn run_command(config: &ProjectConfig) -> Option<String> {
    config
        .run_script
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Best-effort removal of a worktree's preview-info file once the app it
/// describes is no longer running (stop, unexpected exit). A relaunch rewrites
/// it; a worktree teardown removes it with everything else.
fn clear_preview_info(worktree: &Path) {
    let _ = std::fs::remove_file(worktree.join(crate::agent::testing::PREVIEW_INFO_FILE));
}

/// Resolve a setup/teardown command: the configured value wins; otherwise a
/// conventional script committed in the worktree is auto-detected. `None` means
/// there is no step to run.
fn resolve_script(
    configured: &Option<String>,
    worktree: &Path,
    candidates: &[&str],
) -> Option<String> {
    if let Some(cmd) = configured {
        let cmd = cmd.trim();
        return (!cmd.is_empty()).then(|| cmd.to_string());
    }
    candidates
        .iter()
        .find(|rel| worktree.join(rel).exists())
        .map(|rel| format!("bash {rel}"))
}

/// Compute the clickable preview URLs from the worktree's `.wt-offset` (written by
/// the setup script's free-port allocation) and the project's declared ports.
/// Empty when there are no ports or the offset file is missing/unparseable.
fn compute_preview_urls(worktree: &Path, ports: &[PreviewPort]) -> Vec<PreviewUrl> {
    if ports.is_empty() {
        return Vec::new();
    }
    let Some(offset) = std::fs::read_to_string(worktree.join(".wt-offset"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return Vec::new();
    };
    ports
        .iter()
        .map(|p| PreviewUrl {
            label: p.label.clone(),
            url: format!("http://localhost:{}", p.base as u32 + offset),
        })
        .collect()
}

/// SIGTERM then (after a grace) SIGKILL a preview's entire process tree. `pid` is
/// the `sh -lc <run>` we spawn as a process-group leader (`process_group(0)`).
///
/// Signalling that group reaps everything that stayed in it, but `pnpm`/`turbo`
/// move their dev servers — and each `--watch` task — into *new* process groups
/// of their own, which a group-only signal misses, leaving them alive to keep
/// writing into the worktree (and racing its removal on merge). So we also snapshot
/// the descendant tree by ppid *before* killing (once the root dies, reparented
/// survivors become unreachable this way) and signal every descendant directly.
#[cfg(unix)]
pub(super) async fn kill_group(pid: u32) {
    let mut targets = descendants(pid).await;
    targets.push(pid);
    signal_all("-TERM", pid, &targets).await;
    tokio::time::sleep(KILL_GRACE).await;
    signal_all("-KILL", pid, &targets).await;
}

/// Send `sig` to the process group led by `group` (negated pid) and, belt-and-
/// suspenders, to each pid in `pids` individually — covering children that left
/// the group. `kill` signals every valid target even if some are already gone.
#[cfg(unix)]
async fn signal_all(sig: &str, group: u32, pids: &[u32]) {
    let mut args: Vec<String> = Vec::with_capacity(pids.len() + 2);
    args.push(sig.to_string());
    args.push(format!("-{group}"));
    args.extend(pids.iter().map(u32::to_string));
    let _ = Command::new("kill")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Every descendant pid of `root` (children, grandchildren, …) from a single `ps`
/// snapshot. Empty if `ps` can't be read — the group signal still covers the
/// common case.
#[cfg(unix)]
async fn descendants(root: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .await
    else {
        return Vec::new();
    };
    // ppid -> children.
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                found.push(k);
                stack.push(k);
            }
        }
    }
    found
}

#[cfg(not(unix))]
pub(super) async fn kill_group(_pid: u32) {}

/// Synchronously reap several preview process trees at once — the app's
/// last-gasp cleanup on shutdown, where there is no async runtime to await on.
/// Snapshots each leader's descendants, `SIGTERM`s everything, waits one grace
/// period, then `SIGKILL`s. Passing all pids together bounds the blocking to a
/// single grace period no matter how many previews are up.
#[cfg(unix)]
pub(super) fn reap_all_blocking(leaders: &[u32]) {
    if leaders.is_empty() {
        return;
    }
    let mut targets: Vec<u32> = Vec::new();
    for &pid in leaders {
        targets.extend(descendants_blocking(pid));
        targets.push(pid);
    }
    signal_pids_blocking("-TERM", leaders, &targets);
    std::thread::sleep(KILL_GRACE);
    signal_pids_blocking("-KILL", leaders, &targets);
}

/// Blocking twin of [`signal_all`]: signal each process group (negated leader
/// pid) plus each individual target pid.
#[cfg(unix)]
fn signal_pids_blocking(sig: &str, leaders: &[u32], pids: &[u32]) {
    let mut args: Vec<String> = Vec::with_capacity(leaders.len() + pids.len() + 1);
    args.push(sig.to_string());
    args.extend(leaders.iter().map(|g| format!("-{g}")));
    args.extend(pids.iter().map(u32::to_string));
    let _ = std::process::Command::new("kill")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Blocking twin of [`descendants`]: every descendant pid of `root` from one
/// synchronous `ps` snapshot. Empty if `ps` can't be read.
#[cfg(unix)]
fn descendants_blocking(root: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    else {
        return Vec::new();
    };
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                found.push(k);
                stack.push(k);
            }
        }
    }
    found
}

#[cfg(not(unix))]
pub(super) fn reap_all_blocking(_leaders: &[u32]) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_add_offset_to_each_port() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".wt-offset"), "100\n").unwrap();
        let ports = vec![
            PreviewPort {
                label: "web".into(),
                base: 3000,
            },
            PreviewPort {
                label: "api".into(),
                base: 3001,
            },
        ];
        let urls = compute_preview_urls(dir.path(), &ports);
        assert_eq!(urls[0].url, "http://localhost:3100");
        assert_eq!(urls[1].url, "http://localhost:3101");
    }

    #[test]
    fn urls_empty_without_offset_file() {
        let dir = tempfile::tempdir().unwrap();
        let ports = vec![PreviewPort {
            label: "web".into(),
            base: 3000,
        }];
        assert!(compute_preview_urls(dir.path(), &ports).is_empty());
    }

    #[test]
    fn resolve_prefers_configured_then_autodetects() {
        let dir = tempfile::tempdir().unwrap();
        // Configured wins.
        assert_eq!(
            resolve_script(&Some("pnpm setup".into()), dir.path(), SETUP_CANDIDATES),
            Some("pnpm setup".into())
        );
        // Blank configured → fall through to auto-detect (nothing present yet).
        assert_eq!(
            resolve_script(&Some("  ".into()), dir.path(), SETUP_CANDIDATES),
            None
        );
        // A conventional script on disk is picked up.
        std::fs::write(dir.path().join("setup-worktree.sh"), "#!/bin/bash\n").unwrap();
        assert_eq!(
            resolve_script(&None, dir.path(), SETUP_CANDIDATES),
            Some("bash setup-worktree.sh".into())
        );
    }
}

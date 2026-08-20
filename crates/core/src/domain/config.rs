//! Configuration: per-card, per-project, and global defaults.
//!
//! Defaults cascade: a new card inherits its project's defaults, and a new
//! project inherits the global [`AppSettings`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::model::{Effort, ModelSpec, Provider};

/// The token replaced by the target directory in a configured "open in
/// terminal/editor" command (see [`AppSettings::terminal_command`]).
pub const OPEN_PATH_PLACEHOLDER: &str = "{path}";

/// Split a user-configured open-command template into a program + argv, with
/// [`OPEN_PATH_PLACEHOLDER`] replaced by `dir`. When the template contains no
/// placeholder, `dir` is appended as the final argument — so a bare `zed` or
/// `code` works as well as `zed {path}`. Returns `None` for a blank template or
/// one that doesn't parse as a shell command.
///
/// Tokenizing here (rather than handing the string to a shell) keeps paths with
/// spaces intact and leaves no shell-injection surface: the caller spawns the
/// program directly with these args.
pub fn resolve_open_command(template: &str, dir: &Path) -> Option<Vec<String>> {
    let tokens = shlex::split(template.trim())?;
    if tokens.is_empty() {
        return None;
    }
    let path = dir.to_string_lossy();
    let mut substituted = false;
    let mut argv: Vec<String> = tokens
        .into_iter()
        .map(|tok| {
            if tok.contains(OPEN_PATH_PLACEHOLDER) {
                substituted = true;
                tok.replace(OPEN_PATH_PLACEHOLDER, &path)
            } else {
                tok
            }
        })
        .collect();
    if !substituted {
        argv.push(path.into_owned());
    }
    Some(argv)
}

/// What a card *is*: a change to implement, or a read-only investigation whose
/// deliverable is a conclusion. Investigations run in the main checkout with no
/// worktree/branch/PR, on the card's `plan` spec, and end parked on their
/// conclusion (see `CardState::Concluded`). A card can change kind while it sits
/// in the starting block — including a concluded investigation converted into an
/// implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CardKind {
    /// Implement a change (the default; every record written before this field
    /// existed is a task).
    #[default]
    Task,
    /// Read-only: investigate/audit and return a conclusion, no code changes.
    Investigation,
}

/// Per-card execution config. The plan, implement, and review phases are
/// configured independently (different model and/or effort), per the product spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardConfig {
    pub provider: Provider,
    /// Task vs. investigation — see [`CardKind`]. `#[serde(default)]` keeps
    /// records written before this field existed loadable (as tasks).
    #[serde(default)]
    pub kind: CardKind,
    pub plan: ModelSpec,
    pub implement: ModelSpec,
    /// Model for the read-only review phases — self-review and PR-comment
    /// triage. `None` means "run them on `implement`", which is both the default
    /// and what every record written before this field existed implies: those
    /// phases had no spec of their own and used the implement one. Read it
    /// through [`Self::review_spec`], never directly.
    #[serde(default)]
    pub review: Option<ModelSpec>,
}

impl CardConfig {
    /// Sensible defaults for a freshly chosen provider.
    pub fn default_for(provider: Provider) -> Self {
        match provider {
            Provider::Claude => CardConfig {
                provider,
                kind: CardKind::default(),
                plan: ModelSpec::new("opus", Effort::XHigh),
                implement: ModelSpec::new("opus", Effort::Medium),
                review: None,
            },
            // gpt-5.5, not the gpt-5.3-codex coding specialist: verified live
            // (Jul 2026), ChatGPT-account auth rejects the codex-branded models
            // and gpt-5.4 with "not supported when using Codex with a ChatGPT
            // account" — only gpt-5.5 and gpt-5.4-mini were accepted, and
            // Usine's codex provider only supports ChatGPT login.
            Provider::Codex => CardConfig {
                provider,
                kind: CardKind::default(),
                plan: ModelSpec::new("gpt-5.5", Effort::High),
                implement: ModelSpec::new("gpt-5.5", Effort::Medium),
                review: None,
            },
        }
    }

    /// The spec the review and triage phases actually run at: the explicit
    /// review override when set, else the implement spec.
    pub fn review_spec(&self) -> ModelSpec {
        self.review
            .clone()
            .unwrap_or_else(|| self.implement.clone())
    }
}

impl Default for CardConfig {
    fn default() -> Self {
        CardConfig::default_for(Provider::Claude)
    }
}

/// One service a project's preview exposes. Its clickable URL is
/// `http://localhost:{base + offset}`, where `offset` is read from the worktree's
/// `.wt-offset` file (written by the setup script's free-port allocation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewPort {
    /// Display label (e.g. "frontend", "backend").
    pub label: String,
    /// The service's port in the *main* checkout; the per-worktree offset is added.
    pub base: u16,
}

/// Per-project defaults plus forge settings (reviewer, base branch) and the
/// preview/worktree scripts that let the app be run straight from a card's
/// worktree. `#[serde(default)]` on the newer fields keeps `ProjectRecord`s
/// written before they existed loadable (the store uses a self-describing JSON
/// codec, so absent fields fall back to the default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub default_provider: Provider,
    pub default_plan: ModelSpec,
    pub default_implement: ModelSpec,
    /// Review-phase model for this project. Seeds new cards' `review`, and is
    /// the spec the contributor-PR review runs at (those have no card behind
    /// them). `None` means "same as `default_implement`" — see
    /// [`CardConfig::review`]. Read it through [`Self::review_spec`].
    #[serde(default)]
    pub default_review: Option<ModelSpec>,
    /// GitHub username to request as reviewer on created PRs.
    pub reviewer: Option<String>,
    /// Auto-detected base branch, refreshed from the repo at every startup.
    /// Never read this directly for git/PR operations — go through
    /// [`Self::effective_base_branch`], which lets a user pin win.
    pub base_branch: String,
    /// User-pinned base branch. When set (non-blank), it wins over the
    /// auto-detected `base_branch` everywhere: worktree cut point, PR target,
    /// conflict resolution, diffs. `None` or blank means auto-detect.
    #[serde(default)]
    pub pinned_base_branch: Option<String>,
    /// GitHub logins whose open PRs should surface in the PR-review workflow.
    /// Picked from the same collaborator list as `reviewer`.
    #[serde(default)]
    pub review_contributors: Vec<String>,
    /// Command run in a freshly-created worktree to make it runnable — install
    /// deps, stand up an isolated DB, assign per-worktree ports, etc. When unset,
    /// a conventional `setup-worktree.sh` in the repo is auto-detected. It must
    /// write only gitignored paths so the worktree's `git status` stays clean.
    #[serde(default)]
    pub worktree_setup_script: Option<String>,
    /// Command that launches the app inside the worktree for preview/testing.
    /// When unset, the "Test app" action is unavailable.
    #[serde(default)]
    pub run_script: Option<String>,
    /// Command run in a card's worktree at the validation gate before a PR is
    /// opened (build, tests, linters). Non-zero exit means validation failed.
    /// When unset, the gate is skipped entirely.
    #[serde(default)]
    pub validate_script: Option<String>,
    /// Command run in the worktree when a preview stops, before it's torn down
    /// (e.g. `docker compose down -v`). Auto-detected as `teardown-worktree.sh`
    /// when unset. Best-effort — a failure never blocks stopping the preview.
    #[serde(default)]
    pub worktree_teardown_script: Option<String>,
    /// Per-service ports the preview exposes, surfaced as clickable URLs.
    #[serde(default)]
    pub preview_ports: Vec<PreviewPort>,
}

impl ProjectConfig {
    /// Build the [`CardConfig`] a new card in this project should start with.
    pub fn new_card_config(&self) -> CardConfig {
        CardConfig {
            provider: self.default_provider,
            kind: CardKind::default(),
            plan: self.default_plan.clone(),
            implement: self.default_implement.clone(),
            review: self.default_review.clone(),
        }
    }

    /// The spec this project's contributor-PR reviews run at, and the fallback
    /// behind [`CardConfig::review_spec`] for cards it seeds.
    pub fn review_spec(&self) -> ModelSpec {
        self.default_review
            .clone()
            .unwrap_or_else(|| self.default_implement.clone())
    }

    /// The base branch every git/PR operation should use: the user's pin when
    /// one is set (blank counts as unset), else the auto-detected branch.
    pub fn effective_base_branch(&self) -> &str {
        self.pinned_base_branch
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.base_branch)
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        let base = CardConfig::default_for(Provider::Claude);
        ProjectConfig {
            default_provider: base.provider,
            default_plan: base.plan,
            default_implement: base.implement,
            default_review: base.review,
            reviewer: None,
            base_branch: "dev".to_string(),
            pinned_base_branch: None,
            review_contributors: Vec::new(),
            worktree_setup_script: None,
            run_script: None,
            validate_script: None,
            worktree_teardown_script: None,
            preview_ports: Vec::new(),
        }
    }
}

/// Global app defaults, used to seed new projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    pub default_provider: Provider,
    pub default_plan: ModelSpec,
    pub default_implement: ModelSpec,
    /// Review-phase default for new projects. `None` means "same as
    /// `default_implement`" — see [`CardConfig::review`].
    #[serde(default)]
    pub default_review: Option<ModelSpec>,
    /// Command run for a card's "Open in terminal" action. `{path}` is replaced
    /// by the card's worktree (or the project checkout); if absent it's appended.
    /// Cross-platform by construction — e.g. `open -a Terminal {path}` (macOS),
    /// `gnome-terminal --working-directory={path}` (Linux), `wt -d {path}`
    /// (Windows). `None`/blank disables the action. See [`resolve_open_command`].
    #[serde(default)]
    pub terminal_command: Option<String>,
    /// Command run for a card's "Open in editor" action, same `{path}` rule as
    /// [`Self::terminal_command`] — e.g. `zed {path}`, `code {path}`, or just
    /// `code`. `None`/blank disables the action.
    #[serde(default)]
    pub editor_command: Option<String>,
    /// Global cap on concurrently running agent runs and validation checks
    /// (the memory/compute-heavy work) across all projects; further starts
    /// queue FIFO and launch as slots free. `0` = unlimited. Agent Chat
    /// questions, PR-comment triage, and previews are exempt (see
    /// `RunMode::is_capped`).
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: u32,
}

/// Serde default for [`AppSettings::max_concurrent_runs`]: a settings record
/// written before the field existed must load as the default cap, not as 0
/// (which would mean unlimited).
pub(crate) fn default_max_concurrent_runs() -> u32 {
    5
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings::default_for(Provider::Claude)
    }
}

impl AppSettings {
    /// Sensible global defaults for a freshly chosen provider — the provider's
    /// own model presets, not Claude's with the provider enum flipped.
    pub fn default_for(provider: Provider) -> Self {
        let base = CardConfig::default_for(provider);
        AppSettings {
            default_provider: base.provider,
            default_plan: base.plan,
            default_implement: base.implement,
            default_review: base.review,
            terminal_command: None,
            editor_command: None,
            max_concurrent_runs: default_max_concurrent_runs(),
        }
    }

    /// Build the [`ProjectConfig`] a new project should start with. Only the
    /// model defaults come from global settings; everything else (base branch,
    /// preview ports, worktree scripts) uses `ProjectConfig`'s own defaults.
    pub fn new_project_config(&self) -> ProjectConfig {
        ProjectConfig {
            default_provider: self.default_provider,
            default_plan: self.default_plan.clone(),
            default_implement: self.default_implement.clone(),
            default_review: self.default_review.clone(),
            ..ProjectConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_placeholder_and_preserves_spaces() {
        // A path with spaces must survive as a single argument.
        let argv = resolve_open_command("zed {path}", Path::new("/tmp/wt one")).unwrap();
        assert_eq!(argv, ["zed", "/tmp/wt one"]);

        // Placeholder embedded inside a token (e.g. a `--flag=value`).
        let argv =
            resolve_open_command("gnome-terminal --working-directory={path}", Path::new("/r"))
                .unwrap();
        assert_eq!(argv, ["gnome-terminal", "--working-directory=/r"]);
    }

    #[test]
    fn appends_path_when_no_placeholder() {
        let argv = resolve_open_command("code", Path::new("/repo")).unwrap();
        assert_eq!(argv, ["code", "/repo"]);
    }

    #[test]
    fn quoted_token_stays_intact() {
        let argv = resolve_open_command(r#"open -a "Visual Studio Code" {path}"#, Path::new("/r"))
            .unwrap();
        assert_eq!(argv, ["open", "-a", "Visual Studio Code", "/r"]);
    }

    #[test]
    fn blank_or_unparseable_template_is_none() {
        assert!(resolve_open_command("", Path::new("/r")).is_none());
        assert!(resolve_open_command("   ", Path::new("/r")).is_none());
        // An unterminated quote doesn't parse as a command.
        assert!(resolve_open_command("code \"unterminated", Path::new("/r")).is_none());
    }

    #[test]
    fn settings_without_command_fields_still_load() {
        // A settings record written before the terminal/editor fields existed
        // must deserialize (the store's JSON codec + `#[serde(default)]`).
        let json = r#"{
            "default_provider": "claude",
            "default_plan": {"model": "opus", "effort": "xhigh"},
            "default_implement": {"model": "opus", "effort": "medium"}
        }"#;
        let s: AppSettings = serde_json::from_str(json).expect("loads without new fields");
        assert_eq!(s.terminal_command, None);
        assert_eq!(s.editor_command, None);
        assert_eq!(s.default_review, None);
        // The concurrency cap defaults to 5, not 0 — 0 would mean unlimited.
        assert_eq!(s.max_concurrent_runs, 5);
    }

    #[test]
    fn project_config_without_new_fields_still_loads() {
        // A ProjectRecord written before `pinned_base_branch`/`validate_script`
        // existed must deserialize (JSON codec + `#[serde(default)]`).
        let json = r#"{
            "default_provider": "claude",
            "default_plan": {"model": "opus", "effort": "xhigh"},
            "default_implement": {"model": "opus", "effort": "medium"},
            "reviewer": null,
            "base_branch": "main"
        }"#;
        let c: ProjectConfig = serde_json::from_str(json).expect("loads without new fields");
        assert_eq!(c.pinned_base_branch, None);
        assert_eq!(c.validate_script, None);
        assert_eq!(c.effective_base_branch(), "main");
        assert!(c.preview_ports.is_empty());
    }

    #[test]
    fn pinned_base_branch_wins_unless_blank() {
        let mut c = ProjectConfig {
            base_branch: "main".to_string(),
            ..ProjectConfig::default()
        };
        assert_eq!(c.effective_base_branch(), "main");

        c.pinned_base_branch = Some("release/v2".to_string());
        assert_eq!(c.effective_base_branch(), "release/v2");

        // Blank or whitespace pins count as "not pinned".
        c.pinned_base_branch = Some("   ".to_string());
        assert_eq!(c.effective_base_branch(), "main");
    }

    #[test]
    fn new_project_defaults_are_stack_neutral() {
        // No seeded preview port, no scripts: a new project makes no assumption
        // about what kind of app it hosts.
        let c = ProjectConfig::default();
        assert!(c.preview_ports.is_empty());
        assert_eq!(c.validate_script, None);
    }

    #[test]
    fn config_without_review_field_falls_back_to_implement() {
        // A card written before the review field existed ran its review/triage
        // phases on the implement spec. Loading it must preserve exactly that —
        // in particular it must NOT invent a Claude default on a Codex card,
        // which would hand the codex CLI an `opus` model id.
        let json = r#"{
            "provider": "codex",
            "plan": {"model": "gpt-5-codex", "effort": "high"},
            "implement": {"model": "gpt-5-codex", "effort": "medium"}
        }"#;
        let c: CardConfig = serde_json::from_str(json).expect("loads without the review field");
        assert_eq!(c.review, None);
        // Records written before the kind field existed load as tasks.
        assert_eq!(c.kind, CardKind::Task);
        assert_eq!(
            c.review_spec(),
            ModelSpec::new("gpt-5-codex", Effort::Medium)
        );

        // An explicit override wins over the implement spec.
        let c = CardConfig {
            review: Some(ModelSpec::new("gpt-5-mini", Effort::Low)),
            ..c
        };
        assert_eq!(c.review_spec(), ModelSpec::new("gpt-5-mini", Effort::Low));
    }

    #[test]
    fn app_settings_default_for_carries_provider_presets() {
        // Seeding Codex settings must bring Codex model ids along, not leave
        // Claude's `opus` presets behind a flipped provider enum.
        let codex = AppSettings::default_for(Provider::Codex);
        let base = CardConfig::default_for(Provider::Codex);
        assert_eq!(codex.default_provider, Provider::Codex);
        assert_eq!(codex.default_plan, base.plan);
        assert_eq!(codex.default_implement, base.implement);

        // The plain Default is still the Claude preset.
        assert_eq!(
            AppSettings::default(),
            AppSettings::default_for(Provider::Claude)
        );
    }

    #[test]
    fn review_default_cascades_from_settings_to_card() {
        let settings = AppSettings {
            default_review: Some(ModelSpec::new("sonnet", Effort::High)),
            ..AppSettings::default()
        };
        let project = settings.new_project_config();
        assert_eq!(
            project.review_spec(),
            ModelSpec::new("sonnet", Effort::High)
        );

        let card = project.new_card_config();
        assert_eq!(card.review_spec(), ModelSpec::new("sonnet", Effort::High));

        // With no override anywhere, every level resolves to its implement spec.
        let project = AppSettings::default().new_project_config();
        assert_eq!(project.review_spec(), project.default_implement);
        let card = project.new_card_config();
        assert_eq!(card.review_spec(), card.implement);
    }
}

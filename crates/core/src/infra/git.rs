//! Git worktree management.
//!
//! Following the "shell for mutation, git2 for inspection" rule: mutations
//! (worktree add/remove, branch ops) shell out to the real `git` binary, which
//! handles every edge case correctly; read-only inspection mostly uses `git2`
//! to avoid a process spawn on the UI path. The exception is [`is_dirty`], which
//! shells out so its result matches `git status` exactly (see its docs).

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::{CoreError, Result};

/// Cap on any single `git` invocation so a hung command (auth prompt, network
/// stall on push) can't block a run actor — or, before the dispatch loop was
/// made concurrent, the whole executor — indefinitely.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

// --- argv builders (unit-tested) -------------------------------------------

/// Cut a new `branch` at `base` (any commit-ish, typically `origin/<name>`) in a
/// worktree at `path`. `--no-track` keeps the branch from adopting a
/// remote-tracking `base` as its upstream — the card's branch pushes to its own
/// `origin/usine/...`, never to the base it forked from.
pub fn worktree_add_args(branch: &str, path: &str, base: &str) -> Vec<String> {
    vec![
        "worktree".into(),
        "add".into(),
        "--no-track".into(),
        "-b".into(),
        branch.into(),
        path.into(),
        base.into(),
    ]
}

pub fn worktree_remove_args(path: &str) -> Vec<String> {
    vec![
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        path.into(),
    ]
}

/// Attach a worktree to an *already-existing* local branch (no `-b`, which would
/// try to create a new branch). Used to check out a fetched PR head for review.
pub fn worktree_add_existing_args(branch: &str, path: &str) -> Vec<String> {
    vec!["worktree".into(), "add".into(), path.into(), branch.into()]
}

/// Fetch a PR's head commit into a local branch via the `pull/<n>/head` ref
/// (resolves through the base repo, so it works for PRs from forks too). The `+`
/// force-updates the local branch if it already exists (e.g. re-reviewing).
pub fn fetch_pr_args(pr_number: u64, local_branch: &str) -> Vec<String> {
    vec![
        "fetch".into(),
        "origin".into(),
        format!("+pull/{pr_number}/head:{local_branch}"),
    ]
}

/// Attach a DETACHED worktree at `commitish` (claims no branch), so it works even
/// when `commitish` is a branch already checked out in another working tree. Used
/// to run the read-only self-review off the branch's committed HEAD without
/// touching the user's working copy.
pub fn worktree_add_detached_args(path: &str, commitish: &str) -> Vec<String> {
    vec![
        "worktree".into(),
        "add".into(),
        "--detach".into(),
        path.into(),
        commitish.into(),
    ]
}

pub fn reset_mixed_args(gitref: &str) -> Vec<String> {
    vec!["reset".into(), "--mixed".into(), gitref.into()]
}

pub fn rename_branch_args(old: &str, new: &str) -> Vec<String> {
    vec!["branch".into(), "-m".into(), old.into(), new.into()]
}

/// Every local branch, shorthand. `for-each-ref` reports the names git actually
/// has — which, on a case-insensitive filesystem, is not always the name a ref
/// was created with (see [`canonicalize_branch_case`]), so this is the honest
/// source for both the case check and the post-rename verification.
pub fn list_branches_args() -> Vec<String> {
    vec![
        "for-each-ref".into(),
        "--format=%(refname:short)".into(),
        "refs/heads/".into(),
    ]
}

// --- branch-name hygiene (unit-tested) --------------------------------------

/// Coerce a user-typed branch name into one `git check-ref-format` accepts.
///
/// Anything git forbids — whitespace, control characters, `~^:?*[\` and `@{` —
/// collapses to `-`, then the separators are tidied: no doubled `-` or `.`, no
/// `..`, no empty path components, no leading/trailing `-` `.` `/`, and no
/// component ending in `.lock`. Returns an empty string if nothing usable is
/// left, which callers must treat as "not a branch name".
///
/// Case is deliberately preserved: `JIRA-123/thing` is a legitimate branch name
/// under plenty of team conventions, and lowercasing it would be a surprise.
/// Case *collisions* are a separate problem — see [`canonicalize_branch_case`].
pub fn sanitize_branch_name(input: &str) -> String {
    let mapped: String = input
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '-' | '_' | '.' => c,
            c if c.is_alphanumeric() => c,
            _ => '-',
        })
        .collect();

    mapped
        .split('/')
        .map(|part| {
            let part = collapse_runs(part, '-');
            let part = collapse_runs(&part, '.');
            let mut part = part.trim_matches(|c| c == '-' || c == '.').to_string();
            while let Some(stripped) = part.strip_suffix(".lock") {
                part = stripped.trim_end_matches('.').to_string();
            }
            part
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Squash runs of `c` down to a single occurrence.
fn collapse_runs(s: &str, c: char) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == c && out.ends_with(c) {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Rewrite `requested`'s directory segments to the capitalisation those
/// directories already have on disk.
///
/// Git stores a loose ref as a file (`refs/heads/fix/thing`), so on a
/// case-insensitive filesystem — APFS, HFS+, NTFS — asking for `Fix/thing` when
/// a `fix/` directory already exists writes the ref into `fix/` and the branch
/// silently becomes `fix/thing`. `git branch -m` still exits 0, but the
/// requested name now resolves to nothing: `git push` fails with "cannot be
/// resolved to branch", HEAD is left dangling, and renaming to the lowercase
/// name fails with "already exists" because it does.
///
/// Adopting the on-disk case up front asks git for the name it was going to
/// produce anyway, so the rename, HEAD, and the name we push all agree. Only
/// directory segments fold — the leaf is a plain file, and a leaf collision is
/// caught honestly by `git branch -m` as "already exists".
///
/// This keys off the branch list rather than probing the filesystem, so on a
/// *case-sensitive* filesystem it also rewrites a `Fix/x` that git would have
/// accepted alongside `fix/other`. That's deliberate: the result is still a
/// valid, sensible name, sibling ref directories differing only in case are a
/// footgun in their own right, and the caller reports the name it settled on.
pub fn canonicalize_branch_case(requested: &str, existing: &[String]) -> String {
    let segments: Vec<&str> = requested.split('/').collect();
    // A name with no `/` creates no directories, so nothing can capture it.
    if segments.len() < 2 {
        return requested.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        // The leaf is a file, not a directory.
        if i + 1 == segments.len() {
            out.push((*segment).to_string());
            break;
        }
        let on_disk = existing.iter().find_map(|branch| {
            let parts: Vec<&str> = branch.split('/').collect();
            // Segment `i` has to be a directory for this branch too, and sit
            // under the same parent path we've canonicalised so far.
            if parts.len() <= i + 1 || !parts[..i].iter().zip(&out).all(|(p, o)| *p == o.as_str()) {
                return None;
            }
            let part = parts[i];
            (part.eq_ignore_ascii_case(segment) && part != *segment).then(|| part.to_string())
        });
        out.push(on_disk.unwrap_or_else(|| (*segment).to_string()));
    }
    out.join("/")
}

/// Delete a local branch. `-D` (not `-d`) because a squash-merged branch never
/// looks merged to git — its commits were replaced by a single new one on base.
/// Fails while the branch is checked out anywhere, including in a worktree.
pub fn delete_branch_args(branch: &str) -> Vec<String> {
    vec!["branch".into(), "-D".into(), branch.into()]
}

/// Fetch a whole remote (no refspec), so every remote-tracking branch — the base
/// branch we're about to merge included — is up to date. A bare `git fetch
/// origin <base>` only reliably writes `FETCH_HEAD`.
pub fn fetch_args(remote: &str) -> Vec<String> {
    vec!["fetch".into(), remote.into()]
}

/// Push an explicit refspec to `remote` (which may be a URL, for a fork whose
/// head we're allowed to edit). No `-u`: the local review branch must not adopt
/// someone else's PR branch as its upstream.
pub fn push_refspec_args(remote: &str, refspec: &str) -> Vec<String> {
    vec!["push".into(), remote.into(), refspec.into()]
}

/// How many commits `branch` carries that its remote-tracking ref doesn't —
/// i.e. what a push would send. Purely local: whatever the last fetch or push
/// left in `origin/<branch>`, no network round-trip.
pub fn unpushed_count_args(branch: &str) -> Vec<String> {
    vec![
        "rev-list".into(),
        "--count".into(),
        format!("origin/{branch}..HEAD"),
    ]
}

/// The checked-out commit, abbreviated — the sha a fix's diff is based on, and
/// the one named back to the author once it's pushed.
pub fn head_sha_args() -> Vec<String> {
    vec!["rev-parse".into(), "--short".into(), "HEAD".into()]
}

pub fn remote_url_args(remote: &str) -> Vec<String> {
    vec!["remote".into(), "get-url".into(), remote.into()]
}

/// The URL to push a fork's PR branch to, derived from `head_repo`
/// (`owner/repo`) and the transport the user already uses for `origin`. Keeping
/// the transport matters: an HTTPS-authenticated user has no SSH key loaded,
/// and an SSH user often has no HTTPS credential helper.
pub fn fork_push_url(origin_url: &str, head_repo: &str) -> String {
    if origin_url.starts_with("git@") || origin_url.starts_with("ssh://") {
        format!("git@github.com:{head_repo}.git")
    } else {
        format!("https://github.com/{head_repo}.git")
    }
}

/// Every local branch plus every `origin/*` remote-tracking branch, shorthand.
/// Feeds the adopt-branch picker; `origin/HEAD` (a symref, not a branch) rides
/// along and is filtered by the caller.
pub fn list_all_branches_args() -> Vec<String> {
    vec![
        "for-each-ref".into(),
        "--format=%(refname:short)".into(),
        "refs/heads/".into(),
        "refs/remotes/origin/".into(),
    ]
}

/// The working tree's uncommitted changes to TRACKED files as an applicable
/// patch (`--binary` so images and the like survive). Untracked files are not
/// in the diff — see [`untracked_files_args`].
pub fn uncommitted_patch_args() -> Vec<String> {
    vec!["diff".into(), "HEAD".into(), "--binary".into()]
}

/// Apply a patch file produced by [`uncommitted_patch_args`] onto the working
/// tree. `--whitespace=nowarn`: adopted changes are applied verbatim, not
/// reviewed for hygiene.
pub fn apply_patch_args(patch_file: &str) -> Vec<String> {
    vec![
        "apply".into(),
        "--whitespace=nowarn".into(),
        patch_file.into(),
    ]
}

/// The working tree's untracked (and not ignored) files, NUL-separated. `-z`
/// so paths come out raw — without it, `core.quotePath` C-quotes any non-ASCII
/// filename and the quoted string no longer names a real file.
pub fn untracked_files_args() -> Vec<String> {
    vec![
        "ls-files".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
    ]
}

/// Merge `gitref` into the checked-out branch. `--no-edit` keeps git from
/// opening an editor for the merge commit message.
pub fn merge_args(gitref: &str) -> Vec<String> {
    vec!["merge".into(), "--no-edit".into(), gitref.into()]
}

/// The paths left unresolved by a stopped merge (`U` = unmerged).
pub fn conflicted_files_args() -> Vec<String> {
    vec![
        "diff".into(),
        "--name-only".into(),
        "--diff-filter=U".into(),
    ]
}

/// Whether a merge is stopped in progress here. Asks git for `MERGE_HEAD`
/// rather than probing `.git/MERGE_HEAD`: in a linked worktree `.git` is a
/// file pointing elsewhere, so the path probe would always miss.
pub fn merge_head_args() -> Vec<String> {
    vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        "MERGE_HEAD".into(),
    ]
}

/// Every path the working tree changed relative to HEAD — the candidate set for
/// a conflict-marker scan, alongside [`conflicted_files_args`]. Needed because
/// an agent that edits a conflicted file without `git add`ing it leaves the
/// path unmerged in the index even though it is resolved, and conversely a
/// resolved-then-staged file drops out of the unmerged list entirely.
pub fn changed_vs_head_args() -> Vec<String> {
    vec!["diff".into(), "--name-only".into(), "HEAD".into()]
}

/// True when `text` still carries both halves of a conflict marker pair.
/// Content-based on purpose: the index alone can't tell a resolved file from an
/// unresolved one once the agent has been editing (see [`changed_vs_head_args`]).
fn holds_conflict_markers(text: &str) -> bool {
    let mut open = false;
    let mut close = false;
    for line in text.lines() {
        open |= line.starts_with("<<<<<<< ");
        close |= line.starts_with(">>>>>>> ");
    }
    open && close
}

/// How merging a ref into the current branch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge committed, or the branch was already up to date.
    Clean,
    /// The merge stopped on conflicts, leaving the worktree mid-merge with these
    /// paths unresolved. Resolving them and committing completes the merge.
    Conflicted(Vec<String>),
}

// --- mutation trait (real + simulated) -------------------------------------

#[async_trait]
pub trait GitOps: Send + Sync {
    /// Cut a new `branch` from commit-ish `base` in a fresh worktree at `path`.
    async fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        path: &Path,
        base: &str,
    ) -> Result<()>;
    async fn remove_worktree(&self, repo: &Path, path: &Path) -> Result<()>;
    /// Attach a worktree at `path` to an existing local `branch` (no new branch).
    async fn worktree_add_existing(&self, repo: &Path, branch: &str, path: &Path) -> Result<()>;
    /// Attach a DETACHED worktree at `commitish` (claims no branch, so it works
    /// even when `commitish` is a branch checked out elsewhere).
    async fn worktree_add_detached(&self, repo: &Path, path: &Path, commitish: &str) -> Result<()>;
    /// Fetch a PR's head into a local branch (works for forks via the pull ref).
    async fn fetch_pr(&self, repo: &Path, pr_number: u64, local_branch: &str) -> Result<()>;
    /// Move HEAD to `gitref` while keeping the working tree, so committed work
    /// becomes uncommitted changes — `git reset HEAD^`, generalized to any ref.
    async fn reset_mixed(&self, dir: &Path, gitref: &str) -> Result<()>;
    /// Rename a local branch (`git branch -m <old> <new>`).
    async fn rename_branch(&self, dir: &Path, old: &str, new: &str) -> Result<()>;
    /// Every local branch name, as git actually holds it. Backends that don't
    /// model refs (the simulator, test doubles) report none; callers must read
    /// an empty list as "no information", never as "the repo has no branches".
    async fn local_branches(&self, _dir: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Every local branch plus every `origin/*` remote-tracking branch. Same
    /// "empty means no information" contract as [`Self::local_branches`].
    async fn list_all_branches(&self, _dir: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// The working tree's uncommitted changes to tracked files, as a `--binary`
    /// patch (empty when clean). Raw bytes, not a `String` — a text diff of a
    /// non-UTF-8 file must survive the round-trip byte-for-byte or `git apply`
    /// rejects it. Backends that don't model a working tree report an empty
    /// patch.
    async fn uncommitted_patch(&self, _dir: &Path) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    /// Apply a patch from [`Self::uncommitted_patch`] onto `dir`'s working tree.
    async fn apply_patch(&self, _dir: &Path, _patch: &[u8]) -> Result<()> {
        Ok(())
    }
    /// The working tree's untracked (not ignored) files, repo-relative.
    async fn untracked_files(&self, _dir: &Path) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    /// Force-delete a local branch. The branch must not be checked out anywhere
    /// — remove its worktree first.
    async fn delete_branch(&self, repo: &Path, branch: &str) -> Result<()>;
    /// Fetch every branch of `remote`, refreshing its remote-tracking refs.
    async fn fetch(&self, dir: &Path, remote: &str) -> Result<()>;
    /// Merge `gitref` into the checked-out branch. A conflicted merge is an
    /// outcome, not an error — the worktree is left mid-merge for the caller to
    /// resolve. Any other failure (dirty tree, unknown ref) is an error.
    async fn merge_ref(&self, dir: &Path, gitref: &str) -> Result<MergeOutcome>;
    /// Stage everything and commit. Returns `true` if a commit landed, `false`
    /// if there was nothing to commit (a clean tree) — the caller uses this to
    /// tell "the run produced work" from "the run changed nothing".
    async fn commit_all(&self, dir: &Path, message: &str) -> Result<bool>;
    /// Whether `dir`'s HEAD carries any commit beyond its merge base with
    /// `base`. Backends that don't model history report `false`; callers must
    /// read that as "no information", never as proof the branch is empty.
    async fn branch_has_commits(&self, _dir: &Path, _base: &str) -> Result<bool> {
        Ok(false)
    }
    /// Whether a merge is stopped in progress in `dir` (`MERGE_HEAD` exists).
    /// Backends that don't model merge state report `false`, which callers must
    /// read as "no information" — it only ever *skips* the conflict-run checks.
    async fn merge_in_progress(&self, _dir: &Path) -> Result<bool> {
        Ok(false)
    }
    /// The paths in `dir` that still carry conflict markers. Empty means either
    /// "resolved" or "this backend doesn't model working trees"; callers use a
    /// non-empty answer as proof that the run left conflicts behind.
    async fn unresolved_conflicts(&self, _dir: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Discard every uncommitted change in `dir` — tracked edits (`git reset
    /// --hard`) and untracked files (`git clean -fd`) — returning the tree to
    /// HEAD. Backends that don't model working-tree state treat it as a no-op.
    async fn discard_changes(&self, _dir: &Path) -> Result<()> {
        Ok(())
    }
    /// Push `branch` to `origin`, setting upstream.
    async fn push(&self, dir: &Path, branch: &str) -> Result<()>;
    /// Push an explicit refspec to `remote` (a remote name or a URL), without
    /// touching the local branch's upstream. Backends that don't model remotes
    /// treat it as a no-op.
    async fn push_refspec(&self, _dir: &Path, _remote: &str, _refspec: &str) -> Result<()> {
        Ok(())
    }
    /// The abbreviated sha of `dir`'s HEAD. Backends that don't model history
    /// report an empty string, which callers must read as "no information".
    async fn head_sha(&self, _dir: &Path) -> Result<String> {
        Ok(String::new())
    }
    /// A remote's configured URL (empty when unknown — see [`Self::head_sha`]).
    async fn remote_url(&self, _dir: &Path, _remote: &str) -> Result<String> {
        Ok(String::new())
    }
    /// Whether `branch` holds commits the remote doesn't have, so a push would
    /// actually send something (and re-trigger CI). Backends that don't model
    /// remotes report `false`, and so does a repo whose remote-tracking ref is
    /// missing: callers use this to decide whether to *invalidate* a known-good
    /// state, and guessing "yes" there costs more than guessing "no".
    async fn branch_ahead_of_remote(&self, _dir: &Path, _branch: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Shells out to the real `git` binary.
pub struct RealGit;

#[async_trait]
impl GitOps for RealGit {
    async fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        path: &Path,
        base: &str,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(
            repo,
            &worktree_add_args(branch, &path.to_string_lossy(), base),
        )
        .await
        .map(|_| ())
    }

    async fn remove_worktree(&self, repo: &Path, path: &Path) -> Result<()> {
        run_git(repo, &worktree_remove_args(&path.to_string_lossy()))
            .await
            .map(|_| ())
    }

    async fn worktree_add_existing(&self, repo: &Path, branch: &str, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(
            repo,
            &worktree_add_existing_args(branch, &path.to_string_lossy()),
        )
        .await
        .map(|_| ())
    }

    async fn fetch_pr(&self, repo: &Path, pr_number: u64, local_branch: &str) -> Result<()> {
        run_git(repo, &fetch_pr_args(pr_number, local_branch))
            .await
            .map(|_| ())
    }

    async fn worktree_add_detached(&self, repo: &Path, path: &Path, commitish: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(
            repo,
            &worktree_add_detached_args(&path.to_string_lossy(), commitish),
        )
        .await
        .map(|_| ())
    }

    async fn reset_mixed(&self, dir: &Path, gitref: &str) -> Result<()> {
        run_git(dir, &reset_mixed_args(gitref)).await.map(|_| ())
    }

    async fn rename_branch(&self, dir: &Path, old: &str, new: &str) -> Result<()> {
        run_git(dir, &rename_branch_args(old, new))
            .await
            .map(|_| ())
    }

    async fn local_branches(&self, dir: &Path) -> Result<Vec<String>> {
        Ok(run_git(dir, &list_branches_args())
            .await?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn list_all_branches(&self, dir: &Path) -> Result<Vec<String>> {
        Ok(run_git(dir, &list_all_branches_args())
            .await?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn uncommitted_patch(&self, dir: &Path) -> Result<Vec<u8>> {
        run_git_bytes(dir, &uncommitted_patch_args()).await
    }

    async fn apply_patch(&self, dir: &Path, patch: &[u8]) -> Result<()> {
        // `run_git` has no stdin plumbing, so the patch goes through a temp
        // file — outside the worktree, so `git add -A` can never sweep the
        // patch file itself onto the branch.
        let file = std::env::temp_dir().join(format!("usine-{}.patch", uuid::Uuid::new_v4()));
        std::fs::write(&file, patch)?;
        let out = run_git(dir, &apply_patch_args(&file.to_string_lossy())).await;
        let _ = std::fs::remove_file(&file);
        out.map(|_| ())
    }

    async fn untracked_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let out = run_git_bytes(dir, &untracked_files_args()).await?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
            .map(path_from_bytes)
            .collect())
    }

    async fn delete_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        run_git(repo, &delete_branch_args(branch))
            .await
            .map(|_| ())
            .map_err(|e| {
                // Git's refusal wording varies by version ("used by worktree
                // at" vs "checked out at"); name the worktree conflict
                // ourselves so callers can rely on one stable message.
                let msg = e.to_string();
                if msg.contains("checked out at") || msg.contains("worktree") {
                    CoreError::other(format!(
                        "branch '{branch}' is still checked out in a worktree — remove the worktree first ({msg})"
                    ))
                } else {
                    e
                }
            })
    }

    async fn fetch(&self, dir: &Path, remote: &str) -> Result<()> {
        run_git(dir, &fetch_args(remote)).await.map(|_| ())
    }

    async fn merge_ref(&self, dir: &Path, gitref: &str) -> Result<MergeOutcome> {
        let Err(e) = run_git(dir, &merge_args(gitref)).await else {
            return Ok(MergeOutcome::Clean);
        };
        // `git merge` exits non-zero both for a conflict and for a genuine
        // failure (dirty tree, unknown ref, a merge already in progress). Only
        // unmerged paths distinguish the two — without them, the worktree isn't
        // mid-merge and there's nothing for an agent to resolve, so report the
        // original error rather than sending it into an empty conflict.
        let files: Vec<String> = run_git(dir, &conflicted_files_args())
            .await
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if files.is_empty() {
            return Err(e);
        }
        Ok(MergeOutcome::Conflicted(files))
    }

    async fn commit_all(&self, dir: &Path, message: &str) -> Result<bool> {
        run_git(dir, &["add".into(), "-A".into()]).await?;
        // `git commit` exits non-zero when there's nothing to commit; report that
        // as `Ok(false)` (no commit landed) rather than an error, so the caller
        // can distinguish it from a run that actually produced changes.
        let out = timeout(
            GIT_TIMEOUT,
            Command::new("git")
                .current_dir(dir)
                .args(["commit", "--no-verify", "-m", message])
                .output(),
        )
        .await
        .map_err(|_| CoreError::other("git commit timed out"))??;
        if out.status.success() {
            return Ok(true);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("nothing to commit") || stderr.contains("nothing to commit") {
            return Ok(false);
        }
        Err(CoreError::other(format!(
            "git commit failed: {}",
            stderr.trim()
        )))
    }

    async fn branch_has_commits(&self, dir: &Path, base: &str) -> Result<bool> {
        Ok(!log_subjects(dir, base, "HEAD")?.is_empty())
    }

    async fn merge_in_progress(&self, dir: &Path) -> Result<bool> {
        Ok(run_git(dir, &merge_head_args()).await.is_ok())
    }

    async fn unresolved_conflicts(&self, dir: &Path) -> Result<Vec<String>> {
        let mut candidates: Vec<String> = Vec::new();
        for args in [conflicted_files_args(), changed_vs_head_args()] {
            for line in run_git(dir, &args).await.unwrap_or_default().lines() {
                let path = line.trim();
                if !path.is_empty() && !candidates.iter().any(|c| c == path) {
                    candidates.push(path.to_string());
                }
            }
        }
        // Read the files themselves: only surviving markers prove a path is
        // still unresolved (a binary/unreadable file simply can't carry them).
        Ok(candidates
            .into_iter()
            .filter(|p| {
                std::fs::read_to_string(dir.join(p))
                    .map(|t| holds_conflict_markers(&t))
                    .unwrap_or(false)
            })
            .collect())
    }

    async fn discard_changes(&self, dir: &Path) -> Result<()> {
        run_git(dir, &["reset".into(), "--hard".into()]).await?;
        run_git(dir, &["clean".into(), "-fd".into()])
            .await
            .map(|_| ())
    }

    async fn push(&self, dir: &Path, branch: &str) -> Result<()> {
        run_git(
            dir,
            &["push".into(), "-u".into(), "origin".into(), branch.into()],
        )
        .await
        .map(|_| ())
    }

    async fn push_refspec(&self, dir: &Path, remote: &str, refspec: &str) -> Result<()> {
        run_git(dir, &push_refspec_args(remote, refspec))
            .await
            .map(|_| ())
    }

    async fn head_sha(&self, dir: &Path) -> Result<String> {
        Ok(run_git(dir, &head_sha_args()).await?.trim().to_string())
    }

    async fn branch_ahead_of_remote(&self, dir: &Path, branch: &str) -> Result<bool> {
        // No remote-tracking ref (never pushed, or pruned) fails the rev-list;
        // the documented `false` is what the caller wants there.
        let out = run_git(dir, &unpushed_count_args(branch)).await?;
        Ok(out.trim().parse::<u64>().unwrap_or(0) > 0)
    }

    async fn remote_url(&self, dir: &Path, remote: &str) -> Result<String> {
        Ok(run_git(dir, &remote_url_args(remote))
            .await?
            .trim()
            .to_string())
    }
}

/// No-op git for Phase A so the board flows without a real repo.
pub struct SimGit;

#[async_trait]
impl GitOps for SimGit {
    async fn create_worktree(&self, _: &Path, _: &str, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn remove_worktree(&self, _: &Path, _: &Path) -> Result<()> {
        Ok(())
    }
    async fn worktree_add_existing(&self, _: &Path, _: &str, _: &Path) -> Result<()> {
        Ok(())
    }
    async fn worktree_add_detached(&self, _: &Path, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn fetch_pr(&self, _: &Path, _: u64, _: &str) -> Result<()> {
        Ok(())
    }
    async fn reset_mixed(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn rename_branch(&self, _: &Path, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn delete_branch(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn fetch(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn merge_ref(&self, _: &Path, _: &str) -> Result<MergeOutcome> {
        Ok(MergeOutcome::Clean)
    }
    async fn commit_all(&self, _: &Path, _: &str) -> Result<bool> {
        // Pretend the run committed so the sim flow advances.
        Ok(true)
    }
    async fn push(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
}

async fn run_git(cwd: &Path, args: &[String]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run_git_bytes(cwd, args).await?).to_string())
}

/// Like [`run_git`], but hands back stdout untouched — for output that must
/// stay byte-exact (patches, `-z` path listings), where a lossy UTF-8 pass
/// would corrupt non-UTF-8 content.
async fn run_git_bytes(cwd: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = timeout(
        GIT_TIMEOUT,
        Command::new("git").current_dir(cwd).args(args).output(),
    )
    .await
    .map_err(|_| {
        CoreError::other(format!(
            "git {} timed out after {}s",
            args.join(" "),
            GIT_TIMEOUT.as_secs()
        ))
    })??;
    if !out.status.success() {
        return Err(CoreError::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// A repo-relative path from git's raw (`-z`) output bytes. On Unix, paths are
/// bytes and convert losslessly; elsewhere fall back to lossy UTF-8.
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).to_string())
    }
}

// --- read-only inspection (git2) -------------------------------------------

/// Current branch shorthand of a repo/worktree, for UI display.
pub fn current_branch(repo: &Path) -> Result<String> {
    let r = git2::Repository::open(repo)?;
    let head = r.head()?;
    Ok(head.shorthand().unwrap_or("HEAD").to_string())
}

/// The branch new worktrees should fork from (and PRs should target).
///
/// Prefers `dev` — the user's integration branch — then `main`/`master`, then
/// whatever HEAD currently points at. Falls back to `dev` when the repo can't
/// be inspected so the stated default still wins. A name is only returned if a
/// matching local *or* remote-tracking branch actually exists, so we never try
/// to branch off a `dev` that isn't there.
pub fn detect_base_branch(repo: &Path) -> String {
    if let Ok(r) = git2::Repository::open(repo) {
        for name in ["dev", "main", "master"] {
            let exists = r.find_branch(name, git2::BranchType::Local).is_ok()
                || r.revparse_single(&format!("origin/{name}")).is_ok();
            if exists {
                return name.to_string();
            }
        }
    }
    // No conventional integration branch found: fall back to the current branch,
    // else the stated default.
    current_branch(repo).unwrap_or_else(|_| "dev".to_string())
}

/// Resolve a ref name to an Oid, tolerating a bare branch name that only exists
/// as a remote-tracking branch (`origin/<name>`).
fn resolve_oid(r: &git2::Repository, name: &str) -> Result<git2::Oid> {
    let obj = r
        .revparse_single(name)
        .or_else(|_| r.revparse_single(&format!("origin/{name}")))?;
    Ok(obj.id())
}

/// The ref a *contributor's* PR should be diffed against: `origin/<base>` when
/// that remote-tracking branch exists, else `base` unchanged.
///
/// Deliberately the opposite preference from [`resolve_oid`]. A local branch
/// named `dev` is an artifact of the user's own checkout and can sit arbitrarily
/// far behind the remote; taking the merge base against it puts the fork point
/// back before everything merged into `dev` since the last pull, so the PR's
/// diff swells with other people's already-merged commits. The forge computes
/// the PR diff against the *remote* base, and so must we.
///
/// Only meaningful right after the remote has been fetched — see the callers.
pub fn remote_tracking_base(repo: &Path, base: &str) -> String {
    let remote = format!("origin/{base}");
    match git2::Repository::open(repo) {
        Ok(r) if r.revparse_single(&remote).is_ok() => remote,
        _ => base.to_string(),
    }
}

/// The best common ancestor of `base` and `head`, as a hex Oid string. Used to
/// diff a branch against where it forked from `base`.
pub fn merge_base(repo: &Path, base: &str, head: &str) -> Result<String> {
    let r = git2::Repository::open(repo)?;
    let base_oid = resolve_oid(&r, base)?;
    let head_oid = resolve_oid(&r, head)?;
    Ok(r.merge_base(base_oid, head_oid)?.to_string())
}

/// The subjects of the commits on `head` since its merge base with `base`,
/// newest first. Empty when `head` sits on (or behind) `base` — the signal the
/// adopt flow uses to refuse an empty adoption. Both names tolerate bare branch
/// names that only exist as `origin/<name>` (see [`resolve_oid`]).
pub fn log_subjects(repo: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let r = git2::Repository::open(repo)?;
    let base_oid = resolve_oid(&r, base)?;
    let head_oid = resolve_oid(&r, head)?;
    let fork = r.merge_base(base_oid, head_oid)?;
    let mut walk = r.revwalk()?;
    walk.push(head_oid)?;
    walk.hide(fork)?;
    let mut subjects = Vec::new();
    for oid in walk {
        let commit = r.find_commit(oid?)?;
        subjects.push(
            commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or("(no message)")
                .to_string(),
        );
    }
    Ok(subjects)
}

/// The working tree that has local `branch` checked out — the main checkout or
/// any linked worktree — or `None` when nothing does (or the branch is
/// remote-only). Pairs with [`is_dirty`] for the adopt flow's dirty probe, and
/// tells retire-original that deleting would fail.
pub fn checkout_of_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let r = git2::Repository::open(repo).ok()?;
    let refname = format!("refs/heads/{branch}");
    let head_is_branch = |repo: &git2::Repository| {
        repo.head()
            .ok()
            .and_then(|h| h.name().ok().map(|n| n == refname))
            .unwrap_or(false)
    };
    if head_is_branch(&r) {
        return r.workdir().map(Path::to_path_buf);
    }
    for name in r.worktrees().ok()?.iter().flatten().flatten() {
        let Ok(wt) = r.find_worktree(name) else {
            continue;
        };
        if let Ok(wr) = git2::Repository::open(wt.path()) {
            if head_is_branch(&wr) {
                return Some(wt.path().to_path_buf());
            }
        }
    }
    None
}

/// Whether `name` resolves to a commit in the repo (tolerating bare branch
/// names that only exist as `origin/<name>`).
pub fn commitish_exists(repo: &Path, name: &str) -> bool {
    git2::Repository::open(repo)
        .ok()
        .is_some_and(|r| resolve_oid(&r, name).is_ok())
}

/// Whether the working tree has uncommitted changes — matching exactly what
/// `git status` reports (tracked modifications *and* untracked files).
///
/// This shells out instead of using `git2` on purpose: libgit2's status can
/// diverge from the CLI (`.gitattributes`/CRLF clean-smudge filters, a racy
/// index stat-cache, ignored/nested-worktree handling), which made it report a
/// clean tree as dirty. `git status --porcelain` is the source of truth the
/// user sees, and this is not a hot UI path, so the process spawn is fine.
pub async fn is_dirty(repo: &Path) -> Result<bool> {
    let out = run_git(repo, &["status".into(), "--porcelain".into()]).await?;
    Ok(!out.trim().is_empty())
}

/// Make git ignore `patterns` in every checkout of the repo `worktree` belongs
/// to, by appending them to the shared `info/exclude` — never the project's
/// `.gitignore`, because these are usine runtime artifacts (the preview info /
/// port-offset files written into card worktrees), not project files. Without
/// this a `git add -A` — finalize's auto-commit, or one the agent runs itself —
/// would sweep them onto the card's branch. Best-effort: on failure the caller
/// proceeds without the guard.
pub async fn ensure_excluded(worktree: &Path, patterns: &[&str]) {
    let args: Vec<String> = [
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "info/exclude",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let Ok(out) = run_git(worktree, &args).await else {
        return;
    };
    let path = PathBuf::from(out.trim());
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let additions = exclude_additions(&existing, patterns);
    if additions.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for p in additions {
        content.push_str(&p);
        content.push('\n');
    }
    let _ = std::fs::write(&path, content);
}

/// The subset of `patterns` not already present (as whole lines) in an exclude
/// file's `existing` content.
fn exclude_additions(existing: &str, patterns: &[&str]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| !existing.lines().any(|l| l.trim() == **p))
        .map(|p| p.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_refspec_sets_no_upstream() {
        let args = push_refspec_args("origin", "usine-review/12:feat/x");
        assert_eq!(args, vec!["push", "origin", "usine-review/12:feat/x"]);
        assert!(!args.iter().any(|a| a == "-u"));
    }

    #[test]
    fn fork_push_url_keeps_the_origin_transport() {
        assert_eq!(
            fork_push_url("git@github.com:me/repo.git", "octocat/repo"),
            "git@github.com:octocat/repo.git"
        );
        assert_eq!(
            fork_push_url("ssh://git@github.com/me/repo.git", "octocat/repo"),
            "git@github.com:octocat/repo.git"
        );
        assert_eq!(
            fork_push_url("https://github.com/me/repo.git", "octocat/repo"),
            "https://github.com/octocat/repo.git"
        );
        // No origin URL to read → HTTPS, which needs no key loaded.
        assert_eq!(
            fork_push_url("", "octocat/repo"),
            "https://github.com/octocat/repo.git"
        );
    }

    #[test]
    fn exclude_additions_skips_present_lines() {
        let existing = "# usine\n.wt-offset\n";
        assert_eq!(
            exclude_additions(existing, &[".wt-offset", ".usine-preview.json"]),
            vec![".usine-preview.json".to_string()]
        );
        // Idempotent: everything present → nothing to add.
        assert!(exclude_additions(".usine-preview.json\n.wt-offset\n", &[".wt-offset"]).is_empty());
        assert_eq!(exclude_additions("", &[".wt-offset"]), vec![".wt-offset"]);
    }

    #[test]
    fn worktree_add_is_well_formed() {
        let args = worktree_add_args("usine/feat", "/repo/.usine/wt/abc", "origin/main");
        assert_eq!(args[0], "worktree");
        assert_eq!(args[1], "add");
        assert!(args.contains(&"--no-track".to_string()));
        assert!(args.contains(&"-b".to_string()));
        assert!(args.contains(&"usine/feat".to_string()));
        assert_eq!(args.last().unwrap(), "origin/main");
    }

    #[test]
    fn reset_mixed_is_well_formed() {
        assert_eq!(
            reset_mixed_args("abc123"),
            vec!["reset", "--mixed", "abc123"]
        );
    }

    #[test]
    fn sanitize_strips_what_git_rejects() {
        assert_eq!(sanitize_branch_name("  feat/add oauth  "), "feat/add-oauth");
        assert_eq!(
            sanitize_branch_name("feat/a~b^c:d?e*f[g\\h"),
            "feat/a-b-c-d-e-f-g-h"
        );
        assert_eq!(sanitize_branch_name("feat//a..b"), "feat/a.b");
        assert_eq!(sanitize_branch_name("/feat/-thing-/"), "feat/thing");
        assert_eq!(sanitize_branch_name("feat/thing.lock"), "feat/thing");
        // `@{` is illegal in a ref; the digits it wrapped are harmless to keep.
        assert_eq!(sanitize_branch_name("feat/x@{1}"), "feat/x-1");
    }

    /// `JIRA-123/…` is a real convention — sanitising must not lowercase it.
    #[test]
    fn sanitize_preserves_case() {
        assert_eq!(
            sanitize_branch_name("JIRA-123/Fix-Thing"),
            "JIRA-123/Fix-Thing"
        );
    }

    /// Nothing usable left is reported as empty, not as a bogus name.
    #[test]
    fn sanitize_can_come_back_empty() {
        assert_eq!(sanitize_branch_name("///"), "");
        assert_eq!(sanitize_branch_name("@"), "");
        assert_eq!(sanitize_branch_name("   "), "");
    }

    /// The bug: an existing `fix/` directory captures a requested `Fix/` on a
    /// case-insensitive filesystem, so ask for the name git would really create.
    #[test]
    fn canonicalize_adopts_an_existing_directorys_case() {
        let existing = vec!["fix/some-other".to_string(), "main".to_string()];
        assert_eq!(
            canonicalize_branch_case("Fix/displayed-last-name-ordering", &existing),
            "fix/displayed-last-name-ordering"
        );
    }

    /// The leaf is a file, not a directory — `git branch -m` reports a collision
    /// there honestly, so it must be left alone.
    #[test]
    fn canonicalize_leaves_the_leaf_alone() {
        let existing = vec!["feat/bar".to_string()];
        assert_eq!(canonicalize_branch_case("feat/Bar", &existing), "feat/Bar");
    }

    #[test]
    fn canonicalize_is_a_no_op_without_a_collision() {
        let existing = vec!["fix/other".to_string()];
        assert_eq!(
            canonicalize_branch_case("Feat/thing", &existing),
            "Feat/thing"
        );
        assert_eq!(
            canonicalize_branch_case("fix/thing", &existing),
            "fix/thing"
        );
        assert_eq!(
            canonicalize_branch_case("standalone", &existing),
            "standalone"
        );
        assert_eq!(canonicalize_branch_case("Fix/thing", &[]), "Fix/thing");
    }

    /// Only a directory sharing the same parent path can capture a segment.
    #[test]
    fn canonicalize_matches_nested_directories_by_parent() {
        let existing = vec!["team/fix/other".to_string(), "Fix/elsewhere".to_string()];
        assert_eq!(
            canonicalize_branch_case("team/Fix/thing", &existing),
            "team/fix/thing"
        );
        // `Fix/elsewhere` lives at the root, so it can't capture `a/Fix/…`.
        assert_eq!(
            canonicalize_branch_case("a/Fix/thing", &existing),
            "a/Fix/thing"
        );
    }

    #[test]
    fn list_branches_reports_real_names() {
        assert_eq!(
            list_branches_args(),
            vec!["for-each-ref", "--format=%(refname:short)", "refs/heads/"]
        );
    }

    #[test]
    fn rename_branch_is_well_formed() {
        assert_eq!(
            rename_branch_args("usine/old", "feat/new"),
            vec!["branch", "-m", "usine/old", "feat/new"]
        );
    }

    /// `-d` refuses a squash-merged branch (its commits never land on base as-is).
    #[test]
    fn delete_branch_forces() {
        assert_eq!(
            delete_branch_args("feat/licensee-deletion"),
            vec!["branch", "-D", "feat/licensee-deletion"]
        );
    }

    /// `--no-edit`: a merge that opens `$EDITOR` in a headless worktree hangs
    /// until the git timeout kills it.
    #[test]
    fn merge_never_opens_an_editor() {
        assert_eq!(
            merge_args("origin/dev"),
            vec!["merge", "--no-edit", "origin/dev"]
        );
    }

    /// Fetching the remote wholesale (not `fetch origin dev`) is what refreshes
    /// `origin/dev` itself, which is the ref the merge then names.
    #[test]
    fn fetch_takes_no_refspec() {
        assert_eq!(fetch_args("origin"), vec!["fetch", "origin"]);
    }

    #[test]
    fn conflicted_files_selects_unmerged_paths() {
        let args = conflicted_files_args();
        assert_eq!(args[0], "diff");
        assert!(args.iter().any(|a| a == "--diff-filter=U"));
        assert!(args.iter().any(|a| a == "--name-only"));
    }

    /// `--quiet` is what makes a missing MERGE_HEAD a plain non-zero exit
    /// instead of an error on stderr, and `--verify` rejects anything that
    /// isn't a single resolvable ref.
    #[test]
    fn merge_head_probe_is_quiet_and_verified() {
        let args = merge_head_args();
        assert_eq!(args[0], "rev-parse");
        assert!(args.iter().any(|a| a == "--quiet"));
        assert!(args.iter().any(|a| a == "--verify"));
        assert_eq!(args.last().unwrap(), "MERGE_HEAD");
    }

    #[test]
    fn changed_vs_head_lists_working_tree_edits() {
        assert_eq!(changed_vs_head_args(), vec!["diff", "--name-only", "HEAD"]);
    }

    /// Both halves must be present: a lone `<<<<<<<` in prose (or a test
    /// fixture describing conflicts) isn't an unresolved merge.
    #[test]
    fn conflict_markers_need_both_halves() {
        assert!(holds_conflict_markers(
            "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> origin/dev\n"
        ));
        assert!(!holds_conflict_markers("<<<<<<< HEAD\nours only\n"));
        assert!(!holds_conflict_markers("a\nb\n"));
        // Markers are line-leading; a mention inside a sentence is not one.
        assert!(!holds_conflict_markers("we saw <<<<<<< and >>>>>>> here\n"));
    }

    #[test]
    fn worktree_add_existing_omits_new_branch_flag() {
        let args = worktree_add_existing_args("usine-review/42", "/wt/review-42");
        assert_eq!(args[0], "worktree");
        assert_eq!(args[1], "add");
        assert!(!args.contains(&"-b".to_string()));
        assert_eq!(args[2], "/wt/review-42");
        assert_eq!(args[3], "usine-review/42");
    }

    #[test]
    fn fetch_pr_uses_pull_head_refspec() {
        let args = fetch_pr_args(42, "usine-review/42");
        assert_eq!(args[0], "fetch");
        assert_eq!(args[1], "origin");
        assert_eq!(args[2], "+pull/42/head:usine-review/42");
    }

    /// The picker's listing must cover remote-tracking branches too — a branch
    /// pushed from another machine has no local ref to adopt from.
    #[test]
    fn list_all_branches_covers_local_and_origin() {
        assert_eq!(
            list_all_branches_args(),
            vec![
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/",
                "refs/remotes/origin/"
            ]
        );
    }

    /// `--binary` so an adopted image/asset change survives the patch round-trip.
    #[test]
    fn uncommitted_patch_is_binary_safe() {
        assert_eq!(uncommitted_patch_args(), vec!["diff", "HEAD", "--binary"]);
    }

    #[test]
    fn apply_patch_is_well_formed() {
        assert_eq!(
            apply_patch_args("/tmp/x.patch"),
            vec!["apply", "--whitespace=nowarn", "/tmp/x.patch"]
        );
    }

    /// `--exclude-standard` keeps ignored files (build output, node_modules)
    /// out of an adopted dirty snapshot; `-z` keeps non-ASCII paths from being
    /// C-quoted by `core.quotePath`.
    #[test]
    fn untracked_files_respects_ignores() {
        assert_eq!(
            untracked_files_args(),
            vec!["ls-files", "--others", "--exclude-standard", "-z"]
        );
    }

    #[test]
    fn worktree_add_detached_is_well_formed() {
        let args = worktree_add_detached_args("/wt/selfreview", "usine/feat");
        assert_eq!(&args[0..3], &["worktree", "add", "--detach"]);
        assert_eq!(args[3], "/wt/selfreview");
        assert_eq!(args[4], "usine/feat");
    }
}

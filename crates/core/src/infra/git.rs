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
    /// Push `branch` to `origin`, setting upstream.
    async fn push(&self, dir: &Path, branch: &str) -> Result<()>;
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

    async fn delete_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        run_git(repo, &delete_branch_args(branch))
            .await
            .map(|_| ())
            .map_err(|e| {
                // git's wording for this refusal varies by version ("used by
                // worktree at" vs "checked out at"); give callers one stable
                // message naming the worktree conflict.
                let msg = e.to_string();
                if msg.contains("used by worktree") || msg.contains("checked out at") {
                    CoreError::other(format!(
                        "branch '{branch}' is still held by a worktree; remove the worktree first ({msg})"
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

    async fn push(&self, dir: &Path, branch: &str) -> Result<()> {
        run_git(
            dir,
            &["push".into(), "-u".into(), "origin".into(), branch.into()],
        )
        .await
        .map(|_| ())
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
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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

    #[test]
    fn worktree_add_detached_is_well_formed() {
        let args = worktree_add_detached_args("/wt/selfreview", "usine/feat");
        assert_eq!(&args[0..3], &["worktree", "add", "--detach"]);
        assert_eq!(args[3], "/wt/selfreview");
        assert_eq!(args[4], "usine/feat");
    }
}

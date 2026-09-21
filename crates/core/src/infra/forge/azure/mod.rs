//! Azure DevOps Services (Azure Repos + Azure Pipelines), over its REST API
//! (api-version 7.1).
//!
//! [`AzureForges`] is the [`ForgeFactory`] the registry routes Azure projects
//! through: it reads the repository's coordinates off `origin` and hands out
//! one [`AzureForge`] per repository, sharing the HTTP client, the credentials
//! and each organization's "who am I" answer between them.
//!
//! Where Azure's model differs from GitHub's, the adapter absorbs it so the
//! executor's logic holds unchanged:
//! - comments live in threads, and Azure's own activity (votes, pushes, merge
//!   attempts) arrives as threads too — filtered out, or the comment count
//!   that must reach zero before an approval clears the merge would never get
//!   there (see `wire::thread_comments`);
//! - reviews are votes with no body; the feedback is in threads, which triage
//!   reads like inline comments, and PR-level threads can be replied to;
//! - a declined comment is closed as *won't fix* ([`Forge::decline_comment`]),
//!   since an active thread can block completion under a comment policy;
//! - completing a PR is asynchronous and policy-gated, so [`Forge::merge`]
//!   waits for the merge to land and names the blocking policies otherwise;
//! - CI is the PR's build-validation and status policies plus posted statuses,
//!   with a stale green build — or a status posted before the latest push —
//!   read as pending.
//!
//! The executor reads a PR's comments, reviews, threads, checks,
//! mergeability and live state back to back on every poll; the PR and its
//! threads are each fetched once per such burst (a few-second cache,
//! invalidated by every write), so a refresh costs four requests, not ten.

mod client;
#[cfg(test)]
mod fake_tests;
mod wire;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};

use super::remote::{encode_segment, parse_azure_remote, AzureRepo};
use super::{
    FailedCheck, Forge, ForgeFactory, LivePrState, OpenPr, PrPushTarget, PrSummary, ReviewScope,
};
use crate::domain::model::{
    CheckStatus, DraftComment, Mergeable, PrInfo, PrState, ReviewComment, ReviewEvent,
    ReviewSummary, ReviewThread,
};
use crate::error::{CoreError, Result};

pub use client::PAT_ENV;

/// How long one read of a PR (or its threads, evaluations, statuses) serves
/// the reads that follow it — long enough to cover one poll's burst of calls
/// for the same PR, short enough that nothing observable goes stale.
const READ_CACHE: Duration = Duration::from_secs(5);

/// How long `merge` waits for Azure to finish completing a PR, and how often
/// it looks.
const MERGE_WAIT: Duration = Duration::from_secs(90);
const MERGE_POLL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};

/// How many failed steps' logs `failed_check_logs` pulls per build.
const LOGS_PER_BUILD: usize = 3;

/// One organization's connection: the shared HTTP client, the org's REST
/// roots, and the identity its credentials authenticate as (looked up once,
/// retried until it works).
struct Org {
    http: Arc<client::Http>,
    /// `https://dev.azure.com/{org}`.
    url: String,
    /// `https://vssps.dev.azure.com/{org}` — identities live on their own host.
    vssps: String,
    me: tokio::sync::OnceCell<(String, String)>,
}

impl Org {
    /// `(identity id, handle)` of the authenticated user.
    async fn me(&self) -> Result<&(String, String)> {
        self.me
            .get_or_try_init(|| async {
                let v = self
                    .http
                    .json(
                        Method::GET,
                        &format!("{}/_apis/connectionData", self.url),
                        None,
                    )
                    .await?;
                wire::connection_identity(&v).ok_or_else(|| {
                    CoreError::forge("Azure DevOps didn't say who the credentials belong to")
                })
            })
            .await
    }
}

/// The Azure DevOps [`ForgeFactory`]: an [`AzureForge`] per repository,
/// sharing credentials and per-organization state.
pub struct AzureForges {
    http: Arc<client::Http>,
    /// The service roots (`https://dev.azure.com`, `https://vssps.dev.azure.com`)
    /// — a fake server's address in tests.
    roots: (String, String),
    orgs: Mutex<HashMap<String, Arc<Org>>>,
    repos: Mutex<HashMap<AzureRepo, Arc<AzureForge>>>,
}

impl Default for AzureForges {
    fn default() -> Self {
        Self::new()
    }
}

impl AzureForges {
    pub fn new() -> Self {
        AzureForges {
            http: Arc::new(client::Http::new(None)),
            roots: (
                "https://dev.azure.com".into(),
                "https://vssps.dev.azure.com".into(),
            ),
            orgs: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
        }
    }

    /// Talk to `base` (for every service) with a fixed token — the fake Azure
    /// DevOps server the adapter's tests run against.
    #[cfg(test)]
    fn at(base: &str, pat: &str) -> Self {
        AzureForges {
            http: Arc::new(client::Http::new(Some(pat.to_string()))),
            roots: (base.to_string(), base.to_string()),
            orgs: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
        }
    }

    /// The forge for one repository, by coordinates (cached).
    pub fn for_coords(&self, repo: AzureRepo) -> Arc<AzureForge> {
        let mut repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = repos.get(&repo) {
            return Arc::clone(f);
        }
        let org = {
            let mut orgs = self.orgs.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(
                orgs.entry(repo.org.to_ascii_lowercase())
                    .or_insert_with(|| {
                        let org = encode_segment(&repo.org);
                        Arc::new(Org {
                            http: Arc::clone(&self.http),
                            url: format!("{}/{org}", self.roots.0),
                            vssps: format!("{}/{org}", self.roots.1),
                            me: tokio::sync::OnceCell::new(),
                        })
                    }),
            )
        };
        let forge = Arc::new(AzureForge {
            org,
            repo: repo.clone(),
            cache: Mutex::new(HashMap::new()),
        });
        repos.insert(repo, Arc::clone(&forge));
        forge
    }
}

impl ForgeFactory for AzureForges {
    fn for_repo(&self, repo: &Path) -> Result<Arc<dyn Forge>> {
        let url = crate::infra::git::origin_url(repo)
            .ok_or_else(|| CoreError::forge("the repository has no `origin` remote"))?;
        // Reached only for a project whose host is Azure DevOps — detected, or
        // pinned for a remote detection can't place — so the path shapes are
        // read on any host.
        match parse_azure_remote(&url) {
            Some(coords) => Ok(self.for_coords(coords)),
            None => Err(CoreError::forge(format!(
                "`origin` ({url}) isn't an Azure DevOps repository — its path needs the \
                 `v3/{{org}}/{{project}}/{{repo}}` or `{{org}}/{{project}}/_git/{{repo}}` shape; \
                 fix the remote, or set the project's code host back to GitHub in its settings"
            ))),
        }
    }
}

/// Which read of a PR a cache entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Read {
    Pr,
    Threads,
    Evaluations,
    Statuses,
    Iterations,
}

/// One Azure Repos repository.
pub struct AzureForge {
    org: Arc<Org>,
    repo: AzureRepo,
    cache: Mutex<HashMap<(u64, Read), (Instant, Value)>>,
}

impl AzureForge {
    fn http(&self) -> &client::Http {
        &self.org.http
    }

    /// `…/{org}/{project}`.
    fn project_url(&self) -> String {
        format!("{}/{}", self.org.url, encode_segment(&self.repo.project))
    }

    /// `…/{org}/{project}/_apis/git/repositories/{repo}`.
    fn git_api(&self) -> String {
        format!(
            "{}/_apis/git/repositories/{}",
            self.project_url(),
            encode_segment(&self.repo.repo)
        )
    }

    fn pr_url(&self, pr: u64) -> String {
        format!("{}/pullrequests/{pr}?api-version=7.1", self.git_api())
    }

    fn pr_sub_url(&self, pr: u64, rest: &str) -> String {
        format!(
            "{}/pullRequests/{pr}/{rest}?api-version=7.1",
            self.git_api()
        )
    }

    fn cached(&self, key: (u64, Read)) -> Option<Value> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(&key)
            .filter(|(at, _)| at.elapsed() < READ_CACHE)
            .map(|(_, v)| v.clone())
    }

    fn remember(&self, key: (u64, Read), value: &Value) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, (at, _)| at.elapsed() < READ_CACHE);
        cache.insert(key, (Instant::now(), value.clone()));
    }

    /// Drop every cached read of `pr` — after any write to it.
    fn invalidate(&self, pr: u64) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|(id, _), _| *id != pr);
    }

    async fn read(&self, pr: u64, what: Read, url: String) -> Result<Value> {
        if let Some(v) = self.cached((pr, what)) {
            return Ok(v);
        }
        let v = self.http().json(Method::GET, &url, None).await?;
        self.remember((pr, what), &v);
        Ok(v)
    }

    async fn get_pr(&self, pr: u64) -> Result<Value> {
        self.read(pr, Read::Pr, self.pr_url(pr)).await
    }

    async fn fresh_pr(&self, pr: u64) -> Result<Value> {
        self.invalidate(pr);
        self.get_pr(pr).await
    }

    async fn get_threads(&self, pr: u64) -> Result<Value> {
        self.read(pr, Read::Threads, self.pr_sub_url(pr, "threads"))
            .await
    }

    async fn get_statuses(&self, pr: u64) -> Result<Value> {
        self.read(pr, Read::Statuses, self.pr_sub_url(pr, "statuses"))
            .await
    }

    async fn get_iterations(&self, pr: u64) -> Result<Value> {
        self.read(pr, Read::Iterations, self.pr_sub_url(pr, "iterations"))
            .await
    }

    /// The PR's policy evaluations. Policies hang off the *project* (by id)
    /// and the PR's artifact id; the endpoint only exists as a preview.
    async fn get_evaluations(&self, pr: u64, pr_json: &Value) -> Result<Value> {
        let project_id = pr_json
            .pointer("/repository/project/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::forge("the PR payload names no project id"))?;
        let artifact = format!("vstfs:///CodeReview/CodeReviewId/{project_id}/{pr}");
        let url = format!(
            "{}/_apis/policy/evaluations?artifactId={}&api-version=7.1-preview.1",
            self.project_url(),
            encode_segment(&artifact)
        );
        self.read(pr, Read::Evaluations, url).await
    }

    async fn me(&self) -> Result<(String, String)> {
        self.org.me().await.cloned()
    }

    async fn active_prs(&self) -> Result<Vec<Value>> {
        let url = format!(
            "{}/pullrequests?searchCriteria.status=active&$top=200&api-version=7.1",
            self.git_api()
        );
        Ok(wire::pr_list(
            &self.http().json(Method::GET, &url, None).await?,
        ))
    }

    /// The identity id for a handle (an email, usually) — what naming a PR
    /// reviewer takes on Azure.
    async fn identity_id(&self, handle: &str) -> Result<String> {
        let url = format!(
            "{}/_apis/identities?searchFilter=General&filterValue={}&queryMembership=None\
             &api-version=7.1",
            self.org.vssps,
            encode_segment(handle)
        );
        let v = self.http().json(Method::GET, &url, None).await?;
        wire::first_identity_id(&v)
            .ok_or_else(|| CoreError::forge(format!("no Azure DevOps user matches {handle}")))
    }

    /// The PR's CI signals rolled up.
    async fn checks(&self, pr: u64) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        let pr_json = self.get_pr(pr).await?;
        let mut signals =
            wire::policy_signals(&self.repo, &self.get_evaluations(pr, &pr_json).await?);
        let statuses = self.get_statuses(pr).await?;
        // Which push is current only matters once something posted a status.
        let current = if wire::pr_list(&statuses).is_empty() {
            None
        } else {
            wire::latest_iteration(&self.get_iterations(pr).await?)
        };
        signals.extend(wire::status_signals(&statuses, current.as_ref()));
        Ok(wire::rollup(&signals))
    }

    async fn post_thread(&self, pr: u64, body: &Value) -> Result<()> {
        self.http()
            .json(Method::POST, &self.pr_sub_url(pr, "threads"), Some(body))
            .await?;
        self.invalidate(pr);
        Ok(())
    }

    async fn set_thread_status(&self, pr: u64, thread: u64, status: i64) -> Result<()> {
        self.http()
            .json(
                Method::PATCH,
                &self.pr_sub_url(pr, &format!("threads/{thread}")),
                Some(&json!({ "status": status })),
            )
            .await?;
        self.invalidate(pr);
        Ok(())
    }

    /// Why a PR's completion was refused, when branch policies are the reason.
    async fn policy_block(&self, pr: u64) -> Option<String> {
        let pr_json = self.fresh_pr(pr).await.ok()?;
        let evaluations = self.get_evaluations(pr, &pr_json).await.ok()?;
        let names = wire::blocking_policies(&evaluations);
        (!names.is_empty()).then(|| format!("blocked by branch policy: {}", names.join(", ")))
    }
}

#[async_trait]
impl Forge for AzureForge {
    async fn create_pr(
        &self,
        _repo: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo> {
        let reviewer = super::normalize_reviewer(reviewer);
        // Resolve the reviewer first: a PR created with an unknown reviewer id
        // is refused outright, and a PR created without one — then failing to
        // add them — is still better reported by the executor's recovery
        // (which finds the open PR and says the reviewer wasn't requested)
        // than by leaving the PR unrecorded.
        let reviewer_id = match &reviewer {
            Some(handle) => Some(self.identity_id(handle).await),
            None => None,
        };
        let id_for_body = reviewer_id
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(String::as_str);
        let created = self
            .http()
            .json(
                Method::POST,
                &format!("{}/pullrequests?api-version=7.1", self.git_api()),
                Some(&wire::create_pr_body(
                    title,
                    body,
                    base,
                    head,
                    id_for_body,
                    draft,
                )),
            )
            .await?;
        let number = wire::pr_id(&created);
        if number == 0 {
            return Err(CoreError::forge("Azure DevOps created no PR id"));
        }
        if let Some(Err(e)) = reviewer_id {
            return Err(CoreError::forge(format!(
                "PR !{number} opened, but its reviewer couldn't be requested: {e}"
            )));
        }
        Ok(PrInfo {
            number,
            url: wire::pr_web_url(&self.repo, &created),
            title: title.to_string(),
            state: PrState::open(draft),
            reviewer,
            reviewer_recorded: true,
        })
    }

    async fn fetch_comments(&self, _repo: &Path, pr_number: u64) -> Result<Vec<ReviewComment>> {
        Ok(wire::thread_comments(&self.get_threads(pr_number).await?))
    }

    async fn list_review_prs(&self, _repo: &Path, scope: ReviewScope) -> Result<Vec<PrSummary>> {
        let authors: Option<Vec<String>> = match &scope {
            ReviewScope::Authors(a) => {
                let a: Vec<String> = a
                    .iter()
                    .filter_map(|h| normalize_azure_identity(h))
                    .collect();
                if a.is_empty() {
                    return Ok(Vec::new());
                }
                Some(a)
            }
            ReviewScope::Everyone => None,
        };
        let (me, _) = self.me().await?;
        let mut out = Vec::new();
        for pr in self.active_prs().await? {
            if !wire::review_candidate(&pr, &me, authors.as_deref()) || wire::my_vote(&pr, &me) != 0
            {
                continue;
            }
            let number = wire::pr_id(&pr);
            // A Comment review casts no vote, so "reviewed by me" also means
            // "I wrote in one of its threads".
            match self.get_threads(number).await {
                Ok(threads) if wire::i_commented(&threads, &me) => continue,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("review scan: couldn't read threads of !{number}: {e}");
                    continue;
                }
            }
            self.remember((number, Read::Pr), &pr);
            let checks = match self.checks(number).await {
                Ok((status, _)) => status,
                Err(e) => {
                    tracing::warn!("review scan: couldn't read checks of !{number}: {e}");
                    CheckStatus::None
                }
            };
            out.push(wire::pr_summary(&self.repo, &pr, checks));
        }
        Ok(out)
    }

    async fn submit_review(
        &self,
        _repo: &Path,
        pr_number: u64,
        event: ReviewEvent,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<()> {
        let (me, _) = self.me().await?;
        // Not one atomic request as on GitHub: each comment is a thread, then
        // the vote. Skip what an interrupted earlier attempt already posted,
        // so resending doesn't duplicate it.
        self.invalidate(pr_number);
        let existing = self.get_threads(pr_number).await?;
        for c in comments {
            let Some(line) = c.line else { continue };
            let anchor = Some((c.path.as_str(), line));
            if wire::already_posted(&existing, &me, anchor, &c.body) {
                continue;
            }
            self.post_thread(
                pr_number,
                &wire::new_thread_body(&c.body, anchor, wire::THREAD_ACTIVE),
            )
            .await?;
        }
        if !body.trim().is_empty() && !wire::already_posted(&existing, &me, None, body) {
            // An approval's summary asks nothing of the author; left active it
            // would still count against a comment-resolution policy.
            let status = if event == ReviewEvent::Approve {
                wire::THREAD_CLOSED
            } else {
                wire::THREAD_ACTIVE
            };
            self.post_thread(pr_number, &wire::new_thread_body(body, None, status))
                .await?;
        }
        if let Some(vote) = wire::vote_for(event) {
            self.http()
                .json(
                    Method::PUT,
                    &self.pr_sub_url(pr_number, &format!("reviewers/{me}")),
                    Some(&json!({ "vote": vote })),
                )
                .await?;
            self.invalidate(pr_number);
        }
        Ok(())
    }

    async fn list_reviewers(&self, _repo: &Path) -> Result<Vec<String>> {
        let project = self
            .http()
            .json(
                Method::GET,
                &format!(
                    "{}/_apis/projects/{}?api-version=7.1",
                    self.org.url,
                    encode_segment(&self.repo.project)
                ),
                None,
            )
            .await?;
        let team = project
            .pointer("/defaultTeam/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::forge("the Azure DevOps project has no default team"))?;
        let members = self
            .http()
            .json(
                Method::GET,
                &format!(
                    "{}/_apis/projects/{}/teams/{team}/members?$top=500&api-version=7.1",
                    self.org.url,
                    encode_segment(&self.repo.project)
                ),
                None,
            )
            .await?;
        let me = self.me().await.map(|(id, _)| id).unwrap_or_default();
        Ok(wire::team_members(&members, &me))
    }

    async fn list_pr_authors(&self, _repo: &Path) -> Result<Vec<String>> {
        let me = self.me().await.map(|(id, _)| id).unwrap_or_default();
        Ok(wire::pr_authors(&self.active_prs().await?, &me))
    }

    async fn list_submitted_reviews(
        &self,
        _repo: &Path,
        pr_number: u64,
    ) -> Result<Vec<ReviewSummary>> {
        Ok(wire::reviews(&self.get_pr(pr_number).await?))
    }

    async fn reply_to_comment(
        &self,
        _repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        let (thread, comment) = wire::unpack_comment_id(comment_id);
        self.http()
            .json(
                Method::POST,
                &self.pr_sub_url(pr_number, &format!("threads/{thread}/comments")),
                Some(&wire::reply_body(body, comment)),
            )
            .await?;
        self.invalidate(pr_number);
        Ok(())
    }

    async fn decline_comment(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        self.reply_to_comment(repo, pr_number, comment_id, body)
            .await?;
        let (thread, _) = wire::unpack_comment_id(comment_id);
        self.set_thread_status(pr_number, thread, wire::THREAD_WONT_FIX)
            .await
    }

    async fn mark_ready(&self, _repo: &Path, pr_number: u64) -> Result<()> {
        self.http()
            .json(
                Method::PATCH,
                &self.pr_url(pr_number),
                Some(&json!({ "isDraft": false })),
            )
            .await?;
        self.invalidate(pr_number);
        Ok(())
    }

    async fn merge(&self, _repo: &Path, pr_number: u64) -> Result<()> {
        let pr = self.fresh_pr(pr_number).await?;
        if pr.get("status").and_then(Value::as_str) == Some("completed") {
            return Ok(());
        }
        let head = pr
            .pointer("/lastMergeSourceCommit/commitId")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                CoreError::forge(format!(
                    "PR !{pr_number} has no source commit yet — Azure DevOps is still \
                     processing its last push; retry in a moment"
                ))
            })?
            .to_string();
        let patched = self
            .http()
            .json(
                Method::PATCH,
                &self.pr_url(pr_number),
                Some(&wire::complete_pr_body(&head)),
            )
            .await;
        self.invalidate(pr_number);
        if let Err(e) = patched {
            return Err(match self.policy_block(pr_number).await {
                Some(block) => CoreError::forge(format!("{block} ({e})")),
                None => e,
            });
        }
        // Completion runs asynchronously: `Ok` must mean the merge landed,
        // because the caller removes the worktree and the branch right after.
        let deadline = Instant::now() + MERGE_WAIT;
        loop {
            let pr = self.fresh_pr(pr_number).await?;
            if pr.get("status").and_then(Value::as_str) == Some("completed") {
                return Ok(());
            }
            match pr.get("mergeStatus").and_then(Value::as_str).unwrap_or("") {
                "conflicts" => {
                    return Err(CoreError::forge(format!(
                        "PR !{pr_number} conflicts with its target branch"
                    )))
                }
                "rejectedByPolicy" => {
                    let block = self
                        .policy_block(pr_number)
                        .await
                        .unwrap_or_else(|| "blocked by branch policy".into());
                    return Err(CoreError::forge(format!(
                        "PR !{pr_number} wasn't merged: {block}"
                    )));
                }
                "failure" => {
                    let why = pr
                        .get("mergeFailureMessage")
                        .and_then(Value::as_str)
                        .unwrap_or("Azure DevOps reported a merge failure");
                    return Err(CoreError::forge(format!(
                        "PR !{pr_number} wasn't merged: {why}"
                    )));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                let block = self.policy_block(pr_number).await;
                return Err(CoreError::forge(format!(
                    "Azure DevOps hasn't completed PR !{pr_number} yet{} — check it on Azure DevOps",
                    block.map(|b| format!(" ({b})")).unwrap_or_default()
                )));
            }
            tokio::time::sleep(MERGE_POLL).await;
        }
    }

    async fn is_merged(&self, _repo: &Path, pr_number: u64) -> Result<bool> {
        Ok(wire::pr_state(&self.fresh_pr(pr_number).await?) == Some(LivePrState::Merged))
    }

    async fn merge_status(&self, _repo: &Path, pr_number: u64) -> Result<Mergeable> {
        Ok(wire::mergeable(&self.get_pr(pr_number).await?))
    }

    /// Through git rather than the refs API (which wants the branch's current
    /// object id): the push credentials the user already has are the ones
    /// that may delete the branch.
    async fn delete_remote_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        let out = tokio::time::timeout(
            Duration::from_secs(120),
            tokio::process::Command::new("git")
                .current_dir(repo)
                .args(["push", "origin", "--delete", branch])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| CoreError::forge("deleting the remote branch timed out"))??;
        if !out.status.success() {
            return Err(CoreError::forge(format!(
                "couldn't delete origin/{branch}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn resolve_threads(
        &self,
        _repo: &Path,
        pr_number: u64,
        comment_ids: &[u64],
    ) -> Result<usize> {
        if comment_ids.is_empty() {
            return Ok(0);
        }
        self.invalidate(pr_number);
        let threads = self.get_threads(pr_number).await?;
        let wanted: HashSet<u64> = comment_ids
            .iter()
            .map(|id| wire::unpack_comment_id(*id).0)
            .collect();
        let mut resolved = 0;
        for thread in wanted {
            if !wire::thread_is_open(&threads, thread) {
                continue;
            }
            // One thread failing (a permission race) mustn't stop the rest.
            if self
                .set_thread_status(pr_number, thread, wire::THREAD_FIXED)
                .await
                .is_ok()
            {
                resolved += 1;
            }
        }
        Ok(resolved)
    }

    async fn list_threads(&self, _repo: &Path, pr_number: u64) -> Result<Vec<ReviewThread>> {
        let (me, _) = self.me().await?;
        Ok(wire::threads(&self.get_threads(pr_number).await?, &me))
    }

    async fn pr_checks(
        &self,
        _repo: &Path,
        pr_number: u64,
    ) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        self.checks(pr_number).await
    }

    async fn failed_check_logs(
        &self,
        _repo: &Path,
        failed: &[FailedCheck],
    ) -> Vec<(String, String)> {
        let mut logs = Vec::new();
        let mut seen = HashSet::new();
        for check in failed {
            let Some(build) = wire::build_id_from_url(&check.url) else {
                continue;
            };
            if !seen.insert(build) {
                continue;
            }
            let builds = format!("{}/_apis/build/builds/{build}", self.project_url());
            let timeline = match self
                .http()
                .json(
                    Method::GET,
                    &format!("{builds}/timeline?api-version=7.1"),
                    None,
                )
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("fix checks: couldn't read the timeline of build {build}: {e}");
                    continue;
                }
            };
            let mut text = String::new();
            for (step, log_id) in wire::failed_log_ids(&timeline)
                .into_iter()
                .take(LOGS_PER_BUILD)
            {
                match self
                    .http()
                    .text(&format!("{builds}/logs/{log_id}?api-version=7.1"))
                    .await
                {
                    Ok(log) if !log.trim().is_empty() => {
                        text.push_str(&format!("=== {step} ===\n{}\n", log.trim_end()));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        "fix checks: couldn't read log {log_id} of build {build}: {e}"
                    ),
                }
            }
            if !text.is_empty() {
                logs.push((check.name.clone(), text));
            }
        }
        logs
    }

    async fn pr_live_state(&self, _repo: &Path, pr_number: u64) -> Result<Option<LivePrState>> {
        Ok(wire::pr_state(&self.get_pr(pr_number).await?))
    }

    async fn pr_push_target(&self, _repo: &Path, pr_number: u64) -> Result<Option<PrPushTarget>> {
        Ok(wire::push_target(&self.fresh_pr(pr_number).await?))
    }

    async fn comment_on_pr(&self, _repo: &Path, pr_number: u64, body: &str) -> Result<()> {
        // Closed on arrival: a note from us ("pushed the fix") asks nothing of
        // anyone, and must not count as an unresolved comment.
        self.post_thread(
            pr_number,
            &wire::new_thread_body(body, None, wire::THREAD_CLOSED),
        )
        .await
    }

    async fn pr_for_head(&self, _repo: &Path, head: &str) -> Result<Option<PrInfo>> {
        let url = format!(
            "{}/pullrequests?searchCriteria.sourceRefName={}&searchCriteria.status=active\
             &api-version=7.1",
            self.git_api(),
            encode_segment(&wire::head_ref(head))
        );
        let Ok(v) = self.http().json(Method::GET, &url, None).await else {
            return Ok(None);
        };
        Ok(wire::pr_list(&v)
            .iter()
            .find_map(|pr| wire::pr_info(&self.repo, pr)))
    }

    async fn list_open_prs(&self, _repo: &Path) -> Result<Vec<OpenPr>> {
        // "Who am I" only orders the list and flags "not yours", so a failed
        // lookup degrades to an unflagged listing rather than an error.
        let (prs, me) = tokio::join!(self.active_prs(), self.me());
        let me = me.map(|(id, _)| id).unwrap_or_default();
        let mut prs: Vec<OpenPr> = prs?
            .iter()
            .map(|pr| wire::open_pr(&self.repo, pr, &me))
            .filter(|p| p.number != 0 && !p.head_ref.is_empty())
            .collect();
        // The user's own PRs first; stable, so Azure's newest-first order
        // holds within.
        prs.sort_by_key(|p| !p.mine);
        Ok(prs)
    }

    async fn pr_by_number(&self, _repo: &Path, pr_number: u64) -> Result<Option<PrInfo>> {
        // A failure is an error, not "no PR": the caller must tell "closed"
        // from "couldn't ask".
        let pr = self.fresh_pr(pr_number).await?;
        Ok(wire::pr_info(&self.repo, &pr)
            .filter(|info| matches!(info.state, PrState::Open | PrState::Draft)))
    }
}

/// An Azure DevOps identity as typed by a human — an email (`uniqueName`), a
/// `DOMAIN\user` account, or an identity GUID — lowercased, or `None` when it
/// can't be one.
pub fn normalize_azure_identity(input: &str) -> Option<String> {
    let id = input
        .trim()
        .trim_start_matches('@')
        .trim()
        .to_ascii_lowercase();
    let valid = !id.is_empty()
        && !id.chars().any(char::is_whitespace)
        && (id.contains('@') || id.contains('\\') || is_guid(&id));
    valid.then_some(id)
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Whether the checkout carries Azure Pipelines YAML: the conventional
/// `azure-pipelines.yml` at the root, or YAML under `.azure-pipelines/` /
/// `.pipelines/`. Only a fallback — build validation is configured
/// server-side as a branch policy, and a real observation always wins.
pub fn repo_has_pipelines(repo: &Path) -> bool {
    let is_yaml = |p: &Path| {
        matches!(
            p.extension()
                .and_then(|x| x.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("yml") | Some("yaml")
        )
    };
    if ["azure-pipelines.yml", "azure-pipelines.yaml"]
        .iter()
        .any(|f| repo.join(f).is_file())
    {
        return true;
    }
    [".azure-pipelines", ".pipelines"].iter().any(|d| {
        std::fs::read_dir(repo.join(d))
            .map(|entries| entries.flatten().any(|e| is_yaml(&e.path())))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_emails_accounts_or_guids() {
        assert_eq!(
            normalize_azure_identity("  @Rita@Fabrikam.com "),
            Some("rita@fabrikam.com".into())
        );
        assert_eq!(
            normalize_azure_identity("FABRIKAM\\rita"),
            Some("fabrikam\\rita".into())
        );
        assert_eq!(
            normalize_azure_identity("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
            Some("6f9619ff-8b86-d011-b42d-00c04fc964ff".into())
        );
        for bad in ["", "rita", "Rita Smith", "rita smith@x.com"] {
            assert_eq!(normalize_azure_identity(bad), None, "{bad}");
        }
    }

    #[test]
    fn pipelines_yaml_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!repo_has_pipelines(dir.path()));
        std::fs::create_dir(dir.path().join(".pipelines")).unwrap();
        std::fs::write(dir.path().join(".pipelines/notes.txt"), "").unwrap();
        assert!(!repo_has_pipelines(dir.path()));
        std::fs::write(dir.path().join(".pipelines/ci.yaml"), "").unwrap();
        assert!(repo_has_pipelines(dir.path()));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("azure-pipelines.yml"), "").unwrap();
        assert!(repo_has_pipelines(dir.path()));
    }

    #[test]
    fn a_non_azure_origin_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.remote("origin", "git@github.com:o/r.git").unwrap();
        let err = AzureForges::new()
            .for_repo(dir.path())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("isn't an Azure DevOps repository"), "{err}");
        repo.remote_set_url("origin", "https://dev.azure.com/o/p/_git/r")
            .unwrap();
        assert!(AzureForges::new().for_repo(dir.path()).is_ok());
        // An SSH host alias: detection can't place it, a pin must still work.
        repo.remote_set_url("origin", "git@azure-work:v3/o/p/r")
            .unwrap();
        assert_eq!(crate::infra::forge::detect_forge(dir.path()), None);
        assert!(AzureForges::new().for_repo(dir.path()).is_ok());
    }

    #[test]
    fn one_forge_per_repository() {
        let forges = AzureForges::new();
        let coords = AzureRepo {
            org: "o".into(),
            project: "p".into(),
            repo: "r".into(),
        };
        let a = forges.for_coords(coords.clone());
        let b = forges.for_coords(coords);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a.org, &b.org));
    }
}

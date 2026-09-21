//! [`AzureForge`] end to end against a fake Azure DevOps: a local HTTP server
//! answering the documented REST routes with canned payloads and recording
//! every request, so each test pins down *what the adapter sends* (methods,
//! paths, bodies) as well as what it makes of the answers.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::wire::{pack_comment_id, THREAD_WONT_FIX};
use super::*;
use crate::infra::forge::remote::AzureRepo;

const ME: &str = "11111111-1111-1111-1111-111111111111";
const PROJECT_ID: &str = "22222222-2222-2222-2222-222222222222";

/// A recorded request.
#[derive(Debug, Clone)]
struct Hit {
    method: String,
    path: String,
    query: String,
    body: Value,
}

struct Route {
    method: &'static str,
    path: String,
    /// Only match when the query contains this.
    query: Option<&'static str>,
    /// Served in order; the last one repeats.
    replies: VecDeque<(u16, Value)>,
}

#[derive(Clone)]
struct Fake {
    base: String,
    routes: Arc<Mutex<Vec<Route>>>,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Fake {
    async fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Fake {
            base,
            routes: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(Mutex::new(Vec::new())),
        };
        let server = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let server = server.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let method = first.next().unwrap_or("").to_string();
                    let target = first.next().unwrap_or("").to_string();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let body = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
                    let (status, reply) = server.answer(&method, path, query, body);
                    let text = reply.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        fake
    }

    fn answer(&self, method: &str, path: &str, query: &str, body: Value) -> (u16, Value) {
        let path = crate::infra::forge::remote::decode_segment(path);
        self.hits.lock().unwrap().push(Hit {
            method: method.to_string(),
            path: path.clone(),
            query: query.to_string(),
            body,
        });
        let mut routes = self.routes.lock().unwrap();
        let route = routes.iter_mut().find(|r| {
            r.method == method && r.path == path && r.query.is_none_or(|q| query.contains(q))
        });
        match route {
            Some(r) if r.replies.len() > 1 => r.replies.pop_front().unwrap(),
            Some(r) => r.replies.front().cloned().unwrap_or((200, Value::Null)),
            None => (
                404,
                json!({ "message": format!("no fake route for {method} {path}") }),
            ),
        }
    }

    fn on(&self, method: &'static str, path: &str, replies: Vec<(u16, Value)>) {
        self.on_query(method, path, None, replies);
    }

    fn on_query(
        &self,
        method: &'static str,
        path: &str,
        query: Option<&'static str>,
        replies: Vec<(u16, Value)>,
    ) {
        // Newest first, so a test can override a default route.
        self.routes.lock().unwrap().insert(
            0,
            Route {
                method,
                path: path.to_string(),
                query,
                replies: replies.into(),
            },
        );
    }

    fn hits(&self, method: &str, path: &str) -> Vec<Hit> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.method == method && h.path == path)
            .cloned()
            .collect()
    }

    /// The forge for `fab/Fiber/app`, with "me" already answerable.
    fn forge(&self) -> Arc<AzureForge> {
        self.on(
            "GET",
            "/fab/_apis/connectionData",
            vec![(200, json!({ "authenticatedUser": { "id": ME, "properties": { "Account": { "$value": "me@x.com" } } } }))],
        );
        AzureForges::at(&self.base, "test-pat").for_coords(AzureRepo {
            org: "fab".into(),
            project: "Fiber".into(),
            repo: "app".into(),
        })
    }
}

const GIT: &str = "/fab/Fiber/_apis/git/repositories/app";

fn pr_json(status: &str, merge_status: &str) -> Value {
    json!({
        "pullRequestId": 22,
        "status": status,
        "isDraft": false,
        "title": "T",
        "mergeStatus": merge_status,
        "sourceRefName": "refs/heads/usine/x",
        "targetRefName": "refs/heads/main",
        "lastMergeSourceCommit": { "commitId": "abc123" },
        "repository": { "project": { "id": PROJECT_ID } },
        "reviewers": [],
    })
}

fn identity(id: &str, email: &str) -> Value {
    json!({ "id": id, "displayName": email, "uniqueName": email })
}

fn repo() -> &'static Path {
    Path::new("/unused")
}

#[tokio::test]
async fn create_pr_resolves_the_reviewer_and_links_the_web_page() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        "/fab/_apis/identities",
        vec![(200, json!({ "value": [{ "id": "rita-id" }] }))],
    );
    fake.on(
        "POST",
        &format!("{GIT}/pullrequests"),
        vec![(201, json!({ "pullRequestId": 7, "repository": { "webUrl": "https://dev.azure.com/fab/Fiber/_git/app" } }))],
    );
    let pr = forge
        .create_pr(
            repo(),
            "Title",
            "Body",
            "main",
            "usine/x",
            Some(" Rita@x.com "),
            true,
        )
        .await
        .unwrap();
    assert_eq!(pr.number, 7);
    assert_eq!(pr.state, PrState::Draft);
    assert_eq!(
        pr.url,
        "https://dev.azure.com/fab/Fiber/_git/app/pullrequest/7"
    );
    assert_eq!(pr.reviewer.as_deref(), Some("Rita@x.com"));
    let lookup = &fake.hits("GET", "/fab/_apis/identities")[0];
    assert!(
        lookup.query.contains("filterValue=Rita%40x.com"),
        "{}",
        lookup.query
    );
    let body = &fake.hits("POST", &format!("{GIT}/pullrequests"))[0].body;
    assert_eq!(body["sourceRefName"], "refs/heads/usine/x");
    assert_eq!(body["targetRefName"], "refs/heads/main");
    assert_eq!(body["isDraft"], true);
    assert_eq!(body["reviewers"][0]["id"], "rita-id");
}

#[tokio::test]
async fn an_unknown_reviewer_still_opens_the_pr_but_reports_it() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        "/fab/_apis/identities",
        vec![(200, json!({ "value": [] }))],
    );
    fake.on(
        "POST",
        &format!("{GIT}/pullrequests"),
        vec![(201, json!({ "pullRequestId": 8 }))],
    );
    let err = forge
        .create_pr(
            repo(),
            "T",
            "B",
            "main",
            "usine/x",
            Some("ghost@x.com"),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
    // The executor's recovery finds the open PR and warns about the reviewer.
    assert!(err.contains("!8") && err.contains("ghost@x.com"), "{err}");
    assert_eq!(fake.hits("POST", &format!("{GIT}/pullrequests")).len(), 1);
}

fn threads() -> Value {
    json!({ "value": [
        { "id": 141, "status": "active",
          "threadContext": { "filePath": "/src/lib.rs", "rightFileStart": { "line": 12, "offset": 1 } },
          "comments": [ { "id": 1, "author": identity("r", "rita@x.com"), "content": "Extract this.", "commentType": "text" } ] },
        { "id": 144, "properties": { "CodeReviewThreadType": { "$value": "VoteUpdate" } },
          "comments": [ { "id": 1, "content": "Rita voted", "commentType": "system" } ] },
        { "id": 145, "status": "active",
          "comments": [
            { "id": 1, "author": identity("r", "rita@x.com"), "content": "Changelog?", "commentType": "text" },
            { "id": 2, "author": identity(ME, "me@x.com"), "content": "Added.", "commentType": "text" },
          ] },
    ]})
}

#[tokio::test]
async fn triage_sees_user_threads_and_a_decline_closes_its_thread() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        &format!("{GIT}/pullRequests/22/threads"),
        vec![(200, threads())],
    );
    fake.on(
        "POST",
        &format!("{GIT}/pullRequests/22/threads/141/comments"),
        vec![(200, json!({}))],
    );
    fake.on(
        "PATCH",
        &format!("{GIT}/pullRequests/22/threads/141"),
        vec![(200, json!({}))],
    );

    let comments = forge.fetch_comments(repo(), 22).await.unwrap();
    assert_eq!(comments.len(), 3, "the vote's system thread is left out");
    let threads = forge.list_threads(repo(), 22).await.unwrap();
    let unanswered: Vec<&str> = threads
        .iter()
        .filter(|t| t.is_unanswered())
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(unanswered, vec!["141"], "I spoke last on 145");
    // The poll's back-to-back reads of one PR share one fetch.
    assert_eq!(
        fake.hits("GET", &format!("{GIT}/pullRequests/22/threads"))
            .len(),
        1
    );

    forge
        .decline_comment(repo(), 22, pack_comment_id(141, 1), "Out of scope here.")
        .await
        .unwrap();
    let reply = &fake.hits(
        "POST",
        &format!("{GIT}/pullRequests/22/threads/141/comments"),
    )[0]
    .body;
    assert_eq!(reply["content"], "Out of scope here.");
    assert_eq!(reply["parentCommentId"], 1);
    let patch = &fake.hits("PATCH", &format!("{GIT}/pullRequests/22/threads/141"))[0].body;
    assert_eq!(patch["status"], THREAD_WONT_FIX);
}

#[tokio::test]
async fn resolving_fixes_each_open_thread_once() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        &format!("{GIT}/pullRequests/22/threads"),
        vec![(200, threads())],
    );
    fake.on(
        "PATCH",
        &format!("{GIT}/pullRequests/22/threads/141"),
        vec![(200, json!({}))],
    );
    fake.on(
        "PATCH",
        &format!("{GIT}/pullRequests/22/threads/145"),
        vec![(200, json!({}))],
    );
    let n = forge
        .resolve_threads(
            repo(),
            22,
            &[
                pack_comment_id(141, 1),
                pack_comment_id(145, 1),
                pack_comment_id(145, 2),
            ],
        )
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        fake.hits("PATCH", &format!("{GIT}/pullRequests/22/threads/145"))
            .len(),
        1
    );
}

#[tokio::test]
async fn merge_waits_until_the_pr_is_really_completed() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    let pr = format!("{GIT}/pullrequests/22");
    fake.on(
        "GET",
        &pr,
        vec![
            (200, pr_json("active", "succeeded")),
            (200, pr_json("active", "queued")),
            (200, pr_json("completed", "succeeded")),
        ],
    );
    fake.on("PATCH", &pr, vec![(200, pr_json("active", "queued"))]);
    forge.merge(repo(), 22).await.unwrap();
    let patch = &fake.hits("PATCH", &pr)[0].body;
    assert_eq!(patch["status"], "completed");
    assert_eq!(patch["lastMergeSourceCommit"]["commitId"], "abc123");
    assert_eq!(patch["completionOptions"]["mergeStrategy"], "squash");
    assert_eq!(patch["completionOptions"]["deleteSourceBranch"], false);
    assert!(fake.hits("GET", &pr).len() >= 3, "polled until completed");
}

#[tokio::test]
async fn a_merge_refused_by_policy_names_the_policies() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    let pr = format!("{GIT}/pullrequests/22");
    fake.on("GET", &pr, vec![(200, pr_json("active", "succeeded"))]);
    fake.on(
        "PATCH",
        &pr,
        vec![(
            400,
            json!({ "message": "TF401181: The pull request cannot be completed" }),
        )],
    );
    fake.on(
        "GET",
        "/fab/Fiber/_apis/policy/evaluations",
        vec![(200, json!({ "value": [
            { "status": "rejected", "configuration": { "isEnabled": true, "isBlocking": true,
              "type": { "id": "fa4e907d-c16b-4a4c-9dfa-4906e5d171dd", "displayName": "Minimum number of reviewers" } } },
        ]}))],
    );
    let err = forge.merge(repo(), 22).await.unwrap_err().to_string();
    assert!(err.contains("Minimum number of reviewers"), "{err}");
    let eval = &fake.hits("GET", "/fab/Fiber/_apis/policy/evaluations")[0];
    assert!(eval.query.contains("7.1-preview.1"), "{}", eval.query);
    assert!(eval.query.contains(PROJECT_ID), "{}", eval.query);
}

#[tokio::test]
async fn a_conflicting_merge_is_reported_and_reads_as_conflicting() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    let pr = format!("{GIT}/pullrequests/22");
    fake.on(
        "GET",
        &pr,
        vec![
            (200, pr_json("active", "succeeded")),
            (200, pr_json("active", "conflicts")),
        ],
    );
    fake.on("PATCH", &pr, vec![(200, json!({}))]);
    let err = forge.merge(repo(), 22).await.unwrap_err().to_string();
    assert!(err.contains("conflicts"), "{err}");
    assert_eq!(
        forge.merge_status(repo(), 22).await.unwrap(),
        Mergeable::Conflicting
    );
}

#[tokio::test]
async fn checks_roll_up_build_policies_and_posted_statuses() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        &format!("{GIT}/pullrequests/22"),
        vec![(200, pr_json("active", "succeeded"))],
    );
    fake.on(
        "GET",
        "/fab/Fiber/_apis/policy/evaluations",
        vec![(200, json!({ "value": [
            { "status": "approved", "configuration": { "isEnabled": true, "type": { "id": "0609b952-1397-4640-95ec-e00a01b2c241", "displayName": "Build" } }, "context": { "buildId": 5 } },
            { "status": "rejected", "configuration": { "isEnabled": true, "type": { "id": "fa4e907d-c16b-4a4c-9dfa-4906e5d171dd", "displayName": "Minimum number of reviewers" } } },
        ]}))],
    );
    fake.on(
        "GET",
        &format!("{GIT}/pullRequests/22/statuses"),
        vec![(200, json!({ "value": [
            { "id": 1, "state": "failed", "iterationId": 1, "context": { "genre": "sonar", "name": "quality" }, "targetUrl": "https://sonar/1" },
        ]}))],
    );
    fake.on(
        "GET",
        &format!("{GIT}/pullRequests/22/iterations"),
        vec![(
            200,
            json!({ "value": [ { "id": 1, "createdDate": "2024-05-01T10:00:00Z" } ] }),
        )],
    );
    let (status, failed) = forge.pr_checks(repo(), 22).await.unwrap();
    assert_eq!(status, CheckStatus::Failing);
    assert_eq!(failed.len(), 1, "the reviewer policy is not CI");
    assert_eq!(failed[0].name, "sonar/quality");
}

#[tokio::test]
async fn failed_build_logs_are_fetched_per_build() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        "/fab/Fiber/_apis/build/builds/5/timeline",
        vec![(200, json!({ "records": [ { "name": "Run tests", "type": "Task", "result": "failed", "log": { "id": 9 } } ] }))],
    );
    fake.on(
        "GET",
        "/fab/Fiber/_apis/build/builds/5/logs/9",
        vec![(200, json!("assertion failed: left == right"))],
    );
    let url = "https://dev.azure.com/fab/Fiber/_build/results?buildId=5";
    let failed = vec![
        FailedCheck {
            name: "PR build".into(),
            workflow: String::new(),
            url: url.into(),
        },
        FailedCheck {
            name: "PR build (again)".into(),
            workflow: String::new(),
            url: url.into(),
        },
    ];
    let logs = forge.failed_check_logs(repo(), &failed).await;
    assert_eq!(logs.len(), 1, "one log per build");
    assert_eq!(logs[0].0, "PR build");
    assert!(logs[0].1.contains("=== Run tests ===") && logs[0].1.contains("assertion failed"));
}

#[tokio::test]
async fn a_resent_review_skips_what_landed_and_casts_the_vote() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        &format!("{GIT}/pullRequests/22/threads"),
        vec![(200, json!({ "value": [
            { "id": 7, "status": "active",
              "threadContext": { "filePath": "/src/a.rs", "rightFileStart": { "line": 3 } },
              "comments": [ { "id": 1, "author": identity(ME, "me@x.com"), "content": "First", "commentType": "text" } ] },
        ]}))],
    );
    fake.on(
        "POST",
        &format!("{GIT}/pullRequests/22/threads"),
        vec![(200, json!({}))],
    );
    fake.on(
        "PUT",
        &format!("{GIT}/pullRequests/22/reviewers/{ME}"),
        vec![(200, json!({}))],
    );
    let draft = |path: &str, line: Option<u64>, body: &str| DraftComment {
        path: path.into(),
        line,
        body: body.into(),
        severity: String::new(),
        selected: true,
    };
    forge
        .submit_review(
            repo(),
            22,
            ReviewEvent::RequestChanges,
            "Summary",
            &[
                draft("src/a.rs", Some(3), "First"),
                draft("src/b.rs", Some(9), "Second"),
            ],
        )
        .await
        .unwrap();
    let posted: Vec<Value> = fake
        .hits("POST", &format!("{GIT}/pullRequests/22/threads"))
        .into_iter()
        .map(|h| h.body)
        .collect();
    assert_eq!(posted.len(), 2, "the second comment and the summary");
    assert_eq!(posted[0]["threadContext"]["filePath"], "/src/b.rs");
    assert_eq!(posted[0]["threadContext"]["rightFileStart"]["line"], 9);
    assert!(posted[1].get("threadContext").is_none());
    let vote = &fake.hits("PUT", &format!("{GIT}/pullRequests/22/reviewers/{ME}"))[0].body;
    assert_eq!(vote["vote"], -5);
}

#[tokio::test]
async fn the_review_scan_skips_mine_voted_and_commented_prs() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    let with = |id: u64, creator: Value, reviewers: Value| {
        let mut pr = pr_json("active", "succeeded");
        pr["pullRequestId"] = json!(id);
        pr["createdBy"] = creator;
        pr["reviewers"] = reviewers;
        pr
    };
    fake.on(
        "GET",
        &format!("{GIT}/pullrequests"),
        vec![(200, json!({ "value": [
            with(1, identity(ME, "me@x.com"), json!([])),
            with(2, identity("r", "rita@x.com"), json!([{ "id": ME, "uniqueName": "me@x.com", "vote": 10 }])),
            with(3, identity("r", "rita@x.com"), json!([])),
            with(4, identity("s", "sam@x.com"), json!([])),
        ]}))],
    );
    for (id, mine) in [(3, false), (4, true)] {
        let author = if mine {
            identity(ME, "me@x.com")
        } else {
            identity("r", "rita@x.com")
        };
        fake.on(
            "GET",
            &format!("{GIT}/pullRequests/{id}/threads"),
            vec![(200, json!({ "value": [ { "id": 1, "status": "active",
                "comments": [ { "id": 1, "author": author, "content": "hi", "commentType": "text" } ] } ] }))],
        );
        fake.on(
            "GET",
            &format!("{GIT}/pullRequests/{id}/statuses"),
            vec![(200, json!({ "value": [] }))],
        );
    }
    fake.on(
        "GET",
        "/fab/Fiber/_apis/policy/evaluations",
        vec![(200, json!({ "value": [] }))],
    );
    let prs = forge
        .list_review_prs(repo(), ReviewScope::Everyone)
        .await
        .unwrap();
    let numbers: Vec<u64> = prs.iter().map(|p| p.number).collect();
    assert_eq!(
        numbers,
        vec![3],
        "1 is mine, 2 has my vote, 4 has my comment"
    );
    assert_eq!(prs[0].author, "rita@x.com");
    assert_eq!(prs[0].head_ref, "usine/x");
    assert_eq!(prs[0].base_ref, "main");
    let pinned = forge
        .list_review_prs(repo(), ReviewScope::Authors(vec!["Sam@x.com".into()]))
        .await
        .unwrap();
    assert!(pinned.is_empty(), "sam's only PR already has my comment");
}

#[tokio::test]
async fn a_ga_version_refused_as_preview_is_retried_as_preview() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        "/fab/_apis/projects/Fiber",
        vec![(200, json!({ "defaultTeam": { "id": "team" } }))],
    );
    // Routes are matched newest first: the GA refusal, then the preview
    // answer that shadows it for `…-preview` queries.
    fake.on_query(
        "GET",
        "/fab/_apis/projects/Fiber/teams/team/members",
        Some("api-version=7.1"),
        vec![(400, json!({ "message": "The requested version \"7.1\" of the resource is under preview. The -preview flag must be supplied in the api-version for such requests." }))],
    );
    fake.on_query(
        "GET",
        "/fab/_apis/projects/Fiber/teams/team/members",
        Some("api-version=7.1-preview"),
        vec![(200, json!({ "value": [
            { "identity": identity("r", "Rita@x.com") },
            { "identity": identity(ME, "me@x.com") },
            { "identity": { "id": "g", "displayName": "[Fiber]\\Readers", "uniqueName": "g", "isContainer": true } },
        ]}))],
    );
    let reviewers = forge.list_reviewers(repo()).await.unwrap();
    assert_eq!(reviewers, vec!["rita@x.com"]);
    assert_eq!(
        fake.hits("GET", "/fab/_apis/projects/Fiber/teams/team/members")
            .len(),
        2
    );
}

#[tokio::test]
async fn a_rejected_token_says_so() {
    let fake = Fake::start().await;
    let forge = fake.forge();
    fake.on(
        "GET",
        &format!("{GIT}/pullrequests/22"),
        vec![(401, json!({}))],
    );
    let err = forge
        .pr_live_state(repo(), 22)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("personal access token"), "{err}");
}

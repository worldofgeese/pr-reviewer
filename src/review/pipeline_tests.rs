//! End-to-end review lifecycle tests: `ReviewEngine::review_pr` against a
//! mock GitHub (one httptest server per round, holding that round's GitHub
//! state), a stub `claude` harness on PATH, and one SQLite database carried
//! across rounds, as the daemon does.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use httptest::{matchers::*, responders::*, Expectation, Server};
use serde_json::{json, Value};

use crate::config::{AppConfig, RepoConfig};
use crate::github::client::GitHubClient;
use crate::review::engine::{ReviewEngine, ReviewOptions};
use crate::store::db::{dedupe_key, Database, ReviewClaim, ReviewFindingUpsert};

const MARKER: &str = crate::review::policy::REVIEW_METADATA_MARKER;

/// A `claude` stub that answers review prompts with `response-N.md` (N
/// counts review prompts) and saves each prompt as `prompt-N.md`. Other
/// prompts (the in-progress comment) get a fixed sentence.
fn harness_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("pr-reviewer-harness")
            .tempdir()
            .unwrap()
            .keep();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            r#"#!/bin/sh
input=$(cat)
case "$input" in
  *pr-review-json*) ;;
  *) printf 'Taking a look.\n'; exit 0 ;;
esac
dir="$(dirname "$0")/runs/$(printf '%s' "$input" | sed -n 's/^Run: //p' | head -n 1)"
n=$(cat "$dir/count" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s' "$n" > "$dir/count"
printf '%s' "$input" > "$dir/prompt-$n.md"
printf '%s|' "$@" > "$dir/args-$n.txt"
cat "$dir/response-$n.md"
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Prepending to PATH changes no other lookup: the stub is only
        // found as `claude`, which no other test runs. std serializes env
        // access from Rust code.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![dir.clone()];
        entries.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
        dir
    })
}

/// One test's stub responses. The repo's `custom_instructions` carry a
/// `Run: <name>` line into the prompt so parallel tests do not share files.
struct Run {
    name: String,
    dir: PathBuf,
}

impl Run {
    fn new(name: &str) -> Self {
        let dir = harness_dir().join("runs").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            name: name.to_string(),
            dir,
        }
    }

    fn respond(&self, n: usize, review: Value) {
        std::fs::write(
            self.dir.join(format!("response-{n}.md")),
            format!("```pr-review-json\n{review}\n```\n"),
        )
        .unwrap();
    }

    fn prompt(&self, n: usize) -> String {
        std::fs::read_to_string(self.dir.join(format!("prompt-{n}.md"))).unwrap()
    }

    /// The stub's command-line arguments for review prompt N, each ended by `|`.
    fn args(&self, n: usize) -> String {
        std::fs::read_to_string(self.dir.join(format!("args-{n}.txt"))).unwrap()
    }

    fn reviews_run(&self) -> usize {
        std::fs::read_to_string(self.dir.join("count"))
            .map(|n| n.trim().parse().unwrap())
            .unwrap_or(0)
    }
}

fn review_json(verdict: &str, comments: Value) -> Value {
    json!({
        "summary": "Review of the fetcher.",
        "verdict": verdict,
        "confidence": {
            "level": "high",
            "reasons": ["sufficient_diff_evidence"],
            "justification": "Visible in src/fetch.rs."
        },
        "ui_screenshot_needed": false,
        "comments": comments
    })
}

fn pr_json(number: u64, sha: &str, head_ref: &str, base_ref: &str) -> Value {
    json!({
        "number": number,
        "title": format!("PR {number}"),
        "body": "Change the fetcher.",
        "state": "open",
        "draft": false,
        "user": {"login": "alice", "type": "User"},
        "head": {"sha": sha, "ref": head_ref,
                 "repo": {"full_name": "o/r", "owner": {"login": "o"}}},
        "base": {"ref": base_ref,
                 "repo": {"full_name": "o/r", "owner": {"login": "o"}, "default_branch": "main"}}
    })
}

fn bot_review(id: u64, sha: &str, state: &str) -> Value {
    json!({
        "id": id,
        "body": format!("<details>\n{MARKER}https://example.test)\n</details>\nEarlier review."),
        "user": {"login": "bot-user", "type": "User"},
        "state": state,
        "commit_id": sha,
        "submitted_at": "2026-10-16T10:00:00Z"
    })
}

fn bot_comment(id: u64, line: u32, body: &str) -> Value {
    json!({
        "id": id, "body": body, "path": "src/fetch.rs", "line": line,
        "user": {"login": "bot-user", "type": "User"},
        "in_reply_to_id": null,
        "created_at": format!("2026-10-16T10:{:02}:00Z", id % 60)
    })
}

/// GitHub as one round sees it.
struct GitHubState {
    pr: Value,
    diff: String,
    reviews: Vec<Value>,
    review_comments: Vec<Value>,
    /// (base sha, head sha, diff) for the incremental compare.
    compare: Option<(String, String, String)>,
    /// Open PRs by head branch, for stack lookups.
    heads: Vec<(String, Value)>,
    /// Status for `GET .../reviews` and `POST .../reviews` (200 = works).
    list_reviews_status: u16,
    post_review_status: u16,
    /// Status for `POST .../statuses/<sha>` (201 = created).
    commit_status_status: u16,
    /// Root comment ids of the review threads a human resolved.
    resolved: Vec<u64>,
    /// Status for the PR diff request (200 = works).
    diff_status: u16,
    /// `pr-reviewer/claim` statuses on the head before the round, oldest first.
    claims: Vec<Value>,
    /// A peer's claim that lands right after this host's claim does.
    rival_claim: Option<Value>,
}

impl GitHubState {
    fn new(pr: Value, diff: String) -> Self {
        Self {
            pr,
            diff,
            reviews: vec![],
            review_comments: vec![],
            compare: None,
            heads: vec![],
            list_reviews_status: 200,
            post_review_status: 200,
            commit_status_status: 201,
            resolved: vec![],
            diff_status: 200,
            claims: vec![],
            rival_claim: None,
        }
    }
}

/// A `pr-reviewer/claim` status as GitHub reports it, made `age_secs` ago.
fn claim_status(state: &str, description: &str, age_secs: i64) -> Value {
    let created = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
    json!({
        "context": "pr-reviewer/claim",
        "state": state,
        "description": description,
        "created_at": created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

struct MockGitHub {
    server: Server,
    posted_reviews: Arc<Mutex<Vec<Value>>>,
    /// The bodies of the PR's issue comments, in creation order.
    issue_comments: Arc<Mutex<Vec<Value>>>,
    dismissed: Arc<Mutex<Vec<String>>>,
    /// The bodies of the commit status requests for the PR's head sha, except
    /// the `pr-reviewer/claim` ones.
    statuses: Arc<Mutex<Vec<Value>>>,
    /// The `pr-reviewer/claim` statuses on the head, oldest first.
    claims: Arc<Mutex<Vec<Value>>>,
}

fn mock_github(state: GitHubState) -> MockGitHub {
    let server = Server::run();
    let number = state.pr["number"].as_u64().unwrap();
    let pr_path = format!("/repos/o/r/pulls/{number}");
    // Fallback for everything a round does not care about (file contents,
    // conventions, durable comments): GitHub's "not found".
    server.expect(
        Expectation::matching(any())
            .times(..)
            .respond_with(status_code(404)),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", "/user"))
            .times(..)
            .respond_with(json_encoded(json!({"login": "bot-user"}))),
    );
    server.expect(
        Expectation::matching(request::method_path("GET", pr_path.clone()))
            .times(..)
            .respond_with(json_encoded(state.pr.clone())),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", pr_path.clone()),
            request::headers(contains(("accept", "application/vnd.github.v3.diff"))),
        ])
        .times(..)
        .respond_with(status_code(state.diff_status).body(state.diff.clone())),
    );
    if state.list_reviews_status == 200 {
        server.expect(
            Expectation::matching(request::method_path("GET", format!("{pr_path}/reviews")))
                .times(..)
                .respond_with(json_encoded(Value::Array(state.reviews.clone()))),
        );
    } else {
        server.expect(
            Expectation::matching(request::method_path("GET", format!("{pr_path}/reviews")))
                .times(..)
                .respond_with(status_code(state.list_reviews_status)),
        );
    }
    server.expect(
        Expectation::matching(request::method_path("GET", format!("{pr_path}/comments")))
            .times(..)
            .respond_with(json_encoded(Value::Array(state.review_comments.clone()))),
    );
    // The PR's issue comments, as GitHub keeps them: a create adds one, an
    // update rewrites it. Comment ids count from 1.
    let issue_comments: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let issue_comments_path = format!("/repos/o/r/issues/{number}/comments");
    let listed = issue_comments.clone();
    server.expect(
        Expectation::matching(request::method_path("GET", issue_comments_path.clone()))
            .times(..)
            .respond_with(move || json_encoded(Value::Array(listed.lock().unwrap().clone()))),
    );
    let created = issue_comments.clone();
    let created_back = issue_comments.clone();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", issue_comments_path),
            request::body(json_decoded(move |body: &Value| {
                let mut comments = created.lock().unwrap();
                let id = comments.len() as u64 + 1;
                comments.push(json!({
                    "id": id, "body": body["body"],
                    "user": {"login": "bot-user", "type": "User"},
                    "created_at": "2026-10-16T10:00:00Z"
                }));
                true
            })),
        ])
        .times(..)
        .respond_with(move || json_encoded(created_back.lock().unwrap().last().cloned())),
    );
    let updated = issue_comments.clone();
    let updated_back = issue_comments.clone();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("PATCH", "/repos/o/r/issues/comments/1"),
            request::body(json_decoded(move |body: &Value| {
                updated.lock().unwrap()[0]["body"] = body["body"].clone();
                true
            })),
        ])
        .times(..)
        .respond_with(move || json_encoded(updated_back.lock().unwrap()[0].clone())),
    );
    let threads: Vec<Value> = state
        .review_comments
        .iter()
        .map(|c| {
            let resolved = state.resolved.contains(&c["id"].as_u64().unwrap());
            json!({"isResolved": resolved, "comments": {"nodes": [{"databaseId": c["id"]}]}})
        })
        .collect();
    server.expect(
        Expectation::matching(request::method_path("POST", "/graphql"))
            .times(..)
            .respond_with(json_encoded(
                json!({"data": {"repository": {"pullRequest": {
                "reviewThreads": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                                  "nodes": threads}}}}}),
            )),
    );
    if let Some((base, head, diff)) = state.compare {
        server.expect(
            Expectation::matching(request::method_path(
                "GET",
                format!("/repos/o/r/compare/{base}...{head}"),
            ))
            .times(..)
            .respond_with(status_code(200).body(diff)),
        );
    }
    for (head_ref, pr) in state.heads {
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/repos/o/r/pulls"),
                request::query(url_decoded(contains(("head", format!("o:{head_ref}"))))),
            ])
            .times(..)
            .respond_with(json_encoded(json!([pr]))),
        );
    }
    let posted_reviews = Arc::new(Mutex::new(Vec::new()));
    let sink = posted_reviews.clone();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", format!("{pr_path}/reviews")),
            request::body(json_decoded(move |body: &Value| {
                sink.lock().unwrap().push(body.clone());
                true
            })),
        ])
        .times(..)
        .respond_with(status_code(state.post_review_status).body("{\"id\": 999}")),
    );
    let dismissed = Arc::new(Mutex::new(Vec::new()));
    for review in &state.reviews {
        let path = format!("{pr_path}/reviews/{}/dismissals", review["id"]);
        let sink = dismissed.clone();
        let recorded = path.clone();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("PUT", path),
                request::body(json_decoded(move |_: &Value| {
                    sink.lock().unwrap().push(recorded.clone());
                    true
                })),
            ])
            .times(..)
            .respond_with(json_encoded(json!({}))),
        );
    }
    let head_sha = state.pr["head"]["sha"].as_str().unwrap().to_string();
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let claims = Arc::new(Mutex::new(state.claims.clone()));
    let sink = statuses.clone();
    let claim_sink = claims.clone();
    let rival_claim = state.rival_claim.clone();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", format!("/repos/o/r/statuses/{head_sha}")),
            request::body(json_decoded(move |body: &Value| {
                if body["context"] == "pr-reviewer/claim" {
                    let mut claims = claim_sink.lock().unwrap();
                    claims.push(json!({
                        "context": body["context"], "state": body["state"],
                        "description": body["description"],
                        "created_at": chrono::Utc::now()
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    }));
                    if body["state"] == "pending" {
                        claims.extend(rival_claim.clone());
                    }
                } else {
                    sink.lock().unwrap().push(body.clone());
                }
                true
            })),
        ])
        .times(..)
        .respond_with(status_code(state.commit_status_status).body("{}")),
    );
    // GitHub's combined status: the latest status of each context.
    let latest = claims.clone();
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            format!("/repos/o/r/commits/{head_sha}/status"),
        ))
        .times(..)
        .respond_with(move || {
            json_encoded(json!({"statuses": latest.lock().unwrap().last().cloned()
                .into_iter().collect::<Vec<_>>()}))
        }),
    );
    MockGitHub {
        server,
        posted_reviews,
        issue_comments,
        dismissed,
        statuses,
        claims,
    }
}

fn config(run: &Run) -> Arc<AppConfig> {
    let mut config = AppConfig::default();
    config.harness.model = "test-model".to_string();
    config.harness.timeout_secs = 60;
    config.repos.push(RepoConfig {
        owner: "o".to_string(),
        name: "r".to_string(),
        local_path: None,
        harness: None,
        model: None,
        reasoning_effort: None,
        review_tools: None,
        fork_policy: Default::default(),
        trusted_authors: vec![],
        ignore_paths: vec![],
        custom_instructions: Some(format!("Run: {}", run.name)),
        gitnexus: false,
        workflow: vec![],
        auto_fix: Default::default(),
        review_policy: Default::default(),
    });
    Arc::new(config)
}

async fn review_round(
    config: &Arc<AppConfig>,
    db: &Database,
    github: &MockGitHub,
    number: u64,
) -> crate::review::engine::ReviewRunResult {
    let client =
        GitHubClient::new_with_base("test-token".into(), &github.server.url_str("")).unwrap();
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone()).with_host("test-host");
    engine.init().await;
    engine
        .review_pr(&config.repos[0], number, ReviewOptions::default())
        .await
        .unwrap()
}

async fn try_review_round(
    config: &Arc<AppConfig>,
    db: &Database,
    github: &MockGitHub,
    number: u64,
) -> anyhow::Result<crate::review::engine::ReviewRunResult> {
    let client =
        GitHubClient::new_with_base("test-token".into(), &github.server.url_str("")).unwrap();
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone()).with_host("test-host");
    engine.init().await;
    engine
        .review_pr(&config.repos[0], number, ReviewOptions::default())
        .await
}

fn fetch_diff(header: &str, body_lines: &[&str]) -> String {
    format!(
        "diff --git a/src/fetch.rs b/src/fetch.rs\nindex 1111111..2222222 100644\n--- a/src/fetch.rs\n+++ b/src/fetch.rs\n{header}\n{}\n",
        body_lines.join("\n")
    )
}

const BLOCKER: &str = "The retry loop in `fetch_all` never sleeps, so a 429 from GitHub spins the CPU and burns the whole rate limit in seconds.";
const CACHE_WARNING: &str = "The cache key in `Cache::shared` ignores the token scope, so two tokens with different repo access can read each other's cached responses.";
const NITPICK: &str = "`loop_with_backoff` reads the backoff from a magic number; name it so the next reader knows the unit.";

#[tokio::test]
async fn review_lifecycle_caps_rounds_skips_rebases_and_escalates_critical_findings() {
    let run = Run::new("lifecycle");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();

    // Round 1: the full review finds a blocker and a warning.
    let diff1 = fetch_diff(
        "@@ -1,4 +1,6 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            "+    let pages = loop_without_sleep(client);",
            "+    let cache = Cache::shared(TOKEN_SCOPE_IGNORED);",
            "     let mut out = Vec::new();",
            "     out",
            " }",
        ],
    );
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([
                {"file": "src/fetch.rs", "line": 2, "body": BLOCKER, "severity": "blocking",
                 "finding_kind": "correctness", "evidence_note": "Line 2 calls loop_without_sleep."},
                {"file": "src/fetch.rs", "line": 3, "body": CACHE_WARNING, "severity": "warning",
                 "finding_kind": "security"}
            ]),
        ),
    );
    let gh = mock_github(GitHubState {
        pr: pr_json(7, "s1", "feat/fetch", "main"),
        diff: diff1,
        reviews: vec![],
        review_comments: vec![],
        compare: None,
        heads: vec![],
        list_reviews_status: 200,
        post_review_status: 200,
        commit_status_status: 201,
        resolved: vec![],
        diff_status: 200,
        claims: vec![],
        rival_claim: None,
    });
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    {
        let posted = gh.posted_reviews.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0]["event"], "REQUEST_CHANGES");
        assert_eq!(posted[0]["comments"].as_array().unwrap().len(), 2);
    }
    assert!(run
        .prompt(1)
        .contains("This is the first review of this PR. Make it complete"));

    // Round 2: the author fixed the blocker. The model repeats the open
    // warning and adds a nitpick. The warning is not posted again, the
    // blocker is resolved, and the bot dismisses its change request.
    let diff2 = fetch_diff(
        "@@ -1,4 +1,6 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            "+    let pages = loop_with_backoff(client);",
            "+    let cache = Cache::shared(TOKEN_SCOPE_IGNORED);",
            "     let mut out = Vec::new();",
            "     out",
            " }",
        ],
    );
    run.respond(
        2,
        review_json(
            "comment",
            json!([
                {"file": "src/fetch.rs", "line": 3, "severity": "warning", "finding_kind": "security",
                 "body": "Still open: `Cache::shared` builds its key without the token scope, so tokens with different repo access read each other's cached responses."},
                {"file": "src/fetch.rs", "line": 2, "body": NITPICK, "severity": "nitpick",
                 "finding_kind": "correctness"}
            ]),
        ),
    );
    let round1_comments = vec![
        bot_comment(201, 2, BLOCKER),
        bot_comment(202, 3, CACHE_WARNING),
    ];
    let gh = mock_github(GitHubState {
        pr: pr_json(7, "s2", "feat/fetch", "main"),
        diff: diff2.clone(),
        reviews: vec![bot_review(100, "s1", "CHANGES_REQUESTED")],
        review_comments: round1_comments.clone(),
        compare: Some(("s1".into(), "s2".into(), diff2.clone())),
        heads: vec![],
        list_reviews_status: 200,
        post_review_status: 200,
        commit_status_status: 201,
        resolved: vec![],
        diff_status: 200,
        claims: vec![],
        rival_claim: None,
    });
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    {
        let posted = gh.posted_reviews.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0]["event"], "COMMENT");
        let comments = posted[0]["comments"].as_array().unwrap();
        assert_eq!(
            comments.len(),
            1,
            "only the new nitpick is posted: {comments:?}"
        );
        assert_eq!(comments[0]["body"], NITPICK);
        let body = posted[0]["body"].as_str().unwrap();
        assert!(
            body.contains("1 earlier finding is still open in its thread"),
            "{body}"
        );
        assert!(body.contains("Fixed or likely addressed: 1"), "{body}");
    }
    assert_eq!(
        *gh.dismissed.lock().unwrap(),
        vec!["/repos/o/r/pulls/7/reviews/100/dismissals".to_string()],
        "the stale change request is dismissed"
    );
    assert!(run
        .prompt(2)
        .contains("This is review cycle 2 of up to 2 review cycles"));

    // Round 3: a rebase. Line numbers, the index line and a farther context
    // line change; the PR's own change does not. No review, no round used.
    let diff3 = fetch_diff(
        "@@ -9,4 +9,6 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            "+    let pages = loop_with_backoff(client);",
            "+    let cache = Cache::shared(TOKEN_SCOPE_IGNORED);",
            "     let mut out = Vec::new();",
            "     out // collected",
            " }",
        ],
    )
    .replace("1111111..2222222", "3333333..4444444");
    let round2_comments = {
        let mut comments = round1_comments.clone();
        comments.push(bot_comment(203, 2, NITPICK));
        comments
    };
    let gh = mock_github(GitHubState {
        pr: pr_json(7, "s3", "feat/fetch", "main"),
        diff: diff3,
        reviews: vec![
            bot_review(100, "s1", "DISMISSED"),
            bot_review(101, "s2", "COMMENTED"),
        ],
        review_comments: round2_comments.clone(),
        compare: None,
        heads: vec![],
        list_reviews_status: 200,
        post_review_status: 200,
        commit_status_status: 201,
        resolved: vec![],
        diff_status: 200,
        claims: vec![],
        rival_claim: None,
    });
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "skipped:unchanged-diff");
    assert!(gh.posted_reviews.lock().unwrap().is_empty());
    assert_eq!(run.reviews_run(), 2, "the harness did not run");

    // Round 3 of review (fourth push): past the limit of 2. A critical
    // security finding on a changed line is still posted inline and requests
    // changes; the other new findings become non-blocking notes.
    let diff4 = fetch_diff(
        "@@ -9,4 +9,7 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            "+    let pages = loop_with_backoff(client);",
            "+    let cache = Cache::shared(TOKEN_SCOPE_IGNORED);",
            "+    let query = format!(\"SELECT * FROM pages WHERE owner = '{}'\", client.owner());",
            "     let mut out = Vec::new();",
            "     out // collected",
            " }",
        ],
    );
    const SQL: &str = "`query` interpolates `client.owner()` into the SQL string, so an owner name with a quote injects SQL.";
    const BOUND: &str = "`loop_with_backoff` has no upper bound on attempts; add a cap so a dead endpoint does not retry forever.";
    const README: &str = "The README still describes the old retry behaviour of the fetcher and the backoff settings it no longer has.";
    run.respond(
        3,
        review_json(
            "request_changes",
            json!([
                {"file": "src/fetch.rs", "line": 12, "body": SQL, "severity": "blocking",
                 "finding_kind": "security", "evidence_note": "New line 12 builds the query with format!."},
                {"file": "src/fetch.rs", "line": 10, "body": BOUND, "severity": "warning",
                 "finding_kind": "correctness"},
                {"file": "README.md", "line": 10, "body": README, "severity": "blocking",
                 "finding_kind": "documentation", "evidence_note": "README line 10."}
            ]),
        ),
    );
    let gh = mock_github(GitHubState {
        pr: pr_json(7, "s4", "feat/fetch", "main"),
        diff: diff4.clone(),
        reviews: vec![
            bot_review(100, "s1", "DISMISSED"),
            bot_review(101, "s2", "COMMENTED"),
        ],
        review_comments: round2_comments,
        compare: Some(("s3".into(), "s4".into(), diff4)),
        heads: vec![],
        list_reviews_status: 200,
        post_review_status: 200,
        commit_status_status: 201,
        resolved: vec![],
        diff_status: 200,
        claims: vec![],
        rival_claim: None,
    });
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    let posted = gh.posted_reviews.lock().unwrap();
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0]["event"], "REQUEST_CHANGES",
        "the critical finding still blocks"
    );
    let comments = posted[0]["comments"].as_array().unwrap();
    assert_eq!(
        comments.len(),
        1,
        "only the critical finding is inline: {comments:?}"
    );
    assert_eq!(comments[0]["body"], SQL);
    let body = posted[0]["body"].as_str().unwrap();
    assert!(
        body.contains("### Follow-up notes (non-blocking)"),
        "{body}"
    );
    assert!(
        body.contains("review cycle 3 and inline findings stop after review cycle 2"),
        "{body}"
    );
    assert!(body.contains(BOUND) && body.contains(README), "{body}");
    assert!(run
        .prompt(3)
        .contains("after the limit of 2 review cycles with inline findings"));
}

#[tokio::test]
async fn stacked_child_does_not_repeat_its_parents_findings() {
    let run = Run::new("stack");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();

    // The parent PR #7 already tracks the cache-key warning.
    db.upsert_review_findings(vec![ReviewFindingUpsert {
        repo: "o/r".to_string(),
        pr_number: 7,
        author: Some("alice".to_string()),
        fingerprint: "parent-cache".to_string(),
        recurrence_fingerprint: None,
        sha: "p1".to_string(),
        status: "open".to_string(),
        severity: "warning".to_string(),
        finding_kind: "security".to_string(),
        path: "src/fetch.rs".to_string(),
        line: Some(3),
        body: CACHE_WARNING.to_string(),
        evidence_note: None,
        github_comment_id: None,
        resolved_by_sha: None,
        resolution_reason: None,
    }])
    .await
    .unwrap();

    // The child #8 sits on #7's branch and changes another file.
    let child_diff = "diff --git a/src/pages.rs b/src/pages.rs\n--- a/src/pages.rs\n+++ b/src/pages.rs\n@@ -1,2 +1,3 @@\n pub fn pages() -> usize {\n+    let total = fetch_all(&CLIENT).len() * PAGE_SIZE_UNCHECKED;\n     0\n"
        .to_string();
    const OVERFLOW: &str = "`PAGE_SIZE_UNCHECKED` times the page count can overflow `usize` on 32-bit targets; use `checked_mul`.";
    run.respond(
        1,
        review_json(
            "comment",
            json!([
                // The parent's problem, seen through the child's context.
                {"file": "src/fetch.rs", "line": 3, "severity": "warning", "finding_kind": "security",
                 "body": "The cache key in `Cache::shared` still ignores the token scope, so two tokens with different repo access can read each other's cached responses."},
                {"file": "src/pages.rs", "line": 2, "body": OVERFLOW, "severity": "warning",
                 "finding_kind": "correctness"}
            ]),
        ),
    );
    let gh = mock_github(GitHubState {
        pr: pr_json(8, "c1", "feat/pages", "feat/fetch"),
        diff: child_diff,
        reviews: vec![],
        review_comments: vec![],
        compare: None,
        heads: vec![(
            "feat/fetch".to_string(),
            pr_json(7, "p1", "feat/fetch", "main"),
        )],
        list_reviews_status: 200,
        post_review_status: 200,
        commit_status_status: 201,
        resolved: vec![],
        diff_status: 200,
        claims: vec![],
        rival_claim: None,
    });
    let result = review_round(&config, &db, &gh, 8).await;
    assert_eq!(result.status, "completed");
    let posted = gh.posted_reviews.lock().unwrap().clone();
    let comments = posted[0]["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0]["body"], OVERFLOW);
    let body = posted[0]["body"].as_str().unwrap();
    assert!(
        body.contains("1 finding is already tracked on #7"),
        "{body}"
    );

    // A review session runs with no built-in tools by default.
    assert!(
        run.args(1)
            .contains("|--tools||--strict-mcp-config|--dangerously"),
        "{}",
        run.args(1)
    );
    let prompt = run.prompt(1);
    assert!(prompt.contains("## Stacked PR"), "{prompt}");
    assert!(prompt.contains("head branch of #7"), "{prompt}");
    assert!(prompt.contains("#7 `src/fetch.rs:3`"), "{prompt}");
    let child_ledger = db.list_review_findings("o/r", 8).await.unwrap();
    assert_eq!(
        child_ledger.len(),
        1,
        "the parent's finding stays on #7 only"
    );
}

fn simple_diff(guard: &str) -> String {
    fetch_diff(
        "@@ -1,3 +1,4 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            &format!("+    {guard}"),
            "     let mut out = Vec::new();",
            "     out",
        ],
    )
}

#[tokio::test]
async fn a_review_that_never_reached_github_is_not_treated_as_reviewed() {
    let run = Run::new("failed-post");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    let finding = json!([{"file": "src/fetch.rs", "line": 2, "body": BLOCKER,
        "severity": "blocking", "finding_kind": "correctness", "evidence_note": "Line 2."}]);
    run.respond(1, review_json("request_changes", finding.clone()));
    run.respond(2, review_json("request_changes", finding));

    let mut state = GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    );
    state.post_review_status = 500;
    let gh = mock_github(state);
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    assert!(
        result.fingerprint.is_none(),
        "a failed post records no fingerprint"
    );
    assert!(
        db.list_review_findings("o/r", 7).await.unwrap().is_empty(),
        "the ledger records only what reached GitHub"
    );

    // A rebase with the same change: the review runs again and posts.
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s2", "feat/fetch", "main"),
        simple_diff("retry();").replace("@@ -1,3 +1,4 @@", "@@ -7,3 +7,4 @@"),
    ));
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    assert_eq!(gh.posted_reviews.lock().unwrap().len(), 1);
    assert_eq!(run.reviews_run(), 2);
}

#[tokio::test]
async fn an_unknown_review_history_fails_the_review_instead_of_restarting_the_count() {
    let run = Run::new("no-history");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    let mut state = GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    );
    state.list_reviews_status = 502;
    let gh = mock_github(state);
    let err = try_review_round(&config, &db, &gh, 7).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("failed to list existing reviews"),
        "{err:#}"
    );
    assert_eq!(run.reviews_run(), 0, "the harness did not run");
    assert!(gh.posted_reviews.lock().unwrap().is_empty());
}

#[tokio::test]
async fn past_the_limit_notes_do_not_block_and_off_diff_critical_findings_stay_visible() {
    let run = Run::new("notes-only");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    const STYLE: &str = "`retry_slowly` is a vague name; `retry_with_backoff` says what it does.";
    const OFF_DIFF: &str = "The unchanged `parse_page` below trusts the `Content-Length` header and can allocate gigabytes for a hostile response.";
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([
                {"file": "src/fetch.rs", "line": 2, "body": STYLE, "severity": "nitpick",
                 "finding_kind": "correctness"},
                // Critical kind and evidence, but on a context line.
                {"file": "src/fetch.rs", "line": 3, "body": OFF_DIFF, "severity": "blocking",
                 "finding_kind": "security", "evidence_note": "parse_page at line 3."}
            ]),
        ),
    );
    let mut state = GitHubState::new(
        pr_json(7, "s3", "feat/fetch", "main"),
        simple_diff("retry_slowly();"),
    );
    state.reviews = vec![
        bot_review(100, "s1", "COMMENTED"),
        bot_review(101, "s2", "COMMENTED"),
    ];
    let gh = mock_github(state);
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    let posted = gh.posted_reviews.lock().unwrap().clone();
    assert_eq!(posted[0]["event"], "COMMENT", "notes never request changes");
    assert!(
        posted[0]["comments"].as_array().is_none_or(Vec::is_empty),
        "no inline threads: {:?}",
        posted[0]["comments"]
    );
    let body = posted[0]["body"].as_str().unwrap();
    let blocker = body
        .find(OFF_DIFF)
        .expect("the critical finding is still shown");
    let nit = body.find(STYLE).expect("the nitpick is a note");
    assert!(blocker < nit, "possible blockers are listed first: {body}");
    assert!(body.contains("possible blocker"), "{body}");
}

#[tokio::test]
async fn an_outdated_thread_does_not_hide_a_different_finding_elsewhere_in_the_file() {
    let run = Run::new("outdated-thread");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    // Shares 6 of 12 meaningful words with BLOCKER (containment 0.5), and the
    // `fetch_all` identifier, but it is a different bug 300 lines away.
    const RATE: &str = "`fetch_all` never checks the rate limit header before retrying, so a revoked token keeps sending requests until GitHub blocks the whole app.";
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([{"file": "src/fetch.rs", "line": 300, "body": RATE, "severity": "blocking",
                    "finding_kind": "security", "evidence_note": "New line 300 retries without reading the header."}]),
        ),
    );
    let diff = "diff --git a/src/fetch.rs b/src/fetch.rs\n--- a/src/fetch.rs\n+++ b/src/fetch.rs\n@@ -298,2 +298,3 @@\n fn retry(client: &Client) {\n+    client.send_again();\n }\n".replace("@@ -298,2 +298,3 @@\n fn", "@@ -299,2 +299,3 @@\n fn");
    let mut state = GitHubState::new(pr_json(7, "s2", "feat/fetch", "main"), diff);
    state.reviews = vec![bot_review(100, "s1", "COMMENTED")];
    // A push changed the lines under the old thread: GitHub reports no line.
    let mut outdated = bot_comment(201, 2, BLOCKER);
    outdated["line"] = Value::Null;
    outdated["original_line"] = json!(2);
    state.review_comments = vec![outdated];
    let gh = mock_github(state);
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(result.status, "completed");
    let posted = gh.posted_reviews.lock().unwrap().clone();
    let comments = posted[0]["comments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        comments.len(),
        1,
        "the new finding is posted, not held back: {posted:?}"
    );
    assert_eq!(comments[0]["body"], RATE);
    assert_eq!(posted[0]["event"], "REQUEST_CHANGES");
}

#[tokio::test]
async fn a_rebase_of_a_pr_with_a_binary_file_keeps_its_findings_open() {
    let run = Run::new("binary-rebase");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    let binary = "diff --git a/logo.png b/logo.png\nindex 1111111..2222222 100644\nBinary files a/logo.png and b/logo.png differ\n";
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([{"file": "src/fetch.rs", "line": 2, "body": BLOCKER, "severity": "blocking",
                    "finding_kind": "correctness", "evidence_note": "Line 2."}]),
        ),
    );
    run.respond(2, review_json("comment", json!([])));

    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        format!("{}{binary}", simple_diff("retry_forever();")),
    ));
    assert_eq!(review_round(&config, &db, &gh, 7).await.status, "completed");

    // Rebase: same change, new line numbers. GitHub's three-dot compare of the
    // old and new head lists every file of the PR after a rebase.
    let rebased = format!(
        "{}{binary}",
        simple_diff("retry_forever();").replace("@@ -1,3 +1,4 @@", "@@ -20,3 +20,4 @@")
    );
    let mut state = GitHubState::new(pr_json(7, "s2", "feat/fetch", "main"), rebased.clone());
    state.reviews = vec![bot_review(100, "s1", "CHANGES_REQUESTED")];
    state.review_comments = vec![bot_comment(201, 2, BLOCKER)];
    state.compare = Some(("s1".into(), "s2".into(), rebased));
    let gh = mock_github(state);
    let result = review_round(&config, &db, &gh, 7).await;
    assert_eq!(
        run.reviews_run(),
        2,
        "a diff with a binary file is always reviewed"
    );
    assert_eq!(result.status, "skipped:no-novel-findings");
    let ledger = db.list_review_findings("o/r", 7).await.unwrap();
    assert_eq!(ledger.len(), 1);
    assert!(
        matches!(ledger[0].status.as_str(), "open" | "still_blocking"),
        "a rebase does not fix a finding: {}",
        ledger[0].status
    );
    assert!(
        gh.dismissed.lock().unwrap().is_empty(),
        "the change request stays"
    );
}

#[tokio::test]
async fn resolving_the_blocking_thread_dismisses_the_change_request_without_a_review() {
    let run = Run::new("human-answers");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();

    // Round 1 requests changes for a blocker.
    let diff = fetch_diff(
        "@@ -1,3 +1,4 @@",
        &[
            " pub fn fetch_all(client: &Client) -> Vec<Page> {",
            "+    let pages = loop_without_sleep(client);",
            "     pages",
            " }",
        ],
    );
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([{"file": "src/fetch.rs", "line": 2, "body": BLOCKER, "severity": "blocking",
                    "finding_kind": "correctness",
                    "evidence_note": "Line 2 calls loop_without_sleep."}]),
        ),
    );
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        diff.clone(),
    ));
    review_round(&config, &db, &gh, 7).await;
    assert_eq!(
        gh.posted_reviews.lock().unwrap()[0]["event"],
        "REQUEST_CHANGES"
    );

    // The author fixes it elsewhere and nobody pushes to this file. While the
    // thread is open, the change request stays.
    let sync = |resolved: Vec<u64>| {
        let mut state = GitHubState::new(pr_json(7, "s1", "feat/fetch", "main"), diff.clone());
        state.reviews = vec![bot_review(100, "s1", "CHANGES_REQUESTED")];
        state.review_comments = vec![bot_comment(201, 2, BLOCKER)];
        state.resolved = resolved;
        let gh = mock_github(state);
        let config = config.clone();
        let db = db.clone();
        async move {
            let client =
                GitHubClient::new_with_base("test-token".into(), &gh.server.url_str("")).unwrap();
            let mut engine = ReviewEngine::new(config.clone(), client, db);
            engine.init().await;
            let pr: crate::github::types::PullRequest =
                serde_json::from_value(pr_json(7, "s1", "feat/fetch", "main")).unwrap();
            let changed = engine
                .sync_human_answers(&config.repos[0], &pr)
                .await
                .unwrap();
            let dismissed = gh.dismissed.lock().unwrap().clone();
            assert!(gh.posted_reviews.lock().unwrap().is_empty());
            (changed, dismissed)
        }
    };
    assert_eq!(sync(vec![]).await, (false, vec![]));

    // A human resolves the thread: the bot dismisses its change request,
    // and no review runs.
    assert_eq!(
        sync(vec![201]).await,
        (
            true,
            vec!["/repos/o/r/pulls/7/reviews/100/dismissals".to_string()]
        )
    );
    assert_eq!(run.reviews_run(), 1);
    let ledger = db.list_review_findings("o/r", 7).await.unwrap();
    assert_eq!(ledger[0].status, "resolved");
}

fn comment_bodies(gh: &MockGitHub) -> Vec<String> {
    gh.issue_comments
        .lock()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap().to_string())
        .collect()
}

/// A first-touch diff that `should_skip_review` skips must not leave the
/// "will follow up shortly" comment behind: the comment is rewritten to say
/// the review was skipped and why, and no review is posted.
async fn assert_first_touch_skip_retracts_follow_up(name: &str, diff: &str, reason: &str) {
    let run = Run::new(name);
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();

    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/docs", "main"),
        diff.to_string(),
    ));
    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, format!("skipped:{reason}"));
    assert_eq!(run.reviews_run(), 0, "a skipped diff runs no review");
    assert!(gh.posted_reviews.lock().unwrap().is_empty());
    let bodies = comment_bodies(&gh);
    assert_eq!(bodies.len(), 1, "one comment, updated in place: {bodies:?}");
    assert!(
        !bodies[0].contains("will follow up"),
        "no promise of a review that never comes: {}",
        bodies[0]
    );
    assert!(
        bodies[0].contains("skipped") && bodies[0].contains(reason),
        "the comment says the review was skipped and why: {}",
        bodies[0]
    );
    assert!(bodies[0].contains("<!-- pr-reviewer-review pr=7 -->"));
}

#[tokio::test]
async fn a_docs_only_first_touch_skip_leaves_no_follow_up_promise() {
    let diff = "diff --git a/README.md b/README.md\nindex 1111111..2222222 100644\n--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,2 @@\n # Title\n+More words.\n";
    assert_first_touch_skip_retracts_follow_up("skip-docs-only", diff, "docs-only").await;
}

#[tokio::test]
async fn an_empty_diff_first_touch_skip_leaves_no_follow_up_promise() {
    assert_first_touch_skip_retracts_follow_up("skip-empty-diff", "", "empty-diff").await;
}

#[tokio::test]
async fn an_unchanged_diff_first_touch_skip_leaves_no_follow_up_promise() {
    let run = Run::new("skip-unchanged-diff");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    run.respond(1, review_json("no_issues", json!([])));

    // Round 1 reviews s1 and records the change's fingerprint. Its mock is
    // gone in round 2, so GitHub shows no review: a first touch again.
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    ));
    review_round(&config, &db, &gh, 7).await;

    let gh = mock_github(GitHubState::new(
        pr_json(7, "s2", "feat/fetch", "main"),
        simple_diff("retry();"),
    ));
    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "skipped:unchanged-diff");
    let bodies = comment_bodies(&gh);
    assert_eq!(bodies.len(), 1, "one comment, updated in place: {bodies:?}");
    assert!(!bodies[0].contains("will follow up"), "{}", bodies[0]);
    assert!(bodies[0].contains("skipped") && bodies[0].contains("unchanged-diff"));
}

#[tokio::test]
async fn a_normal_first_touch_posts_the_follow_up_comment_and_then_the_review() {
    let run = Run::new("first-touch-review");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    run.respond(1, review_json("no_issues", json!([])));

    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    ));
    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "completed");
    assert_eq!(gh.posted_reviews.lock().unwrap().len(), 1);
    let bodies = comment_bodies(&gh);
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(
        bodies[0].contains("pr-reviewer-status:in-progress") || bodies[0].contains("Review posted"),
        "{}",
        bodies[0]
    );
}

#[tokio::test]
async fn a_dry_run_skip_posts_no_comment() {
    let run = Run::new("skip-dry-run");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    let diff = "diff --git a/README.md b/README.md\nindex 1111111..2222222 100644\n--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,2 @@\n # Title\n+More words.\n";
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/docs", "main"),
        diff.to_string(),
    ));
    let client = GitHubClient::new_with_base("test-token".into(), &gh.server.url_str("")).unwrap();
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone());
    engine.init().await;
    let options = ReviewOptions {
        dry_run: true,
        ..ReviewOptions::default()
    };
    let result = engine
        .review_pr(&config.repos[0], 7, options)
        .await
        .unwrap();
    assert_eq!(result.status, "skipped:docs-only");
    assert!(comment_bodies(&gh).is_empty());
}

#[tokio::test]
async fn a_review_claimed_before_a_restart_is_reviewed_after_the_start_up_requeue() {
    let run = Run::new("restart-requeue");
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    run.respond(1, review_json("no_issues", json!([])));

    // The old daemon claimed the review and died: a claimed work item plus the
    // claim row of the review it had started.
    let harness = config.harness.default.as_str();
    let key = dedupe_key("o/r", 7, "s1", harness, false);
    assert!(db
        .claim_review(ReviewClaim {
            dedupe_key: key,
            repo: "o/r".to_string(),
            pr_number: 7,
            sha: "s1".to_string(),
            harness: harness.to_string(),
            model: None,
        })
        .await
        .unwrap());
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    ));
    let before = review_round(&config, &db, &gh, 7).await;
    assert_eq!(
        before.status, "skipped:duplicate",
        "the orphaned claim blocks the review until it is released"
    );

    db.requeue_orphaned_claims().await.unwrap();

    let after = review_round(&config, &db, &gh, 7).await;
    assert_eq!(after.status, "completed");
    assert_eq!(gh.posted_reviews.lock().unwrap().len(), 1);
    assert_eq!(run.reviews_run(), 1);
}

/// Round 1 posts a blocking finding on `s1`; round 2 rebases to `s2` with the
/// reviewer finding nothing new. Returns round 2's result and the statuses
/// GitHub received.
async fn re_review_with_nothing_new(
    name: &str,
    configure: impl FnOnce(&mut AppConfig),
    status_response: u16,
    dry_run: bool,
) -> (crate::review::engine::ReviewRunResult, Vec<Value>) {
    let run = Run::new(name);
    let mut app = (*config(&run)).clone();
    configure(&mut app);
    let config = Arc::new(app);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    run.respond(
        1,
        review_json(
            "request_changes",
            json!([{"file": "src/fetch.rs", "line": 2, "body": BLOCKER, "severity": "blocking",
                    "finding_kind": "correctness", "evidence_note": "Line 2."}]),
        ),
    );
    run.respond(2, review_json("comment", json!([])));

    let binary = "diff --git a/logo.png b/logo.png\nindex 1111111..2222222 100644\nBinary files a/logo.png and b/logo.png differ\n";
    let gh = mock_github(GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        format!("{}{binary}", simple_diff("retry_forever();")),
    ));
    assert_eq!(review_round(&config, &db, &gh, 7).await.status, "completed");
    assert!(
        gh.statuses.lock().unwrap().is_empty(),
        "a review that posts findings posts no status"
    );

    // A rebase: same change, new line numbers. The binary file makes the
    // review run anyway, and the reviewer finds nothing new.
    let rebased = format!(
        "{}{binary}",
        simple_diff("retry_forever();").replace("@@ -1,3 +1,4 @@", "@@ -20,3 +20,4 @@")
    );
    let mut state = GitHubState::new(pr_json(7, "s2", "feat/fetch", "main"), rebased.clone());
    state.compare = Some(("s1".into(), "s2".into(), rebased));
    state.reviews = vec![bot_review(100, "s1", "CHANGES_REQUESTED")];
    state.review_comments = vec![bot_comment(201, 2, BLOCKER)];
    state.commit_status_status = status_response;
    let gh = mock_github(state);
    let client = GitHubClient::new_with_base("test-token".into(), &gh.server.url_str("")).unwrap();
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone());
    engine.init().await;
    let options = ReviewOptions {
        dry_run,
        ..ReviewOptions::default()
    };
    let result = engine
        .review_pr(&config.repos[0], 7, options)
        .await
        .unwrap();
    let statuses = gh.statuses.lock().unwrap().clone();
    (result, statuses)
}

fn no_new_findings_status() -> Value {
    json!({"state": "success", "context": "pr-reviewer", "description": "Reviewed: no new findings"})
}

#[tokio::test]
async fn a_re_review_with_nothing_new_posts_a_status_when_the_setting_is_on() {
    let (result, statuses) = re_review_with_nothing_new(
        "status-on",
        |c| c.defaults.status_on_no_new_findings = true,
        201,
        false,
    )
    .await;
    assert_eq!(result.status, "skipped:no-novel-findings");
    assert_eq!(statuses, vec![no_new_findings_status()]);
}

#[tokio::test]
async fn a_re_review_with_nothing_new_posts_no_status_by_default() {
    let (result, statuses) = re_review_with_nothing_new("status-off", |_| {}, 201, false).await;
    assert_eq!(result.status, "skipped:no-novel-findings");
    assert!(statuses.is_empty());
}

#[tokio::test]
async fn a_repo_override_turns_the_status_on_over_a_default_off() {
    let (_, statuses) = re_review_with_nothing_new(
        "status-repo-on",
        |c| c.repos[0].review_policy.status_on_no_new_findings = Some(true),
        201,
        false,
    )
    .await;
    assert_eq!(statuses, vec![no_new_findings_status()]);
}

#[tokio::test]
async fn a_repo_override_turns_the_status_off_over_a_default_on() {
    let (result, statuses) = re_review_with_nothing_new(
        "status-repo-off",
        |c| {
            c.defaults.status_on_no_new_findings = true;
            c.repos[0].review_policy.status_on_no_new_findings = Some(false);
        },
        201,
        false,
    )
    .await;
    assert_eq!(result.status, "skipped:no-novel-findings");
    assert!(statuses.is_empty());
}

#[tokio::test]
async fn a_refused_status_does_not_fail_the_review() {
    let (result, statuses) = re_review_with_nothing_new(
        "status-403",
        |c| c.defaults.status_on_no_new_findings = true,
        403,
        false,
    )
    .await;
    assert_eq!(result.status, "skipped:no-novel-findings");
    assert_eq!(statuses.len(), 1, "the status was tried once");
}

#[tokio::test]
async fn a_dry_run_posts_no_status() {
    let (result, statuses) = re_review_with_nothing_new(
        "status-dry-run",
        |c| c.defaults.status_on_no_new_findings = true,
        201,
        true,
    )
    .await;
    assert_eq!(result.status, "completed:dry-run");
    assert!(statuses.is_empty());
}

/// The state and description of each claim status that GitHub received.
fn claim_log(gh: &MockGitHub) -> Vec<(String, String)> {
    gh.claims
        .lock()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["state"].as_str().unwrap().to_string(),
                c["description"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn claim_round_setup(name: &str) -> (Run, Arc<AppConfig>, Database, tempfile::TempDir) {
    let run = Run::new(name);
    let config = config(&run);
    let db_dir = tempfile::tempdir().unwrap();
    let db = Database::new(db_dir.path().join("state.db")).await.unwrap();
    run.respond(1, review_json("no_issues", json!([])));
    (run, config, db, db_dir)
}

fn state_with_claims(claims: Vec<Value>) -> GitHubState {
    let mut state = GitHubState::new(
        pr_json(7, "s1", "feat/fetch", "main"),
        simple_diff("retry();"),
    );
    state.claims = claims;
    state
}

#[tokio::test]
async fn a_review_cycle_claims_the_head_first_and_clears_the_claim_when_it_ends() {
    let (run, config, db, _dir) = claim_round_setup("claim-set-and-cleared").await;
    let gh = mock_github(state_with_claims(vec![]));

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "completed");
    assert_eq!(run.reviews_run(), 1);
    let log = claim_log(&gh);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].0, "pending");
    assert!(log[0].1.starts_with("claimed by test-host at "), "{log:?}");
    assert_eq!(log[1].0, "success");
    assert!(log[1].1.starts_with("released by test-host at "), "{log:?}");
}

#[tokio::test]
async fn a_head_with_a_fresh_claim_from_another_host_is_skipped() {
    let (run, config, db, _dir) = claim_round_setup("claim-skip-fresh").await;
    let gh = mock_github(state_with_claims(vec![claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T07:00:00Z",
        60,
    )]));

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "skipped:claimed-by-peer");
    assert_eq!(run.reviews_run(), 0);
    assert!(gh.posted_reviews.lock().unwrap().is_empty());
    assert!(comment_bodies(&gh).is_empty());
    assert_eq!(gh.claims.lock().unwrap().len(), 1, "no claim was added");

    // The skip leaves nothing behind, so the head can be reviewed later.
    let free = mock_github(state_with_claims(vec![]));
    assert_eq!(
        review_round(&config, &db, &free, 7).await.status,
        "completed"
    );
}

#[tokio::test]
async fn a_claim_older_than_the_ttl_is_taken_over() {
    let (run, config, db, _dir) = claim_round_setup("claim-take-over").await;
    let gh = mock_github(state_with_claims(vec![claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T06:00:00Z",
        16 * 60,
    )]));

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "completed");
    assert_eq!(run.reviews_run(), 1);
    let log = claim_log(&gh);
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(log[1].1.starts_with("claimed by test-host at "), "{log:?}");
    assert!(log[2].1.starts_with("released by test-host at "), "{log:?}");
}

#[tokio::test]
async fn a_host_backs_off_when_the_read_back_shows_a_newer_claim_from_a_peer() {
    let (run, config, db, _dir) = claim_round_setup("claim-read-back-conflict").await;
    let mut state = state_with_claims(vec![]);
    state.rival_claim = Some(claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T07:00:00Z",
        0,
    ));
    let gh = mock_github(state);

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "skipped:claimed-by-peer");
    assert_eq!(run.reviews_run(), 0);
    assert!(gh.posted_reviews.lock().unwrap().is_empty());
    let log = claim_log(&gh);
    assert_eq!(
        log.len(),
        2,
        "the loser leaves the winner's claim alone: {log:?}"
    );
    assert!(log[1].1.starts_with("claimed by other-host at "), "{log:?}");
}

#[tokio::test]
async fn a_failed_review_cycle_clears_the_claim() {
    let (_run, config, db, _dir) = claim_round_setup("claim-cleared-on-failure").await;
    // GitHub cannot serve the diff, so the review cycle fails.
    let mut state = state_with_claims(vec![]);
    state.diff_status = 500;
    let gh = mock_github(state);

    let result = try_review_round(&config, &db, &gh, 7).await;

    assert!(result.is_err());
    let log = claim_log(&gh);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].0, "success");
    assert!(log[1].1.starts_with("released by test-host at "), "{log:?}");
}

#[tokio::test]
async fn a_github_error_on_the_claim_does_not_stop_the_review() {
    let (run, config, db, _dir) = claim_round_setup("claim-github-error").await;
    let mut state = state_with_claims(vec![]);
    state.commit_status_status = 403;
    let gh = mock_github(state);

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "completed");
    assert_eq!(run.reviews_run(), 1);
}

#[tokio::test]
async fn a_dry_run_sets_no_claim() {
    let (_run, config, db, _dir) = claim_round_setup("claim-dry-run").await;
    let gh = mock_github(state_with_claims(vec![]));
    let client = GitHubClient::new_with_base("test-token".into(), &gh.server.url_str("")).unwrap();
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone()).with_host("test-host");
    engine.init().await;
    let options = ReviewOptions {
        dry_run: true,
        ..ReviewOptions::default()
    };
    engine
        .review_pr(&config.repos[0], 7, options)
        .await
        .unwrap();
    assert!(gh.claims.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_read_back_that_shows_only_an_expired_peer_claim_keeps_the_claim() {
    let (run, config, db, _dir) = claim_round_setup("claim-read-back-lag").await;
    let mut state = state_with_claims(vec![]);
    state.rival_claim = Some(claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T06:00:00Z",
        20 * 60,
    ));
    let gh = mock_github(state);

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "completed");
    assert_eq!(run.reviews_run(), 1);
}

/// An already-reviewed head: the bot's review on `s1` is on GitHub.
fn reviewed_state_with_claims(claims: Vec<Value>) -> GitHubState {
    let mut state = state_with_claims(claims);
    state.reviews = vec![bot_review(1, "s1", "COMMENTED")];
    state
}

#[tokio::test]
async fn a_stale_claim_on_an_already_reviewed_head_is_released() {
    let (_run, config, db, _dir) = claim_round_setup("claim-stale-reviewed").await;
    let gh = mock_github(reviewed_state_with_claims(vec![claim_status(
        "pending",
        "claimed by dead-host at 2026-11-11T06:00:00Z",
        16 * 60,
    )]));

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "skipped:already-posted");
    let log = claim_log(&gh);
    assert_eq!(log.len(), 2, "{log:?}");
    // Automerge reads the latest status of the context: it is no longer pending.
    assert_eq!(log[1].0, "success");
    assert!(log[1].1.starts_with("released by test-host at "), "{log:?}");
}

#[tokio::test]
async fn a_fresh_peer_claim_on_an_already_reviewed_head_is_left_alone() {
    let (_run, config, db, _dir) = claim_round_setup("claim-fresh-reviewed").await;
    let gh = mock_github(reviewed_state_with_claims(vec![claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T07:00:00Z",
        60,
    )]));

    let result = review_round(&config, &db, &gh, 7).await;

    assert_eq!(result.status, "skipped:already-posted");
    assert_eq!(gh.claims.lock().unwrap().len(), 1, "the peer's claim stays");
}

async fn release_abandoned_on_s1(gh: &MockGitHub, include_own: bool) {
    let client = GitHubClient::new_with_base("test-token".into(), &gh.server.url_str("")).unwrap();
    crate::review::head_claim::release_abandoned(&client, "o", "r", "s1", "test-host", include_own)
        .await;
}

#[tokio::test]
async fn a_start_up_releases_the_pending_claim_that_this_host_left() {
    let gh = mock_github(state_with_claims(vec![claim_status(
        "pending",
        "claimed by test-host at 2026-11-11T07:09:00Z",
        60,
    )]));

    release_abandoned_on_s1(&gh, true).await;

    let log = claim_log(&gh);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1].0, "success");
    assert!(log[1].1.starts_with("released by test-host at "), "{log:?}");
}

#[tokio::test]
async fn a_start_up_leaves_a_fresh_claim_of_a_peer() {
    let gh = mock_github(state_with_claims(vec![claim_status(
        "pending",
        "claimed by other-host at 2026-11-11T07:09:00Z",
        60,
    )]));

    release_abandoned_on_s1(&gh, true).await;

    assert_eq!(claim_log(&gh).len(), 1, "the peer's claim stays");
}

#[tokio::test]
async fn a_running_daemon_leaves_its_own_fresh_claim() {
    let gh = mock_github(state_with_claims(vec![claim_status(
        "pending",
        "claimed by test-host at 2026-11-11T07:09:00Z",
        60,
    )]));

    release_abandoned_on_s1(&gh, false).await;

    assert_eq!(claim_log(&gh).len(), 1, "a live cycle keeps its claim");
}

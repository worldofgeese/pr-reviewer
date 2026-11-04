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
use crate::store::db::{Database, ReviewFindingUpsert};

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
    /// Root comment ids of the review threads a human resolved.
    resolved: Vec<u64>,
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
            resolved: vec![],
        }
    }
}

struct MockGitHub {
    server: Server,
    posted_reviews: Arc<Mutex<Vec<Value>>>,
    dismissed: Arc<Mutex<Vec<String>>>,
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
        .respond_with(status_code(200).body(state.diff.clone())),
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
    server.expect(
        Expectation::matching(request::method_path(
            "GET",
            format!("/repos/o/r/issues/{number}/comments"),
        ))
        .times(..)
        .respond_with(json_encoded(json!([]))),
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
    MockGitHub {
        server,
        posted_reviews,
        dismissed,
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
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone());
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
    let mut engine = ReviewEngine::new(config.clone(), client, db.clone());
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
        resolved: vec![],
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
        resolved: vec![],
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
        .contains("This is review round 2 of up to 2 rounds"));

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
        resolved: vec![],
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
        resolved: vec![],
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
        body.contains("review round 3 and inline findings stop after round 2"),
        "{body}"
    );
    assert!(body.contains(BOUND) && body.contains(README), "{body}");
    assert!(run
        .prompt(3)
        .contains("after the limit of 2 rounds with inline findings"));
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
        resolved: vec![],
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

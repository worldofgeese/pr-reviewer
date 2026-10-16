use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tempfile::tempdir;

use crate::config::{AppConfig, HarnessKind, ReasoningEffort, RepoConfig};
use crate::context::diff_parser::{parse_unified_diff, DiffSide};
use crate::context::gitnexus;
use crate::context::retriever::{assemble_context, ContextMode};
use crate::github::comments;
use crate::github::types::{
    CreateReviewComment, CreateReviewRequest, IssueComment, PullRequest, PullRequestReview,
    ReviewComment,
};
use crate::github::{client::GitHubClient, pr};
use crate::harness;
use crate::harness::spawn::{ensure_harness_available, run_harness, HarnessRunRequest};
use crate::review::instructions::{open_instruction_pr, recurring_finding_fingerprint};
use crate::review::parser::{
    parse_reply_output, parse_review_output, CommentSeverity, FindingKind, ParseOutcome,
    ReplyParseOutcome, ReviewComment as ParsedReviewComment, ReviewVerdict,
};
use crate::review::policy::{
    count_prior_bot_reviews, decide_disposition, final_verdict, follow_up_notes_section,
    normalize_text, same_finding, same_finding_near, stale_change_requests, token_set, CarriedOver,
    Disposition, FindingContext, FindingRef, FollowUpNote, ReviewRound, REVIEW_METADATA_MARKER,
};
use crate::review::prompt::build_review_prompt;
use crate::review::stack::{
    ancestor_with_same_finding, build_stack_context, change_fingerprint, resolve_stack,
    ChangeFingerprint,
};
use crate::safety::{evaluate_fork_policy, ForkDecision};
use crate::store::db::{
    dedupe_key, Database, ReviewAttemptRecord, ReviewClaim, ReviewFindingRecord,
    ReviewFindingUpsert,
};

const IN_PROGRESS_COMMENT_TIMEOUT_SECS: u64 = 4;
const IN_PROGRESS_COMMENT_MAX_CHARS: usize = 220;
const GITHUB_COMMENT_MAX_CHARS: usize = 65_536;
const GITHUB_COMMENT_TRUNCATION_NOTE: &str =
    "\n\n[truncated by pr-reviewer to fit GitHub's comment length limit]";
const SCREENSHOT_NOTE_TEXT: &str =
    "> **Note:** This PR touches UI files but no screenshots were referenced in the description. Consider adding visual previews for reviewers.";
const DURABLE_REVIEW_MARKER_PREFIX: &str = "<!-- pr-reviewer-review";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FindingStatus {
    Open,
    LikelyAddressed,
    /// A human resolved the thread on GitHub without a rebuttal reply.
    ResolvedByHuman,
    DismissedByHuman,
    RejectedWithRationale,
    OutOfScopeForPr,
}

#[derive(Debug, Clone)]
struct FindingRecord {
    key: String,
    path: String,
    line: Option<u32>,
    body: String,
    token_set: HashSet<String>,
    status: FindingStatus,
}

impl FindingRecord {
    fn as_ref(&self) -> FindingRef<'_> {
        FindingRef {
            path: &self.path,
            line: self.line,
            body: &self.body,
        }
    }
}

fn stored_finding_ref(stored: &ReviewFindingRecord) -> FindingRef<'_> {
    FindingRef {
        path: &stored.path,
        line: stored.line.and_then(|line| u32::try_from(line).ok()),
        body: &stored.body,
    }
}

#[derive(Debug, Clone, Default)]
struct ReviewMemory {
    findings: Vec<FindingRecord>,
    already_noted_screenshot: bool,
    has_prior_bot_review: bool,
    explicit_boundaries: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingFinding {
    fingerprint: String,
    path: String,
    line: Option<u32>,
    body: String,
    evidence_note: Option<String>,
    severity: CommentSeverity,
    finding_kind: FindingKind,
    kind: PendingKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PendingKind {
    /// Posted inline in this review.
    Inline,
    /// Re-reported while its thread is still open; not posted again.
    StillOpen,
    /// Reported as a non-blocking follow-up note in the review body.
    Note,
}

#[derive(Debug, Clone, Default)]
struct FindingDelta {
    newly_found: usize,
    still_blocking: usize,
    likely_fixed: usize,
    rebutted_or_scoped: usize,
}

impl FindingDelta {
    /// Whether anything changed since the last review. A finding that is only
    /// still blocking is not news: its thread and change request still stand.
    fn is_novel(&self) -> bool {
        self.newly_found + self.likely_fixed + self.rebutted_or_scoped > 0
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewOptions {
    pub dry_run: bool,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub force: bool,
    pub rerun_completed: bool,
}

#[derive(Debug, Clone)]
pub struct ReviewRunResult {
    pub repo: String,
    pub pr_number: u64,
    pub sha: String,
    pub status: String,
    pub verdict: Option<ReviewVerdict>,
    pub comments_posted: usize,
    /// Change fingerprint of the reviewed diff, recorded so the next push
    /// with the same change can be skipped. `None` when nothing was posted.
    pub fingerprint: Option<ChangeFingerprint>,
}

#[derive(Clone)]
pub struct ReviewEngine {
    config: Arc<AppConfig>,
    github: GitHubClient,
    db: Database,
    authenticated_user: Option<String>,
}

impl ReviewEngine {
    pub fn new(config: Arc<AppConfig>, github: GitHubClient, db: Database) -> Self {
        Self {
            config,
            github,
            db,
            authenticated_user: None,
        }
    }

    pub async fn init(&mut self) {
        self.authenticated_user = self.github.get_authenticated_user().await.ok();
    }

    pub fn authenticated_user(&self) -> Option<&str> {
        self.authenticated_user.as_deref()
    }

    pub async fn review_pr(
        &self,
        repo_cfg: &RepoConfig,
        pr_number: u64,
        options: ReviewOptions,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        let pr_data =
            pr::get_pull_request(&self.github, &repo_cfg.owner, &repo_cfg.name, pr_number)
                .await
                .with_context(|| format!("failed fetching PR {repo_name}#{pr_number}"))?;

        self.review_existing_pr(repo_cfg, &pr_data, options).await
    }

    pub async fn review_existing_pr(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        options: ReviewOptions,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        if pr_data.draft && !self.config.defaults.review_drafts {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:draft".to_string(),
                verdict: None,
                comments_posted: 0,
                fingerprint: None,
            });
        }

        let fork_decision = evaluate_fork_policy(repo_cfg, pr_data);
        if let ForkDecision::Skip(reason) = fork_decision {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: format!("skipped:{reason}"),
                verdict: None,
                comments_posted: 0,
                fingerprint: None,
            });
        }

        let harness_kind = options
            .harness
            .unwrap_or_else(|| repo_cfg.resolved_harness(&self.config));
        let model = options
            .model
            .clone()
            .or_else(|| repo_cfg.model.clone())
            .unwrap_or_else(|| self.config.harness.model.clone());
        let dry_run = options.dry_run || self.config.defaults.dry_run;
        let reasoning_effort = options
            .reasoning_effort
            .or(repo_cfg.reasoning_effort)
            .or(self.config.harness.reasoning_effort);
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
            dry_run,
        );

        let claim = ReviewClaim {
            dedupe_key: dedupe.clone(),
            repo: repo_name.clone(),
            pr_number: pr_data.number as i64,
            sha: pr_data.head.sha.clone(),
            harness: harness_kind.as_str().to_string(),
            model: Some(model.clone()),
        };

        if options.force {
            match self.db.delete_review_claim(&dedupe).await? {
                Some(ref status) if status == "claimed" => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "force flag deleted an in-flight 'claimed' entry — if the daemon is \
                         mid-pipeline for this PR, its complete_review/fail_review will silently \
                         no-op, leaving the new claim stuck until the next stale sweep"
                    );
                }
                Some(_) => {
                    tracing::info!(dedupe_key = %dedupe, "force flag cleared existing failed claim");
                }
                None => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "--force had no effect: no stale or failed claim found; \
                         if this PR+SHA was already successfully reviewed the completed \
                         entry is preserved and cannot be overridden with --force"
                    );
                }
            }
        }
        if options.rerun_completed {
            match self.db.delete_completed_review_claim(&dedupe).await? {
                Some(_) => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "rerun-completed deleted a completed review claim for this exact mode"
                    );
                }
                None => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "--rerun-completed had no effect: no completed claim found for this exact mode"
                    );
                }
            }
        }

        if !self.db.claim_review(claim).await? {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:duplicate".to_string(),
                verdict: None,
                comments_posted: 0,
                fingerprint: None,
            });
        }

        let started = Instant::now();
        let harness_impl = harness::for_kind(harness_kind);
        if let Err(err) = ensure_harness_available(harness_impl.as_ref()) {
            let duration = started.elapsed().as_secs_f64();
            let _ = self
                .db
                .fail_review(&dedupe, &format!("{err:#}"), duration)
                .await;
            return Err(err);
        }

        // Fetch existing reviews once — used for both in-progress guard and duplicate check
        // inside run_review_pipeline, avoiding a redundant API call.
        // The review list decides the review round and guards against posting
        // twice, so it must be known. A failed work item is not queued again
        // for the same head, so retry transient errors here before failing.
        let existing_reviews = match with_github_retries("list existing reviews", || {
            comments::get_existing_reviews(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
            )
        })
        .await
        {
            Ok(reviews) => reviews,
            Err(err) => {
                let duration = started.elapsed().as_secs_f64();
                let _ = self
                    .db
                    .fail_review(&dedupe, &format!("{err:#}"), duration)
                    .await;
                return Err(err);
            }
        };

        // Check if we already posted a review for this SHA before posting in-progress comment,
        // to avoid orphaned "started reviewing" comments when the already-posted guard fires.
        if !dry_run {
            let reviewer_owner = match &self.authenticated_user {
                Some(login) => login.clone(),
                None => self
                    .github
                    .get_authenticated_user()
                    .await
                    .unwrap_or_else(|_| self.config.defaults.bot_name.clone()),
            };

            let already_posted = existing_reviews.iter().any(|r| {
                r.commit_id.as_deref() == Some(pr_data.head.sha.as_str())
                    && login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        Some(reviewer_owner.as_str()),
                    )
            });

            if !already_posted {
                let has_prior_bot_review = existing_reviews.iter().any(|r| {
                    login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        Some(reviewer_owner.as_str()),
                    )
                });

                // Only post in-progress comment on first touch. Follow-up rounds
                // skip it to reduce noise.
                if !has_prior_bot_review {
                    let in_progress = self
                        .compose_in_progress_comment(
                            harness_kind,
                            &model,
                            reasoning_effort,
                            &pr_data.user.login,
                            &pr_data.title,
                            &pr_data.head.sha,
                            false,
                        )
                        .await;
                    let durable_body = build_durable_in_progress_comment(pr_data, &in_progress);
                    if let Err(err) = self
                        .sync_durable_review_comment(repo_cfg, pr_data, &durable_body, true)
                        .await
                    {
                        tracing::warn!(
                            repo = %repo_cfg.full_name(),
                            pr = pr_data.number,
                            error = %err,
                            "failed to sync durable in-progress review comment"
                        );
                    }
                }
            }
        }

        let outcome = self
            .run_review_pipeline(
                repo_cfg,
                pr_data,
                harness_kind,
                &model,
                reasoning_effort,
                dry_run,
                options.force || options.rerun_completed,
                existing_reviews,
            )
            .await;

        match outcome {
            Ok(mut result) => {
                self.db
                    .upsert_pr_state(
                        &repo_name,
                        pr_data.number as i64,
                        Some(&pr_data.head.sha),
                        None,
                    )
                    .await?;
                // A reviewed head without a fingerprint (a non-text file in
                // the diff) clears the old one, so it is never compared
                // against a review the ledger has moved past.
                let fingerprint_json = result
                    .fingerprint
                    .as_ref()
                    .map(|fingerprint| serde_json::to_string(&fingerprint.files))
                    .transpose()?;
                if result.fingerprint.is_some() || result.status == "completed" {
                    self.db
                        .set_last_reviewed_fingerprint(
                            &repo_name,
                            pr_data.number as i64,
                            result.fingerprint.as_ref().map(|fp| fp.overall.as_str()),
                            fingerprint_json.as_deref(),
                        )
                        .await?;
                }
                result.sha = pr_data.head.sha.clone();
                Ok(result)
            }
            Err(err) => {
                let duration = started.elapsed().as_secs_f64();
                let _ = self
                    .db
                    .fail_review(&dedupe, &format!("{err:#}"), duration)
                    .await;
                Err(err)
            }
        }
    }

    async fn sync_durable_review_comment(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        body: &str,
        create_if_missing: bool,
    ) -> Result<()> {
        let issue_comments = comments::get_issue_comments(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();
        if let Some(existing) = find_durable_review_comment(
            &issue_comments,
            pr_data.number,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
        ) {
            comments::update_issue_comment(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                existing.id,
                body,
            )
            .await?;
        } else if create_if_missing {
            comments::create_issue_comment(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                body,
            )
            .await?;
        }
        Ok(())
    }

    async fn compose_in_progress_comment(
        &self,
        harness_kind: HarnessKind,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        pr_author: &str,
        pr_title: &str,
        sha: &str,
        has_prior_bot_review: bool,
    ) -> String {
        let fallback =
            build_in_progress_comment_fallback(pr_author, pr_title, sha, has_prior_bot_review);

        let prompt =
            build_in_progress_comment_prompt(pr_author, pr_title, sha, has_prior_bot_review);

        let temp = match tempdir() {
            Ok(dir) => dir,
            Err(err) => {
                tracing::warn!(error = %err, "failed to create temp dir for in-progress comment prompt");
                return fallback;
            }
        };

        let harness_impl = harness::for_kind(harness_kind);
        let output = match run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model: model.to_string(),
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: IN_PROGRESS_COMMENT_TIMEOUT_SECS,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    harness = harness_kind.as_str(),
                    error = %err,
                    "in-progress comment generation failed; using fallback"
                );
                return fallback;
            }
        };

        let Some(raw) = extract_in_progress_comment(&output.stdout, &output.stderr) else {
            return fallback;
        };
        let Some(mut composed) = normalize_in_progress_comment(&raw) else {
            return fallback;
        };

        if has_prior_bot_review && looks_like_reintroduction(&composed) {
            return fallback;
        }

        if !has_prior_bot_review && !composed.contains("[pr-reviewer](") {
            let suffix = format!(" Powered by [pr-reviewer]({}).", pr_reviewer_project_url());
            if composed.len() + suffix.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
                return fallback;
            }
            composed.push_str(&suffix);
        }

        if composed.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
            return fallback;
        }

        composed
    }

    async fn run_review_pipeline(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        harness_kind: HarnessKind,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        dry_run: bool,
        force: bool,
        existing_reviews: Vec<PullRequestReview>,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
            dry_run,
        );

        let policy = repo_cfg.resolved_review_policy(&self.config);
        let prior_state = self
            .db
            .get_pr_state(&repo_name, pr_data.number as i64)
            .await?;
        let prior_diff_hash = prior_state
            .as_ref()
            .and_then(|s| s.last_reviewed_diff_hash.clone());
        let prior_file_hashes: Option<BTreeMap<String, String>> = prior_state
            .as_ref()
            .and_then(|s| s.last_reviewed_file_hashes.as_deref())
            .and_then(|json| serde_json::from_str(json).ok());
        let prior_sha = prior_state
            .and_then(|s| s.last_reviewed_sha)
            .filter(|sha| sha != &pr_data.head.sha);

        let diff = pr::get_diff(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await?;
        let parsed_diff = parse_unified_diff(&diff).context("failed parsing diff")?;

        let changed_files: Vec<String> = parsed_diff
            .files
            .iter()
            .map(|f| {
                if f.new_path != "/dev/null" {
                    f.new_path.clone()
                } else {
                    f.old_path.clone()
                }
            })
            .collect();

        // A push that leaves the PR's own change as it was (a rebase, a
        // cascade from a stacked parent, a retarget after the parent merged)
        // needs no new review: the earlier findings still apply as they are.
        let fingerprint = change_fingerprint(&parsed_diff);
        if policy.skip_unchanged_diff
            && !force
            && prior_sha.is_some()
            && prior_diff_hash.is_some()
            && fingerprint.as_ref().map(|fp| fp.overall.as_str()) == prior_diff_hash.as_deref()
        {
            tracing::info!(
                repo = %repo_name,
                pr = pr_data.number,
                previous_sha = prior_sha.as_deref().unwrap_or_default(),
                "skipping review: the PR's own change is unchanged since the last review"
            );
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some("skipped:unchanged-diff"),
                    0.0,
                    0,
                    parsed_diff.total_hunk_lines as i64,
                    None,
                    None,
                    None,
                )
                .await?;
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:unchanged-diff".to_string(),
                verdict: None,
                comments_posted: 0,
                fingerprint,
            });
        }

        // Skip docs-only or trivial diffs to avoid wasting compute
        if let Some(skip_reason) = should_skip_review(&changed_files, &self.config.defaults) {
            tracing::info!(
                repo = %repo_name,
                pr = pr_data.number,
                reason = skip_reason,
                "skipping review: {skip_reason}",
            );
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(&format!("skipped:{skip_reason}")),
                    0.0,
                    0,
                    parsed_diff.total_hunk_lines as i64,
                    None,
                    None,
                    None,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: format!("skipped:{skip_reason}"),
                verdict: None,
                comments_posted: 0,
                fingerprint: None,
            });
        }

        let mut gitnexus_used = if repo_cfg.gitnexus { Some(false) } else { None };
        let mut gitnexus_latency_ms: Option<i64> = None;
        let mut gitnexus_hit_count: Option<i64> = None;

        let gitnexus_context = if repo_cfg.gitnexus {
            match repo_cfg.effective_local_path() {
                Ok(local) => {
                    // Fetch latest for managed clones so GitNexus index is fresh
                    if repo_cfg.is_managed() {
                        if let Err(err) =
                            crate::repo_manager::fetch_latest_managed(&local, self.github.token())
                                .await
                        {
                            tracing::warn!(
                                repo = %repo_cfg.full_name(),
                                error = %err,
                                "failed to fetch latest for managed clone; using stale state"
                            );
                        }
                    }
                    match gitnexus::is_index_stale(&local).await {
                        Ok(Some(true)) => {
                            tracing::warn!(
                                repo = %repo_cfg.full_name(),
                                "gitnexus index is stale; run `pr-reviewer index {}` to refresh",
                                repo_cfg.full_name()
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::debug!(
                                repo = %repo_cfg.full_name(),
                                error = %err,
                                "failed to determine gitnexus index freshness"
                            );
                        }
                    }

                    match gitnexus::query_context_with_metrics(
                        &local,
                        repo_cfg.name.as_str(),
                        &changed_files,
                    )
                    .await
                    {
                        Ok(result) => {
                            gitnexus_used = Some(result.used);
                            if result.used {
                                gitnexus_latency_ms = Some(result.latency_ms);
                                gitnexus_hit_count = Some(result.hit_count);
                            }
                            result.text
                        }
                        Err(_) => None,
                    }
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let context_mode = match evaluate_fork_policy(repo_cfg, pr_data) {
            ForkDecision::Limited => ContextMode::Limited,
            _ => ContextMode::Full,
        };

        let assembled = assemble_context(
            &self.github,
            repo_cfg,
            pr_data,
            &diff,
            &parsed_diff,
            &self.config.defaults,
            context_mode,
            gitnexus_context.as_deref(),
        )
        .await?;

        // Fetch inline review comments, PR issue comments, and repo conventions in parallel
        let (review_comments, issue_comments, repo_conventions) = tokio::join!(
            async {
                comments::get_review_comments(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    None,
                )
                .await
                .unwrap_or_default()
            },
            async {
                comments::get_issue_comments(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                )
                .await
                .unwrap_or_default()
            },
            crate::context::retriever::fetch_repo_conventions(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                &pr_data.head.sha,
            ),
        );

        let bot_name = self.config.defaults.bot_name.as_str();
        // The account the bot posts as. Needed to count review rounds; without
        // it every review would look like round 1 and the limit never applies.
        let bot_alias_owned = match self.authenticated_user.clone() {
            Some(login) => login,
            None => with_github_retries("identify the bot account", || {
                self.github.get_authenticated_user()
            })
            .await
            .context("cannot identify the bot account, so review rounds cannot be counted")?,
        };
        let bot_alias = Some(bot_alias_owned.as_str());
        let resolved_threads = if review_comments
            .iter()
            .any(|comment| login_matches_bot(&comment.user.login, bot_name, bot_alias))
        {
            self.github
                .get_review_thread_resolution(&repo_cfg.owner, &repo_cfg.name, pr_data.number)
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        repo = %repo_name,
                        pr = pr_data.number,
                        error = %err,
                        "failed to read review thread resolution; treating threads as open"
                    );
                    HashMap::new()
                })
        } else {
            HashMap::new()
        };

        let mut context_with_history = assembled.text.clone();
        let stored_findings = self
            .db
            .list_review_findings(&repo_name, pr_data.number as i64)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to load review finding ledger; continuing with GitHub thread memory only"
                );
                vec![]
            });
        let review_memory = build_review_memory(
            &existing_reviews,
            &review_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            bot_alias,
            pr_data.head.sha.as_str(),
            Some(&changed_files),
            &resolved_threads,
        );
        let prior_reviews_context = build_prior_reviews_context(
            &existing_reviews,
            &review_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            bot_alias,
            pr_data.head.sha.as_str(),
        );
        if !prior_reviews_context.is_empty() {
            context_with_history.push_str("\n## Prior Review History\n");
            context_with_history.push_str(&prior_reviews_context);
            context_with_history.push('\n');
        }

        let addressed_findings = build_addressed_findings(
            &review_comments,
            &parsed_diff,
            self.config.defaults.bot_name.as_str(),
            bot_alias,
            &resolved_threads,
        );
        if !addressed_findings.is_empty() {
            context_with_history.push_str("\n## Previously Flagged Items\n");
            context_with_history.push_str(&addressed_findings);
            context_with_history.push('\n');
        }
        let review_state_context = build_review_state_context(&review_memory);
        if !review_state_context.is_empty() {
            context_with_history.push_str("\n## Review State\n");
            context_with_history.push_str(&review_state_context);
            context_with_history.push('\n');
        }
        let finding_ledger_context = build_stored_findings_context(&stored_findings);
        if !finding_ledger_context.is_empty() {
            context_with_history.push_str("\n## Persisted Finding Ledger\n");
            context_with_history.push_str(&finding_ledger_context);
            context_with_history.push('\n');
        }

        let mut changed_since_last: Option<HashSet<String>> = None;
        if let Some(previous_sha) = prior_sha.as_deref() {
            if let Ok(delta_diff) = self
                .github
                .get_compare_diff(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    previous_sha,
                    &pr_data.head.sha,
                )
                .await
            {
                context_with_history.push_str(&format!(
                    "\n## Incremental Diff Since Last Reviewed SHA ({previous_sha} -> {})\n```diff\n",
                    pr_data.head.sha
                ));
                context_with_history.push_str(&truncate_lines(&delta_diff, 400));
                context_with_history.push_str("\n```\n");
                changed_since_last = parse_unified_diff(&delta_diff).ok().map(|delta| {
                    delta
                        .files
                        .iter()
                        .flat_map(|file| [file.old_path.clone(), file.new_path.clone()])
                        .collect()
                });
            }
        }
        // The PR's own change per file is exact; the compare diff above also
        // lists files a rebase brought in from the base. Prefer the former.
        if let (Some(current), Some(previous)) = (fingerprint.as_ref(), prior_file_hashes.as_ref())
        {
            changed_since_last = Some(current.changed_files(previous));
        }

        let context_original_bytes = context_with_history.len();
        if context_with_history.len() > self.config.defaults.max_prompt_bytes {
            let max_bytes = self.config.defaults.max_prompt_bytes;
            let note = "\n\n[context truncated before prompt build due size budget]\n";
            if max_bytes > note.len() {
                truncate_utf8_to_max_bytes(&mut context_with_history, max_bytes - note.len());
                context_with_history.push_str(note);
            } else {
                truncate_utf8_to_max_bytes(&mut context_with_history, max_bytes);
            }
        }

        let has_prior_reviews = review_memory.has_prior_bot_review
            || !prior_reviews_context.is_empty()
            || !stored_findings.is_empty();

        // Detect UI file changes and check for screenshots in PR body
        let ui_files = detect_ui_files(&changed_files);
        let ui_files_for_prompt =
            if !ui_files.is_empty() && !has_screenshot_references(pr_data.body.as_deref()) {
                Some(ui_files)
            } else {
                None
            };

        let round = ReviewRound::after_prior_reviews(
            count_prior_bot_reviews(&existing_reviews, bot_name, bot_alias, &pr_data.head.sha),
            policy.max_review_cycles,
        );
        let stack_info = if policy.stack_aware {
            resolve_stack(&self.github, &repo_cfg.owner, &repo_cfg.name, pr_data)
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        repo = %repo_name,
                        pr = pr_data.number,
                        error = %err,
                        "failed to resolve PR stack; reviewing it as a standalone PR"
                    );
                    None
                })
        } else {
            None
        };
        let mut ancestor_findings: Vec<ReviewFindingRecord> = Vec::new();
        for ancestor in stack_info.iter().flat_map(|info| info.ancestors.iter()) {
            match self
                .db
                .list_review_findings(&repo_name, ancestor.number as i64)
                .await
            {
                Ok(findings) => ancestor_findings.extend(
                    findings
                        .into_iter()
                        .filter(|finding| finding_status_is_active(&finding.status)),
                ),
                Err(err) => tracing::warn!(
                    repo = %repo_name,
                    pr = ancestor.number,
                    error = %err,
                    "failed to load ancestor findings"
                ),
            }
        }
        let stack_context = stack_info
            .as_ref()
            .map(|info| build_stack_context(pr_data, info, &ancestor_findings));

        let prompt = build_review_prompt(
            repo_cfg,
            pr_data,
            &context_with_history,
            &self.config.defaults.bot_name,
            repo_conventions.as_deref(),
            has_prior_reviews,
            ui_files_for_prompt.as_deref(),
            round,
            &policy,
            stack_context.as_deref(),
        );

        tracing::info!(
            repo = %repo_cfg.full_name(),
            pr = pr_data.number,
            context_bytes_before = context_original_bytes,
            context_bytes_after = context_with_history.len(),
            prompt_bytes = prompt.len(),
            changed_files_included = assembled.files_included,
            related_files_included = assembled.related_files_included,
            assembled_context_bytes = assembled.bytes_total,
            assembled_context_truncated = assembled.truncated,
            "built review prompt"
        );

        let temp = tempdir().context("failed to create temp working dir")?;
        let prompt_path = temp.path().join("review_prompt.md");
        tokio::fs::write(&prompt_path, &prompt)
            .await
            .context("failed to write prompt file")?;

        let harness_impl = harness::for_kind(harness_kind);
        let harness_output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model: model.to_string(),
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let parse_outcome = parse_review_output(&harness_output.stdout, &harness_output.stderr)?;

        let mut body = String::new();
        let mut verdict = ReviewVerdict::Comment;
        let mut inline_comments: Vec<CreateReviewComment> = Vec::new();
        let mut unmapped_comments: Vec<String> = Vec::new();
        let mut pending_findings: Vec<PendingFinding> = Vec::new();
        let mut follow_up_notes: Vec<FollowUpNote> = Vec::new();
        let mut carried_over = CarriedOver::default();
        let has_structured_findings: bool;

        // Keep provenance available without putting review mechanics above the finding.
        body.push_str(&build_review_metadata_block(
            model,
            &short_sha(&pr_data.head.sha),
        ));

        match parse_outcome {
            ParseOutcome::Structured(review) => {
                body.push_str(&review.summary);
                verdict = review.verdict;
                let confidence_reasons = review
                    .confidence
                    .reasons
                    .iter()
                    .map(|reason| reason.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(
                    "\n\n**Confidence:** {} [{}] - {}\n",
                    review.confidence.level, confidence_reasons, review.confidence.justification,
                ));

                if review.ui_screenshot_needed && !review_memory.already_noted_screenshot {
                    body.push('\n');
                    body.push_str(SCREENSHOT_NOTE_TEXT);
                    body.push('\n');
                }

                let mut seen_keys = HashSet::new();
                let mut surviving_comments: Vec<ParsedReviewComment> = Vec::new();
                for mut comment in review.comments {
                    let path = clean_comment_path(&comment.file);
                    let key = finding_key(&path, Some(comment.line), &comment.body);
                    if !seen_keys.insert(key) {
                        continue;
                    }
                    if should_suppress_finding(&comment, &path, &review_memory) {
                        continue;
                    }
                    apply_scope_drift_policy(&mut comment);
                    if comment.severity == CommentSeverity::Blocking
                        && !blocker_has_sufficient_evidence(
                            &comment,
                            &path,
                            review.confidence.level,
                        )
                    {
                        comment.severity = CommentSeverity::Warning;
                    }
                    surviving_comments.push(comment);
                }

                let mut dispositions = Vec::new();
                for comment in surviving_comments {
                    let path = clean_comment_path(&comment.file);
                    let candidate = FindingRef {
                        path: &path,
                        line: Some(comment.line),
                        body: &comment.body,
                    };
                    // Matches that hide a new finding completely must be near
                    // the earlier one, so a similar bug elsewhere is still posted.
                    let thread_with = |status: FindingStatus| {
                        review_memory.findings.iter().any(|prior| {
                            prior.status == status && same_finding_near(prior.as_ref(), candidate)
                        })
                    };
                    let on_changed_line = parsed_diff.is_changed_line(&path, comment.line);
                    let context = FindingContext {
                        open_thread: thread_with(FindingStatus::Open),
                        resolved_thread: thread_with(FindingStatus::ResolvedByHuman),
                        already_noted: stored_findings.iter().any(|stored| {
                            stored.status == "noted"
                                && same_finding_near(stored_finding_ref(stored), candidate)
                        }),
                        // A finding on a line this PR changes is about this PR's
                        // code, even when a lower PR has a similar one.
                        tracked_on_ancestor: if on_changed_line {
                            None
                        } else {
                            ancestor_with_same_finding(candidate, &ancestor_findings)
                        },
                        file_changed_since_last: changed_since_last
                            .as_ref()
                            .is_none_or(|changed| changed.contains(&path)),
                        on_changed_line,
                    };
                    let disposition = decide_disposition(&comment, round, &policy, context);
                    dispositions.push((comment.severity, disposition));
                    carried_over.record(disposition);
                    let pending = PendingFinding {
                        fingerprint: finding_key(&path, Some(comment.line), &comment.body),
                        path: path.clone(),
                        line: Some(comment.line),
                        body: comment.body.clone(),
                        evidence_note: comment.evidence_note.clone(),
                        severity: comment.severity,
                        finding_kind: comment.finding_kind,
                        kind: match disposition {
                            Disposition::StillOpen => PendingKind::StillOpen,
                            Disposition::FollowUpNote => PendingKind::Note,
                            _ => PendingKind::Inline,
                        },
                    };
                    match disposition {
                        Disposition::Inline => pending_findings.push(pending),
                        Disposition::StillOpen => {
                            // Keep it active in the ledger; its thread already shows it.
                            pending_findings.push(pending);
                            continue;
                        }
                        Disposition::FollowUpNote => {
                            follow_up_notes.push(FollowUpNote {
                                path: path.clone(),
                                line: comment.line,
                                body: comment.body.clone(),
                                severity: comment.severity,
                                finding_kind: comment.finding_kind,
                            });
                            pending_findings.push(pending);
                            continue;
                        }
                        Disposition::Resolved
                        | Disposition::TrackedOnAncestor(_)
                        | Disposition::AlreadyNoted => continue,
                    }
                    let right = parsed_diff.position_for(&path, comment.line, DiffSide::Right);
                    let left = parsed_diff.position_for(&path, comment.line, DiffSide::Left);

                    if let Some(position) = right.or(left) {
                        inline_comments.push(CreateReviewComment {
                            path: path.clone(),
                            line: comment.line,
                            side: match position.side {
                                DiffSide::Left => "LEFT".to_string(),
                                DiffSide::Right => "RIGHT".to_string(),
                            },
                            body: comment.body,
                        });
                    } else {
                        unmapped_comments
                            .push(format!("- {}:{} - {}", path, comment.line, comment.body));
                    }
                }
                verdict = final_verdict(verdict, &dispositions);
                has_structured_findings = !inline_comments.is_empty()
                    || !unmapped_comments.is_empty()
                    || !follow_up_notes.is_empty();
            }
            ParseOutcome::RawSummary(raw) => {
                if looks_like_harness_transport_output(&raw) {
                    return Err(anyhow!(
                        "harness returned machine transport output instead of a review body"
                    ));
                }
                has_structured_findings = true;
                body.push_str("Harness output could not be parsed as structured JSON.\n\n");
                body.push_str(&raw);
            }
            ParseOutcome::Empty => {
                return Err(anyhow!("harness returned empty output"));
            }
        }

        if !unmapped_comments.is_empty() {
            body.push_str("\n\nUnmapped findings (not on changed lines):\n");
            for line in &unmapped_comments {
                body.push_str(&line);
                body.push('\n');
            }
        }
        if let Some(notes) = follow_up_notes_section(round, &follow_up_notes) {
            body.push_str("\n\n");
            body.push_str(&notes);
        }
        if let Some(carried) = carried_over.section() {
            body.push_str("\n\n");
            body.push_str(&carried);
            body.push('\n');
        }

        let (finding_updates, finding_delta, final_fingerprints) = reconcile_finding_ledger(
            &repo_name,
            pr_data.number as i64,
            &pr_data.head.sha,
            &pr_data.user.login,
            &stored_findings,
            &pending_findings,
            &review_memory,
            &changed_files.iter().cloned().collect(),
            changed_since_last.as_ref(),
        );
        let finding_delta_summary = if stored_findings.is_empty() {
            None
        } else {
            build_finding_delta_summary(&finding_delta)
        };
        if let Some(delta_summary) = finding_delta_summary.as_deref() {
            body.push_str("\n\n");
            body.push_str(delta_summary);
            body.push('\n');
        }

        let body = truncate_github_comment_body(&body);

        if dry_run {
            tracing::info!(
                repo = %repo_name,
                pr = pr_data.number,
                round = round.number,
                verdict = verdict_label(verdict),
                inline = ?inline_comments
                    .iter()
                    .map(|c| format!("{}:{} {}", c.path, c.line, c.body))
                    .collect::<Vec<_>>(),
                "dry run, review not posted:\n{body}"
            );
            self.db
                .complete_review(
                    &dedupe,
                    inline_comments.len() as i64,
                    Some(verdict_label(verdict)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "completed:dry-run".to_string(),
                verdict: Some(verdict),
                comments_posted: inline_comments.len(),
                fingerprint: None,
            });
        }

        let screenshot_note_added =
            body.contains(SCREENSHOT_NOTE_TEXT) && !review_memory.already_noted_screenshot;
        if review_memory.has_prior_bot_review
            && inline_comments.is_empty()
            && unmapped_comments.is_empty()
            && !screenshot_note_added
            && !finding_delta.is_novel()
            && !has_structured_findings
        {
            // Nothing new: findings still open stay in their threads, and an
            // earlier change request stays in force while one of them blocks.
            if policy.dismiss_stale_change_requests {
                let would_post = if verdict == ReviewVerdict::RequestChanges {
                    "REQUEST_CHANGES"
                } else {
                    "COMMENT"
                };
                self.dismiss_stale_change_requests(
                    repo_cfg,
                    pr_data,
                    &existing_reviews,
                    bot_alias,
                    &review_memory,
                    would_post,
                )
                .await;
            }
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(verdict_label(ReviewVerdict::Comment)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:no-novel-findings".to_string(),
                verdict: Some(ReviewVerdict::Comment),
                comments_posted: 0,
                fingerprint: fingerprint.clone(),
            });
        }

        if !force
            && existing_reviews.iter().any(|r| {
                r.commit_id.as_deref() == Some(pr_data.head.sha.as_str())
                    && login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        bot_alias,
                    )
            })
        {
            tracing::warn!(
                repo = %repo_name,
                pr = pr_data.number,
                "a review of this head is already on GitHub; its findings are not in the ledger"
            );
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(verdict_label(ReviewVerdict::Comment)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:already-posted".to_string(),
                verdict: Some(ReviewVerdict::Comment),
                comments_posted: 0,
                fingerprint: fingerprint.clone(),
            });
        }

        // Bot never approves. REQUEST_CHANGES still needs self-review downgrade
        // because GitHub rejects it on your own PRs.
        let event = match verdict {
            ReviewVerdict::NoIssues | ReviewVerdict::Approve | ReviewVerdict::Comment => "COMMENT",
            ReviewVerdict::RequestChanges => {
                let bot_login = match &self.authenticated_user {
                    Some(u) => Some(u.clone()),
                    None => self.github.get_authenticated_user().await.ok(),
                };
                match bot_login.as_deref() {
                    Some(u) if u.eq_ignore_ascii_case(&pr_data.user.login) => "COMMENT",
                    Some(_) => "REQUEST_CHANGES",
                    None => {
                        tracing::warn!("could not determine authenticated user; downgrading REQUEST_CHANGES to COMMENT");
                        "COMMENT"
                    }
                }
            }
        };

        let request = CreateReviewRequest {
            body: body.clone(),
            event: event.to_string(),
            comments: inline_comments.clone(),
        };

        let post_result = comments::create_review(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            &request,
        )
        .await;

        // If review post failed (e.g. 422 on self-review), retry as COMMENT without inline comments.
        // Append the inline comments to the body so they're not silently lost.
        let mut used_retry = false;
        let post_result = if let Err(ref first_err) = post_result {
            let err_str = format!("{first_err}");
            if err_str.contains("(422") || err_str.contains("Can not approve") {
                tracing::warn!(
                    repo = %repo_cfg.full_name(),
                    pr = pr_data.number,
                    error = %first_err,
                    "review post failed with 422; retrying as COMMENT with inline comments folded into body"
                );
                let retry_request = CreateReviewRequest {
                    body: build_retry_review_body(&body, &inline_comments),
                    event: "COMMENT".to_string(),
                    comments: vec![],
                };
                used_retry = true;
                comments::create_review(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    &retry_request,
                )
                .await
            } else {
                post_result
            }
        } else {
            post_result
        };

        let findings_visible_to_github = post_result.is_ok();
        let post_error = post_result.err();

        // When 422 retry posted comments in the body instead of inline, record 0 inline comments.
        // Also store the actual posted event (which may differ from the LLM verdict after
        // self-review downgrade or 422 retry).
        let posted_inline = if used_retry {
            0
        } else {
            inline_comments.len() as i64
        };
        let actual_event = if used_retry { "COMMENT" } else { event };
        let has_blocking_findings = pending_findings
            .iter()
            .any(|finding| finding.severity == CommentSeverity::Blocking);
        let durable_body = if let Some(err) = post_error.as_ref() {
            let fallback = format!("Review post failed.\n\nError: {err}\n\n{body}");
            build_durable_review_comment(
                pr_data,
                model,
                verdict,
                actual_event,
                posted_inline,
                false,
                has_blocking_findings,
                &fallback,
            )
        } else {
            build_durable_review_status_comment(
                pr_data,
                model,
                verdict,
                actual_event,
                posted_inline,
            )
        };
        let create_durable_comment = !findings_visible_to_github;
        if let Err(err) = self
            .sync_durable_review_comment(repo_cfg, pr_data, &durable_body, create_durable_comment)
            .await
        {
            tracing::warn!(
                repo = %repo_cfg.full_name(),
                pr = pr_data.number,
                error = %err,
                "failed to sync durable completed review comment"
            );
        }

        if findings_visible_to_github {
            // The ledger records only what reached GitHub.
            if !finding_updates.is_empty() {
                self.db.upsert_review_findings(finding_updates).await?;
            }
            self.db
                .mark_review_findings_posted(
                    &repo_name,
                    pr_data.number as i64,
                    &pr_data.head.sha,
                    pending_findings
                        .iter()
                        .zip(&final_fingerprints)
                        .filter(|(finding, _)| finding.kind != PendingKind::StillOpen)
                        .map(|(_, fingerprint)| fingerprint.clone())
                        .collect(),
                )
                .await?;
            if policy.dismiss_stale_change_requests {
                self.dismiss_stale_change_requests(
                    repo_cfg,
                    pr_data,
                    &existing_reviews,
                    bot_alias,
                    &review_memory,
                    actual_event,
                )
                .await;
            }
        }

        self.db
            .complete_review(
                &dedupe,
                posted_inline,
                Some(actual_event),
                harness_output.duration_secs,
                assembled.files_included as i64,
                assembled.diff_lines as i64,
                gitnexus_used,
                gitnexus_latency_ms,
                gitnexus_hit_count,
            )
            .await?;

        if findings_visible_to_github {
            if let Err(err) = self
                .handle_recurring_instruction_prs(repo_cfg, pr_data, &pending_findings)
                .await
            {
                tracing::warn!(
                    repo = %repo_cfg.full_name(),
                    pr = pr_data.number,
                    error = %err,
                    "failed to process recurring finding instruction PR suggestions"
                );
            }
        }

        Ok(ReviewRunResult {
            repo: repo_name,
            pr_number: pr_data.number,
            sha: pr_data.head.sha.clone(),
            status: "completed".to_string(),
            verdict: Some(verdict),
            comments_posted: if used_retry { 0 } else { inline_comments.len() },
            // Record the change as reviewed only once the review reached
            // GitHub; otherwise the next push with the same change re-reviews.
            fingerprint: if findings_visible_to_github {
                fingerprint
            } else {
                None
            },
        })
    }

    /// Dismiss the bot's earlier REQUEST_CHANGES reviews when this review does
    /// not request changes and no blocking finding is open. The bot never
    /// approves, so an earlier change request would otherwise keep blocking
    /// the PR after its findings were fixed.
    async fn dismiss_stale_change_requests(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        existing_reviews: &[PullRequestReview],
        bot_alias: Option<&str>,
        review_memory: &ReviewMemory,
        posted_event: &str,
    ) {
        let repo_name = repo_cfg.full_name();
        let blocking_open = match self
            .db
            .list_review_findings(&repo_name, pr_data.number as i64)
            .await
        {
            // An empty ledger means the findings behind an earlier change
            // request are unknown (older version, reset database): keep it.
            Ok(findings) if findings.is_empty() => return,
            // An open bot thread with no ledger row (posted by an older
            // version, say) may be a blocker: keep the change request.
            Ok(findings)
                if review_memory
                    .findings
                    .iter()
                    .filter(|thread| thread.status == FindingStatus::Open)
                    .any(|thread| {
                        !findings.iter().any(|stored| {
                            same_finding_near(stored_finding_ref(stored), thread.as_ref())
                        })
                    }) =>
            {
                return
            }
            Ok(findings) => findings.iter().any(|finding| {
                finding_status_is_active(&finding.status) && finding.severity == "blocking"
            }),
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to read finding ledger; keeping earlier change requests"
                );
                return;
            }
        };
        let message = format!(
            "pr-reviewer: no blocking finding is open as of {}.",
            short_sha(&pr_data.head.sha)
        );
        for review in stale_change_requests(
            existing_reviews,
            self.config.defaults.bot_name.as_str(),
            bot_alias,
            posted_event,
            blocking_open,
        ) {
            match self
                .github
                .dismiss_review(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    review.id,
                    &message,
                )
                .await
            {
                Ok(()) => tracing::info!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    review = review.id,
                    "dismissed stale change request"
                ),
                Err(err) => tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    review = review.id,
                    error = %err,
                    "failed to dismiss stale change request"
                ),
            }
        }
    }

    pub async fn finalize_closed_pr_review(
        &self,
        repo_cfg: &RepoConfig,
        pr_number: u64,
        reviewed_sha: &str,
    ) -> Result<bool> {
        let pr_data =
            pr::get_pull_request(&self.github, &repo_cfg.owner, &repo_cfg.name, pr_number)
                .await
                .with_context(|| {
                    format!(
                        "failed fetching PR {}#{pr_number} for final archive",
                        repo_cfg.full_name()
                    )
                })?;

        // A PR is terminal when state != "open" OR merged_at is set.  The
        // `state` field defaults to "open" when absent from the payload (see
        // #[serde(default = "default_pr_state")]).  Checking merged_at as a
        // secondary signal ensures that a PR with a missing state field but a
        // populated merged_at (e.g. a partial payload) is still finalized rather
        // than silently skipped.
        if pr_data.state.eq_ignore_ascii_case("open") && pr_data.merged_at.is_none() {
            return Ok(false);
        }

        self.archive_closed_pr_review(repo_cfg, &pr_data, reviewed_sha)
            .await?;
        Ok(true)
    }

    async fn archive_closed_pr_review(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        reviewed_sha: &str,
    ) -> Result<()> {
        let repo_name = repo_cfg.full_name();
        let terminal_state = terminal_pr_state(pr_data);
        let review_attempts = self
            .db
            .list_pr_review_attempts(&repo_name, pr_data.number as i64)
            .await?;
        let finding_ledger = self
            .db
            .list_review_findings(&repo_name, pr_data.number as i64)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch finding ledger for final archive"
                );
                vec![]
            });

        let (existing_reviews, review_comments, issue_comments) = tokio::join!(
            comments::get_existing_reviews(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
            ),
            comments::get_review_comments(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                None,
            ),
            comments::get_issue_comments(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
            ),
        );

        // Log warnings for API failures so the transcript honestly reflects
        // missing data instead of silently treating errors as "no content".
        let existing_reviews = match existing_reviews {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch reviews for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };
        let review_comments = match review_comments {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch review comments for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };
        let issue_comments = match issue_comments {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch issue comments for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };

        let transcript = build_final_review_transcript(
            &repo_name,
            pr_data,
            terminal_state,
            &review_attempts,
            &existing_reviews,
            &review_comments,
            &issue_comments,
            &finding_ledger,
        );

        let summary = match self
            .compose_final_review_summary(repo_cfg, pr_data, terminal_state, &transcript)
            .await
        {
            Ok(summary) => summary,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to generate final review summary; using fallback"
                );
                build_final_review_summary_fallback(pr_data, terminal_state, &review_attempts)
            }
        };

        self.db
            .upsert_review_archive(
                &repo_name,
                pr_data.number as i64,
                reviewed_sha,
                terminal_state,
                pr_data
                    .closed_at
                    .as_deref()
                    .or(pr_data.merged_at.as_deref()),
                &transcript,
                &summary,
            )
            .await?;

        tracing::info!(
            repo = %repo_name,
            pr = pr_data.number,
            terminal_state,
            "saved final review transcript and summary"
        );
        Ok(())
    }

    async fn compose_final_review_summary(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        terminal_state: &str,
        transcript: &str,
    ) -> Result<String> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();
        let reasoning_effort = repo_cfg.resolved_reasoning_effort(&self.config);
        let mut prompt = build_final_review_summary_prompt(
            &repo_cfg.full_name(),
            pr_data,
            terminal_state,
            transcript,
        );
        let max_bytes = self.config.defaults.max_prompt_bytes;
        let note =
            "\n\n[review transcript truncated before final-summary generation due size budget]\n";
        if prompt.len() > max_bytes {
            if max_bytes > note.len() {
                truncate_utf8_to_max_bytes(&mut prompt, max_bytes - note.len());
                prompt.push_str(note);
            } else {
                truncate_utf8_to_max_bytes(&mut prompt, max_bytes);
            }
        }

        let temp = tempdir().context("failed to create temp dir for final review summary")?;
        let harness_impl = harness::for_kind(harness_kind);
        let output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model,
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let summary = normalize_summary_output(&output.stdout, &output.stderr)
            .ok_or_else(|| anyhow!("final review summary harness returned empty output"))?;
        Ok(summary)
    }

    async fn handle_recurring_instruction_prs(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        findings: &[PendingFinding],
    ) -> Result<()> {
        let config = &self.config.instruction_prs;
        if !config.enabled || findings.is_empty() {
            return Ok(());
        }

        let repo_name = repo_cfg.full_name();
        let candidates = self
            .db
            .recurring_finding_candidates(&repo_name, config.min_distinct_prs, 1)
            .await?;

        for candidate in candidates {
            let claimed = self
                .db
                .claim_instruction_suggestion_pr(
                    &repo_name,
                    &candidate.fingerprint,
                    &config.mode.to_string(),
                )
                .await?;
            if !claimed {
                tracing::info!(
                    repo = %repo_name,
                    fingerprint = %candidate.fingerprint,
                    "recurring finding instruction PR already claimed"
                );
                continue;
            }

            match open_instruction_pr(
                &self.github,
                repo_cfg,
                &pr_data.base.ref_name,
                config,
                &candidate,
            )
            .await
            {
                Ok(Some(outcome)) => {
                    self.db
                        .record_instruction_suggestion_pr(
                            &repo_name,
                            &candidate.fingerprint,
                            &config.mode.to_string(),
                            &outcome.branch,
                            &outcome.pr_url,
                        )
                        .await?;
                    tracing::info!(
                        repo = %repo_name,
                        fingerprint = %candidate.fingerprint,
                        branch = %outcome.branch,
                        pr_url = %outcome.pr_url,
                        "opened recurring finding instruction PR"
                    );
                }
                Ok(None) => {
                    self.db
                        .record_instruction_suggestion_already_present(
                            &repo_name,
                            &candidate.fingerprint,
                            &config.mode.to_string(),
                        )
                        .await?;
                    tracing::info!(
                        repo = %repo_name,
                        fingerprint = %candidate.fingerprint,
                        "recurring finding already present in AGENTS.md"
                    );
                }
                Err(err) => {
                    if let Err(release_err) = self
                        .db
                        .release_instruction_suggestion_claim(&repo_name, &candidate.fingerprint)
                        .await
                    {
                        tracing::warn!(
                            repo = %repo_name,
                            fingerprint = %candidate.fingerprint,
                            error = %release_err,
                            "failed to release recurring finding instruction PR claim"
                        );
                    }
                    tracing::warn!(
                        repo = %repo_name,
                        fingerprint = %candidate.fingerprint,
                        error = %err,
                        "failed to open recurring finding instruction PR"
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn reply_to_comment(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        comment: &ReviewComment,
    ) -> Result<()> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();
        let reasoning_effort = repo_cfg.resolved_reasoning_effort(&self.config);

        let latest_diff = pr::get_diff(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();

        let all_comments = comments::get_review_comments(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            None,
        )
        .await
        .unwrap_or_default();
        let thread_history = build_thread_history(&all_comments, comment.id);
        let issue_comments = comments::get_issue_comments(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();
        let review_memory = build_review_memory(
            &[],
            &all_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
            pr_data.head.sha.as_str(),
            None,
            &HashMap::new(),
        );

        if let Some(reply) = build_direct_pushback_reply(
            &thread_history,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
        ) {
            comments::reply_to_review_comment(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                comment.id,
                &reply,
            )
            .await?;
            return Ok(());
        }

        let mut file_snippet = String::new();
        if let Some(line) = comment.line {
            if let Ok(Some(content)) = self
                .github
                .get_file_content(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    &comment.path,
                    &pr_data.head.sha,
                )
                .await
            {
                file_snippet = extract_file_snippet(&content, line as usize, 12);
            }
        }

        let mut prompt = String::new();
        prompt.push_str("Respond to this review thread comment.\n");
        prompt.push_str("First classify the thread state as one of: fixed, not_fixed, intentional_or_accepted, out_of_scope_for_this_pr, needs_human_judgment.\n");
        prompt.push_str("If a human explicitly says the concern is intentional, acceptable, or out of scope for this PR, acknowledge that and stop pressing the same line in this thread. If fixed, say so explicitly and reference concrete evidence. If not fixed, explain what remains without reopening adjacent design debates.\n");
        prompt.push_str("Output JSON in a fenced block tagged exactly `pr-review-reply-json` with this schema:\n");
        prompt.push_str("{\"reply\": string}\n\n");
        prompt.push_str(&format!("Repo: {}/{}\n", repo_cfg.owner, repo_cfg.name));
        prompt.push_str(&format!("PR: #{}\n", pr_data.number));
        prompt.push_str(&format!("Target comment id: {}\n", comment.id));
        prompt.push_str(&format!("Target comment path: {}\n", comment.path));
        prompt.push_str(&format!("Target comment line: {:?}\n", comment.line));
        prompt.push_str("\nThread history:\n");
        prompt.push_str(&thread_history);
        prompt.push_str("\n\nLatest diff (truncated):\n```diff\n");
        prompt.push_str(&truncate_lines(&latest_diff, 300));
        prompt.push_str("\n```\n");

        let review_state_context = build_review_state_context(&review_memory);
        if !review_state_context.is_empty() {
            prompt.push_str("\nReview state:\n");
            prompt.push_str(&review_state_context);
            prompt.push('\n');
        }

        if !file_snippet.is_empty() {
            prompt.push_str("\nLatest file snippet around referenced line:\n```\n");
            prompt.push_str(&file_snippet);
            prompt.push_str("\n```\n");
        }

        let temp = tempdir().context("failed to create temp dir for reply")?;
        let harness_impl = harness::for_kind(harness_kind);
        let output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model,
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let reply = match parse_reply_output(&output.stdout, &output.stderr)? {
            ReplyParseOutcome::Structured(update) => update.reply,
            ReplyParseOutcome::Raw(raw) => raw,
            ReplyParseOutcome::Empty => String::new(),
        };

        if reply.is_empty() {
            return Err(anyhow!("harness returned empty reply output"));
        }

        comments::reply_to_review_comment(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            comment.id,
            &reply,
        )
        .await?;

        Ok(())
    }

    pub async fn respond_to_maintainer_command(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        command: &str,
        source_comment_id: u64,
        source_author: &str,
        source_body: &str,
        orchestration_note: Option<&str>,
        last_reviewed_sha: Option<&str>,
    ) -> Result<()> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();
        let reasoning_effort = repo_cfg.resolved_reasoning_effort(&self.config);
        let repo_name = repo_cfg.full_name();
        let review_state = if last_reviewed_sha == Some(pr_data.head.sha.as_str()) {
            "reviewed_current_head"
        } else if last_reviewed_sha.is_some() {
            "new_head_pending_review"
        } else {
            "not_reviewed_yet"
        };
        let attempts = self
            .db
            .list_pr_review_attempts(&repo_name, pr_data.number as i64)
            .await
            .unwrap_or_default();

        let mut prompt = String::new();
        prompt.push_str("You are pr-reviewer, a persistent repository maintainer agent.\n");
        prompt.push_str("A maintainer addressed you in a pull request issue comment. Respond as the maintainer agent, using the operational state below. Do not invent facts. Keep the reply concise and useful.\n");
        prompt.push_str("Output JSON in a fenced block tagged exactly `pr-review-reply-json` with this schema:\n");
        prompt.push_str("{\"reply\": string}\n\n");
        prompt.push_str(&format!("Repository: {repo_name}\n"));
        prompt.push_str(&format!("Pull request: #{}\n", pr_data.number));
        prompt.push_str(&format!("PR title: {}\n", pr_data.title));
        prompt.push_str(&format!("Current head SHA: {}\n", pr_data.head.sha));
        prompt.push_str(&format!("Review state: {review_state}\n"));
        prompt.push_str(&format!("Command: {command}\n"));
        if let Some(note) = orchestration_note {
            prompt.push_str(&format!("Daemon orchestration result: {note}\n"));
        }
        prompt.push_str(&format!("Source comment id: {source_comment_id}\n"));
        prompt.push_str(&format!("Source author: {source_author}\n"));
        prompt.push_str("\nSource comment:\n");
        prompt.push_str(source_body);
        prompt.push_str("\n\nRecent review attempts:\n");
        if attempts.is_empty() {
            prompt.push_str("- none\n");
        } else {
            for attempt in attempts.iter().rev().take(6).rev() {
                prompt.push_str(&format!(
                    "- sha={} status={} verdict={} comments={} completed={}\n",
                    short_sha(&attempt.sha),
                    attempt.status,
                    attempt.verdict.as_deref().unwrap_or("unknown"),
                    attempt
                        .comments_posted
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    attempt.completed_at.as_deref().unwrap_or("not completed"),
                ));
            }
        }
        if let Some(instructions) = repo_cfg.custom_instructions.as_deref() {
            prompt.push_str("\nRepository instructions:\n");
            prompt.push_str(instructions);
            prompt.push('\n');
        }

        let temp = tempdir().context("failed to create temp dir for command response")?;
        let harness_impl = harness::for_kind(harness_kind);
        let output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model,
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let reply = match parse_reply_output(&output.stdout, &output.stderr)? {
            ReplyParseOutcome::Structured(update) => update.reply,
            ReplyParseOutcome::Raw(raw) => raw,
            ReplyParseOutcome::Empty => String::new(),
        };
        let reply = reply.trim();
        if reply.is_empty() {
            return Err(anyhow!("harness returned empty command response"));
        }

        let marker = format!(
            "<!-- pr-reviewer-command-response command={} pr={} source_comment={} sha={} -->",
            command, pr_data.number, source_comment_id, pr_data.head.sha
        );
        comments::create_issue_comment(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            &format!("{marker}\n\n{reply}"),
        )
        .await?;

        Ok(())
    }
}

fn clean_comment_path(path: &str) -> String {
    path.trim().trim_start_matches("./").to_string()
}

/// Dismissal signals from humans indicating a finding should not be re-flagged.
const DISMISSAL_SIGNALS: &[&str] = &[
    "intentional",
    "by design",
    "won't fix",
    "wontfix",
    "acknowledged",
    "expected",
    "not a bug",
    "fine",
    "this is fine",
    "leave it",
    "acceptable",
    "known",
    "on purpose",
];

const OUT_OF_SCOPE_SIGNALS: &[&str] = &[
    "out of scope",
    "not in scope",
    "not in this pr",
    "not for this pr",
    "not taking further",
    "not expanding this pr",
    "not reopening",
    "not part of this pr",
];

const RATIONALE_REJECTION_SIGNALS: &[&str] = &[
    "false positive",
    "not correct",
    "you are wrong",
    "this is wrong",
    "that is wrong",
    "misread",
    "misunderstanding",
    "not the issue",
];

fn has_dismissal_signal(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Split on whitespace and punctuation, but keep hyphens attached to words
    // so "fine-grained" stays as one token and doesn't match "fine"
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    DISMISSAL_SIGNALS.iter().any(|signal| {
        if signal.contains(' ') {
            joined.contains(signal)
        } else {
            words.iter().any(|w| *w == *signal)
        }
    })
}

fn has_out_of_scope_signal(text: &str) -> bool {
    let lower = normalize_text(text);
    OUT_OF_SCOPE_SIGNALS
        .iter()
        .any(|signal| lower.contains(&normalize_text(signal)))
}

fn has_rationale_rejection_signal(text: &str) -> bool {
    let lower = normalize_text(text);
    RATIONALE_REJECTION_SIGNALS
        .iter()
        .any(|signal| lower.contains(&normalize_text(signal)))
}

fn finding_key(path: &str, line: Option<u32>, body: &str) -> String {
    format!(
        "{}|{}|{}",
        path.trim().to_ascii_lowercase(),
        line.map(|v| v.to_string())
            .unwrap_or_else(|| "0".to_string()),
        normalize_text(body)
    )
}

fn token_overlap(left: &HashSet<String>, right: &HashSet<String>) -> usize {
    left.intersection(right).count()
}

/// `resolved_threads` maps a thread's root comment id to whether a human
/// resolved it on GitHub; pass an empty map when that is unknown.
fn build_review_memory(
    reviews: &[PullRequestReview],
    review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
    bot_name: &str,
    bot_alias: Option<&str>,
    current_sha: &str,
    changed_files: Option<&[String]>,
    resolved_threads: &HashMap<u64, bool>,
) -> ReviewMemory {
    let parent_map: HashMap<u64, Option<u64>> = review_comments
        .iter()
        .map(|c| (c.id, c.in_reply_to_id))
        .collect();
    let mut threads: HashMap<u64, Vec<&ReviewComment>> = HashMap::new();
    for comment in review_comments {
        let root = find_thread_root_id(&parent_map, comment.id);
        threads.entry(root).or_default().push(comment);
    }

    let mut explicit_boundaries = Vec::new();
    for comment in issue_comments {
        if !login_matches_bot(&comment.user.login, bot_name, bot_alias)
            && has_out_of_scope_signal(&comment.body)
        {
            explicit_boundaries.push(comment.body.clone());
        }
    }

    let findings = threads
        .into_iter()
        .filter_map(|(root_id, comments)| {
            let resolved = resolved_threads.get(&root_id).copied().unwrap_or(false);
            let mut sorted = comments;
            sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            let finding = sorted
                .iter()
                .find(|c| login_matches_bot(&c.user.login, bot_name, bot_alias))?;

            let human_replies: Vec<&&ReviewComment> = sorted
                .iter()
                .filter(|c| !login_matches_bot(&c.user.login, bot_name, bot_alias))
                .collect();
            if human_replies.is_empty() {
                let mut status = FindingStatus::Open;
                if let Some(changed_files) = changed_files {
                    if !changed_files.iter().any(|p| p == &finding.path) {
                        status = FindingStatus::LikelyAddressed;
                    }
                }
                if resolved {
                    status = FindingStatus::ResolvedByHuman;
                }
                return Some(FindingRecord {
                    key: finding_key(&finding.path, finding.line, &finding.body),
                    path: finding.path.clone(),
                    line: finding.line,
                    body: finding.body.clone(),
                    token_set: token_set(&finding.body),
                    status,
                });
            }

            let mut status = FindingStatus::Open;

            for reply in &human_replies {
                let reply_status = classify_rebuttal_status(&reply.body);
                if let Some(reply_status) = reply_status {
                    if reply_status == FindingStatus::OutOfScopeForPr {
                        explicit_boundaries.push(reply.body.clone());
                    }
                    if status_precedence(&reply_status) > status_precedence(&status) {
                        status = reply_status;
                    }
                }
            }

            if status == FindingStatus::Open {
                if let Some(changed_files) = changed_files {
                    if !changed_files.iter().any(|p| p == &finding.path) {
                        status = FindingStatus::LikelyAddressed;
                    }
                }
            }
            if resolved
                && status_precedence(&status) < status_precedence(&FindingStatus::ResolvedByHuman)
            {
                status = FindingStatus::ResolvedByHuman;
            }

            Some(FindingRecord {
                key: finding_key(&finding.path, finding.line, &finding.body),
                path: finding.path.clone(),
                line: finding.line,
                body: finding.body.clone(),
                token_set: token_set(&finding.body),
                status,
            })
        })
        .collect::<Vec<_>>();

    let already_noted_screenshot = reviews.iter().any(|review| {
        login_matches_bot(&review.user.login, bot_name, bot_alias)
            && review
                .body
                .as_deref()
                .is_some_and(|body| body.contains(SCREENSHOT_NOTE_TEXT))
    }) || issue_comments.iter().any(|comment| {
        login_matches_bot(&comment.user.login, bot_name, bot_alias)
            && comment.body.contains(SCREENSHOT_NOTE_TEXT)
    });

    let has_prior_bot_review = reviews.iter().any(|review| {
        login_matches_bot(&review.user.login, bot_name, bot_alias)
            && review.commit_id.as_deref() != Some(current_sha)
    });

    ReviewMemory {
        findings,
        already_noted_screenshot,
        has_prior_bot_review,
        explicit_boundaries,
    }
}

fn classify_rebuttal_status(text: &str) -> Option<FindingStatus> {
    if has_out_of_scope_signal(text) {
        return Some(FindingStatus::OutOfScopeForPr);
    }

    if has_rationale_rejection_signal(text) {
        return Some(FindingStatus::RejectedWithRationale);
    }

    if has_dismissal_signal(text) {
        return Some(FindingStatus::DismissedByHuman);
    }

    None
}

fn status_precedence(status: &FindingStatus) -> usize {
    match status {
        FindingStatus::Open => 0,
        FindingStatus::LikelyAddressed => 1,
        FindingStatus::ResolvedByHuman => 2,
        FindingStatus::DismissedByHuman => 3,
        FindingStatus::RejectedWithRationale => 4,
        FindingStatus::OutOfScopeForPr => 5,
    }
}

// Strip confidence markdown/JSON blocks from a review body to save context space.
fn strip_confidence_blocks(body: &str) -> String {
    let mut out = String::new();
    let mut in_confidence_block = false;
    let mut in_unmapped = false;
    for line in body.lines() {
        // Enter confidence block (fenced code or heading)
        if line.starts_with("### Confidence:") || line.starts_with("```pr-review-confidence-json") {
            in_confidence_block = true;
            continue;
        }
        // Exit confidence block
        if in_confidence_block {
            if line.starts_with("```")
                || (line.starts_with("### ") && !line.starts_with("### Confidence:"))
            {
                in_confidence_block = false;
                if line.starts_with("### ") {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }
        // Enter unmapped findings section
        if line.starts_with("Unmapped findings (not on changed lines):") {
            in_unmapped = true;
            continue;
        }
        // Unmapped findings: skip indented lines (list items), exit on anything else
        if in_unmapped {
            if line.starts_with("- ") || line.starts_with("  ") || line.trim().is_empty() {
                continue;
            }
            // Non-indented, non-empty line: end of unmapped section
            in_unmapped = false;
        }
        // Skip old confidence dimension line items
        if line.starts_with("- Style consistency")
            || line.starts_with("- Repository conventions adherence")
            || line.starts_with("- Merge conflict detection")
            || line.starts_with("- Security vulnerability detection")
            || line.starts_with("- Injection risk detection")
            || line.starts_with("- Attack-surface risk")
            || line.starts_with("- Future hardening")
            || line.starts_with("- Scope alignment")
            || line.starts_with("- Existing functionality")
            || line.starts_with("- Existing tooling")
            || line.starts_with("- Functional completeness")
            || line.starts_with("- Pattern correctness")
            || line.starts_with("- Documentation coverage")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn build_prior_reviews_context(
    reviews: &[PullRequestReview],
    _review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
    bot_name: &str,
    bot_alias: Option<&str>,
    current_sha: &str,
) -> String {
    let mut out = String::new();

    // --- Section 1: Prior bot review summaries ---
    let mut relevant: Vec<&PullRequestReview> = reviews
        .iter()
        .filter(|r| {
            login_matches_bot(&r.user.login, bot_name, bot_alias)
                && r.commit_id.as_deref() != Some(current_sha)
        })
        .collect();
    relevant.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
    let tail = if relevant.len() > 3 {
        &relevant[relevant.len() - 3..]
    } else {
        &relevant[..]
    };

    for review in tail {
        let sha = review.commit_id.as_deref().unwrap_or("unknown-sha");
        let state = review.state.as_deref().unwrap_or("UNKNOWN");
        let submitted = review.submitted_at.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "### Review on sha={} state={} ({})\n",
            sha, state, submitted
        ));
        if let Some(body) = review.body.as_deref() {
            let cleaned = strip_confidence_blocks(body);
            let truncated: String = cleaned.chars().take(2000).collect();
            out.push_str(truncated.trim());
            if cleaned.len() > 2000 {
                out.push_str("\n... [truncated]");
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // Note: Inline comment threads are handled by build_addressed_findings()
    // which provides status labels ([dismissed], [likely addressed], etc.)
    // to avoid double-inclusion of the same thread data.

    // --- Section 2: PR issue comments (general conversation) ---
    let human_issue_comments: Vec<&IssueComment> = issue_comments
        .iter()
        .filter(|c| !login_matches_bot(&c.user.login, bot_name, bot_alias))
        .collect();

    if !human_issue_comments.is_empty() {
        let tail = if human_issue_comments.len() > 5 {
            &human_issue_comments[human_issue_comments.len() - 5..]
        } else {
            &human_issue_comments[..]
        };

        out.push_str("### PR Conversation\n");
        for comment in tail {
            let body_preview: String = comment.body.lines().take(15).collect::<Vec<_>>().join("\n");
            out.push_str(&format!(
                "- (@{}, {}): {}\n",
                comment.user.login,
                comment.created_at,
                body_preview.replace('\n', " "),
            ));
        }
        out.push('\n');
    }

    out
}

/// Build a list of previously flagged findings with their status.
fn build_addressed_findings(
    review_comments: &[ReviewComment],
    parsed_diff: &crate::context::diff_parser::ParsedDiff,
    bot_name: &str,
    bot_alias: Option<&str>,
    resolved_threads: &HashMap<u64, bool>,
) -> String {
    if review_comments.is_empty() {
        return String::new();
    }

    let changed_files: Vec<String> = parsed_diff
        .files
        .iter()
        .map(|f| {
            if f.new_path != "/dev/null" {
                f.new_path.clone()
            } else {
                f.old_path.clone()
            }
        })
        .collect();
    let changed_file_set: std::collections::HashSet<&str> =
        changed_files.iter().map(|f| f.as_str()).collect();

    let memory = build_review_memory(
        &[],
        review_comments,
        &[],
        bot_name,
        bot_alias,
        "",
        Some(&changed_files),
        resolved_threads,
    );

    let mut out = String::new();

    for finding in memory.findings {
        let file_in_diff = changed_file_set.contains(finding.path.as_str());
        let status = match finding.status {
            FindingStatus::DismissedByHuman => "[dismissed by human]",
            FindingStatus::RejectedWithRationale => "[rejected with rationale]",
            FindingStatus::OutOfScopeForPr => "[out of scope for this pr]",
            FindingStatus::ResolvedByHuman => "[resolved by human]",
            FindingStatus::Open if !file_in_diff => "[likely addressed]",
            FindingStatus::Open | FindingStatus::LikelyAddressed => "[potentially addressed]",
        };

        let body_preview: String = finding.body.chars().take(200).collect();
        out.push_str(&format!(
            "- {}:{:?} {} {}\n",
            finding.path,
            finding.line,
            status,
            body_preview.replace('\n', " "),
        ));
    }

    out
}

fn build_review_state_context(memory: &ReviewMemory) -> String {
    let mut out = String::new();

    for finding in &memory.findings {
        let status = match finding.status {
            FindingStatus::Open => "open",
            FindingStatus::LikelyAddressed => "likely addressed",
            FindingStatus::ResolvedByHuman => "resolved by human",
            FindingStatus::DismissedByHuman => "dismissed by human",
            FindingStatus::RejectedWithRationale => "rejected with rationale",
            FindingStatus::OutOfScopeForPr => "out of scope for this pr",
        };
        let preview: String = finding.body.chars().take(180).collect();
        out.push_str(&format!(
            "- {}:{:?} [{}] {}\n",
            finding.path,
            finding.line,
            status,
            preview.replace('\n', " ")
        ));
    }

    if !memory.explicit_boundaries.is_empty() {
        out.push_str("\n### Explicit PR boundaries from human replies\n");
        for boundary in &memory.explicit_boundaries {
            out.push_str("- ");
            out.push_str(&boundary.replace('\n', " "));
            out.push('\n');
        }
    }

    out
}

fn build_stored_findings_context(findings: &[ReviewFindingRecord]) -> String {
    let active: Vec<&ReviewFindingRecord> = findings
        .iter()
        .filter(|finding| finding_status_is_active(&finding.status))
        .collect();
    if active.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for finding in active.iter().rev().take(12).rev() {
        let line = finding
            .line
            .map(|line| line.to_string())
            .unwrap_or_else(|| "?".to_string());
        let preview: String = finding.body.chars().take(220).collect();
        out.push_str(&format!(
            "- {}:{} [{} / {} / {}] {}\n",
            finding.path,
            line,
            finding.status,
            finding.severity,
            finding.finding_kind,
            preview.replace('\n', " ")
        ));
    }
    out
}

/// Update the persisted ledger with this review's findings.
///
/// A current finding updates the stored row it matches, so a re-raised
/// finding keeps its history instead of counting as new: exact fingerprints
/// first, then `same_finding`, preferring active rows. A follow-up note never
/// turns an active row inactive. A stored finding that is not re-emitted is
/// resolved only when its file left the diff or its own change differs from
/// the last review (`changed_since_last`): if the file did not change, the
/// problem cannot have been fixed, so the finding stays open.
///
/// Returns the upserts, the delta, and the fingerprint each current finding
/// was stored under (same order as `current_findings`).
fn reconcile_finding_ledger(
    repo: &str,
    pr_number: i64,
    sha: &str,
    author: &str,
    stored_findings: &[ReviewFindingRecord],
    current_findings: &[PendingFinding],
    review_memory: &ReviewMemory,
    diff_paths: &HashSet<String>,
    changed_since_last: Option<&HashSet<String>>,
) -> (Vec<ReviewFindingUpsert>, FindingDelta, Vec<String>) {
    let mut matched: HashSet<&str> = HashSet::new();
    let mut existing_for: Vec<Option<&ReviewFindingRecord>> = vec![None; current_findings.len()];

    // Pass 1: exact fingerprints.
    for (index, finding) in current_findings.iter().enumerate() {
        if let Some(stored) = stored_findings.iter().find(|stored| {
            stored.fingerprint == finding.fingerprint
                && !matched.contains(stored.fingerprint.as_str())
        }) {
            matched.insert(stored.fingerprint.as_str());
            existing_for[index] = Some(stored);
        }
    }
    // Pass 2: the same finding reworded or moved; active rows first.
    for (index, finding) in current_findings.iter().enumerate() {
        if existing_for[index].is_some() {
            continue;
        }
        let current = FindingRef {
            path: &finding.path,
            line: finding.line,
            body: &finding.body,
        };
        let candidates = || {
            stored_findings.iter().filter(|stored| {
                !matched.contains(stored.fingerprint.as_str())
                    && same_finding(stored_finding_ref(stored), current)
            })
        };
        let stored = candidates()
            .find(|stored| finding_status_is_active(&stored.status))
            .or_else(|| candidates().next());
        if let Some(stored) = stored {
            matched.insert(stored.fingerprint.as_str());
            existing_for[index] = Some(stored);
        }
    }

    let mut updates = Vec::new();
    let mut delta = FindingDelta::default();
    let mut final_fingerprints = Vec::with_capacity(current_findings.len());

    for (finding, existing) in current_findings.iter().zip(&existing_for) {
        let was_active = existing.is_some_and(|stored| finding_status_is_active(&stored.status));
        let status = if was_active {
            if finding.severity == CommentSeverity::Blocking {
                delta.still_blocking += 1;
                "still_blocking"
            } else {
                "open"
            }
        } else if finding.kind == PendingKind::Note {
            "noted"
        } else if finding.kind == PendingKind::StillOpen {
            // Its thread is open on GitHub but the ledger lost it (a reset
            // database): record it again without calling it new.
            if finding.severity == CommentSeverity::Blocking {
                "still_blocking"
            } else {
                "open"
            }
        } else {
            delta.newly_found += 1;
            "open"
        };
        let fingerprint = existing
            .map(|stored| stored.fingerprint.clone())
            .unwrap_or_else(|| finding.fingerprint.clone());
        final_fingerprints.push(fingerprint.clone());

        updates.push(ReviewFindingUpsert {
            repo: repo.to_string(),
            pr_number,
            author: Some(author.to_string()),
            fingerprint,
            recurrence_fingerprint: Some(recurring_finding_fingerprint(&finding.body)),
            sha: sha.to_string(),
            status: status.to_string(),
            severity: severity_label(finding.severity).to_string(),
            finding_kind: finding.finding_kind.as_str().to_string(),
            path: finding.path.clone(),
            line: finding.line.map(i64::from),
            body: finding.body.clone(),
            evidence_note: finding.evidence_note.clone(),
            github_comment_id: existing.and_then(|stored| stored.github_comment_id),
            resolved_by_sha: None,
            resolution_reason: None,
        });
    }

    for stored in stored_findings {
        if !finding_status_is_active(&stored.status)
            || matched.contains(stored.fingerprint.as_str())
        {
            continue;
        }

        let classified = classified_prior_status(stored, review_memory);
        let answered_by_human = matches!(
            classified,
            Some(
                FindingStatus::DismissedByHuman
                    | FindingStatus::RejectedWithRationale
                    | FindingStatus::OutOfScopeForPr
                    | FindingStatus::ResolvedByHuman
            )
        );
        // Unknown (no per-file hashes and no compare diff) counts as unchanged:
        // keeping a finding open is safer than closing one that still applies.
        let file_unchanged = diff_paths.contains(&stored.path)
            && changed_since_last.is_none_or(|changed| !changed.contains(&stored.path));
        if !answered_by_human && file_unchanged {
            // Not re-emitted, but this PR's change to its file is the same as
            // at the last review, so it cannot have been fixed. Keep it open.
            continue;
        }

        let (status, reason) = match classified {
            Some(FindingStatus::DismissedByHuman) | Some(FindingStatus::RejectedWithRationale) => {
                delta.rebutted_or_scoped += 1;
                ("rebutted", "human rebuttal or rationale")
            }
            Some(FindingStatus::OutOfScopeForPr) => {
                delta.rebutted_or_scoped += 1;
                ("out_of_scope", "human marked out of scope")
            }
            Some(FindingStatus::ResolvedByHuman) => {
                delta.likely_fixed += 1;
                ("resolved", "human resolved the review thread")
            }
            Some(FindingStatus::LikelyAddressed) => {
                delta.likely_fixed += 1;
                ("likely_fixed", "finding no longer present in current diff")
            }
            _ => {
                delta.likely_fixed += 1;
                ("likely_fixed", "not re-emitted after its file changed")
            }
        };

        updates.push(ReviewFindingUpsert {
            repo: repo.to_string(),
            pr_number,
            author: stored.author.clone(),
            fingerprint: stored.fingerprint.clone(),
            recurrence_fingerprint: stored.recurrence_fingerprint.clone(),
            sha: sha.to_string(),
            status: status.to_string(),
            severity: stored.severity.clone(),
            finding_kind: stored.finding_kind.clone(),
            path: stored.path.clone(),
            line: stored.line,
            body: stored.body.clone(),
            evidence_note: stored.evidence_note.clone(),
            github_comment_id: stored.github_comment_id,
            resolved_by_sha: Some(sha.to_string()),
            resolution_reason: Some(reason.to_string()),
        });
    }

    (updates, delta, final_fingerprints)
}

fn classified_prior_status(
    stored: &ReviewFindingRecord,
    review_memory: &ReviewMemory,
) -> Option<FindingStatus> {
    review_memory
        .findings
        .iter()
        .filter(|finding| same_finding(finding.as_ref(), stored_finding_ref(stored)))
        .map(|finding| finding.status)
        .max_by_key(status_precedence)
}

fn finding_status_is_active(status: &str) -> bool {
    matches!(status, "open" | "still_blocking")
}

fn severity_label(severity: CommentSeverity) -> &'static str {
    match severity {
        CommentSeverity::Blocking => "blocking",
        CommentSeverity::Warning => "warning",
        CommentSeverity::Nitpick => "nitpick",
    }
}

fn build_finding_delta_summary(delta: &FindingDelta) -> Option<String> {
    if delta.newly_found == 0
        && delta.still_blocking == 0
        && delta.likely_fixed == 0
        && delta.rebutted_or_scoped == 0
    {
        return None;
    }

    Some(format!(
        "**Since my last review:**\n- Fixed or likely addressed: {}\n- Still blocking: {}\n- Newly found: {}\n- Suppressed due to human rebuttal/out-of-scope: {}",
        delta.likely_fixed, delta.still_blocking, delta.newly_found, delta.rebutted_or_scoped
    ))
}

fn apply_scope_drift_policy(comment: &mut ParsedReviewComment) {
    if comment.finding_kind != FindingKind::ScopeDrift
        || comment.severity != CommentSeverity::Blocking
        || scope_drift_is_high_risk(comment)
    {
        return;
    }

    comment.severity = CommentSeverity::Warning;
}

fn scope_drift_is_high_risk(comment: &ParsedReviewComment) -> bool {
    matches!(
        comment.finding_kind,
        FindingKind::GeneratedArtifact
            | FindingKind::SecretExposure
            | FindingKind::LocalEnvironmentLeak
            | FindingKind::Security
            | FindingKind::DataIntegrity
    ) || {
        let body = normalize_text(&comment.body);
        let path = normalize_text(&comment.file);
        const HIGH_RISK_SCOPE_TERMS: &[&str] = &[
            "generated",
            "dist/",
            ".svelte-kit",
            "secret",
            "token",
            "private environment",
            "local path",
            "machine-specific",
            "unrelated production",
            "data exposure",
            "attack surface",
            "credential",
        ];
        HIGH_RISK_SCOPE_TERMS
            .iter()
            .any(|term| body.contains(term) || path.contains(term))
    }
}

fn can_reopen_rebutted_finding(
    comment: &ParsedReviewComment,
    path: &str,
    memory: &ReviewMemory,
) -> bool {
    comment
        .evidence_note
        .as_deref()
        .is_some_and(|note| !note.trim().is_empty())
        || memory.findings.iter().any(|prior| {
            prior.path == path
                && prior.status == FindingStatus::Open
                && prior.line != Some(comment.line)
                && token_overlap(&prior.token_set, &token_set(&comment.body)) >= 2
        })
}

/// Drop findings a human already answered: rebutted, out of scope, or
/// likely addressed. Findings whose thread is still open or was resolved are
/// handled by the review-cycle policy (`decide_disposition`) instead.
fn should_suppress_finding(
    comment: &ParsedReviewComment,
    path: &str,
    memory: &ReviewMemory,
) -> bool {
    let candidate = FindingRef {
        path,
        line: Some(comment.line),
        body: &comment.body,
    };
    let tokens = token_set(&comment.body);

    for prior in &memory.findings {
        if !same_finding(prior.as_ref(), candidate) {
            continue;
        }

        match prior.status {
            FindingStatus::DismissedByHuman
            | FindingStatus::RejectedWithRationale
            | FindingStatus::OutOfScopeForPr => {
                if !can_reopen_rebutted_finding(comment, path, memory) {
                    return true;
                }
            }
            FindingStatus::LikelyAddressed => return true,
            FindingStatus::Open | FindingStatus::ResolvedByHuman => {}
        }
    }

    if !memory.explicit_boundaries.is_empty() {
        for boundary in &memory.explicit_boundaries {
            if token_overlap(&tokens, &token_set(boundary)) >= 2
                && !can_reopen_rebutted_finding(comment, path, memory)
            {
                return true;
            }
        }
    }

    false
}

fn blocker_has_sufficient_evidence(
    comment: &ParsedReviewComment,
    path: &str,
    review_confidence: crate::review::parser::ConfidenceLevel,
) -> bool {
    if comment
        .evidence_note
        .as_deref()
        .is_some_and(|note| !note.trim().is_empty())
    {
        return true;
    }

    if review_confidence == crate::review::parser::ConfidenceLevel::Low {
        return false;
    }

    let body = normalize_text(&comment.body);
    let path_normalized = normalize_text(path);

    // Strong: body references the specific file path
    if body.contains(&path_normalized) {
        return true;
    }

    // Strong: causal verb phrases that indicate concrete reasoning
    const CAUSAL_PHRASES: &[&str] = &[
        "will panic",
        "will fail",
        "will crash",
        "will break",
        "will deadlock",
        "will overflow",
        "will loop",
        "will block",
        "will hang",
        "will leak",
        "will corrupt",
        "will lose",
        "will drop",
        "will skip",
        "will miss",
        "will silently",
        "will always",
        "will never",
        "can panic",
        "can fail",
        "can crash",
        "can deadlock",
        "can overflow",
        "can corrupt",
        "can lose",
        "unconditionally",
        "hardcodes",
        "hardcoded",
        "unreachable",
        "always returns",
        "never returns",
        "infinite loop",
        "use after free",
        "null dereference",
        "data race",
        "undefined behavior",
        "buffer overflow",
        "sql injection",
        "command injection",
    ];

    CAUSAL_PHRASES.iter().any(|phrase| body.contains(phrase))
}

fn truncate_lines(input: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in input.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... [truncated]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn build_thread_history(comments: &[ReviewComment], target_comment_id: u64) -> String {
    if comments.is_empty() {
        return "No thread history available.".to_string();
    }

    let parent_map: HashMap<u64, Option<u64>> =
        comments.iter().map(|c| (c.id, c.in_reply_to_id)).collect();
    let target_root = find_thread_root_id(&parent_map, target_comment_id);

    let mut thread_comments: Vec<&ReviewComment> = comments
        .iter()
        .filter(|c| find_thread_root_id(&parent_map, c.id) == target_root)
        .collect();

    thread_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut out = String::new();
    for c in thread_comments {
        let label = if c.user.is_bot() {
            format!("{} (bot)", c.user.login)
        } else {
            c.user.login.clone()
        };
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            c.created_at,
            label,
            c.body.replace('\n', " ")
        ));
    }
    if out.is_empty() {
        out.push_str("No thread history available.");
    }
    out
}

fn build_direct_pushback_reply(
    thread_history: &str,
    bot_name: &str,
    bot_alias: Option<&str>,
) -> Option<String> {
    let bot_lower = bot_name.to_ascii_lowercase();
    let alias_lower = bot_alias.map(|a| a.to_ascii_lowercase());
    let mention_target = format!("@{}", bot_lower);
    let mention_alias = alias_lower.as_ref().map(|a| format!("@{}", a));

    // Check for @bot mentions only in non-bot lines (avoid matching the bot's own text)
    let human_lines: Vec<&str> = thread_history
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            // Thread history format: "- [timestamp] login: body"
            // Skip lines authored by the bot itself
            !line_is_authored_by_bot(&lower, &bot_lower, alias_lower.as_deref())
        })
        .collect();

    for line in &human_lines {
        let lower = line.to_ascii_lowercase();
        if lower.contains(&mention_target) {
            return None;
        }
        if let Some(ref alias_mention) = mention_alias {
            if lower.contains(alias_mention) {
                return None;
            }
        }
    }

    // Only scan human lines for pushback signals. The bot's own review body
    // can contain phrases like "not correct" or "on purpose" as part of its
    // analysis, which would falsely trigger a canned acknowledgement.
    if human_lines
        .iter()
        .rev()
        .any(|line| has_out_of_scope_signal(line))
    {
        return Some(
            "acknowledged. i won't keep pushing that line in this PR unless later changes show a direct regression in the scoped feature.".to_string(),
        );
    }

    if human_lines
        .iter()
        .rev()
        .any(|line| has_dismissal_signal(line) || has_rationale_rejection_signal(line))
    {
        return Some(
            "got it. treating that as intentional for this PR, so i won't keep re-raising the same concern unless later changes add new concrete evidence.".to_string(),
        );
    }

    None
}

/// Check if a lowercased thread-history line was authored by the bot.
/// Thread history format: `- [timestamp] login: body` or `- [timestamp] login (bot): body`
fn line_is_authored_by_bot(lower_line: &str, bot_lower: &str, alias_lower: Option<&str>) -> bool {
    // Strip leading "- [timestamp] " to get "login: body" or "login (bot): body"
    let after_bracket = match lower_line.find("] ") {
        Some(pos) => &lower_line[pos + 2..],
        None => return false,
    };
    let raw_login = match after_bracket.find(": ") {
        Some(pos) => after_bracket[..pos].trim(),
        None => return false,
    };
    // Strip " (bot)" suffix if present
    let login = raw_login.strip_suffix(" (bot)").unwrap_or(raw_login);
    login == bot_lower || alias_lower.is_some_and(|a| login == a)
}

fn find_thread_root_id(parent_map: &HashMap<u64, Option<u64>>, id: u64) -> u64 {
    let mut current = id;
    let mut guard = 0usize;
    while guard < 64 {
        guard += 1;
        match parent_map.get(&current).copied().flatten() {
            Some(parent) => current = parent,
            None => return current,
        }
    }
    current
}

fn extract_file_snippet(content: &str, line: usize, radius: usize) -> String {
    if line == 0 {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let start = line.saturating_sub(radius + 1);
    let end = (line + radius).min(lines.len());

    let mut out = String::new();
    for idx in start..end {
        out.push_str(&format!("{:>5}: {}\n", idx + 1, lines[idx]));
    }
    out
}

#[derive(Debug)]
struct ReviewTranscriptEvent {
    sort_key: String,
    ordinal: usize,
    heading: String,
    body: String,
}

fn terminal_pr_state(pr_data: &PullRequest) -> &'static str {
    // merged_at being set is the authoritative signal that a PR was merged.
    // We intentionally do not also require state=="closed" here, because
    // finalize_closed_pr_review proceeds whenever merged_at.is_some()
    // (regardless of the state field).  Requiring state=="closed" in the
    // labeler would cause a partial payload (merged_at set, state absent /
    // defaulting to "open") to be archived by the gate but labeled "closed"
    // — a contradiction.  Using merged_at alone keeps both functions
    // consistent: if the gate treats merged_at as sufficient evidence of a
    // merge, so does the labeler.
    if pr_data.merged_at.is_some() {
        "merged"
    } else {
        "closed"
    }
}

fn build_final_review_transcript(
    repo_name: &str,
    pr_data: &PullRequest,
    terminal_state: &str,
    review_attempts: &[ReviewAttemptRecord],
    existing_reviews: &[PullRequestReview],
    review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
    finding_ledger: &[ReviewFindingRecord],
) -> String {
    let mut transcript = String::new();
    transcript.push_str("# PR Review Transcript\n\n");
    transcript.push_str(&format!("- Repo: {repo_name}\n"));
    transcript.push_str(&format!("- PR: #{} - {}\n", pr_data.number, pr_data.title));
    transcript.push_str(&format!("- Author: @{}\n", pr_data.user.login));
    transcript.push_str(&format!("- Terminal state: {terminal_state}\n"));
    transcript.push_str(&format!(
        "- Head SHA at archive time: {}\n",
        pr_data.head.sha
    ));
    if let Some(url) = pr_data.html_url.as_deref() {
        transcript.push_str(&format!("- URL: {url}\n"));
    }
    if let Some(closed_at) = pr_data
        .closed_at
        .as_deref()
        .or(pr_data.merged_at.as_deref())
    {
        transcript.push_str(&format!("- Closed at: {closed_at}\n"));
    }
    if let Some(body) = pr_data
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        transcript.push_str("\n## PR Description\n\n");
        transcript.push_str(&quote_markdown_block(body));
        transcript.push('\n');
    }
    transcript.push('\n');

    if !finding_ledger.is_empty() {
        transcript.push_str("## Finding Resolution State\n\n");
        for finding in finding_ledger {
            let line = finding
                .line
                .map(|line| line.to_string())
                .unwrap_or_else(|| "?".to_string());
            transcript.push_str(&format!(
                "- {}:{} [{} / {} / {}] first_sha={} last_seen_sha={}",
                finding.path,
                line,
                finding.status,
                finding.severity,
                finding.finding_kind,
                short_sha(&finding.first_sha),
                short_sha(&finding.last_seen_sha)
            ));
            if let Some(resolved_by) = finding.resolved_by_sha.as_deref() {
                transcript.push_str(&format!(" resolved_by={}", short_sha(resolved_by)));
            }
            if let Some(reason) = finding.resolution_reason.as_deref() {
                transcript.push_str(&format!(" reason={}", reason.replace('\n', " ")));
            }
            transcript.push('\n');
            let preview: String = finding.body.chars().take(280).collect();
            transcript.push_str(&format!("  - {}\n", preview.replace('\n', " ")));
        }
        transcript.push('\n');
    }

    let mut events = Vec::new();
    let mut ordinal = 0usize;

    for attempt in review_attempts {
        ordinal += 1;
        let mut body = String::new();
        body.push_str(&format!("- sha: {}\n", attempt.sha));
        body.push_str(&format!("- harness: {}\n", attempt.harness));
        if let Some(model) = attempt.model.as_deref() {
            body.push_str(&format!("- model: {model}\n"));
        }
        body.push_str(&format!("- status: {}\n", attempt.status));
        if let Some(verdict) = attempt.verdict.as_deref() {
            body.push_str(&format!("- verdict: {verdict}\n"));
        }
        if let Some(comments_posted) = attempt.comments_posted {
            body.push_str(&format!("- comments_posted: {comments_posted}\n"));
        }
        if let Some(duration_secs) = attempt.duration_secs {
            body.push_str(&format!("- duration_secs: {:.1}\n", duration_secs));
        }
        if let Some(completed_at) = attempt.completed_at.as_deref() {
            body.push_str(&format!("- completed_at: {completed_at}\n"));
        }
        if let Some(error) = attempt.error_message.as_deref() {
            body.push_str("- error:\n");
            body.push_str(&quote_markdown_block(error));
        }

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&attempt.created_at),
            ordinal,
            heading: format!("Local review attempt {}", attempt.created_at),
            body,
        });
    }

    for review in existing_reviews {
        ordinal += 1;
        let submitted_at = review.submitted_at.as_deref().unwrap_or("unknown");
        let mut body = String::new();
        if let Some(state) = review.state.as_deref() {
            body.push_str(&format!("- state: {state}\n"));
        }
        if let Some(commit_id) = review.commit_id.as_deref() {
            body.push_str(&format!("- commit: {commit_id}\n"));
        }
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(
            review.body.as_deref().unwrap_or("(empty review body)"),
        ));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(submitted_at),
            ordinal,
            heading: format!("GitHub review by @{} ({submitted_at})", review.user.login),
            body,
        });
    }

    for comment in review_comments {
        ordinal += 1;
        let location = comment
            .line
            .map(|line| format!("{}:{}", comment.path, line))
            .unwrap_or_else(|| comment.path.clone());
        let mut body = String::new();
        if let Some(parent_id) = comment.in_reply_to_id {
            body.push_str(&format!("- in_reply_to_id: {parent_id}\n"));
        }
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(&comment.body));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&comment.created_at),
            ordinal,
            heading: format!(
                "Review thread comment by @{} on {} ({})",
                comment.user.login, location, comment.created_at
            ),
            body,
        });
    }

    for comment in issue_comments {
        ordinal += 1;
        let mut body = String::new();
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(&comment.body));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&comment.created_at),
            ordinal,
            heading: format!(
                "Issue comment by @{} ({})",
                comment.user.login, comment.created_at
            ),
            body,
        });
    }

    events.sort_by(|left, right| {
        left.sort_key
            .cmp(&right.sort_key)
            .then(left.ordinal.cmp(&right.ordinal))
    });

    if events.is_empty() {
        transcript
            .push_str("No persisted review attempts or GitHub conversation were available.\n");
        return transcript;
    }

    transcript.push_str("## Timeline\n\n");
    for event in events {
        transcript.push_str("### ");
        transcript.push_str(&event.heading);
        transcript.push('\n');
        transcript.push_str(&event.body);
        if !event.body.ends_with('\n') {
            transcript.push('\n');
        }
        transcript.push('\n');
    }

    transcript
}

fn build_final_review_summary_prompt(
    repo_name: &str,
    pr_data: &PullRequest,
    terminal_state: &str,
    transcript: &str,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Write a concise retrospective summary of this completed PR review.\n");
    prompt.push_str("The PR is already closed or merged, so describe what the review covered and how it concluded.\n");
    prompt.push_str("Focus on the important review themes, notable issues raised, the Finding Resolution State section, whether follow-up discussion resolved them, and the final outcome.\n");
    prompt.push_str("If details are sparse, say that plainly instead of inventing specifics.\n");
    prompt.push_str("Output plain markdown only. No code fences. Keep it under 250 words.\n\n");
    prompt.push_str(&format!("Repo: {repo_name}\n"));
    prompt.push_str(&format!("PR: #{}\n", pr_data.number));
    prompt.push_str(&format!("Title: {}\n", pr_data.title));
    prompt.push_str(&format!("Author: @{}\n", pr_data.user.login));
    prompt.push_str(&format!("Terminal state: {terminal_state}\n"));
    prompt.push_str(&format!(
        "Head SHA at archive time: {}\n\n",
        pr_data.head.sha
    ));
    prompt.push_str("Transcript:\n\n");
    prompt.push_str(transcript);
    prompt
}

fn build_final_review_summary_fallback(
    pr_data: &PullRequest,
    terminal_state: &str,
    review_attempts: &[ReviewAttemptRecord],
) -> String {
    let completed = review_attempts
        .iter()
        .filter(|attempt| attempt.status == "completed")
        .count();
    let failed = review_attempts
        .iter()
        .filter(|attempt| attempt.status == "failed")
        .count();
    let last_verdict = review_attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.verdict.as_deref());

    let mut summary = format!(
        "PR #{} ended as {} after {} recorded review attempt(s).",
        pr_data.number,
        terminal_state,
        review_attempts.len()
    );
    if completed > 0 || failed > 0 {
        summary.push_str(&format!(" {} completed, {} failed.", completed, failed));
    }
    if let Some(verdict) = last_verdict {
        summary.push_str(&format!(" Final recorded verdict: {verdict}."));
    }
    summary.push_str(" Full transcript archived in the database.");
    summary
}

fn normalize_summary_output(stdout: &str, _stderr: &str) -> Option<String> {
    let out = stdout.trim();
    if out.is_empty() {
        return None;
    }

    let normalized = strip_fenced_code_block(out);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Strip an optional fenced code block wrapper (``` or ```lang) that some
/// harnesses add around their output.  Falls back to plain backtick trimming
/// for outputs that are just backtick-quoted without a newline.
fn strip_fenced_code_block(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() >= 2 {
        let first = lines[0].trim();
        let last = lines[lines.len() - 1].trim();
        if first.starts_with("```") && last == "```" {
            // Strip the opening fence (with optional language tag) and closing fence.
            // Return the inner content — even if empty — rather than falling through
            // to the backtick-trimming path, which would leave the language tag (e.g.
            // "markdown") as if it were real content when the inner body is blank.
            let inner = &lines[1..lines.len() - 1];
            return inner.join("\n").trim().to_string();
        }
    }
    // Fallback: strip loose backticks from edges (bare ` wrapping)
    text.trim_matches('`').trim().to_string()
}

fn quote_markdown_block(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "> (empty)\n".to_string();
    }

    let mut out = String::new();
    for line in trimmed.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn timestamp_sort_key(timestamp: &str) -> String {
    let trimmed = timestamp.trim();
    if trimmed.is_empty() {
        return "9999-12-31T23:59:59Z".to_string();
    }
    if trimmed.contains('T') {
        return trimmed.to_string();
    }
    format!("{}Z", trimmed.replace(' ', "T"))
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 8 {
        sha
    } else {
        &sha[..8]
    }
}

#[derive(Debug, Deserialize)]
struct InProgressCommentPayload {
    comment: String,
}

fn build_in_progress_comment_prompt(
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
    let clean_author = pr_author.trim().trim_start_matches('@');
    let author_label = if clean_author.is_empty() {
        "@teammate".to_string()
    } else {
        format!("@{clean_author}")
    };

    let title_context = sanitize_in_progress_title(pr_title.trim());
    let focus = infer_pr_focus(pr_title);
    let short = short_sha(sha);

    let mut prompt = String::new();
    prompt.push_str("Write one short GitHub PR in-progress comment.\n");
    prompt.push_str("Voice: conversational teammate, calm, direct, plainspoken.\n");
    prompt.push_str("No emojis. No customer-support phrasing.\n");
    prompt.push_str("Output MUST be a fenced JSON block tagged `pr-review-in-progress-json` with exactly this schema:\n");
    prompt.push_str("{\"comment\": string}\n");
    prompt.push_str("Comment rules:\n");
    prompt.push_str("- One or two sentences only.\n");
    prompt.push_str("- Single line only.\n");
    prompt.push_str("- Keep it under 220 characters.\n");
    prompt.push_str("- Mention commit `");
    prompt.push_str(short);
    prompt.push_str("`.\n");
    prompt.push_str("- Mention that you're reviewing ");
    prompt.push_str(focus);
    prompt.push_str(" in `");
    prompt.push_str(&title_context);
    prompt.push_str("`.\n");
    prompt.push_str("- Address the author as ");
    prompt.push_str(&author_label);
    prompt.push_str(".\n");
    prompt.push_str(
        "- Do NOT introduce yourself, say you are automated, or include a pr-reviewer link; metadata is handled separately.\n",
    );
    if has_prior_bot_review {
        prompt.push_str("- This is a follow-up pass on the same PR: do NOT re-introduce yourself and do NOT include a tool/repo link.\n");
    } else {
        prompt.push_str(
            "- This is first-touch on this PR: simply acknowledge that you are taking a look.\n",
        );
    }

    prompt
}

fn build_in_progress_comment_fallback(
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
    let clean_author = pr_author.trim().trim_start_matches('@');
    let greeting = if clean_author.is_empty() {
        "Hi there".to_string()
    } else {
        format!("Hi @{clean_author}")
    };

    let clean_title = pr_title.trim();
    let title_context = if clean_title.is_empty() {
        "this PR".to_string()
    } else {
        format!("`{}`", sanitize_in_progress_title(clean_title))
    };

    if has_prior_bot_review {
        format!(
            "{} - quick follow-up pass on {} (commit `{}`); taking another look at {} and I will report back shortly.",
            greeting,
            title_context,
            short_sha(sha),
            infer_pr_focus(pr_title),
        )
    } else {
        format!(
            "{} - I'm taking a look at {} in {} (commit `{}`) and will follow up shortly.",
            greeting,
            infer_pr_focus(pr_title),
            title_context,
            short_sha(sha),
        )
    }
}

fn extract_in_progress_comment(stdout: &str, stderr: &str) -> Option<String> {
    let raw = preferred_harness_output(stdout, stderr);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(marked) = extract_marked_json(trimmed, "pr-review-in-progress-json") {
        if let Ok(parsed) = serde_json::from_str::<InProgressCommentPayload>(&marked) {
            return Some(parsed.comment);
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(comment) = find_comment_text(&value) {
            return Some(comment);
        }
    }

    let mut candidates = extract_json_objects(trimmed);
    candidates.reverse();
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if let Some(comment) = find_comment_text(&value) {
                return Some(comment);
            }
        }
    }

    // Do not post arbitrary raw harness output to GitHub comments.
    // If we cannot parse structured text, fall back to deterministic templates.
    let _ = looks_like_harness_error_output(trimmed);
    None
}

fn preferred_harness_output(stdout: &str, stderr: &str) -> String {
    if !stdout.trim().is_empty() {
        return stdout.trim().to_string();
    }
    stderr.trim().to_string()
}

fn normalize_in_progress_comment(raw: &str) -> Option<String> {
    let collapsed = raw
        .trim()
        .trim_matches('"')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if collapsed.is_empty() {
        return None;
    }

    if collapsed.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
        return None;
    }

    Some(collapsed)
}

fn truncate_github_comment_body(body: &str) -> String {
    if body.chars().count() <= GITHUB_COMMENT_MAX_CHARS {
        return body.to_string();
    }

    let keep = GITHUB_COMMENT_MAX_CHARS.saturating_sub(GITHUB_COMMENT_TRUNCATION_NOTE.len());
    let mut truncated: String = body.chars().take(keep).collect();
    while truncated.ends_with('\n') {
        truncated.pop();
    }
    truncated.push_str(GITHUB_COMMENT_TRUNCATION_NOTE);
    truncated
}

fn build_retry_review_body(body: &str, inline_comments: &[CreateReviewComment]) -> String {
    if inline_comments.is_empty() {
        return truncate_github_comment_body(body);
    }

    let mut retry_body = truncate_github_comment_body(body);
    let header = "\n\n**Inline comments (could not post as line comments):**\n";
    if retry_body.chars().count() + header.chars().count() > GITHUB_COMMENT_MAX_CHARS {
        return retry_body;
    }
    retry_body.push_str(header);

    let total = inline_comments.len();
    let mut appended = 0usize;

    for (idx, comment) in inline_comments.iter().enumerate() {
        let normalized_comment = comment
            .body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!(
            "- `{}:{}` - {}\n",
            comment.path, comment.line, normalized_comment
        );
        let remaining = total.saturating_sub(idx + 1);
        let omission_note = if remaining > 0 {
            format!(
                "\n_Omitted {remaining} additional inline comments to stay within GitHub's body limit._"
            )
        } else {
            String::new()
        };

        if retry_body.chars().count() + line.chars().count() + omission_note.chars().count()
            > GITHUB_COMMENT_MAX_CHARS
        {
            break;
        }

        retry_body.push_str(&line);
        appended += 1;
    }

    let omitted = total.saturating_sub(appended);
    if omitted > 0 {
        let omission_note = format!(
            "\n_Omitted {omitted} additional inline comments to stay within GitHub's body limit._"
        );
        if retry_body.chars().count() + omission_note.chars().count() <= GITHUB_COMMENT_MAX_CHARS {
            retry_body.push_str(&omission_note);
        }
    }

    retry_body
}

/// Run a GitHub call up to three times, waiting 1 s and then 3 s between
/// attempts, for lookups whose failure would otherwise fail the review.
async fn with_github_retries<T, F, Fut>(what: &str, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delays = [1u64, 3].into_iter();
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(err) => match delays.next() {
                Some(delay) => {
                    tracing::warn!(error = %err, "failed to {what}; retrying in {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
                None => return Err(err.context(format!("failed to {what}"))),
            },
        }
    }
}

fn build_review_metadata_block(model: &str, short_sha: &str) -> String {
    format!(
        "<details>\n<summary>Review metadata</summary>\n\n{}{})\n- Model: `{model}`\n- Commit: `{short_sha}`\n\n</details>\n\n",
        REVIEW_METADATA_MARKER,
        pr_reviewer_project_url(),
    )
}

fn looks_like_reintroduction(comment: &str) -> bool {
    let lower = comment.to_ascii_lowercase();
    lower.contains("pr-reviewing agent")
        || lower.contains("automated code reviewer")
        || lower.contains("powered by [pr-reviewer]")
        || lower.contains("powered by pr-reviewer")
        || lower.contains("[pr-reviewer](")
}

fn looks_like_harness_error_output(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("error:")
        || lower.contains("traceback")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("exit status")
        || lower.contains("usage:")
        || lower.contains("command not found")
}

fn looks_like_harness_transport_output(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains("{\"type\":\"thread.started\"")
        || trimmed.contains("{\"type\":\"turn.started\"")
        || (trimmed.contains("\"type\":\"command_execution\"")
            && trimmed.contains("\"item.started\""))
        || trimmed.contains("Unable to open session log file")
        || (trimmed.contains("codex: line") && trimmed.contains("unbound variable"))
}

fn truncate_utf8_to_max_bytes(input: &mut String, max_bytes: usize) {
    if input.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes.min(input.len());
    while boundary > 0 && !input.is_char_boundary(boundary) {
        boundary -= 1;
    }
    input.truncate(boundary);
}

fn login_matches_bot(
    login: &str,
    configured_bot_name: &str,
    authenticated_user: Option<&str>,
) -> bool {
    login.eq_ignore_ascii_case(configured_bot_name)
        || authenticated_user.is_some_and(|user| login.eq_ignore_ascii_case(user))
}

fn extract_marked_json(text: &str, marker_name: &str) -> Option<String> {
    let marker = format!("```{marker_name}");
    let start = text.find(&marker)?;
    let after = &text[start + marker.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

fn extract_json_objects(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut start_idx: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start_idx = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start_idx.take() {
                        results.push(text[start..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    results
}

fn find_comment_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["comment", "status_comment", "message", "text", "content"] {
                if let Some(text) = map.get(key).and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        return Some(text.to_string());
                    }
                }
            }
            for nested in map.values() {
                if let Some(text) = find_comment_text(nested) {
                    return Some(text);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(text) = find_comment_text(item) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn sanitize_in_progress_title(title: &str) -> String {
    let sanitized = title
        .replace(['\r', '\n'], " ")
        .replace('"', "'")
        .replace('`', "'");
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pr_reviewer_project_url() -> &'static str {
    resolve_project_url(env!("CARGO_PKG_REPOSITORY"))
}

fn resolve_project_url(configured: &'static str) -> &'static str {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        "https://github.com/NicholaiVogel/pr-reviewer"
    } else {
        trimmed
    }
}

fn infer_pr_focus(pr_title: &str) -> &'static str {
    let tokens = title_tokens(pr_title);

    let is_fix = has_focus_keyword(
        &tokens,
        &[
            "fix",
            "fixes",
            "fixed",
            "fixing",
            "bug",
            "bugs",
            "hotfix",
            "regression",
            "issue",
            "issues",
            "patch",
            "patches",
        ],
    );
    if is_fix {
        return "the fixes";
    }

    let is_feature = has_focus_keyword(
        &tokens,
        &[
            "feat",
            "feature",
            "features",
            "implement",
            "implements",
            "implemented",
            "introduce",
            "introduces",
            "introduced",
            "add",
            "adds",
            "added",
            "support",
            "supports",
            "supported",
        ],
    );
    if is_feature {
        return "the feature work";
    }

    let is_docs = has_focus_keyword(
        &tokens,
        &["docs", "doc", "readme", "guide", "guides", "documentation"],
    );
    if is_docs {
        return "the documentation updates";
    }

    let is_refactor = has_focus_keyword(
        &tokens,
        &[
            "refactor",
            "refactors",
            "cleanup",
            "cleanups",
            "chore",
            "chores",
            "rename",
            "renames",
            "simplify",
            "simplifies",
            "simplified",
        ],
    );
    if is_refactor {
        return "the refactor and cleanup work";
    }

    "the changes"
}

fn title_tokens(pr_title: &str) -> Vec<String> {
    pr_title
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn has_focus_keyword(tokens: &[String], keywords: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| keywords.iter().any(|keyword| token == keyword))
}

/// Check if a diff should be skipped based on file types and config.
fn should_skip_review(
    changed_files: &[String],
    defaults: &crate::config::DefaultsConfig,
) -> Option<&'static str> {
    if changed_files.is_empty() {
        return Some("empty-diff");
    }

    if defaults.skip_docs_only {
        let all_docs = changed_files.iter().all(|f| {
            let lower = f.to_lowercase();
            lower.ends_with(".md")
                || lower.ends_with(".txt")
                || lower.ends_with(".rst")
                || lower.ends_with(".adoc")
                || lower.starts_with("docs/")
                || lower.starts_with("doc/")
                || lower == "license"
                || lower == "license.md"
                || lower == "license.txt"
                || lower == "changelog"
                || lower == "changelog.md"
                || lower == "changelog.txt"
                || lower == "changes"
                || lower == "changes.md"
                || lower == "changes.txt"
                || lower == "readme"
                || lower == "readme.md"
        });
        if all_docs {
            return Some("docs-only");
        }
    }

    None
}

const UI_EXTENSIONS: &[&str] = &[
    ".css", ".scss", ".less", ".svelte", ".tsx", ".jsx", ".vue", ".html",
];

fn detect_ui_files(changed_files: &[String]) -> Vec<String> {
    changed_files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            UI_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
        })
        .cloned()
        .collect()
}

fn has_screenshot_references(pr_body: Option<&str>) -> bool {
    let Some(body) = pr_body else {
        return false;
    };
    let lower = body.to_lowercase();
    lower.contains("![")
        || lower.contains("<img")
        || lower.contains(".png")
        || lower.contains(".jpg")
        || lower.contains(".jpeg")
        || lower.contains(".gif")
        || lower.contains(".webp")
        || lower.contains("screenshot")
}

fn build_durable_in_progress_comment(pr_data: &PullRequest, status_comment: &str) -> String {
    let mut body = String::new();
    body.push_str(&durable_review_marker(pr_data.number));
    body.push('\n');
    body.push_str(&format!(
        "<!-- pr-reviewer-status:in-progress pr={} sha={} -->\n\n",
        pr_data.number, pr_data.head.sha
    ));
    body.push_str(status_comment.trim());
    body.push_str("\n\n_This comment is updated in place by pr-reviewer._");
    truncate_github_comment_body(&body)
}

fn build_durable_review_comment(
    pr_data: &PullRequest,
    model: &str,
    verdict: ReviewVerdict,
    actual_event: &str,
    posted_inline: i64,
    findings_visible_to_github: bool,
    has_blocking_findings: bool,
    review_body: &str,
) -> String {
    let mut body = String::new();
    body.push_str(&durable_review_marker(pr_data.number));
    body.push('\n');
    body.push_str(&durable_verdict_marker(
        pr_data.number,
        &pr_data.head.sha,
        verdict,
        actual_event,
    ));
    body.push('\n');
    if findings_visible_to_github
        && has_blocking_findings
        && verdict == ReviewVerdict::RequestChanges
    {
        body.push_str(&format!(
            "<!-- pr-reviewer-action:fix-required pr={} sha={} finding=review-feedback -->\n",
            pr_data.number, pr_data.head.sha
        ));
    }
    body.push('\n');
    if !findings_visible_to_github {
        body.push_str(
            "GitHub could not attach the structured review, so the findings are included here.\n\n",
        );
    }
    body.push_str(review_body.trim());
    body.push_str(&format!(
        "\n\n<!-- pr-reviewer-meta model={model} event={actual_event} posted_inline={posted_inline} -->\n"
    ));
    truncate_github_comment_body(&body)
}

fn build_durable_review_status_comment(
    pr_data: &PullRequest,
    model: &str,
    verdict: ReviewVerdict,
    actual_event: &str,
    posted_inline: i64,
) -> String {
    let mut body = String::new();
    body.push_str(&durable_review_marker(pr_data.number));
    body.push('\n');
    body.push_str(&durable_verdict_marker(
        pr_data.number,
        &pr_data.head.sha,
        verdict,
        actual_event,
    ));
    body.push_str(&format!(
        "\n\nReview posted through GitHub's PR review UI for commit `{}`.",
        short_sha(&pr_data.head.sha)
    ));
    body.push_str(&format!(
        "\n\n<!-- pr-reviewer-meta model={model} event={actual_event} posted_inline={posted_inline} -->\n"
    ));
    truncate_github_comment_body(&body)
}

fn durable_review_marker(pr_number: u64) -> String {
    format!("{DURABLE_REVIEW_MARKER_PREFIX} pr={pr_number} -->")
}

fn durable_verdict_marker(
    pr_number: u64,
    sha: &str,
    verdict: ReviewVerdict,
    actual_event: &str,
) -> String {
    format!(
        "<!-- pr-reviewer-verdict:{} pr={} sha={} event={} -->",
        durable_verdict_slug(verdict),
        pr_number,
        sha,
        actual_event
    )
}

fn durable_verdict_slug(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::RequestChanges => "needs-changes",
        ReviewVerdict::NoIssues | ReviewVerdict::Approve => "no-issues",
        ReviewVerdict::Comment => "comment",
    }
}

fn find_durable_review_comment<'a>(
    issue_comments: &'a [IssueComment],
    pr_number: u64,
    bot_name: &str,
    authenticated_user: Option<&str>,
) -> Option<&'a IssueComment> {
    let marker = durable_review_marker(pr_number);
    let mut marked = issue_comments
        .iter()
        .rev()
        .filter(|comment| comment.body.contains(&marker));

    if let Some(comment) = marked
        .clone()
        .find(|comment| login_matches_bot(&comment.user.login, bot_name, authenticated_user))
    {
        return Some(comment);
    }

    marked.find(|comment| comment.body.contains("<!-- pr-reviewer-meta"))
}

fn verdict_label(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::NoIssues | ReviewVerdict::Approve => "NO_ISSUES",
        ReviewVerdict::Comment => "COMMENT",
        ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use crate::github::types::{
        CreateReviewComment, IssueComment, PullRequest, PullRequestBase, PullRequestHead,
        PullRequestReview, RepoRef, ReviewComment, User,
    };
    use crate::review::parser::{
        CommentSeverity, ConfidenceLevel, FindingKind, ReviewComment as ParsedReviewComment,
        ReviewVerdict,
    };
    use crate::store::db::ReviewFindingRecord;

    use super::{
        apply_scope_drift_policy, blocker_has_sufficient_evidence, build_direct_pushback_reply,
        build_durable_in_progress_comment, build_durable_review_comment,
        build_durable_review_status_comment, build_final_review_summary_prompt,
        build_final_review_transcript, build_finding_delta_summary,
        build_in_progress_comment_fallback, build_prior_reviews_context, build_retry_review_body,
        build_review_memory, build_review_metadata_block, build_thread_history,
        durable_review_marker, extract_in_progress_comment, find_durable_review_comment,
        finding_key, infer_pr_focus, line_is_authored_by_bot, login_matches_bot,
        looks_like_harness_error_output, looks_like_harness_transport_output,
        looks_like_reintroduction, normalize_in_progress_comment, normalize_summary_output,
        reconcile_finding_ledger, resolve_project_url, should_suppress_finding,
        truncate_github_comment_body, FindingStatus, PendingFinding, ReviewMemory,
        GITHUB_COMMENT_MAX_CHARS, GITHUB_COMMENT_TRUNCATION_NOTE,
    };

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: None,
        }
    }

    fn test_bot_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: Some("Bot".to_string()),
        }
    }

    fn test_repo_ref(full_name: &str, owner: &str) -> RepoRef {
        RepoRef {
            full_name: full_name.to_string(),
            fork: false,
            owner: test_user(owner),
            default_branch: None,
        }
    }

    fn test_pull_request_with_body(body: Option<&str>) -> PullRequest {
        PullRequest {
            number: 15,
            title: "archive review transcript".to_string(),
            body: body.map(ToString::to_string),
            draft: false,
            state: "closed".to_string(),
            user: test_user("contributor"),
            head: PullRequestHead {
                sha: "abc123".to_string(),
                ref_name: "feature/archive".to_string(),
                repo: Some(test_repo_ref("owner/repo", "owner")),
            },
            base: PullRequestBase {
                ref_name: "main".to_string(),
                repo: test_repo_ref("owner/repo", "owner"),
            },
            html_url: Some("https://github.com/owner/repo/pull/15".to_string()),
            updated_at: Some("2026-04-13T00:00:00Z".to_string()),
            closed_at: Some("2026-04-13T01:00:00Z".to_string()),
            merged_at: Some("2026-04-13T01:00:00Z".to_string()),
            stack: None,
        }
    }

    fn review_comment(
        id: u64,
        in_reply_to: Option<u64>,
        login: &str,
        path: &str,
        line: Option<u32>,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            body: body.to_string(),
            path: path.to_string(),
            line,
            user: test_user(login),
            in_reply_to_id: in_reply_to,
            created_at: format!("2026-03-30T00:{:02}:00Z", id),
        }
    }

    fn issue_comment(id: u64, login: &str, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            user: test_user(login),
            author_association: None,
            created_at: format!("2026-03-30T00:{:02}:00Z", id),
        }
    }

    fn stored_finding(status: &str, severity: &str, fingerprint: &str) -> ReviewFindingRecord {
        ReviewFindingRecord {
            repo: "owner/repo".to_string(),
            pr_number: 15,
            author: Some("alice".to_string()),
            fingerprint: fingerprint.to_string(),
            recurrence_fingerprint: Some("task-ownership".to_string()),
            first_sha: "sha-1".to_string(),
            last_seen_sha: "sha-1".to_string(),
            status: status.to_string(),
            severity: severity.to_string(),
            finding_kind: "correctness".to_string(),
            path: "src/task.rs".to_string(),
            line: Some(42),
            body: "task ownership is wrong here".to_string(),
            evidence_note: None,
            github_comment_id: None,
            resolved_by_sha: None,
            resolution_reason: None,
            created_at: "2026-03-30T00:00:00Z".to_string(),
            updated_at: "2026-03-30T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn final_review_transcript_includes_pr_description() {
        let pr = test_pull_request_with_body(Some(
            "## Goal
Ship the archive feature safely.",
        ));
        let transcript =
            build_final_review_transcript("owner/repo", &pr, "merged", &[], &[], &[], &[], &[]);

        assert!(transcript.contains("## PR Description"));
        assert!(transcript.contains("> ## Goal"));
        assert!(transcript.contains("> Ship the archive feature safely."));
    }

    #[test]
    fn final_review_summary_prompt_carries_pr_description_through_transcript() {
        let pr = test_pull_request_with_body(Some("Summarize this context too."));
        let transcript =
            build_final_review_transcript("owner/repo", &pr, "merged", &[], &[], &[], &[], &[]);
        let prompt = build_final_review_summary_prompt("owner/repo", &pr, "merged", &transcript);

        assert!(prompt.contains("Transcript:"));
        assert!(prompt.contains("## PR Description"));
        assert!(prompt.contains("> Summarize this context too."));
    }

    #[test]
    fn durable_in_progress_comment_includes_stable_marker() {
        let pr = test_pull_request_with_body(Some("Review me."));
        let body = build_durable_in_progress_comment(&pr, "Taking a look.");

        assert!(body.contains("<!-- pr-reviewer-review pr=15 -->"));
        assert!(body.contains("<!-- pr-reviewer-status:in-progress pr=15 sha=abc123 -->"));
        assert!(body.contains("Taking a look."));
    }

    #[test]
    fn durable_review_comment_emits_fix_marker_for_blocking_changes() {
        let pr = test_pull_request_with_body(Some("Review me."));
        let body = build_durable_review_comment(
            &pr,
            "gpt-5.4",
            ReviewVerdict::RequestChanges,
            "REQUEST_CHANGES",
            2,
            true,
            true,
            "summary",
        );

        assert!(body.contains("<!-- pr-reviewer-review pr=15 -->"));
        assert!(body.contains(
            "<!-- pr-reviewer-verdict:needs-changes pr=15 sha=abc123 event=REQUEST_CHANGES -->"
        ));
        assert!(body.contains(
            "<!-- pr-reviewer-action:fix-required pr=15 sha=abc123 finding=review-feedback -->"
        ));
        assert!(body.contains("\n\nsummary\n\n<!-- pr-reviewer-meta"));
        assert!(body.contains(
            "<!-- pr-reviewer-meta model=gpt-5.4 event=REQUEST_CHANGES posted_inline=2 -->"
        ));
        assert!(!body.contains("Review result"));
        assert!(!body.contains("Commit:"));
        assert!(!body.contains("inline finding"));
        assert!(!body.contains("I posted"));
        assert!(!body.contains("Updated in place"));
    }

    #[test]
    fn durable_status_comment_does_not_mirror_review_body() {
        let pr = test_pull_request_with_body(Some("Review me."));
        let body = build_durable_review_status_comment(
            &pr,
            "gpt-5.4",
            ReviewVerdict::Comment,
            "COMMENT",
            2,
        );

        assert!(body.contains("<!-- pr-reviewer-review pr=15 -->"));
        assert!(
            body.contains("<!-- pr-reviewer-verdict:comment pr=15 sha=abc123 event=COMMENT -->")
        );
        assert!(body.contains("Review posted through GitHub's PR review UI"));
        assert!(body.contains("commit `abc123`"));
        assert!(
            body.contains("<!-- pr-reviewer-meta model=gpt-5.4 event=COMMENT posted_inline=2 -->")
        );
        assert!(!body.contains("Review metadata"));
        assert!(!body.contains("summary"));
        assert!(!body.contains("Confidence:"));
    }

    #[test]
    fn review_metadata_block_is_collapsed() {
        let body = build_review_metadata_block("gpt-5.3-codex", "e6f9a1e8");

        assert!(body.starts_with("<details>\n<summary>Review metadata</summary>"));
        assert!(body.contains("- Reviewer: [pr-reviewer]("));
        assert!(body.contains("- Model: `gpt-5.3-codex`"));
        assert!(body.contains("- Commit: `e6f9a1e8`"));
        assert!(!body.contains("> Automated review by"));
        assert!(body.ends_with("</details>\n\n"));
    }

    #[test]
    fn durable_comment_lookup_prefers_latest_bot_marker() {
        let marker = durable_review_marker(15);
        let comments = vec![
            issue_comment(1, "contributor", &marker),
            issue_comment(2, "pr-reviewer", &marker),
            issue_comment(3, "pr-reviewer", &marker),
        ];

        let found = find_durable_review_comment(&comments, 15, "pr-reviewer", None)
            .expect("bot marker comment");

        assert_eq!(found.id, 3);
    }

    #[test]
    fn durable_comment_lookup_recovers_stale_bot_alias_by_meta_marker() {
        let marker = durable_review_marker(15);
        let comments = vec![issue_comment(
            1,
            "PR-Reviewer-Ant",
            &format!(
                "{marker}\n<!-- pr-reviewer-meta model=gpt-5.5 event=COMMENT posted_inline=1 -->"
            ),
        )];

        let found = find_durable_review_comment(&comments, 15, "pr-reviewer", None)
            .expect("durable pr-reviewer marker comment");

        assert_eq!(found.id, 1);
    }

    #[test]
    fn final_review_transcript_includes_finding_resolution_state() {
        let pr = test_pull_request_with_body(None);
        let findings = vec![stored_finding("likely_fixed", "blocking", "fp-1")];
        let transcript = build_final_review_transcript(
            "owner/repo",
            &pr,
            "merged",
            &[],
            &[],
            &[],
            &[],
            &findings,
        );

        assert!(transcript.contains("## Finding Resolution State"));
        assert!(transcript.contains("[likely_fixed / blocking / correctness]"));
    }

    #[test]
    fn summary_output_ignores_stderr_when_stdout_is_empty() {
        let normalized = normalize_summary_output("", "claude diagnostic: auth warning");

        assert!(normalized.is_none());
    }

    #[test]
    fn summary_output_strips_fenced_stdout() {
        let normalized = normalize_summary_output(
            "```markdown
Reviewed the archive flow.
```",
            "ignored warning",
        )
        .expect("stdout should be normalized");

        assert_eq!(normalized, "Reviewed the archive flow.");
    }

    #[test]
    fn first_touch_comment_avoids_reintroduction() {
        let message = build_in_progress_comment_fallback(
            "contributor",
            "fix race condition in queue",
            "527fae59abcde",
            false,
        );

        assert!(message.contains("Hi @contributor"));
        assert!(message.contains("commit `527fae59`"));
        assert!(!message.contains("automated code reviewer"));
        assert!(!message.contains("[pr-reviewer]("));
    }

    #[test]
    fn follow_up_comment_is_conversational_without_reintro() {
        let message = build_in_progress_comment_fallback(
            "contributor",
            "fix race condition in queue",
            "527fae59abcde",
            true,
        );

        assert!(message.contains("quick follow-up pass"));
        assert!(!message.contains("PR-reviewing agent"));
        assert!(!message.contains("[pr-reviewer]("));
    }

    #[test]
    fn fallback_comment_sanitizes_double_quotes_in_title() {
        let message =
            build_in_progress_comment_fallback("contributor", "fix \"the\" bug", "527fae59", false);
        assert!(message.contains("`fix 'the' bug`"));
    }

    #[test]
    fn fallback_comment_sanitizes_backticks_in_title() {
        let message =
            build_in_progress_comment_fallback("contributor", "fix `foo` crash", "527fae59", false);
        assert!(message.contains("`fix 'foo' crash`"));
    }

    #[test]
    fn fallback_comment_neutralizes_markdown_links_in_title() {
        let message = build_in_progress_comment_fallback(
            "contributor",
            "docs [click](https://example.com)",
            "527fae59",
            false,
        );
        assert!(message.contains("`docs [click](https://example.com)`"));
    }

    #[test]
    fn fallback_comment_normalizes_newlines_in_title() {
        let message = build_in_progress_comment_fallback(
            "contributor",
            "fix quote\nand link\r\nrendering",
            "527fae59",
            false,
        );
        assert!(message.contains("`fix quote and link rendering`"));
        assert!(!message.contains('\n'));
        assert!(!message.contains('\r'));
    }

    #[test]
    fn parses_marked_json_in_progress_comment() {
        let output = "```pr-review-in-progress-json\n{\"comment\":\"Hi @you - quick pass on commit `abc12345`.\"}\n```";
        let parsed = extract_in_progress_comment(output, "").expect("parsed comment");
        assert_eq!(parsed, "Hi @you - quick pass on commit `abc12345`.");
    }

    #[test]
    fn rejects_unstructured_raw_in_progress_comment_output() {
        assert!(extract_in_progress_comment("just some raw text", "").is_none());
    }

    #[test]
    fn normalizes_generated_comment_whitespace() {
        let normalized = normalize_in_progress_comment("  hi there \n  doing a pass  ").unwrap();
        assert_eq!(normalized, "hi there doing a pass");
    }

    #[test]
    fn truncates_github_comment_body_to_limit() {
        let body = "a".repeat(GITHUB_COMMENT_MAX_CHARS + 100);
        let truncated = truncate_github_comment_body(&body);

        assert_eq!(truncated.chars().count(), GITHUB_COMMENT_MAX_CHARS);
        assert!(truncated.ends_with(GITHUB_COMMENT_TRUNCATION_NOTE));
    }

    #[test]
    fn retry_review_body_omits_extra_inline_comments_to_fit_limit() {
        let base = "intro\n".repeat(5000);
        let inline_comments = vec![
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 10,
                side: "RIGHT".to_string(),
                body: "x".repeat(20_000),
            },
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 20,
                side: "RIGHT".to_string(),
                body: "y".repeat(20_000),
            },
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 30,
                side: "RIGHT".to_string(),
                body: "z".repeat(20_000),
            },
        ];

        let retry_body = build_retry_review_body(&base, &inline_comments);

        assert!(retry_body.chars().count() <= GITHUB_COMMENT_MAX_CHARS);
        assert!(retry_body.contains("Inline comments (could not post as line comments):"));
        assert!(retry_body.contains("Omitted"));
    }

    #[test]
    fn reintroduction_detector_flags_tool_intro() {
        assert!(looks_like_reintroduction(
            "I'm @octocat's PR-reviewing agent powered by [pr-reviewer](https://example.com)."
        ));
    }

    #[test]
    fn harness_error_output_is_rejected() {
        assert!(looks_like_harness_error_output(
            "Error: command returned exit status 1"
        ));
    }

    #[test]
    fn harness_transport_output_is_rejected() {
        assert!(looks_like_harness_transport_output(
            "{\"type\":\"thread.started\",\"thread_id\":\"abc\"}\n{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\"}}"
        ));
        assert!(looks_like_harness_transport_output(
            "/home/nicholai/.config/signet/bin/codex: line 6: HOME: unbound variable"
        ));
    }

    #[test]
    fn login_match_checks_configured_and_authenticated_names() {
        assert!(login_matches_bot(
            "pr-reviewer",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
        assert!(login_matches_bot(
            "NicholaiVogel",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
        assert!(!login_matches_bot(
            "someone-else",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
    }

    #[test]
    fn project_url_uses_fallback_when_empty() {
        assert_eq!(
            resolve_project_url(""),
            "https://github.com/NicholaiVogel/pr-reviewer"
        );
    }

    #[test]
    fn infer_focus_uses_title_keywords() {
        assert_eq!(infer_pr_focus("fix flaky test"), "the fixes");
        assert_eq!(
            infer_pr_focus("feat: add metrics dashboard"),
            "the feature work"
        );
        assert_eq!(
            infer_pr_focus("docs: update README"),
            "the documentation updates"
        );
        assert_eq!(
            infer_pr_focus("refactor parser internals"),
            "the refactor and cleanup work"
        );
        assert_eq!(infer_pr_focus("misc updates"), "the changes");
    }

    #[test]
    fn infer_focus_avoids_substring_false_positives() {
        assert_eq!(infer_pr_focus("address logging"), "the changes");
        assert_eq!(infer_pr_focus("dismissal policy"), "the changes");
        assert_eq!(infer_pr_focus("resolve review comments"), "the changes");
    }

    #[test]
    fn prior_reviews_context_uses_alias_match() {
        let reviews = vec![PullRequestReview {
            id: 1,
            body: Some("looks good".to_string()),
            user: test_user("NicholaiVogel"),
            state: Some("COMMENTED".to_string()),
            commit_id: Some("previoussha".to_string()),
            submitted_at: Some("2026-03-17T00:00:00Z".to_string()),
        }];

        let context = build_prior_reviews_context(
            &reviews,
            &[],
            &[],
            "pr-reviewer",
            Some("NicholaiVogel"),
            "currentsha",
        );
        assert!(context.contains("looks good"));
    }

    #[test]
    fn build_review_memory_marks_explicit_scope_boundaries() {
        let review_comments = vec![
            ReviewComment {
                id: 10,
                body: "Blocking: task ownership is wrong here".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(42),
                user: test_user("pr-reviewer"),
                in_reply_to_id: None,
                created_at: "2026-03-30T00:00:00Z".to_string(),
            },
            ReviewComment {
                id: 11,
                body: "not taking further task ownership feedback in this PR".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(42),
                user: test_user("NicholaiVogel"),
                in_reply_to_id: Some(10),
                created_at: "2026-03-30T00:01:00Z".to_string(),
            },
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            None,
            &HashMap::new(),
        );
        assert_eq!(memory.findings.len(), 1);
        assert_eq!(memory.findings[0].status, FindingStatus::OutOfScopeForPr);
        assert_eq!(memory.explicit_boundaries.len(), 1);
    }

    #[test]
    fn build_review_memory_marks_unmentioned_findings_as_likely_addressed() {
        let review_comments = vec![ReviewComment {
            id: 10,
            body: "Potential blocking issue in task cleanup".to_string(),
            path: "src/task.rs".to_string(),
            line: Some(42),
            user: test_user("pr-reviewer"),
            in_reply_to_id: None,
            created_at: "2026-03-30T00:00:00Z".to_string(),
        }];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            Some(&["src/other.rs".to_string()]),
            &HashMap::new(),
        );

        assert_eq!(memory.findings[0].status, FindingStatus::LikelyAddressed);
    }

    #[test]
    fn suppresses_repeated_out_of_scope_finding_without_new_evidence() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key(
                    "src/task.rs",
                    Some(42),
                    "Blocking: task ownership is wrong here",
                ),
                path: "src/task.rs".to_string(),
                line: Some(42),
                body: "Blocking: task ownership is wrong here".to_string(),
                token_set: super::token_set("Blocking: task ownership is wrong here"),
                status: FindingStatus::OutOfScopeForPr,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![
                "not taking further task ownership feedback in this PR".to_string()
            ],
        };

        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 42,
            body: "Blocking: task ownership is wrong here".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(should_suppress_finding(&comment, "src/task.rs", &memory));
    }

    #[test]
    fn allows_reopening_rebutted_finding_with_evidence_note() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key("src/task.rs", Some(42), "task ownership is wrong here"),
                path: "src/task.rs".to_string(),
                line: Some(42),
                body: "task ownership is wrong here".to_string(),
                token_set: super::token_set("task ownership is wrong here"),
                status: FindingStatus::RejectedWithRationale,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![],
        };

        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 50,
            body: "Blocking: task ownership is still wrong on the new trigger path".to_string(),
            evidence_note: Some(
                "new trigger path at src/task.rs:50 now routes cross-agent writes".to_string(),
            ),
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(!should_suppress_finding(&comment, "src/task.rs", &memory));
    }

    #[test]
    fn weak_blocker_is_downgraded_without_evidence() {
        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 10,
            body: "Blocking: this might be a problem later.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/task.rs",
            ConfidenceLevel::Low
        ));
    }

    #[test]
    fn scope_drift_without_artifact_or_secret_risk_is_downgraded() {
        let mut comment = ParsedReviewComment {
            file: "src/cli.rs".to_string(),
            line: 10,
            body: "PR description does not mention this related CLI flag.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::ScopeDrift,
        };

        apply_scope_drift_policy(&mut comment);

        assert_eq!(comment.severity, CommentSeverity::Warning);
    }

    #[test]
    fn scope_drift_with_generated_artifact_risk_stays_blocking() {
        let mut comment = ParsedReviewComment {
            file: "packages/app/.svelte-kit/generated/client/nodes/1.js".to_string(),
            line: 1,
            body: "Generated build artifacts are committed in an unrelated PR.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::ScopeDrift,
        };

        apply_scope_drift_policy(&mut comment);

        assert_eq!(comment.severity, CommentSeverity::Blocking);
    }

    #[test]
    fn reconcile_marks_repeated_blocker_as_still_blocking() {
        let fingerprint = finding_key("src/task.rs", Some(42), "task ownership is wrong here");
        let stored = vec![stored_finding("open", "blocking", &fingerprint)];
        let current = vec![PendingFinding {
            fingerprint: fingerprint.clone(),
            path: "src/task.rs".to_string(),
            line: Some(42),
            body: "task ownership is wrong here".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
            kind: super::PendingKind::Inline,
        }];
        let memory = ReviewMemory::default();

        let (updates, delta, _) = reconcile_finding_ledger(
            "owner/repo",
            15,
            "sha-2",
            "alice",
            &stored,
            &current,
            &memory,
            &HashSet::new(),
            None,
        );

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, "still_blocking");
        assert_eq!(delta.still_blocking, 1);
        assert!(build_finding_delta_summary(&delta)
            .unwrap()
            .contains("Still blocking: 1"));
    }

    #[test]
    fn reconcile_marks_missing_open_finding_as_likely_fixed() {
        let stored = vec![stored_finding("open", "blocking", "fp-1")];
        let memory = ReviewMemory::default();

        let (updates, delta, _) = reconcile_finding_ledger(
            "owner/repo",
            15,
            "sha-2",
            "alice",
            &stored,
            &[],
            &memory,
            &HashSet::new(),
            None,
        );

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, "likely_fixed");
        assert_eq!(updates[0].resolved_by_sha.as_deref(), Some("sha-2"));
        assert_eq!(delta.likely_fixed, 1);
    }

    #[test]
    fn direct_pushback_reply_acknowledges_boundary() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: i'm not taking further task ownership feedback in this PR",
            "pr-reviewer",
            None,
        )
        .expect("reply");
        assert!(reply.contains("won't keep pushing"));
    }

    #[test]
    fn direct_pushback_reply_detects_bot_mention_format() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: thanks @pr-reviewer",
            "pr-reviewer",
            None,
        );

        assert!(reply.is_none());
    }

    // --- Regression tests: blocker evidence gate ---

    #[test]
    fn blocker_with_causal_phrase_passes_evidence_gate() {
        let comment = ParsedReviewComment {
            file: "src/engine.rs".to_string(),
            line: 100,
            body: "This will panic when the input is empty because unwrap() is called on None"
                .to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/engine.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_with_bare_will_no_consequence_fails_evidence_gate() {
        // "will" alone without a consequence verb should NOT pass
        let comment = ParsedReviewComment {
            file: "src/engine.rs".to_string(),
            line: 100,
            body: "This function will need refactoring eventually for maintainability.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/engine.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_referencing_file_path_passes_evidence_gate() {
        let comment = ParsedReviewComment {
            file: "src/config.rs".to_string(),
            line: 55,
            body: "The default in src/config.rs contradicts the documented behavior".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/config.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_with_evidence_note_always_passes() {
        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 10,
            body: "something vague".to_string(),
            evidence_note: Some("line 10 assigns None then unwraps at line 12".to_string()),
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/task.rs",
            ConfidenceLevel::Low,
        ));
    }

    #[test]
    fn blocker_with_sql_injection_phrase_passes() {
        let comment = ParsedReviewComment {
            file: "src/db.rs".to_string(),
            line: 30,
            body: "User input is interpolated directly into the query, creating a sql injection vector".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/db.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_low_confidence_no_evidence_fails() {
        let comment = ParsedReviewComment {
            file: "src/db.rs".to_string(),
            line: 30,
            body: "This will panic and crash and burn".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/db.rs",
            ConfidenceLevel::Low,
        ));
    }

    // --- Regression tests: bot-mention guard ---

    #[test]
    fn bot_mention_guard_ignores_bots_own_text() {
        // Bot's own comment references @pr-reviewer, but a human said "not in scope"
        // The guard should NOT bail just because the bot mentioned itself
        let history = "- [2026-03-30] pr-reviewer: see @pr-reviewer docs for details\n\
                        - [2026-03-30] NicholaiVogel: this is out of scope for this PR";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_some(), "should produce a pushback reply");
        assert!(reply.unwrap().contains("won't keep pushing"));
    }

    #[test]
    fn bot_mention_guard_fires_on_human_mention() {
        // Human explicitly @-mentions the bot, expecting a substantive response
        let history = "- [2026-03-30] pr-reviewer: this code has a bug\n\
                        - [2026-03-30] NicholaiVogel: @pr-reviewer can you re-check this?";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_none(), "should bail when human mentions the bot");
    }

    #[test]
    fn bot_mention_guard_checks_alias() {
        let history = "- [2026-03-30] NicholaiVogel: @PR-Reviewer-Ant please look again";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", Some("PR-Reviewer-Ant"));
        assert!(
            reply.is_none(),
            "should bail when human mentions the bot alias"
        );
    }

    #[test]
    fn bot_mention_guard_alias_in_bot_line_ignored() {
        let history = "- [2026-03-30] PR-Reviewer-Ant: reviewed by @PR-Reviewer-Ant\n\
                        - [2026-03-30] NicholaiVogel: nah, out of scope";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", Some("PR-Reviewer-Ant"));
        assert!(
            reply.is_some(),
            "bot's own alias mention should not trigger the guard"
        );
    }

    // --- Regression tests: status precedence in build_review_memory ---

    #[test]
    fn status_precedence_keeps_strongest_signal() {
        // RejectedWithRationale (3) > DismissedByHuman (2)
        // If reply #1 is rationale rejection and reply #2 is dismissal,
        // the final status should remain RejectedWithRationale
        use super::status_precedence;

        assert!(
            status_precedence(&FindingStatus::RejectedWithRationale)
                > status_precedence(&FindingStatus::DismissedByHuman)
        );
        assert!(
            status_precedence(&FindingStatus::OutOfScopeForPr)
                > status_precedence(&FindingStatus::RejectedWithRationale)
        );
        assert!(
            status_precedence(&FindingStatus::DismissedByHuman)
                > status_precedence(&FindingStatus::LikelyAddressed)
        );
    }

    #[test]
    fn build_review_memory_rationale_not_downgraded_by_later_dismissal() {
        // Simulates: reply #1 has rationale rejection, reply #2 is a soft dismissal.
        // Final status must be RejectedWithRationale (stronger), not DismissedByHuman.
        let review_comments = vec![
            review_comment(
                1,
                None,
                "pr-reviewer",
                "src/task.rs",
                Some(10),
                "Bug: wrong logic here",
            ),
            review_comment(
                2,
                Some(1),
                "NicholaiVogel",
                "src/task.rs",
                Some(10),
                "this is a false positive, the spec requires this behavior",
            ),
            review_comment(
                3,
                Some(1),
                "AnotherDev",
                "src/task.rs",
                Some(10),
                "yeah just leave it, it's fine",
            ),
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head-sha",
            Some(&["src/task.rs".to_string()]),
            &HashMap::new(),
        );

        assert_eq!(memory.findings.len(), 1);
        assert_eq!(
            memory.findings[0].status,
            FindingStatus::RejectedWithRationale,
            "stronger rationale rejection should not be downgraded by a later soft dismissal"
        );
    }

    // --- Regression tests: LikelyAddressed ---

    #[test]
    fn finding_on_unchanged_file_is_likely_addressed() {
        let review_comments = vec![review_comment(
            1,
            None,
            "pr-reviewer",
            "src/old.rs",
            Some(10),
            "Bug found here",
        )];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "new-sha",
            Some(&["src/new.rs".to_string()]),
            &HashMap::new(),
        );

        assert_eq!(memory.findings.len(), 1);
        assert_eq!(memory.findings[0].status, FindingStatus::LikelyAddressed);
    }

    #[test]
    fn likely_addressed_finding_is_suppressed() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key("src/old.rs", Some(10), "Bug found here"),
                path: "src/old.rs".to_string(),
                line: Some(10),
                body: "Bug found here".to_string(),
                token_set: super::token_set("Bug found here"),
                status: FindingStatus::LikelyAddressed,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![],
        };

        let comment = ParsedReviewComment {
            file: "src/old.rs".to_string(),
            line: 10,
            body: "Bug found here".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Warning,
            finding_kind: FindingKind::Correctness,
        };

        assert!(should_suppress_finding(&comment, "src/old.rs", &memory));
    }

    // --- Regression test: line_is_authored_by_bot ---

    #[test]
    fn line_is_authored_by_bot_parses_thread_format() {
        use super::line_is_authored_by_bot;

        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer: some review text",
            "pr-reviewer",
            None,
        ));
        assert!(!line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] nicholaivogel: some comment",
            "pr-reviewer",
            None,
        ));
        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer-ant: automated review",
            "pr-reviewer",
            Some("pr-reviewer-ant"),
        ));
        assert!(!line_is_authored_by_bot(
            "no bracket line",
            "pr-reviewer",
            None,
        ));
    }

    // --- Regression test: pushback reply for dismissal signal ---

    #[test]
    fn direct_pushback_reply_acknowledges_dismissal() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: nah, this is intentional, leave it",
            "pr-reviewer",
            None,
        )
        .expect("should produce reply");
        assert!(reply.contains("treating that as intentional"));
    }

    #[test]
    fn direct_pushback_reply_returns_none_for_neutral_comment() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: interesting, let me think about it",
            "pr-reviewer",
            None,
        );
        assert!(
            reply.is_none(),
            "neutral comments should not trigger a canned reply"
        );
    }

    // --- Multi-bot safeguard tests ---

    #[test]
    fn user_is_bot_detects_github_bot_accounts() {
        let bot = test_bot_user("BusyBee3333");
        assert!(bot.is_bot());

        let human = test_user("NicholaiVogel");
        assert!(!human.is_bot());

        let unknown = User {
            login: "someone".to_string(),
            account_type: None,
        };
        assert!(
            !unknown.is_bot(),
            "None account_type should not be treated as bot"
        );
    }

    #[test]
    fn build_thread_history_labels_bot_comments() {
        let comments = vec![
            ReviewComment {
                id: 1,
                body: "Bug: wrong logic here".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_bot_user("BusyBee3333"),
                in_reply_to_id: None,
                created_at: "2026-03-30T04:16:00Z".to_string(),
            },
            ReviewComment {
                id: 2,
                body: "I disagree, this is intentional".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_user("NicholaiVogel"),
                in_reply_to_id: Some(1),
                created_at: "2026-03-30T04:17:00Z".to_string(),
            },
        ];

        let history = build_thread_history(&comments, 1);
        assert!(
            history.contains("BusyBee3333 (bot):"),
            "bot comments should be labeled: {history}"
        );
        assert!(
            !history.contains("NicholaiVogel (bot)"),
            "human comments should not be labeled as bot: {history}"
        );
        assert!(history.contains("NicholaiVogel:"));
    }

    #[test]
    fn line_is_authored_by_bot_handles_bot_label_suffix() {
        // The new format includes "(bot)" for bot accounts
        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer (bot): some review text",
            "pr-reviewer",
            None,
        ));
        assert!(!line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] nicholaivogel: some comment",
            "pr-reviewer",
            None,
        ));
    }

    #[test]
    fn bot_mention_guard_ignores_bot_labeled_lines() {
        // Bot's own comment has (bot) label and references @pr-reviewer,
        // but a human said "out of scope". Guard should not bail.
        let history = "- [2026-03-30] pr-reviewer (bot): see @pr-reviewer docs for details\n\
                        - [2026-03-30] NicholaiVogel: this is out of scope for this PR";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(
            reply.is_some(),
            "should produce a pushback reply despite bot's own @mention"
        );
    }

    #[test]
    fn build_review_memory_treats_other_bot_as_non_human() {
        // When another bot (BusyBee3333) replies to our finding, it should not
        // be treated as a human dismissal. Only this bot's own comments and
        // genuine human comments should influence finding status.
        //
        // Note: currently build_review_memory uses login_matches_bot which only
        // knows about *this* bot's identity. Other bots' comments are treated as
        // human replies. This test documents the current behavior and would need
        // updating if we add peer-bot awareness to build_review_memory.
        let review_comments = vec![
            review_comment(1, None, "pr-reviewer", "src/task.rs", Some(10), "Bug here"),
            // Another bot says "fine" (a dismissal signal)
            ReviewComment {
                id: 2,
                body: "fine, this looks acceptable".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_bot_user("BusyBee3333"),
                in_reply_to_id: Some(1),
                created_at: "2026-03-30T00:02:00Z".to_string(),
            },
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head-sha",
            Some(&["src/task.rs".to_string()]),
            &HashMap::new(),
        );

        // Currently other bots ARE treated as human. This test documents
        // that behavior so we notice if/when we change it.
        assert_eq!(memory.findings.len(), 1);
        assert_eq!(
            memory.findings[0].status,
            FindingStatus::DismissedByHuman,
            "other bot comments are currently classified as human replies (known limitation)"
        );
    }

    #[test]
    fn signal_scan_ignores_bots_own_review_text() {
        // The bot's previous review body might contain phrases like "not correct"
        // or "on purpose" as part of its analysis. These should NOT trigger
        // a canned pushback reply when a human makes a neutral comment afterward.
        let history = "- [2026-03-30] pr-reviewer: this logic is not correct and the pattern is intentional duplication\n\
                        - [2026-03-30] NicholaiVogel: interesting, let me look at this more closely";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(
            reply.is_none(),
            "bot's own text containing 'not correct' or 'intentional' should not trigger a canned reply"
        );
    }

    #[test]
    fn signal_scan_still_fires_on_human_dismissal_with_bot_noise() {
        // Even when the bot's own text contains signal words, a genuine human
        // dismissal should still be detected.
        let history = "- [2026-03-30] pr-reviewer: this is not correct behavior\n\
                        - [2026-03-30] NicholaiVogel: nah, it's fine, leave it";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_some(), "human dismissal should still be detected");
        assert!(reply.unwrap().contains("treating that as intentional"));
    }

    // ---- Review lifecycle regression tests (review cycles, dedupe, stacks) ----

    fn pending(
        path: &str,
        line: u32,
        body: &str,
        severity: CommentSeverity,
        noted: bool,
    ) -> PendingFinding {
        let kind = if noted {
            super::PendingKind::Note
        } else {
            super::PendingKind::Inline
        };
        PendingFinding {
            fingerprint: finding_key(path, Some(line), body),
            path: path.to_string(),
            line: Some(line),
            body: body.to_string(),
            evidence_note: None,
            severity,
            finding_kind: FindingKind::Correctness,
            kind,
        }
    }

    /// Stored rows as `upsert_review_findings` would leave them.
    fn as_stored(updates: &[crate::store::db::ReviewFindingUpsert]) -> Vec<ReviewFindingRecord> {
        updates
            .iter()
            .map(|u| ReviewFindingRecord {
                repo: u.repo.clone(),
                pr_number: u.pr_number,
                author: u.author.clone(),
                fingerprint: u.fingerprint.clone(),
                recurrence_fingerprint: u.recurrence_fingerprint.clone(),
                first_sha: u.sha.clone(),
                last_seen_sha: u.sha.clone(),
                status: u.status.clone(),
                severity: u.severity.clone(),
                finding_kind: u.finding_kind.clone(),
                path: u.path.clone(),
                line: u.line,
                body: u.body.clone(),
                evidence_note: u.evidence_note.clone(),
                github_comment_id: u.github_comment_id,
                resolved_by_sha: u.resolved_by_sha.clone(),
                resolution_reason: u.resolution_reason.clone(),
                created_at: String::new(),
                updated_at: String::new(),
            })
            .collect()
    }

    const BLOCKER: &str = "The retry loop in `fetch_all` never sleeps, so a 429 from GitHub spins the CPU and burns the whole rate limit in seconds.";
    const BLOCKER_REWORDED: &str = "Still present: `fetch_all` retries a 429 from GitHub without sleeping, so the loop spins and burns the rate limit in seconds.";
    const WARNING: &str = "The cache key in `cache.rs` ignores the token scope, so two tokens with different repo access can read each other's cached responses.";

    fn paths(items: &[&str]) -> HashSet<String> {
        items.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn lifecycle_initial_review_then_fix_then_push() {
        let memory = ReviewMemory::default();
        // Round 1: two findings on two files.
        let (round1, delta1, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-1",
            "alice",
            &[],
            &[
                pending(
                    "src/fetch.rs",
                    40,
                    BLOCKER,
                    CommentSeverity::Blocking,
                    false,
                ),
                pending("src/cache.rs", 12, WARNING, CommentSeverity::Warning, false),
            ],
            &memory,
            &paths(&["src/fetch.rs", "src/cache.rs"]),
            None,
        );
        assert_eq!(delta1.newly_found, 2);
        let stored = as_stored(&round1);

        // Push 2 fixes fetch.rs only. The model reports nothing.
        let (round2, delta2, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-2",
            "alice",
            &stored,
            &[],
            &memory,
            &paths(&["src/fetch.rs", "src/cache.rs"]),
            Some(&paths(&["src/fetch.rs"])),
        );
        assert_eq!(
            delta2.likely_fixed, 1,
            "only the finding in the changed file is resolved"
        );
        assert_eq!(round2.len(), 1);
        assert_eq!(round2[0].path, "src/fetch.rs");
        assert_eq!(round2[0].status, "likely_fixed");
        assert_eq!(
            round2[0].resolution_reason.as_deref(),
            Some("not re-emitted after its file changed")
        );
    }

    #[test]
    fn lifecycle_reworded_re_raise_updates_the_existing_row() {
        let memory = ReviewMemory::default();
        let (round1, _, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-1",
            "alice",
            &[],
            &[pending(
                "src/fetch.rs",
                40,
                BLOCKER,
                CommentSeverity::Blocking,
                false,
            )],
            &memory,
            &paths(&["src/fetch.rs"]),
            None,
        );
        let stored = as_stored(&round1);
        // The next push shifted lines; the model re-raises the same problem.
        let (round2, delta, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-2",
            "alice",
            &stored,
            &[pending(
                "src/fetch.rs",
                47,
                BLOCKER_REWORDED,
                CommentSeverity::Blocking,
                false,
            )],
            &memory,
            &paths(&["src/fetch.rs"]),
            Some(&paths(&["src/fetch.rs"])),
        );
        assert_eq!(delta.newly_found, 0, "a re-raised finding is not new");
        assert_eq!(delta.still_blocking, 1);
        assert_eq!(
            round2.len(),
            1,
            "no second row, and the old row is not resolved"
        );
        assert_eq!(round2[0].fingerprint, stored[0].fingerprint);
        assert_eq!(round2[0].status, "still_blocking");
    }

    #[test]
    fn lifecycle_stack_rebase_does_not_resolve_findings_in_untouched_files() {
        // A cascade rebase brings only the parent's file into the compare diff.
        let stored = vec![ReviewFindingRecord {
            path: "src/child.rs".to_string(),
            ..stored_finding("open", "blocking", "child|1|x")
        }];
        let (updates, delta, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-rebased",
            "alice",
            &stored,
            &[],
            &ReviewMemory::default(),
            &paths(&["src/child.rs"]),
            Some(&paths(&["src/parent.rs"])),
        );
        assert!(updates.is_empty(), "{updates:?}");
        assert_eq!(delta.likely_fixed, 0);
    }

    #[test]
    fn follow_up_notes_are_recorded_inactive_and_not_counted_as_new() {
        let (updates, delta, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-3",
            "alice",
            &[],
            &[pending(
                "src/cache.rs",
                12,
                WARNING,
                CommentSeverity::Warning,
                true,
            )],
            &ReviewMemory::default(),
            &paths(&["src/cache.rs"]),
            None,
        );
        assert_eq!(updates[0].status, "noted");
        assert_eq!(delta.newly_found, 0);
        assert!(!super::finding_status_is_active("noted"));
    }

    #[test]
    fn resolved_threads_are_remembered_but_rebuttals_win() {
        let comments = vec![
            review_comment(1, None, "pr-reviewer", "src/fetch.rs", Some(40), BLOCKER),
            review_comment(2, None, "pr-reviewer", "src/cache.rs", Some(12), WARNING),
            review_comment(
                3,
                Some(2),
                "alice",
                "src/cache.rs",
                Some(12),
                "This is intentional, won't fix.",
            ),
        ];
        let resolved = HashMap::from([(1, true), (2, true)]);
        let memory = build_review_memory(
            &[],
            &comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            Some(&["src/fetch.rs".to_string(), "src/cache.rs".to_string()]),
            &resolved,
        );
        let status = |path: &str| {
            memory
                .findings
                .iter()
                .find(|f| f.path == path)
                .unwrap()
                .status
        };
        assert_eq!(status("src/fetch.rs"), FindingStatus::ResolvedByHuman);
        assert_ne!(
            status("src/cache.rs"),
            FindingStatus::ResolvedByHuman,
            "an explicit human rebuttal is more specific than resolving"
        );
    }

    #[test]
    fn dismissed_finding_no_longer_hides_unrelated_findings_in_the_same_file() {
        let comments = vec![
            review_comment(1, None, "pr-reviewer", "src/fetch.rs", Some(40), BLOCKER),
            review_comment(
                2,
                Some(1),
                "alice",
                "src/fetch.rs",
                Some(40),
                "This is intentional: the caller already sleeps between pages.",
            ),
        ];
        let memory = build_review_memory(
            &[],
            &comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            Some(&["src/fetch.rs".to_string()]),
            &HashMap::new(),
        );
        let reraised = ParsedReviewComment {
            file: "src/fetch.rs".to_string(),
            line: 41,
            body: BLOCKER_REWORDED.to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
            finding_kind: FindingKind::Correctness,
        };
        // Shares "fetch_all", "github" and "rate" with the dismissed finding
        // but is a different problem; the old two-token rule dropped it.
        let unrelated = ParsedReviewComment {
            body: "`fetch_all` builds the GitHub URL from the raw branch name, so a branch containing `#` truncates the request and the rate counter is never updated.".to_string(),
            line: 90,
            ..reraised.clone()
        };
        assert!(should_suppress_finding(&reraised, "src/fetch.rs", &memory));
        assert!(!should_suppress_finding(
            &unrelated,
            "src/fetch.rs",
            &memory
        ));
    }

    #[test]
    fn ledger_prefers_exact_rows_and_keeps_active_rows_active() {
        let memory = ReviewMemory::default();
        let (round1, _, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-1",
            "alice",
            &[],
            &[pending(
                "src/fetch.rs",
                40,
                BLOCKER,
                CommentSeverity::Blocking,
                false,
            )],
            &memory,
            &paths(&["src/fetch.rs"]),
            None,
        );
        let stored = as_stored(&round1);

        // Past the limit the model re-reports the open blocker; the policy
        // files it as a note. It must stay an active blocker in the ledger.
        let (updates, delta, fingerprints) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-2",
            "alice",
            &stored,
            &[pending(
                "src/fetch.rs",
                44,
                BLOCKER_REWORDED,
                CommentSeverity::Blocking,
                true,
            )],
            &memory,
            &paths(&["src/fetch.rs"]),
            Some(&paths(&["src/fetch.rs"])),
        );
        assert_eq!(updates[0].status, "still_blocking");
        assert_eq!(delta.still_blocking, 1);
        assert_eq!(
            fingerprints,
            vec![stored[0].fingerprint.clone()],
            "posted-state updates must target the stored row"
        );
    }

    #[test]
    fn ledger_exact_match_is_not_stolen_by_a_fuzzy_match() {
        let memory = ReviewMemory::default();
        let original = pending(
            "src/fetch.rs",
            40,
            BLOCKER,
            CommentSeverity::Blocking,
            false,
        );
        let (round1, _, _) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-1",
            "alice",
            &[],
            &[original.clone()],
            &memory,
            &paths(&["src/fetch.rs"]),
            None,
        );
        let stored = as_stored(&round1);
        // The reworded copy comes first, the exact copy second: the exact one
        // keeps the row, and the reworded one becomes a separate finding.
        let reworded = pending(
            "src/fetch.rs",
            41,
            BLOCKER_REWORDED,
            CommentSeverity::Blocking,
            false,
        );
        let (_, delta, fingerprints) = reconcile_finding_ledger(
            "o/r",
            7,
            "sha-2",
            "alice",
            &stored,
            &[reworded, original.clone()],
            &memory,
            &paths(&["src/fetch.rs"]),
            None,
        );
        assert_eq!(fingerprints[1], original.fingerprint);
        assert_ne!(fingerprints[0], fingerprints[1]);
        assert_eq!(delta.still_blocking, 1);
        assert_eq!(delta.newly_found, 1);
    }
}

use crate::config::{RepoConfig, ReviewPolicy};
use crate::github::types::PullRequest;
use crate::review::policy::{ReviewRound, RoundMode};

pub fn build_review_prompt(
    repo_cfg: &RepoConfig,
    pr: &PullRequest,
    context: &str,
    bot_name: &str,
    repo_conventions: Option<&str>,
    has_prior_reviews: bool,
    ui_files_changed: Option<&[String]>,
    round: ReviewRound,
    policy: &ReviewPolicy,
    stack_context: Option<&str>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str("Review this pull request. Be discerning. Use good judgment.\n\n");
    prompt.push_str(
        "Focus on: bugs, security flaws, data corruption, race conditions, logic mistakes, breaking changes, and patterns that diverge from codebase conventions. Do not flag style-only issues unless they conceal a correctness problem.\n\n",
    );
    prompt.push_str(&format!(
        "You are {bot_name}, a persistent repository maintainer agent reviewing this PR. Write with the judgment and voice of a careful maintainer: direct, specific, and conversational. A little personality is fine when it follows naturally from the evidence, but avoid jokes, catchphrases, emojis, or performative cleverness. Do not sound like a template.\n\n",
    ));
    prompt.push_str("Instructions:\n");
    prompt.push_str(
        "1. Read the PR description. Validate whether the implementation achieves what the description claims. If the PR says it does X, verify the code does X. If the implementation diverges from stated goals, flag this.\n",
    );
    prompt.push_str(
        "2. Check for introduced security vulnerabilities, injection risks, and attack-surface expansion.\n",
    );

    let mut instruction_num = 3;

    if repo_conventions.is_some() {
        prompt.push_str(&format!(
            "{}. Check adherence to repository conventions provided below. Flag deviations only when they affect correctness, consistency, or maintainability.\n",
            instruction_num,
        ));
        instruction_num += 1;
    }

    if has_prior_reviews {
        prompt.push_str(&format!(
            "{}. Prior review history and inline discussion are in the context. Items marked [dismissed by human], [rejected with rationale], [out of scope for this pr], or [likely addressed] MUST NOT be re-flagged unless the new diff adds materially new evidence. If you re-raise a prior concern, include a short `evidence_note` naming the new lines or changed path that justify reopening it. If a human said a concern is intentional, acceptable, or out of scope for this PR, respect that and do not press the same angle again.\n",
            instruction_num,
        ));
        instruction_num += 1;
    }

    if let Some(ui_files) = ui_files_changed {
        let file_list: String = ui_files
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_str(&format!(
            "{}. This PR modifies UI files: {}. If the PR description does not reference screenshots or visual previews, set ui_screenshot_needed to true. This is a non-blocking metadata note only, never a blocker, and it should only be surfaced once per PR.\n",
            instruction_num, file_list,
        ));
        let _ = instruction_num; // suppress unused warning
    }

    prompt.push_str(
        "Do not turn adjacent architecture preferences into blockers. A blocker must be grounded in concrete evidence from the changed code or targeted context and must directly affect the PR's stated behavior, security, or data integrity.\n\n",
    );
    prompt.push_str(
        "Public review text should be natural and useful to a maintainer. Do not prefix the summary with labels like \"[Automated Review]\" or restate that you are automated; review metadata is handled outside your JSON output.\n\n",
    );

    prompt.push_str(
        "\nYou MUST NOT approve this PR or state it is safe to merge. You are not authorized to make that call. Your role is to flag issues or signal readiness for human review.\n\n",
    );

    prompt.push_str("Output a JSON object in a fenced block tagged exactly `pr-review-json`.\n");
    prompt.push_str("Schema:\n");
    prompt.push_str("- summary: string (one or two natural sentences describing what you found)\n");
    prompt.push_str("- verdict: one of [\"no_issues\", \"comment\", \"request_changes\"]\n");
    prompt.push_str("  - \"no_issues\": nothing worth flagging, ready for human review\n");
    prompt.push_str("  - \"comment\": found issues worth discussing but not blocking\n");
    prompt.push_str("  - \"request_changes\": found issues that should be fixed before merge\n");
    prompt.push_str("- confidence: object with:\n");
    prompt.push_str("  - level: one of [\"high\", \"medium\", \"low\"]\n");
    prompt.push_str("  - reasons: array of one or more reason codes from [\"sufficient_diff_evidence\", \"targeted_context_included\", \"missing_runtime_repro\", \"missing_cross_module_context\", \"ambiguous_requirements\"]\n");
    prompt.push_str("  - justification: string explaining your confidence with concrete evidence. Do NOT use boilerplate like \"full repository context is unavailable\" unless you name a specific missing artifact (example: failing test case name, runtime trace, or exact file/module that is missing).\n");
    prompt.push_str("- ui_screenshot_needed: boolean (true if UI files changed and no screenshots referenced in PR description)\n");
    prompt.push_str("- comments: array of objects with:\n");
    prompt.push_str("  - file: string (file path)\n");
    prompt.push_str("  - line: integer (line number in the changed file)\n");
    prompt.push_str("  - body: string (the finding)\n");
    prompt.push_str("  - evidence_note: optional string (required when re-raising a previously rebutted concern, briefly naming the new lines or changed path that justify reopening it)\n");
    prompt.push_str("  - severity: one of [\"blocking\", \"warning\", \"nitpick\"]\n");
    prompt.push_str("  - finding_kind: one of [\"correctness\", \"security\", \"data_integrity\", \"race_condition\", \"breaking_change\", \"scope_drift\", \"generated_artifact\", \"secret_exposure\", \"local_environment_leak\", \"test_gap\", \"documentation\"]\n");
    prompt.push_str(
        "\nLine numbers must refer to changed lines in the current diff whenever possible.\n\n",
    );
    prompt.push_str(
        "Scope/description drift is non-blocking by default. Mark it blocking only when it includes generated artifacts, secret names, local machine paths, unrelated production code, or security/data exposure.\n\n",
    );

    prompt.push_str("Example output:\n");
    prompt.push_str("```pr-review-json\n");
    prompt.push_str("{\n");
    prompt.push_str(
        "  \"summary\": \"I found one security issue and one null-handling bug worth fixing before this lands.\",\n",
    );
    prompt.push_str("  \"verdict\": \"request_changes\",\n");
    prompt.push_str("  \"confidence\": {\n");
    prompt.push_str("    \"level\": \"high\",\n");
    prompt.push_str(
        "    \"reasons\": [\"sufficient_diff_evidence\", \"targeted_context_included\"],\n",
    );
    prompt.push_str("    \"justification\": \"The SQL interpolation is explicit in src/auth.rs and is reachable from the login handler. The issue is directly provable from this diff.\"\n");
    prompt.push_str("  },\n");
    prompt.push_str("  \"ui_screenshot_needed\": false,\n");
    prompt.push_str("  \"comments\": [\n");
    prompt.push_str("    {\"file\": \"src/auth.rs\", \"line\": 41, \"body\": \"Potential SQL injection when interpolating user input into query string.\", \"severity\": \"blocking\", \"finding_kind\": \"security\"}\n");
    prompt.push_str("  ]\n");
    prompt.push_str("}\n");
    prompt.push_str("```\n\n");

    prompt.push_str(&format!(
        "Repository: {}/{}\n",
        repo_cfg.owner, repo_cfg.name,
    ));
    prompt.push_str(&format!("Pull Request: #{}\n", pr.number));
    prompt.push_str(&format!("Base branch: `{}`\n", pr.base.ref_name));

    if let Some(section) = build_round_section(round, policy) {
        prompt.push_str("\n## Review Round\n");
        prompt.push_str(&section);
    }

    if let Some(stack) = stack_context.filter(|text| !text.trim().is_empty()) {
        prompt.push_str("\n## Stacked PR\n");
        prompt.push_str(stack);
    }

    if let Some(conventions) = repo_conventions {
        if !conventions.trim().is_empty() {
            prompt.push_str("\n## Repository Conventions\n");
            prompt.push_str(conventions);
            prompt.push('\n');
        }
    }

    if let Some(custom) = repo_cfg.custom_instructions.as_deref() {
        if !custom.trim().is_empty() {
            prompt.push_str("\n## Custom Repository Instructions\n");
            prompt.push_str(custom);
            prompt.push('\n');
        }
    }

    prompt.push_str("\n## Context\n");
    prompt.push_str(context);

    prompt
}

/// Tells the model what this round is for. `None` keeps the old prompt for
/// unlimited later rounds.
fn build_round_section(round: ReviewRound, policy: &ReviewPolicy) -> Option<String> {
    let kinds = policy
        .critical_finding_kinds
        .iter()
        .map(|kind| format!("\"{}\"", kind.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    match round.mode {
        RoundMode::Full if round.number == 1 && round.limit > 0 => Some(format!(
            "This is the first review of this PR. Make it complete: report every finding you have now. Later rounds verify fixes, and after round {} new findings are only shown as non-blocking notes.\n",
            round.limit
        )),
        RoundMode::Full => None,
        RoundMode::Verification => Some(format!(
            "This is review round {} of up to {} rounds with inline findings. First check each earlier finding in the context (Previously Flagged Items, Persisted Finding Ledger): leave out the ones that are fixed, and repeat one that is still present only with its earlier wording. Report a new finding only if it is in code changed since your last review (see the incremental diff) or it is a real bug, security or data-integrity problem. Do not raise new wording, style or documentation points about code you already reviewed.\n",
            round.number, round.limit
        )),
        RoundMode::FollowUp => Some(format!(
            "This is review round {}, after the limit of {} rounds with inline findings. New findings are shown as non-blocking notes, not as review threads. A finding is posted inline, and can block the merge, only if its severity is \"blocking\", its finding_kind is one of [{kinds}], and its evidence_note names the exact changed lines that prove it. Use that only for real defects. Keep this review short: say what is fixed, and list at most a few new notes.\n",
            round.number, round.limit
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AutoFixConfig, ForkPolicy};
    use crate::github::types::{PullRequestBase, PullRequestHead, RepoRef, User};

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: None,
        }
    }

    fn test_repo_ref(full_name: &str) -> RepoRef {
        RepoRef {
            full_name: full_name.to_string(),
            fork: false,
            owner: test_user("owner"),
            default_branch: None,
        }
    }

    fn test_repo_config() -> RepoConfig {
        RepoConfig {
            owner: "owner".to_string(),
            name: "repo".to_string(),
            local_path: None,
            harness: None,
            model: None,
            reasoning_effort: None,
            fork_policy: ForkPolicy::Ignore,
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: true,
            workflow: vec![],
            auto_fix: AutoFixConfig::default(),
            review_policy: Default::default(),
        }
    }

    fn test_pull_request() -> PullRequest {
        PullRequest {
            number: 42,
            title: "Tighten the review prompt".to_string(),
            body: Some("Please review this.".to_string()),
            draft: false,
            state: "open".to_string(),
            user: test_user("contributor"),
            head: PullRequestHead {
                sha: "abc123".to_string(),
                ref_name: "feature".to_string(),
                repo: Some(test_repo_ref("owner/repo")),
            },
            base: PullRequestBase {
                ref_name: "main".to_string(),
                repo: test_repo_ref("owner/repo"),
            },
            html_url: None,
            updated_at: None,
            closed_at: None,
            merged_at: None,
            stack: None,
        }
    }

    #[test]
    fn review_prompt_allows_natural_reviewer_voice() {
        let prompt = build_review_prompt(
            &test_repo_config(),
            &test_pull_request(),
            "diff context",
            "PR-Reviewer-Ant",
            None,
            false,
            None,
            ReviewRound::after_prior_reviews(0, 2),
            &ReviewPolicy::default(),
            None,
        );

        assert!(prompt.contains("persistent repository maintainer agent"));
        assert!(prompt.contains("direct, specific, and conversational"));
        assert!(prompt.contains("A little personality is fine"));
        assert!(prompt.contains("Do not sound like a template"));
        assert!(
            prompt.contains("Do not prefix the summary with labels like \"[Automated Review]\"")
        );
        assert!(!prompt.contains("You are an automated review tool"));
        assert!(!prompt.contains("first line must identify this as an automated bot review"));
        assert!(!prompt.contains("\"summary\": \"[Automated Review]"));
    }

    fn prompt_for(prior_reviews: u32, limit: u32, stack: Option<&str>) -> String {
        build_review_prompt(
            &test_repo_config(),
            &test_pull_request(),
            "diff context",
            "bot",
            None,
            prior_reviews > 0,
            None,
            ReviewRound::after_prior_reviews(prior_reviews, limit),
            &ReviewPolicy::default(),
            stack,
        )
    }

    #[test]
    fn first_round_asks_for_a_complete_review() {
        let prompt = prompt_for(0, 2, None);
        assert!(prompt.contains("This is the first review of this PR. Make it complete"));
        assert!(prompt.contains("after round 2 new findings are only shown as non-blocking notes"));
        assert!(prompt.contains("Base branch: `main`"));
    }

    #[test]
    fn verification_round_focuses_on_earlier_findings() {
        let prompt = prompt_for(1, 2, None);
        assert!(prompt.contains("This is review round 2 of up to 2 rounds"));
        assert!(prompt.contains("First check each earlier finding"));
    }

    #[test]
    fn follow_up_round_states_the_critical_bar() {
        let prompt = prompt_for(4, 2, None);
        assert!(prompt.contains("This is review round 5, after the limit of 2 rounds"));
        assert!(prompt
            .contains("\"security\", \"secret_exposure\", \"data_integrity\", \"correctness\""));
        assert!(prompt.contains("evidence_note names the exact changed lines"));
    }

    #[test]
    fn unlimited_rounds_keep_the_old_prompt() {
        let prompt = prompt_for(6, 0, None);
        assert!(!prompt.contains("## Review Round"));
    }

    #[test]
    fn stack_context_gets_its_own_section() {
        let prompt = prompt_for(0, 2, Some("This PR is part of a stack of pull requests."));
        let section = prompt.find("## Stacked PR").expect("stack section");
        let context = prompt.find("## Context").expect("context section");
        assert!(
            section < context,
            "stack section comes before the truncatable context"
        );
        assert!(!prompt_for(0, 2, Some("  ")).contains("## Stacked PR"));
    }
}

//! Review-cycle policy: which round a review is, what happens to each
//! finding in that round, and when the bot's own change requests are stale.
//!
//! Round 1 is the full review. Rounds 2..=`max_review_cycles` verify earlier
//! findings. Later rounds still run, but new findings that are not critical
//! become non-blocking notes in the review body instead of inline threads,
//! so a PR cannot be held open by an endless stream of new comments.

use std::collections::HashSet;

use crate::config::ReviewPolicy;
use crate::github::types::PullRequestReview;
use crate::review::parser::{CommentSeverity, FindingKind, ReviewComment, ReviewVerdict};

/// Line in the metadata block of every review the bot posts. Counting
/// reviews that carry it tells the bot how many rounds it has run, even when
/// it posts under a personal account that humans also review with.
pub const REVIEW_METADATA_MARKER: &str = "- Reviewer: [pr-reviewer](";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RoundMode {
    /// Post every finding inline. Round 1, or every round when there is no limit.
    Full,
    /// Re-check earlier findings; new findings are still posted inline.
    Verification,
    /// Past the limit: only critical findings are posted inline.
    FollowUp,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ReviewRound {
    /// 1-based number of this review on the PR.
    pub number: u32,
    /// `max_review_cycles`; 0 means no limit.
    pub limit: u32,
    pub mode: RoundMode,
}

impl ReviewRound {
    pub fn after_prior_reviews(prior_reviews: u32, limit: u32) -> Self {
        let number = prior_reviews.saturating_add(1);
        let mode = if number == 1 || limit == 0 {
            RoundMode::Full
        } else if number <= limit {
            RoundMode::Verification
        } else {
            RoundMode::FollowUp
        };
        Self {
            number,
            limit,
            mode,
        }
    }
}

/// Whether `review` was posted by this bot. Reviews carrying the metadata
/// marker count when posted by the bot account or its authenticated alias;
/// marker-less reviews count only from a dedicated account named `bot_name`.
pub fn is_bot_review(review: &PullRequestReview, bot_name: &str, bot_alias: Option<&str>) -> bool {
    let login = review.user.login.as_str();
    let has_marker = review
        .body
        .as_deref()
        .is_some_and(|body| body.contains(REVIEW_METADATA_MARKER));
    if has_marker {
        login.eq_ignore_ascii_case(bot_name)
            || bot_alias.is_some_and(|alias| login.eq_ignore_ascii_case(alias))
    } else {
        login.eq_ignore_ascii_case(bot_name)
    }
}

/// Number of review rounds the bot already posted on earlier commits: one
/// per reviewed commit, so several reviews of one commit count once.
pub fn count_prior_bot_reviews(
    reviews: &[PullRequestReview],
    bot_name: &str,
    bot_alias: Option<&str>,
    current_sha: &str,
) -> u32 {
    reviews
        .iter()
        .filter(|review| review.commit_id.as_deref() != Some(current_sha))
        .filter(|review| is_bot_review(review, bot_name, bot_alias))
        .map(|review| review.commit_id.as_deref().unwrap_or_default())
        .collect::<HashSet<_>>()
        .len() as u32
}

pub(crate) fn normalize_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn token_set(text: &str) -> HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "this", "that", "with", "from", "into", "when", "will", "would", "should", "could", "also",
        "then", "than", "they", "them", "their", "about", "under", "over", "after", "before",
        "because", "which", "while", "where", "there", "here", "have", "has", "had", "does",
        "doesnt", "dont", "isnt", "cant", "only", "just", "being", "through", "still", "later",
        "every", "other", "again", "across", "agent", "scope", "scoped", "pr", "review",
        "reviewer", "blocking", "warning", "comment",
    ];

    normalize_text(text)
        .split_whitespace()
        .filter(|token| token.len() >= 4 && !STOPWORDS.contains(token))
        .map(ToString::to_string)
        .collect()
}

/// A finding as stored or posted: where it is and what it says.
#[derive(Debug, Clone, Copy)]
pub struct FindingRef<'a> {
    pub path: &'a str,
    pub line: Option<u32>,
    pub body: &'a str,
}

/// Lines within this distance count as "near the same line".
const NEAR_LINE_DISTANCE: u32 = 15;
/// Share of the smaller token set two findings must have in common.
const SAME_FINDING_CONTAINMENT: f64 = 0.6;
const SAME_FINDING_CONTAINMENT_NEAR: f64 = 0.45;

/// Whether two findings describe the same problem in the same file.
///
/// Calibrated on 2,200 production findings: when the model re-raises a
/// finding it keeps most of the meaningful words (at least 60% of the
/// smaller token set, or 45% when it sits near the same line), while
/// different findings on one file rarely share more than 30%. Counting
/// shared tokens instead of a share does not work: unrelated findings on
/// one file routinely share two to six words. Findings that quote code
/// identifiers must share one, so "`delete_user` lacks an authorization
/// check" and the same sentence about `update_user` stay two findings.
pub fn same_finding(a: FindingRef<'_>, b: FindingRef<'_>) -> bool {
    if !a.path.trim().eq_ignore_ascii_case(b.path.trim()) {
        return false;
    }
    let (left, right) = (token_set(a.body), token_set(b.body));
    let smaller = left.len().min(right.len());
    if smaller < SAME_FINDING_MIN_TOKENS {
        return normalize_text(a.body) == normalize_text(b.body);
    }
    let (left_ids, right_ids) = (code_identifiers(a.body), code_identifiers(b.body));
    if !left_ids.is_empty() && !right_ids.is_empty() && left_ids.is_disjoint(&right_ids) {
        return false;
    }
    let containment = left.intersection(&right).count() as f64 / smaller as f64;
    containment >= SAME_FINDING_CONTAINMENT
        || (lines_are_near(a, b) && containment >= SAME_FINDING_CONTAINMENT_NEAR)
}

/// `same_finding` near the same line. Used where a match hides a new
/// finding completely (open or resolved thread), so a similar finding
/// elsewhere in the file is still posted.
pub fn same_finding_near(a: FindingRef<'_>, b: FindingRef<'_>) -> bool {
    lines_are_near(a, b) && same_finding(a, b)
}

/// Below this many meaningful words, only identical text is the same finding.
const SAME_FINDING_MIN_TOKENS: usize = 3;

/// Code identifiers quoted in backticks: names with `_`, `.`, `::`, `->`,
/// or camelCase, with or without a trailing `()`. Each yields its full name
/// and its last component, so `llm.has_pending` and `has_pending` share one.
fn code_identifiers(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for (index, span) in text.split('`').enumerate() {
        if index % 2 == 0 || span.len() > 80 {
            continue;
        }
        let span = span.trim().trim_end_matches("()");
        let is_identifier = span
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && span
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '>'));
        let looks_like_code = span.contains('_')
            || span.contains('.')
            || span.contains("::")
            || span.contains("->")
            || span
                .as_bytes()
                .windows(2)
                .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase());
        if !(is_identifier && looks_like_code) {
            continue;
        }
        let lower = span.to_ascii_lowercase();
        if let Some(last) = lower
            .rsplit(['.', ':', '>'])
            .next()
            .filter(|last| !last.is_empty())
        {
            out.insert(last.to_string());
        }
        out.insert(lower);
    }
    out
}

/// An unknown line is never near: a thread whose line GitHub no longer
/// knows must not hide findings anywhere in its file.
fn lines_are_near(a: FindingRef<'_>, b: FindingRef<'_>) -> bool {
    match (a.line, b.line) {
        (Some(x), Some(y)) => x.abs_diff(y) <= NEAR_LINE_DISTANCE,
        _ => false,
    }
}

/// Blocking, of a configured critical kind, and backed by an evidence note.
/// Only these findings may still block after the review-cycle limit, or
/// reopen a thread a human resolved.
pub fn is_critical(comment: &ReviewComment, policy: &ReviewPolicy) -> bool {
    comment.severity == CommentSeverity::Blocking
        && policy
            .critical_finding_kinds
            .contains(&comment.finding_kind)
        && comment
            .evidence_note
            .as_deref()
            .is_some_and(|note| !note.trim().is_empty())
}

/// What the bot already knows about a new finding.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct FindingContext {
    /// The bot already posted this finding in a thread that is still open.
    pub open_thread: bool,
    /// The bot posted this finding before and a human resolved the thread.
    pub resolved_thread: bool,
    /// The finding is already reported as a follow-up note.
    pub already_noted: bool,
    /// The finding is already open on this ancestor PR of a stack. Only set
    /// for findings on lines this PR did not change.
    pub tracked_on_ancestor: Option<u64>,
    /// The finding's file has a different own change than at the last
    /// review (true when that is unknown).
    pub file_changed_since_last: bool,
    /// The finding sits on a line this PR adds or removes (context lines
    /// around a change do not count).
    pub on_changed_line: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Disposition {
    /// Post as an inline review comment.
    Inline,
    /// Its thread is still open; keep it active but do not post it again.
    StillOpen,
    /// A human resolved its thread; drop it.
    Resolved,
    /// An ancestor PR in the stack already tracks it; drop it here.
    TrackedOnAncestor(u64),
    /// Already reported as a follow-up note; drop it.
    AlreadyNoted,
    /// Report it in the non-blocking notes section of the review body.
    FollowUpNote,
}

pub fn decide_disposition(
    comment: &ReviewComment,
    round: ReviewRound,
    policy: &ReviewPolicy,
    context: FindingContext,
) -> Disposition {
    let critical = is_critical(comment, policy);
    if let Some(pr) = context.tracked_on_ancestor {
        // A critical finding is never dropped because a lower PR has a
        // similar one: it may be the same bug class in this PR's code.
        if !critical {
            return Disposition::TrackedOnAncestor(pr);
        }
    }
    if context.open_thread {
        return Disposition::StillOpen;
    }
    if context.resolved_thread {
        // Reopen a thread a human resolved only for a critical finding in a
        // file whose change differs from the last review: new code, not the
        // same argument again.
        let allowed_this_round = round.mode != RoundMode::FollowUp || context.on_changed_line;
        return if critical && context.file_changed_since_last && allowed_this_round {
            Disposition::Inline
        } else {
            Disposition::Resolved
        };
    }
    if context.already_noted && !critical {
        return Disposition::AlreadyNoted;
    }
    if round.mode == RoundMode::FollowUp && !(critical && context.on_changed_line) {
        return Disposition::FollowUpNote;
    }
    Disposition::Inline
}

/// The verdict after dispositions: a change request needs a blocking finding
/// that is posted inline now or still open in its thread. The model's
/// verdict is never raised, only lowered.
pub fn final_verdict(
    model_verdict: ReviewVerdict,
    findings: &[(CommentSeverity, Disposition)],
) -> ReviewVerdict {
    let verdict = model_verdict.normalized();
    let has_blocker = findings.iter().any(|(severity, disposition)| {
        *severity == CommentSeverity::Blocking
            && matches!(disposition, Disposition::Inline | Disposition::StillOpen)
    });
    if verdict == ReviewVerdict::RequestChanges && !has_blocker {
        ReviewVerdict::Comment
    } else {
        verdict
    }
}

/// A finding reported in the notes section instead of inline.
#[derive(Debug, Clone)]
pub struct FollowUpNote {
    pub path: String,
    pub line: u32,
    pub body: String,
    pub severity: CommentSeverity,
    pub finding_kind: FindingKind,
}

pub fn follow_up_notes_section(round: ReviewRound, notes: &[FollowUpNote]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let mut ordered: Vec<&FollowUpNote> = notes.iter().collect();
    ordered.sort_by_key(|note| match note.severity {
        CommentSeverity::Blocking => 0,
        CommentSeverity::Warning => 1,
        CommentSeverity::Nitpick => 2,
    });
    let mut out = format!(
        "### Follow-up notes (non-blocking)\nThis is review round {} and inline findings stop after round {}, so these new findings are notes only. They do not block merging. Critical problems on changed lines are still posted inline.\n",
        round.number, round.limit
    );
    for note in ordered {
        let label = match note.severity {
            CommentSeverity::Blocking => "possible blocker",
            CommentSeverity::Warning => "warning",
            CommentSeverity::Nitpick => "nitpick",
        };
        out.push_str(&format!(
            "- `{}:{}` ({label}, {}): {}\n",
            note.path,
            note.line,
            note.finding_kind.as_str(),
            note.body.replace('\n', " ")
        ));
    }
    Some(out)
}

/// One line per kind of finding the bot chose not to post again.
#[derive(Debug, Clone, Default)]
pub struct CarriedOver {
    pub still_open: usize,
    pub resolved: usize,
    pub already_noted: usize,
    /// (ancestor PR, number of findings it already tracks)
    pub on_ancestors: Vec<(u64, usize)>,
}

impl CarriedOver {
    pub fn record(&mut self, disposition: Disposition) {
        match disposition {
            Disposition::StillOpen => self.still_open += 1,
            Disposition::Resolved => self.resolved += 1,
            Disposition::AlreadyNoted => self.already_noted += 1,
            Disposition::TrackedOnAncestor(pr) => {
                match self
                    .on_ancestors
                    .iter_mut()
                    .find(|(number, _)| *number == pr)
                {
                    Some(entry) => entry.1 += 1,
                    None => self.on_ancestors.push((pr, 1)),
                }
            }
            Disposition::Inline | Disposition::FollowUpNote => {}
        }
    }

    pub fn section(&self) -> Option<String> {
        let plural = |n: usize| if n == 1 { "finding is" } else { "findings are" };
        let mut lines = Vec::new();
        if self.still_open > 0 {
            lines.push(format!(
                "- {} earlier {} still open in {} thread; I did not post {} again.",
                self.still_open,
                plural(self.still_open),
                if self.still_open == 1 { "its" } else { "their" },
                if self.still_open == 1 { "it" } else { "them" },
            ));
        }
        if self.resolved > 0 {
            lines.push(format!(
                "- {} {} a thread a human resolved, so I did not post {} again.",
                self.resolved,
                if self.resolved == 1 {
                    "finding matches"
                } else {
                    "findings match"
                },
                if self.resolved == 1 { "it" } else { "them" },
            ));
        }
        if self.already_noted > 0 {
            lines.push(format!(
                "- {} {} already in an earlier follow-up note.",
                self.already_noted,
                plural(self.already_noted),
            ));
        }
        for (pr, count) in &self.on_ancestors {
            lines.push(format!(
                "- {count} {} already tracked on #{pr}, lower in this stack, so I did not repeat {} here.",
                plural(*count),
                if *count == 1 { "it" } else { "them" },
            ));
        }
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }
}

/// The bot's earlier change requests to dismiss after posting `posted_event`.
/// None while this review requests changes or a blocking finding is open.
pub fn stale_change_requests<'a>(
    reviews: &'a [PullRequestReview],
    bot_name: &str,
    bot_alias: Option<&str>,
    posted_event: &str,
    blocking_finding_open: bool,
) -> Vec<&'a PullRequestReview> {
    if posted_event == "REQUEST_CHANGES" || blocking_finding_open {
        return Vec::new();
    }
    reviews
        .iter()
        .filter(|review| review.state.as_deref() == Some("CHANGES_REQUESTED"))
        .filter(|review| is_bot_review(review, bot_name, bot_alias))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::User;

    fn review(
        id: u64,
        login: &str,
        sha: &str,
        state: &str,
        with_marker: bool,
    ) -> PullRequestReview {
        PullRequestReview {
            id,
            body: Some(if with_marker {
                format!("<details>\n{REVIEW_METADATA_MARKER}https://x)\n</details>\nsummary")
            } else {
                "Looks fine to me.".to_string()
            }),
            user: User {
                login: login.to_string(),
                account_type: None,
            },
            state: Some(state.to_string()),
            commit_id: Some(sha.to_string()),
            submitted_at: None,
        }
    }

    fn finding(
        severity: CommentSeverity,
        kind: FindingKind,
        evidence: Option<&str>,
    ) -> ReviewComment {
        ReviewComment {
            file: "src/lib.rs".to_string(),
            line: 10,
            body: "The handler drops the error.".to_string(),
            severity,
            evidence_note: evidence.map(ToString::to_string),
            finding_kind: kind,
        }
    }

    fn policy() -> ReviewPolicy {
        ReviewPolicy::default()
    }

    #[test]
    fn rounds_follow_the_limit() {
        assert_eq!(ReviewRound::after_prior_reviews(0, 2).mode, RoundMode::Full);
        assert_eq!(
            ReviewRound::after_prior_reviews(1, 2).mode,
            RoundMode::Verification
        );
        let third = ReviewRound::after_prior_reviews(2, 2);
        assert_eq!((third.number, third.mode), (3, RoundMode::FollowUp));
        assert_eq!(
            ReviewRound::after_prior_reviews(1, 1).mode,
            RoundMode::FollowUp
        );
        assert_eq!(
            ReviewRound::after_prior_reviews(14, 0).mode,
            RoundMode::Full,
            "0 keeps the old unlimited behaviour"
        );
    }

    #[test]
    fn prior_rounds_count_only_marked_bot_reviews_on_other_commits() {
        let reviews = vec![
            review(1, "alice", "sha1", "COMMENTED", true),
            review(2, "alice", "sha2", "CHANGES_REQUESTED", true),
            // The same personal account reviewing by hand is not a bot round.
            review(3, "alice", "sha2", "COMMENTED", false),
            // Someone else quoting the marker is not a bot round either.
            review(4, "mallory", "sha2", "COMMENTED", true),
            // A review of the current head does not count as a prior round.
            review(5, "alice", "sha3", "COMMENTED", true),
            // A dedicated bot account counts even without the marker, but a
            // second bot review of an already reviewed commit is the same round.
            review(6, "pr-reviewer", "sha1", "COMMENTED", false),
            review(7, "pr-reviewer", "sha4", "COMMENTED", false),
        ];
        assert_eq!(
            count_prior_bot_reviews(&reviews, "pr-reviewer", Some("alice"), "sha3"),
            3,
            "rounds are reviewed commits: sha1, sha2 and sha4"
        );
    }

    #[test]
    fn same_finding_matches_rewording_but_not_neighbours() {
        let first = FindingRef {
            path: "scripts/setup.sh",
            line: Some(233),
            body: "One crash window is left. `mv config.tmp config.toml` and `touch \"$done\"` are two steps. If the process is killed between them the next start re-runs setup.",
        };
        let reworded = FindingRef {
            path: "scripts/setup.sh",
            line: Some(236),
            body: "This fixes the old gap, but one small window is left: `mv config.tmp config.toml` and `touch \"$done\"` are separate steps, so a kill between them re-runs setup.",
        };
        let neighbour = FindingRef {
            path: "scripts/setup.sh",
            line: Some(240),
            body: "The worker is started before the service is registered, so `svc register` can race the first health probe and the service never shows up.",
        };
        let elsewhere = FindingRef {
            path: "scripts/other.sh",
            ..reworded
        };
        assert!(same_finding(first, reworded));
        assert!(!same_finding(first, neighbour));
        assert!(!same_finding(first, elsewhere));
    }

    #[test]
    fn full_and_verification_rounds_post_new_findings_inline() {
        for prior in [0, 1] {
            let round = ReviewRound::after_prior_reviews(prior, 2);
            let warning = finding(CommentSeverity::Warning, FindingKind::Documentation, None);
            assert_eq!(
                decide_disposition(&warning, round, &policy(), FindingContext::default()),
                Disposition::Inline
            );
        }
    }

    #[test]
    fn follow_up_round_turns_non_critical_findings_into_notes() {
        let round = ReviewRound::after_prior_reviews(2, 2);
        for comment in [
            finding(CommentSeverity::Warning, FindingKind::Correctness, None),
            finding(CommentSeverity::Nitpick, FindingKind::Documentation, None),
            // Blocking, but documentation is not a critical kind.
            finding(
                CommentSeverity::Blocking,
                FindingKind::Documentation,
                Some("line 154"),
            ),
            // Blocking and critical kind, but no evidence note.
            finding(CommentSeverity::Blocking, FindingKind::Security, None),
        ] {
            assert_eq!(
                decide_disposition(&comment, round, &policy(), FindingContext::default()),
                Disposition::FollowUpNote,
                "{comment:?}"
            );
        }
    }

    #[test]
    fn critical_findings_stay_inline_after_the_limit() {
        let round = ReviewRound::after_prior_reviews(9, 2);
        let critical = finding(
            CommentSeverity::Blocking,
            FindingKind::Security,
            Some("New line 41 interpolates the user id into the SQL string."),
        );
        let on_changed_line = FindingContext {
            on_changed_line: true,
            ..Default::default()
        };
        let disposition = decide_disposition(&critical, round, &policy(), on_changed_line);
        assert_eq!(disposition, Disposition::Inline);
        assert_eq!(
            decide_disposition(&critical, round, &policy(), FindingContext::default()),
            Disposition::FollowUpNote,
            "after the limit a critical finding off the PR's changed lines is a visible note"
        );
        assert_eq!(
            final_verdict(
                ReviewVerdict::RequestChanges,
                &[(CommentSeverity::Blocking, disposition)]
            ),
            ReviewVerdict::RequestChanges
        );
    }

    #[test]
    fn critical_kinds_are_configurable() {
        let round = ReviewRound::after_prior_reviews(5, 2);
        let mut only_security = policy();
        only_security.critical_finding_kinds = vec![FindingKind::Security];
        let correctness = finding(
            CommentSeverity::Blocking,
            FindingKind::Correctness,
            Some("x"),
        );
        let on_changed_line = FindingContext {
            on_changed_line: true,
            ..Default::default()
        };
        assert_eq!(
            decide_disposition(&correctness, round, &only_security, on_changed_line),
            Disposition::FollowUpNote
        );
        assert_eq!(
            decide_disposition(&correctness, round, &policy(), on_changed_line),
            Disposition::Inline
        );
    }

    #[test]
    fn known_findings_are_not_posted_again() {
        let round = ReviewRound::after_prior_reviews(1, 2);
        let warning = finding(CommentSeverity::Warning, FindingKind::Correctness, None);
        let critical = finding(
            CommentSeverity::Blocking,
            FindingKind::DataIntegrity,
            Some("new path"),
        );
        let open = FindingContext {
            open_thread: true,
            ..Default::default()
        };
        let resolved = FindingContext {
            resolved_thread: true,
            ..Default::default()
        };
        let noted = FindingContext {
            already_noted: true,
            ..Default::default()
        };
        let ancestor = FindingContext {
            tracked_on_ancestor: Some(12),
            ..Default::default()
        };
        assert_eq!(
            decide_disposition(&warning, round, &policy(), open),
            Disposition::StillOpen
        );
        assert_eq!(
            decide_disposition(&critical, round, &policy(), open),
            Disposition::StillOpen
        );
        assert_eq!(
            decide_disposition(&warning, round, &policy(), resolved),
            Disposition::Resolved
        );
        assert_eq!(
            decide_disposition(&critical, round, &policy(), resolved),
            Disposition::Resolved,
            "the same code again does not reopen a thread a human resolved"
        );
        let resolved_new_code = FindingContext {
            resolved_thread: true,
            file_changed_since_last: true,
            ..Default::default()
        };
        assert_eq!(
            decide_disposition(&critical, round, &policy(), resolved_new_code),
            Disposition::Inline,
            "a critical finding in changed code reopens a resolved thread"
        );
        assert_eq!(
            decide_disposition(&warning, round, &policy(), resolved_new_code),
            Disposition::Resolved
        );
        assert_eq!(
            decide_disposition(&warning, round, &policy(), noted),
            Disposition::AlreadyNoted
        );
        assert_eq!(
            decide_disposition(&warning, round, &policy(), ancestor),
            Disposition::TrackedOnAncestor(12)
        );
        assert_eq!(
            decide_disposition(&critical, round, &policy(), ancestor),
            Disposition::Inline,
            "a critical finding is never dropped as a duplicate of a lower PR's finding"
        );
    }

    #[test]
    fn change_requests_need_a_live_blocker() {
        use CommentSeverity::*;
        use Disposition::*;
        let rc = ReviewVerdict::RequestChanges;
        assert_eq!(
            final_verdict(rc, &[(Blocking, FollowUpNote)]),
            ReviewVerdict::Comment
        );
        assert_eq!(
            final_verdict(rc, &[(Blocking, TrackedOnAncestor(1))]),
            ReviewVerdict::Comment
        );
        assert_eq!(
            final_verdict(rc, &[(Warning, Inline)]),
            ReviewVerdict::Comment
        );
        assert_eq!(final_verdict(rc, &[(Blocking, StillOpen)]), rc);
        assert_eq!(
            final_verdict(ReviewVerdict::Comment, &[(Blocking, Inline)]),
            ReviewVerdict::Comment,
            "the model's verdict is never raised"
        );
    }

    #[test]
    fn notes_section_lists_possible_blockers_first() {
        let round = ReviewRound::after_prior_reviews(3, 2);
        let note = |severity, body: &str| FollowUpNote {
            path: "a.rs".to_string(),
            line: 3,
            body: body.to_string(),
            severity,
            finding_kind: FindingKind::Correctness,
        };
        let section = follow_up_notes_section(
            round,
            &[
                note(CommentSeverity::Nitpick, "rename x"),
                note(CommentSeverity::Blocking, "may panic"),
            ],
        )
        .unwrap();
        assert!(section.contains("review round 4 and inline findings stop after round 2"));
        let blocker = section.find("may panic").unwrap();
        let nit = section.find("rename x").unwrap();
        assert!(blocker < nit, "{section}");
        assert!(follow_up_notes_section(round, &[]).is_none());
    }

    #[test]
    fn carried_over_section_summarizes_skipped_findings() {
        let mut carried = CarriedOver::default();
        carried.record(Disposition::StillOpen);
        carried.record(Disposition::TrackedOnAncestor(12));
        carried.record(Disposition::TrackedOnAncestor(12));
        carried.record(Disposition::Inline);
        let section = carried.section().unwrap();
        assert!(section.contains("1 earlier finding is still open in its thread"));
        assert!(section.contains("2 findings are already tracked on #12"));
        assert!(CarriedOver::default().section().is_none());
    }

    #[test]
    fn stale_change_requests_are_dismissed_only_when_nothing_blocks() {
        let reviews = vec![
            review(1, "alice", "sha1", "CHANGES_REQUESTED", true),
            review(2, "bob", "sha1", "CHANGES_REQUESTED", false),
            review(3, "alice", "sha2", "COMMENTED", true),
        ];
        let stale = stale_change_requests(&reviews, "pr-reviewer", Some("alice"), "COMMENT", false);
        assert_eq!(
            stale.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1],
            "bob's review is human"
        );
        assert!(
            stale_change_requests(&reviews, "pr-reviewer", Some("alice"), "COMMENT", true)
                .is_empty()
        );
        assert!(stale_change_requests(
            &reviews,
            "pr-reviewer",
            Some("alice"),
            "REQUEST_CHANGES",
            false
        )
        .is_empty());
    }

    #[test]
    fn different_identifiers_are_different_findings() {
        let delete = FindingRef {
            path: "src/users.rs",
            line: Some(10),
            body: "Missing authorization check: `delete_user` does not verify the caller owns the account, so any authenticated user can delete other accounts.",
        };
        let update = FindingRef {
            line: Some(12),
            body: "Missing authorization check: `update_user` does not verify the caller owns the account, so any authenticated user can modify other accounts.",
            ..delete
        };
        assert!(!same_finding(delete, update));
        let qualified = FindingRef {
            body: "Still open: `users::delete_user` does not verify the caller owns the account, so any authenticated user can delete other accounts.",
            ..update
        };
        assert!(
            same_finding(delete, qualified),
            "a qualified name shares its last component"
        );
    }

    #[test]
    fn short_findings_must_match_exactly() {
        let short = FindingRef {
            path: "src/view.rs",
            line: Some(3),
            body: "XSS in render",
        };
        let longer = FindingRef {
            body: "XSS in render_comment and render_title, both unescaped",
            ..short
        };
        assert!(!same_finding(short, longer));
        assert!(same_finding(
            short,
            FindingRef {
                line: Some(90),
                ..short
            }
        ));
    }

    #[test]
    fn hiding_matches_must_be_near() {
        let first = FindingRef {
            path: "src/fetch.rs",
            line: Some(40),
            body: "The retry loop in `fetch_all` never sleeps, so a 429 from GitHub spins the CPU and burns the rate limit.",
        };
        let far = FindingRef {
            line: Some(400),
            ..first
        };
        assert!(same_finding(first, far));
        assert!(!same_finding_near(first, far));
        assert!(same_finding_near(
            first,
            FindingRef {
                line: Some(52),
                ..first
            }
        ));
    }

    #[test]
    fn an_unknown_line_is_never_near() {
        let thread = FindingRef {
            path: "src/fetch.rs",
            line: None,
            body: "The retry loop in `fetch_all` never sleeps, so a 429 from GitHub spins the CPU and burns the whole rate limit in seconds.",
        };
        let other = FindingRef {
            path: "src/fetch.rs",
            line: Some(300),
            body: "`fetch_all` never checks the rate limit header before retrying, so a revoked token keeps sending requests until GitHub blocks the whole app.",
        };
        assert!(!same_finding_near(thread, other));
        assert!(
            !same_finding(thread, other),
            "0.5 containment needs a known nearby line"
        );
        assert!(
            !same_finding_near(thread, thread),
            "without a line nothing is hidden, even the same text"
        );
    }
}

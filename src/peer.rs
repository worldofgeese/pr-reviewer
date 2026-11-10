//! Peer daemons: other pr-reviewer daemons that watch the same repositories
//! under their own GitHub account (`[defaults] peer_logins`).
//!
//! A peer's output is recognized by the same markers this daemon writes, and
//! only when the peer's login posted it, so a forged marker from anyone else
//! does not hide a review.

use chrono::{DateTime, Utc};

use crate::github::types::{IssueComment, PullRequestReview, ReviewComment};
use crate::review::policy::REVIEW_METADATA_MARKER;

/// What a peer already did for one head commit of a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerActivity {
    /// No peer has touched this head commit.
    None,
    /// A peer posted a review of this head commit, or recorded that it
    /// skipped it (for example as docs-only).
    Done { login: String },
    /// A peer started a review of this head commit within the harness
    /// timeout and has not finished it yet.
    InProgress { login: String },
}

pub fn is_peer(login: &str, peers: &[String]) -> bool {
    peers.iter().any(|peer| peer.eq_ignore_ascii_case(login))
}

/// What the peers did for `sha`. A peer's "in progress" comment older than
/// `stale_after_secs` (the harness timeout) means the peer died mid-review,
/// so it does not count.
pub fn peer_activity(
    reviews: &[PullRequestReview],
    issue_comments: &[IssueComment],
    peers: &[String],
    pr_number: u64,
    sha: &str,
    stale_after_secs: u64,
    now: DateTime<Utc>,
) -> PeerActivity {
    if peers.is_empty() {
        return PeerActivity::None;
    }
    if let Some(review) = reviews.iter().find(|review| {
        review.commit_id.as_deref() == Some(sha)
            && is_peer(&review.user.login, peers)
            && review
                .body
                .as_deref()
                .is_some_and(|body| body.contains(REVIEW_METADATA_MARKER))
    }) {
        return PeerActivity::Done {
            login: review.user.login.clone(),
        };
    }

    // The trailing space keeps a SHA from matching a longer SHA.
    let head = format!("pr={pr_number} sha={sha} ");
    let skipped_marker = format!("<!-- pr-reviewer-status:skipped {head}");
    let in_progress_marker = format!("<!-- pr-reviewer-status:in-progress {head}-->");
    let mut in_progress = None;
    for comment in issue_comments
        .iter()
        .filter(|comment| is_peer(&comment.user.login, peers))
    {
        let body = comment.body.as_str();
        let verdict_for_head = body
            .lines()
            .any(|line| line.starts_with("<!-- pr-reviewer-verdict:") && line.contains(&head));
        if verdict_for_head || body.contains(&skipped_marker) {
            return PeerActivity::Done {
                login: comment.user.login.clone(),
            };
        }
        if body.contains(&in_progress_marker)
            && is_fresh(comment.last_changed_at(), stale_after_secs, now)
        {
            in_progress = Some(comment.user.login.clone());
        }
    }
    match in_progress {
        Some(login) => PeerActivity::InProgress { login },
        None => PeerActivity::None,
    }
}

/// The peer that already answered the maintainer command in comment
/// `source_comment_id`, if any.
pub fn peer_answered_command<'a>(
    issue_comments: &'a [IssueComment],
    peers: &[String],
    source_comment_id: u64,
) -> Option<&'a str> {
    peer_command_response(issue_comments, peers, source_comment_id, |_| true)
}

/// The peer that repairs the `@pr-reviewer fix` in comment
/// `source_comment_id`. A peer with auto-fix off answers the command too, but
/// its answer says `repair=off` and leaves the repair to this daemon.
pub fn peer_took_repair<'a>(
    issue_comments: &'a [IssueComment],
    peers: &[String],
    source_comment_id: u64,
) -> Option<&'a str> {
    peer_command_response(issue_comments, peers, source_comment_id, |marker| {
        marker.contains(" repair=on ")
    })
}

fn peer_command_response<'a>(
    issue_comments: &'a [IssueComment],
    peers: &[String],
    source_comment_id: u64,
    accept: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    let needle = format!(" source_comment={source_comment_id} ");
    issue_comments
        .iter()
        .filter(|comment| is_peer(&comment.user.login, peers))
        .find(|comment| {
            comment.body.lines().any(|line| {
                line.starts_with("<!-- pr-reviewer-command-response ")
                    && line.contains(&needle)
                    && accept(line)
            })
        })
        .map(|comment| comment.user.login.as_str())
}

/// The peer whose review comment started the thread of `comment_id`. That
/// peer answers mentions in its own threads.
pub fn peer_thread_owner<'a>(
    review_comments: &'a [ReviewComment],
    peers: &[String],
    comment_id: u64,
) -> Option<&'a str> {
    let mut current = review_comments.iter().find(|c| c.id == comment_id)?;
    // Bounded walk: a malformed reply chain must not loop forever.
    for _ in 0..review_comments.len() {
        let Some(parent_id) = current.in_reply_to_id else {
            break;
        };
        let Some(parent) = review_comments.iter().find(|c| c.id == parent_id) else {
            break;
        };
        current = parent;
    }
    is_peer(&current.user.login, peers).then_some(current.user.login.as_str())
}

fn is_fresh(timestamp: &str, stale_after_secs: u64, now: DateTime<Utc>) -> bool {
    let Ok(at) = DateTime::parse_from_rfc3339(timestamp) else {
        // Unknown age: assume the peer is still working rather than review
        // twice. The work item is deferred, not dropped.
        return true;
    };
    let age = now.signed_duration_since(at.with_timezone(&Utc));
    age.num_seconds() < stale_after_secs as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::User;

    const SHA: &str = "abc123";

    fn user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: None,
        }
    }

    fn peers() -> Vec<String> {
        vec!["Tao".to_string()]
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn review(login: &str, sha: &str, body: &str) -> PullRequestReview {
        PullRequestReview {
            id: 1,
            body: Some(body.to_string()),
            user: user(login),
            state: Some("COMMENTED".to_string()),
            commit_id: Some(sha.to_string()),
            submitted_at: None,
        }
    }

    fn comment(id: u64, login: &str, body: &str, at: &str) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            user: user(login),
            author_association: None,
            created_at: at.to_string(),
            updated_at: None,
        }
    }

    fn metadata() -> String {
        format!("summary\n{REVIEW_METADATA_MARKER}https://example)")
    }

    fn activity(reviews: &[PullRequestReview], comments: &[IssueComment]) -> PeerActivity {
        peer_activity(reviews, comments, &peers(), 7, SHA, 1800, now())
    }

    #[test]
    fn a_marked_peer_review_of_the_head_is_done() {
        let reviews = [review("tao", SHA, &metadata())];
        assert_eq!(
            activity(&reviews, &[]),
            PeerActivity::Done {
                login: "tao".to_string()
            }
        );
    }

    #[test]
    fn a_peer_review_without_the_marker_is_a_human_review() {
        let reviews = [review("tao", SHA, "LGTM")];
        assert_eq!(activity(&reviews, &[]), PeerActivity::None);
    }

    #[test]
    fn a_marked_review_from_a_non_peer_is_ignored() {
        let reviews = [review("someone", SHA, &metadata())];
        assert_eq!(activity(&reviews, &[]), PeerActivity::None);
    }

    #[test]
    fn a_peer_review_of_an_older_head_is_ignored() {
        let reviews = [review("tao", "old", &metadata())];
        assert_eq!(activity(&reviews, &[]), PeerActivity::None);
    }

    #[test]
    fn a_peer_skip_of_the_head_is_done() {
        let body = format!(
            "<!-- pr-reviewer-review pr=7 -->\n<!-- pr-reviewer-status:skipped pr=7 sha={SHA} reason=docs-only -->"
        );
        let comments = [comment(1, "tao", &body, "2026-11-10T11:00:00Z")];
        assert!(matches!(
            activity(&[], &comments),
            PeerActivity::Done { .. }
        ));
    }

    #[test]
    fn a_peer_verdict_comment_for_the_head_is_done() {
        let body = format!(
            "<!-- pr-reviewer-review pr=7 -->\n<!-- pr-reviewer-verdict:comment pr=7 sha={SHA} event=COMMENT -->"
        );
        let comments = [comment(1, "tao", &body, "2026-11-10T11:00:00Z")];
        assert!(matches!(
            activity(&[], &comments),
            PeerActivity::Done { .. }
        ));
    }

    #[test]
    fn a_fresh_peer_in_progress_comment_is_in_progress() {
        let body = format!("<!-- pr-reviewer-status:in-progress pr=7 sha={SHA} -->\n\nLooking.");
        let comments = [comment(1, "tao", &body, "2026-11-10T11:50:00Z")];
        assert_eq!(
            activity(&[], &comments),
            PeerActivity::InProgress {
                login: "tao".to_string()
            }
        );
    }

    #[test]
    fn a_stale_peer_in_progress_comment_does_not_hold_the_review() {
        let body = format!("<!-- pr-reviewer-status:in-progress pr=7 sha={SHA} -->");
        let comments = [comment(1, "tao", &body, "2026-11-10T10:00:00Z")];
        assert_eq!(activity(&[], &comments), PeerActivity::None);
    }

    #[test]
    fn a_peer_in_progress_comment_for_an_older_head_is_ignored() {
        let body = "<!-- pr-reviewer-status:in-progress pr=7 sha=old -->";
        let comments = [comment(1, "tao", body, "2026-11-10T11:59:00Z")];
        assert_eq!(activity(&[], &comments), PeerActivity::None);
    }

    #[test]
    fn a_sha_prefix_does_not_match_a_longer_sha() {
        let body = format!("<!-- pr-reviewer-status:skipped pr=7 sha={SHA}ff reason=docs-only -->");
        let comments = [comment(1, "tao", &body, "2026-11-10T11:00:00Z")];
        assert_eq!(activity(&[], &comments), PeerActivity::None);
    }

    #[test]
    fn no_peers_means_no_activity() {
        let reviews = [review("tao", SHA, &metadata())];
        assert_eq!(
            peer_activity(&reviews, &[], &[], 7, SHA, 1800, now()),
            PeerActivity::None
        );
    }

    #[test]
    fn finds_the_peer_that_answered_a_command() {
        let body = format!(
            "<!-- pr-reviewer-command-response command=status pr=7 source_comment=42 sha={SHA} -->\n\nAll good."
        );
        let comments = [
            comment(
                1,
                "someone",
                "<!-- pr-reviewer-command-response command=status pr=7 source_comment=42 sha=x -->",
                "t",
            ),
            comment(2, "tao", &body, "t"),
        ];
        assert_eq!(peer_answered_command(&comments, &peers(), 42), Some("tao"));
        assert_eq!(peer_answered_command(&comments, &peers(), 4), None);
    }

    #[test]
    fn a_peer_with_auto_fix_off_answers_fix_but_leaves_the_repair() {
        let answer = |repair: &str| {
            format!(
                "<!-- pr-reviewer-command-response command=fix pr=7 source_comment=42 sha={SHA} repair={repair} -->\n\nOK."
            )
        };
        let off = [comment(1, "tao", &answer("off"), "t")];
        assert_eq!(peer_answered_command(&off, &peers(), 42), Some("tao"));
        assert_eq!(peer_took_repair(&off, &peers(), 42), None);

        let on = [comment(1, "tao", &answer("on"), "t")];
        assert_eq!(peer_took_repair(&on, &peers(), 42), Some("tao"));
    }

    fn review_comment(id: u64, login: &str, parent: Option<u64>) -> ReviewComment {
        ReviewComment {
            id,
            body: "text".to_string(),
            path: "a.rs".to_string(),
            line: Some(1),
            original_line: None,
            user: user(login),
            in_reply_to_id: parent,
            created_at: "t".to_string(),
        }
    }

    #[test]
    fn a_thread_started_by_a_peer_belongs_to_the_peer() {
        let comments = [
            review_comment(1, "tao", None),
            review_comment(2, "human", Some(1)),
            review_comment(3, "human", Some(2)),
        ];
        assert_eq!(peer_thread_owner(&comments, &peers(), 3), Some("tao"));
    }

    #[test]
    fn a_thread_started_by_anyone_else_has_no_peer_owner() {
        let comments = [
            review_comment(1, "me", None),
            review_comment(2, "tao", Some(1)),
        ];
        assert_eq!(peer_thread_owner(&comments, &peers(), 2), None);
    }
}

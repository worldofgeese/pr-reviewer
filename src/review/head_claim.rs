//! A claim on a pull request head, kept as a commit status on GitHub.
//!
//! Two pr-reviewer daemons that watch one repository can start a review
//! cycle on the same head. The in-progress comment and the peer delay narrow
//! that race. The head claim narrows it further: before a review cycle the
//! daemon sets a `pending` status with the context `pr-reviewer/claim` on the
//! head, and skips a head that another host claimed less than [`CLAIM_TTL`]
//! ago. It works with today's personal identities, because it needs only the
//! token that posts the review.
//!
//! The claim is a best-effort guard, not a lock. Two hosts that claim in the
//! same instant can both read their own claim back. A GitHub error never
//! stops a review cycle: the daemon logs it and reviews without a claim.
//!
//! GitHub statuses never expire, so a daemon that dies inside a review cycle
//! leaves a `pending` claim. [`release_abandoned`] ends it: at start-up for the
//! claims of this host, and later for any claim older than [`CLAIM_TTL`].

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::github::client::GitHubClient;
use crate::github::types::CommitStatus;

/// The commit status context that holds the claim.
pub const CLAIM_CONTEXT: &str = "pr-reviewer/claim";

/// The status of a review run that skipped a head because a peer holds it.
pub const SKIPPED_CLAIMED_BY_PEER: &str = "skipped:claimed-by-peer";

/// How long another host's claim holds the head before a peer takes over.
pub const CLAIM_TTL: Duration = Duration::minutes(15);

/// GitHub cuts a status description at 140 characters.
const MAX_HOST_CHARS: usize = 80;

/// What a daemon learns when it tries to claim a head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadClaim {
    /// This host holds the claim. Call [`release`] when the cycle ends.
    Held,
    /// A peer holds the head. Do not review it now.
    Contested { host: String },
    /// GitHub could not tell, or refused the claim. Review without one.
    Unavailable,
}

/// The name that this daemon puts in its claims.
pub fn local_host() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// Claim `sha` for `host`, unless a peer holds it.
pub async fn acquire(
    github: &GitHubClient,
    owner: &str,
    repo: &str,
    sha: &str,
    host: &str,
) -> HeadClaim {
    let read = match github.get_commit_statuses(owner, repo, sha).await {
        Ok(read) => read,
        Err(err) => {
            tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not read the head claim; reviewing without one");
            return HeadClaim::Unavailable;
        }
    };
    if let Some(peer) = fresh_claim_of_another_host(&read.statuses, host, read.server_time) {
        return HeadClaim::Contested { host: peer };
    }

    let description = describe("claimed", host, Utc::now());
    if let Err(err) = github
        .create_commit_status(owner, repo, sha, "pending", CLAIM_CONTEXT, &description)
        .await
    {
        tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not set the head claim; reviewing without one");
        return HeadClaim::Unavailable;
    }

    // Read the claim back: when two hosts claim together, the latest wins. Only
    // a fresh claim of a peer wins. Anything else is a read that lags behind
    // the write, so this host keeps its claim.
    match github.get_commit_statuses(owner, repo, sha).await {
        Ok(read) => match fresh_claim_of_another_host(&read.statuses, host, read.server_time) {
            Some(peer) => HeadClaim::Contested { host: peer },
            None => HeadClaim::Held,
        },
        Err(err) => {
            tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not read the head claim back; reviewing");
            HeadClaim::Held
        }
    }
}

/// Clear the claim of `host` when its review cycle ends or fails. GitHub has
/// no way to delete a status, so this posts a `success` status that says the
/// claim is released. A claim that a peer took over in the meantime stays.
pub async fn release(github: &GitHubClient, owner: &str, repo: &str, sha: &str, host: &str) {
    match github.get_commit_statuses(owner, repo, sha).await {
        Ok(read) => {
            if !latest_claim(&read.statuses).is_some_and(|latest| holds_claim(latest, host)) {
                tracing::info!(repo = %format!("{owner}/{repo}"), sha, "the head claim is no longer ours; leaving it");
                return;
            }
        }
        // Better to clear a claim that may be gone than to hold peers off.
        Err(err) => {
            tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not read the head claim before releasing it");
        }
    }
    let description = describe("released", host, Utc::now());
    if let Err(err) = github
        .create_commit_status(owner, repo, sha, "success", CLAIM_CONTEXT, &description)
        .await
    {
        tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not release the head claim; it expires by itself");
    }
}

/// Release the latest claim on `sha` when nobody can still be working on it,
/// so a daemon that died inside a review cycle does not leave the head
/// `pending` for good. GitHub statuses never expire, and a merge rule that
/// needs every check to finish waits on a `pending` one. Posts the same
/// `success` status as [`release`]. A fresh claim of a peer stays. See
/// [`abandoned_claim`] for what counts as abandoned. `include_own` is for
/// start-up, when no review cycle of `host` can be running.
pub async fn release_abandoned(
    github: &GitHubClient,
    owner: &str,
    repo: &str,
    sha: &str,
    host: &str,
    include_own: bool,
) {
    let read = match github.get_commit_statuses(owner, repo, sha).await {
        Ok(read) => read,
        Err(err) => {
            tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not read the head claim to look for an abandoned one");
            return;
        }
    };
    let Some(abandoned) = abandoned_claim(&read.statuses, host, read.server_time, include_own)
    else {
        return;
    };
    tracing::warn!(repo = %format!("{owner}/{repo}"), sha, claimed_by = %abandoned, "releasing an abandoned head claim");
    let description = describe("released", host, Utc::now());
    if let Err(err) = github
        .create_commit_status(owner, repo, sha, "success", CLAIM_CONTEXT, &description)
        .await
    {
        tracing::warn!(repo = %format!("{owner}/{repo}"), sha, error = %err, "could not release the abandoned head claim");
    }
}

fn describe(verb: &str, host: &str, now: DateTime<Utc>) -> String {
    let host: String = host.chars().take(MAX_HOST_CHARS).collect();
    format!(
        "{verb} by {host} at {}",
        now.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

fn latest_claim(statuses: &[CommitStatus]) -> Option<&CommitStatus> {
    statuses
        .iter()
        .find(|status| status.context == CLAIM_CONTEXT)
}

/// The host named by a `pending` claim, or `None` for any other status.
fn claim_host(status: &CommitStatus) -> Option<&str> {
    if status.context != CLAIM_CONTEXT || status.state != "pending" {
        return None;
    }
    let rest = status.description.as_deref()?.strip_prefix("claimed by ")?;
    rest.rsplit_once(" at ").map(|(host, _)| host)
}

fn holds_claim(status: &CommitStatus, host: &str) -> bool {
    let ours: String = host.chars().take(MAX_HOST_CHARS).collect();
    claim_host(status) == Some(ours.as_str())
}

/// The host of a claim that another host made less than [`CLAIM_TTL`] before
/// `now`. Pass GitHub's clock as `now`, so that a skewed clock on this host
/// cannot shorten or lengthen the claim.
fn fresh_claim_of_another_host(
    statuses: &[CommitStatus],
    host: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    let claim = latest_claim(statuses)?;
    let peer = claim_host(claim)?;
    if holds_claim(claim, host) {
        return None;
    }
    let claimed_at = DateTime::parse_from_rfc3339(&claim.created_at).ok()?;
    (now.signed_duration_since(claimed_at) < CLAIM_TTL).then(|| peer.to_string())
}

/// The host of the latest claim on a head when it is `pending` and
/// abandoned: older than [`CLAIM_TTL`] by GitHub's clock (`now`), whoever made
/// it, or made by `host` when `include_own` says that no cycle of `host` can
/// still run. A claim whose time cannot be read is left alone.
fn abandoned_claim<'a>(
    statuses: &'a [CommitStatus],
    host: &str,
    now: DateTime<Utc>,
    include_own: bool,
) -> Option<&'a str> {
    let claim = latest_claim(statuses)?;
    let claimer = claim_host(claim)?;
    if include_own && holds_claim(claim, host) {
        return Some(claimer);
    }
    let claimed_at = DateTime::parse_from_rfc3339(&claim.created_at).ok()?;
    (now.signed_duration_since(claimed_at) >= CLAIM_TTL).then_some(claimer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str, description: &str, created_at: &str) -> CommitStatus {
        CommitStatus {
            context: CLAIM_CONTEXT.to_string(),
            state: state.to_string(),
            description: Some(description.to_string()),
            created_at: created_at.to_string(),
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-11-11T07:10:00Z".parse().unwrap()
    }

    #[test]
    fn a_description_names_the_host_and_the_utc_time() {
        assert_eq!(
            describe("claimed", "mac-1", now()),
            "claimed by mac-1 at 2026-11-11T07:10:00Z"
        );
    }

    #[test]
    fn a_fresh_claim_of_another_host_holds_the_head() {
        let statuses = [status(
            "pending",
            "claimed by mac-2 at 2026-11-11T07:05:00Z",
            "2026-11-11T07:05:00Z",
        )];
        assert_eq!(
            fresh_claim_of_another_host(&statuses, "mac-1", now()),
            Some("mac-2".to_string())
        );
    }

    #[test]
    fn a_claim_older_than_the_ttl_does_not_hold_the_head() {
        let statuses = [status(
            "pending",
            "claimed by mac-2 at 2026-11-11T06:54:00Z",
            "2026-11-11T06:54:00Z",
        )];
        assert_eq!(fresh_claim_of_another_host(&statuses, "mac-1", now()), None);
    }

    #[test]
    fn our_own_claim_does_not_hold_the_head_against_us() {
        let statuses = [status(
            "pending",
            "claimed by mac-1 at 2026-11-11T07:09:00Z",
            "2026-11-11T07:09:00Z",
        )];
        assert_eq!(fresh_claim_of_another_host(&statuses, "mac-1", now()), None);
    }

    #[test]
    fn a_released_claim_does_not_hold_the_head() {
        let statuses = [status(
            "success",
            "released by mac-2 at 2026-11-11T07:09:00Z",
            "2026-11-11T07:09:00Z",
        )];
        assert_eq!(fresh_claim_of_another_host(&statuses, "mac-1", now()), None);
    }

    #[test]
    fn other_contexts_and_unreadable_claims_are_ignored() {
        let mut other = status(
            "pending",
            "claimed by mac-2 at 2026-11-11T07:09:00Z",
            "2026-11-11T07:09:00Z",
        );
        other.context = "ci/build".to_string();
        let garbled = status("pending", "somebody was here", "2026-11-11T07:09:00Z");
        assert_eq!(fresh_claim_of_another_host(&[other], "mac-1", now()), None);
        assert_eq!(
            fresh_claim_of_another_host(&[garbled], "mac-1", now()),
            None
        );
    }

    #[test]
    fn a_host_name_that_contains_at_still_parses() {
        let s = status(
            "pending",
            "claimed by lab at the-bench at 2026-11-11T07:09:00Z",
            "2026-11-11T07:09:00Z",
        );
        assert_eq!(claim_host(&s), Some("lab at the-bench"));
    }

    #[test]
    fn a_pending_claim_older_than_the_ttl_is_abandoned_whoever_made_it() {
        let statuses = [status(
            "pending",
            "claimed by mac-2 at 2026-11-11T06:54:00Z",
            "2026-11-11T06:54:00Z",
        )];
        assert_eq!(
            abandoned_claim(&statuses, "mac-1", now(), false),
            Some("mac-2")
        );
    }

    #[test]
    fn a_fresh_claim_of_a_peer_is_never_abandoned() {
        let statuses = [status(
            "pending",
            "claimed by mac-2 at 2026-11-11T07:05:00Z",
            "2026-11-11T07:05:00Z",
        )];
        assert_eq!(abandoned_claim(&statuses, "mac-1", now(), false), None);
        assert_eq!(abandoned_claim(&statuses, "mac-1", now(), true), None);
    }

    #[test]
    fn a_fresh_claim_of_this_host_is_abandoned_only_when_asked() {
        // Before a restart, no review cycle of this host can still run.
        let statuses = [status(
            "pending",
            "claimed by mac-1 at 2026-11-11T07:09:00Z",
            "2026-11-11T07:09:00Z",
        )];
        assert_eq!(abandoned_claim(&statuses, "mac-1", now(), false), None);
        assert_eq!(
            abandoned_claim(&statuses, "mac-1", now(), true),
            Some("mac-1")
        );
    }

    #[test]
    fn a_released_claim_is_not_abandoned() {
        let statuses = [status(
            "success",
            "released by mac-2 at 2026-11-11T06:00:00Z",
            "2026-11-11T06:00:00Z",
        )];
        assert_eq!(abandoned_claim(&statuses, "mac-1", now(), true), None);
    }

    #[test]
    fn a_claim_with_an_unreadable_time_is_left_alone() {
        let statuses = [status(
            "pending",
            "claimed by mac-2 at 2026-11-11T06:00:00Z",
            "not a time",
        )];
        assert_eq!(abandoned_claim(&statuses, "mac-1", now(), false), None);
    }
}

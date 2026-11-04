use crate::config::{ForkPolicy, RepoConfig};
use crate::github::types::PullRequest;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ForkDecision {
    Skip(String),
    Limited,
    Full,
}

pub fn evaluate_fork_policy(repo_cfg: &RepoConfig, pr: &PullRequest) -> ForkDecision {
    let author = pr.user.login.as_str();
    if repo_cfg
        .trusted_authors
        .iter()
        .any(|trusted| trusted.eq_ignore_ascii_case(author))
    {
        return ForkDecision::Full;
    }

    if !is_fork_pr(repo_cfg, pr) {
        return ForkDecision::Full;
    }

    match repo_cfg.fork_policy {
        ForkPolicy::Ignore => ForkDecision::Skip("fork PR ignored by policy".to_string()),
        ForkPolicy::Limited => ForkDecision::Limited,
        ForkPolicy::Full => ForkDecision::Full,
    }
}

pub fn is_fork_pr(repo_cfg: &RepoConfig, pr: &PullRequest) -> bool {
    let expected = format!("{}/{}", repo_cfg.owner, repo_cfg.name);
    match &pr.head.repo {
        Some(repo) => !repo.full_name.eq_ignore_ascii_case(&expected),
        None => true,
    }
}

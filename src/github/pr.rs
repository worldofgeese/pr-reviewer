use anyhow::Result;

use crate::github::client::GitHubClient;
use crate::github::types::PullRequest;

pub async fn get_pull_request(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PullRequest> {
    client.get_pull_request(owner, repo, number).await
}

pub async fn get_diff(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<String> {
    client.get_pr_diff(owner, repo, number).await
}

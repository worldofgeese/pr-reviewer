//! Stacked pull requests.
//!
//! A stacked PR targets the head branch of another PR instead of the trunk.
//! GitHub then shows only the PR's own change in its diff, but stack tools
//! (`gh stack sync`, `gt restack`, ...) rebase and force-push every PR above
//! a changed one. Every rebase gives each upper PR a new head SHA while its
//! own change stays the same, so reviewing per head SHA re-reviews the whole
//! stack on every push. This module detects stacks, fingerprints a PR's own
//! change so rebase-only pushes can be skipped, and describes the stack to
//! the reviewer.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::context::diff_parser::{DiffLine, DiffLineType, ParsedDiff};
use crate::github::client::GitHubClient;
use crate::github::types::PullRequest;
use crate::review::policy::{same_finding, FindingRef};
use crate::store::db::ReviewFindingRecord;

/// Longest base-branch chain followed when looking for ancestors.
const MAX_STACK_DEPTH: usize = 20;

/// Fingerprint of a PR's own change, overall and per file.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChangeFingerprint {
    pub overall: String,
    /// Per-file hashes, keyed by the file's path in the PR head (old path
    /// for deleted files). Used to tell which files changed between reviews.
    pub files: BTreeMap<String, String>,
}

impl ChangeFingerprint {
    /// Files whose own change differs from `previous`, including files
    /// that were added to or dropped from the diff.
    pub fn changed_files(&self, previous: &BTreeMap<String, String>) -> HashSet<String> {
        let mut changed: HashSet<String> = self
            .files
            .iter()
            .filter(|(path, hash)| previous.get(*path) != Some(*hash))
            .map(|(path, _)| path.clone())
            .collect();
        changed.extend(
            previous
                .keys()
                .filter(|path| !self.files.contains_key(*path))
                .cloned(),
        );
        changed
    }
}

/// Fingerprint of a PR's own change. Per file it hashes the paths and each
/// group of added and removed lines together with the unchanged line just
/// before and just after the group. Line numbers, the rest of the context,
/// `index` lines and trailing whitespace are left out.
///
/// So a rebase onto a moved base keeps the fingerprint: a cascade from a
/// stacked parent, a retarget to the trunk after the parent merges, or a
/// merge of the base branch. An edit to the change alters it, and so does
/// moving an added line past an unchanged one (a guard moved after the
/// call it guards). A lower PR that edits a line directly next to this
/// PR's change also alters it, which costs one extra review.
///
/// Returns `None` when the diff has no text hunks to compare, such as a
/// binary or mode-only change, or a file too large for a patch: those
/// pushes are always reviewed.
pub fn change_fingerprint(diff: &ParsedDiff) -> Option<ChangeFingerprint> {
    if diff.files.is_empty() || diff.files.iter().any(|file| file.hunks.is_empty()) {
        return None;
    }
    let mut files = BTreeMap::new();
    for file in &diff.files {
        let mut hasher = Sha256::new();
        hasher.update(file.old_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.new_path.as_bytes());
        hasher.update(b"\n");
        for hunk in &file.hunks {
            hash_change_groups(&mut hasher, &hunk.lines);
        }
        let key = if file.new_path == "/dev/null" {
            file.old_path.clone()
        } else {
            file.new_path.clone()
        };
        files.insert(key, format!("{:x}", hasher.finalize()));
    }
    let mut overall = Sha256::new();
    for (path, hash) in &files {
        overall.update(path.as_bytes());
        overall.update(b"\0");
        overall.update(hash.as_bytes());
        overall.update(b"\n");
    }
    Some(ChangeFingerprint {
        overall: format!("{:x}", overall.finalize()),
        files,
    })
}

fn hash_change_groups(hasher: &mut Sha256, lines: &[DiffLine]) {
    let mut index = 0;
    while index < lines.len() {
        if lines[index].line_type == DiffLineType::Context {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && lines[index].line_type != DiffLineType::Context {
            index += 1;
        }
        let before = start
            .checked_sub(1)
            .map(|i| lines[i].content.trim_end())
            .unwrap_or_default();
        let after = lines
            .get(index)
            .map(|line| line.content.trim_end())
            .unwrap_or_default();
        hasher.update(b"@ ");
        hasher.update(before.as_bytes());
        hasher.update(b"\n");
        for line in &lines[start..index] {
            hasher.update(if line.line_type == DiffLineType::Added {
                b"+"
            } else {
                b"-"
            });
            hasher.update(line.content.trim_end().as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(b"@ ");
        hasher.update(after.as_bytes());
        hasher.update(b"\n");
    }
}

/// A PR below this one in its stack.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StackAncestor {
    pub number: u64,
    pub title: String,
    pub head_ref: String,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct StackInfo {
    /// GitHub's native stack number (`gh stack`), if the PR is in one.
    pub native_number: Option<u64>,
    pub position: Option<u32>,
    pub size: Option<u32>,
    /// The branch the stack lands on, usually the default branch.
    pub trunk: String,
    /// Ancestors, from the direct parent down to the bottom of the stack.
    pub ancestors: Vec<StackAncestor>,
}

impl StackInfo {
    pub fn parent(&self) -> Option<&StackAncestor> {
        self.ancestors.first()
    }
}

/// The branch a stack lands on: the native stack's base, else the
/// repository's default branch, else `main`.
pub fn trunk_of(pr: &PullRequest) -> String {
    pr.stack
        .as_ref()
        .and_then(|stack| stack.base.as_ref())
        .and_then(|base| base.ref_name.clone())
        .or_else(|| pr.base.repo.default_branch.clone())
        .unwrap_or_else(|| "main".to_string())
}

/// The open PR in `repo` whose head branch is `base_ref`, if it is not one
/// already visited. PRs from forks never count: a stack lives in one repo,
/// and a fork could otherwise claim to be any PR's parent.
pub fn pick_parent<'a>(
    candidates: &'a [PullRequest],
    repo_full_name: &str,
    base_ref: &str,
    visited: &HashSet<u64>,
) -> Option<&'a PullRequest> {
    candidates.iter().find(|candidate| {
        candidate.state == "open"
            && candidate.head.ref_name == base_ref
            && candidate
                .head
                .repo
                .as_ref()
                .is_some_and(|repo| repo.full_name.eq_ignore_ascii_case(repo_full_name))
            && !visited.contains(&candidate.number)
    })
}

/// Stack membership of `pr`, from GitHub's native `stack` field and by
/// following base branches to the PRs that own them. `None` for a PR that
/// is not part of any stack.
pub async fn resolve_stack(
    github: &GitHubClient,
    owner: &str,
    repo: &str,
    pr: &PullRequest,
) -> Result<Option<StackInfo>> {
    let trunk = trunk_of(pr);
    let repo_full_name = format!("{owner}/{repo}");
    let mut visited = HashSet::from([pr.number]);
    let mut ancestors = Vec::new();
    let mut base_ref = pr.base.ref_name.clone();

    while base_ref != trunk && ancestors.len() < MAX_STACK_DEPTH {
        let candidates = github
            .find_pull_requests_by_head(owner, repo, &base_ref)
            .await?;
        let Some(parent) = pick_parent(&candidates, &repo_full_name, &base_ref, &visited) else {
            break;
        };
        visited.insert(parent.number);
        ancestors.push(StackAncestor {
            number: parent.number,
            title: parent.title.clone(),
            head_ref: parent.head.ref_name.clone(),
        });
        base_ref = parent.base.ref_name.clone();
    }

    if ancestors.is_empty() && pr.stack.is_none() {
        return Ok(None);
    }
    let native = pr.stack.as_ref();
    Ok(Some(StackInfo {
        native_number: native.and_then(|stack| stack.number),
        position: native.and_then(|stack| stack.position),
        size: native.and_then(|stack| stack.size),
        trunk,
        ancestors,
    }))
}

/// The ancestor PR that already has an open finding matching this one.
pub fn ancestor_with_same_finding(
    finding: FindingRef<'_>,
    ancestor_findings: &[ReviewFindingRecord],
) -> Option<u64> {
    ancestor_findings
        .iter()
        .find(|stored| {
            same_finding(
                finding,
                FindingRef {
                    path: &stored.path,
                    line: stored.line.map(|line| line as u32),
                    body: &stored.body,
                },
            )
        })
        .map(|stored| stored.pr_number as u64)
}

/// Prompt section that tells the reviewer where this PR sits in its stack.
pub fn build_stack_context(
    pr: &PullRequest,
    info: &StackInfo,
    ancestor_findings: &[ReviewFindingRecord],
) -> String {
    let mut out = String::new();
    let native = match (info.native_number, info.position, info.size) {
        (Some(number), Some(position), Some(size)) => {
            format!(" (GitHub stack #{number}, position {position} of {size}, bottom first)")
        }
        _ => String::new(),
    };
    match info.parent() {
        Some(parent) => {
            out.push_str(&format!(
                "This PR is part of a stack of pull requests{native}. Its base branch `{}` is the head branch of #{} (\"{}\"), so the diff above contains only this PR's own change on top of #{}.\n",
                pr.base.ref_name, parent.number, parent.title, parent.number,
            ));
            let lower = info
                .ancestors
                .iter()
                .map(|ancestor| format!("#{}", ancestor.number))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "Lower PRs in the stack, nearest first: {lower}. They land on `{}` before this one, and their changes are reviewed on those PRs.\n",
                info.trunk,
            ));
            out.push_str("- Report findings only on lines this PR changes. Do not report problems in code that a lower PR introduced. If this PR depends on such a problem, say so in one sentence in the summary and name the PR.\n");
            out.push_str(&format!(
                "- A checkout of `{}` does not contain the lower PRs' changes. Files and symbols that only a lower PR adds are expected; their absence there is not a defect of this PR.\n",
                info.trunk,
            ));
        }
        None => {
            out.push_str(&format!(
                "This PR is the bottom of a stack of pull requests{native}. PRs above it build on its change, so flag interface or behavior changes that later PRs would have to absorb.\n",
            ));
        }
    }
    if !ancestor_findings.is_empty() {
        out.push_str("\n### Findings already open on lower PRs\nThese are tracked on their own PRs. Do not repeat them here.\n");
        for finding in ancestor_findings.iter().take(20) {
            let line = finding
                .line
                .map(|line| line.to_string())
                .unwrap_or_else(|| "?".to_string());
            let preview: String = finding.body.chars().take(200).collect();
            out.push_str(&format!(
                "- #{} `{}:{}` [{}] {}\n",
                finding.pr_number,
                finding.path,
                line,
                finding.severity,
                preview.replace('\n', " "),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::diff_parser::parse_unified_diff;
    use crate::github::types::{
        PullRequestBase, PullRequestHead, PullRequestStack, PullRequestStackBase, RepoRef, User,
    };

    fn repo_ref(full_name: &str) -> RepoRef {
        RepoRef {
            full_name: full_name.to_string(),
            fork: false,
            owner: User {
                login: full_name.split('/').next().unwrap().to_string(),
                account_type: None,
            },
            default_branch: Some("main".to_string()),
        }
    }

    fn pr(number: u64, head: &str, base: &str, state: &str) -> PullRequest {
        PullRequest {
            number,
            title: format!("Layer {number}"),
            body: None,
            draft: false,
            state: state.to_string(),
            user: User {
                login: "alice".to_string(),
                account_type: None,
            },
            head: PullRequestHead {
                sha: format!("sha-{number}"),
                ref_name: head.to_string(),
                repo: Some(repo_ref("o/r")),
            },
            base: PullRequestBase {
                ref_name: base.to_string(),
                repo: repo_ref("o/r"),
            },
            html_url: None,
            updated_at: None,
            closed_at: None,
            merged_at: None,
            stack: None,
        }
    }

    fn stored(pr_number: i64, path: &str, line: i64, body: &str) -> ReviewFindingRecord {
        ReviewFindingRecord {
            repo: "o/r".to_string(),
            pr_number,
            author: None,
            fingerprint: format!("{path}|{line}"),
            recurrence_fingerprint: None,
            first_sha: "s".to_string(),
            last_seen_sha: "s".to_string(),
            status: "open".to_string(),
            severity: "warning".to_string(),
            finding_kind: "correctness".to_string(),
            path: path.to_string(),
            line: Some(line),
            body: body.to_string(),
            evidence_note: None,
            github_comment_id: None,
            resolved_by_sha: None,
            resolution_reason: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    const CHILD_DIFF: &str = "diff --git a/src/api.rs b/src/api.rs
index 1111111..2222222 100644
--- a/src/api.rs
+++ b/src/api.rs
@@ -10,6 +10,7 @@ fn handler() {
     let a = 1;
     let b = 2;
-    call(a);
+    call(a, b);
+    log(b);
     done();
 }
";

    // The same change after the parent PR inserted lines above it and
    // edited a farther context line: new index line, hunk header and context.
    const CHILD_DIFF_REBASED: &str = "diff --git a/src/api.rs b/src/api.rs
index 3333333..4444444 100644
--- a/src/api.rs
+++ b/src/api.rs
@@ -14,6 +14,7 @@ fn handler() {
     let a = 10;
     let b = 2;
-    call(a);
+    call(a, b);
+    log(b);
     done();
 }
";

    const CHILD_DIFF_EDITED: &str = "diff --git a/src/api.rs b/src/api.rs
index 3333333..5555555 100644
--- a/src/api.rs
+++ b/src/api.rs
@@ -14,6 +14,7 @@ fn handler() {
     let a = 10;
     let b = 2;
-    call(a);
+    call(a, b);
+    log(a);
     done();
 }
";

    fn fingerprint(diff: &str) -> String {
        change_fingerprint(&parse_unified_diff(diff).unwrap())
            .expect("text diff has a fingerprint")
            .overall
    }

    #[test]
    fn rebase_keeps_the_change_fingerprint() {
        assert_eq!(fingerprint(CHILD_DIFF), fingerprint(CHILD_DIFF_REBASED));
    }

    #[test]
    fn editing_the_change_alters_the_fingerprint() {
        assert_ne!(
            fingerprint(CHILD_DIFF_REBASED),
            fingerprint(CHILD_DIFF_EDITED)
        );
    }

    #[test]
    fn renames_and_file_order_are_part_of_the_fingerprint() {
        let renamed = CHILD_DIFF.replace("a/src/api.rs", "a/src/old_api.rs");
        assert_ne!(fingerprint(CHILD_DIFF), fingerprint(&renamed));

        let second = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let forward = format!("{CHILD_DIFF}{second}");
        let backward = format!("{second}{CHILD_DIFF}");
        assert_eq!(fingerprint(&forward), fingerprint(&backward));
    }

    #[test]
    fn trunk_prefers_native_stack_base_then_default_branch() {
        let mut plain = pr(1, "feat/a", "feat/base", "open");
        assert_eq!(trunk_of(&plain), "main");
        plain.base.repo.default_branch = Some("develop".to_string());
        assert_eq!(trunk_of(&plain), "develop");
        plain.stack = Some(PullRequestStack {
            number: Some(9),
            size: Some(3),
            position: Some(2),
            base: Some(PullRequestStackBase {
                ref_name: Some("release".to_string()),
            }),
        });
        assert_eq!(trunk_of(&plain), "release");
    }

    #[test]
    fn parent_must_be_open_same_repo_and_unvisited() {
        let mut fork = pr(2, "feat/a", "main", "open");
        fork.head.repo = Some(repo_ref("mallory/r"));
        let closed = pr(3, "feat/a", "main", "closed");
        let open = pr(4, "feat/a", "main", "open");
        let candidates = vec![fork, closed, open];
        let visited = HashSet::new();
        assert_eq!(
            pick_parent(&candidates, "o/r", "feat/a", &visited).map(|p| p.number),
            Some(4)
        );
        assert!(pick_parent(&candidates, "o/r", "feat/a", &HashSet::from([4])).is_none());
        assert!(pick_parent(&candidates, "o/r", "feat/other", &visited).is_none());
    }

    #[tokio::test]
    async fn resolve_stack_walks_the_base_chain_to_the_trunk() {
        use httptest::{matchers::*, responders::*, Expectation, Server};
        let server = Server::run();
        let lookup = |head: &str, body: serde_json::Value| {
            Expectation::matching(all_of![
                request::method_path("GET", "/repos/o/r/pulls"),
                request::query(url_decoded(contains(("head", format!("o:{head}"))))),
            ])
            .respond_with(json_encoded(body))
        };
        let as_json = |p: &PullRequest| {
            serde_json::json!({
                "number": p.number, "title": p.title, "state": p.state,
                "user": {"login": "alice"},
                "head": {"sha": p.head.sha, "ref": p.head.ref_name,
                         "repo": {"full_name": "o/r", "owner": {"login": "o"}}},
                "base": {"ref": p.base.ref_name,
                         "repo": {"full_name": "o/r", "owner": {"login": "o"}, "default_branch": "main"}}
            })
        };
        let bottom = pr(11, "feat/bottom", "main", "open");
        let middle = pr(12, "feat/middle", "feat/bottom", "open");
        server.expect(lookup("feat/middle", serde_json::json!([as_json(&middle)])));
        server.expect(lookup("feat/bottom", serde_json::json!([as_json(&bottom)])));

        let client = GitHubClient::new_with_base("t".into(), &server.url_str("")).unwrap();
        let top = pr(13, "feat/top", "feat/middle", "open");
        let info = resolve_stack(&client, "o", "r", &top)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info.ancestors.iter().map(|a| a.number).collect::<Vec<_>>(),
            vec![12, 11]
        );
        assert_eq!(info.trunk, "main");

        // A plain PR to the trunk makes no lookups and is not a stack.
        let plain = pr(500, "feat/plain", "main", "open");
        assert!(resolve_stack(&client, "o", "r", &plain)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn retargeted_child_is_the_bottom_of_its_native_stack() {
        // After the parent merges, GitHub retargets the child to the trunk.
        // The base chain ends at once, but the native stack still lists it.
        let client = GitHubClient::new_with_base("t".into(), "http://127.0.0.1:9").unwrap();
        let mut child = pr(12, "feat/middle", "main", "open");
        child.stack = Some(PullRequestStack {
            number: Some(90),
            size: Some(7),
            position: Some(1),
            base: Some(PullRequestStackBase {
                ref_name: Some("main".to_string()),
            }),
        });
        let info = resolve_stack(&client, "o", "r", &child)
            .await
            .unwrap()
            .unwrap();
        assert!(info.parent().is_none());
        let context = build_stack_context(&child, &info, &[]);
        assert!(context.contains("bottom of a stack"), "{context}");
        assert!(
            context.contains("GitHub stack #90, position 1 of 7"),
            "{context}"
        );
    }

    #[test]
    fn stack_context_names_the_parent_and_lists_its_open_findings() {
        let top = pr(13, "feat/top", "feat/middle", "open");
        let info = StackInfo {
            native_number: Some(90),
            position: Some(3),
            size: Some(8),
            trunk: "main".to_string(),
            ancestors: vec![
                StackAncestor {
                    number: 12,
                    title: "add cache layer".to_string(),
                    head_ref: "feat/middle".to_string(),
                },
                StackAncestor {
                    number: 11,
                    title: "add fetcher".to_string(),
                    head_ref: "feat/bottom".to_string(),
                },
            ],
        };
        let open = vec![stored(
            12,
            "src/auth/guard.sh",
            12,
            "fails open when jq fails",
        )];
        let context = build_stack_context(&top, &info, &open);
        assert!(
            context.contains("head branch of #12 (\"add cache layer\")"),
            "{context}"
        );
        assert!(context.contains("nearest first: #12, #11"), "{context}");
        assert!(context.contains("position 3 of 8"), "{context}");
        assert!(
            context.contains("does not contain the lower PRs' changes"),
            "{context}"
        );
        assert!(context.contains("#12 `src/auth/guard.sh:12`"), "{context}");
    }

    #[test]
    fn overlapping_child_finding_is_attributed_to_the_ancestor() {
        let ancestors = vec![
            stored(
                12,
                "src/auth/guard.sh",
                40,
                "The guard fails open when `jq` fails: an empty owner passes the check and the write goes through.",
            ),
            stored(11, "src/other.sh", 3, "Unrelated finding about the other script."),
        ];
        let duplicate = FindingRef {
            path: "src/auth/guard.sh",
            line: Some(44),
            body: "When `jq` fails the guard fails open: the owner is empty, the check passes, and the write goes through.",
        };
        let new_issue = FindingRef {
            path: "src/auth/guard.sh",
            line: Some(90),
            body: "The new `dep relate` branch checks only the first bead id, so the second bead can belong to another owner.",
        };
        assert_eq!(ancestor_with_same_finding(duplicate, &ancestors), Some(12));
        assert_eq!(ancestor_with_same_finding(new_issue, &ancestors), None);
    }

    #[test]
    fn moving_an_added_line_past_unchanged_code_alters_the_fingerprint() {
        let guard_first = "diff --git a/src/admin.rs b/src/admin.rs
--- a/src/admin.rs
+++ b/src/admin.rs
@@ -1,3 +1,4 @@
 fn wipe(user: &User, db: &Db) {
+    authorize(user);
     delete_all(db);
 }
";
        let guard_after = "diff --git a/src/admin.rs b/src/admin.rs
--- a/src/admin.rs
+++ b/src/admin.rs
@@ -1,3 +1,4 @@
 fn wipe(user: &User, db: &Db) {
     delete_all(db);
+    authorize(user);
 }
";
        assert_ne!(fingerprint(guard_first), fingerprint(guard_after));
    }

    #[test]
    fn an_edit_next_to_the_change_counts_as_a_change() {
        // Costs one extra review when a lower PR edits the adjacent line,
        // the safe direction.
        let adjacent = CHILD_DIFF_REBASED.replace("     done();", "     finish();");
        assert_ne!(fingerprint(CHILD_DIFF_REBASED), fingerprint(&adjacent));
    }

    #[test]
    fn diffs_without_text_hunks_have_no_fingerprint() {
        let binary = format!(
            "{CHILD_DIFF}diff --git a/logo.png b/logo.png\nindex 1..2 100644\nBinary files a/logo.png and b/logo.png differ\n"
        );
        assert!(change_fingerprint(&parse_unified_diff(&binary).unwrap()).is_none());
        assert!(change_fingerprint(&parse_unified_diff("").unwrap()).is_none());
    }

    #[test]
    fn changed_files_reports_edited_added_and_dropped_files() {
        let second = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let before =
            change_fingerprint(&parse_unified_diff(&format!("{CHILD_DIFF}{second}")).unwrap())
                .unwrap();
        let rebased = change_fingerprint(
            &parse_unified_diff(&format!("{CHILD_DIFF_REBASED}{second}")).unwrap(),
        )
        .unwrap();
        assert!(rebased.changed_files(&before.files).is_empty());

        let edited = change_fingerprint(&parse_unified_diff(CHILD_DIFF_EDITED).unwrap()).unwrap();
        let changed = edited.changed_files(&before.files);
        assert_eq!(
            changed,
            HashSet::from(["src/api.rs".to_string(), "README.md".to_string()]),
            "api.rs changed and README.md left the diff"
        );
    }
}

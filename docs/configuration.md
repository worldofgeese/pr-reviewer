# Configuration reference

All configuration lives at `~/.config/pr-reviewer/config.toml`. Every field can be inspected and set via CLI:

```bash
pr-reviewer config list
pr-reviewer config get harness.default
pr-reviewer config set daemon.poll_interval_secs 60
```

---

## Config precedence

Settings are resolved in this order, from highest to lowest priority:

```
per-repo config field
    > global config field
        > hardcoded default
```

For example, if a repo sets `harness = "codex"` but the global `[harness]` section sets `default = "claude-code"`, that repo uses Codex. A repo with no `harness` field falls back to the global default.

---

## `[harness]`

Controls which AI CLI tool runs reviews and how long to wait for a response.

```toml
[harness]
default = "claude-code"   # claude-code | opencode | codex
model = "claude-sonnet-4-6"
reasoning_effort = "low"  # optional, currently used by codex
timeout_secs = 600
```

| Field | Default | Description |
|-------|---------|-------------|
| `default` | `claude-code` | Harness to use when no per-repo override is set |
| `model` | `claude-sonnet-4-6` | Model identifier passed to the harness CLI |
| `reasoning_effort` | unset | Optional reasoning effort override. When using Codex, pr-reviewer passes this through as `-c model_reasoning_effort="..."` |
| `timeout_secs` | `600` | Seconds to wait for a harness response before treating the review as failed |

### Harness selection guide

| Harness | CLI | Best for |
|---------|-----|----------|
| `claude-code` | `claude` | Highest review quality; requires Claude Pro/Max subscription |
| `opencode` | `opencode` | Open-source alternative; supports multiple providers via config |
| `codex` | `codex` | OpenAI Codex; fastest option for high-volume repos |

Model names are passed directly to the harness CLI. Use whatever model identifier that CLI accepts — pr-reviewer does no validation.

For Codex, `reasoning_effort` can be `none`, `low`, `medium`, `high`, or `xhigh`. Leave it unset to use your normal Codex defaults.

---

## `[daemon]`

Controls polling behavior and concurrency.

```toml
[daemon]
poll_interval_secs = 30
max_poll_interval_secs = 300
mode = "poll"
webhook_port = 3847
max_concurrent_reviews = 2
```

| Field | Default | Description |
|-------|---------|-------------|
| `poll_interval_secs` | `30` | Base polling interval in seconds. Adaptive backoff starts here. |
| `max_poll_interval_secs` | `300` | Maximum backoff ceiling. The daemon will never wait longer than this between polls. |
| `mode` | `poll` | `poll` is the only production-ready mode; `webhook` is not yet complete |
| `webhook_port` | `3847` | Port for webhook mode (reserved for future use) |
| `max_concurrent_reviews` | `2` | Maximum number of harness processes running in parallel |

### Adaptive backoff

The daemon starts polling at `poll_interval_secs` and backs off exponentially (with jitter) when:

- All repos return `304 Not Modified` (nothing changed — back off)
- The GitHub rate limit is nearly exhausted

It resets to the base interval when a new PR is found. The ceiling is `max_poll_interval_secs`.

For repos with low PR volume, you can set both values higher to reduce unnecessary API calls:

```bash
pr-reviewer config set daemon.poll_interval_secs 120
pr-reviewer config set daemon.max_poll_interval_secs 600
```

### Concurrency

`max_concurrent_reviews` controls how many harness processes can run at the same time. Each harness invocation is CPU and memory intensive. On a machine with limited resources or a Claude Pro subscription (which has its own concurrency limits), keep this at `1` or `2`.

---

## `[defaults]`

Review behavior defaults applied to all repos unless overridden.

```toml
[defaults]
auto_review = true
review_drafts = false
max_files = 50
max_diff_lines = 3000
max_file_size_bytes = 100000
bot_name = "pr-reviewer"
dry_run = false
max_prompt_bytes = 204800
```

| Field | Default | Description |
|-------|---------|-------------|
| `auto_review` | `true` | Whether the daemon auto-reviews new PRs. Set to `false` to only allow manual `review` commands. |
| `review_drafts` | `false` | Whether to review draft PRs. |
| `max_files` | `50` | Maximum number of changed files to include in context. PRs with more files are truncated. |
| `max_diff_lines` | `3000` | Maximum diff lines to include. Large diffs are truncated at this limit. |
| `max_file_size_bytes` | `100000` | Maximum size of a single file to include as context. Larger files are skipped. |
| `bot_name` | `pr-reviewer` | GitHub username of the bot account. Used for self-review detection and deduplication. |
| `dry_run` | `false` | If `true`, run the full pipeline but log the output instead of posting to GitHub. |
| `max_prompt_bytes` | `204800` | Maximum prompt size sent to the harness (200KB). Context is trimmed to fit. |
| `max_review_cycles` | `2` | Review rounds that may post inline findings. After this many rounds, new findings that are not critical become non-blocking notes. `0` = no limit. See [Review cycles](#review-cycles). |
| `critical_finding_kinds` | `["security", "secret_exposure", "data_integrity", "correctness"]` | Finding kinds that can still be posted inline, and request changes, after the limit. |
| `skip_unchanged_diff` | `true` | Skip the review of a push that leaves the PR's own diff unchanged, such as a rebase. See [Stacked PRs](#stacked-prs). |
| `stack_aware` | `true` | Detect stacked PRs, tell the reviewer about the parent PR, and drop findings that a lower PR already tracks. |
| `dismiss_stale_change_requests` | `true` | Dismiss the bot's earlier `REQUEST_CHANGES` reviews when no blocking finding is open. |

### Tuning context limits

Smaller limits = faster reviews, lower cost, less risk of hitting model context windows.
Larger limits = more complete reviews for large PRs, but slower and potentially truncated by the model.

For repos with typically small PRs (e.g., docs, config changes):

```bash
pr-reviewer config set defaults.max_diff_lines 1000
pr-reviewer config set defaults.max_files 20
```

For repos with large, complex PRs:

```bash
pr-reviewer config set defaults.max_diff_lines 5000
pr-reviewer config set defaults.max_prompt_bytes 307200   # 300KB
```

---

## Review cycles

pr-reviewer reviews every new head commit of a PR. Each posted review is one **review round**. Without a limit, every round can add new inline findings, and each one opens a thread that someone must resolve. `max_review_cycles` stops that loop.

| Round | What the bot does |
|-------|-------------------|
| 1 | Full review. The prompt asks for every finding now, because later rounds focus on fixes. |
| 2 to `max_review_cycles` | Verification. The bot checks its earlier findings first. It reports a new finding only for code that changed since its last review, or for a real bug, security or data-integrity problem. |
| After `max_review_cycles` | Follow-up. New findings go into a **Follow-up notes (non-blocking)** section of the review body. They do not open threads and cannot request changes. |

A **critical finding** is the exception. After the limit it is still posted inline, and it can request changes. A finding is critical when all three are true:

- its severity is `blocking`,
- its `finding_kind` is in `critical_finding_kinds`,
- it has an `evidence_note` that names the changed lines.

After the limit, a critical finding must also sit on a line the PR adds or removes. A critical finding elsewhere is listed first in the notes, as a "possible blocker". The default `critical_finding_kinds` includes `correctness`, so a proven blocking bug in changed code can still stop a merge in any round. To bound blocking findings too, remove `correctness` from the list. The cost of the changed-line rule: after the limit, a break in a file the PR did not touch, such as a caller of a changed function, is only a note.

A round is one reviewed commit. The bot counts its earlier reviews on GitHub, so the count survives a reset of its database. If GitHub does not return the list of reviews, the bot tries once more and then fails the review. It does not guess the round.

The bot also does not repeat itself:

- If a new finding matches one of its threads that is still open, the bot does not post it again. The finding stays open, and a blocking one still counts as blocking. If that is all the bot has to say, it posts no review: the thread and the earlier change request still stand.
- If a human resolved the thread, the finding is dropped. A critical finding opens it again only when this PR's change to that file is different from the last review.
- A follow-up note is not repeated in later rounds.
- A stored finding is marked fixed only when this PR's change to its file is different from the last review, or the file left the diff. A push that leaves the file's change as it was cannot fix it.

Two findings match when they are in the same file and share most of their meaningful words. If both quote code identifiers in backticks, they must share one, so the same sentence about `delete_user` and about `update_user` stays two findings. To hide a new finding as "still open" or "resolved", the match must also be within 15 lines. The same rule decides which findings a reply in a review thread, such as "this is intentional", covers, so the reply does not hide unrelated findings in the same file. A comment in the PR conversation that puts a topic out of scope ("not in this PR") is broader: it hides findings on that topic in any file, unless the finding has an `evidence_note`. Critical findings always have one, so they are never hidden this way.

The bot never approves a PR. So its `REQUEST_CHANGES` review would stay on the PR after the findings are fixed. With `dismiss_stale_change_requests`, the bot dismisses its own earlier change requests when no blocking finding is open in its records, after a posted review or after a review with nothing new. If it has no records for the PR, it leaves the change request alone.

Set `max_review_cycles = 0` to keep the old behavior: every round posts every finding inline.

---

## Stacked PRs

A **stacked PR** targets the head branch of another PR instead of the default branch. Its diff shows only its own change. Stack tools such as `gh stack`, Graphite (`gt`) and git-spice rebase and force-push every PR above a PR that changed. Each upper PR then gets a new head commit, although its own change is the same.

With `skip_unchanged_diff`, pr-reviewer stores a fingerprint of each reviewed diff, once the review is on GitHub. The fingerprint covers the file paths, the added and removed lines, and the unchanged line just before and just after each group of changes. It leaves out line numbers, the other context lines and `index` lines. If a new head commit has the same fingerprint as the last reviewed one, the review is skipped (`skipped:unchanged-diff` in `pr-reviewer logs`). The skip does not use up a review round. These pushes keep the fingerprint:

- a rebase onto a moved base branch, for example after a change in a lower PR,
- the retarget to the default branch after the parent PR merges,
- a merge of the base branch into the PR branch.

Any change to the PR's own added or removed lines makes the bot review it again. So does moving an added line past an unchanged one, such as a guard moved after the call it guards. A diff with a file that has no text hunks (a binary file, a mode change, a file too large for a patch) has no fingerprint, so it is always reviewed. Use `pr-reviewer review owner/repo#N --force` to review an unchanged diff anyway.

With `stack_aware`, pr-reviewer finds the PR's place in its stack:

1. GitHub's native stack (`gh stack`) adds a `stack` object to the PR. It gives the stack number, the PR's position and the stack size.
2. For other tools, the bot follows the base branch to the open PR in the same repository whose head branch it is, and repeats this down to the default branch. PRs from forks are never used as parents.

The review prompt then names the parent PR and lists the lower PRs. It tells the reviewer to report findings only on lines this PR changes, and that a checkout of the default branch does not contain the lower PRs' changes. Open findings of the lower PRs are listed in the prompt. A new finding on a line this PR does not change, that matches one of them, is not posted on the upper PR, because the lower PR already tracks it. A finding on a line this PR changes is always this PR's own, and a critical finding is never dropped this way. Findings stay on the PR where they were raised, also after other PRs in the stack merge or rebase.

Limits:

- Stacks are found by branch name. A PR that targets a long-lived branch, such as `develop`, while an open PR merges `develop` into `main`, is treated as stacked on that PR.
- A skipped push is not reviewed against the new base. A conflict resolution that changes the PR's own lines is reviewed, but a semantic break that a lower PR causes is not. CI catches those.
- The fingerprint covers text hunks only. A change to a binary file alone does not change it.
- If a lower branch is force-pushed and the upper branch is not rebased yet, the upper PR's diff can briefly include the lower PR's commits. A review in that window sees the larger diff. `gh stack` pushes all branches at once, which keeps the window short.

---

## `[instruction_prs]`

Controls whether pr-reviewer turns repeated review findings into AGENTS.md guardrail PRs. This is disabled by default.

```toml
[instruction_prs]
enabled = false
mode = "conservative" # conservative | broad
min_distinct_prs = 2
branch_prefix = "pr-reviewer/agents-guardrail"
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | If `true`, recurring reviewer findings can open a real PR against `AGENTS.md`. |
| `mode` | `conservative` | `conservative` adds a small guardrail entry. `broad` writes a larger section entry that can cover documentation drift and stale repo guidance. |
| `min_distinct_prs` | `2` | Minimum number of distinct PRs where the same finding must appear before a suggestion PR is opened. The same author across unrelated PRs still counts. |
| `branch_prefix` | `pr-reviewer/agents-guardrail` | Prefix for generated suggestion branches. |

Only pr-reviewer's own posted findings are tracked. Findings suppressed by prior-review memory, human pushback, or duplicate detection are not counted, and repeated examples inside a single PR do not trigger a suggestion by themselves.

---

## `[[repos]]`

Per-repo configuration. Each `[[repos]]` block in the TOML array adds one repo to the watch list. All fields except `owner` and `name` are optional and override the global defaults.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
local_path = "/optional/existing/clone"
harness = "claude-code"
model = "claude-sonnet-4-6"
fork_policy = "ignore"
trusted_authors = ["dependabot[bot]", "renovate[bot]"]
ignore_paths = ["*.lock", "dist/**", "vendor/**"]
custom_instructions = "This project uses a custom ORM. Watch for N+1 queries."
gitnexus = true

[repos.auto_fix]
enabled = false
scan_default_branch = false
base_branch = "main"
branch_prefix = "pr-reviewer/auto-fix"
draft_pr = true
max_changed_files = 20
max_repairs_per_pr = 10
max_repairs_per_head = 1
```

| Field | Description |
|-------|-------------|
| `owner` | GitHub organization or username |
| `name` | Repository name |
| `local_path` | Path to an existing local clone. If omitted, the repo is auto-cloned to `~/.config/pr-reviewer/repos/{owner}/{name}/` |
| `harness` | Override the global harness for this repo |
| `model` | Override the global model for this repo |
| `fork_policy` | How to handle PRs from forks (see below) |
| `trusted_authors` | Authors whose fork PRs get full context regardless of `fork_policy` |
| `ignore_paths` | Glob patterns for files to exclude from context |
| `custom_instructions` | Free-text hints appended to the review prompt |
| `gitnexus` | Whether to include GitNexus impact analysis in context. Default: `true` (best-effort; gracefully skipped if unavailable) |
| `review_policy` | Per-repo overrides of `max_review_cycles`, `critical_finding_kinds`, `skip_unchanged_diff`, `stack_aware` and `dismiss_stale_change_requests`. Unset fields use `[defaults]`. |

```toml
[repos.review_policy]
max_review_cycles = 1
critical_finding_kinds = ["security", "secret_exposure"]
```

### Maintainer work queue

The daemon normalizes GitHub activity into persistent work items before invoking a harness. New PR heads enqueue `review_pr`, maintainer mentions enqueue `respond_to_command`, and live fix markers enqueue `repair_pr`. The queue is orchestration state only: the daemon claims, dedupes, rate-limits, and records each task, while the configured agent decides how to review, explain, or repair.

`pr-reviewer status` shows pending, claimed, and failed work counts. Use `pr-reviewer queue list`, `show`, `retry`, and `cancel` to inspect or operate individual tasks. Completed, failed, and canceled task records remain in SQLite for debugging and idempotency.

Maintainers can also operate the queue from PR issue comments:

- `@pr-reviewer status` or `@pr-reviewer explain` enqueues an agent-authored response.
- `@pr-reviewer fix` enqueues a `repair_pr` task for the current head when auto-fix is enabled.
- `@pr-reviewer retry` retries the latest failed task for that PR; `@pr-reviewer retry <id>` targets one queue item.
- `@pr-reviewer cancel` cancels pending or failed tasks for that PR; `@pr-reviewer cancel <id>` targets one queue item.

### `[repos.auto_fix]`

Opt-in autonomous fixing for managed clones. When enabled, the daemon enqueues a `repair_pr` work item only after a live `pr-reviewer-action:fix-required` marker targets the exact current PR head SHA. Repos with `local_path` are skipped so the daemon never resets a user-managed checkout. Default-branch scanning remains available only when `scan_default_branch = true`.

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable marker-driven PR-head repairs for managed clones. |
| `scan_default_branch` | `false` | Also run the legacy release-branch bug/security scanner and open draft fix PRs. |
| `base_branch` | `"main"` | Branch to scan when `scan_default_branch` is enabled. |
| `branch_prefix` | `"pr-reviewer/auto-fix"` | Prefix for pushed bot branches. |
| `draft_pr` | `true` | Open generated PRs as drafts. |
| `max_changed_files` | `20` | Abort and reset if the harness changes too many files. |
| `max_repairs_per_pr` | `10` | Stop automatic repair after this many attempts on one PR. |
| `max_repairs_per_head` | `1` | Stop duplicate repair attempts against the same PR head SHA. |

When enabled, pr-reviewer enriches context with:
- ranked GitNexus processes related to changed files
- top changed symbols with caller/callee context
- best-effort upstream impact snapshots (short timeout, non-blocking fallback)

### Auto-managed clones vs local_path

When `local_path` is omitted, pr-reviewer clones the repo to `~/.config/pr-reviewer/repos/{owner}/{name}/` and manages it automatically:

- Git hooks are disabled (`core.hooksPath=/dev/null`)
- Auth is passed via `http.extraHeader`, never stored in the remote URL
- `fetch_latest` runs before each review to keep the index fresh

Use `local_path` only if you already have a clone you want to reuse. Note that the review context is assembled from GitHub API responses, not the local clone — the clone is only used for GitNexus indexing.

---

## Fork policy

Controls what happens when pr-reviewer encounters a PR from a fork.

| Policy | Behavior |
|--------|----------|
| `ignore` | Skip fork PRs entirely. No review is posted. (default) |
| `limited` | Review with diff only. File contents are not included to avoid executing untrusted code paths in context assembly. |
| `full` | Full context, same as a non-fork PR. Use only for forks you trust (e.g., internal org forks). |

`trusted_authors` overrides the policy for specific GitHub usernames. A trusted author's PR always gets full context regardless of `fork_policy`:

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
fork_policy = "limited"
trusted_authors = ["dependabot[bot]", "known-external-contributor"]
```

---

## Confidence scoring

Review output includes a simple confidence object:

- `level`: `high`, `medium`, or `low`
- `reasons`: short machine-readable reason labels
- `justification`: a concrete explanation of what evidence is present or missing

The confidence level is shown in the posted review body. Low-confidence blocking
findings are downgraded unless they include concrete evidence, which prevents
speculative `REQUEST_CHANGES` reviews. GitHub self-review restrictions are handled
separately: when the authenticated user authored the PR, verdicts are posted as
`COMMENT` because GitHub rejects self-approval and self-request-changes reviews.

Confidence scoring is not configurable per repo today. If a repo routinely
triggers low-confidence reviews, use `custom_instructions` to give the model more
context about the domain, expected invariants, and validation patterns.

---

## `ignore_paths`

Glob patterns for files to exclude from diff context. Matched files are dropped before the prompt is assembled. Use this to avoid spending context budget on generated or vendored files.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
ignore_paths = [
  "*.lock",           # lockfiles
  "dist/**",          # build output
  "vendor/**",        # vendored dependencies
  "*.min.js",         # minified assets
  "**/__generated__/**",  # codegen output
]
```

Patterns use glob syntax (same as `.gitignore` glob rules).

---

## `custom_instructions`

Free-text instructions appended to the review prompt for a specific repo. Use this to give the model domain context it can't infer from the code alone.

```toml
[[repos]]
owner = "your-org"
name = "payments-service"
custom_instructions = """
This service handles PCI-scoped payment processing. Any code touching card data or transaction
records should be reviewed against our PCI-DSS controls. Flag any logging of cardholder data
as a critical finding.
"""
```

**Security note:** `custom_instructions` is included verbatim in the prompt. It is not a security boundary — a malicious PR could attempt to override or contradict these instructions via prompt injection in code comments or commit messages. The harness runs in a sandboxed environment, but the model itself is not immune to prompt injection in untrusted input.

---

## Multi-step workflows

A repo can define an ordered sequence of review steps, each with its own harness, model, and conditions. Steps run in the order defined; a failed step does not block subsequent steps.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"

[[repos.workflow]]
id = "security-scan"
name = "Security review"
harness = "claude-code"
model = "claude-opus-4-6"
custom_instructions = "Focus exclusively on security vulnerabilities."
enabled = true
[repos.workflow.conditions]
file_patterns = ["src/**", "api/**"]
min_diff_lines = 5
skip_drafts = true
label_patterns = []

[[repos.workflow]]
id = "general-review"
name = "General code review"
harness = "claude-code"
model = "claude-sonnet-4-6"
enabled = true
[repos.workflow.conditions]
min_diff_lines = 10
skip_drafts = false
```

### Workflow conditions

| Field | Description |
|-------|-------------|
| `file_patterns` | Glob patterns — step runs only if at least one changed file matches. Empty list means always run. |
| `min_diff_lines` | Minimum number of changed lines required to trigger this step. |
| `skip_drafts` | If `true`, skip this step for draft PRs. |
| `label_patterns` | PR must have at least one label matching a pattern. Empty list means always run. |

When no workflow steps are defined, pr-reviewer runs a single default review using the repo's harness and model settings.

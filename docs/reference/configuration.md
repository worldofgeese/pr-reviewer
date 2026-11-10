# Configuration reference

All configuration lives in `config.toml` in the config directory. See [Environment variables](environment.md#config-directory) for its path on your operating system. Every field can be inspected and set via CLI:

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
timeout_secs = 1800
review_tools = []         # claude-code only; built-in tools a review session may use
```

| Field | Default | Description |
|-------|---------|-------------|
| `default` | `claude-code` | Harness to use when no per-repo override is set |
| `model` | `claude-sonnet-4-6` | Model identifier passed to the harness CLI |
| `reasoning_effort` | unset | Optional reasoning effort override. When using Codex, pr-reviewer passes this through as `-c model_reasoning_effort="..."` |
| `timeout_secs` | `1800` | Seconds to wait for a harness response before treating the review as failed |
| `review_tools` | `[]` | Built-in tools that a review session may use. Empty means none. See [Review session tools](#review-session-tools). A repository can override it. |

### Review session tools

A review session reads untrusted input: the diff of the pull request and the repository's own instructions. pr-reviewer limits what such a session can do.

The `claude-code` harness starts a session with `--tools <list>`, which removes every built-in tool that is not in the list. A review session needs no tools. Its prompt already holds the pull request description, the diff, the changed files and the related files, and the session runs in an empty temporary directory. So the default `review_tools = []` removes all built-in tools: no `Bash`, no file edits, no network fetches, and no file reads.

To let review sessions read files, list the file-reading tools:

```toml
[harness]
review_tools = ["Read", "Grep", "Glob"]
```

**These tools are not read-only in effect.** They are not confined to the session's directory. They can read any file that the user running pr-reviewer can read, including the keyfile and `config.toml` in the config directory. A prompt-injected diff could get such content into a public review comment. Enable them only for repositories whose authors you trust. Add `Bash`, `Edit`, `Write` or `WebFetch` only for a repository that you trust fully.

pr-reviewer also starts every `claude-code` session with `--strict-mcp-config`, so the MCP servers of your Claude Code configuration are not available. `review_tools` lists built-in tools only. The hooks of your Claude Code configuration still run.

`review_tools` covers every session that answers a pull request: the review, the final summary, the replies to comments and the `@pr-reviewer` commands. The first-touch "in progress" comment always runs with no tools.

[Auto-fix](#reposauto_fix) sessions do not use this setting. They edit the managed clone, so they always run with a fixed set: `Read`, `Grep`, `Glob`, `Edit`, `Write` and `Bash`.

The `codex` and `opencode` harnesses ignore `review_tools`.

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
| `max_review_cycles` | `2` | Review cycles that may post inline findings. After this many review cycles, new findings that are not critical become non-blocking notes. `0` = no limit. See [Review cycles](../explanation/how-a-review-works.md#review-cycles). |
| `critical_finding_kinds` | `["security", "secret_exposure", "data_integrity", "correctness"]` | Finding kinds that can still be posted inline, and request changes, after the limit. |
| `skip_unchanged_diff` | `true` | Skip the review of a push that leaves the PR's own diff unchanged, such as a rebase. See [Stacked PRs](../explanation/how-a-review-works.md#stacked-prs). |
| `stack_aware` | `true` | Detect stacked PRs, tell the reviewer about the parent PR, and drop findings that a lower PR already tracks. |
| `dismiss_stale_change_requests` | `true` | Dismiss the bot's earlier `REQUEST_CHANGES` reviews when no blocking finding is open. |
| `status_on_no_new_findings` | `false` | When a re-review finds nothing new (`skipped:no-novel-findings`), post a `success` commit status on the head commit: context `pr-reviewer`, description `Reviewed: no new findings`. A merge gate can then tell "reviewed, nothing new" from "not reviewed yet". It does not mean that no blocking finding is open; gates should also read the PR's review state. Needs the `repo:status` scope (classic token) or "Commit statuses: write" (fine-grained token). If GitHub refuses the status, the bot logs a warning and the review is unaffected. With `dry_run`, the bot logs the status instead. |
| `peer_logins` | `[]` | GitHub logins of [peer daemons](../how-to/share-repositories-with-a-peer.md): other pr-reviewer daemons that watch the same repositories under their own account. The daemon skips a head commit that a peer reviewed or skipped, waits while a peer reviews it, skips a command that a peer answered and does not reply in a review thread that a peer started. A peer's comment counts only when it carries a pr-reviewer marker. |
| `standby_delay_secs` | `0` | Seconds to wait before the daemon reviews a new head commit or answers a command. Set it on a standby daemon so that a peer with no delay goes first. `0` = no wait. |

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
review_tools = ["Read", "Grep", "Glob"]
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
| `local_path` | Path to an existing local clone. If omitted, the repo is auto-cloned to `repos/{owner}/{name}/` in the config directory |
| `harness` | Override the global harness for this repo |
| `model` | Override the global model for this repo |
| `review_tools` | Override `[harness] review_tools` for this repo. `[]` removes all tools even when the global setting lists some. See [Review session tools](#review-session-tools) |
| `fork_policy` | How to handle PRs from forks (see [Fork policy](../explanation/how-a-review-works.md#fork-policy)) |
| `trusted_authors` | Authors whose fork PRs get full context regardless of `fork_policy` |
| `ignore_paths` | Glob patterns for files to exclude from context |
| `custom_instructions` | Free-text hints appended to the review prompt |
| `gitnexus` | Whether to include GitNexus impact analysis in context. Default: `true` (best-effort; gracefully skipped if unavailable) |
| `review_policy` | Per-repo overrides of `max_review_cycles`, `critical_finding_kinds`, `skip_unchanged_diff`, `stack_aware`, `dismiss_stale_change_requests` and `status_on_no_new_findings`. Unset fields use `[defaults]`. |

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

When `local_path` is omitted, pr-reviewer clones the repo to `repos/{owner}/{name}/` in the config directory and manages it automatically:

- Git hooks are disabled (`core.hooksPath=/dev/null`)
- Auth is passed via `http.extraHeader`, never stored in the remote URL
- `fetch_latest` runs before each review to keep the index fresh

Use `local_path` only if you already have a clone you want to reuse. Note that the review context is assembled from GitHub API responses, not the local clone — the clone is only used for GitNexus indexing.

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

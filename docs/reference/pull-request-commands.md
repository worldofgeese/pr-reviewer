# Pull request commands and labels

This page lists what the daemon reads from a pull request in a watched repository: bot commands, thread mentions, reply phrases, markers and labels. To use them, see [Respond to a review](../how-to/respond-to-a-review.md). For the keys that change this behavior, see the [configuration reference](configuration.md).

The bot name is the configuration key `defaults.bot_name`. Its default is `pr-reviewer`, and this page writes `@pr-reviewer`.

## Where the daemon reads comments

| Place | What the daemon reads | Who can write it |
|-------|-----------------------|------------------|
| A comment on the **Conversation** tab of a pull request | [Commands](#commands) | A repository owner, member or collaborator |
| A comment in an inline review thread | [Mentions](#thread-mentions) and [reply phrases](#phrases-in-a-thread-reply) | Any user |
| A comment on the **Conversation** tab | [Out-of-scope phrases](#phrases-in-a-thread-reply) for the whole pull request | Any user except the bot |

The daemon skips its own comments and comments from any GitHub account of type bot. A comment is the daemon's own only when it carries a `<!-- pr-reviewer-... -->` [marker](#markers) and its author is the authenticated account. If you run the daemon on your own account, your commands still work, because they carry no marker. A marker in a comment from any other account gives that comment no special treatment. When the daemon skips one of its own comments, it logs the comment id at `debug` level. It does not read the pull request description or the body of a review.

## Commands

A command is a comment on the **Conversation** tab. The daemon reads each comment once.

| Command | Effect |
|---------|--------|
| `@pr-reviewer status` | Queues an agent-written answer about the review state of the pull request and the recent review attempts. |
| `@pr-reviewer explain` | Same as `status`. `@pr-reviewer why` is an alias. |
| `@pr-reviewer fix` | Queues a `repair_pr` task for the current head commit, when `auto_fix.enabled` is `true` for the repository. Otherwise the answer says that auto-fix is off. |
| `@pr-reviewer retry` | Requeues the latest failed work item of this pull request. |
| `@pr-reviewer retry <id>` | Requeues the failed work item with that ID, when it belongs to this pull request. The ID can have a leading `#`. |
| `@pr-reviewer cancel` | Cancels the pending and failed work items of this pull request. |
| `@pr-reviewer cancel <id>` | Cancels one work item of this pull request. |

The bot answers each command with a pull request comment. The answer starts with a hidden `pr-reviewer-command-response` marker.

### Command syntax

- The command must start a line. Leading spaces are allowed. `example: @pr-reviewer fix` is not a command.
- The mention matches `@pr-reviewer` and `@pr-reviewer[bot]`, in any letter case.
- The command word follows after whitespace or one of `:`, `,`, `-`. For example, `@pr-reviewer: retry #123` works.
- The command word must stand alone. `@pr-reviewer fixture setup` and `@pr-reviewer fixing this` are not commands.
- If a comment has several lines, the first line with a known command wins. Text after the command word on the same line is ignored, except an ID for `retry` and `cancel`.
- A command from a user whose association is not `OWNER`, `MEMBER` or `COLLABORATOR` is ignored. The daemon logs it.

### What `fix` needs

The daemon starts a repair only when all of these are true:

| Condition | Setting or fact |
|-----------|-----------------|
| Auto-fix is on | `[repos.auto_fix] enabled = true` |
| The repository uses a managed clone | `local_path` is not set |
| The branch is in the same repository | Not a [fork PR](../explanation/how-a-review-works.md#fork-policy) |
| The head commit did not change since the command | The queued task names the head commit |
| The repair limits are not reached | `max_repairs_per_head` (default 1), `max_repairs_per_pr` (default 10) |
| The harness changes few files | `max_changed_files` (default 20). The bot resets the clone above that |

The repair commit has the message `fix: address pr-reviewer findings for <12 characters of the commit>`. The daemon pushes it to the branch of the pull request. The new head commit starts a new review cycle.

When `auto_fix.enabled` is `true`, the daemon also queues a repair without a command. It does this when a comment for the current head commit contains the [`fix-required` marker](#markers) and the daemon's authenticated account wrote it. The daemon ignores the marker in a comment from any other account.

## Thread mentions

A comment in an inline review thread that contains the exact text of the bot mention (`@pr-reviewer` by default) gets a reply in the same thread. The match is case-sensitive and can sit anywhere in the comment.

| Rule | Value |
|------|-------|
| Who can mention | Any user except bot accounts |
| Reply limit | 5 replies in 10 minutes for one pull request. The daemon skips the mention above that |
| Dedupe | One reply for each comment |
| Reply content | The agent classifies the thread as `fixed`, `not_fixed`, `intentional_or_accepted`, `out_of_scope_for_this_pr` or `needs_human_judgment`, and writes a reply |

## Phrases in a thread reply

A reply in the thread of a bot finding sets the status of that finding at the next review cycle, or at the next check of human answers. The daemon does not need a mention for this. It matches the text in lower case. A phrase with spaces matches as a substring. A single word matches as a whole word (`fine` does not match `fine-grained`).

If a reply holds phrases of several kinds, the first row of this table that matches wins.

| Status | Phrases |
|--------|---------|
| Out of scope for this PR | `out of scope`, `not in scope`, `not in this pr`, `not for this pr`, `not taking further`, `not expanding this pr`, `not reopening`, `not part of this pr` |
| Rejected with rationale | `false positive`, `not correct`, `you are wrong`, `this is wrong`, `that is wrong`, `misread`, `misunderstanding`, `not the issue` |
| Dismissed by a human | `intentional`, `by design`, `won't fix`, `wontfix`, `acknowledged`, `expected`, `not a bug`, `fine`, `this is fine`, `leave it`, `acceptable`, `known`, `on purpose` |

Use a straight apostrophe in `won't fix`.

Other effects:

| Event | Status | Effect |
|-------|--------|--------|
| A user resolves the thread on GitHub | Resolved by a human | The finding closes. |
| A comment on the **Conversation** tab contains an out-of-scope phrase | Boundary for the pull request | Findings on that topic in any file are dropped, unless a finding has an evidence note. |

A finding with one of the statuses above is not posted again, unless the new finding has an evidence note. The daemon checks the threads of a reviewed pull request at most every 2 minutes while a blocking finding is open. When a human answered every open blocking finding, it records the answers and dismisses the change request of the bot. This needs `dismiss_stale_change_requests` (default `true`). See [Review cycles](../explanation/how-a-review-works.md#review-cycles).

## Markers

A marker is an HTML comment in a bot comment. The reader does not see it on GitHub.

| Marker | Written by | Read by the daemon for |
|--------|-----------|------------------------|
| `<!-- pr-reviewer-action:fix-required pr=<number> sha=<head> finding=review-feedback -->` | The bot, in its review comment, when the review requests changes with blocking findings | Queuing an automatic repair. The marker must come from the bot account and name the current head commit |
| `- Reviewer: [pr-reviewer](` in the **Review metadata** section | The bot, in every review | Counting the review cycle. The count is the number of reviewed commits plus one |
| `<!-- pr-reviewer-command-response ... -->` | The bot, in every answer to a command | Nothing. It identifies the answer |

## Labels

The daemon reads no pull request labels. No code in the review pipeline checks the labels of a pull request, and a label cannot start, stop or change a review.

The `label_patterns` condition of a workflow step is stored in the configuration and checked for size by `pr-reviewer serve`. No review code reads it.

## Pull request state that changes a review

| State | Effect | Setting |
|-------|--------|---------|
| Draft | No review (`skipped:draft`) | `defaults.review_drafts` (default `false`) |
| Only documentation files changed | No review (`skipped:docs-only`) | `defaults.skip_docs_only` (default `true`) |
| Head commit with the same own changes as the last review | No review (`skipped:unchanged-diff`) | `skip_unchanged_diff` (default `true`) |
| Branch in a fork | Skipped, diff only or full review | `fork_policy`, `trusted_authors` |
| Base branch is the head branch of another open pull request | Reviewed as a stacked PR | `stack_aware` (default `true`) |

The `pr-reviewer logs` output shows the `skipped:` status of each pull request.

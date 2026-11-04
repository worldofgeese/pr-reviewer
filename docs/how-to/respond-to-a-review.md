# Respond to a review

To answer a review that pr-reviewer posted on your pull request, fix the finding, push back on it, or ask the bot to fix it. This page also covers what changes after the review-cycle limit and in a stacked PR. For the reasons behind this behavior, read [How a review works](../explanation/how-a-review-works.md). For the exact commands and phrases, see [Pull request commands and labels](../reference/pull-request-commands.md).

Prerequisites:

- Your pull request is in a repository that pr-reviewer watches.
- pr-reviewer posted a review on it. The review shows a collapsed **Review metadata** section with a link to pr-reviewer, the model and the commit.

## Read the review

1. Open the pull request and go to the **Conversation** tab.
2. Read the review body. It holds the summary, the confidence line and, when earlier findings changed, a **Since my last review** count of fixed, still blocking, new and suppressed findings.
3. Read the inline threads. Each thread is one finding that can block the merge.
4. Look for a **Follow-up notes (non-blocking)** section in the review body. A follow-up note is a finding that cannot block the merge. See [Read follow-up notes after the limit](#read-follow-up-notes-after-the-limit).

pr-reviewer never approves a pull request. A review with a blocking finding requests changes, unless you wrote the pull request with the same account that pr-reviewer uses. GitHub does not allow that account to request changes on its own pull request, so the review is a plain comment.

## Fix a finding

To fix a finding yourself:

1. Change the code and push a commit to your branch.
2. Wait for the next review. The bot reviews every new head commit, and each review is one review cycle.

The bot marks a finding as fixed only when your pull request changes the file of the finding in a different way than at the last review, or when the file left the diff. A push that leaves that file as it was does not fix the finding.

If the fix is in another file than the one the finding names, the finding stays open. Resolve its thread, or answer it, as described in the next section.

When no blocking finding is open any more, the bot dismisses its earlier change request. This needs `dismiss_stale_change_requests`, which is on by default.

## Push back on a finding

To tell the bot that a finding is wrong, intentional or out of scope:

1. Open the thread of the finding.
2. Reply with a plain sentence that contains one of the phrases in the table. The bot reads the phrase, not the intent. A reply such as "ok" or "fixed it" does not change the status of the finding.
3. Optionally, click **Resolve conversation**.

| Your reply contains | The bot records the finding as |
|---------------------|--------------------------------|
| `out of scope`, `not in scope`, `not in this PR`, `not for this PR`, `not part of this PR` | Out of scope for this PR |
| `false positive`, `not correct`, `you are wrong`, `this is wrong`, `misread`, `misunderstanding`, `not the issue` | Rejected with rationale |
| `intentional`, `by design`, `won't fix`, `acknowledged`, `expected`, `not a bug`, `on purpose`, `acceptable`, `known`, `fine` | Dismissed by a human |

The full list of phrases is in the [reference](../reference/pull-request-commands.md#phrases-in-a-thread-reply). Add the reason to your reply, because the next reviewer, human or bot, reads it.

Result: the bot does not post the same finding again. It can raise it again only when it has concrete evidence that names the changed lines.

To put a topic out of scope for the whole pull request, write one of the out-of-scope phrases in a comment on the **Conversation** tab, for example "Error handling is not in this PR". The bot then drops findings on that topic in every file, unless the finding has concrete evidence.

Resolving a thread without a reply also closes the finding. It does not start a review. If the thread was the last open blocking finding, the daemon notices the answer within about 2 minutes, records it and dismisses the change request. It does not run a review for this.

## Ask the bot a question

To ask about a finding without changing its status, mention the bot in the thread, for example `@pr-reviewer is this still a problem after my last commit?`. The bot replies in the thread. Any author can do this. The bot answers at most 5 times in 10 minutes on one pull request.

Write the mention exactly as the configured bot name, `@pr-reviewer` by default. The daemon matches the name with the same letter case.

## Ask the bot to fix a finding

To have the bot push a fix to your branch:

1. Write `@pr-reviewer fix` on its own line in a comment on the **Conversation** tab. The bot obeys the command only from a repository owner, member or collaborator, and ignores it from anyone else. If you are not one of them, ask one to write it.
2. Wait for the bot to answer in the same pull request.

The bot pushes a commit with the message `fix: address pr-reviewer findings for <commit>` only when all of these are true:

- The operator turned on `auto_fix.enabled` for the repository.
- Your pull request branch is in the same repository, not in a fork.
- The repository is a managed clone, not a `local_path` checkout.
- The bot did not reach its repair limits (1 repair per head commit and 10 per pull request by default).

If `auto_fix.enabled` is off, the bot says so in its answer. If another condition is false, the bot pushes nothing and posts no reason. The operator finds it in the daemon log. When `auto_fix.enabled` is on, the bot also starts a repair on its own after a review with blocking findings, so you often do not need the command.

The pushed commit is a new head commit, so it starts a new review cycle.

Other commands are for status and for the work queue: `status`, `explain`, `retry` and `cancel`. See the [command reference](../reference/pull-request-commands.md#commands).

## Get a new review

pr-reviewer reviews when the head commit of the pull request changes. To get a new review, push a commit.

These events do not start a review:

- Resolving a thread or replying to one.
- Marking a draft pull request as ready. The daemon queues one review for each head commit, and it skips drafts by default (`review_drafts = false`). It does not queue the same commit again, so push a commit after you mark the pull request as ready.
- A push that leaves the changes of your pull request as they were, such as a rebase. See [Work in a stacked PR](#work-in-a-stacked-pr).

An operator can force a review of an unchanged diff with `pr-reviewer review owner/repo#N --force`. See the [CLI reference](../reference/cli.md#review).

## Read follow-up notes after the limit

The setting `max_review_cycles` (default 2) limits the review cycles that open new threads. After that limit:

- New findings go into the **Follow-up notes (non-blocking)** section of the review body. They open no thread and cannot request changes.
- The bot still reviews every new head commit and still checks its earlier findings.
- A critical finding is still posted inline and can request changes. It must be blocking, of a critical kind such as `security` or `correctness`, and backed by an evidence note that names changed lines. After the limit it must also sit on a line that your pull request adds or removes.
- A critical finding on a line your pull request does not change appears first in the notes as a "possible blocker". It does not request changes.

Read a follow-up note as advice. Fix it, move it to a new issue, or ignore it. A note appears once and is not repeated in later review cycles.

## Work in a stacked PR

A stacked PR targets the head branch of another open pull request. When you rebase your stack, the bot does not review a pushed branch again if its own changes are the same as at the last review. The log shows `skipped:unchanged-diff` and no review cycle is used.

Keep in mind:

- The bot reviews only the lines that your pull request changes. It does not post a finding that matches an open finding of a lower pull request on a line you did not change. The lower pull request tracks it.
- A finding stays on the pull request where the bot raised it, also after the lower pull requests merge.
- The bot cannot see a break that a lower pull request causes after a rebase. Rely on CI for that.
- A push that moves an added line past an unchanged line, or that edits your own lines, is reviewed again.

## Work in a fork PR

For a fork PR, the result depends on the `fork_policy` that the operator set for the repository:

| Policy | What you see |
|--------|--------------|
| `ignore` (default) | No review. The log shows a skipped review. |
| `limited` | A review from the diff only. The bot does not read file contents. |
| `full` | A full review, as for a pull request from the same repository. |

A repository can list `trusted_authors`. A pull request from a trusted author always gets a full review. The `fix` command never changes a fork branch.

## Troubleshoot

- No review appeared: the pull request can be a draft, change only documentation files (`skipped:docs-only`), come from a fork under the `ignore` policy, or the daemon is not running. Ask the operator to run `pr-reviewer logs --repo owner/repo` and read the [troubleshooting reference](../reference/troubleshooting.md).
- The bot ignores your `@pr-reviewer fix` comment: your account is not an owner, member or collaborator, the command is not at the start of a line, or the command word is followed by more letters (`@pr-reviewer fixing` does not match).
- The bot raised a finding that you answered: your reply had none of the phrases in the table. Reply again with one of them.

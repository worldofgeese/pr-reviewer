# Share repositories with a peer daemon

Two people can run pr-reviewer on the same watched repositories, each on their own machine and under their own GitHub account. Then reviews go on when one machine is off. Without the settings in this guide, both daemons review every head commit and both answer every command.

A **peer daemon** is another pr-reviewer daemon that watches the same repositories under another GitHub account. This guide sets up one **primary** daemon, which reviews at once, and one **standby** daemon, which waits and reviews only what the primary did not.

Prerequisites:

- Both daemons run a release that has the `peer_logins` setting. Run `pr-reviewer config get defaults.peer_logins` to check.
- You know the GitHub login of each daemon. Run `gh api user --jq .login` as the user that the daemon's token belongs to.
- Both daemons watch the same repositories. Run `pr-reviewer list` on each machine to compare.
- Each daemon's GitHub token can write commit statuses: the `repo:status` scope on a classic token, or "Commit statuses: read and write" on a fine-grained token. Without it, the daemon cannot set a [head claim](#the-head-claim) and relies on the checks below.

## Configure the primary daemon

To make a daemon the primary:

1. On the primary machine, name the standby's login as a peer. Replace `standby-login` with the login of the standby daemon.

   ```bash
   pr-reviewer config set defaults.peer_logins '["standby-login"]'
   ```

2. Leave `defaults.standby_delay_secs` at `0`.

3. Restart the daemon so that it reads the new configuration. See [Run pr-reviewer as a service](run-as-a-service.md).

Result: the primary reviews new head commits at once. It skips a head commit that the standby already reviewed while the primary was off.

## Configure the standby daemon

To make a daemon the standby:

1. On the standby machine, name the primary's login as a peer. Replace `primary-login` with the login of the primary daemon.

   ```bash
   pr-reviewer config set defaults.peer_logins '["primary-login"]'
   ```

2. Set the delay. The standby waits this many seconds after it sees a new head commit or a command. Choose a delay that is longer than a typical review takes on the primary, for example 10 minutes.

   ```bash
   pr-reviewer config set defaults.standby_delay_secs 600
   ```

3. Restart the daemon.

Result: when the primary is on, the standby finds the primary's review after the delay and skips the head commit. When the primary is off, the standby reviews after the delay.

## What each daemon checks

Before a daemon reviews a head commit, it reads the reviews and comments of the pull request. It skips the head commit when a peer:

- posted a review of that head commit that carries the pr-reviewer metadata, or
- recorded in its status comment that it reviewed or skipped that head commit, for example as docs-only.

When a peer's status comment says that the peer is reviewing that head commit, the daemon checks again after 2 minutes. A status comment older than `harness.timeout_secs` does not count, because the peer stopped before it finished.

For commands such as `@pr-reviewer status` and `@pr-reviewer fix`, the daemon skips a command that a peer already answered. For `@pr-reviewer fix`, it still runs the repair when the peer that answered has auto-fix off. It does not reply in a review thread that a peer started, because the peer answers there.

On the **Conversation** tab, a comment from a peer's login counts as the peer's work only when it carries a pr-reviewer marker. So a person who uses the peer's account by hand can still give the bot commands there. In review threads, the daemon ignores every comment from a peer's login, as it ignores its own.

## The head claim

Each daemon also sets a **head claim** on the head commit before a review cycle. The claim is a commit status in `pending` state with the context `pr-reviewer/claim` and a description such as `claimed by mac-1 at 2026-11-11T07:05:12Z`. You see it in the **Checks** area of the pull request. It needs no setting. The daemon uses the machine's host name, so give each machine its own host name. Anyone who can see the pull request can read that host name in the status.

- A daemon skips a head commit when another host's claim is less than 15 minutes old. It checks again after 2 minutes. After 15 minutes it takes the head commit over, because the other daemon probably stopped.
- After a daemon sets its claim, it reads the claim back. If the latest claim is a fresh claim of a peer, the daemon backs off and leaves the peer's claim alone.
- When the review cycle ends, or fails, the daemon posts a `success` status with the description `released by mac-1 at ...`. GitHub cannot delete a status, so this is how a claim ends. The claim is `pending` only while a review cycle runs.
- GitHub statuses never expire. If a daemon stops inside a review cycle (a kill, a restart, a sleeping machine), its claim stays `pending`, and a merge rule that needs every check to finish waits on it. The daemon cleans this up and posts the same `released` status: at start, for every open pull request, it releases a `pending` claim that its own host made; and while it runs, for a head commit it already reviewed, it releases a `pending` claim of any host that is older than 15 minutes. It checks each reviewed pull request about every 15 minutes, also while GitHub reports the pull request list as unchanged. With `defaults.dry_run` on, it releases nothing. It never touches a claim of a peer that is less than 15 minutes old.
- A daemon that cannot read or set the claim, for example because its token cannot write commit statuses, logs a warning and reviews anyway.

The claim narrows the race but is not a lock. If both daemons claim in the same second, both can read their own claim back and both review. A review cycle that runs longer than 15 minutes can also lose its claim to a peer.

## Limits

- A review that a peer posted earlier does not count toward this daemon's [review cycles](../explanation/how-a-review-works.md#review-cycles). If the standby reviews a pull request that the primary reviewed before, the standby starts at review cycle 1 and can raise findings again that the primary's open threads already hold.
- Each daemon dismisses only its own change requests. A change request from the standby stays until the standby dismisses it, or until a person does.
- A mention in a review thread that a peer started waits for that peer. When the peer is off, it answers after it starts again.
- A daemon uses its own `harness.timeout_secs` to decide that a peer's status comment is stale. Give both daemons the same value.
- When both daemons are on and a review on the primary takes longer than the delay, before the primary posts its status comment, both can review the same head commit. The head claim makes this rare. Raise the delay if you still see it.
- Both daemons need distinct host names. Two machines with the same host name cannot tell their claims apart.

## Next steps

- See `peer_logins` and `standby_delay_secs` in the [configuration reference](../reference/configuration.md#defaults).
- If both daemons still review the same head commit, see [Troubleshooting](../reference/troubleshooting.md#duplicate-reviews).

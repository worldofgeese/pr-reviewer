# Troubleshooting

This page lists common failures by symptom, with the cause and the fix. For any problem, run the command again with `RUST_LOG=debug` in front of it to get verbose output, as in `RUST_LOG=debug pr-reviewer status`.

In this page, the **config directory** is the folder where pr-reviewer keeps its configuration and state. Its path depends on your operating system. See [Environment variables](environment.md#config-directory).

## Commands show the wrong state

### `status` says the daemon is stopped, but a daemon runs

Each config directory has its own daemon, database and token. Your shell and your service can use different directories. Check the variable in your shell:

```bash
echo "${PR_REVIEWER_CONFIG_DIR:-not set}"
```

If the variable is set, `pr-reviewer` reads that directory and not the default. Run `unset PR_REVIEWER_CONFIG_DIR` to use the default directory again.

### `init` removed my configuration

`init` replaces `config.toml` with the defaults. Restore your last backup. See [Back up and upgrade](../how-to/back-up-and-upgrade.md#restore-the-state). Run `init` only once for each config directory.

---

---

## Token errors

### "GitHub token not found"

No token is available from any source. This also happens when `PR_REVIEWER_CONFIG_DIR` points at a directory that has no token. Set one:

```bash
pr-reviewer config set-token --passphrase
```

Or set the `GITHUB_TOKEN` environment variable as a fallback.

See `pr-reviewer config token-status` to confirm which source is active and what the resolution chain found.

### 401 Unauthorized from GitHub

The token is present but rejected. Likely causes:

- The token has expired (fine-grained PATs expire)
- The token lacks a required scope. It needs `pull_requests:read+write` and `contents:read` (fine-grained), or `repo` (classic). If `repos.auto_fix.enabled = true`, it also needs `contents:write` so it can push fix branches.
- The token was revoked

Generate a new PAT and re-run `pr-reviewer config set-token --passphrase`.

### "passphrase required but PR_REVIEWER_PASSPHRASE not set"

You encrypted the token with `--passphrase` and are running the daemon without the passphrase env var. Either:

```bash
# Set the env var before starting the daemon
export PR_REVIEWER_PASSPHRASE="your-passphrase"
pr-reviewer start --daemon
```

Or see [Run as a service](../how-to/run-as-a-service.md) for the systemd `EnvironmentFile=` pattern, which is the recommended approach for production.

### `set-token` fails silently or exits with error

- Make sure that the config directory exists (`pr-reviewer init` creates it)
- If using `--signet`, confirm Signet is installed and `signet secret set GITHUB_TOKEN` works independently
- If piping via `--stdin`, confirm the token has no trailing newline issues: `printf '%s' "$TOKEN" | pr-reviewer config set-token --stdin`

---

## Adding and reviewing repositories

### `add` fails with "git clone failed" and "Repository not found"

The clone uses your token over HTTPS. GitHub reports "Repository not found" when the name is wrong or when the token cannot see the repository. Check the name, and make sure that the token has access to the repository. For a private repository, a classic token needs the `repo` scope. A fine-grained token must list the repository.

### `review` fails with "repo not configured"

The repository is not a watched repository in this config directory. Run `pr-reviewer list` to see the watched repositories, then `pr-reviewer add owner/repo`.

### `review` fails with "invalid target format"

The target must have the form `owner/repo#number`, for example `pr-reviewer review owner/repo#42`. Quote the target in `zsh` if your shell treats `#` as a comment.

---

## GitHub rate limits

### "rate limit exceeded" or 403 responses from GitHub

The daemon automatically backs off when the rate limit is hit and waits until the reset window. Check the current state:

```bash
pr-reviewer status
```

This shows remaining API quota and the next reset time.

If you reach the limit often:

- Increase `daemon.poll_interval_secs` in config (e.g., from 30 to 60)
- Reduce `daemon.max_concurrent_reviews` to serialize reviews instead of running them in parallel
- Reduce the number of watched repos

The daemon caches the ETag of each list response. An unchanged response does not count against your quota. The cache works when you see `304 Not Modified` in the debug log.

---

## Harness not found

### "harness binary not found" or `No such file or directory`

The harness process spawns in a scrubbed environment, which strips `HOME`, `PATH` modifications from shell rc files, and version manager shims (`nvm`, `pyenv`, `rbenv`, etc.).

**The binary must be on the system PATH as installed by the package manager, not via a user-space version manager shim.**

```bash
# Confirm the binary is reachable without shell rc:
env -i PATH=/usr/local/bin:/usr/bin:/bin which claude
env -i PATH=/usr/local/bin:/usr/bin:/bin which opencode
env -i PATH=/usr/local/bin:/usr/bin:/bin which codex
```

If the binary is only accessible via a version manager:

1. Install the tool globally via `npm install -g` or `cargo install` to a system path
2. Or create a wrapper script at `/usr/local/bin/claude` that calls the version-manager-managed binary

### `pr-reviewer` itself was just installed, but your shell still says `No such file or directory`

This is usually Bash caching an old command path.

```bash
hash -r
pr-reviewer --version
```

The installer now refreshes a `~/.local/bin/pr-reviewer` shim for Cargo installs, which fixes most stale-path cases on the next invocation.

### The harness runs but authentication fails

AI CLIs need to be authenticated separately. pr-reviewer removes `ANTHROPIC_API_KEY` and similar variables from the environment of the AI CLI on purpose. Sign in to Claude Code, OpenCode or Codex with the login of the tool itself, not with environment variables.

Confirm the harness works outside of pr-reviewer:

```bash
echo "hello" | claude --dangerously-skip-permissions -p -
opencode run "hello"
codex exec "hello"
```

---

## Harness timeout or parse failure

### Review logged as failed with "timeout"

The default timeout is 1800 seconds (30 minutes). Very large PRs or slow models can exceed this.

Increase it in config:

```bash
pr-reviewer config set harness.timeout_secs 3600
```

Or per-repo via `config.toml`:

```toml
[[repos]]
owner = "your-org"
name = "large-repo"
# the timeout is global only: use harness.timeout_secs
```

### "failed to parse harness output" or review posted with no comments

The parser extracts JSON from the harness output using three strategies: marked code block → brace-match fallback → raw fallback. Parse failures usually mean the model returned malformed output.

Debug by running a dry-run and inspecting the raw output:

```bash
RUST_LOG=debug pr-reviewer review owner/repo#42 --dry-run 2>&1 | less
```

Look for `raw harness output` in the debug log. Common causes:

- Model hit a context limit and truncated mid-JSON
- Model added a preamble before the JSON block (the brace-match fallback handles this, but deeply nested truncation can still fail)
- `max_prompt_bytes` set too high, causing the model to time out before completing

Reduce context if needed:

```bash
pr-reviewer config set defaults.max_diff_lines 1500
pr-reviewer config set defaults.max_files 25
```

---

## Review not posting

### Review runs but nothing appears on GitHub

First check whether dry run is on:

```bash
pr-reviewer config get defaults.dry_run
```

If `true`, set it back to `false`:

```bash
pr-reviewer config set defaults.dry_run false
```

### "skipping: PR is a draft"

Draft PRs are skipped by default. To review drafts:

```bash
pr-reviewer config set defaults.review_drafts true
```

Or per-repo in `config.toml`:

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
# add a workflow step with skip_drafts = false, or set globally:
```

### "skipping: fork PR, policy is ignore"

Fork PRs are skipped when `fork_policy = "ignore"` (the default). Options:

- `limited` reviews the diff but does not include full file contents. This is safer for untrusted forks.
- `full` gives the full context, as for a pull request that is not from a fork.

```toml
[[repos]]
owner = "your-org"
name = "your-repo"
fork_policy = "limited"
trusted_authors = ["known-contributor"]   # these get full context regardless
```

### Self-review: review posts as COMMENT instead of APPROVE/REQUEST_CHANGES

GitHub does not allow you to approve or request changes on your own PRs. If the authenticated user is the PR author, the verdict is automatically downgraded to `COMMENT` with inline feedback folded into the body. This is expected behavior, not a bug.

### 422 error: "pull request review thread not pullable"

GitHub rejected inline comment positions (happens when position mapping goes wrong on very old or re-based PRs). pr-reviewer automatically retries these as a `COMMENT` review with inline comments collapsed into the body. Check logs to confirm the retry succeeded.

---

## Duplicate reviews

### Two identical reviews posted on the same PR

The DB dedupe key (`sha256(repo + pr + sha + harness)`) prevents duplicates within a single install. If you wiped the database, the GitHub-side check (querying existing reviews for bot name + SHA match) acts as the second line of defense.

If duplicates are appearing:

1. Confirm you only have one instance of pr-reviewer running: `pr-reviewer status`
2. Confirm the `bot_name` in config matches what was used when the prior review was posted
3. If another person runs a daemon on the same repositories under their own account, name each other in `defaults.peer_logins` and give one daemon a `defaults.standby_delay_secs`. See [Share repositories with a peer daemon](../how-to/share-repositories-with-a-peer.md).
4. Open the head commit's statuses on the pull request. Look for `pr-reviewer/claim`. If it never appears, the daemon's token cannot write commit statuses (it needs `repo:status` or "Commit statuses: read and write"); the daemon then logs `could not set the head claim` and reviews without a [head claim](../how-to/share-repositories-with-a-peer.md#the-head-claim). If both daemons show the same host name in their claims, give each machine its own host name.

### A pull request stays blocked on a `pr-reviewer/claim` check that is pending

The daemon that set the [head claim](../how-to/share-repositories-with-a-peer.md#the-head-claim) stopped before it released the claim. The claim is `pending` only during a review cycle, and the daemon ends it with a `success` status. A running daemon releases a claim that is older than 15 minutes within about 15 more minutes (log line `releasing an abandoned head claim`), and a daemon that starts again releases its own claim at once. To clear it by hand, post the status that the daemon would post:

```bash
gh api repos/OWNER/REPO/statuses/HEAD_SHA -f state=success -f context=pr-reviewer/claim -f description="released by hand"
```

### A pull request is not reviewed and the log says "a peer host holds the head claim"

Another host set the `pr-reviewer/claim` status on the head commit less than 15 minutes ago. The daemon checks again after 2 minutes and takes the head commit over once the claim is 15 minutes old. A manual `pr-reviewer review` skips the head commit too. To review sooner, ask the peer's owner to finish or stop their review cycle: the daemon then posts a `released` status.

### Review stuck in "in-progress" state after a crash

When it starts, the daemon puts every `claimed` work item back in the queue and drops the unfinished review claims, whatever their age. The next worker run then performs the review. Restart the daemon to recover a stuck item. To clear a stale or failed claim for one pull request at once, run the review again with `--force`:

```bash
pr-reviewer review owner/repo#42 --force
```

---

## Daemon issues

### Daemon won't start: "another pr-reviewer daemon already uses the config directory"

Another daemon holds the lock on this config directory. The error names the directory and, when readable, the process ID of the holder. Check it:

```bash
pr-reviewer status
```

Stop that daemon, or give the second daemon its own directory with `PR_REVIEWER_CONFIG_DIR`. Do not delete `daemon.pid` or `daemon.lock`: the lock belongs to the running process, and the operating system releases it when that process exits, so a crash leaves nothing to clean up. A leftover `daemon.pid` does not block a start.

### Daemon won't start: "failed to lock ... daemon.lock"

The file system of the config directory does not support file locks. Network and some FUSE file systems can fail this way. The daemon refuses to run without the lock. Move the config directory to a local disk with `PR_REVIEWER_CONFIG_DIR`.

### "database is locked"

Only one pr-reviewer process should access the state database at a time. If you see this:

1. Confirm that only one daemon runs on this config directory: `pr-reviewer status`
2. Stop it with `pr-reviewer stop`, or with your service manager
3. The WAL mode journal files (`.db-wal`, `.db-shm`) left behind by a crashed process are safe to delete if no pr-reviewer process is running

### Daemon starts but immediately stops

Check the logs:

```bash
# systemd on Linux
journalctl -u pr-reviewer -n 50

# launchd on macOS: the log file named in the plist
tail -n 50 ~/Library/Logs/pr-reviewer.log

# started with --daemon: run in the foreground to see startup errors
RUST_LOG=info pr-reviewer start
```

Common causes: missing token, config parse error, binary not found.

`pr-reviewer start --daemon` discards all output, so it shows no error. Run `RUST_LOG=info pr-reviewer start` in the foreground, or use a service manager. See [Run pr-reviewer as a service](../how-to/run-as-a-service.md).

### The service stops with "failed to run ioreg" (macOS)

Without a passphrase, pr-reviewer decrypts the token with a key that it derives from the ID of the machine. On macOS it reads this ID with `ioreg`, which lives in `/usr/sbin`. A LaunchAgent has a short `PATH`. Add `/usr/sbin` to the `PATH` in the plist, then restart the service:

```xml
<key>PATH</key>
<string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
```

### The daemon starts again after `pr-reviewer stop`

A service manager restarts a daemon that exits. Stop the service with the service manager instead: `launchctl bootout "gui/$(id -u)/com.example.pr-reviewer"` on macOS, or `sudo systemctl stop pr-reviewer` on Linux.

### Reviews fail with "harness executable ... was not found in PATH"

The daemon process cannot see the configured AI CLI (`claude`, `opencode`, or `codex`).
This is common in a service, because a service does not read your shell startup files. For launchd on macOS, set `PATH` in the `EnvironmentVariables` of the plist. See [Run pr-reviewer as a service](../how-to/run-as-a-service.md).

Check the service environment on Linux:

```bash
systemctl show pr-reviewer --property=Environment
```

Add the CLI install directory to the unit file:

```ini
Environment=PATH=/home/youruser/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/bin
```

Then reload and restart:

```bash
sudo systemctl daemon-reload
sudo systemctl restart pr-reviewer
```

---

## GitNexus issues

### GitNexus warnings in logs but reviews still work

GitNexus is optional. If it is not installed or the repository is not indexed, pr-reviewer uses only the diff and the file contents. Reviews still run. The warning is for information only.

### "gitnexus: command not found"

Install it:

```bash
npm install -g gitnexus
# or rely on fallback execution:
# pr-reviewer falls back to `npx gitnexus` if the binary is not installed
```

### Multiple repositories indexed and GitNexus query fails

If you run GitNexus manually and see an error like "Multiple repositories indexed", pass an explicit repo:

```bash
gitnexus query -r your-repo-name "your query"
```

pr-reviewer now passes `-r` automatically for GitNexus queries to avoid this ambiguity.

### GitNexus output is empty or stale

The index needs to be built for each repo before it provides useful context. Re-index:

```bash
pr-reviewer index owner/repo
# or directly:
npx gitnexus analyze /path/to/repo
```

The `.gitnexus/` directory in the repo root shows that the repository has an index. If the directory is missing, the index has never been built.

### GitNexus output appears on stderr, not stdout

This is expected. KuzuDB, the graph database of GitNexus, captures standard output at the operating system level, so GitNexus writes its output to standard error. pr-reviewer reads both streams and prefers standard error. You do not need to do anything.

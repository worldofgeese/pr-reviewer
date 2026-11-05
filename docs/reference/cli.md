# CLI reference

This page lists every `pr-reviewer` command and flag. For the keys that `config set` accepts, see the [configuration reference](configuration.md). For the variables that change the behavior of every command, see [Environment variables](environment.md).

```text
pr-reviewer <COMMAND> [OPTIONS]
```

To see the built-in help of any command, add `--help`:

```bash
pr-reviewer --help
pr-reviewer review --help
```

A command that fails prints `Error:` and its cause, and exits with status 1.

## Commands

| Command | Purpose |
|---------|---------|
| [`init`](#init) | Create the config file and the state database |
| [`add`](#add) | Watch a repository |
| [`remove`](#remove) | Stop watching a repository |
| [`list`](#list) | List the watched repositories |
| [`cleanup`](#cleanup) | Delete managed clones that no watched repository uses |
| [`index`](#index) | Refresh the GitNexus index of a repository |
| [`review`](#review) | Review one pull request |
| [`start`](#start) | Start the daemon |
| [`stop`](#stop) | Stop the daemon |
| [`status`](#status) | Show the daemon status |
| [`queue`](#queue) | Inspect and operate the maintainer work queue |
| [`logs`](#logs) | Show the review history |
| [`stats`](#stats) | Show usage statistics |
| [`config`](#config) | Read and write the configuration and the token |
| [`serve`](#serve) | Start the experimental workflow builder |

## `init`

```bash
pr-reviewer init
```

Writes a default `config.toml` and creates `state.db` in the config directory. It also writes a file `config.example.toml` in the current directory.

`init` replaces the existing `config.toml` with the defaults. Do not run it in a config directory that you already configured.

## `add`

```bash
pr-reviewer add <owner/repo> [--harness <harness>] [--model <model>] \
  [--reasoning-effort <effort>] [--fork-policy <policy>] [--path <dir>]
pr-reviewer add --scan <dir>
pr-reviewer add --org <org>
```

Adds a watched repository to the configuration. Without `--path`, `add` makes a managed clone in `repos/<owner>/<name>` in the config directory. It needs a stored token for this. If the GitNexus tool is installed, `add` also builds the index of the repository.

| Flag | Description |
|------|-------------|
| `<owner/repo>` | The repository to watch. |
| `--path <dir>` | Use an existing local clone instead of a managed clone. The directory must exist. |
| `--harness <harness>` | The AI CLI for this repository: `claude-code`, `opencode` or `codex`. The default is the value of `harness.default`. |
| `--model <model>` | The model for this repository. The default is the value of `harness.model`. |
| `--reasoning-effort <effort>` | The reasoning effort: `none`, `low`, `medium`, `high` or `xhigh`. |
| `--fork-policy <policy>` | How to treat fork PRs: `ignore` (default), `limited` or `full`. See [How a review works](../explanation/how-a-review-works.md). |
| `--scan <dir>` | Add every Git repository directly inside `<dir>`. The owner of each is `local`. Edit the owner in `config.toml` afterwards. |
| `--org <org>` | Not implemented. The command fails and tells you to use `--path`. |

`add` fails with `repo already configured` if the repository is already watched.

## `remove`

```bash
pr-reviewer remove <owner/repo> [--purge]
```

Stops watching a repository. With `--purge`, it also deletes the managed clone. `--purge` has no effect on a repository that uses `--path`.

`remove` deletes the whole entry in `config.toml`, including its `[repos.auto_fix]` block. A later `add` starts from the default settings. Before it removes the entry, `remove` compares the entry with a newly registered one. It prints a warning on standard output that lists each non-default setting that it discards, for example:

```text
warning: removing owner/repo discards these non-default settings; they are lost and `add` starts from defaults:
  auto_fix.enabled = true
  custom_instructions = "Check the migrations"
```

The list covers `harness`, `model`, `reasoning_effort`, `fork_policy`, `trusted_authors`, `ignore_paths`, `custom_instructions`, `gitnexus`, `workflow`, `auto_fix.*` and `review_policy.*`. It does not list the owner, the name or `--path`. If every setting is default, `remove` prints no warning. `remove` does not ask for confirmation. It does not touch the review history.

## `list`

```bash
pr-reviewer list
```

Prints one line for each watched repository with its clone path, harness, model, reasoning effort, fork policy and auto-fix state (`auto_fix=enabled` or `auto_fix=disabled`). A managed clone has the mark `[managed]`. `list` has no JSON form.

## `cleanup`

```bash
pr-reviewer cleanup
```

Deletes managed clones that no watched repository uses.

## `index`

```bash
pr-reviewer index <owner/repo>
pr-reviewer index --all
```

Runs `gitnexus analyze` for one watched repository, or for all of them with `--all`.

## `review`

```bash
pr-reviewer review <owner/repo>#<number> [--dry-run] [--harness <harness>] \
  [--model <model>] [--reasoning-effort <effort>] [--force] [--rerun-completed]
```

Reviews one pull request of a watched repository. The last line of the output has the form `owner/repo#N completed verdict=... comments=...`.

| Flag | Description |
|------|-------------|
| `--dry-run` | Run the whole pipeline but do not post. Set `RUST_LOG=info` to see the review text. The status reads `completed:dry-run`. |
| `--harness <harness>` | Use this AI CLI for this run only. |
| `--model <model>` | Use this model for this run only. |
| `--reasoning-effort <effort>` | Use this reasoning effort for this run only: `none`, `low`, `medium`, `high` or `xhigh`. |
| `--force` | Clear a stale or failed local claim for this commit. Review even when the diff did not change since the last review. A completed review is not affected. |
| `--rerun-completed` | Review again a commit that has a completed review in the same mode. It clears the local claim and skips the check for a review that is already on GitHub. Use it to test changes. |

## `start`

```bash
pr-reviewer start [--daemon]
```

Starts the daemon. It polls GitHub for new and updated pull requests of the watched repositories and reviews them.

Without `--daemon`, the daemon runs in the foreground and logs to the terminal. With `--daemon`, `start` runs the daemon as a background process and prints its process ID. The background process discards all output. To keep logs, use a service manager. See [Run pr-reviewer as a service](../how-to/run-as-a-service.md).

The daemon writes its process ID to `daemon.pid` in the config directory. That file is for information, `stop` and `status`. Ownership is decided by an exclusive lock on `daemon.lock` in the same directory.

Only one daemon can run per config directory:

- The daemon takes the lock when it starts and keeps it until the process exits. The operating system releases the lock when the process exits or crashes, so a crash leaves no stale lock.
- If another daemon holds the lock, `start` exits with a non-zero status. The error names the config directory and, when it can read it, the process ID of the holder. `start --daemon` reports this error too. The queue is not changed.
- After it takes the lock, the daemon puts every work item that is still `claimed` back to `pending`, and drops the matching unfinished review claims. A new process cannot own a claim, so all of them are left over from an earlier process, however recent. The work runs again.
- The lock works on one host. It does not protect a config directory on a network file system that several hosts share.

`status`, `queue` and `logs` do not take the lock. They run next to a live daemon.

## `stop`

```bash
pr-reviewer stop
```

Sends `SIGTERM` to the process ID in `daemon.pid`, removes the file and waits up to 10 seconds for the process to exit, so that a `start` right after `stop` is not refused by the lock. Do not use it for a daemon that a service manager restarts.

## `status`

```bash
pr-reviewer status [--json]
```

Shows whether the daemon runs, its process ID, start time, uptime and last heartbeat, the counts of the work queue, the last GitHub rate limit and the watched repositories. Each watched repository shows its auto-fix state (`auto_fix: enabled` or `auto_fix: disabled`). `--json` prints the same data as JSON: `watched_repos` stays a list of names, and `auto_fix_enabled` maps each name to `true` or `false`.

## `queue`

```bash
pr-reviewer queue list [--repo <owner/repo>] [--status <status>] [--limit <n>] [--json]
pr-reviewer queue show <id> [--json]
pr-reviewer queue retry <id>
pr-reviewer queue cancel <id>
```

Inspects and operates the persistent queue of maintainer work. New pull request heads, maintainer commands and repair markers enter this queue before the daemon starts the AI CLI.

| Subcommand | Description |
|------------|-------------|
| `list` | Lists work items. `--status` is one of `pending`, `claimed`, `failed`, `completed` or `canceled`. `--limit` defaults to 25. The command prints `no queued work found` for an empty queue. |
| `show <id>` | Shows one work item. |
| `retry <id>` | Puts a failed or canceled work item back in the queue. |
| `cancel <id>` | Cancels a pending or failed work item. |

Maintainers can also operate the queue from comments on a pull request: `@pr-reviewer fix`, `@pr-reviewer retry [id]` and `@pr-reviewer cancel [id]`.

## `logs`

```bash
pr-reviewer logs [--repo <owner/repo>] [--since <time>] [--model <model>] \
  [--harness <harness>] [--limit <n>]
```

Shows the review history, newest first. `--limit` defaults to 50. `--since` takes a time in the form `2026-03-01` or `2026-03-01 10:00:00`. The value is compared as text with the time of each review. Dry runs appear in the history.

## `stats`

```bash
pr-reviewer stats [--repo <owner/repo>] [--since <time>]
```

Shows the number of reviews, their outcomes and average time, and the counts by repository, model and verdict. `--since` takes the same values as for `logs`.

## `config`

```bash
pr-reviewer config list
pr-reviewer config get <key>
pr-reviewer config set <key> <value>

pr-reviewer config set-token [--stdin] [--passphrase] [--signet]
pr-reviewer config remove-token
pr-reviewer config token-status
```

| Subcommand | Description |
|------------|-------------|
| `list` | Prints the whole configuration as TOML. |
| `get <key>` | Prints one value. Keys use dots, as in `daemon.poll_interval_secs`. |
| `set <key> <value>` | Sets one value. See the [configuration reference](configuration.md) for the keys. |
| `set-token` | Encrypts a GitHub token and stores it. Without a flag, it asks for the token. |
| `remove-token` | Removes the stored token. |
| `token-status` | Shows which token source pr-reviewer uses and a preview. The preview never shows the token. |

Flags of `set-token`:

| Flag | Description |
|------|-------------|
| `--stdin` | Read the token from standard input instead of asking for it. |
| `--passphrase` | Ask for a passphrase and protect the token with it. The daemon then needs `PR_REVIEWER_PASSPHRASE`. You cannot combine this flag with `--stdin`. |
| `--signet` | Store the token in the Signet secret store instead of the config file. |

Examples:

```bash
pr-reviewer config set harness.default codex
pr-reviewer config set daemon.poll_interval_secs 60
gh auth token | pr-reviewer config set-token --stdin
```

For the order in which pr-reviewer looks for a token, see [Environment variables](environment.md#token-sources).

## `serve`

```bash
pr-reviewer serve [--port <port>]
```

Starts the experimental workflow builder. It is not ready for use and is off by default. `serve` fails until you run `pr-reviewer config set serve.enabled true`. `--port` defaults to 3848.

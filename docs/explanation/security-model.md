# Security model

This page explains how pr-reviewer protects your GitHub token and your machine, and where the protection ends. Read it before you change code that touches tokens, clones or the AI command-line tool (the **harness**). To set a token, see the [CLI reference](../reference/cli.md). For the variables, see [Environment variables](../reference/environment.md).

## What the design protects

pr-reviewer has two assets to protect and one risk to contain:

- **Your GitHub token.** It can read and write pull requests. It sits on disk for as long as the daemon runs.
- **Your machine and its other secrets.** The daemon runs as your user.
- **The risk:** the text of a pull request is untrusted. A diff, a comment or a file in it can try to steer the AI tool that reads it. The AI tool can run commands.

Each measure below answers one of these.

## Token encryption

`pr-reviewer config set-token` never writes the token in plain text. It encrypts the token twice with AES-256-GCM, so each layer authenticates the data it protects. The result is one base64 value in `github.encrypted_token` in `config.toml`.

| Layer | Key | What it protects against |
|-------|-----|--------------------------|
| Outer | The **keyfile**: 32 random bytes in `keyfile` in the config directory, mode `0600` | Someone who copies `config.toml` without the keyfile |
| Inner, with a passphrase | A key that Argon2id derives from your **passphrase** (64 MiB of memory, 3 passes, a random 16-byte salt) | Someone who copies both files |
| Inner, without a passphrase | A key that Argon2id derives from the machine identity and your user name | Only a different machine or user |

The machine identity is the content of `/etc/machine-id` on Linux and the `IOPlatformUUID` on macOS. Other users on the machine can read these values, so the inner layer without a passphrase adds little. The keyfile is the real protection in that case. Whoever can read the keyfile and `config.toml` as your user can decrypt the token.

The keyfile is created once, with an exclusive create (`O_EXCL`), so two processes cannot create two different keys at the same time. If the mode is not `0600` on a later start, pr-reviewer logs a warning.

Every encrypted value has fresh random salt and nonces. A wrong key or a changed file fails the authentication check. The error then says which layer failed: the keyfile changed, the passphrase is wrong, or the machine identity changed.

### The passphrase

With `--passphrase`, the daemon cannot start until `PR_REVIEWER_PASSPHRASE` is set. It stops with an error, not with a weaker mode. A passphrase stored next to the files it protects adds nothing, so keep it out of the config directory and out of backups of that directory. See [Back up and upgrade](../how-to/back-up-and-upgrade.md).

### Where the token comes from

pr-reviewer looks for a token in this order and uses the first one it finds:

1. The Signet secret store, if Signet is installed and holds the secret.
2. The encrypted token in `config.toml`.
3. A plain-text token in `config.toml`. This is a legacy form, and pr-reviewer logs a warning.
4. The `GITHUB_TOKEN` environment variable.

`pr-reviewer config token-status` shows the source and a preview. The preview never shows the token.

## Managed clones

A **managed clone** is the checkout that pr-reviewer keeps of each watched repository, under `repos/<owner>/<name>/` in the config directory. These rules keep the token and untrusted code apart:

- **The token is never in a URL or in `.git/config`.** The clone uses `https://github.com/<owner>/<name>.git`. Git gets the token through the `http.extraHeader` setting, as `Authorization: Basic` with `x-access-token` as the user name. pr-reviewer passes that setting in environment variables (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0` and `GIT_CONFIG_VALUE_0`), not in command-line arguments. Other users on the machine can read command-line arguments in the process list, but not the environment of your process.
- **The stored remote has no credentials.** After the clone, pr-reviewer sets the `origin` URL again without credentials.
- **Git hooks do not run.** Every Git command sets `core.hooksPath=/dev/null`, so a hook in a watched repository cannot run code.
- **The clone is only the default branch.** `--single-branch` limits what the clone fetches.
- **Fetches clean the clone.** Before a review, pr-reviewer fetches and removes untracked and ignored files from clones that it owns. It never does this to a directory that you set with `local_path`.
- **No pull request branch is checked out.** pr-reviewer builds the context of a review from GitHub API responses (the diff and file contents). The managed clone serves only the optional code index.

## Environment scrubbing

The harness is a program that runs as you, so it must not inherit your secrets. Before it starts, pr-reviewer removes these variables from its environment:

- `HOME`, `SSH_AUTH_SOCK`
- `GH_TOKEN`, `GITHUB_TOKEN`
- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`
- every variable that starts with `AWS_`, `GCP_` or `AZURE_`
- `CLAUDECODE`

It also sets `SIGNET_NO_HOOKS=1`, so that hooks of the Signet memory tool do not run inside a review.

pr-reviewer starts the harness in a new temporary directory, not in a repository. The prompt goes in through standard input where the tool allows it, because a long prompt can exceed the limit for arguments. If the harness runs past `timeout_secs`, pr-reviewer fails the review. For a harness that reads standard input, such as `claude-code`, it also kills the process when the task ends, so no process stays behind.

The `claude-code` harness runs with `--dangerously-skip-permissions`. It cannot ask a person, because nobody sits at the terminal. So pr-reviewer limits the tools instead. It starts every `claude-code` session with `--tools`, which removes each built-in tool that is not listed, and with `--strict-mcp-config`, which skips the MCP servers of your Claude Code configuration. A review session gets no built-in tools by default, so it cannot run a command, edit a file, read a file or fetch a URL, whatever the diff or the repository's instructions say. A repository can allow more tools with `review_tools`. See [Review session tools](../reference/configuration.md#review-session-tools). Auto-fix sessions edit the managed clone and use a fixed set of tools. Hooks of your Claude Code configuration still run; pr-reviewer only disables the hooks of the Signet memory tool.

## Fork pull requests

A **fork PR** comes from a repository that anyone can create. Its author controls the text that reaches the harness. The `fork_policy` setting of each watched repository decides what happens:

- `ignore` (the default) skips fork PRs.
- `limited` reviews the diff only.
- `full` gives them the same context as other PRs.

`trusted_authors` makes the listed users count as `full`. See [How a review works](how-a-review-works.md#fork-policy).

## Limits

The measures above reduce risk. They do not make a review safe against everything:

- **The harness is not in an operating system sandbox.** Scrubbing the environment and starting in a temporary directory do not stop a command that uses an absolute path. With the default `review_tools = []`, a `claude-code` review session has no tool that could do this. A session that you give `Read`, `Grep` or `Glob` can read any file that your user can read, including the keyfile and `config.toml` in the config directory, and a prompt-injected diff could get the content into a public review comment. A session with `Bash` can also use the network. The `codex` and `opencode` harnesses ignore `review_tools`, so their sessions have this reach. Run the daemon as a user that has nothing else to lose.
- **A prompt injection can still change a review.** A pull request can talk the AI tool into a wrong or an empty review. The review is advice. Treat it that way, especially for fork PRs.
- **The keyfile and `config.toml` together decrypt a token without a passphrase.** Protect the whole config directory, including backups.
- **The token has the rights you gave it.** Use a fine-grained token that is limited to the watched repositories, with the least rights that work.

## Rules that keep the model intact

Code that changes pr-reviewer keeps these rules:

- Never log or print a token, a passphrase or a header that contains one. Previews show a prefix at most.
- Never put a token in a URL, in a command-line argument or in `.git/config`.
- Pass Git authentication through the environment, as `git_basic_auth_header` in `src/repo_manager.rs` does. Keep the regression test that fixes the header format.
- Run Git against a managed clone with `core.hooksPath=/dev/null`.
- Add a new secret variable to the list in `scrub_environment` in `src/harness/spawn.rs`.
- Start a harness only through `src/harness/spawn.rs`.
- Add a test for each of these rules that you touch.

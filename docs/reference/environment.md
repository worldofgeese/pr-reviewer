# Environment variables

This page lists the environment variables that pr-reviewer reads, the location of the config directory, and the order in which pr-reviewer looks for a GitHub token.

## Variables

| Variable | Description |
|----------|-------------|
| `PR_REVIEWER_CONFIG_DIR` | The path of the config directory. If it is set, pr-reviewer uses this directory instead of the default one. Each directory has its own configuration, token, database and clones. |
| `PR_REVIEWER_PASSPHRASE` | The passphrase for a token that you stored with `--passphrase`. The daemon needs it at startup. |
| `GITHUB_TOKEN` | A fallback token. pr-reviewer uses it only when no other token source has a token. |
| `RUST_LOG` | The log level: `error`, `warn`, `info`, `debug` or `trace`. The default is `error`. Use `info` for a daemon and `debug` to find a problem. |
| `XDG_CONFIG_HOME` | On Linux, the parent of the default config directory. |
| `USER`, `LOGNAME` | The user name. pr-reviewer uses it to derive the key for a token without a passphrase. A service must provide one of them. |
| `SIGNET_NO_HOOKS` | pr-reviewer sets this to `1` in the environment of the AI CLI that it starts. You do not set it. |
| `PR_REVIEWER_INTERNAL_DAEMON` | pr-reviewer sets this when it starts the background process for `start --daemon`. Do not set it. |

For finer log filtering, name the module:

```bash
# pr-reviewer logs at debug, everything else at warn
RUST_LOG=warn,pr_reviewer=debug pr-reviewer start
```

A service has its own environment. It does not read your shell profile. See [Run pr-reviewer as a service](../how-to/run-as-a-service.md).

## Config directory

pr-reviewer keeps all of its state in one directory. If `PR_REVIEWER_CONFIG_DIR` is not set, the path depends on the operating system:

| Operating system | Default path |
|------------------|--------------|
| macOS | `~/Library/Application Support/pr-reviewer` |
| Linux | `$XDG_CONFIG_HOME/pr-reviewer`, or `~/.config/pr-reviewer` if `XDG_CONFIG_HOME` is not set |

The directory contains these files:

| Path | Content |
|------|---------|
| `config.toml` | The configuration. See the [configuration reference](configuration.md). |
| `state.db` | The SQLite database: the review log, the state of each pull request and the daemon status. |
| `state.db-wal`, `state.db-shm` | Temporary files of the database. |
| `keyfile` | The 32-byte key that encrypts the token. The file mode is `0600`. |
| `daemon.pid` | The process ID of the running daemon. For information only. |
| `daemon.lock` | The lock that allows one daemon per config directory. The running daemon holds it and writes its process ID into it. |
| `repos/<owner>/<name>/` | The managed clones. |

To run a second, separate setup, for example for a test, set `PR_REVIEWER_CONFIG_DIR` to an empty directory. The [first review tutorial](../tutorials/getting-started.md) does this.

## Token sources

pr-reviewer looks for the GitHub token in this order and uses the first one that it finds:

1. The Signet secret store, if Signet is installed and has the secret.
2. The encrypted token in `config.toml`. This is the token that `config set-token` stores.
3. A plain-text token in `config.toml`. pr-reviewer logs a warning when it uses this one.
4. The `GITHUB_TOKEN` environment variable.

Run `pr-reviewer config token-status` to see which source is active. If the token has a passphrase, the daemon reads the passphrase from `PR_REVIEWER_PASSPHRASE`.

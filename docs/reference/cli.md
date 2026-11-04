# CLI reference

```
pr-reviewer <COMMAND> [OPTIONS]
```

## Global help
```bash
pr-reviewer --help
pr-reviewer <command> --help
```

## `init`
```bash
pr-reviewer init
```
Creates config and SQLite state in `~/.config/pr-reviewer/`.

## `add`
```bash
# auto-clones to managed directory
pr-reviewer add <owner/repo> \
  [--harness claude-code|opencode|codex] \
  [--model <model>] \
  [--fork-policy ignore|limited|full]

# or use an existing local clone
pr-reviewer add <owner/repo> --path <local_path> [...]

pr-reviewer add --scan <directory>
pr-reviewer add --org <org>   # currently not implemented
```

## `remove`
```bash
pr-reviewer remove <owner/repo>
pr-reviewer remove <owner/repo> --purge   # also deletes managed clone
```

## `cleanup`
```bash
pr-reviewer cleanup   # removes orphaned managed clones
```

## `list`
```bash
pr-reviewer list
```

## `index`
```bash
pr-reviewer index <owner/repo>
pr-reviewer index --all
```
Runs `gitnexus analyze` for the target repo(s).

## `review`
```bash
pr-reviewer review <owner/repo>#<pr_number> \
  [--dry-run] \
  [--harness claude-code|opencode|codex] \
  [--model <model>] \
  [--reasoning-effort none|low|medium|high|xhigh] \
  [--force] \
  [--rerun-completed]
```
`--force` clears stale or failed local claims for the exact review mode. `--rerun-completed` is the operator escape hatch for dogfooding: it clears a completed local claim for the exact PR/SHA/harness/dry-run mode and bypasses the GitHub already-posted guard.

## `start`
```bash
pr-reviewer start
pr-reviewer start --daemon
```
Starts polling loop in foreground or background.

## `stop`
```bash
pr-reviewer stop
```

## `status`
```bash
pr-reviewer status
```
Shows daemon/process state and recent polling metadata.

## `queue`
```bash
pr-reviewer queue list [--repo <owner/repo>] [--status pending|claimed|failed|completed|canceled] [--limit <n>] [--json]
pr-reviewer queue show <id> [--json]
pr-reviewer queue retry <id>
pr-reviewer queue cancel <id>
```
Inspects and operates the persistent maintainer work queue. New PR heads, maintainer commands, and repair markers land here before the daemon invokes the configured agent.

Maintainers can also operate the queue from PR issue comments with `@pr-reviewer fix`, `@pr-reviewer retry [id]`, and `@pr-reviewer cancel [id]`. `status`, `explain`, and queue-control acknowledgements are posted through the configured agent response path.

## `logs`
```bash
pr-reviewer logs \
  [--repo <owner/repo>] \
  [--since <datetime>] \
  [--model <model>] \
  [--harness <harness>] \
  [--limit <n>]
```

## `stats`
```bash
pr-reviewer stats [--repo <owner/repo>] [--since <datetime>]
```

## `config`
```bash
pr-reviewer config list
pr-reviewer config get <key>
pr-reviewer config set <key> <value>

# Token management
pr-reviewer config set-token [--passphrase] [--signet] [--stdin] [TOKEN]
pr-reviewer config remove-token
pr-reviewer config token-status
```

Examples:
```bash
pr-reviewer config set harness.default codex
pr-reviewer config set daemon.poll_interval_secs 60
pr-reviewer config set-token --passphrase
pr-reviewer config token-status
```

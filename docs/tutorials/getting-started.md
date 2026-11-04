# Getting started

## 1. Initialize

```bash
pr-reviewer init
```

This creates `~/.config/pr-reviewer/config.toml` and the SQLite state database.

## 2. Set your GitHub token

```bash
# encrypt and store (prompted for token + optional passphrase)
pr-reviewer config set-token --passphrase

# or pipe from stdin
echo "ghp_..." | pr-reviewer config set-token --stdin

# or store in Signet (if installed)
pr-reviewer config set-token --signet

# check what's configured
pr-reviewer config token-status
```

## 3. Add a repository

```bash
# auto-clones to ~/.config/pr-reviewer/repos/ (recommended)
pr-reviewer add owner/repo \
  --harness claude-code \
  --model claude-sonnet-4-6

# or point to an existing local clone
pr-reviewer add owner/repo --path /path/to/local/clone
```

If GitNexus is installed, this automatically runs `gitnexus analyze` to index the repo.

## 4. Test with a dry run

```bash
pr-reviewer review owner/repo#42 --dry-run
pr-reviewer review owner/repo#42 --dry-run --harness codex --model gpt-5.4 --reasoning-effort low
```

This runs the full review pipeline but logs the output instead of posting to GitHub.

## 5. Review for real

```bash
pr-reviewer review owner/repo#42
```

## 6. Start the daemon

```bash
# foreground (see logs in terminal)
pr-reviewer start

# background (writes PID file, stop with `pr-reviewer stop`)
pr-reviewer start --daemon

# check status
pr-reviewer status
```

The daemon polls GitHub for new/updated PRs and automatically reviews them. It uses ETag caching and adaptive backoff to stay well within GitHub's rate limits.

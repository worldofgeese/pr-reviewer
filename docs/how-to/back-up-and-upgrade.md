# Back up and upgrade

How to back up pr-reviewer state and upgrade to a newer version.

---

## State backup and restore

The state that matters:

| File | Description |
|------|-------------|
| `~/.config/pr-reviewer/config.toml` | All configuration including repo list |
| `~/.config/pr-reviewer/state.db` | SQLite database: review log, PR state, daemon status |
| `~/.config/pr-reviewer/keyfile` | 32-byte AES key for token decryption. **Critical — without this, encrypted tokens cannot be decrypted** |

Back up all three together:

```bash
tar czf pr-reviewer-backup-$(date +%Y%m%d).tar.gz \
  -C "$HOME" \
  .config/pr-reviewer/config.toml \
  .config/pr-reviewer/state.db \
  .config/pr-reviewer/keyfile
```

Store the backup somewhere the passphrase is not also stored. If the keyfile is backed up and the backup is compromised, the token is only protected by the passphrase (if `--passphrase` was used).

### Restore

```bash
# Stop the daemon first
pr-reviewer stop

# Restore files
tar xzf pr-reviewer-backup-20260315.tar.gz -C "$HOME"

# Restart
pr-reviewer start --daemon
```

If restoring to a new machine, the keyfile must match what was used to encrypt the token. The keyfile is machine-specific — if you lose it, re-run `pr-reviewer config set-token` to re-encrypt with the new machine's keyfile.

---

## Upgrading

```bash
# Stop the daemon
pr-reviewer stop

# Pull latest and rebuild
cd /path/to/pr-reviewer
git pull
cargo install --path .

# Schema migrations run automatically on next startup
pr-reviewer start --daemon
```

The database schema is versioned with a `schema_version` table and migrations apply automatically on startup. There is no manual migration step.

If something goes wrong after an upgrade:

1. Check `RUST_LOG=debug pr-reviewer status` for startup errors
2. Roll back: `git checkout <previous-tag> && cargo install --path .`
3. The database format is backward-compatible across patch versions; major version upgrades may require a migration note in the release

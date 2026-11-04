# Back up and upgrade

This guide shows how to back up the state of pr-reviewer, restore it, and upgrade to a new version without losing your token or your review history.

Prerequisite: you know the path of your config directory. See [Environment variables](../reference/environment.md#config-directory) for the default path on each operating system.

The commands below use the shell variable `CONFIG_DIR`. Set it first:

```bash
# macOS
CONFIG_DIR="$HOME/Library/Application Support/pr-reviewer"

# Linux
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/pr-reviewer"
```

If you set `PR_REVIEWER_CONFIG_DIR`, use that path instead.

## What to back up

The whole config directory contains the state of pr-reviewer:

| Path | Content |
|------|---------|
| `config.toml` | All configuration, including the list of watched repositories |
| `state.db` | SQLite database with the review log, the state of each pull request and the daemon status |
| `state.db-wal`, `state.db-shm` | Pending writes of the database. They exist while the database is open or after a crash |
| `keyfile` | The 32-byte key that decrypts the token. Without this file, you must store the token again |
| `repos/` | The managed clones. The daemon does not clone them again on its own |

## Back up the state

To back up the state:

1. Stop the daemon. A running daemon writes to the database while you copy it.

   - For a service on macOS, run `launchctl bootout "gui/$(id -u)/com.example.pr-reviewer"`.
   - For a service on Linux, run `sudo systemctl stop pr-reviewer`.
   - For a daemon that you started with `start --daemon`, run `pr-reviewer stop`.

2. Create the archive. The `umask` makes the archive readable by you only, because it contains the keyfile.

   ```bash
   (umask 077 && tar czf "pr-reviewer-backup-$(date +%Y%m%d).tar.gz" \
     -C "$(dirname "$CONFIG_DIR")" "$(basename "$CONFIG_DIR")")
   ```

3. Start the daemon again.

Result: the file `pr-reviewer-backup-YYYYMMDD.tar.gz` contains the config directory.

Store the archive where the passphrase is not stored. If someone gets both the archive and the passphrase, they can read the token. If you did not use a passphrase, the keyfile is the only protection, and the archive contains it.

## Restore the state

To restore a backup:

1. Stop the daemon, as in the steps above.

2. Move the damaged config directory out of the way, then unpack the archive into the parent directory.

   ```bash
   mv "$CONFIG_DIR" "$CONFIG_DIR.old"
   tar xzf pr-reviewer-backup-20260315.tar.gz -C "$(dirname "$CONFIG_DIR")"
   ```

   The archive contains a folder with the name of the original config directory. If the original name differs from `$CONFIG_DIR`, rename the unpacked folder.

3. Make sure that the token and the repositories are back.

   ```bash
   pr-reviewer config token-status
   pr-reviewer list
   ```

4. Start the daemon again.

Result: `token-status` reports the stored token, and `list` shows your watched repositories.

The keyfile ties the token to its machine. If you restore to a different machine and the token does not decrypt, store the token again with `pr-reviewer config set-token`.

## Upgrade to a new version

Every release has a tag, such as `v0.2.0`. See the [releases page](https://github.com/worldofgeese/pr-reviewer/releases) for the tags and their notes.

pr-reviewer changes the database schema on its own. The first command of a new version that opens the database migrates it. That includes `status`, `list` and `logs`. An older version cannot always read a migrated database. For this reason, back up before the first command of the new version.

To upgrade:

1. Read the release notes of the new version.

2. [Back up the state](#back-up-the-state). This also stops the daemon.

3. Install the new version. Replace `v0.2.0` with the tag that you want. The `--force` flag replaces the installed binary.

   ```bash
   cargo install --git https://github.com/worldofgeese/pr-reviewer --tag v0.2.0 --locked --force
   ```

4. Make sure that the version is the one you want.

   ```bash
   pr-reviewer --version
   ```

5. Start the daemon again, and check it.

   ```bash
   pr-reviewer status
   ```

Result: `pr-reviewer status` reports the new version and a running daemon. If you run the daemon as a service, restart the service instead of running `pr-reviewer start`. See [Run pr-reviewer as a service](run-as-a-service.md).

## Roll back an upgrade

To go back to the previous version:

1. Stop the daemon.

2. Install the previous tag, as in step 3 of the upgrade.

3. Restore the backup that you made before the upgrade. The older version cannot always read the migrated database. See [Restore the state](#restore-the-state).

4. Start the daemon again.

If the daemon does not start after an upgrade, run it in the foreground with `RUST_LOG=debug pr-reviewer start` to see the error. See [Troubleshooting](../reference/troubleshooting.md#daemon-issues).

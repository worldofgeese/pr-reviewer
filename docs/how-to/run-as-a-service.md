# Run pr-reviewer as a service

How to run pr-reviewer reliably in production — as a systemd service, with secure token handling and proper logging. For backups and upgrades, see [Back up and upgrade](back-up-and-upgrade.md). For the variables the daemon reads, see the [environment reference](../reference/environment.md).

---

## Token setup for production

For any long-running setup, use `--passphrase` when storing your token. This adds an Argon2id-derived key as the inner encryption layer, so even if your keyfile is compromised, the token is still protected.

```bash
pr-reviewer config set-token --passphrase
# Prompted: enter GitHub token, then passphrase
```

Without `--passphrase`, the inner layer uses `/etc/machine-id` + username — both world-readable, so the keyfile at `~/.config/pr-reviewer/keyfile` is your only real security boundary.

When the daemon starts with a passphrase-protected token, it reads the passphrase from the `PR_REVIEWER_PASSPHRASE` environment variable. If this variable is absent, the daemon will error on startup rather than silently fail to decrypt.

---

## Running as a systemd service

### 1. Create the unit file

```ini
# /etc/systemd/system/pr-reviewer.service
[Unit]
Description=pr-reviewer daemon
After=network.target

[Service]
Type=simple
User=nicholai
EnvironmentFile=/etc/pr-reviewer/secrets
Environment=PATH=/home/nicholai/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/bin
ExecStart=/usr/local/bin/pr-reviewer start
Restart=on-failure
RestartSec=10s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

Replace `User=nicholai` with the user that has the pr-reviewer config in `~/.config/pr-reviewer/`.
Also replace the `/home/nicholai/.local/bin` PATH entry with the directory that contains your
AI CLI tools (`claude`, `opencode`, or `codex`). systemd does not inherit your interactive shell
PATH, so a CLI that works in your terminal can still be invisible to the daemon.

### 2. Create the secrets file

```bash
sudo mkdir -p /etc/pr-reviewer
sudo tee /etc/pr-reviewer/secrets > /dev/null <<'EOF'
PR_REVIEWER_PASSPHRASE=your-passphrase-here
RUST_LOG=info
EOF
sudo chmod 600 /etc/pr-reviewer/secrets
sudo chown root:root /etc/pr-reviewer/secrets
```

The secrets file must be `0600`. If the service user needs to read it, adjust ownership accordingly:

```bash
sudo chown root:nicholai /etc/pr-reviewer/secrets
```

### 3. Enable and start

```bash
sudo systemctl daemon-reload
sudo systemctl enable pr-reviewer
sudo systemctl start pr-reviewer
sudo systemctl status pr-reviewer
```

---

## Log management

### Viewing logs with journald (systemd)

```bash
# Follow live
journalctl -u pr-reviewer -f

# Last 100 lines
journalctl -u pr-reviewer -n 100

# Since a specific time
journalctl -u pr-reviewer --since "2026-03-15 10:00:00"

# Filter by log level (requires RUST_LOG=info or higher)
journalctl -u pr-reviewer -g "WARN\|ERROR"
```

### Writing to a log file instead

If you're not using systemd, redirect output:

```bash
RUST_LOG=info pr-reviewer start >> /var/log/pr-reviewer.log 2>&1 &
```

Or use a tool like `tee` to capture both to file and terminal:

```bash
RUST_LOG=info pr-reviewer start 2>&1 | tee /var/log/pr-reviewer.log
```

### Log rotation

If writing to a file, set up logrotate:

```
# /etc/logrotate.d/pr-reviewer
/var/log/pr-reviewer.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    postrotate
        # pr-reviewer has no SIGHUP handler for log-file reopen, so a full
        # restart is required. This causes a brief service interruption (~1s)
        # on each rotation. --no-block returns immediately; systemd handles
        # the restart asynchronously.
        systemctl restart pr-reviewer --no-block || true
    endscript
}
```

---

## Monitoring health

```bash
# Basic health check
pr-reviewer status

# Review history for a specific repo
pr-reviewer logs --repo owner/repo --limit 20

# Usage statistics
pr-reviewer stats --repo owner/repo
pr-reviewer stats --since 2026-03-01
```

`pr-reviewer status` shows:

- Whether the daemon process is running
- Uptime, start time, and last heartbeat
- Pending, claimed, and failed maintainer work counts
- The active worker count and legacy claimed review count
- Watched repos from config
- The last known GitHub rate limit snapshot

Add `--json` when you want the same snapshot in a machine-readable form.

Use `pr-reviewer queue list`, `pr-reviewer queue show <id>`, `pr-reviewer queue retry <id>`, and `pr-reviewer queue cancel <id>` to inspect or operate individual queued maintainer tasks.

Things to watch for in logs:

- Repeated `WARN: rate limit` — consider increasing `poll_interval_secs`
- `ERROR: harness timeout` — review pipeline failing; check harness auth or increase `timeout_secs`
- `WARN: failed to post review, retrying as COMMENT` — GitHub position validation issue; harmless, review still posts
- `ERROR: token decryption failed` — passphrase mismatch or corrupted keyfile

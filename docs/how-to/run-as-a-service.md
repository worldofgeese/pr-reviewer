# Run pr-reviewer as a service

A service keeps the pr-reviewer daemon running after you close your terminal. It also starts the daemon again after a crash or a restart of the machine. This guide covers launchd on macOS and systemd on Linux.

Prerequisites:

- You finished the [first review tutorial](../tutorials/getting-started.md), or you ran `pr-reviewer init`, stored a token and added a repository as the user that runs the service.
- You know the full path of the binary. Run `command -v pr-reviewer` to find it.
- You know which directory contains your AI CLI (`claude`, `opencode` or `codex`). Run `command -v claude` to find it.

A service does not read your shell profile. It has a short `PATH` of its own. If a CLI works in your terminal but the daemon cannot find it, the `PATH` of the service is the cause.

The `start --daemon` flag is not a service. It starts one background process and discards its output. Use a service manager when you need logs and automatic restarts.

## Protect the token with a passphrase

We recommend a passphrase for every long-running setup. Without one, the keyfile in the config directory is the only protection of the token. To add a passphrase:

1. Store the token again, this time with `--passphrase`. pr-reviewer asks for the token and then for the passphrase.

   ```bash
   pr-reviewer config set-token --passphrase
   ```

2. Keep the passphrase ready. The daemon reads it from the environment variable `PR_REVIEWER_PASSPHRASE`. It stops at startup if the variable is missing.

Result: `pr-reviewer config token-status` prints `preview: [passphrase-protected]`.

## Run on macOS with launchd

**launchd** is the service manager of macOS. A **LaunchAgent** is a service that runs for one user. To run pr-reviewer as a LaunchAgent:

1. Create the file `~/Library/LaunchAgents/com.example.pr-reviewer.plist`. Replace `/Users/you` with your home directory, because launchd does not expand `~`. Replace the `PATH` value so that it includes the directory of your AI CLI.

   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0">
   <dict>
     <key>Label</key>
     <string>com.example.pr-reviewer</string>
     <key>ProgramArguments</key>
     <array>
       <string>/Users/you/.cargo/bin/pr-reviewer</string>
       <string>start</string>
     </array>
     <key>EnvironmentVariables</key>
     <dict>
       <key>PATH</key>
       <string>/Users/you/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
       <key>RUST_LOG</key>
       <string>info</string>
     </dict>
     <key>RunAtLoad</key>
     <true/>
     <key>KeepAlive</key>
     <true/>
     <key>ThrottleInterval</key>
     <integer>10</integer>
     <key>StandardOutPath</key>
     <string>/Users/you/Library/Logs/pr-reviewer.log</string>
     <key>StandardErrorPath</key>
     <string>/Users/you/Library/Logs/pr-reviewer.log</string>
   </dict>
   </plist>
   ```

   The `PATH` must keep `/usr/sbin`. On macOS, pr-reviewer runs `ioreg` from that directory to decrypt a token that has no passphrase. Without it, the daemon stops with `failed to run ioreg`.

2. If you use a passphrase, add the variable to the `EnvironmentVariables` dictionary. Then make the file readable by you only.

   ```xml
       <key>PR_REVIEWER_PASSPHRASE</key>
       <string>your-passphrase</string>
   ```

   ```bash
   chmod 600 ~/Library/LaunchAgents/com.example.pr-reviewer.plist
   ```

3. Check the file for syntax errors.

   ```bash
   plutil -lint ~/Library/LaunchAgents/com.example.pr-reviewer.plist
   ```

4. Load and start the service.

   ```bash
   launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.example.pr-reviewer.plist
   ```

5. Make sure that the service runs.

   ```bash
   launchctl print "gui/$(id -u)/com.example.pr-reviewer" | grep -E '^\s+(state|pid)'
   pr-reviewer status
   ```

Result: `launchctl` reports `state = running` and `pr-reviewer status` reports `running`. The service starts at every login.

To restart the service, run:

```bash
launchctl kickstart -k "gui/$(id -u)/com.example.pr-reviewer"
```

To stop and unload it, run:

```bash
launchctl bootout "gui/$(id -u)/com.example.pr-reviewer"
```

Do not use `pr-reviewer stop` for a service. `KeepAlive` makes launchd start the daemon again at once.

## Run on Linux with systemd

To run pr-reviewer as a system service on Linux:

1. Create the file `/etc/systemd/system/pr-reviewer.service`. Replace `youruser` with the user that owns the pr-reviewer configuration. Replace the `PATH` value so that it includes the directory of your AI CLI. Replace the `ExecStart` path with the output of `command -v pr-reviewer`.

   ```ini
   [Unit]
   Description=pr-reviewer daemon
   After=network.target

   [Service]
   Type=simple
   User=youruser
   EnvironmentFile=/etc/pr-reviewer/secrets
   Environment=PATH=/home/youruser/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/bin
   ExecStart=/home/youruser/.cargo/bin/pr-reviewer start
   Restart=on-failure
   RestartSec=10s
   StandardOutput=journal
   StandardError=journal

   [Install]
   WantedBy=multi-user.target
   ```

2. Create the file `/etc/pr-reviewer/secrets`. It contains the passphrase and the log level. Only root and the service user can read it.

   ```bash
   sudo mkdir -p /etc/pr-reviewer
   sudo tee /etc/pr-reviewer/secrets > /dev/null <<'EOF'
   PR_REVIEWER_PASSPHRASE=your-passphrase
   RUST_LOG=info
   EOF
   sudo chown root:youruser /etc/pr-reviewer/secrets
   sudo chmod 640 /etc/pr-reviewer/secrets
   ```

   If you did not use a passphrase, remove the first line.

3. Enable and start the service.

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now pr-reviewer
   sudo systemctl status pr-reviewer
   ```

Result: `systemctl status` reports `active (running)`. The service starts at every boot.

To restart or stop the service, run `sudo systemctl restart pr-reviewer` or `sudo systemctl stop pr-reviewer`.

## Read the logs

The daemon logs at the level in `RUST_LOG`. The default level is `error`, so set `RUST_LOG=info` in the service as the examples above do. See [Environment variables](../reference/environment.md).

On macOS, read the log file that you set in the plist:

```bash
tail -f ~/Library/Logs/pr-reviewer.log
```

launchd does not rotate this file. Rotate it with `newsyslog`. See `man newsyslog.conf`.

On Linux, read the journal:

```bash
# Follow live
journalctl -u pr-reviewer -f

# Last 100 lines
journalctl -u pr-reviewer -n 100

# Since a time
journalctl -u pr-reviewer --since "2026-03-15 10:00:00"

# Warnings and errors only
journalctl -u pr-reviewer -g "WARN|ERROR"
```

To write a log file instead of the journal, redirect the output of a foreground daemon and rotate the file with logrotate:

```bash
RUST_LOG=info pr-reviewer start >> /var/log/pr-reviewer.log 2>&1
```

```text
# /etc/logrotate.d/pr-reviewer
/var/log/pr-reviewer.log {
    daily
    rotate 7
    compress
    missingok
    notifempty
    postrotate
        # pr-reviewer cannot reopen its log file, so restart it.
        # The restart interrupts the service for about one second.
        systemctl restart pr-reviewer --no-block || true
    endscript
}
```

## Check the health of the service

To check that the daemon works:

1. Show the status.

   ```bash
   pr-reviewer status
   ```

   The status shows whether the daemon runs, its uptime and last heartbeat, the queue counts, the watched repositories and the last GitHub rate limit. Add `--json` for machine-readable output.

2. List the latest reviews.

   ```bash
   pr-reviewer logs --repo owner/repo --limit 20
   ```

3. Show usage statistics.

   ```bash
   pr-reviewer stats --repo owner/repo
   ```

Watch the logs for these messages:

| Message | Meaning |
|---------|---------|
| `harness timed out after <n>s` | The AI CLI did not answer in time. Check its login or raise `harness.timeout_secs`. |
| `failed to <action>; retrying in <n>s` | A call to GitHub failed for now. pr-reviewer tries again. |
| `L1 decryption failed (keyfile may have changed)` | The keyfile is not the one that encrypted the token. Store the token again. |
| `L2 decryption failed (wrong passphrase?)` | The passphrase in `PR_REVIEWER_PASSPHRASE` is wrong. |
| `L2 decryption failed (machine identity may have changed)` | The service runs as another user or on another machine. Store the token again. |

To inspect the queue of maintainer tasks, see the `queue` commands in the [CLI reference](../reference/cli.md#queue).

## Next steps

- [Back up and upgrade](back-up-and-upgrade.md) before you install a new version.
- [Troubleshooting](../reference/troubleshooting.md) lists the common failures of the daemon.

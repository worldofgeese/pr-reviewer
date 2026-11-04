# Environment variables

| Variable | Description |
|----------|-------------|
| `PR_REVIEWER_PASSPHRASE` | Passphrase for decrypting tokens stored with `--passphrase`. Required if token was encrypted with a passphrase. |
| `GITHUB_TOKEN` | Fallback token source if no encrypted token is configured. Lowest priority in the resolution chain. |
| `RUST_LOG` | Log level filter. Values: `error`, `warn`, `info`, `debug`, `trace`. Default: `error`. Use `info` for production, `debug` for diagnosis. |
| `SIGNET_NO_HOOKS` | Set to `1` to disable Signet hook invocation. pr-reviewer sets this automatically in spawned harness environments. |

For fine-grained log filtering:

```bash
# Log only pr-reviewer's own output at debug, everything else at warn
RUST_LOG=warn,pr_reviewer=debug pr-reviewer start
```

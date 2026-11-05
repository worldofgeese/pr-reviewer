<p align="center">
  <img src="public/logo.png" alt="pr-reviewer" width="280" />
</p>

<h1 align="center">pr-reviewer</h1>

<p align="center">
  Open-source, self-hosted PR review tool. Like Greptile or CodeRabbit, but runs on your machine.
</p>

---

pr-reviewer watches your GitHub repositories for pull requests, starts a local AI coding CLI to review each one, and posts the review back to GitHub. It needs no API keys. It uses the subscription of the CLI that you already have: Claude Code, OpenCode or Codex.

- **Review cycles with a limit.** After a set number of review cycles, new findings become follow-up notes that do not block the merge.
- **Stacked PRs.** A rebase of a stack does not start new reviews.
- **Your harness, your model.** Choose the AI CLI and the model for each repository.
- **Protected secrets.** The token is encrypted at rest, and the AI CLI gets a scrubbed environment. See the [security model](docs/explanation/security-model.md).
- **Idempotent.** A restart never posts a review twice.

## Install

You need a [Rust toolchain](https://rustup.rs/), a GitHub token and at least one AI CLI that you have signed in to. Install the released version:

```bash
cargo install --git https://github.com/worldofgeese/pr-reviewer --tag v0.2.0 --locked
```

With Nix, you need no Rust toolchain. The repository is a flake that builds the same code:

```bash
nix run github:worldofgeese/pr-reviewer -- --version
```

To use it from another flake, add `inputs.pr-reviewer.url = "github:worldofgeese/pr-reviewer"` and use `inputs.pr-reviewer.packages.${system}.default`.

Then follow the [first review tutorial](docs/tutorials/getting-started.md). It takes about 15 minutes and ends with a real review on a repository that you can throw away.

## Documentation

Start at the [documentation index](docs/README.md). It routes you by role.

- **You write pull requests:** [How a review works](docs/explanation/how-a-review-works.md)
- **You run pr-reviewer:** [Tutorial](docs/tutorials/getting-started.md), [run as a service](docs/how-to/run-as-a-service.md), [back up and upgrade](docs/how-to/back-up-and-upgrade.md), [CLI](docs/reference/cli.md), [configuration](docs/reference/configuration.md), [environment variables](docs/reference/environment.md) and [troubleshooting](docs/reference/troubleshooting.md)
- **You change pr-reviewer:** [Contribute a change](docs/how-to/contribute-a-change.md), [bring in an upstream change](docs/how-to/bring-in-an-upstream-change.md) and the [security model](docs/explanation/security-model.md)

To report a bug, [open an issue](https://github.com/worldofgeese/pr-reviewer/issues).

## License

[MIT](LICENSE). pr-reviewer is a hard fork of [NicholaiVogel/pr-reviewer](https://github.com/NicholaiVogel/pr-reviewer). See [ADR-0001](docs/adr/0001-maintain-a-hard-fork.md).

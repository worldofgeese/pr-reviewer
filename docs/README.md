# pr-reviewer documentation

pr-reviewer is a self-hosted daemon that reviews GitHub pull requests with a local AI coding CLI and posts each review back to GitHub. This page is the start of the documentation. Pick the route that matches what you do.

## Start here

### I write pull requests that pr-reviewer reviews

Read [How a review works](explanation/how-a-review-works.md). It explains review cycles, follow-up notes, stacked PRs, fork PRs and confidence.

### I run pr-reviewer

1. Follow the [first review tutorial](tutorials/getting-started.md). You install version 0.2.0, review a pull request in a throwaway repository and start the daemon. It takes about 15 minutes.
2. [Run pr-reviewer as a service](how-to/run-as-a-service.md) with launchd on macOS or systemd on Linux.
3. [Back up and upgrade](how-to/back-up-and-upgrade.md) before you install a new version.
4. Look things up in the [CLI reference](reference/cli.md), the [configuration reference](reference/configuration.md) and the [environment variables](reference/environment.md).
5. If something fails, see [Troubleshooting](reference/troubleshooting.md).

### I maintain or contribute to pr-reviewer

1. [Contribute a change](how-to/contribute-a-change.md) from clone to release: issues, triage labels, tests, the pull request template and merge commits.
2. [Bring in an upstream change](how-to/bring-in-an-upstream-change.md) with a cherry-pick, as [ADR-0001](adr/0001-maintain-a-hard-fork.md) requires.
3. Read the [security model](explanation/security-model.md) before you touch tokens, clones or the AI CLI.
4. [AGENTS.md](../AGENTS.md) describes the architecture and conventions, and [GLOSSARY.md](../GLOSSARY.md) fixes the words that the code and docs use.
5. [docs/adr/](adr/) holds the architecture decision records.

## Contents

| Kind | Page |
|------|------|
| Tutorial | [Run your first review](tutorials/getting-started.md) |
| How-to guide | [Run as a service](how-to/run-as-a-service.md) |
| How-to guide | [Back up and upgrade](how-to/back-up-and-upgrade.md) |
| How-to guide | [Contribute a change](how-to/contribute-a-change.md) |
| How-to guide | [Bring in an upstream change](how-to/bring-in-an-upstream-change.md) |
| Reference | [CLI](reference/cli.md) |
| Reference | [Configuration](reference/configuration.md) |
| Reference | [Environment variables](reference/environment.md) |
| Reference | [Troubleshooting](reference/troubleshooting.md) |
| Explanation | [How a review works](explanation/how-a-review-works.md) |
| Explanation | [Security model](explanation/security-model.md) |

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

- [CONTRIBUTING.md](../CONTRIBUTING.md) explains how to build, test and submit changes.
- [AGENTS.md](../AGENTS.md) describes the architecture and conventions.
- [GLOSSARY.md](../GLOSSARY.md) fixes the words that the code and docs use.
- [docs/adr/](adr/) holds the architecture decision records, such as [ADR-0001](adr/0001-maintain-a-hard-fork.md).

## Contents

| Kind | Page |
|------|------|
| Tutorial | [Run your first review](tutorials/getting-started.md) |
| How-to guide | [Run as a service](how-to/run-as-a-service.md) |
| How-to guide | [Back up and upgrade](how-to/back-up-and-upgrade.md) |
| Reference | [CLI](reference/cli.md) |
| Reference | [Configuration](reference/configuration.md) |
| Reference | [Environment variables](reference/environment.md) |
| Reference | [Troubleshooting](reference/troubleshooting.md) |
| Explanation | [How a review works](explanation/how-a-review-works.md) |

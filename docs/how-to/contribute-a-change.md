# Contribute a change

This guide shows how to take a change from an idea to a merged pull request, and how to release it. It is for maintainers of pr-reviewer and for anyone who sends a fix.

Prerequisites:

- Git, and the [GitHub CLI](https://cli.github.com/) (`gh`) signed in to an account that can push to `worldofgeese/pr-reviewer`.
- The stable Rust toolchain, version 1.75 or later. Install it with [rustup](https://rustup.rs/).
- Optional: the beads CLI (`bd`), for the work tracker. See [Pick up work](#pick-up-work).

pr-reviewer is a hard fork of its upstream. Your change lands on our `main` and does not go to upstream. See [ADR-0001](../adr/0001-maintain-a-hard-fork.md). To bring in a change that upstream made, see [Bring in an upstream change](bring-in-an-upstream-change.md).

## Set up your clone

To set up your clone:

1. Clone the repository and go into it.

   ```bash
   git clone https://github.com/worldofgeese/pr-reviewer.git
   cd pr-reviewer
   ```

2. Make `gh` target our repository by default.

   ```bash
   gh repo set-default worldofgeese/pr-reviewer
   ```

   The repository is a GitHub fork. Without this step, `gh pr create` and `gh issue create` can pick upstream. In scripts, also pass `-R worldofgeese/pr-reviewer`.

3. Build and test, to make sure that your clone works.

   ```bash
   cargo test
   ```

Result: the tests pass on a clean `main`.

## Pick up work

Two tools share the work, and each has one job:

- **GitHub Issues** on `worldofgeese/pr-reviewer` take reports from the public.
- **Beads** (`bd`) tracks the work: specifications, tickets and the order between them. Beads stores its data in this repository.

To pick up work:

1. Triage new reports. List the issues that wait for a decision.

   ```bash
   gh issue list -R worldofgeese/pr-reviewer --state open --label needs-triage
   ```

2. Label each issue with exactly one category label and one state label.

   | Kind | Label | Meaning |
   |------|-------|---------|
   | Category | `bug` | Something is broken. |
   | Category | `enhancement` | A new feature or an improvement. |
   | State | `needs-triage` | A maintainer must decide what to do. |
   | State | `needs-info` | The reporter must answer a question. |
   | State | `ready-for-agent` | The issue is fully specified, so an agent can do it without a human. |
   | State | `ready-for-human` | A human must do the work. |
   | State | `wontfix` | We do not plan to act on it. |

   Remove `needs-triage` when you set the new state. Post your comment before you close an issue.

3. Set up beads on a fresh clone. The command never deletes existing data.

   ```bash
   bd bootstrap
   ```

4. Pull the new issues into beads. Pass the token in the environment. Never store it with `bd config`, because `.beads/config.yaml` is public.

   ```bash
   GITHUB_TOKEN="$(gh auth token)" bd github sync --pull-only
   ```

   Each bead keeps `gh-<number>` as its external reference.

5. Find work and claim it.

   ```bash
   bd ready --json
   bd update <id> --claim
   ```

Beads data syncs to this public repository. Keep confidential details out of every bead. Do not run `bd edit`, because it opens an interactive editor.

For all commands of both tools, see the [issue tracker notes](../agents/issue-tracker.md) and the [triage labels](../agents/triage-labels.md).

## Do not delete the beads data on GitHub

Beads keeps its data on GitHub, in two places that look like clutter:

- the ref `refs/dolt/data`
- the branch `__dolt_remote_info__`, which `bd dolt push` creates as Dolt metadata

Never delete either one. If you delete them, the shared beads data is gone, and `bd bootstrap` can no longer restore it on a fresh clone. Leave the branch in place when you clean up branches.

## Change the code

To change the code:

1. Create a branch from `main`.

   ```bash
   git switch -c fix/short-description origin/main
   ```

2. Read [GLOSSARY.md](../../GLOSSARY.md) and use its words in code, tests, comments and messages. For example, write "review cycle", not "review round".

3. Write a test that fails for the right reason. Test behavior from the outside, not how the code works inside. The highest seam is the whole review: `src/review/pipeline_tests.rs` runs review cycles against a mock GitHub and a stub `claude` harness. Add your test there when you can.

4. Make the test pass with the smallest change.

5. Run the same three checks as CI. The CI workflow in `.github/workflows/ci.yml` runs them on every pull request and on every push to `main`.

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

   Run `cargo fmt` to fix formatting.

6. Commit with a [Conventional Commits](https://www.conventionalcommits.org/) message. Explain why, not only what.

   ```text
   fix(repo_manager): authenticate git over HTTPS with Basic, not Bearer
   ```

Result: a branch with tests and a clean `fmt`, `clippy` and `cargo test`.

### Follow the code style

- Return `anyhow::Result` and add `context()` to every call that can fail. Do not use `unwrap()` or `expect()` outside tests.
- Keep files under about 700 lines and nesting under 3 levels.
- Let `cargo fmt` set the line width.
- Log with the `tracing` crate.
- Decide whether two findings are the same only through `policy::same_finding`. Do not add another rule. See [AGENTS.md](../../AGENTS.md).

[AGENTS.md](../../AGENTS.md) describes the architecture and the main patterns.

## Open the pull request

To open the pull request:

1. Push the branch.

   ```bash
   git push -u origin HEAD
   ```

2. Open the pull request against `main` on our repository.

   ```bash
   gh pr create -R worldofgeese/pr-reviewer --base main --fill
   ```

3. Replace the body with the template below. Keep each part short. Show the evidence, such as a failing test that now passes.

   ````markdown
   ## Summary

   <a diagram, a diff sketch or a file tree that shows the key point>

   ## Evidence

   - **Before:** <the failing test run, output or screenshot>
     **After:** <the passing test run, output or screenshot>

   ## Merge Danger

   **Door:** <one-way or two-way>

   **Blast Radius:** <one word>
   ````

   A two-way door is a change that you can roll back. A one-way door is a change that you cannot, such as a change to the stored data format. The blast radius is the scope of what the change can break.

4. To close an issue with the merge, write `Fixes #<number>` in the body. The number must be an issue of our repository.

5. Wait for the CI check to pass.

## Merge the pull request

The repository allows merge commits, and we use them. The repository deletes the head branch after the merge.

To merge:

1. Make sure that the CI check passed and that you resolved every review thread.
2. Merge with a merge commit.

   ```bash
   gh pr merge <number> -R worldofgeese/pr-reviewer --merge
   ```

3. Close the work. `Fixes #<number>` closes the issue. Close the bead yourself.

   ```bash
   bd close <id> --reason "<what changed>, <pull request URL>"
   bd dolt push
   ```

If GitHub returns a 5xx error, wait and try again with a longer pause each time. Do not work around an outage.

## Release a version

Releases use the version in `Cargo.toml`. The install command in the documentation builds the tag.

To release a version:

1. Make sure that `main` has everything you want to ship and that CI passed on it.

2. Raise the version in `Cargo.toml`. Build once, so that `Cargo.lock` follows.

   ```bash
   cargo check
   ```

3. Commit both files, open a pull request and merge it as above. Use this message.

   ```text
   chore(release): bump version to 0.3.0
   ```

4. Update your clone, and tag the merge commit. The tag must start with `v`.

   ```bash
   git switch main && git pull
   git tag -a v0.3.0 -m "v0.3.0"
   git push origin v0.3.0
   ```

5. Create the release. Group the notes under **Fixes** and **Install**, as in [the release of v0.2.0](https://github.com/worldofgeese/pr-reviewer/releases/tag/v0.2.0). Link each pull request.

   ```bash
   gh release create v0.3.0 -R worldofgeese/pr-reviewer --title v0.3.0 --notes-file notes.md
   ```

6. Make sure that the install command builds the tag.

   ```bash
   cargo install --git https://github.com/worldofgeese/pr-reviewer --tag v0.3.0 --locked
   pr-reviewer --version
   ```

Result: `pr-reviewer --version` prints `pr-reviewer 0.3.0`. Everyone who runs a pinned build can now move to the new tag. We own releases and security fixes, because nobody else ships them for us.

## Add a harness

A **harness** is the AI command-line tool that pr-reviewer starts to write a review. To add one:

1. Create `src/harness/my_harness.rs` and implement the `Harness` trait.
2. Add the variant to `HarnessKind` in `src/harness/mod.rs`, and add it to `create_harness()` in the same file.
3. Add a test that checks the command that the harness builds.
4. Add the harness to the table in the [configuration reference](../reference/configuration.md#harness-selection-guide).

## Report a bug

Open an issue on `worldofgeese/pr-reviewer`, never on upstream. Include:

- what you expected,
- what happened,
- the steps to reproduce it,
- the output of `pr-reviewer status`,
- the relevant log lines.

For the common failures, see [Troubleshooting](../reference/troubleshooting.md) first.

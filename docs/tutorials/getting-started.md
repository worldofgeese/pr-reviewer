# Run your first review

In this tutorial, you install pr-reviewer, point it at a throwaway repository, and watch it review a pull request. You finish with a running daemon. The whole tutorial takes about 15 minutes.

Everything runs in a temporary **config directory**. The config directory is the folder where pr-reviewer keeps its configuration, token, database and clones. Because the tutorial uses its own folder, it cannot disturb a daemon that already runs on your machine.

## Before you begin

You need:

- A Rust toolchain. Install it with [rustup](https://rustup.rs/).
- `git` and the GitHub CLI (`gh`), signed in to GitHub with `gh auth login`.
- One AI coding CLI that is signed in. The tutorial uses `claude` (Claude Code). `codex` and `opencode` also work. See the [configuration reference](../reference/configuration.md#harness) for how to choose.
- A private GitHub repository that you can push to and that has at least one commit. This is your **sandbox repository**. Use a throwaway repository with no other open pull requests. The tutorial opens and closes a pull request in it, and the daemon reviews every open pull request that it finds.

The examples show the sandbox repository `worldofgeese/pr-reviewer-livetest`. Your output shows your own repository name.

## Install pr-reviewer

To install version 0.2.0:

1. Run the install command. It builds the tag `v0.2.0` and takes a few minutes.

   ```bash
   cargo install --git https://github.com/worldofgeese/pr-reviewer --tag v0.2.0 --locked
   ```

   The last lines of the output name the installed version:

   ```text
   Installing /home/you/.cargo/bin/pr-reviewer
   Installed package `pr-reviewer v0.2.0 (https://github.com/worldofgeese/pr-reviewer?tag=v0.2.0#e63f7947)` (executable `pr-reviewer`)
   ```

2. Make sure that `~/.cargo/bin` is on your `PATH`, then check the version.

   ```bash
   pr-reviewer --version
   ```

   ```text
   pr-reviewer 0.2.0
   ```

## Use a separate config directory

To keep the tutorial apart from any real setup:

1. Create a temporary directory and tell pr-reviewer to use it.

   ```bash
   export PR_REVIEWER_CONFIG_DIR="$(mktemp -d)"
   ```

2. Keep the same terminal open for the rest of the tutorial. The variable only exists in this terminal.

Result: every `pr-reviewer` command in this terminal now reads and writes `$PR_REVIEWER_CONFIG_DIR`. See [Environment variables](../reference/environment.md) for the full list.

## Initialize the config directory

To create the configuration file and the state database:

1. Run `init`.

   ```bash
   pr-reviewer init
   ```

   ```text
   initialized config at /tmp/tmp.XXXXXXXXXX/config.toml
   initialized db at /tmp/tmp.XXXXXXXXXX/state.db
   ```

Result: the directory now contains `config.toml` and `state.db`. The configuration starts with the default harness `claude-code` and the model `claude-sonnet-4-6`. The command also writes `config.example.toml` in the current directory. You can delete that file.

## Store a GitHub token

pr-reviewer needs a GitHub token to read pull requests and post reviews. To store one:

1. Pipe the token of your `gh` session into `config set-token`. The token never appears on screen.

   ```bash
   gh auth token | pr-reviewer config set-token --stdin
   ```

   ```text
   token encrypted (machine-bound) and saved to config
   ```

2. Check which source pr-reviewer uses.

   ```bash
   pr-reviewer config token-status
   ```

   ```text
   source: encrypted config
   preview: [machine-bound]
   ```

Result: pr-reviewer stores the token encrypted in the config directory. For a long-running setup, protect the token with a passphrase. See [Run pr-reviewer as a service](../how-to/run-as-a-service.md).

## Add the sandbox repository

pr-reviewer reviews **watched repositories**. For each one, it keeps a **managed clone**, which is a local checkout in the config directory.

To watch your sandbox repository:

1. Set a variable with the name of your sandbox repository. Replace the value with your own `owner/name`.

   ```bash
   export SANDBOX_REPO=worldofgeese/pr-reviewer-livetest
   ```

2. Add the repository.

   ```bash
   pr-reviewer add "$SANDBOX_REPO"
   ```

   ```text
   cloned to /tmp/tmp.XXXXXXXXXX/repos/worldofgeese/pr-reviewer-livetest
   added repo worldofgeese/pr-reviewer-livetest
   gitnexus index built for worldofgeese/pr-reviewer-livetest
   ```

   The last line only appears when the optional GitNexus tool is installed. A warning there does not stop the tutorial.

3. List the watched repositories.

   ```bash
   pr-reviewer list
   ```

   ```text
   worldofgeese/pr-reviewer-livetest  path=/tmp/tmp.XXXXXXXXXX/repos/worldofgeese/pr-reviewer-livetest [managed]  harness=claude-code  model=claude-sonnet-4-6  reasoning=-  fork_policy=Ignore
   ```

Result: the clone proves that your token can read the repository over HTTPS.

## Open a throwaway pull request

pr-reviewer needs a pull request to review. To create one in the sandbox repository:

1. Clone the sandbox repository into a temporary directory and create a branch.

   ```bash
   cd "$(mktemp -d)"
   gh repo clone "$SANDBOX_REPO" sandbox
   cd sandbox
   git switch -c tutorial/add-average
   ```

2. Add a small file that has an obvious weakness.

   ```bash
   cat > average.py <<'EOF'
   def average(values):
       return sum(values) / len(values)
   EOF
   ```

3. Commit, push and open the pull request.

   ```bash
   git add average.py
   git commit -m "Add average helper"
   git push -u origin tutorial/add-average
   gh pr create --fill
   ```

   The last line of the output is the address of the new pull request:

   ```text
   https://github.com/worldofgeese/pr-reviewer-livetest/pull/4
   ```

4. Save the number of the pull request.

   ```bash
   PR_NUMBER="$(gh pr view --json number --jq .number)"
   echo "$PR_NUMBER"
   ```

   ```text
   4
   ```

Result: your sandbox repository has an open pull request from the branch `tutorial/add-average`.

## Run a dry run

A **dry run** runs the whole review but prints the result instead of posting it. To start one:

1. Run `review` with `--dry-run`. `RUST_LOG=info` makes pr-reviewer print the review. The command takes from 20 seconds to a minute.

   ```bash
   RUST_LOG=info pr-reviewer review "$SANDBOX_REPO#$PR_NUMBER" --dry-run
   ```

   The output is shortened here. The timestamps are left out, and your review text differs.

   ```text
   INFO pr_reviewer: GitHub token resolved source=encrypted config
   INFO pr_reviewer::context::retriever: assembled review context repo=worldofgeese/pr-reviewer-livetest pr=4 files_included=1 ...
   INFO pr_reviewer::review::engine: built review prompt repo=worldofgeese/pr-reviewer-livetest pr=4 ...
   INFO pr_reviewer::review::engine: dry run, review not posted:
   <details>
   <summary>Review metadata</summary>

   - Reviewer: [pr-reviewer](https://github.com/worldofgeese/pr-reviewer)
   - Model: `claude-sonnet-4-6`
   - Commit: `a02948c0`

   </details>

   The function is correct for the happy path but raises ZeroDivisionError on empty input and TypeError on non-numeric elements with no handling or documentation of these cases. No tests were added.

   **Confidence:** High [sufficient_diff_evidence] - Both issues are directly provable from the two-line implementation: `len([])` is 0, and `sum(['a', 'b'])` raises TypeError. No cross-module context is needed to confirm these.
    repo=worldofgeese/pr-reviewer-livetest pr=4 round=1 verdict="COMMENT" inline=["average.py:2 Empty input causes ZeroDivisionError ..."]
   worldofgeese/pr-reviewer-livetest#4 completed:dry-run verdict=Some(Comment) comments=2
   ```

2. Open the pull request in your browser. It has no review yet.

Result: the last line reports `completed:dry-run`, the verdict of the review and the number of **findings**. A finding is one problem that the review reports. Nothing reached GitHub.

## Post a live review

To post a review:

1. Run `review` without `--dry-run`.

   ```bash
   pr-reviewer review "$SANDBOX_REPO#$PR_NUMBER"
   ```

   ```text
   worldofgeese/pr-reviewer-livetest#4 completed verdict=Some(Comment) comments=2
   ```

2. Open the pull request in your browser.

Result: the pull request has a review from your GitHub account with a summary and inline comments. The verdict is `COMMENT`, because GitHub does not let you approve or request changes on your own pull request. pr-reviewer also adds a comment that records which commit it reviewed. To see how a review works, read [How a review works](../explanation/how-a-review-works.md).

## Check the review history

To see what pr-reviewer did:

1. List the review history.

   ```bash
   pr-reviewer logs
   ```

   ```text
   #2 2026-11-04 11:25:47 pr#4 sha=a02948c0 worldofgeese/pr-reviewer-livetest completed comments=Some(2) verdict=Some("COMMENT") duration=Some(21.006610708) ...
   #1 2026-11-04 11:25:18 pr#4 sha=a02948c0 worldofgeese/pr-reviewer-livetest completed comments=Some(2) verdict=Some("COMMENT") duration=Some(17.086816542) ...
   ```

Result: one line for each review, dry runs included.

## Start the daemon

The daemon polls GitHub and reviews new and updated pull requests on its own. To start it:

1. Start the daemon in the background and wait a few seconds.

   ```bash
   pr-reviewer start --daemon
   sleep 8
   ```

   ```text
   daemon started with pid 7394
   ```

2. Check the status.

   ```bash
   pr-reviewer status
   ```

   ```text
   pr-reviewer 0.2.0
   self-hosted PR review daemon

   Status
     running
     pid: 7394
     started: 2026-11-04T11:26:37.092517+00:00
     uptime: 3s
     last heartbeat: 2026-11-04T11:26:39.271072+00:00 (1s)

   Queue
     pending work: 0
     ...

   Rate limit
     remaining: 4710 / 5000
     resets at: 2026-11-04T11:37:17+00:00

   Watched repos (1)
     - worldofgeese/pr-reviewer-livetest
   ```

Result: the daemon runs and watches your sandbox repository. It does not review the pull request again, because that commit already has a review.

## Clean up

To remove everything that the tutorial made:

1. Stop the daemon.

   ```bash
   pr-reviewer stop
   ```

   ```text
   stopped daemon pid 7394
   ```

2. Close the pull request and delete its branch.

   ```bash
   gh pr close "$PR_NUMBER" --delete-branch
   ```

3. Delete the config directory and forget the variables.

   ```bash
   rm -rf "$PR_REVIEWER_CONFIG_DIR"
   unset PR_REVIEWER_CONFIG_DIR SANDBOX_REPO PR_NUMBER
   ```

## Next steps

- [Run pr-reviewer as a service](../how-to/run-as-a-service.md) so reviews continue after you close the terminal.
- [Back up and upgrade](../how-to/back-up-and-upgrade.md) before you move to a new version.
- If a step failed, see [Troubleshooting](../reference/troubleshooting.md).
- Add your real repositories with `pr-reviewer add`. See the [CLI reference](../reference/cli.md).

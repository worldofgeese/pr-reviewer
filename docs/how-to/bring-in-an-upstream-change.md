# Bring in an upstream change

This guide shows how to take one fix from **upstream** (`NicholaiVogel/pr-reviewer`, the project that pr-reviewer forked from) into our `main`. You pick each change on purpose with `git cherry-pick`.

[ADR-0001](../adr/0001-maintain-a-hard-fork.md) sets the rules:

- Our `main` is the source of truth. Upstream is a read-only remote.
- Changes come in by deliberate cherry-pick, never by a bulk merge or a rebase on upstream.
- We do not open issues or pull requests on upstream, and we do not comment there.

Prerequisites: a clone that you set up as in [Contribute a change](contribute-a-change.md#set-up-your-clone), and a clean working tree.

## Add the upstream remote

Skip this part if `git remote -v` already lists `upstream`.

To add the remote:

1. Add upstream and fetch it.

   ```bash
   git remote add upstream https://github.com/NicholaiVogel/pr-reviewer.git
   git fetch upstream
   ```

2. Turn off pushing to it. This makes a push to upstream fail, so nobody sends a change there by mistake.

   ```bash
   git remote set-url --push upstream DISABLED
   ```

Result: `git remote -v` shows `upstream DISABLED (push)`.

## Find a change to bring in

To list the upstream commits that we do not have yet:

```bash
git fetch upstream
git cherry -v main upstream/main
```

A line that starts with `+` is a commit that has no equal change in `main`. A line that starts with `-` has one already. The histories have split, so some old commits can show `+` even though we hold the same fix under a different shape. Read the change before you decide.

To read one commit:

```bash
git show <sha>
```

Ask these questions:

- Does it fix a bug that we have? Reproduce the bug on `main` first.
- Does the code it touches still exist on `main`? Our review cycle and stacked PR code differs from upstream.
- Is the licence the same? Upstream is MIT, like us, and the notice stays in `LICENSE`.

## Cherry-pick the change

To bring the change in:

1. Create a branch from `main`.

   ```bash
   git switch -c fix/from-upstream-short-name origin/main
   ```

2. Cherry-pick the commit. The `-x` flag adds the line `(cherry picked from commit <sha>)` to the message, which keeps the link to the source.

   ```bash
   git cherry-pick -x <sha>
   ```

3. If Git reports conflicts, fix them, then run `git add <files>` and `git cherry-pick --continue`. If the conflicts show that the code has moved too far, run `git cherry-pick --abort`. Then write the fix again on your own branch, test first, and name the upstream commit in the message.

4. Fix the commit message. Use a [Conventional Commits](https://www.conventionalcommits.org/) subject. Do not leave a bare `#<number>` from upstream in the message, because GitHub links it to the issue or pull request with that number on our repository. Write "upstream issue 20" instead.

5. Add or adjust a test. A change that arrives without a test does not land.

6. Run the checks of CI.

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

Result: one branch with the change, its test and a clean build.

## Open the pull request

Follow [Open the pull request](contribute-a-change.md#open-the-pull-request) and [Merge the pull request](contribute-a-change.md#merge-the-pull-request). In the **Summary**, name the upstream commit as plain text, for example "from upstream commit 4d27724". Do not paste a link to an upstream issue or pull request, and do not comment on upstream.

If upstream fixed a bug that we also track, link our issue with `Fixes #<number>`. The number must be an issue of our repository.

## What not to do

- Do not run `git merge upstream/main`. It brings in every change at once, including changes we rejected.
- Do not run `git rebase upstream/main` on `main` or on a branch of ours.
- Do not push to upstream. The remote refuses it.
- Do not open an issue, a pull request or a comment on upstream.

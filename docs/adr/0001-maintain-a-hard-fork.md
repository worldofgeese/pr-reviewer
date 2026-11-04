---
status: accepted
date: 2026-11-04
---

# Maintain pr-reviewer as a hard fork of upstream

Upstream (`NicholaiVogel/pr-reviewer`) has merged nothing since 2026-05-01 and
has not answered issues #21 to #31 or pull request #32. Those fix bugs that
block our use: git clones fail on Bearer auth, re-reviews have no limit, and
every rebase of a stack is reviewed again. So `worldofgeese/pr-reviewer` is
now the source of truth. Work lands on our `main`, we do not propose changes
upstream, and upstream is a read-only remote that we cherry-pick from on
purpose.

## Considered options

- **Soft fork**: keep sending pull requests upstream and rebase on it. We
  rejected it because upstream does not respond, so our fixes would wait
  forever.
- **Local patches on top of upstream** (what we did before): the fixes lived
  as patch files in one machine's Nix configuration, where teammates could
  not find, review or install them.

## Consequences

- Teammates install, report issues against and contribute to
  `worldofgeese/pr-reviewer`.
- We own releases and security fixes. Nobody else ships them for us.
- Changes from upstream come in only by deliberate cherry-pick, never by a
  bulk merge.
- The MIT licence notice of upstream stays in `LICENSE`.

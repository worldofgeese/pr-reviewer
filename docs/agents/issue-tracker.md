# Issue tracker: GitHub Issues (intake) + beads (work)

This repo uses two tools, each with one job:

- **GitHub Issues** on `worldofgeese/pr-reviewer` are the public intake.
  Anyone can report a bug or ask for a feature there. `/triage` works on them
  with the labels in `triage-labels.md`.
- **Beads** (`bd`) is the work tracker: specs, tickets, blocking edges and
  follow-up work. Run `bd prime` at the start of a session.

This repo is a GitHub fork, so `gh` can default to upstream
(the upstream in `GLOSSARY.md`). Run `gh repo set-default worldofgeese/pr-reviewer`
once per clone, and pass `-R worldofgeese/pr-reviewer` in scripts. Never file
issues, comment or open pull requests upstream (ADR-0001).

## GitHub Issues (intake)

- **Create**: `gh issue create -R worldofgeese/pr-reviewer --title "..." --body-file -`
- **Read**: `gh issue view <n> -R worldofgeese/pr-reviewer --json number,title,body,labels,comments`
- **List for triage**: `gh issue list -R worldofgeese/pr-reviewer --state open --label needs-triage`
- **Comment / label / close**: `gh issue comment`, `gh issue edit --add-label` /
  `--remove-label`, `gh issue close`. Post the comment before you close.
- **Into beads**: `bd github sync --pull-only`. Each bead keeps `gh-<n>` as its
  external reference. Pass the token in the environment
  (`GITHUB_TOKEN="$(gh auth token)" bd github sync --pull-only`); never store
  it with `bd config`, because `.beads/config.yaml` is committed to this
  public repo.
- **Close the loop**: the fixing pull request says `Fixes #<n>`. The merge
  closes the issue; then close the bead.

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external
PRs as feature requests; `/triage` reads this flag.)_

## Beads (work)

- **Create**: `bd create "<title>" -t task -p 2 --body-file - --acceptance "..."`
  (types: `bug`, `feature`, `task`, `epic`, `chore`, `decision`)
- **Child of a spec**: add `--parent <epic-id>`
- **Blocking edge**: `bd dep add <id> --blocked-by <blocker-id>`
- **Frontier**: `bd ready --json`, or `bd ready --parent <epic-id> --json`
- **Read / claim / close**: `bd show <id>`, `bd update <id> --claim`,
  `bd close <id> --reason "..."`
- **Make one bead public**: `bd github push <id>`
- Never run `bd edit`: it opens an interactive editor.

Beads data syncs to this public repository. Keep confidential details out of
every bead.

## When a skill says "publish to the issue tracker"

Create beads. A spec is an `epic`; its tickets are children of it, with
blocking edges added by `bd dep add`. Only public intake goes to GitHub Issues.

## When a skill says "fetch the relevant ticket"

`bd show <id>`. For `#<n>`, try `gh pr view <n> -R worldofgeese/pr-reviewer`
first and then `gh issue view <n> -R worldofgeese/pr-reviewer`: issues and pull
requests share one number space.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a bead of type `epic` labelled
`wayfinder:map`. Its tickets are children labelled `wayfinder:<type>`
(`research`, `prototype`, `grilling`, `task`).

- **Frontier**: `bd ready --parent <map-id>`
- **Claim**: `bd update <id> --claim`, the first write of the session
- **Resolve**: `bd close <id> --reason "<answer>"`, then
  `bd update <map-id> --append-notes "<gist and pointer>"`

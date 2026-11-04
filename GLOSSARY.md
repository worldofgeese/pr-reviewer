# pr-reviewer

pr-reviewer is a self-hosted daemon that reviews GitHub pull requests with a
local AI coding CLI and posts each review back to GitHub. This glossary fixes
the words that the code, the docs and the team use for it.

## Language

### The project

**pr-reviewer**:
This project, maintained at `worldofgeese/pr-reviewer`.
_Avoid_: the fork, our fork

**Upstream**:
The original project, `NicholaiVogel/pr-reviewer`, from which pr-reviewer was
forked.
_Avoid_: origin, parent, the original

### Repositories

**Watched repository**:
A repository whose pull requests pr-reviewer reviews.
_Avoid_: managed repo, configured repo

**Managed clone**:
The local checkout that pr-reviewer keeps of a watched repository.
_Avoid_: managed repo, local repo

### Reviews

**Review cycle**:
One review that pr-reviewer posts on the head commit of a pull request.
_Avoid_: review round, round, pass, iteration

**Finding**:
One problem that a review cycle reports about a pull request.
_Avoid_: issue, comment

**Follow-up note**:
A finding that pr-reviewer reports in the review body instead of as a review
thread, so it cannot block the merge.
_Avoid_: non-blocking note

**Stacked PR**:
A pull request whose base branch is the head branch of another open pull
request.
_Avoid_: dependent PR, chained PR

**Fork PR**:
A pull request whose head branch lives in a fork of the repository that it
targets.
_Avoid_: forked PR, external PR

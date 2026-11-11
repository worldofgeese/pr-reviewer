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
The original project (named in ADR-0001), from which pr-reviewer was
forked.
_Avoid_: origin, parent, the original

### Repositories

**Watched repository**:
A repository whose pull requests pr-reviewer reviews.
_Avoid_: managed repo, configured repo

**Managed clone**:
The local checkout that pr-reviewer keeps of a watched repository.
_Avoid_: managed repo, local repo

**Peer daemon**:
Another pr-reviewer daemon that watches the same repositories under another
GitHub account, named in `peer_logins`.
_Avoid_: other instance, second bot

### Reviews

**Review cycle**:
One review that pr-reviewer posts on the head commit of a pull request.
_Avoid_: review round, round, pass, iteration

**Head claim**:
A `pending` commit status with the context `pr-reviewer/claim` that a daemon
sets on the head commit of a pull request before a review cycle. It tells a
peer daemon to skip that head commit for 15 minutes. The daemon ends the claim
with a `success` status, which it also posts for a claim that a dead daemon left
behind.
_Avoid_: lock, review claim (that is the database entry), claim status

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

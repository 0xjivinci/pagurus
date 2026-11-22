---
name: git-workflow
description: Use git safely in a project: see what changed, commit in small steps on the agent branch, write clear messages and PR descriptions.
---

Use when the user asks to commit, review changes, prepare a pull request, or find out what changed.

## Steps
1. Look first: `git status`, `git diff` (or `git diff --stat` for many files), `git log --oneline -10`.
2. Commit one logical change at a time with the `git` tool. It commits on an `agent/<date>` branch, so the user's branch stays untouched.
3. Message: a short first line in the imperative ("Fix crash when the config is empty"), then, if useful, a blank line and why.
4. For a pull request, write the description the user can paste:
   - **What**: 1-3 bullets
   - **Why**: the problem it solves
   - **How it was tested**: commands and results
   - **Risks / follow-ups**
5. Tell the user the branch name and how to merge it (`git merge agent/…` or open a PR). Pushing is theirs to do.

## Rules
- Never commit secrets (.env, keys, tokens). Check `git diff` before committing.
- Don't rewrite history (rebase, amend, force) on branches the user shares.
- Merge conflicts: show both sides and ask, unless the right answer is obvious and tested.

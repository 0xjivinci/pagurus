---
name: coding
description: Build or change code in a project folder: read first, small edits, run the checks, commit on the agent branch, report honestly.
---

Use when the user asks to write, fix, extend or refactor code, or to build a tool or script.

## Before you start
- Work inside a project folder (project mode: `pagurus code <folder>`). Without one you can only write in the workspace: say where files really are, never a path you didn't write.
- Read the project's AGENTS.md / README and the files you'll touch. Use `grep` and `glob` to find code instead of guessing paths.
- Unclear requirement that changes the design: ask one short question. Otherwise pick a sensible default and say so.

## Steps
1. Plan in 2-5 bullets (use `todo` for longer work).
2. Make small, focused edits with `edit_file` (several `edits` at once for one change). New files: `write_file`. Keep files small and match the project's style.
3. Run `check` after every meaningful change (tests, build, lint). Read failures, fix the cause, run it again.
4. Try what you built the way the user will use it (run the script with real input). Tools meant for Telegram or cron must work without an interactive terminal: no menus or dialogs unless the user asked for them.
5. Commit with `git` (it commits on an `agent/…` branch and never pushes) with a one-line message saying what and why.
6. Report: what changed (files), how you checked it, how to run it, and anything left undone.

## Rules
- Never claim it works unless a check or a real run showed it. Say what you couldn't test.
- Don't rewrite whole files to change a few lines; don't reformat code you didn't change.
- Secrets go in environment variables or config files the user controls, never in code.
- Stuck after 3 attempts at the same error: stop, explain what you tried, and ask.

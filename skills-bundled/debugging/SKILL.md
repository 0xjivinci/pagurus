---
name: debugging
description: Find the root cause of a bug or failure before fixing it: reproduce, narrow down, fix, prove the fix.
---

Use when something is broken, crashes, gives wrong results, fails a test, or "used to work".

## Steps
1. **Reproduce.** Get the exact error: run the failing command or test yourself and read the full output. No reproduction, no fix: say so and ask for what's missing.
2. **Read the evidence.** The first error matters most; later ones are often consequences. Note file, line, input and what changed recently (`git log`, `git diff`).
3. **Form one hypothesis** that explains everything you saw. Write it down in one line.
4. **Test it cheaply**: a print, a smaller input, one variable at a time. If it's wrong, drop it and form the next one; don't stack guesses.
5. **Fix the cause**, not the symptom: the smallest change that makes the hypothesis' cause go away.
6. **Prove it**: rerun the reproduction and the related tests (`check`). Add a test that would have caught it, if the project has tests.
7. Report: cause, fix, how you verified it.

## Rules
- Never "fix" by deleting the failing test, catching and ignoring the error, or adding sleeps.
- Two failed fixes in a row means the hypothesis is wrong: go back to step 2.
- Logs and services: `journalctl -u <unit> -n 100 --no-pager`, then look for the first failure after the last start.

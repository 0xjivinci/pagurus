# How pagurus learns

[← back to the README](../README.md)

## Learning

pagurus learns on its own, like Hermes. It records what you tell it about yourself, what it discovers about your setup,
lessons from failures and corrections, and procedures that worked (as skills). It reflects every 8 turns, after a `/bad`,
and when a chat goes quiet. You govern it:

```
› how full is my disk? btw keep answers short
…
learned 2 things on its own (review with /learn)

$ pagurus learn                 # what it learned since you last looked
  a14aa2  just now  user + Prefers terse answers.
  51ce7e  just now  skill disk-check: Check free disk space on the Pi
$ pagurus learn undo 51ce7e     # exact rollback from the ledger
$ pagurus learn lock user       # it may never touch USER.md again
$ pagurus learn history         # everything, tagged self / approved / undo
```

- **Memory** is two small files with hard budgets: `USER.md` (about you, 1375 chars) and `MEMORY.md` (your
  environment, 2200 chars), in the same format as Hermes. Old entries must be replaced to make room, and a daily
  consolidation pass merges duplicates.
- **Skills** are `skills/<name>/SKILL.md` folders in the open agentskills.io format, the same one Hermes Agent uses. Each skill records whether it actually worked: the prompt marks it *trusted*, *new, untested* or *failed last
  time*. Reflection repairs failing skills, and a weekly curator merges duplicates and suggests archiving unused ones.
- **Learns on its own, with every write scanned.** Memories and skills become part of the agent's
  instructions, so each change is checked for prompt injection ("ignore previous instructions", "do not tell the user",
  "you are now…"), hidden text (invisible Unicode, hidden HTML), exfiltration (`curl … $API_KEY`, "send … to https://…",
  reading secrets files) and persistence (`authorized_keys`, editing its own config). Anything that matches is
  refused, whatever its source. Ordinary facts that merely mention these topics pass.
- **Every learned item says where it came from:** `chat`, `reflection`, `heartbeat`, `tidy`, `curator` or
  `import from …`, plus "after reading web page …" when it learned right after reading a file, page or log
  (`pagurus learn history`). Anything suspicious is one `undo` away.
- **Imported skills** go through the same scan, and `pagurus import` flags ones that download and run scripts, use sudo,
  delete recursively, decode hidden content or mention credentials.
- **Safeguards:** every change goes through one gate with budgets, a secret scan, locks, a daily cap
  (`auto_daily_limit`) and an append-only ledger with exact undo. Reflection runs with only three tools and treats
  file and command output as data, never instructions. Memories that overlap an existing entry get a hint to replace it.
- **Modes** (`[learning] mode`): `auto` (default), `propose` (you approve every change: `pagurus learn approve <id>`)
  or `off`.
- **Evals:** `pagurus eval` runs 15 scripted scenarios against *your* model in throwaway homes. They check that it learns
  a preference, ignores small talk and one-off requests, resists planted instructions, learns and replaces corrections,
  never stores secrets, respects locks, turns a routine into a skill, patches a skill instead of duplicating it, repairs a
  broken skill, keeps facts when tidying, and learns from a `/bad`.
  Add your own: `pagurus eval my-cases.json`.

## Importing from Hermes Agent

`pagurus import <dir>` reads any folder with `MEMORY.md`, `USER.md` and `skills/`, such as `~/.hermes`.

```sh
pagurus import ~/.hermes                          # dry run: shows what would come over
pagurus import ~/.hermes --apply                  # memory entries (through the learning gate, undoable)
pagurus import ~/.hermes --skills a,b --apply     # plus the skills you name (or --skills all)
```

One skill from GitHub, a URL or a folder (shown and confirmed first, threat-scanned, recorded in `pagurus learn history`
with where it came from, undoable):

```sh
pagurus skills install https://github.com/<owner>/<repo>/tree/main/skills/<name>
pagurus skills install ./my-skill --yes
```

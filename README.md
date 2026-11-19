<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/pagurus-lockup-dark.svg">
    <img src="assets/pagurus-lockup-light.svg" alt="pagurus" width="200">
  </picture>
</p>

<p align="center"><b>A small, self-improving personal agent.</b><br>
The learning loop of Hermes Agent, in one Rust binary of about 3 MB.</p>

<p align="center">
  <a href="https://github.com/0xjivinci/pagurus/releases/latest"><img src="https://img.shields.io/github/v/release/0xjivinci/pagurus?color=111&label=release" alt="latest release"></a>
  <img src="https://img.shields.io/badge/platforms-Linux%20·%20macOS%20·%20Windows%20preview-111" alt="Linux, macOS, Windows preview">
  <img src="https://img.shields.io/badge/binary-~3%20MB-111" alt="about 3 MB">
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-111" alt="MIT or Apache-2.0">
</p>

<p align="center">
  <a href="https://0xjivinci.github.io/pagurus/">Website</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#how-it-learns">How it learns</a> ·
  <a href="#documentation">Docs</a>
</p>

---

pagurus is a personal AI agent that improves with use. It remembers what you tell it, writes skills for
procedures that worked, and reflects on its own conversations. You can see, undo or lock everything it learns.
It is one file of about 3 MB that uses about 6 MB of RAM, so it can stay on all the time: on a laptop, a server
or a Raspberry Pi Zero.

It answers by running read-only commands and reading files and web pages, asks before changing anything, and
messages you in the terminal, Telegram, Discord, Slack or email when something needs attention.

Inspired by [Hermes Agent](https://github.com/NousResearch/hermes-agent) from Nous Research.

```
› why is my server slow? btw keep answers short
  · shell uptime && ps aux --sort=-%cpu | head -8
  · shell df -h / && free -m
Load 7.9 on 4 cores. `restic backup` has used 380% CPU for 40 min. Memory and disk are fine.

› tell me if the disk goes over 90%
  · heartbeat add `df --output=pcent /` > 90
Done: a local check every 10 minutes, no AI calls. One message when it crosses 90%, one when it's back.
learned 2 things on its own (review with /learn)
```

Next time it knows your backup schedule, and that you like short answers.

## Quick start

```sh
# 1. Install (Linux x86_64 / arm64 / armv7 / armv6, macOS Apple Silicon). Checks the SHA-256 before installing.
curl -fsSL https://raw.githubusercontent.com/0xjivinci/pagurus/main/install.sh | sh

# 2. Pick a model and paste a key. Free tiers and local models in LM Studio or Ollama work.
pagurus setup

# 3. Talk to it
pagurus
pagurus run "how full is my disk, and what is using the most CPU?"

# 4. Keep it running for checks, reminders and chat apps
pagurus service install
pagurus doctor        # config, keys, channels, memory use
```

Windows (preview), manual downloads, Raspberry Pi notes and building from source: [docs/install.md](docs/install.md).

## How it learns

pagurus keeps two kinds of knowledge, both plain files you can read and edit:

- **Memory:** short notes about you (`USER.md`) and your setup (`MEMORY.md`), with hard size limits. Old entries are
  replaced, not piled up.
- **Skills:** step-by-step procedures that worked, saved as `skills/<name>/SKILL.md` in the open agentskills.io format.

**When it reflects:** every 8 turns, right after you send `/bad`, and when a chat has been quiet for 20 minutes
(when it runs as a service).

```
$ pagurus learn
  a14aa2  user   + Prefers short answers.
  51ce7e  skill  + check-backup: is last night's restic run done?
$ pagurus learn undo 51ce7e     # exact rollback
$ pagurus learn lock user       # never touch USER.md again
```

**Skills earn trust.** pagurus records whether each skill actually worked. A new skill starts as *untested*, becomes
*trusted* after working at least 3 times (and at least twice as often as it failed), and is marked *failed last time*
when it breaks, so reflection repairs it. A skill pagurus wrote itself is retired after 3 failures in a row or 60 days
unused. Retiring can be undone, and skills you wrote are never retired.

**You stay in charge.** Every change is scanned for planted instructions and secrets, logged with where it came from,
and can be undone. Set `mode = "propose"` to approve each change yourself, or `off` to stop learning.
`pagurus eval` runs 15 learning scenarios against your own model. More: [docs/learning.md](docs/learning.md).

## What it does

| | |
|---|---|
| **Asks first** | Read-only commands (`ps`, `df`, `journalctl`, `cat`…) run on their own. Anything else waits for your yes. Never root. |
| **Watches** | Checks like "disk over 90%" run locally with no model and no tokens, and message you only when something changes. Reminders and daily tasks too. |
| **Talks anywhere** | Terminal, Telegram, Discord, Slack and email, from one background service. Each answers only the people you allow. |
| **Any model** | Any OpenAI-compatible API, Anthropic's own API, or a local model in LM Studio or Ollama, with fallback chains when one is busy. |
| **Reaches further** | Web search (no account needed), MCP servers, your other machines over ssh, and a throwaway Docker sandbox, all under the same rules. |
| **Big jobs** | Parallel sub-agents, a task board that keeps working in the background, and pagurus agents on your other devices that hand each other tasks. |
| **Sees** | Send it a photo, or let it look at a screenshot. Needs a model that can see images. |

## Good to know

- **You bring the model.** pagurus provides the tools, memory and schedules. The answers come from a model you
  connect: free, paid or local.
- **What leaves your machine.** Your messages, and what pagurus reads to answer them, go to your model provider.
  Memory, skills and transcripts stay local. With a local model and the terminal it runs fully offline.
- **Secrets stay out.** Files that look like secrets (`~/.ssh`, keys, tokens, `.env`) are never read. API keys live
  in `~/.pagurus/env` (mode 600), and everything sent out is redacted.
- **It writes only in its own workspace,** and saves a checkpoint before every edit (`/rollback`).
- **What it is not.** Not a coding agent, and it doesn't drive a browser or your desktop. These are left out to keep
  it small. Add what you need through MCP.
- **Maturity.** Version 0.1.0, the first release, with 141 tests. Discord, Slack, email, the task board, the swarm,
  ssh and the sandbox have seen the least real use so far. The Windows build has not been tried on a real Windows PC.

## Documentation

| Page | What's in it |
|---|---|
| [Install](docs/install.md) | Every platform, Windows, the small and full builds, building from source |
| [Talking to pagurus](docs/usage.md) | Commands, approvals, photos and voice; Telegram, Discord, Slack and email setup |
| [Tools](docs/tools.md) | Every tool, MCP servers, other machines over ssh, the Docker sandbox |
| [Models](docs/models.md) | Providers, local models, Anthropic, fallbacks, the chat / background / sub-agent roles |
| [Learning](docs/learning.md) | Memory, skills, the safety scan, learning modes, evals, importing from Hermes Agent |
| [Monitoring](docs/monitoring.md) | Checks, reminders and daily tasks in `HEARTBEAT.md` |
| [Big jobs](docs/big-jobs.md) | Long conversations, sub-agents, the task board, the swarm and its security |
| [Run it as a service](docs/service.md) | systemd and launchd, Raspberry Pi notes |
| [Configuration](docs/configuration.md) | Settings and the files in `~/.pagurus` |
| [Development](docs/development.md) | Tests, cross-builds, the Pi Zero check |
| [Crypto art](docs/art.md) | *uge build:* research Tezos art for collectors and artists, red flags, zero-token alerts |

## Credits

pagurus exists because of [Hermes Agent](https://github.com/NousResearch/hermes-agent) by Nous Research. Hermes showed
what a personal agent that learns can look like: memory about you, skills written from experience, reflection on its
own conversations. pagurus is an independent, much smaller take on that loop, written in Rust. It is not affiliated
with Nous Research. Its memory and skill files use the same format, so Hermes users can bring theirs over with
`pagurus import`.

The name comes from the genus of a small crab that carries its home and settles wherever it fits.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Contributions are
accepted under the same terms.

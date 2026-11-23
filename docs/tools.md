# Tools, MCP and other machines

[← back to the README](../README.md)

## What it can do

| Tool | Notes |
|---|---|
| `shell` | Read-only commands run directly: `ps df free uptime journalctl cat grep ip ss vcgencmd systemctl status/show/list-units …`, also joined with `\|`, `&&`, `\|\|`. **Anything else asks you first** (`/yes` on Telegram, y/N in the terminal), unless you turned on [`auto` or yolo](#approvals-the-safety-floor-and-yolo). Unattended runs refuse it. Some commands never run: see [the safety floor](#approvals-the-safety-floor-and-yolo). |
| `read_file` · `list_dir` · `write_file` | Reads anywhere **except files that look like secrets** (`~/.ssh`, keys, tokens, `.env`, its own `env` file). Writes only in `~/.pagurus/workspace`. |
| `edit_file` | Replaces one snippet in a file (exact, or ignoring whitespace/quote differences when that is still unique). Every write and edit first saves a checkpoint: `/rollback` lists the last 30 and puts a file back. |
| `process` | Long-running commands in the background (a build, `journalctl -f`): **always asks you first**, at most 3 at once, each stopped after 60 min (max 24 h). It reads their output later, and you get a message when one ends. |
| `clarify` | Asks you one short question, with choices as buttons on Telegram, and waits (10 min). Unattended runs can't ask. |
| `web_search` | Searches the web and returns titles, links and snippets; pagurus then reads the best pages and names its sources. DuckDuckGo needs no account; set `BRAVE_API_KEY` or `TAVILY_API_KEY` in the env file, or `[search] searxng_url`, for a steadier engine. |
| `web_fetch` | Reads one URL as text, including local dashboards (`http://127.0.0.1:3000`), and lists the data URLs a page's scripts load. No browser, no downloads. `tools.web = false` turns web search and fetch off. |
| `view_image` | Looks at an image file on the machine (a screenshot, a camera snapshot). Photos you send on Telegram reach the model automatically. Needs a vision model. |
| `send_file` | Sends you a photo or file in your chat app (screenshots, logs): Telegram, Discord or Slack. |
| `heartbeat` | Repeating checks, daily/weekly tasks, one-off reminders: see [Monitoring](monitoring.md). |
| `delegate` | Hands a self-contained job to a sub-agent with a fresh context. Only its report comes back. |
| `todo` | A plan for multi-step jobs that stays in view while it works. |
| `session_search` | Searches your past conversations, best matches first, each with the messages around it; by date (`7d`, `2026-06-01`) and channel. |
| `memory` · `skill_propose` · `skill_view` | Its long-term memory and skills: see [Learning](learning.md). |
| `mcp_<server>_<tool>` | Tools from MCP servers you connect: see [MCP](#mcp). Each call asks you first unless you mark the tool read-only. |

## Coding (full build)

pagurus works on one code base at a time, a **project folder**. Three ways in:

- Start plain `pagurus` inside a project (a folder with `.git`, `Cargo.toml`, `package.json`, `pyproject.toml`,
  `go.mod`, a `Makefile`, `AGENTS.md` …) and it opens it. `/project off` leaves it.
- `pagurus code ~/code/app`, or `/project ~/code/app` in a terminal chat.
- From any chat, Telegram included: "work on ~/code/app" — the `project` tool opens it **after you approve**. Never
  `/`, your home folder itself or pagurus's own folder.

In a project:

- File tools may write there, and relative paths and the shell start there. `read_file` shows line numbers.
- **Four more tools:** `grep` (ripgrep, else grep), `glob` (`src/**/*.rs`), `check` (the project's tests, build or
  lint, found from `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml` or a `Makefile`) and `git` (status, diff,
  log, show, and commits on its own `agent/…` branch; it never pushes).
- **Edits are careful:** `edit_file` takes several changes at once (all or none) and shows a diff. It refuses to edit
  a file that wasn't read, or that changed since it was read.
- **It tests its work:** an answer that changed code without running `check` is sent back once to run it.
- **The project's rules:** `AGENTS.md` (else `CLAUDE.md`, `.cursorrules` …) joins the system prompt.
- **Undo:** `/revert` puts back every file the last answer changed; `/rollback` still restores single files.
- A coding answer may take up to 60 steps and 30 minutes.
- **Approvals:** `check` runs the project's code, so it asks first. With `autonomy = "auto"` the project's usual
  test, build and lint commands run unasked, and yolo covers everything above the floor. A commit always asks, unless yolo
  is on. git runs with the repository's hooks, fsmonitor, pagers, external diff tools and credential helpers switched
  off, so a hostile repository can't run code through it.

## Browser (uge build)

For pages `web_fetch` can't read (they need JavaScript), and for clicking, filling in forms, screenshots and PDFs,
the uge build drives a real headless browser.

- **Engines:** [Lightpanda](https://github.com/lightpanda-io/browser) when it's installed: a small browser that runs
  pages without drawing them, at a fraction of Chromium's memory and CPU. Otherwise Chromium, Google Chrome, Brave
  or Edge. Lightpanda can't draw, so a **screenshot** or **pdf** briefly starts Chromium for that capture only.
  `[tools] browser = "auto"` (default), `"lightpanda"`, `"chromium"`, a path, or `"off"`. pagurus always starts
  Lightpanda with its usage telemetry switched off.
- **open** a page and get its text plus a numbered list of links, buttons and fields; **click** or **type** by number
  or by visible text; **scroll**, **back**, **read** again. Screenshots are shown to the model (if it can see images)
  and saved under `workspace/browser/`, like PDFs, so pagurus can send them to you.
- **Easy on a busy machine:** browsers run at the lowest CPU priority (`nice 19`), start on first use and close after
  3 minutes without use (and when pagurus exits), so a Pi running services and a kiosk keeps them first.
- **Safe by default:** a fresh, empty profile every time, so no access to your own browser's cookies or logins;
  only http(s) pages; cloud metadata addresses can't be reached; downloads are off. Page text counts as outside text
  (checked for prompt injection). **Typing asks you first** (site and text shown) unless yolo is on, and pagurus refuses
  to type its own keys or tokens.

Installing Lightpanda on a Raspberry Pi (64-bit OS) or Linux arm64:

```sh
mkdir -p ~/.local/bin
curl -fL -o ~/.local/bin/lightpanda https://github.com/lightpanda-io/browser/releases/latest/download/lightpanda-aarch64-linux
chmod +x ~/.local/bin/lightpanda
```

## Approvals, the safety floor and yolo

Every command pagurus wants to run is decided in one of these ways, and each decision is written to
`~/.pagurus/audit.jsonl`. `pagurus audit` (or `/audit`) shows the recent ones; `/audit all` includes read-only commands.

| How | When |
|---|---|
| **read-only** | on the allowlist (`tools.shell_allow`) and passes the read-only checks: runs directly |
| **you** · **denied** | anything else: you answer `/yes` or `/no` (y/N in the terminal). `/all` approves everything for the current message |
| **auto** | `autonomy = "auto"`: commands made only of `auto_allow` entries (and allowlisted ones) also run unasked |
| **yolo** | `/yolo 30m`: everything above the floor runs unasked, for at most 12 hours |
| **blocked** | below the safety floor: never runs, whoever approves |

**The safety floor** holds whatever you approve, including `/yes`, `/all` and yolo:

- anything as root: `sudo`, `doas`, `su`, `pkexec`
- deleting whole folders that matter: `/`, system folders, your home folder or a folder directly in it, pagurus's home
- formatting, partitioning or writing straight to disks: `mkfs`, `dd of=/dev/…`, `diskutil erase…`, `fdisk`
- shutting down or restarting the machine, killing every process, fork bombs
- piping a download into a shell (`curl … | sh`, `bash <(curl …)`): download it, read it, then run it
- uploading local files (`curl -T`, `-F file=@…`, `-d @…`, `wget --post-file`)
- changing pagurus's own settings, keys, records or service (`config.toml`, `env`, `audit.jsonl`, the learning ledger,
  the launchd/systemd service, the `pagurus` program), stopping pagurus, and running its own commands except `doctor`
- changing your logins and start-up files (`~/.ssh`, `~/.gnupg`, `.bashrc`, `.zshrc`, `.profile`), `/etc/sudoers`,
  `/etc/passwd`, and replacing or removing your crontab
- whatever you add yourself: `shell_deny = ["git push", "npm publish"]`

The floor applies on this machine, over ssh and to background processes. It does not apply inside the throwaway Docker
sandbox. It guards against mistakes, not against a determined attacker: a command can always be spelled in a way it
doesn't recognise. What it blocks, you can still run yourself.

**Yolo** lasts until it runs out, `/yolo off` or `/stop`. In a chat app it covers only the chat that turned it on (and
only the owner can), never the task board or requests from peers, and every reply shows `⚡ yolo 23m left`. In the
terminal the prompt shows `⚡23m ›`. `pagurus run --yolo[=30m] "…"` does the same for one run. Yolo can't be switched
on in `config.toml`; `tools.shell = "allowlist"` still refuses everything that isn't read-only.

Long tool output (big logs, long pages) isn't cut off: the start and end are shown and the whole text is saved under
`workspace/.tool-output/` for `read_file` to page through.

Whatever a tool returns is sent to your model provider. That's why secret files are never read, and commands that
mention them always need your approval.

## MCP

pagurus can use tools from [MCP](https://modelcontextprotocol.io) servers: local ones it starts (stdio) and remote
ones (Streamable HTTP). Add them to `config.toml`:

```toml
[[mcp]]
name = "fs"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/pi/notes"]
read_only = ["read_*", "list_*", "search_files"]   # these run without asking

[[mcp]]
name = "web"
url = "https://example.com/mcp"
bearer_env = "EXAMPLE_MCP_TOKEN"   # the env var holding the token, e.g. in ~/.pagurus/env
```

- Tools show up as `mcp_<name>_<tool>`. `allow` / `deny` (names or `*` globs) pick which ones the model sees.
- **Every call asks you first** (y/N, or `/yes` on Telegram) unless the tool is in `read_only`; unattended runs
  refuse. `approve = "writes"` also trusts the server's own read-only marking, `approve = "never"` never asks.
- A stdio server gets a clean environment: `PATH`, `HOME`, `LANG` and the like, plus only the variables you list in
  `env = ["NAME", …]`. Your API keys are not passed on.
- Servers start with the first message (up to 30 s), stay running, and stop with pagurus. One that crashed is
  restarted on its next call; one that won't start is retried later. Calls time out after `timeout_secs` (60).
- `pagurus mcp list` shows each server and its tools, `pagurus mcp test <name> [tool '{"arg":…}']` tries one.
- Not supported: OAuth sign-in (use a token), servers asking the agent's model or you for something (sampling,
  elicitation), resources and prompts.

## Other machines and a sandbox

pagurus can run commands on your other machines over `ssh`, and in a throwaway Docker container. It uses the system
`ssh` and `docker` programs (no extra code in the binary) and keeps the same rules as at home: read-only commands
run directly, anything else waits for your approval, and the approval says where it will run.

```toml
[[host]]
name = "nas"                       # the model can only pick names listed here
target = "admin@nas.local"         # or an alias from ~/.ssh/config
# port = 2222
# identity_file = "~/.ssh/id_ed25519_pagurus"   # a path, never the key itself
# shell_allow = ["uptime", "df", "zpool status"] # read-only commands for this host (default: tools.shell_allow)
# approve = "always"               # ask before every command (default "writes": read-only ones run unasked)
# host_key_check = "yes"           # default "accept-new": trust a new host once, refuse a changed key

[sandbox]
docker_image = "python:3.12-slim"  # leave out = no sandbox
network = false                    # on: the container can reach the network (then commands are asked by default)
memory = "512m"
cpus = "1"
mounts = ["/srv/data:/data"]       # extra folders, always read-only; the workspace is /workspace (read-write)
# approve = "ask"                  # default "auto" when network is off: sandboxed commands run without asking
# route_writes = true              # commands that would need approval here run in the sandbox instead
# as_root = true                   # e.g. to apt-get install (default: your user, so files stay yours)
```

Then just ask: "how full are the disks on the nas?" runs `df -h` there. `pagurus host list` shows the hosts,
`pagurus host test nas` tries the connection, and `pagurus doctor` says whether ssh and docker are usable.

- ssh runs with `BatchMode` (keys or an ssh agent; no password prompts), a connect timeout, and a shared
  connection (`ControlPersist`) so repeated commands are fast. Set up the key once yourself (`ssh-copy-id`).
- Neither ssh nor docker sees the agent's environment (your API keys); ssh gets only `SSH_AUTH_SOCK` on top of the
  basics. Output is capped and redacted like local commands, and a timeout kills the command.
- The sandbox is a fresh `docker run --rm` per command: no network by default, all capabilities dropped except file
  ownership, no privilege gain, memory/CPU/process limits, and nothing of this machine but the workspace and your
  read-only mounts. Packages installed in one command are gone in the next; keep what should last in /workspace.
- A remote command is checked as text (pagurus can't see the other machine's files), so wildcards that could reach
  hidden files and recursive searches of home directories need your approval there.

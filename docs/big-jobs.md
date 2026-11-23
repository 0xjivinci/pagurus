# Big jobs: sub-agents, the task board and the swarm

[← back to the README](../README.md)

## Long conversations and big jobs

- **Context stays small.** Old command output is trimmed first. Older messages become a running summary before they
  would leave the window, so nothing falls out unsummarised. `agent.context_chars` (≈6k tokens) bounds it, and pins
  survive it all.
- **Delegation.** A job like "check these five services' logs" goes to a sub-agent. In testing, the sub-agent read about
  6.9k tokens of logs while the main conversation stayed under 1.7k. One `delegate` call can hand out several independent
  tasks: up to `agent.delegate_parallel` (3) sub-agents run at once, each with a fresh context, and their reports come back
  together. Their commands still need your approval, asked one at a time and marked with which sub-agent asks.
- **Recall.** `session_search` looks through every past conversation, CLI and Telegram alike. The small build keeps a
  compact BM25 index next to the transcripts, updated with just the new lines before each search; the uge build uses
  SQLite FTS5 (with stemming) instead and indexes your existing transcripts on its first search. Results are ranked,
  at most two per conversation, each with a snippet around the match and the messages before and after it.
- **Limits.** 16 tool rounds and 10 minutes per message. If pagurus stops early it says so, and "continue" resumes.

## Task board and swarm

### The board

For jobs that take a while or should happen in the background, pagurus keeps a small Kanban board in
`~/.pagurus/board.json` (no database). Ask for something big and pagurus acts as the orchestrator. It splits the job
into tasks, settles shared choices (names, paths, formats) up front and writes them into every task, and orders the tasks
with dependencies. Then it stops; it doesn't do the work itself. `pagurus daemon` works through ready tasks in the
background, `board.workers` at a time (1 is right for a Pi). It messages you when a task is done, blocked or failed.

- **States:** `todo` (waiting for its parents) → `ready` → `running` → `done`, or `blocked` (it needs you), or `failed`.
  A child becomes ready once all its parents are done, and it gets their results in its brief. If a parent fails, the child is blocked.
- **Retries:** a failed run goes back to the queue with a short back-off, up to `board.max_attempts` (2) runs.
- **Crash-safe:** a running task's worker checks in every minute. A task whose worker went quiet for
  `board.claim_timeout_minutes`, or whose daemon restarted, is run again. A late result from the old run is ignored.
- **Circuit breaker:** after 3 failures in a row for the same assignee (say, a peer that's offline), its tasks pause for
  15 minutes and you're told once.
- **Workers** are sub-agents: shell, files and web, no learning. Commands that need approval are put to you on Telegram,
  never covered by `/all`. A worker that needs a decision replies `BLOCKED: …`; `/board unblock <id>` puts the task back in the queue.

```
/board                 open tasks              pagurus board [list|all|blocked|…]
/board 4               one task, with comments pagurus board show 4
/board unblock 4       back into the queue     pagurus board add Check backups -- look at /srv/backup logs
                                               pagurus board block|done|comment <id> …
```

### Swarm: pagurus on several devices

The agent on your laptop can hand work to the one on a Raspberry Pi, and the other way round. Each one runs the
task with **its own** tools, models and approvals, so a peer can never get more than that machine's owner would allow.
Commands that need approval go to the peer's own owner on Telegram, or are refused if it has none.

```toml
# on the Pi (it accepts tasks; LAN or Tailscale address, never a public one)
[peer]
listen = "192.168.1.20:7717"
url = "http://192.168.1.20:7717"
name = "pi"
```

```sh
pi$     pagurus peer pair                       # shows a one-time 6-digit code, valid 10 minutes
laptop$ pagurus peer add pi http://192.168.1.20:7717 483920
laptop$ pagurus peer list                       # who's paired and reachable (/peers on Telegram)
laptop$ pagurus peer ask pi "how full is the backup disk?"
```

After pairing, the model gets an `ask_peer` tool that runs a task on a peer and returns its report. Board tasks can be
assigned to a peer, and the board's retries and circuit breaker apply to it too. Restart `pagurus daemon` after pairing so
chats see the new peer. Set `peer.url` on both machines if each should be able to ask the other.

**Security.** The endpoint is off unless `peer.listen` is set. Pairing is an X25519 key exchange mixed with the code
(HKDF). Five wrong codes void it, and each code works once. The shared secret lives in each machine's `env` file (mode 600,
redacted like every other key). Every request and every answer is HMAC-SHA256 signed over its body, a timestamp (±2
min) and a random nonce, to block forgery, tampering and replay. Bodies are capped at 64 KB and tasks at 32 KB.
Tasks are redacted before they leave, and reports before they come back. Traffic is not encrypted, only authenticated:
use it on your LAN or over Tailscale/WireGuard. Someone actively intercepting the pairing exchange within its 10 minutes
could brute-force the 6-digit code, so pair on a network you trust.

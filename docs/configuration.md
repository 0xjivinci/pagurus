# Configuration and files

[← back to the README](../README.md)

## Configuration

Everything lives in `~/.pagurus/config.toml` (`$PAGURUS_HOME` moves the whole home). The file is commented. The settings
you're most likely to touch:

| Setting | Default | |
|---|---|---|
| `provider.model`, `[[provider.fallback]]`, `[[provider.option]]` | | models, see above |
| `agent.max_iterations` · `turn_budget_secs` | 16 · 600 | tool rounds and time per message |
| `agent.context_chars` · `max_history` · `summarize` | 24000 · 24 · true | conversation window |
| `learning.mode` · `auto_daily_limit` | auto · 12 | learning policy |
| `tools.shell` | ask | `off` · `allowlist` (never ask) · `ask` |
| `tools.read_roots` · `write_roots` · `web` | `["/"]` · `[]` · true | file and web access |
| `daemon.heartbeat_minutes` | 60 | default interval for tasks without one |
| `agent.delegate_parallel` | 3 | sub-agents one `delegate` call runs at once |
| `board.workers` · `max_attempts` · `claim_timeout_minutes` | 1 · 2 · 30 | background task board (0 workers = no board) |
| `[peer] listen` · `url` · `name` · `max_tasks` · `timeout_minutes` | off · · hostname · 1 · 15 | swarm endpoint, see [Swarm](big-jobs.md#swarm-pagurus-on-several-devices) |
| `[telegram] allowed_chat_ids` | | required: only these chats are answered |
| `[[mcp]]` | | MCP servers, see [MCP](tools.md#mcp) |
| `[voice] stt_url` · `tts_url` · `speak` | off · off · voice | voice notes: any OpenAI-compatible `/audio/transcriptions` and `/audio/speech` endpoint; keys via `stt_key_env` / `tts_key_env` |

`pagurus doctor` checks the config, keys, budgets and memory use.

## Files

```
~/.pagurus/
  config.toml   env (secrets, mode 600)   model (your /model choices)
  SOUL.md       personality and ground rules (yours to edit)
  USER.md  MEMORY.md                      long-term memory
  HEARTBEAT.md                            standing tasks
  board.json                              the task board
  peers.json                              paired pagurus agents (their keys are in env)
  skills/<name>/SKILL.md   skills/.archive/   skills/.usage.json
  sessions/*.jsonl                        transcripts (searchable)
  index/sessions.idx                      search index (small build; rebuilt from the transcripts if deleted)
  index/sessions.db                       search index (full build: SQLite FTS5)
  learn/ledger.jsonl  learn/pending/  learn/rejected/
  workspace/                              where file tools may write
  workspace/.tool-output/                 long tool output, for paging
  checkpoints/                            files as they were before pagurus wrote them (/rollback)
```

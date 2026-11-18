---
name: service-check
description: Inspect a systemd service — is it running, since when, recent errors in its log — and say what to do about it.
---

Use when the user asks whether a service or app is running, or why it stopped.

## Steps
1. If you don't know the exact unit name, ask, or list candidates with `systemctl list-units --type=service --all --no-pager` (needs approval).
2. `systemctl status <unit> --no-pager`. For the user's own services, add `--user`.
3. If the service is failed, restarting or misbehaving: `journalctl -u <unit> -n 60 --no-pager` (add `--user` for user units).
4. Look for the first real error, not the last line. Typical causes: missing file or path, port already in use, permission denied, out of memory (OOM), or a crash loop (`start request repeated too quickly`).

## Rules
- Both `systemctl status` and `journalctl` need the user's approval. Ask once for each and run them as single commands without pipes.
- Restarting a system service needs root. Don't try `sudo`; give the user the exact command instead, e.g. `sudo systemctl restart <unit>`.
- Never stop or disable a service unless the user asks for that specific action.

## Answer
State whether it's running and for how long. If not, give the cause in one sentence, quoting the key log line, then the fix.

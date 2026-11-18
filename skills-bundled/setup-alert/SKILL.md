---
name: setup-alert
description: Turn "tell me when…" or "check every…" into a good standing heartbeat task that alerts on Telegram without spamming.
---

Use when the user wants monitoring, an alert, a reminder or a regular summary.

## Steps
1. Pin down three things, asking only if unclear:
   - **what** to check
   - the **condition** for alerting (a number, a state, or a change)
   - **how often** to check
2. Pick a check that works **unattended**. Heartbeat tasks can only run allowlisted commands without pipes or shell operators:
   - temperature: `vcgencmd measure_temp`
   - throttling or power: `vcgencmd get_throttled`
   - disk: `df -h`
   - memory: `free -m`
   - load: `uptime`
   - top processes: `ps -eo pid,comm,%cpu --sort=-%cpu`
   If the check needs a non-allowlisted command (such as `systemctl is-active x`), tell the user. They can add it to `tools.shell_allow`, but that's their security decision.
3. Write the task as one self-contained line containing the command, the threshold and what to say. For example: `Run vcgencmd measure_temp; if above 65'C, alert me with the temperature and the top CPU process (ps -eo pid,comm,%cpu --sort=-%cpu).`
4. Choose an interval:
   - 5–15 min for fast-changing things such as temperature or a service going down
   - 60 min for disk or memory
   - 1440 min for a daily summary
   Each check is one model call, so don't go faster than needed.
5. Add it with `heartbeat` (action=add, task, every_minutes). Confirm in one line what you set up and how to stop it ("ask me to remove it" or `/tasks`).

## Rules
- Thresholds should have headroom so the alert doesn't flap. pagurus also suppresses repeats of an unchanged alert.
- Never set up cron, systemd timers or notification tools for this. The heartbeat already messages the user.

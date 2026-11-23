# Monitoring, reminders and checks

[← back to the README](../README.md)

## Monitoring, reminders and checks

The daemon runs standing tasks from `~/.pagurus/HEARTBEAT.md`. You normally just ask ("alert me when the Pi gets hotter
than 75 °C", "every morning at 8:30 give me a status") and pagurus writes the line:

```
- [every 10m] Look at the tunnel log for errors.        model call each run (min 5 min)
- [daily 08:30] Give me a short morning status.         every day, always reports
- [weekly mon 09:00] Check for OS updates.              every week
- [at 2026-11-16 09:00] Remind me to renew the domain.  once, then removed
- [every 1m] `vcgencmd measure_temp` > 70 Pi is hot     a CHECK: no model, no tokens
- [every 5m] `http://127.0.0.1:8080/` < 1 web app down   a CHECK that a local web service answers
```

A **check** runs a read-only command itself and compares the first number it prints. It messages you only on a
change: once when the line is crossed, once when it's back to normal, and once if the check breaks. It works offline
and costs nothing. Model-based tasks don't repeat an alert while the situation stays the same. Times are the machine's
local time.

An `http://…` check asks a web service on this machine or the home network for a page, without a shell: it prints
`1 (HTTP 200, 8 ms)` when the service answers and `0 (…)` when it's down, hung or failing, so `< 1` alerts. It can't
reach the internet, so a check can't be used to send data anywhere.

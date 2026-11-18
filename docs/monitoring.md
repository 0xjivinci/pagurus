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
```

A **check** runs a read-only command itself and compares the first number it prints. It messages you only on a
change: once when the line is crossed, once when it's back to normal, and once if the check breaks. It works offline
and costs nothing. Model-based tasks don't repeat an alert while the situation stays the same. Times are the machine's
local time.

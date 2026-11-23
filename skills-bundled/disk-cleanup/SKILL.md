---
name: disk-cleanup
description: Find what's filling the disk on a small Linux box and suggest safe ways to free space, without deleting anything yourself.
---

Use when the disk is nearly full or the user asks where the space went.

## Steps
1. `df -h` to see which filesystem is full. The SD card is usually `/`.
2. `du -xh --max-depth=1 /home` and then go one level deeper into the biggest folder. `du` is allowlisted. Use absolute paths, and run each folder as its own command.
3. Common space hogs on Linux:
   - the system journal: `journalctl --disk-usage` (needs approval)
   - the apt package cache under `/var/cache/apt/archives`
   - old logs in `/var/log`
   - browser caches in `~/.cache` (browsers grow this)
   - forgotten downloads, backups, and `node_modules` folders
   - Docker images, if Docker is used

## Rules
- **Never delete anything yourself.** List what you found with sizes, and give the user the commands to run, for example:
  - `sudo journalctl --vacuum-time=7d`
  - `sudo apt-get clean`
  - `rm -rf ~/.cache/chromium` (only if they don't mind logging in again on some sites)
- Warn before suggesting deletion of anything that looks like data, such as photos, databases or backups.

## Answer
Give the top 3–5 space users with sizes, then the safe cleanup commands with an estimate of how much each one frees.

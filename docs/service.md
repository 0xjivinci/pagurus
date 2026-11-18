# Run it as a service

[← back to the README](../README.md)

## Running it all the time (servers, homelabs, Raspberry Pi)

| Board | Release asset |
|---|---|
| Pi Zero / Zero W / Pi 1 (ARMv6) | `pagurus-linux-armv6` (static; also runs on any 32-bit Pi OS) |
| Pi 2 / 3 / 4 / 5, 32-bit OS | `pagurus-linux-armv7` |
| Pi 3 / 4 / 5, 64-bit OS | `pagurus-linux-arm64` |

The installer above picks the right one. To build everything yourself (Docker or Colima, plus a Mac for the macOS build):

```sh
scripts/build-release.sh    # → dist/pagurus-{linux-x86_64,linux-arm64,linux-armv7,linux-armv6,windows-x86_64.exe,macos-arm64} + SHA256SUMS
```

It runs the tests and keeps the small build under 3.5 MiB. It also runs the ARMv6 test suite under QEMU emulating the Pi Zero
W's **ARM1176** CPU, so no ARMv7 instruction can slip in. `.cargo/config.toml` pins the C code (ring's crypto) to
ARMv6, because cross toolchains default to ARMv7, which would crash a Zero W with `Illegal instruction`.
(`scripts/build-pi.sh` is the quick version for just the two Pi builds.)

**Starts by itself:** `pagurus setup` offers it, or run `pagurus service install` any time (`status`, `remove`).
- **Linux:** a systemd *user* service that starts at boot and restarts 15 s after a crash. Run
  `sudo loginctl enable-linger $USER` once so it runs without anyone logged in. ([contrib/pagurus.service](../contrib/pagurus.service))
- **macOS:** a launchd agent (`~/Library/LaunchAgents/ai.pagurus.daemon.plist`) that starts at login and restarts after a
  crash. Log: `~/.pagurus/daemon.log`. If you use LM Studio, set it to start at login with its server on as well.
- **Windows:** not yet; run `pagurus.exe daemon` yourself.

Memory, skills, tasks and model choices are files, so a reboot loses nothing but the current chat's short-term context.
Use one daemon per bot (Telegram, Discord or Slack): two machines with the same token will fight over its messages. It uses one thread
per chat channel, a main loop and one worker for model calls. Idle CPU is about zero. Messages that can't be sent while the network is down
are kept in `outbox.jsonl` and delivered later.

# Install

[← back to the README](../README.md)

## Install

**Linux (PC, server, Raspberry Pi) and macOS (Apple Silicon):**

```sh
curl -fsSL https://raw.githubusercontent.com/0xjivinci/pagurus/main/install.sh | sh
pagurus setup        # model + API key (hidden), Telegram, start automatically
pagurus              # chat
```

The installer picks the right build (`linux-x86_64`, `linux-arm64`, `linux-armv7`, `linux-armv6` for a Pi Zero /
Pi 1, `macos-arm64`), checks it against the published checksums and puts it in `/usr/local/bin` or `~/.local/bin`.
Prefer to do it by hand? Every build is on the [releases page](https://github.com/0xjivinci/pagurus/releases/latest).

**Windows (preview, not yet tested on real Windows):** in PowerShell:

```powershell
Invoke-WebRequest https://github.com/0xjivinci/pagurus/releases/latest/download/pagurus-windows-x86_64.exe -OutFile pagurus.exe
.\pagurus.exe setup
```

Commands run through `cmd` with a Windows read-only list (`tasklist`, `ipconfig`, `systeminfo`, …). For the full
experience, use WSL with the Linux build.

**iPhone and Android:** phones can't host an always-on agent with a shell. Run pagurus on a computer or server,
choose Telegram in `pagurus setup`, and chat with it from the Telegram app.

**From source:** `cargo build --release` (Rust 1.82+). `scripts/build-release.sh` builds every platform with Docker.

**Two builds.** *smol* (about 3 MB) has everything in these docs unless marked *uge build*. *uge* (about 5 MB, at
most 10 MiB) is for laptops and servers and adds the richer terminal chat, coding mode, a headless browser, the skills
hub, Tezos art research and SQLite FTS5 for session search. Install it with
`curl -fsSL https://raw.githubusercontent.com/0xjivinci/pagurus/main/install.sh | sh -s -- --uge` (Linux x86_64 and
arm64, Apple Silicon), download `pagurus-uge-linux-x86_64`, `pagurus-uge-linux-arm64` or `pagurus-uge-macos-arm64`
from the releases page (v0.1.0 called them `pagurus-full-…`), or build it with `cargo build --release --features uge`
(or single features: `tui`, `code`, `browser`, `hub`, `art`, `sqlite`). Both read the same `~/.pagurus`.

`pagurus setup` writes `~/.pagurus/config.toml` and keeps secrets in `~/.pagurus/env` (mode 600), never in the config.

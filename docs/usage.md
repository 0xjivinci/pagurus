# Talking to pagurus

[← back to the README](../README.md)

## Talking to it

**Terminal:** `pagurus` (or `pagurus chat`). Type normally. Commands start with `/` (`/help` lists them).

**Telegram:** `pagurus setup` connects a bot, and only the chat IDs you allow get answers. Useful commands:

| Command | What it does |
|---|---|
| `/model` | list models; `/model 2` switch chat model; `/model background 1`, `/model delegate 5` |
| `/stop` | stop the current answer (and any running command) |
| `/undo` · `/retry` | forget the last exchange · ask it again |
| `/good` · `/bad <what was wrong>` | rate the last answer (a `/bad` makes it reflect on it at once) |
| a photo (with a caption) | pagurus looks at it (needs a model that can see images) |
| a voice note | transcribed and answered (needs `[voice] stt_url`); answered by voice too with `tts_url` |
| `/rollback` · `/rollback <n>` | list file checkpoints · put back a file pagurus wrote or edited |
| `/procs` · `/procs kill <n>` | background processes it started |
| `/pin <fact>` · `/pins` · `/unpin <n>` | keep something in view for this conversation |
| `/new` | start a fresh conversation |
| `/learn` · `/undo <id>` · `/lock <memory\|user\|skill>` | see and govern what it learned |
| `/tasks` · `/skills` · `/curate` · `/tidy` | standing tasks · skills · tidy the skill library · tidy memory |
| `/health` | busy or idle, models cooling down, unsent messages, memory use |
| `/yes` · `/no` · `/all` | answer an approval request (`/all` = everything for this message) |

While pagurus is working it shows "typing…", and new messages are queued. `/stop` and the other commands still work.
When pagurus asks you a question (`clarify`), its choices appear as buttons and your next message is the answer.

### Discord, Slack and email

The same commands, approvals and steering work on Discord, Slack and email too (one daemon can serve all of them at once).
Each channel answers only its own allowlist (empty = the daemon refuses to start), only allowlisted people can `/yes`
an approval, secrets live in `~/.pagurus/env` (config.toml holds only the variable names), and everything sent passes the
secret redactor. `pagurus setup` can configure them, `pagurus doctor` shows their status. Alerts go to the first person of
the first configured channel (Telegram first).

| Channel | Set up | Talks where | Files / images |
|---|---|---|---|
| **Discord** | [Developer portal](https://discord.com/developers/applications) → New application → Bot → copy the token, turn on **MESSAGE CONTENT INTENT**; invite it with the `bot` scope (Send Messages, Attach Files, Read Message History). Your user id: Settings → Advanced → Developer Mode, then right-click yourself → Copy User ID. | DMs, and server messages that @mention the bot (or any message in `channel_ids`) | sends files (10 MB), reads images |
| **Slack** | [api.slack.com/apps](https://api.slack.com/apps) → Create app → enable **Socket Mode** (app-level token `xapp-…` with `connections:write`); bot scopes `chat:write`, `im:history`, `im:write`, `app_mentions:read`, `files:read`, `files:write`; event subscriptions `message.im`, `app_mention`; App Home → allow messages; install, copy the bot token `xoxb-…`. Your member id: profile → ⋯ → Copy member ID. | DMs, and channel messages that @mention it | sends files, reads images |
| **Email** | A mailbox for pagurus with IMAP (TLS, 993) and SMTP (465 TLS or 587 STARTTLS), and an app password. | mail from `allowed_senders` only; replies stay in the thread | plain text only |

```toml
[discord]
token_env = "PAGURUS_DISCORD_TOKEN"
allowed_user_ids = [123456789012345678]

[slack]
bot_token_env = "PAGURUS_SLACK_BOT_TOKEN"   # xoxb-…
app_token_env = "PAGURUS_SLACK_APP_TOKEN"   # xapp-…
allowed_user_ids = ["U012ABCDEF"]

[email]
imap_host = "imap.example.org"
smtp_host = "smtp.example.org"
smtp_port = 465
username = "pagurus@example.org"
password_env = "PAGURUS_EMAIL_PASSWORD"
allowed_senders = ["you@example.org"]
```

Email is careful by design: pagurus only ever fetches unseen mail *from* allowed senders (it never touches the rest of
the mailbox and never writes to anyone else, so no backscatter), ignores auto-replies, bounces and list mail, and by
default (`require_auth = true`) only takes mail your provider marked `dmarc=pass` (or aligned DKIM/SPF), so a forged
From can't drive it. HTML is never rendered: plain-text parts only, quoted history stripped. It checks every
`poll_seconds` (30), so approvals by mail need a quick `/yes` reply (2 minutes).

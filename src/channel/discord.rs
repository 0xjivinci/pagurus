//! Discord bot: the Gateway WebSocket for incoming messages (identify,
//! heartbeat, resume) and the REST API for replies, files and "typing…".
//! Only allowlisted user ids are answered: in DMs, and in servers when they
//! mention the bot (or write in one of `channel_ids`).

use super::ws::{self, Msg, Ws};
use super::{Channel, Incoming, Tls};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API: &str = "https://discord.com/api/v10";
const GATEWAY: &str = "wss://gateway.discord.gg";
/// GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT (privileged: enable it in the developer portal).
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);

#[derive(Deserialize, Clone, Debug)]
pub struct DiscordCfg {
    /// Env var holding the bot token (never the token itself).
    #[serde(default = "default_env")]
    pub token_env: String,
    /// Discord user ids allowed to talk to pagurus (and approve its commands). Empty = nobody.
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
    /// Server channels where allowlisted users needn't @mention the bot.
    #[serde(default)]
    pub channel_ids: Vec<u64>,
}

fn default_env() -> String {
    "PAGURUS_DISCORD_TOKEN".into()
}

#[derive(Default)]
struct Gateway {
    ws: Option<Ws<Tls>>,
    seq: Option<u64>,
    session: String,
    resume_url: String,
    me: String,
    interval: Duration,
    next_beat: Option<Instant>,
    acked: bool,
}

pub struct Discord {
    http: ureq::Agent,
    token: String,
    allowed: Vec<String>,
    channels: Vec<String>,
    gw: Mutex<Gateway>,
    /// user id → DM channel id, for messages pagurus starts (alerts).
    dms: Mutex<HashMap<String, String>>,
    typed: Mutex<HashMap<String, Instant>>,
}

impl Discord {
    pub fn new(c: &DiscordCfg) -> Result<Self, String> {
        let token =
            std::env::var(&c.token_env).map_err(|_| format!("discord enabled but ${} is not set", c.token_env))?;
        if c.allowed_user_ids.is_empty() || c.allowed_user_ids.contains(&0) {
            return Err("discord.allowed_user_ids is empty or contains 0 — refusing to answer strangers".into());
        }
        Ok(Self {
            http: super::http(),
            token: token.trim().to_string(),
            allowed: c.allowed_user_ids.iter().map(u64::to_string).collect(),
            channels: c.channel_ids.iter().map(u64::to_string).collect(),
            gw: Mutex::default(),
            dms: Mutex::default(),
            typed: Mutex::default(),
        })
    }

    fn redact(&self, e: impl ToString) -> String {
        crate::util::redact(&e.to_string().replace(&self.token, "***"))
    }

    fn post(&self, path: &str) -> ureq::Request {
        self.http.post(&format!("{API}{path}")).set("Authorization", &format!("Bot {}", self.token)).set(
            "User-Agent",
            concat!("DiscordBot (https://github.com/0xjivinci/pagurus, ", env!("CARGO_PKG_VERSION"), ")"),
        )
    }

    /// "@<user id>" is a person (their DM channel is opened on first use); anything else a channel id.
    fn channel(&self, to: &str) -> Result<String, String> {
        let Some(user) = to.strip_prefix('@') else { return Ok(to.to_string()) };
        if let Some(c) = self.dms.lock().ok().and_then(|m| m.get(user).cloned()) {
            return Ok(c);
        }
        let v: Value = self
            .post("/users/@me/channels")
            .send_json(json!({ "recipient_id": user }))
            .map_err(|e| self.redact(e))?
            .into_json()
            .map_err(|e| self.redact(e))?;
        let id = v["id"].as_str().ok_or("Discord didn't open a DM")?.to_string();
        if let Ok(mut m) = self.dms.lock() {
            m.insert(user.to_string(), id.clone());
        }
        Ok(id)
    }

    fn send_gw(g: &mut Gateway, v: Value) -> Result<(), String> {
        let ws = g.ws.as_mut().ok_or("not connected")?;
        ws.send_text(&v.to_string()).map_err(|e| format!("discord gateway: {e}"))
    }

    fn connect(&self, g: &mut Gateway) -> Result<(), String> {
        let resume = !g.session.is_empty() && g.seq.is_some() && !g.resume_url.is_empty();
        let base = if resume { g.resume_url.clone() } else { GATEWAY.to_string() };
        let mut w =
            ws::connect(&format!("{}/?v=10&encoding=json", base.trim_end_matches('/')), Duration::from_secs(1))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let hello = loop {
            match w.read().map_err(|e| format!("discord gateway: {e}"))? {
                Some(Msg::Text(t)) => break serde_json::from_str::<Value>(&t).unwrap_or_default(),
                Some(Msg::Close(c)) => return Err(close_reason(c)),
                None if Instant::now() > deadline => return Err("discord gateway: no hello".into()),
                None => {}
            }
        };
        let ms = hello["d"]["heartbeat_interval"]
            .as_u64()
            .filter(|_| hello["op"] == 10)
            .ok_or("discord gateway: expected hello")?;
        g.interval = Duration::from_millis(ms.max(1000));
        // first beat after interval * jitter, as Discord asks
        g.next_beat = Some(Instant::now() + g.interval.mul_f64((super::random_u64() % 1000) as f64 / 1000.0));
        g.acked = true;
        g.ws = Some(w);
        let msg = if resume {
            json!({ "op": 6, "d": { "token": self.token, "session_id": g.session, "seq": g.seq } })
        } else {
            json!({ "op": 2, "d": { "token": self.token, "intents": INTENTS,
                "properties": { "os": std::env::consts::OS, "browser": "pagurus", "device": "pagurus" } } })
        };
        Self::send_gw(g, msg)
    }
}

fn close_reason(code: u16) -> String {
    match code {
        4004 => "Discord rejected the bot token (4004): check discord.token_env".into(),
        4013 | 4014 => format!(
            "Discord refused the intents ({code}): enable MESSAGE CONTENT INTENT for the bot in the developer portal"
        ),
        c => format!("discord gateway closed ({c})"),
    }
}

/// A MESSAGE_CREATE payload → an incoming message, if it is for pagurus and
/// from an allowlisted person.
pub fn parse_message(d: &Value, me: &str, allowed: &[String], channels: &[String]) -> Option<Incoming> {
    let author = d["author"]["id"].as_str().unwrap_or("");
    if author.is_empty() || author == me || d["author"]["bot"] == true || d["webhook_id"].is_string() {
        return None;
    }
    let channel = d["channel_id"].as_str().filter(|c| !c.is_empty())?;
    let dm = d["guild_id"].is_null();
    let mentioned = !me.is_empty() && d["mentions"].as_array().into_iter().flatten().any(|m| m["id"] == me);
    if !dm && !mentioned && !channels.iter().any(|c| c == channel) {
        return None; // server chatter not addressed to pagurus
    }
    if !allowed.iter().any(|a| a == author) {
        eprintln!("pagurus: ignoring Discord message from user {author} (not in allowed_user_ids)");
        return None;
    }
    let mut text = d["content"].as_str().unwrap_or("").to_string();
    if !me.is_empty() {
        text = text.replace(&format!("<@{me}>"), "").replace(&format!("<@!{me}>"), "");
    }
    let image = d["attachments"].as_array().into_iter().flatten().find_map(|a| {
        let img = a["content_type"].as_str().is_some_and(|t| t.starts_with("image/"));
        let small = a["size"].as_u64().unwrap_or(u64::MAX) <= crate::util::MAX_IMAGE_BYTES as u64;
        (img && small).then(|| a["url"].as_str().map(String::from)).flatten()
    });
    let text = super::clip_in(text.trim());
    if text.is_empty() && image.is_none() {
        return None;
    }
    Some((channel.to_string(), text, image))
}

impl Channel for Discord {
    fn name(&self) -> &'static str {
        "dc"
    }

    fn receive(&self) -> Result<Vec<Incoming>, String> {
        let mut guard = self.gw.lock().map_err(|_| "discord: poisoned")?;
        let g = &mut *guard;
        if g.ws.is_none() {
            if let Err(e) = self.connect(g) {
                g.ws = None;
                if e.contains("4004") || e.contains("intents") {
                    eprintln!("pagurus: {e}");
                    std::thread::sleep(Duration::from_secs(600)); // fatal until fixed; don't hammer
                }
                return Err(e);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        let mut out = Vec::new();
        loop {
            if g.next_beat.is_some_and(|t| Instant::now() >= t) {
                if !g.acked {
                    g.ws = None; // zombied connection: reconnect and resume
                    return Err("discord gateway: no heartbeat ack, reconnecting".into());
                }
                Self::send_gw(g, json!({ "op": 1, "d": g.seq }))?;
                g.acked = false;
                g.next_beat = Some(Instant::now() + g.interval);
            }
            let read = g.ws.as_mut().ok_or("not connected")?.read();
            match read {
                Ok(None) if !out.is_empty() || Instant::now() >= deadline => return Ok(out),
                Ok(None) => {}
                Ok(Some(Msg::Close(code))) => {
                    g.ws = None;
                    if matches!(code, 4004 | 4007 | 4009 | 4010..=4014) {
                        g.session.clear();
                        g.seq = None;
                    }
                    return Err(close_reason(code));
                }
                Ok(Some(Msg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    match v["op"].as_u64() {
                        Some(0) => {
                            if let Some(s) = v["s"].as_u64() {
                                g.seq = Some(s);
                            }
                            let d = &v["d"];
                            match v["t"].as_str().unwrap_or("") {
                                "READY" => {
                                    g.session = d["session_id"].as_str().unwrap_or("").to_string();
                                    g.resume_url = d["resume_gateway_url"].as_str().unwrap_or("").to_string();
                                    g.me = d["user"]["id"].as_str().unwrap_or("").to_string();
                                    eprintln!(
                                        "pagurus: discord connected as {}",
                                        d["user"]["username"].as_str().unwrap_or("?")
                                    );
                                }
                                "MESSAGE_CREATE" => out.extend(parse_message(d, &g.me, &self.allowed, &self.channels)),
                                _ => {}
                            }
                        }
                        Some(1) => {
                            Self::send_gw(g, json!({ "op": 1, "d": g.seq }))?;
                        }
                        Some(7) => {
                            g.ws = None; // asked to reconnect: resume next time
                            return Ok(out);
                        }
                        Some(9) => {
                            if v["d"] != true {
                                g.session.clear();
                                g.seq = None;
                            }
                            g.ws = None;
                            std::thread::sleep(Duration::from_secs(2));
                            return Ok(out);
                        }
                        Some(11) => g.acked = true,
                        _ => {}
                    }
                }
                Err(e) => {
                    g.ws = None;
                    return Err(format!("discord gateway: {e}"));
                }
            }
        }
    }

    fn deliver(&self, to: &str, text: &str, _keys: Option<&Value>) -> bool {
        let ch = match self.channel(to) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("pagurus: discord send failed: {e}");
                return false;
            }
        };
        let mut ok = true;
        for part in super::chunks(text, 1900) {
            let body = json!({ "content": part, "allowed_mentions": { "parse": [] } });
            if let Err(e) = self.post(&format!("/channels/{ch}/messages")).send_json(body) {
                eprintln!("pagurus: discord send failed: {}", self.redact(e));
                ok = false;
            }
        }
        ok
    }

    fn upload(&self, to: &str, path: &Path, caption: &str) -> Result<(), String> {
        let ch = self.channel(to)?;
        let bytes = std::fs::read(path).map_err(|e| format!("couldn't read it: {e}"))?;
        if bytes.len() > 10 * 1024 * 1024 {
            return Err("over Discord's 10 MB upload limit".into());
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").replace(['"', '\r', '\n'], "");
        let payload =
            json!({ "content": super::chunks(caption, 1900)[0], "allowed_mentions": { "parse": [] } }).to_string();
        let (ct, body) = crate::voice::multipart(
            &[("payload_json", &payload)],
            "files[0]",
            &name,
            "application/octet-stream",
            &bytes,
        );
        self.post(&format!("/channels/{ch}/messages"))
            .timeout(Duration::from_secs(120))
            .set("Content-Type", &ct)
            .send_bytes(&body)
            .map(|_| ())
            .map_err(|e| self.redact(e))
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        if !(url.starts_with("https://cdn.discordapp.com/") || url.starts_with("https://media.discordapp.net/")) {
            return Err("not a Discord attachment".into());
        }
        let r = self.http.get(url).call().map_err(|e| self.redact(e))?;
        super::read_body(r, crate::util::MAX_IMAGE_BYTES)
    }

    fn show_typing(&self, to: &str) {
        // Discord's indicator lasts ~10 s; the daemon refreshes every 4 s
        if let Ok(mut t) = self.typed.lock() {
            if t.get(to).is_some_and(|at| at.elapsed() < Duration::from_secs(8)) {
                return;
            }
            t.insert(to.to_string(), Instant::now());
        }
        if let Ok(ch) = self.channel(to) {
            let _ = self.post(&format!("/channels/{ch}/typing")).timeout(Duration::from_secs(10)).send_bytes(&[]);
        }
    }

    fn owner(&self) -> String {
        format!("@{}", self.allowed[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_people_addressing_pagurus_get_through() {
        let allowed = vec!["111".to_string()];
        let me = "999";
        let msg = |author: &str, guild: Option<&str>, content: &str, mentions: Value| {
            json!({ "id": "1", "channel_id": "c1", "guild_id": guild, "author": { "id": author },
                    "content": content, "mentions": mentions, "attachments": [] })
        };
        assert_eq!(
            parse_message(&msg("111", None, "hi", json!([])), me, &allowed, &[]),
            Some(("c1".into(), "hi".into(), None)),
            "DM"
        );
        assert_eq!(parse_message(&msg("222", None, "hi", json!([])), me, &allowed, &[]), None, "stranger's DM");
        assert_eq!(
            parse_message(&msg("111", Some("g"), "hi all", json!([])), me, &allowed, &[]),
            None,
            "server chatter"
        );
        let m = msg("111", Some("g"), "<@999> /yes", json!([{ "id": "999" }]));
        assert_eq!(parse_message(&m, me, &allowed, &[]).unwrap().1, "/yes", "mention stripped");
        assert_eq!(
            parse_message(&msg("222", Some("g"), "<@999> rm -rf", json!([{ "id": "999" }])), me, &allowed, &[]),
            None
        );
        assert!(
            parse_message(&msg("111", Some("g"), "hi", json!([])), me, &allowed, &["c1".into()]).is_some(),
            "listed channel"
        );
        let mut bot = msg("111", None, "hi", json!([]));
        bot["author"]["bot"] = json!(true);
        assert_eq!(parse_message(&bot, me, &allowed, &[]), None, "bots are ignored");
        assert_eq!(parse_message(&msg("999", None, "echo", json!([])), me, &allowed, &[]), None, "own messages");
        let mut img = msg("111", None, "", json!([]));
        img["attachments"] =
            json!([{ "url": "https://cdn.discordapp.com/a.png", "content_type": "image/png", "size": 1000 }]);
        assert_eq!(
            parse_message(&img, me, &allowed, &[]).unwrap().2.as_deref(),
            Some("https://cdn.discordapp.com/a.png")
        );
    }

    #[test]
    fn refuses_to_start_without_an_allowlist() {
        std::env::set_var("PAGURUS_TEST_DC_TOKEN", "x");
        let c = DiscordCfg { token_env: "PAGURUS_TEST_DC_TOKEN".into(), allowed_user_ids: vec![], channel_ids: vec![] };
        assert!(Discord::new(&c).is_err());
        let c = DiscordCfg { allowed_user_ids: vec![5], ..c };
        let d = Discord::new(&c).unwrap();
        assert_eq!(d.owner(), "@5");
        assert!(d.fetch("https://evil.example/x.png").is_err(), "only Discord's CDN");
        assert!(close_reason(4014).contains("MESSAGE CONTENT"));
    }
}

//! Slack app over Socket Mode (a WebSocket, so no public URL is needed):
//! apps.connections.open, envelope acks, `message` (DMs) and `app_mention`
//! events; replies with chat.postMessage, files with the external upload flow.
//! Only allowlisted user ids are answered.

use super::ws::{self, Msg, Ws};
use super::{Channel, Incoming, Tls};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API: &str = "https://slack.com/api";

#[derive(Deserialize, Clone, Debug)]
pub struct SlackCfg {
    /// Env var with the bot token (xoxb-…): posting messages and files.
    #[serde(default = "default_bot_env")]
    pub bot_token_env: String,
    /// Env var with the app-level token (xapp-…, scope connections:write): Socket Mode.
    #[serde(default = "default_app_env")]
    pub app_token_env: String,
    /// Slack user ids (U…) allowed to talk to pagurus and approve. Empty = nobody.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
}

fn default_bot_env() -> String {
    "PAGURUS_SLACK_BOT_TOKEN".into()
}
fn default_app_env() -> String {
    "PAGURUS_SLACK_APP_TOKEN".into()
}

#[derive(Default)]
struct Socket {
    ws: Option<Ws<Tls>>,
    me: String,
    /// Recent (channel, ts): a mention in a DM can arrive as two events.
    seen: VecDeque<String>,
}

pub struct Slack {
    http: ureq::Agent,
    bot: String,
    app: String,
    allowed: Vec<String>,
    sock: Mutex<Socket>,
    /// user id → DM channel id, for messages pagurus starts.
    dms: Mutex<HashMap<String, String>>,
}

impl Slack {
    pub fn new(c: &SlackCfg) -> Result<Self, String> {
        let get = |n: &str| std::env::var(n).map(|v| v.trim().to_string()).map_err(|_| format!("slack enabled but ${n} is not set"));
        let (bot, app) = (get(&c.bot_token_env)?, get(&c.app_token_env)?);
        let allowed: Vec<String> = c.allowed_user_ids.iter().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect();
        if allowed.is_empty() {
            return Err("slack.allowed_user_ids is empty — refusing to answer strangers".into());
        }
        Ok(Self { http: super::http(), bot, app, allowed, sock: Mutex::default(), dms: Mutex::default() })
    }

    fn redact(&self, e: impl ToString) -> String {
        crate::util::redact(&e.to_string().replace(&self.bot, "***").replace(&self.app, "***"))
    }

    /// A Web API call; Slack answers HTTP 200 with `ok: false` on errors.
    fn call(&self, method: &str, token: &str, body: Value) -> Result<Value, String> {
        let v: Value = self
            .http
            .post(&format!("{API}/{method}"))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(body)
            .map_err(|e| self.redact(e))?
            .into_json()
            .map_err(|e| self.redact(e))?;
        if v["ok"] != true {
            return Err(format!("slack {method}: {}", v["error"].as_str().unwrap_or("failed")));
        }
        Ok(v)
    }

    /// A user id (U…/W…) becomes their DM channel; channel ids pass through.
    fn channel(&self, to: &str) -> Result<String, String> {
        if !(to.starts_with('U') || to.starts_with('W')) {
            return Ok(to.to_string());
        }
        if let Some(c) = self.dms.lock().ok().and_then(|m| m.get(to).cloned()) {
            return Ok(c);
        }
        let v = self.call("conversations.open", &self.bot, json!({ "users": to }))?;
        let id = v["channel"]["id"].as_str().ok_or("slack: no DM channel")?.to_string();
        if let Ok(mut m) = self.dms.lock() {
            m.insert(to.to_string(), id.clone());
        }
        Ok(id)
    }

    fn connect(&self, s: &mut Socket) -> Result<(), String> {
        if s.me.is_empty() {
            s.me = self.call("auth.test", &self.bot, json!({}))?["user_id"].as_str().unwrap_or("").to_string();
        }
        let v = self.call("apps.connections.open", &self.app, json!({}))?;
        let url = v["url"].as_str().ok_or("slack: no socket URL")?;
        s.ws = Some(ws::connect(url, Duration::from_secs(1))?);
        Ok(())
    }
}

/// Slack's escaping of &, < and > in message text.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// An Events API event → (incoming message, dedupe key), if it is for pagurus
/// and from an allowlisted person.
pub fn parse_event(e: &Value, me: &str, allowed: &[String]) -> Option<(Incoming, String)> {
    match e["type"].as_str()? {
        "message" if e["channel_type"] == "im" => {}
        "app_mention" => {}
        _ => return None,
    }
    if e["bot_id"].is_string() || e["subtype"].as_str().is_some_and(|s| s != "file_share") {
        return None; // bots (pagurus itself), edits, joins, …
    }
    let user = e["user"].as_str().unwrap_or("");
    if user.is_empty() || user == me {
        return None;
    }
    if !allowed.iter().any(|a| a == user) {
        eprintln!("pagurus: ignoring Slack message from user {user} (not in allowed_user_ids)");
        return None;
    }
    let channel = e["channel"].as_str().filter(|c| !c.is_empty())?;
    let mut text = unescape(e["text"].as_str().unwrap_or(""));
    if !me.is_empty() {
        text = text.replace(&format!("<@{me}>"), "");
    }
    let image = e["files"].as_array().into_iter().flatten().find_map(|f| {
        let img = f["mimetype"].as_str().is_some_and(|t| t.starts_with("image/"));
        let small = f["size"].as_u64().unwrap_or(u64::MAX) <= crate::util::MAX_IMAGE_BYTES as u64;
        (img && small).then(|| f["url_private_download"].as_str().map(String::from)).flatten()
    });
    let text = super::clip_in(text.trim());
    if text.is_empty() && image.is_none() {
        return None;
    }
    let key = format!("{channel}/{}", e["ts"].as_str().unwrap_or(""));
    Some(((channel.to_string(), text, image), key))
}

impl Channel for Slack {
    fn name(&self) -> &'static str {
        "sl"
    }

    fn receive(&self) -> Result<Vec<Incoming>, String> {
        let mut guard = self.sock.lock().map_err(|_| "slack: poisoned")?;
        let s = &mut *guard;
        if s.ws.is_none() {
            if let Err(e) = self.connect(s) {
                if e.contains("invalid_auth") || e.contains("not_allowed_token_type") {
                    eprintln!("pagurus: {e} (check the slack tokens)");
                    std::thread::sleep(Duration::from_secs(600));
                }
                return Err(e);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        let mut out = Vec::new();
        loop {
            let read = s.ws.as_mut().ok_or("not connected")?.read();
            match read {
                Ok(None) if !out.is_empty() || Instant::now() >= deadline => return Ok(out),
                Ok(None) => {}
                Ok(Some(Msg::Close(c))) => {
                    s.ws = None;
                    return Err(format!("slack socket closed ({c})"));
                }
                Ok(Some(Msg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    if let Some(id) = v["envelope_id"].as_str() {
                        let ack = json!({ "envelope_id": id }).to_string();
                        if let Err(e) = s.ws.as_mut().ok_or("not connected")?.send_text(&ack) {
                            s.ws = None;
                            return Err(format!("slack socket: {e}"));
                        }
                    }
                    match v["type"].as_str().unwrap_or("") {
                        "disconnect" => {
                            if let Some(mut w) = s.ws.take() {
                                w.close();
                            }
                            return Ok(out); // reconnects on the next call
                        }
                        "events_api" => {
                            if let Some((m, key)) = parse_event(&v["payload"]["event"], &s.me, &self.allowed) {
                                if !s.seen.contains(&key) {
                                    s.seen.push_back(key);
                                    if s.seen.len() > 100 {
                                        s.seen.pop_front();
                                    }
                                    out.push(m);
                                }
                            }
                        }
                        _ => {} // hello, slash commands, interactivity: not used
                    }
                }
                Err(e) => {
                    s.ws = None;
                    return Err(format!("slack socket: {e}"));
                }
            }
        }
    }

    fn deliver(&self, to: &str, text: &str, _keys: Option<&Value>) -> bool {
        let ch = match self.channel(to) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("pagurus: slack send failed: {e}");
                return false;
            }
        };
        let mut ok = true;
        for part in super::chunks(text, 3500) {
            if let Err(e) = self.call("chat.postMessage", &self.bot, json!({ "channel": ch, "text": escape(&part) })) {
                eprintln!("pagurus: slack send failed: {e}");
                ok = false;
            }
        }
        ok
    }

    fn upload(&self, to: &str, path: &Path, caption: &str) -> Result<(), String> {
        let ch = self.channel(to)?;
        let bytes = std::fs::read(path).map_err(|e| format!("couldn't read it: {e}"))?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let v: Value = self
            .http
            .post(&format!("{API}/files.getUploadURLExternal"))
            .set("Authorization", &format!("Bearer {}", self.bot))
            .send_form(&[("filename", &name), ("length", &bytes.len().to_string())])
            .map_err(|e| self.redact(e))?
            .into_json()
            .map_err(|e| self.redact(e))?;
        if v["ok"] != true {
            return Err(format!("slack upload: {}", v["error"].as_str().unwrap_or("failed")));
        }
        let (url, id) = (v["upload_url"].as_str().unwrap_or(""), v["file_id"].as_str().unwrap_or(""));
        if !url.starts_with("https://") {
            return Err("slack upload: no upload URL".into());
        }
        self.http.post(url).timeout(Duration::from_secs(120)).send_bytes(&bytes).map_err(|e| self.redact(e))?;
        let mut body = json!({ "files": [{ "id": id, "title": name }], "channel_id": ch });
        if !caption.is_empty() {
            body["initial_comment"] = json!(escape(caption));
        }
        self.call("files.completeUploadExternal", &self.bot, body).map(|_| ())
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        if !url.starts_with("https://files.slack.com/") {
            return Err("not a Slack file".into()); // the bot token goes only to Slack
        }
        let r = self.http.get(url).set("Authorization", &format!("Bearer {}", self.bot)).call().map_err(|e| self.redact(e))?;
        super::read_body(r, crate::util::MAX_IMAGE_BYTES)
    }

    fn owner(&self) -> String {
        self.allowed[0].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_from_allowlisted_people_only() {
        let allowed = vec!["U1".to_string()];
        let dm = json!({ "type": "message", "channel_type": "im", "user": "U1", "channel": "D9", "text": "a &lt; b &amp;&amp; c", "ts": "1.1" });
        let ((ch, text, img), key) = parse_event(&dm, "UBOT", &allowed).unwrap();
        assert_eq!((ch.as_str(), text.as_str(), img, key.as_str()), ("D9", "a < b && c", None, "D9/1.1"));
        let mut stranger = dm.clone();
        stranger["user"] = json!("U2");
        assert!(parse_event(&stranger, "UBOT", &allowed).is_none());
        let channel_msg = json!({ "type": "message", "channel_type": "channel", "user": "U1", "channel": "C1", "text": "hi" });
        assert!(parse_event(&channel_msg, "UBOT", &allowed).is_none(), "channel chatter needs a mention");
        let mention = json!({ "type": "app_mention", "user": "U1", "channel": "C1", "text": "<@UBOT> /yes", "ts": "2" });
        assert_eq!(parse_event(&mention, "UBOT", &allowed).unwrap().0 .1, "/yes");
        let mut bot = dm.clone();
        bot["bot_id"] = json!("B1");
        assert!(parse_event(&bot, "UBOT", &allowed).is_none(), "pagurus's own replies");
        let mut edit = dm.clone();
        edit["subtype"] = json!("message_changed");
        assert!(parse_event(&edit, "UBOT", &allowed).is_none());
        let img = json!({ "type": "message", "channel_type": "im", "subtype": "file_share", "user": "U1", "channel": "D9", "text": "",
            "files": [{ "mimetype": "image/png", "size": 10, "url_private_download": "https://files.slack.com/x.png" }] });
        assert_eq!(parse_event(&img, "UBOT", &allowed).unwrap().0 .2.as_deref(), Some("https://files.slack.com/x.png"));
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
    }

    #[test]
    fn refuses_to_start_without_an_allowlist() {
        std::env::set_var("PAGURUS_TEST_SL_BOT_TOKEN", "xoxb-1");
        std::env::set_var("PAGURUS_TEST_SL_APP_TOKEN", "xapp-1");
        let c = SlackCfg { bot_token_env: "PAGURUS_TEST_SL_BOT_TOKEN".into(), app_token_env: "PAGURUS_TEST_SL_APP_TOKEN".into(), allowed_user_ids: vec![" ".into()] };
        assert!(Slack::new(&c).is_err());
        let s = Slack::new(&SlackCfg { allowed_user_ids: vec!["U1".into()], ..c }).unwrap();
        assert!(s.fetch("https://evil.example/x").is_err(), "the token goes only to Slack");
        assert_eq!(s.owner(), "U1");
    }
}

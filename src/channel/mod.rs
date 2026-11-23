//! Chat channels (Telegram, Discord, Slack, email) behind one small trait, so
//! the daemon's logic (commands, approvals, steering, per-chat agents, outbox)
//! is the same everywhere. After Hermes' gateway platform adapters.
//!
//! Chats are channel-qualified: "tg:123", "dc:456", "sl:D0123", "mail:me@x.org".
//! The daemon keeps its `i64` chat handles: a Telegram chat's handle is its own
//! id (so sessions, approvals and tests are unchanged), other chats get a
//! handle from [`handle`], which maps them back with [`qualified`].

pub mod discord;
pub mod email;
pub mod slack;
pub mod ws;

use crate::util::{fmt_time, now, read};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One incoming message: (address within the channel, text, file reference
/// for an attached image or voice note).
pub type Incoming = (String, String, Option<String>);

/// Longest message text taken from any channel; the rest is cut.
pub const MAX_TEXT: usize = 16_000;

pub trait Channel: Send + Sync {
    /// Short prefix of this channel's chats: "tg", "dc", "sl", "mail".
    fn name(&self) -> &'static str;
    /// Block until messages arrive or ~a minute passes (then empty: the
    /// network is fine). Only allowlisted people's messages are returned.
    fn receive(&self) -> Result<Vec<Incoming>, String>;
    /// Send text (already redacted). `keys` is a Telegram-style reply markup
    /// (buttons); channels without buttons ignore it. False if it failed.
    fn deliver(&self, to: &str, text: &str, keys: Option<&Value>) -> bool;
    /// Whether `keys` show up as buttons (else choices are listed in the text).
    fn buttons(&self) -> bool {
        false
    }
    fn upload(&self, _to: &str, _path: &Path, _caption: &str) -> Result<(), String> {
        Err("this channel can't carry files".into())
    }
    /// Download a file reference from `receive`.
    fn fetch(&self, _file: &str) -> Result<Vec<u8>, String> {
        Err("this channel can't fetch files".into())
    }
    fn show_typing(&self, _to: &str) {}
    fn voice(&self, _to: &str, _ogg: &[u8]) {}
    /// Where alerts go: the first allowlisted person.
    fn owner(&self) -> String;
}

/// For the system prompt: where the user talks to pagurus.
pub fn describe(cfg: &crate::config::Config) -> String {
    let on: Vec<&str> = [
        ("Telegram", cfg.telegram.is_some()),
        ("Discord", cfg.discord.is_some()),
        ("Slack", cfg.slack.is_some()),
        ("email", cfg.email.is_some()),
    ]
    .iter()
    .filter(|(_, on)| *on)
    .map(|(n, _)| *n)
    .collect();
    if on.is_empty() {
        return String::new();
    }
    format!(" You talk to the user on {} and in the terminal (`pagurus chat`).", on.join(", "))
}

// ---------- chat handles ----------

static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Handles at or above this are interned non-Telegram chats (Telegram ids are far smaller).
const BASE: i64 = 1 << 62;

/// The daemon's handle for a qualified chat.
pub fn handle(q: &str) -> i64 {
    if let Some(id) = q.strip_prefix("tg:").and_then(|n| n.parse::<i64>().ok()).filter(|n| *n < BASE) {
        return id;
    }
    let Ok(mut v) = NAMES.lock() else { return 0 };
    let i = v.iter().position(|n| n == q).unwrap_or_else(|| {
        v.push(q.to_string());
        v.len() - 1
    });
    BASE + i as i64
}

/// The qualified name of a handle: "tg:123", "dc:456", …
pub fn qualified(h: i64) -> String {
    if h >= BASE {
        if let Some(n) = NAMES.lock().ok().and_then(|v| v.get((h - BASE) as usize).cloned()) {
            return n;
        }
    }
    format!("tg:{h}")
}

/// The channel prefix of a handle (also the session kind).
pub fn kind(h: i64) -> String {
    qualified(h).split(':').next().unwrap_or("tg").to_string()
}

// ---------- all configured channels ----------

const OUTBOX_MAX: usize = 50;

pub struct Channels {
    list: Vec<Arc<dyn Channel>>,
    /// Messages that couldn't be sent (network down); retried after each good poll.
    outbox: PathBuf,
}

impl Channels {
    pub fn new(list: Vec<Arc<dyn Channel>>, home: &Path) -> Self {
        Self { list, outbox: home.join("outbox.jsonl") }
    }

    pub fn list(&self) -> &[Arc<dyn Channel>] {
        &self.list
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.list.iter().map(|c| c.name()).collect()
    }

    fn route(&self, chat: i64) -> Option<(&Arc<dyn Channel>, String)> {
        let q = qualified(chat);
        let (name, addr) = q.split_once(':')?;
        self.list.iter().find(|c| c.name() == name).map(|c| (c, addr.to_string()))
    }

    /// Alerts go to the first channel's first allowlisted person.
    pub fn owner(&self) -> i64 {
        self.list.first().map(|c| handle(&format!("{}:{}", c.name(), c.owner()))).unwrap_or(0)
    }

    pub fn send(&self, chat: i64, text: &str) {
        if !self.markup(chat, text, None) {
            self.queue(chat, text);
        }
    }

    /// False if any part failed. Everything sent passes `redact`: chats are kept forever.
    pub fn markup(&self, chat: i64, text: &str, keys: Option<&Value>) -> bool {
        match self.route(chat) {
            Some((c, to)) => c.deliver(&to, &crate::util::redact(text), keys),
            None => {
                eprintln!("pagurus: no channel for chat {}", qualified(chat));
                true // nowhere to retry either
            }
        }
    }

    pub fn buttons(&self, chat: i64) -> bool {
        self.route(chat).is_some_and(|(c, _)| c.buttons())
    }

    pub fn send_file(&self, chat: i64, path: &Path, caption: &str) {
        if let Some((c, to)) = self.route(chat) {
            if let Err(e) = c.upload(&to, path, &crate::util::redact(caption)) {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                self.send(
                    chat,
                    &format!("(couldn't send {name}: {}; it is at {})", crate::util::one_line(&e, 120), path.display()),
                );
            }
        }
    }

    pub fn fetch(&self, chat: i64, file: &str) -> Result<Vec<u8>, String> {
        self.route(chat).ok_or("no channel for this chat")?.0.fetch(file)
    }

    pub fn typing(&self, chat: i64) {
        if let Some((c, to)) = self.route(chat) {
            c.show_typing(&to);
        }
    }

    pub fn voice(&self, chat: i64, ogg: &[u8]) {
        if let Some((c, to)) = self.route(chat) {
            c.voice(&to, ogg);
        }
    }

    fn queue(&self, chat: i64, text: &str) {
        let mut lines: Vec<String> = read(&self.outbox).lines().map(String::from).collect();
        lines.push(json!({ "to": qualified(chat), "t": now(), "text": crate::util::redact(text) }).to_string());
        let skip = lines.len().saturating_sub(OUTBOX_MAX);
        let _ = crate::util::write_atomic(&self.outbox, &(lines[skip..].join("\n") + "\n"));
    }

    pub fn outbox_len(&self) -> usize {
        read(&self.outbox).lines().filter(|l| !l.trim().is_empty()).count()
    }

    /// Deliver what failed earlier, marked with when it was meant to arrive.
    pub fn flush_outbox(&self) {
        if !self.outbox.exists() {
            return;
        }
        let mut kept = Vec::new();
        for line in read(&self.outbox).lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).unwrap_or_default();
            // older outboxes held Telegram chat ids as "chat"
            let to = v["to"]
                .as_str()
                .map(String::from)
                .or_else(|| v["chat"].as_i64().filter(|c| *c != 0).map(|c| format!("tg:{c}")));
            let (t, text) = (v["t"].as_u64().unwrap_or(0), v["text"].as_str().unwrap_or(""));
            let Some(to) = to else { continue };
            if self.markup(handle(&to), &format!("(sent late, from {})\n{text}", fmt_time(t)), None) {
                continue;
            }
            kept.push(line.to_string());
        }
        if kept.is_empty() {
            let _ = std::fs::remove_file(&self.outbox);
        } else {
            let _ = crate::util::write_atomic(&self.outbox, &(kept.join("\n") + "\n"));
        }
    }
}

// ---------- shared helpers ----------

/// Split text into parts of at most `max` characters, preferring line breaks.
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        let cut = rest.char_indices().nth(max).map(|(i, _)| i).unwrap_or(rest.len());
        let at = rest[..cut].rfind('\n').filter(|i| *i > cut / 2).map(|i| i + 1).unwrap_or(cut);
        out.push(rest[..at].to_string());
        rest = &rest[at..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// Keep at most `MAX_TEXT` characters of an incoming message.
pub fn clip_in(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}\n[message cut]", &s[..i]),
        None => s.to_string(),
    }
}

/// Read at most `max` bytes of an HTTP response body.
pub fn read_body(r: ureq::Response, max: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    r.into_reader().take(max as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > max {
        return Err(format!("file is over {} KB", max / 1024));
    }
    Ok(buf)
}

pub fn http() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout(Duration::from_secs(60)).build()
}

/// Pseudo-random bytes for WebSocket keys/masks and Message-IDs (not secrets).
pub fn random_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);
    let t =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut z = N.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ t ^ ((std::process::id() as u64) << 32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------- TLS over a plain socket (WebSocket, IMAP, SMTP) ----------

pub trait Stream: std::io::Read + std::io::Write + Send {}
impl<T: std::io::Read + std::io::Write + Send> Stream for T {}

pub type Tls<S = std::net::TcpStream> = rustls::StreamOwned<rustls::ClientConnection, S>;

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONF: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONF.get_or_init(|| {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let c = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("ring supports TLS 1.2 and 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(c)
    })
    .clone()
}

pub fn tcp(host: &str, port: u16, timeout: Duration) -> Result<std::net::TcpStream, String> {
    use std::net::ToSocketAddrs;
    let mut last = format!("{host}: no address");
    for a in (host, port).to_socket_addrs().map_err(|e| format!("{host}: {e}"))? {
        match std::net::TcpStream::connect_timeout(&a, Duration::from_secs(15)) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
                return Ok(s);
            }
            Err(e) => last = format!("{host}:{port}: {e}"),
        }
    }
    Err(last)
}

/// TLS on top of an open stream, certificate checked against `host`.
pub fn tls<S: Stream>(mut s: S, host: &str) -> Result<Tls<S>, String> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: {e}"))?;
    let mut conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
    while conn.is_handshaking() {
        conn.complete_io(&mut s).map_err(|e| format!("TLS with {host}: {e}"))?;
    }
    Ok(rustls::StreamOwned::new(conn, s))
}

pub fn connect_tls(host: &str, port: u16, timeout: Duration) -> Result<Tls, String> {
    tls(tcp(host, port, timeout)?, host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_are_channel_qualified() {
        assert_eq!(handle("tg:123"), 123, "Telegram keeps its ids");
        assert_eq!(handle("tg:-100500"), -100500);
        let d = handle("dc:999");
        assert!(d >= BASE);
        assert_eq!(handle("dc:999"), d, "same chat, same handle");
        assert_ne!(handle("sl:D1"), d);
        assert_eq!(qualified(d), "dc:999");
        assert_eq!(qualified(42), "tg:42");
        assert_eq!(kind(handle("mail:a@b.c")), "mail");
        assert_eq!(kind(7), "tg");
    }

    #[test]
    fn chunking_and_clipping() {
        assert_eq!(chunks("hello", 10), vec!["hello"]);
        assert_eq!(chunks("", 10), vec![""]);
        let parts = chunks("aaaa\nbbbb\ncccc", 10);
        assert_eq!(parts, vec!["aaaa\nbbbb\n", "cccc"]);
        assert_eq!(chunks("ééééé", 2).len(), 3);
        assert!(clip_in(&"x".repeat(MAX_TEXT + 5)).ends_with("[message cut]"));
    }

    struct Fake(Mutex<Vec<String>>, bool);
    impl Channel for Fake {
        fn name(&self) -> &'static str {
            "fk"
        }
        fn receive(&self) -> Result<Vec<Incoming>, String> {
            Ok(vec![])
        }
        fn deliver(&self, to: &str, text: &str, _: Option<&Value>) -> bool {
            self.0.lock().unwrap().push(format!("{to}|{text}"));
            self.1
        }
        fn owner(&self) -> String {
            "boss".into()
        }
    }

    #[test]
    fn hub_routes_redacts_and_queues() {
        let home = std::env::temp_dir().join(format!("pagurus-ch-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let down = Arc::new(Fake(Mutex::new(vec![]), false));
        let hub = Channels::new(vec![down.clone()], &home);
        let owner = hub.owner();
        assert_eq!(qualified(owner), "fk:boss");
        hub.send(owner, concat!("key gh", "p_abcdefghijklmnopqrstuvwxyz0123456789"));
        assert_eq!(down.0.lock().unwrap()[0], "boss|key [REDACTED]");
        assert_eq!(hub.outbox_len(), 1, "failed sends wait in the outbox");
        assert!(!read(&home.join("outbox.jsonl")).contains("ghp_"));
        let up = Arc::new(Fake(Mutex::new(vec![]), true));
        let hub = Channels::new(vec![up.clone()], &home);
        hub.flush_outbox();
        assert_eq!(hub.outbox_len(), 0);
        assert!(up.0.lock().unwrap()[0].starts_with("boss|(sent late"));
        let _ = std::fs::remove_dir_all(&home);
    }
}

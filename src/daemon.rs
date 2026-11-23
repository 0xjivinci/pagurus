//! `pagurus daemon`. Channel threads (Telegram, Discord, Slack, email) feed
//! messages to the main thread, which answers commands
//! (/stop, /learn, approvals …) and schedules heartbeats; one worker thread
//! runs everything slow (model turns, heartbeats, reflection, tidying) one job
//! at a time. So pagurus stays responsive while it thinks, and `/stop` works.

use crate::agent::Agent;
use crate::config::{Config, TelegramCfg};
use crate::channel::{self, Channels};
use crate::learn;
use crate::tasks;
use crate::util::{fmt_time, now, read};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

const APPROVAL_SECS: u64 = 120;

struct Telegram {
    http: ureq::Agent,
    token: String,
    allowed: Vec<i64>,
    offset: AtomicI64,
}

/// Whether to answer a Telegram message: its chat must be allowed, and in a
/// group the sender must be allowed too. Otherwise every member of an allowed
/// group (or an anonymous admin, or another bot) could drive pagurus, read files
/// through it and approve its commands with /yes.
fn accepted(allowed: &[i64], m: &Value) -> bool {
    let chat = m["chat"]["id"].as_i64().unwrap_or(0);
    if chat == 0 || !allowed.contains(&chat) {
        return false;
    }
    let private = m["chat"]["type"].as_str().map_or(chat > 0, |t| t == "private");
    private || (m["from"]["is_bot"] != true && m["from"]["id"].as_i64().is_some_and(|u| u != 0 && allowed.contains(&u)))
}

#[cfg(test)]
mod auth_tests {
    use serde_json::json;
    #[test]
    fn only_allowed_people_reach_pagurus() {
        let allowed = [111, -100500];
        let msg = |chat: i64, kind: &str, from: i64| json!({"chat": {"id": chat, "type": kind}, "from": {"id": from}, "text": "hi"});
        assert!(super::accepted(&allowed, &msg(111, "private", 111)));
        assert!(!super::accepted(&allowed, &msg(222, "private", 222)), "stranger");
        assert!(super::accepted(&allowed, &msg(-100500, "supergroup", 111)), "owner in an allowed group");
        assert!(!super::accepted(&allowed, &msg(-100500, "supergroup", 333)), "other group member");
        assert!(!super::accepted(&allowed, &json!({"chat": {"id": -100500, "type": "group"}, "sender_chat": {"id": -100500}})), "anonymous admin");
        assert!(!super::accepted(&[0], &json!({"text": "no chat"})));
        let bot = json!({"chat": {"id": -100500, "type": "group"}, "from": {"id": 111, "is_bot": true}});
        assert!(!super::accepted(&allowed, &bot));
    }
}

impl Telegram {
    fn new(c: &TelegramCfg) -> Result<Self, String> {
        let token = std::env::var(&c.token_env).map_err(|_| format!("telegram enabled but ${} is not set", c.token_env))?;
        if c.allowed_chat_ids.is_empty() || c.allowed_chat_ids.contains(&0) {
            return Err("telegram.allowed_chat_ids is empty or contains 0 — refusing to answer strangers".into());
        }
        let http = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).build();
        Ok(Self {
            http,
            token,
            allowed: c.allowed_chat_ids.clone(),
            offset: AtomicI64::new(0),
        })
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.token)
    }

    /// ureq errors include the URL, which includes the token. Never log that.
    fn redact(&self, e: impl ToString) -> String {
        crate::util::redact(&e.to_string().replace(&self.token, "***"))
    }

    /// New messages: (chat, text or caption, file id of an attached image).
    fn poll(&self, wait: u64) -> Result<Vec<(i64, String, Option<String>)>, String> {
        let url =
            format!("{}?timeout={wait}&offset={}&allowed_updates=%5B%22message%22%5D", self.url("getUpdates"), self.offset.load(Ordering::Relaxed));
        let v: Value = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(wait + 20))
            .call()
            .map_err(|e| self.redact(e))?
            .into_json()
            .map_err(|e| self.redact(e))?;
        let mut out = Vec::new();
        for u in v["result"].as_array().into_iter().flatten() {
            self.offset.fetch_max(u["update_id"].as_i64().unwrap_or(0) + 1, Ordering::Relaxed);
            let m = &u["message"];
            let chat = m["chat"]["id"].as_i64().unwrap_or(0);
            let image = best_photo(m).or_else(|| crate::voice::voice_id(m)); // voice notes: tagged file id
            let text = m["text"].as_str().or_else(|| m["caption"].as_str()).unwrap_or("");
            if text.is_empty() && image.is_none() {
                continue; // stickers, … aren't supported
            }
            if accepted(&self.allowed, m) {
                out.push((chat, text.to_string(), image));
            } else {
                let from = m["from"]["id"].as_i64().unwrap_or(0);
                eprintln!("pagurus: ignoring message from chat {chat} / user {from} (not in allowed_chat_ids)");
            }
        }
        Ok(out)
    }

    /// Download a file the user sent (by Telegram file id).
    fn download(&self, file_id: &str) -> Result<Vec<u8>, String> {
        let v: Value = self
            .http
            .get(&format!("{}?file_id={file_id}", self.url("getFile")))
            .timeout(Duration::from_secs(30))
            .call()
            .map_err(|e| self.redact(e))?
            .into_json()
            .map_err(|e| self.redact(e))?;
        let path = v["result"]["file_path"].as_str().ok_or("Telegram didn't return the file")?;
        let resp = self
            .http
            .get(&format!("https://api.telegram.org/file/bot{}/{path}", self.token))
            .timeout(Duration::from_secs(60))
            .call()
            .map_err(|e| self.redact(e))?;
        use std::io::Read;
        let mut buf = Vec::new();
        Read::take(resp.into_reader(), crate::util::MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| self.redact(e))?;
        Ok(buf)
    }

    /// False if any part failed to send.
    fn send_markup(&self, chat: i64, text: &str, markup: Option<Value>) -> bool {
        // replies, errors and approval prompts can quote a key; Telegram keeps chats forever
        let chars: Vec<char> = crate::util::redact(text).chars().collect();
        let parts: Vec<&[char]> = chars.chunks(3900).collect();
        let mut ok = true;
        for (i, part) in parts.iter().enumerate() {
            let mut body = json!({ "chat_id": chat, "text": part.iter().collect::<String>() });
            if i + 1 == parts.len() {
                if let Some(m) = &markup {
                    body["reply_markup"] = m.clone();
                }
            }
            if let Err(e) = self.http.post(&self.url("sendMessage")).timeout(Duration::from_secs(30)).send_json(body) {
                eprintln!("pagurus: telegram send failed: {}", self.redact(e));
                ok = false;
            }
        }
        ok
    }

    /// Upload a file: images as a photo, anything else as a document.
    fn send_file(&self, chat: i64, path: &Path, caption: &str) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        let photo = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp")
            && std::fs::metadata(path).is_ok_and(|m| m.len() <= 10 * 1024 * 1024);
        let (method, field, mime) = match (photo, ext.as_str()) {
            (true, "png") => ("sendPhoto", "photo", "image/png"),
            (true, "webp") => ("sendPhoto", "photo", "image/webp"),
            (true, _) => ("sendPhoto", "photo", "image/jpeg"),
            _ => ("sendDocument", "document", "application/octet-stream"),
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.send_markup(chat, &format!("(couldn't read {}: {e})", path.display()), None);
                return;
            }
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").replace('"', "");
        let boundary = format!("pagurus{}", crate::util::new_id());
        let mut body = Vec::with_capacity(bytes.len() + 512);
        let mut field_text = |k: &str, v: &str| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        };
        field_text("chat_id", &chat.to_string());
        if !caption.is_empty() {
            field_text("caption", caption);
        }
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let res = self
            .http
            .post(&self.url(method))
            .timeout(Duration::from_secs(120))
            .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .send_bytes(&body);
        if let Err(e) = res {
            let e = self.redact(e);
            eprintln!("pagurus: telegram upload failed: {e}");
            self.send_markup(chat, &format!("(couldn't send {name}: {})", crate::util::one_line(&e, 120)), None);
        }
    }

    /// A spoken reply (OGG/Opus) as a Telegram voice note.
    fn send_voice(&self, chat: i64, ogg: &[u8]) {
        let (ct, body) = crate::voice::multipart(&[("chat_id", &chat.to_string())], "voice", "reply.ogg", "audio/ogg", ogg);
        if let Err(e) = self.http.post(&self.url("sendVoice")).timeout(Duration::from_secs(60)).set("Content-Type", &ct).send_bytes(&body) {
            eprintln!("pagurus: telegram voice upload failed: {}", self.redact(e));
        }
    }

    fn typing(&self, chat: i64) {
        let _ = self
            .http
            .post(&self.url("sendChatAction"))
            .timeout(Duration::from_secs(10))
            .send_json(json!({ "chat_id": chat, "action": "typing" }));
    }
}

impl channel::Channel for Telegram {
    fn name(&self) -> &'static str {
        "tg"
    }

    fn receive(&self) -> Result<Vec<channel::Incoming>, String> {
        Ok(self.poll(50)?.into_iter().map(|(chat, text, file)| (chat.to_string(), text, file)).collect())
    }

    fn deliver(&self, to: &str, text: &str, keys: Option<&Value>) -> bool {
        to.parse().is_ok_and(|chat| self.send_markup(chat, text, keys.cloned()))
    }

    fn buttons(&self) -> bool {
        true
    }

    fn upload(&self, to: &str, path: &Path, caption: &str) -> Result<(), String> {
        self.send_file(to.parse().map_err(|_| "bad chat id")?, path, caption);
        Ok(()) // errors are reported in the chat
    }

    fn fetch(&self, file: &str) -> Result<Vec<u8>, String> {
        self.download(file)
    }

    fn show_typing(&self, to: &str) {
        if let Ok(chat) = to.parse() {
            self.typing(chat);
        }
    }

    fn voice(&self, to: &str, ogg: &[u8]) {
        if let Ok(chat) = to.parse() {
            self.send_voice(chat, ogg);
        }
    }

    fn owner(&self) -> String {
        self.allowed[0].to_string()
    }
}

/// Build every configured channel; any misconfigured one stops the daemon.
fn channels(cfg: &Config, home: &Path) -> Result<Option<Arc<Channels>>, String> {
    let mut list: Vec<Arc<dyn channel::Channel>> = Vec::new();
    if let Some(c) = &cfg.telegram {
        list.push(Arc::new(Telegram::new(c)?));
    }
    if let Some(c) = &cfg.discord {
        list.push(Arc::new(channel::discord::Discord::new(c)?));
    }
    if let Some(c) = &cfg.slack {
        list.push(Arc::new(channel::slack::Slack::new(c)?));
    }
    if let Some(c) = &cfg.email {
        list.push(Arc::new(channel::email::Email::new(c)?));
    }
    Ok((!list.is_empty()).then(|| Arc::new(Channels::new(list, home))))
}

pub fn heartbeat_tasks(home: &Path) -> Vec<String> {
    tasks::list(home).into_iter().map(|t| t.text).collect()
}

/// Run the due tasks once. `previous` is the last alert sent for this same
/// set of tasks, so an unchanged situation doesn't page the user every check.
fn heartbeat(home: &Path, cfg: &Config, due: &[String], previous: Option<&(u64, String)>) -> Option<String> {
    let mut prompt = format!(
        "Heartbeat check at {}. Work through these standing tasks. If nothing needs the user's attention, reply exactly \
         HEARTBEAT_OK. Otherwise reply with a short message for the user (it is sent to them as is).\n\n- {}",
        fmt_time(now()),
        due.join("\n- ")
    );
    if let Some((t, msg)) = previous {
        prompt += &format!(
            "\n\nYou last alerted the user about these checks at {}:\n\"{}\"\nIf the situation is essentially the same, reply \
             HEARTBEAT_OK instead of repeating it. Alert again if it got worse or changed.",
            fmt_time(*t),
            msg
        );
    }
    let mut a = match Agent::new(home.to_path_buf(), cfg.clone(), false, "heartbeat") {
        Ok(a) => a,
        Err(e) => return Some(format!("heartbeat could not start: {e}")),
    };
    match a.turn(&prompt) {
        Ok(t) if t.trim().starts_with("HEARTBEAT_OK") => None,
        Ok(t) => Some(t),
        Err(e) => Some(format!("heartbeat failed: {e}")),
    }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        || std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
}

/// One daemon per home: two would fight over the Telegram long-poll (HTTP 409)
/// and run every heartbeat twice.
fn take_lock(home: &Path) -> Result<(), String> {
    let lock = home.join("daemon.pid");
    if let Ok(pid) = read(&lock).trim().parse::<u32>() {
        if pid != std::process::id() && process_alive(pid) {
            return Err(format!("another pagurus daemon is already running for this home (pid {pid})"));
        }
    }
    crate::util::write_atomic(&lock, &std::process::id().to_string())
}


/// The photo size to send to a model: Telegram keeps several, so pick the largest
/// whose long side is at most 1600 px (enough detail, a fraction of the tokens).
/// Images sent "as file" (documents) count too.
fn best_photo(m: &Value) -> Option<String> {
    if let Some(sizes) = m["photo"].as_array() {
        let fits = |p: &&Value| p["width"].as_u64().unwrap_or(0).max(p["height"].as_u64().unwrap_or(0)) <= 1600;
        let pick = sizes.iter().rfind(fits).or_else(|| sizes.first())?;
        return pick["file_id"].as_str().map(String::from);
    }
    let d = &m["document"];
    let image = d["mime_type"].as_str().is_some_and(|t| matches!(t, "image/png" | "image/jpeg" | "image/webp" | "image/gif"));
    let small = d["file_size"].as_u64().unwrap_or(u64::MAX) <= crate::util::MAX_IMAGE_BYTES as u64;
    (image && small).then(|| d["file_id"].as_str().map(String::from)).flatten()
}

// ---------- worker: everything slow runs here, one job at a time ----------

enum Job {
    Turn { chat: i64, agent: Box<Agent>, text: String, images: Vec<String> },
    Review { chat: i64, agent: Box<Agent>, keep: bool },
    Heartbeat { key: String, due: Vec<String>, once: Vec<String>, previous: Option<(u64, String)> },
    Tidy { chat: Option<i64>, force: bool },
    Curate { chat: Option<i64> },
    /// Threshold checks: (task text, check, previous state). No model involved.
    Checks { items: Vec<(String, tasks::Check, u8)> },
    /// The user switched models.
    Reconfigure(Box<Config>),
}

/// Check states: never run, fine, over the line, couldn't run or read.
const UNKNOWN: u8 = 0;
const FINE: u8 = 1;
const TRIPPED: u8 = 2;
const BROKEN: u8 = 3;

enum Event {
    /// The answer, sent as soon as it exists (reflection runs after it).
    Reply { chat: i64, text: String },
    /// The turn is fully done (reflection included); `note` says what was learned.
    TurnDone { chat: i64, agent: Box<Agent>, note: String },
    /// From the Telegram thread: new messages, or an empty poll (network fine).
    Messages(Vec<(i64, String, Option<String>)>),
    Polled,
    Reviewed { chat: i64, agent: Option<Box<Agent>>, note: Option<String> },
    Heartbeat { key: String, once: Vec<String>, msg: Option<String> },
    Tidied { chat: Option<i64>, result: Result<String, String> },
    /// (task text, new state, message for the user if the state changed)
    Checked { results: Vec<(String, u8, Option<String>)> },
    /// The running turn wants a command approved; answer on the sender.
    Approval { chat: i64, cmd: String, answer: Sender<bool> },
    /// The running turn asks the user a question (`clarify`); the answer goes through `ask::answer`.
    Ask { chat: i64, text: String, options: Vec<String> },
    /// Background work (board tasks, peer requests): tell the owner.
    Notice(String),
    /// Background work wants a command approved by the owner (never covered by /all).
    BgApproval { cmd: String, answer: Sender<bool> },
}

fn worker(home: PathBuf, mut cfg: Config, jobs: Receiver<Job>, events: Sender<Event>) {
    for job in jobs {
        crate::util::CANCEL.store(false, Ordering::Relaxed);
        let ev = match job {
            Job::Turn { chat, mut agent, text, images } => {
                let t0 = now();
                let tx = events.clone();
                let ask = move |cmd: &str| {
                    let (answer, rx) = channel();
                    if tx.send(Event::Approval { chat, cmd: cmd.to_string(), answer }).is_err() {
                        return false;
                    }
                    rx.recv_timeout(Duration::from_secs(APPROVAL_SECS + 30)).unwrap_or(false)
                };
                let tx = events.clone();
                crate::ask::set_asker(Some(Box::new(move |q: &str, options: &[String]| {
                    let rx = crate::ask::open(chat);
                    tx.send(Event::Ask { chat, text: q.to_string(), options: options.to_vec() }).ok()?;
                    crate::ask::wait(chat, rx).map(|a| crate::ask::pick(&a, options))
                })));
                let mut reply = agent.turn_full(&text, images, Some(&ask)).unwrap_or_else(|e| format!("error: {e}"));
                crate::ask::set_asker(None);
                if let Some(n) = learn::activity_note(&home, t0) {
                    reply += &format!("\n\n{n} (/learn)");
                }
                // answer first; reflection (several model calls) happens after the user has it
                let _ = events.send(Event::Reply { chat, text: reply });
                let t1 = now();
                if !crate::util::cancelled() {
                    agent.after_turn();
                }
                let note = learn::activity_note(&home, t1).map(|n| format!("{n} (/learn)")).unwrap_or_default();
                Event::TurnDone { chat, agent, note }
            }
            Job::Review { chat, mut agent, keep } => {
                let t0 = now();
                agent.review();
                Event::Reviewed { chat, agent: keep.then_some(agent), note: learn::activity_note(&home, t0) }
            }
            Job::Heartbeat { key, due, once, previous } => {
                Event::Heartbeat { key, once, msg: heartbeat(&home, &cfg.background(), &due, previous.as_ref()) }
            }
            Job::Tidy { chat, force } => {
                // the daily housekeeping also retires self-made skills that keep failing (no model call; in the digest)
                if !force && cfg.learning.mode != "off" {
                    for r in crate::skills::auto_retire(&home) {
                        eprintln!("pagurus: retired skill {r}");
                    }
                }
                Event::Tidied { chat, result: crate::agent::consolidate(&home, &cfg, force) }
            }
            Job::Checks { items } => Event::Checked {
                results: items.into_iter().map(|(text, c, before)| run_check(&home, &cfg, text, &c, before)).collect(),
            },
            Job::Curate { chat } => Event::Tidied {
                chat,
                result: crate::agent::curate(&home, &cfg).map(|r| format!("📚 skills: {r}")),
            },
            Job::Reconfigure(c) => {
                cfg = *c;
                continue;
            }
        };
        if events.send(ev).is_err() {
            break;
        }
        // a turn's temporary memory (pages, images, search results) goes back to the system
        crate::util::release_memory();
    }
}

/// Run one check and decide whether the user needs to hear about it: only on a
/// change of state, so a hot Pi pages once, not every minute.
fn run_check(home: &Path, cfg: &Config, text: String, c: &tasks::Check, before: u8) -> (String, u8, Option<String>) {
    let name = if c.label.is_empty() { format!("`{}`", c.cmd) } else { c.label.clone() };
    let (state, msg) = match crate::tools::run_check(home, cfg, &c.cmd) {
        Ok(out) => match tasks::first_number(&out) {
            Some(v) if c.tripped(v) => (TRIPPED, format!("⚠️ {name}: {v} (alert when {} {})", c.op, c.limit)),
            // still near the line after an alert: stay alerted, say nothing
            Some(_) if before == TRIPPED && !c.recovered(tasks::first_number(&out).unwrap_or(0.0)) => (TRIPPED, String::new()),
            Some(v) => (FINE, format!("✅ back to normal — {name}: {v}")),
            None => (BROKEN, format!("⚠️ check {name} printed no number: {}", crate::util::one_line(&out, 160))),
        },
        Err(e) => (BROKEN, format!("⚠️ check {name} couldn't run: {}", crate::util::one_line(&e, 160))),
    };
    let tell = match (before, state) {
        (b, s) if b == s => false,
        (_, TRIPPED) | (_, BROKEN) => true,
        (TRIPPED, FINE) => true,
        _ => false, // first run fine, or a broken check working again
    };
    (text, state, tell.then_some(msg))
}

// ---------- main thread ----------

struct Chat {
    /// None while the worker has it (a turn or reflection is running).
    agent: Option<Box<Agent>>,
    last: u64,
    /// Messages that arrived while busy, run in order afterwards.
    queue: VecDeque<(String, Vec<String>)>,
    /// A turn (not a reflection) is running: show "typing…".
    typing: bool,
    /// The running turn's steering inbox: messages sent meanwhile join it
    /// after its current tool round.
    inbox: crate::agent::Inbox,
}

struct Pending {
    chat: i64,
    cmd: String,
    answer: Sender<bool>,
    deadline: u64,
}

struct Daemon {
    home: PathBuf,
    cfg: Config,
    ch: Option<Arc<Channels>>,
    jobs: Sender<Job>,
    chats: HashMap<i64, Chat>,
    pending: Option<Pending>,
    /// Approval requests waiting for the one on screen to be answered.
    waiting: VecDeque<(i64, String, Sender<bool>)>,
    /// The user answered /all: approve this chat's commands until its turn ends.
    trusted: Option<i64>,
    /// `/yolo`: this chat's commands run without asking until then (never background work).
    yolo: Option<(i64, u64)>,
    inflight: usize,
    heartbeat_running: bool,
    checks_running: bool,
    check_state: HashMap<String, u8>,
    last_alert: HashMap<String, (u64, String)>,
    started: u64,
}

impl Daemon {
    fn say(&self, chat: i64, text: &str) {
        match &self.ch {
            Some(c) => c.send(chat, text),
            None => println!("[{}] {text}", fmt_time(now())),
        }
    }

    fn owner(&self) -> i64 {
        self.ch.as_ref().map(|c| c.owner()).unwrap_or(0)
    }

    /// Put a command to the user for approval (buttons where the channel has them).
    fn ask(&self, ch: &Channels, chat: i64, cmd: &str) {
        let host = read(Path::new("/etc/hostname")).trim().to_string();
        let keyboard = json!({ "keyboard": [[{ "text": "/yes" }, { "text": "/no" }, { "text": "/all" }]], "one_time_keyboard": true, "resize_keyboard": true });
        ch.markup(
            chat,
            &format!(
                "🔐 pagurus wants to run on {}:\n\n{cmd}\n\nApprove? /yes · /no · /all = everything for this message ({} min, then it's a no)",
                if host.is_empty() { "this machine" } else { &host },
                APPROVAL_SECS / 60
            ),
            Some(&keyboard),
        );
    }

    fn submit(&mut self, job: Job) {
        if self.jobs.send(job).is_ok() {
            self.inflight += 1;
        }
    }

    fn start_turn(&mut self, chat: i64, text: String, images: Vec<String>) {
        let c = self.chats.get_mut(&chat).expect("chat exists");
        let Some(agent) = c.agent.take() else {
            c.queue.push_back((text, images));
            return;
        };
        c.last = now();
        c.typing = true;
        c.inbox = agent.inbox.clone();
        if let Some(c) = &self.ch {
            c.typing(chat);
        }
        self.submit(Job::Turn { chat, agent, text, images });
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Reply { chat, text } => {
                if self.trusted == Some(chat) {
                    self.trusted = None;
                }
                if let Some(c) = self.chats.get_mut(&chat) {
                    c.typing = false;
                    // sent too late for the turn to see: run them next
                    c.queue.extend(c.inbox.lock().map(|mut q| q.drain(..).collect::<Vec<_>>()).unwrap_or_default());
                }
                let text = match self.yolo_left(chat) {
                    Some(left) => format!("{text}\n\n⚡ yolo {} left", crate::autonomy::fmt_left(left)),
                    None => text,
                };
                self.say(chat, &text);
                self.attachments(chat);
                if let (true, Some(t)) = (crate::voice::should_speak(&self.cfg.voice, chat), self.ch.clone()) {
                    let c = self.cfg.voice.clone();
                    std::thread::spawn(move || match crate::voice::speak(&c, &text) {
                        Ok(ogg) => t.voice(chat, &ogg),
                        Err(e) => eprintln!("pagurus: {e}"),
                    });
                }
            }
            Event::TurnDone { chat, agent, note } => {
                self.inflight -= 1;
                if !note.is_empty() {
                    self.say(chat, &note);
                }
                self.give_back(chat, agent);
            }
            Event::Messages(_) | Event::Polled => {} // handled in the main loop
            Event::Reviewed { chat, agent, note } => {
                self.inflight -= 1;
                if let Some(n) = note {
                    self.say(chat, &format!("{n} (/learn)"));
                }
                if let Some(agent) = agent {
                    self.give_back(chat, agent);
                }
            }
            Event::Heartbeat { key, once, msg } => {
                self.inflight -= 1;
                self.heartbeat_running = false;
                match msg {
                    Some(msg) => {
                        eprintln!("pagurus: heartbeat alert: {}", crate::util::one_line(&msg, 200));
                        self.say(self.owner(), &msg);
                        self.attachments(self.owner());
                        self.last_alert.insert(key, (now(), msg));
                    }
                    None => {
                        self.last_alert.remove(&key);
                    }
                }
                for t in &once {
                    tasks::finish(&self.home, t);
                }
            }
            Event::Tidied { chat, result } => {
                self.inflight -= 1;
                match (chat, result) {
                    (Some(c), r) => self.say(c, &r.unwrap_or_else(|e| e)),
                    (None, Ok(msg)) if msg.contains('→') => self.say(self.owner(), &format!("🧹 tidied memory: {msg} (/learn)")),
                    (None, Ok(msg)) if msg.starts_with("📚") && !msg.contains("nothing to curate") => self.say(self.owner(), &msg),
                    (None, Ok(_)) => {}
                    (None, Err(e)) => eprintln!("pagurus: tidy skipped: {e}"),
                }
            }
            Event::Checked { results } => {
                self.inflight -= 1;
                self.checks_running = false;
                for (text, state, msg) in results {
                    self.check_state.insert(text, state);
                    if let Some(m) = msg {
                        eprintln!("pagurus: check: {}", crate::util::one_line(&m, 200));
                        self.say(self.owner(), &m);
                    }
                }
            }
            Event::Approval { chat, cmd, answer } if self.yolo_left(chat).is_some() => {
                eprintln!("pagurus: chat {chat} APPROVED (yolo) `{cmd}`");
                crate::autonomy::audit(&self.home, "yolo", &cmd);
                self.say(chat, &format!("⚡ {}", crate::util::one_line(&cmd, 200)));
                let _ = answer.send(true);
            }
            Event::Approval { chat, cmd, answer } if self.trusted == Some(chat) => {
                eprintln!("pagurus: chat {chat} APPROVED (via /all) `{cmd}`");
                crate::autonomy::audit(&self.home, "all", &cmd);
                self.say(chat, &format!("✅ running (approved for this message): {}", crate::util::one_line(&cmd, 200)));
                let _ = answer.send(true);
            }
            Event::Ask { chat, text, options } => match &self.ch {
                Some(c) if c.buttons(chat) => {
                    c.markup(chat, &format!("❓ {text}"), Some(&crate::ask::keyboard(&options)));
                }
                Some(c) => {
                    let list: String = options.iter().enumerate().map(|(i, o)| format!("\n{}. {o}", i + 1)).collect();
                    c.markup(chat, &format!("❓ {text}{list}"), None);
                }
                None => {
                    crate::ask::answer(chat, "");
                }
            },
            Event::Approval { chat, cmd, answer } => self.ask_approval(chat, cmd, answer),
            Event::BgApproval { cmd, answer } => self.ask_approval(self.owner(), cmd, answer),
            Event::Notice(text) => self.say(self.owner(), &text),
        }
    }

    /// Put a command to the user; one question at a time, the rest wait their turn.
    fn ask_approval(&mut self, chat: i64, cmd: String, answer: Sender<bool>) {
        match self.ch.clone() {
            Some(_) if self.pending.is_some() => self.waiting.push_back((chat, cmd, answer)),
            Some(c) => {
                self.ask(&c, chat, &cmd);
                self.pending = Some(Pending { chat, cmd, answer, deadline: now() + APPROVAL_SECS });
            }
            None => {
                let _ = answer.send(false);
            }
        }
    }

    fn attachments(&self, chat: i64) {
        for (path, caption) in crate::tools::take_attachments() {
            match &self.ch {
                Some(c) => c.send_file(chat, &path, &caption),
                None => println!("📎 {} {caption}", path.display()),
            }
        }
    }

    /// Seconds of yolo left for this chat.
    fn yolo_left(&self, chat: i64) -> Option<u64> {
        self.yolo.filter(|(c, until)| *c == chat && *until > now()).map(|(_, until)| until - now())
    }

    /// Tell the chat once its yolo has run out.
    fn expire_yolo(&mut self) {
        if let Some((chat, until)) = self.yolo {
            if now() >= until {
                self.yolo = None;
                self.say(chat, "yolo ended: commands need your approval again.");
            }
        }
    }

    fn give_back(&mut self, chat: i64, agent: Box<Agent>) {
        let c = self.chats.entry(chat).or_insert_with(|| Chat { agent: None, last: now(), queue: VecDeque::new(), typing: false, inbox: Default::default() });
        c.agent = Some(agent);
        c.typing = false;
        if let Some((next, images)) = c.queue.pop_front() {
            self.start_turn(chat, next, images);
        }
    }

    fn answer(&mut self, ok: bool, how: &str, why: &str) {
        if let Some(p) = self.pending.take() {
            crate::autonomy::audit(&self.home, how, &p.cmd);
            let _ = p.answer.send(ok);
            if let Some(c) = &self.ch {
                c.markup(p.chat, why, Some(&json!({ "remove_keyboard": true })));
            }
        }
        if let Some((chat, cmd, answer)) = self.waiting.pop_front() {
            self.ask_approval(chat, cmd, answer);
        }
    }

    fn health(&self) -> String {
        let up = now().saturating_sub(self.started);
        let busy: Vec<String> = self.chats.iter().filter(|(_, c)| c.agent.is_none()).map(|(id, _)| channel::qualified(*id)).collect();
        let cooling = crate::llm::cooldowns();
        format!(
            "pagurus {} · up {}h{:02}m · model {}\nworker: {}\ncooling down: {}\nunsent messages: {}\nstanding tasks: {} · learning {} · memory {} KiB",
            crate::util::version(),
            up / 3600,
            (up % 3600) / 60,
            self.cfg.provider.model,
            if self.inflight == 0 { "idle".to_string() } else { format!("busy ({} job(s); chats {})", self.inflight, busy.join(", ")) },
            if cooling.is_empty() { "none".into() } else { cooling.join(", ") },
            self.ch.as_ref().map(|c| c.outbox_len()).unwrap_or(0),
            tasks::list(&self.home).len(),
            self.cfg.learning.mode,
            crate::util::rss_kb().unwrap_or(0),
        )
    }

    /// A message from any channel. Returns an immediate reply, if any.
    fn message(&mut self, chat: i64, text: &str, images: Vec<String>) -> Option<String> {
        let mut w = text.split_whitespace();
        let cmd = w.next().unwrap_or("").split('@').next().unwrap_or("").to_lowercase();
        let arg = w.next().unwrap_or("").to_string();
        let rest = w.collect::<Vec<_>>().join(" ");
        if let Some(p) = &self.pending {
            if p.chat == chat {
                match cmd.as_str() {
                    "/yes" | "yes" | "y" => {
                        self.answer(true, "you", "✅ approved — running it");
                        return None;
                    }
                    "/no" | "no" | "n" => {
                        self.answer(false, "denied", "❌ denied");
                        return None;
                    }
                    "/all" => {
                        self.trusted = Some(chat);
                        self.answer(true, "all", "✅ approved — and everything else it runs for this message");
                        return None;
                    }
                    _ => {}
                }
            }
        }
        if !cmd.starts_with('/') && crate::ask::answer(chat, text) {
            return None; // the answer to a question the running turn asked
        }
        let home = self.home.clone();
        let cfg = &self.cfg;
        let need = |r: Result<String, String>| r.unwrap_or_else(|e| e);
        let busy = self.chats.get(&chat).is_some_and(|c| c.agent.is_none());
        Some(match cmd.as_str() {
            "/start" | "/help" => "pagurus here. /model · /good · /bad <what was wrong> · /undo · /retry · /stop · /new · /health · /rollback [n] (file edits) · /procs · /curate · /board [id | unblock id] · /peers · /pin <fact> · /pins · /unpin <n> · /tasks · /learn (what I learned + proposals) · /show <id> · \
                                   /approve <id|all> · /reject <id|all> [note] · /undo <id> · /lock <memory|user|skill> · /unlock … · \
                                   /tidy · /history · /skills [archive|restore <name>] · \
                                   /yolo [30m|off] (no approvals for a while; the safety floor stays) · /audit [all]"
                .into(),
            "/stop" => {
                if !busy {
                    return Some("Nothing is running.".into());
                }
                crate::util::CANCEL.store(true, Ordering::Relaxed);
                if self.trusted == Some(chat) {
                    self.trusted = None;
                }
                if self.yolo.is_some_and(|(c, _)| c == chat) {
                    self.yolo = None;
                    self.say(chat, "yolo off.");
                }
                if let Some(c) = self.chats.get_mut(&chat) {
                    c.queue.clear();
                    if let Ok(mut q) = c.inbox.lock() {
                        q.clear();
                    }
                }
                if self.pending.as_ref().is_some_and(|p| p.chat == chat) {
                    self.answer(false, "denied", "❌ cancelled");
                }
                "⏹ stopping after the current step…".into()
            }
            "/health" | "/status" => self.health(),
            "/model" => {
                if self.chats.values().any(|c| c.agent.is_none()) {
                    return Some("I'm busy right now — switch models when I'm done (or /stop).".into());
                }
                match crate::config::model_command(&home, &self.cfg, &format!("{arg} {rest}")) {
                    Ok((msg, None)) => msg,
                    Ok((msg, Some(new))) => {
                        for c in self.chats.values_mut() {
                            if let Some(a) = c.agent.as_mut() {
                                if let Err(e) = a.switch_model(&new) {
                                    return Some(format!("error: {e}"));
                                }
                            }
                        }
                        let _ = self.jobs.send(Job::Reconfigure(Box::new(new.clone())));
                        self.cfg = new;
                        msg
                    }
                    Err(e) => e,
                }
            }
            "/pin" | "/pins" | "/unpin" => {
                let all = format!("{arg} {rest}").trim().to_string();
                match self.chats.get_mut(&chat) {
                    None if cmd == "/pin" && !all.is_empty() => match Agent::new(home.clone(), cfg.clone(), false, &channel::kind(chat)) {
                        Ok(mut a) => {
                            let r = a.pin(&all);
                            self.chats.insert(chat, Chat { agent: Some(Box::new(a)), last: now(), queue: VecDeque::new(), typing: false, inbox: Default::default() });
                            r
                        }
                        Err(e) => format!("error: {e}"),
                    },
                    None => "Nothing pinned. /pin <fact> keeps something in view for this whole conversation.".into(),
                    Some(c) => match c.agent.as_mut() {
                        None => "I'm busy with your last message — try again in a moment.".into(),
                        Some(a) if cmd == "/pin" => a.pin(&all),
                        Some(a) if cmd == "/unpin" => a.unpin(&all),
                        Some(a) => a.pins_text(),
                    },
                }
            }
            "/new" => {
                if busy {
                    return Some("I'm still working on your last message — /stop first.".into());
                }
                if let Some(c) = self.chats.remove(&chat) {
                    if let Some(agent) = c.agent.filter(|a| a.has_unreviewed()) {
                        self.submit(Job::Review { chat, agent, keep: false });
                    }
                }
                "Fresh session.".into()
            }
            "/yes" | "/no" | "/all" => "Nothing is waiting for approval.".into(),
            "/yolo" if chat != self.owner() => "Only the owner can turn on yolo.".into(),
            "/yolo" => {
                let left = self.yolo_left(chat);
                let mut set = None;
                let reply = crate::autonomy::command(&format!("{arg} {rest}"), left, &mut |secs| set = Some(secs));
                match set {
                    Some(0) => self.yolo = None,
                    Some(secs) => {
                        self.yolo = Some((chat, now() + secs));
                        eprintln!("pagurus: chat {chat} yolo on for {secs}s");
                    }
                    None => {}
                }
                reply
            }
            "/audit" => crate::autonomy::report(&home, arg == "all", 20),
            "/lock" => need(learn::set_lock(&home, &arg, true)),
            "/unlock" => need(learn::set_lock(&home, &arg, false)),
            "/learn" => learn::list_text(&home),
            "/show" => need(learn::show_text(&home, cfg, &arg)),
            "/approve" => need(learn::approve(&home, cfg, &arg)),
            "/reject" => need(learn::reject(&home, &arg, &rest)),
            "/history" => learn::history_text(&home, 15),
            "/rollback" => need(crate::files::rollback(&home, &arg)),
            "/procs" => match arg.as_str() {
                "kill" => need(rest.trim_start_matches('#').parse().map_err(|_| "usage: /procs kill <n>".to_string()).and_then(crate::procs::kill)),
                _ => crate::procs::list(),
            },
            "/tasks" => tasks::describe(&home, cfg.daemon.heartbeat_minutes),
            "/board" => need(crate::board::command(&home, cfg, &format!("{arg} {rest}").split_whitespace().collect::<Vec<_>>())),
            "/peers" => crate::peer::list_text(&home, cfg),
            "/skills" => match arg.as_str() {
                "archive" => need(crate::skills::archive(&home, &rest)),
                "restore" => need(crate::skills::restore(&home, &rest)),
                _ => crate::skills::list_text(&home),
            },
            "/good" | "/bad" => {
                let note = format!("{arg} {rest}").trim().to_string();
                match self.chats.get_mut(&chat).map(|c| c.agent.take()) {
                    None => "There's no answer yet to rate.".into(),
                    Some(None) => "I'm busy with your last message — rate it when it's done.".into(),
                    Some(Some(mut agent)) => {
                        let r = agent.feedback(cmd == "/good", &note);
                        if cmd == "/bad" && r.starts_with("👎") {
                            self.submit(Job::Review { chat, agent, keep: true });
                        } else if let Some(c) = self.chats.get_mut(&chat) {
                            c.agent = Some(agent);
                        }
                        r
                    }
                }
            }
            "/curate" => {
                self.submit(Job::Curate { chat: Some(chat) });
                "📚 reviewing the skill library…".into()
            }
            "/tidy" => {
                self.submit(Job::Tidy { chat: Some(chat), force: true });
                "🧹 tidying memory…".into()
            }
            "/undo" if !arg.is_empty() => need(learn::rollback(&home, &arg)),
            "/undo" | "/retry" => {
                if busy {
                    return Some("I'm still working — /stop first.".into());
                }
                let Some(last) = self.chats.get_mut(&chat).and_then(|c| c.agent.as_mut()).and_then(|a| a.undo_last()) else {
                    return Some("Nothing to undo in this conversation.".into());
                };
                if cmd == "/retry" {
                    self.start_turn(chat, last, vec![]);
                    return None;
                }
                format!("↩️ forgot your last message (\"{}\") and my answer.", crate::util::one_line(&last, 60))
            }
            _ => {
                if let std::collections::hash_map::Entry::Vacant(v) = self.chats.entry(chat) {
                    match Agent::new(home.clone(), cfg.clone(), false, &channel::kind(chat)) {
                        Ok(a) => {
                            v.insert(Chat { agent: Some(Box::new(a)), last: now(), queue: VecDeque::new(), typing: false, inbox: Default::default() });
                        }
                        Err(e) => return Some(format!("error: {e}")),
                    }
                }
                let text = if text.trim().is_empty() { "What's in this image?" } else { text };
                if let Some(c) = self.chats.get(&chat).filter(|c| c.typing) {
                    if let Ok(mut q) = c.inbox.lock() {
                        q.push((text.to_string(), images));
                        return Some("📥 got it — I'll take that into account after the current step (/stop cancels).".into());
                    }
                }
                self.start_turn(chat, text.to_string(), images);
                if busy {
                    return Some("⏳ got it — I'll do this after the current one (/stop cancels).".into());
                }
                return None;
            }
        })
    }

    /// Self-directed learning while idle: reflect once on a chat that went quiet.
    fn idle_reviews(&mut self) {
        let idle = self.cfg.learning.idle_review_minutes * 60;
        if idle == 0 {
            return;
        }
        let quiet: Vec<i64> = self
            .chats
            .iter()
            .filter(|(_, c)| now().saturating_sub(c.last) >= idle && c.agent.as_ref().is_some_and(|a| a.has_unreviewed()))
            .map(|(id, _)| *id)
            .collect();
        for chat in quiet {
            if let Some(agent) = self.chats.get_mut(&chat).and_then(|c| c.agent.take()) {
                self.submit(Job::Review { chat, agent, keep: true });
            }
        }
    }
}

pub fn run(home: PathBuf, cfg: Config) -> Result<(), String> {
    take_lock(&home)?;
    let ch = channels(&cfg, &home)?;
    let (jobs, job_rx) = channel();
    let (event_tx, events) = channel();
    {
        let (home, cfg, tx) = (home.clone(), cfg.clone(), event_tx.clone());
        std::thread::Builder::new()
            .name("pagurus-worker".into())
            .stack_size(512 * 1024)
            .spawn(move || worker(home, cfg, job_rx, tx))
            .map_err(|e| e.to_string())?;
    }
    // each channel receives on its own thread, so the main loop wakes the
    // moment either a message or finished work arrives
    for c in ch.iter().flat_map(|h| h.list().to_vec()) {
        let tx = event_tx.clone();
        std::thread::Builder::new()
            .name(format!("pagurus-{}", c.name()))
            .stack_size(256 * 1024)
            .spawn(move || loop {
                let ev = match c.receive() {
                    Ok(m) if m.is_empty() => Event::Polled,
                    Ok(m) => Event::Messages(m.into_iter().map(|(to, text, file)| (channel::handle(&format!("{}:{to}", c.name())), text, file)).collect()),
                    Err(e) => {
                        eprintln!("pagurus: {} receive failed: {e}", c.name());
                        std::thread::sleep(Duration::from_secs(10));
                        continue;
                    }
                };
                if tx.send(ev).is_err() {
                    break;
                }
            })
            .map_err(|e| e.to_string())?;
    }
    // background work: board tasks and requests from peer pagurus agents
    let hooks = {
        let (tx, tx2) = (event_tx.clone(), event_tx.clone());
        Arc::new(crate::board::Hooks {
            notify: Box::new(move |m| {
                let _ = tx.send(Event::Notice(m));
            }),
            approve: Box::new(move |cmd| {
                let (answer, rx) = channel();
                // it may have to wait behind other questions
                tx2.send(Event::BgApproval { cmd: cmd.to_string(), answer }).is_ok()
                    && rx.recv_timeout(Duration::from_secs(APPROVAL_SECS * 10)).unwrap_or(false)
            }),
            can_ask: ch.is_some(),
        })
    };
    crate::board::spawn(home.clone(), cfg.clone(), hooks.clone());
    #[cfg(feature = "art")]
    match crate::art_dash::serve(home.clone(), &cfg.art.dashboard) {
        Ok(Some(a)) => eprintln!("pagurus: art dashboard on http://{a}"),
        Ok(None) => {}
        Err(e) => eprintln!("pagurus: art dashboard not started: {e}"),
    }
    if let Some(addr) = crate::peer::serve(home.clone(), cfg.clone(), hooks)? {
        eprintln!("pagurus: accepting tasks from paired pagurus agents on {addr} as `{}`", crate::peer::self_name(&cfg));
    }
    drop(event_tx);
    let default_every = cfg.daemon.heartbeat_minutes;
    eprintln!(
        "pagurus daemon · model {} · {} standing task(s), default every {}m · channels: {} · learning {}",
        cfg.provider.model,
        tasks::list(&home).len(),
        default_every,
        ch.as_ref().map(|c| c.names().join(", ")).unwrap_or_else(|| "none".into()),
        cfg.learning.mode
    );
    let mut d = Daemon {
        home,
        cfg,
        ch,
        jobs,
        chats: HashMap::new(),
        pending: None,
        waiting: VecDeque::new(),
        trusted: None,
        yolo: None,
        inflight: 0,
        heartbeat_running: false,
        checks_running: false,
        check_state: HashMap::new(),
        last_alert: HashMap::new(),
        started: now(),
    };
    // timers survive restarts: a daemon restarted daily still tidies, a daily task
    // doesn't fire after every restart, and one missed while offline runs once
    let state = State::load(&d.home);
    let mut next_due: HashMap<String, u64> = state.next_due;
    let (mut last_tidy, mut last_curate) = (state.last_tidy, state.last_curate);
    let mut saved = (last_tidy, last_curate, next_due.clone());
    let mut last_typing = 0;
    loop {

        d.expire_yolo();
        if d.pending.as_ref().is_some_and(|p| now() >= p.deadline) {
            eprintln!("pagurus: approval timed out");
            d.answer(false, "timeout", "⌛ no answer — not running it");
        }

        // schedule due standing tasks (re-read so heartbeat-tool edits apply at once)
        let list = tasks::list(&d.home);
        next_due.retain(|k, _| list.iter().any(|t| &t.text == k));
        let mut soonest = now() + 50;
        let (mut due, mut once, mut checks) = (Vec::new(), Vec::new(), Vec::new());
        for t in &list {
            let check = t.check();
            let busy = if check.is_some() { d.checks_running } else { d.heartbeat_running };
            let fire = if let Some(at) = t.at {
                if now() >= at && !busy {
                    once.push(t.text.clone());
                    true
                } else {
                    soonest = soonest.min(at.max(now() + 1));
                    false
                }
            } else {
                let interval_next = |from: u64| match t.cal {
                    Some(c) => c.next_after(from),
                    None => from + t.every.unwrap_or(default_every) * 60,
                };
                if t.cal.is_none() && t.every.unwrap_or(default_every) == 0 {
                    continue;
                }
                let first = match t.cal {
                    Some(c) => c.next_after(now()),
                    None => now() + if check.is_some() { 10 } else { 60 },
                };
                let mut next = *next_due.entry(t.text.clone()).or_insert(first);
                if t.cal.is_some() && next + CATCH_UP_SECS < now() {
                    next = first; // missed long ago (offline): wait for the next time instead of running late
                    next_due.insert(t.text.clone(), next);
                }
                if now() >= next && !busy {
                    next_due.insert(t.text.clone(), interval_next(now()));
                    true
                } else {
                    soonest = soonest.min(next.max(now() + 1));
                    false
                }
            };
            if !fire {
                continue;
            }
            match check {
                Some(c) => {
                    let before = d.check_state.get(&t.text).copied().unwrap_or(UNKNOWN);
                    checks.push((t.text.clone(), c, before));
                }
                None if t.at.is_some() => due.push(format!("(one-off reminder: always message the user about it) {}", t.text)),
                None if t.cal.is_some() => due.push(format!("(scheduled task: always message the user with the result) {}", t.text)),
                None => due.push(t.text.clone()),
            }
        }
        if saved.0 != last_tidy || saved.1 != last_curate || saved.2 != next_due {
            saved = (last_tidy, last_curate, next_due.clone());
            State { last_tidy, last_curate, next_due: next_due.clone() }.save(&d.home);
        }
        if !checks.is_empty() {
            d.checks_running = true;
            d.submit(Job::Checks { items: checks });
        }
        if !due.is_empty() {
            let key = due.join("\n");
            let previous = d.last_alert.get(&key).cloned();
            d.heartbeat_running = true;
            d.submit(Job::Heartbeat { key, due, once, previous });
        } else {
            for t in &once {
                tasks::finish(&d.home, t); // a one-off check: ran once, done
            }
        }

        // sleep until the next scheduled task, a message, or finished work
        let mut wait = soonest.saturating_sub(now()).clamp(1, 50);
        if d.chats.values().any(|c| c.typing) {
            wait = wait.min(4); // Telegram's "typing…" lasts ~5 s
        }
        if let Some(p) = &d.pending {
            wait = wait.min(p.deadline.saturating_sub(now()).max(1));
        }
        match events.recv_timeout(Duration::from_secs(wait)) {
            Ok(Event::Messages(msgs)) => {
                if let Some(c) = &d.ch {
                    c.flush_outbox();
                }
                for (chat, mut text, image) in msgs {
                    let mut images = Vec::new();
                    if let (Some(v), Some(c)) = (image.as_deref().and_then(|i| i.strip_prefix(crate::voice::TAG)), &d.ch) {
                        match c.fetch(chat, v).and_then(|b| crate::voice::transcribe(&d.cfg.voice, &b)) {
                            Ok(heard) => {
                                crate::voice::heard(chat);
                                text = format!("{text}\n(voice message, transcribed) {heard}").trim().to_string();
                            }
                            Err(e) => {
                                d.say(chat, &format!("(couldn't use that voice message: {})", crate::util::one_line(&e, 240)));
                                continue;
                            }
                        }
                    } else if let (Some(id), Some(c)) = (image, &d.ch) {
                        match c.fetch(chat, &id).and_then(|b| crate::util::image_data_url(&b)) {
                            Ok(url) => images.push(url),
                            Err(e) => d.say(chat, &format!("(couldn't use that image: {})", crate::util::one_line(&e, 120))),
                        }
                    }
                    if let Some(reply) = d.message(chat, &text, images) {
                        d.say(chat, &reply);
                    }
                }
            }
            Ok(Event::Polled) => {
                if let Some(c) = &d.ch {
                    c.flush_outbox();
                }
                if d.inflight == 0 {
                    d.idle_reviews();
                    // at most daily, and only once memory is filling up
                    if now().saturating_sub(last_tidy) >= 86_400 {
                        last_tidy = now();
                        d.submit(Job::Tidy { chat: None, force: false });
                    }
                    // weekly look at the skill library, and only if there's a library to look at
                    if now().saturating_sub(last_curate) >= 7 * 86_400 && crate::tools::skill_index(&d.home).len() >= 3 {
                        last_curate = now();
                        d.submit(Job::Curate { chat: None });
                    }
                }
            }
            Ok(ev) => d.event(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err("worker thread stopped".into()),
        }
        for m in crate::procs::take_finished() {
            d.say(d.owner(), &m); // a background process ended
        }
        if let Some(t) = &d.ch {
            if now() >= last_typing + 4 {
                last_typing = now();
                for (id, c) in &d.chats {
                    if c.typing {
                        t.typing(*id);
                    }
                }
            }
        }
    }
}

/// A scheduled task missed by more than this while pagurus was off is skipped, not run late.
const CATCH_UP_SECS: u64 = 12 * 3600;

/// What the daemon remembers across restarts (`state/daemon.json`).
struct State {
    last_tidy: u64,
    last_curate: u64,
    /// When each standing task (by its text) runs next.
    next_due: HashMap<String, u64>,
}

impl State {
    fn path(home: &Path) -> PathBuf {
        home.join("state").join("daemon.json")
    }

    fn load(home: &Path) -> State {
        let v: serde_json::Value = serde_json::from_str(&read(&Self::path(home))).unwrap_or_default();
        let t = |k: &str| v[k].as_u64().filter(|t| *t > 0 && *t <= now()).unwrap_or_else(now);
        let next_due = v["next_due"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_u64()?))).collect()).unwrap_or_default();
        State { last_tidy: t("last_tidy"), last_curate: t("last_curate"), next_due }
    }

    fn save(&self, home: &Path) {
        let p = Self::path(home);
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(home));
        let v = json!({ "last_tidy": self.last_tidy, "last_curate": self.last_curate, "next_due": self.next_due });
        if let Err(e) = crate::util::write_atomic(&p, &v.to_string()) {
            eprintln!("pagurus: couldn't save daemon state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn daemon_timers_survive_a_restart() {
        let home = std::env::temp_dir().join(format!("pagurus-state-{}", crate::util::new_id()));
        let fresh = super::State::load(&home);
        assert!(fresh.last_tidy >= crate::util::now() - 5 && fresh.next_due.is_empty(), "first start: timers start now");
        let mut due = std::collections::HashMap::new();
        due.insert("check disk".to_string(), 1_900_000_000u64);
        super::State { last_tidy: 1_000, last_curate: 2_000, next_due: due }.save(&home);
        let back = super::State::load(&home);
        assert_eq!((back.last_tidy, back.last_curate), (1_000, 2_000));
        assert_eq!(back.next_due["check disk"], 1_900_000_000);
        let _ = std::fs::remove_dir_all(&home);
    }

    use serde_json::json;

    #[test]
    fn picks_a_sensible_photo_size() {
        let m = json!({"photo": [
            {"file_id": "s", "width": 90, "height": 60},
            {"file_id": "m", "width": 1280, "height": 853},
            {"file_id": "w", "width": 2560, "height": 1706}]});
        assert_eq!(super::best_photo(&m).as_deref(), Some("m"));
        let doc = json!({"document": {"file_id": "d", "mime_type": "image/png", "file_size": 2048}});
        assert_eq!(super::best_photo(&doc).as_deref(), Some("d"));
        let pdf = json!({"document": {"file_id": "p", "mime_type": "application/pdf", "file_size": 2048}});
        assert_eq!(super::best_photo(&pdf), None);
    }

    #[test]
    fn messages_during_a_turn_steer_it() {
        let home = std::env::temp_dir().join(format!("pagurus-steer-{}", crate::util::new_id()));
        let mut cfg = crate::config::Config::default();
        cfg.provider.model = "test".into();
        let (jobs, rx) = std::sync::mpsc::channel();
        let mut d = super::Daemon {
            home,
            cfg,
            ch: None,
            jobs,
            chats: Default::default(),
            pending: None,
            waiting: Default::default(),
            trusted: None,
            yolo: None,
            inflight: 0,
            heartbeat_running: false,
            checks_running: false,
            check_state: Default::default(),
            last_alert: Default::default(),
            started: 0,
        };
        assert!(d.message(7, "why is it so cpu heavy", vec![]).is_none());
        let Ok(super::Job::Turn { agent, .. }) = rx.try_recv() else { panic!("turn not started") };
        let ack = d.message(7, "only look at the browser", vec![]).unwrap();
        assert!(ack.starts_with("📥"));
        assert_eq!(agent.inbox.lock().unwrap()[0].0, "only look at the browser", "the running turn sees it");
        // one the turn didn't get to runs next, after the reply
        d.event(super::Event::Reply { chat: 7, text: "done".into() });
        assert_eq!(d.chats[&7].queue.len(), 1);
        assert!(agent.inbox.lock().unwrap().is_empty());
        // between the reply and the end of reflection, messages queue as before
        assert!(d.message(7, "thanks", vec![]).unwrap().starts_with("⏳"));
        assert_eq!(d.chats[&7].queue.len(), 2);
    }
}

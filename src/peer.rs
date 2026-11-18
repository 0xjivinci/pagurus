//! Swarm: pagurus agents on different devices hand each other tasks.
//!
//! `pagurus daemon` can listen on a LAN/Tailscale address (off unless
//! `peer.listen` is set). Two pagurus agents pair once with a one-time 6-digit code:
//! an X25519 key exchange whose result is mixed with the code (HKDF) gives
//! both sides a shared secret, kept in each one's `env` file. From then on
//! every request and every answer is HMAC-SHA256 signed over a timestamp and
//! a random nonce, so nothing can be forged, changed or replayed.
//!
//! The asked pagurus runs the task as a sub-agent with ITS OWN tools, models and
//! approvals: a command that needs approval is put to its own owner on
//! Telegram, or refused when nobody is there. A peer never gets more than
//! the local user would allow.

use crate::board::Hooks;
use crate::config::Config;
use crate::util::{clip, now, one_line, read, redact};
use ring::{agreement, hkdf, hmac, rand::SecureRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Peer {
    pub name: String,
    /// Where to reach it; empty if it can ask us but we can't ask it.
    pub url: String,
    /// The name that peer knows this pagurus by.
    pub me_as: String,
    pub added: u64,
}

/// Requests older or newer than this are refused (clocks must roughly agree).
const WINDOW: u64 = 120;
const MAX_BODY: usize = 64 * 1024;
const MAX_TASK: usize = 32 * 1024;
const PAIR_SECS: u64 = 600;
const PAIR_TRIES: u32 = 5;

// ---------- small helpers ----------

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || s.len() > 256 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| "no randomness available".to_string())?;
    Ok(b)
}

/// Length-prefixed parts, so ("ab","c") and ("a","bc") sign differently.
fn message(parts: &[&[u8]]) -> Vec<u8> {
    let mut m = Vec::new();
    for p in parts {
        m.extend_from_slice(&(p.len() as u32).to_be_bytes());
        m.extend_from_slice(p);
    }
    m
}

fn sign(key: &[u8], parts: &[&[u8]]) -> String {
    hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &message(parts)).as_ref())
}

fn verify(key: &[u8], parts: &[&[u8]], sig: &str) -> bool {
    unhex(sig).is_some_and(|tag| hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), &message(parts), &tag).is_ok())
}

struct Len32;
impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

/// The pairing secret: the X25519 result mixed with the one-time code.
fn derive(shared: &[u8], code: &str, pub_a: &[u8], pub_b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, code.as_bytes()).extract(shared);
    let info: [&[u8]; 3] = [b"pagurus-peer-v1", pub_a, pub_b];
    if let Ok(okm) = prk.expand(&info, Len32) {
        let _ = okm.fill(&mut out);
    }
    out
}

pub fn valid_name(n: &str) -> Result<(), String> {
    if n.is_empty() || n.len() > 32 || !n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(format!("peer name `{n}`: use 1-32 of a-z, 0-9 and -"));
    }
    Ok(())
}

fn tidy_name(n: &str) -> String {
    let s: String = n.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(32).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "pagurus".into()
    } else {
        s
    }
}

pub fn self_name(cfg: &Config) -> String {
    if !cfg.peer.name.is_empty() {
        return tidy_name(&cfg.peer.name);
    }
    let host = read(Path::new("/etc/hostname")).trim().to_string();
    let host = if host.is_empty() {
        std::process::Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    } else {
        host
    };
    tidy_name(host.split('.').next().unwrap_or(""))
}

fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() <= 200 && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ---------- storage: peers.json (names, urls) + secrets in the env file ----------

pub fn load(home: &Path) -> Vec<Peer> {
    serde_json::from_str(&read(&home.join("peers.json"))).unwrap_or_default()
}

fn save(home: &Path, peers: &[Peer]) -> Result<(), String> {
    crate::util::write_private(&home.join("peers.json"), &serde_json::to_string_pretty(peers).map_err(|e| e.to_string())?)
}

pub fn names(home: &Path) -> Vec<String> {
    load(home).into_iter().map(|p| p.name).collect()
}

fn key_var(name: &str) -> String {
    format!("PAGURUS_PEER_KEY_{}", name.to_uppercase().replace('-', "_"))
}

/// Read fresh from the env file each time: pairing happens while the daemon runs.
fn secret(home: &Path, name: &str) -> Option<Vec<u8>> {
    let k = format!("{}=", key_var(name));
    read(&home.join("env")).lines().find_map(|l| l.trim().strip_prefix(&k).and_then(|v| unhex(v.trim()))).filter(|s| s.len() == 32)
}

fn store(home: &Path, peer: Peer, key: &[u8]) -> Result<(), String> {
    crate::util::set_env_value(home, &key_var(&peer.name), &hex(key))?;
    let mut all = load(home);
    all.retain(|p| p.name != peer.name);
    all.push(peer);
    save(home, &all)
}

pub fn remove(home: &Path, name: &str) -> Result<String, String> {
    let mut all = load(home);
    let n = all.len();
    all.retain(|p| p.name != name);
    if all.len() == n {
        return Err(format!("no peer `{name}`"));
    }
    save(home, &all)?;
    let k = format!("{}=", key_var(name));
    let env: Vec<String> = read(&home.join("env")).lines().filter(|l| !l.trim().starts_with(&k)).map(String::from).collect();
    crate::util::write_private(&home.join("env"), &(env.join("\n") + "\n"))?;
    Ok(format!("removed peer `{name}` and its key"))
}

// ---------- replay protection ----------

static SEEN: std::sync::Mutex<Vec<(u64, String)>> = std::sync::Mutex::new(Vec::new());

/// True the first time a (peer, nonce) shows up within the window.
fn fresh_nonce(peer: &str, nonce: &str, t: u64) -> bool {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|(at, _)| at + 2 * WINDOW + 5 >= t);
    let key = format!("{peer}:{nonce}");
    if seen.iter().any(|(_, k)| *k == key) || seen.len() >= 20_000 {
        return false;
    }
    seen.push((t, key));
    true
}

/// Check a signed request. Returns the peer's key, to sign the answer with.
fn authenticate(home: &Path, h: &Headers, method: &str, path: &str, body: &[u8]) -> Result<(String, Vec<u8>), String> {
    let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or("");
    let (peer, time, nonce, sig) = (get("x-pagurus-peer"), get("x-pagurus-time"), get("x-pagurus-nonce"), get("x-pagurus-sig"));
    valid_name(peer).map_err(|_| "unknown peer".to_string())?;
    let key = secret(home, peer).ok_or("unknown peer")?;
    let t: u64 = time.parse().map_err(|_| "bad timestamp")?;
    if now().abs_diff(t) > WINDOW {
        return Err("timestamp out of range (check both clocks)".into());
    }
    if nonce.len() < 16 || nonce.len() > 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("bad nonce".into());
    }
    if !verify(&key, &[b"req", method.as_bytes(), path.as_bytes(), time.as_bytes(), nonce.as_bytes(), body], sig) {
        return Err("bad signature".into());
    }
    // only after the signature: strangers can't fill the nonce table
    if !fresh_nonce(peer, nonce, now()) {
        return Err("replayed request".into());
    }
    Ok((peer.to_string(), key))
}

// ---------- server (inside `pagurus daemon`) ----------

type Headers = Vec<(String, String)>;

struct Request {
    method: String,
    path: String,
    headers: Headers,
    body: Vec<u8>,
}

fn read_request(r: &mut impl BufRead) -> Result<Request, (u16, String)> {
    let bad = |m: &str| (400, m.to_string());
    let mut head = r.take(8 * 1024);
    let mut line = String::new();
    head.read_line(&mut line).map_err(|_| bad("unreadable request"))?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut headers = Vec::new();
    loop {
        line.clear();
        if head.read_line(&mut line).map_err(|_| bad("unreadable headers"))? == 0 {
            return Err(bad("headers too long or cut off"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if headers.len() >= 40 {
            return Err(bad("too many headers"));
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err((413, "body too large".into()));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).map_err(|_| bad("body cut off"))?;
    Ok(Request { method, path, headers, body })
}

fn respond(mut s: &std::net::TcpStream, status: u16, body: &str, sig: Option<String>) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let sig = sig.map(|s| format!("X-Pagurus-Sig: {s}\r\n")).unwrap_or_default();
    let _ = write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{sig}Connection: close\r\n\r\n{body}",
        body.len()
    );
}

struct Server {
    home: PathBuf,
    cfg: Config,
    hooks: Arc<Hooks>,
    busy: AtomicUsize,
    conns: AtomicUsize,
}

/// Start listening on `peer.listen` (no-op when unset).
pub fn serve(home: PathBuf, cfg: Config, hooks: Arc<Hooks>) -> Result<Option<std::net::SocketAddr>, String> {
    if cfg.peer.listen.trim().is_empty() {
        return Ok(None);
    }
    let l = std::net::TcpListener::bind(cfg.peer.listen.trim()).map_err(|e| format!("peer.listen {}: {e}", cfg.peer.listen))?;
    let addr = l.local_addr().map_err(|e| e.to_string())?;
    let srv = Arc::new(Server { home, cfg, hooks, busy: AtomicUsize::new(0), conns: AtomicUsize::new(0) });
    std::thread::Builder::new()
        .name("pagurus-peer".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            for s in l.incoming().flatten() {
                if srv.conns.load(Ordering::SeqCst) >= 8 {
                    continue; // dropped: too many at once
                }
                srv.conns.fetch_add(1, Ordering::SeqCst);
                let srv = srv.clone();
                let spawned = std::thread::Builder::new().stack_size(1024 * 1024).spawn(move || {
                    srv.handle(s);
                    srv.conns.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    eprintln!("pagurus: peer: could not start a thread");
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Some(addr))
}

impl Server {
    fn handle(&self, s: std::net::TcpStream) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
        let from = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let req = match read_request(&mut BufReader::new(&s)) {
            Ok(r) => r,
            Err((code, m)) => return respond(&s, code, &json!({ "error": m }).to_string(), None),
        };
        if req.method != "POST" {
            return respond(&s, 404, r#"{"error":"not found"}"#, None);
        }
        if req.path == "/pair" {
            let (code, body) = match self.pair(&req.body) {
                Ok(v) => (200, v),
                Err(e) => {
                    eprintln!("pagurus: peer: pairing attempt from {from} refused: {e}");
                    (403, json!({ "error": e }))
                }
            };
            return respond(&s, code, &body.to_string(), None);
        }
        let (peer, key) = match authenticate(&self.home, &req.headers, &req.method, &req.path, &req.body) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("pagurus: peer: refused {} from {from}: {e}", req.path);
                // not signed: an attacker learns nothing from it
                return respond(&s, 401, &json!({ "error": e }).to_string(), None);
            }
        };
        let nonce = req.headers.iter().find(|(k, _)| k == "x-pagurus-nonce").map(|(_, v)| v.clone()).unwrap_or_default();
        let (code, body) = match req.path.as_str() {
            "/ping" => (200, json!({ "name": self_name(&self.cfg), "version": env!("CARGO_PKG_VERSION"), "busy": self.busy.load(Ordering::SeqCst) })),
            "/ask" => self.ask(&peer, &req.body),
            _ => (404, json!({ "error": "not found" })),
        };
        let body = body.to_string();
        let sig = sign(&key, &[b"resp", nonce.as_bytes(), code.to_string().as_bytes(), body.as_bytes()]);
        respond(&s, code, &body, Some(sig));
    }

    fn ask(&self, peer: &str, body: &[u8]) -> (u16, Value) {
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let task = v["task"].as_str().unwrap_or("").trim();
        if task.is_empty() || task.len() > MAX_TASK {
            return (400, json!({ "error": "task missing or too long" }));
        }
        if self.busy.fetch_add(1, Ordering::SeqCst) >= self.cfg.peer.max_tasks.max(1) as usize {
            self.busy.fetch_sub(1, Ordering::SeqCst);
            return (503, json!({ "error": "busy with other peer tasks; try again later" }));
        }
        eprintln!("pagurus: peer `{peer}` asks: {}", one_line(task, 120));
        let label = format!("(asked by peer pagurus `{peer}`)");
        // this machine's owner decides; the peer can't approve anything here
        let ask = |cmd: &str| (self.hooks.approve)(&format!("{cmd}\n{label}"));
        let intro = format!("{}\nThe peer is called `{peer}`.", crate::agent::PEER_INTRO);
        let r = crate::agent::run_sub(&self.home, &self.cfg, task, &intro, if self.hooks.can_ask { Some(&ask) } else { None });
        self.busy.fetch_sub(1, Ordering::SeqCst);
        match r {
            Ok((report, calls)) => (200, json!({ "report": clip(&redact(report.trim()), 8000), "calls": calls })),
            Err(e) => (500, json!({ "error": redact(&one_line(&e, 300)) })),
        }
    }

    /// The asked side of pairing: check the code, finish the key exchange, remember the peer.
    fn pair(&self, body: &[u8]) -> Result<Value, String> {
        let p = self.home.join("pairing-secret.json");
        let mut st: Value = serde_json::from_str(&read(&p)).map_err(|_| "no pairing code is active here (run `pagurus peer pair`)")?;
        let code = st["code"].as_str().unwrap_or("").to_string();
        if code.len() != 6 || now() > st["expires"].as_u64().unwrap_or(0) {
            let _ = std::fs::remove_file(&p);
            return Err("the pairing code expired; run `pagurus peer pair` again".into());
        }
        let v: Value = serde_json::from_slice(body).map_err(|_| "bad request")?;
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let (name, url, pub_a, you_as) = (tidy_name(&s("name")), s("url"), unhex(&s("pub")).ok_or("bad key")?, s("you_as"));
        if !verify(code.as_bytes(), &[b"pair1", &pub_a, s("name").as_bytes(), url.as_bytes(), you_as.as_bytes()], &s("mac")) {
            let tries = st["tries"].as_u64().unwrap_or(0) + 1;
            if tries >= PAIR_TRIES as u64 {
                let _ = std::fs::remove_file(&p);
                (self.hooks.notify)("⚠️ pagurus: too many wrong pairing codes; the code is void now".into());
            } else {
                st["tries"] = json!(tries);
                let _ = crate::util::write_private(&p, &st.to_string());
            }
            return Err("wrong pairing code".into());
        }
        if (!url.is_empty() && !valid_url(&url)) || valid_name(&you_as).is_err() {
            return Err("bad url or name".into());
        }
        let rng = ring::rand::SystemRandom::new();
        let mine = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).map_err(|_| "key generation failed")?;
        let pub_b = mine.compute_public_key().map_err(|_| "key generation failed")?.as_ref().to_vec();
        let key = agreement::agree_ephemeral(mine, &agreement::UnparsedPublicKey::new(&agreement::X25519, &pub_a), |sh| {
            derive(sh, &code, &pub_a, &pub_b)
        })
        .map_err(|_| "bad key")?;
        let _ = std::fs::remove_file(&p); // one-time
        store(&self.home, Peer { name: name.clone(), url: url.clone(), me_as: you_as, added: now() }, &key)?;
        let me = self_name(&self.cfg);
        let mac = sign(&key, &[b"pair2", &pub_a, &pub_b, name.as_bytes(), me.as_bytes()]);
        (self.hooks.notify)(format!("🤝 paired with pagurus `{name}`{}", if url.is_empty() { String::new() } else { format!(" ({url})") }));
        Ok(json!({ "name": me, "as": name, "pub": hex(&pub_b), "mac": mac }))
    }
}

// ---------- client ----------

fn http(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(timeout)
        .user_agent(concat!("pagurus/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn find(home: &Path, name: &str) -> Result<Peer, String> {
    load(home).into_iter().find(|p| p.name == name).ok_or_else(|| {
        let known = names(home);
        format!("no peer `{name}`; paired: {}", if known.is_empty() { "none (pagurus peer add)".into() } else { known.join(", ") })
    })
}

/// A signed POST to a peer; the answer must carry a valid signature too.
fn call(home: &Path, peer: &Peer, path: &str, body: &str, timeout: Duration) -> Result<Value, String> {
    if peer.url.is_empty() {
        return Err(format!("peer `{}` has no address: it can ask this pagurus, not the other way round", peer.name));
    }
    let key = secret(home, &peer.name).ok_or_else(|| format!("no key for peer `{}`; pair again", peer.name))?;
    let (time, nonce) = (now().to_string(), hex(&random(16)?));
    let sig = sign(&key, &[b"req", b"POST", path.as_bytes(), time.as_bytes(), nonce.as_bytes(), body.as_bytes()]);
    let url = format!("{}{path}", peer.url.trim_end_matches('/'));
    let res = http(timeout)
        .post(&url)
        .set("Content-Type", "application/json")
        .set("X-Pagurus-Peer", &peer.me_as)
        .set("X-Pagurus-Time", &time)
        .set("X-Pagurus-Nonce", &nonce)
        .set("X-Pagurus-Sig", &sig)
        .send_string(body);
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("peer `{}` unreachable: {}", peer.name, one_line(&e.to_string(), 160))),
    };
    let status = resp.status();
    let sig = resp.header("X-Pagurus-Sig").unwrap_or("").to_string();
    let mut buf = Vec::new();
    resp.into_reader().take(256 * 1024).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&buf).unwrap_or_default();
    if !verify(&key, &[b"resp", nonce.as_bytes(), status.to_string().as_bytes(), &buf], &sig) {
        let why = v["error"].as_str().map(|e| format!(" (it said: {})", one_line(e, 120))).unwrap_or_default();
        return Err(format!("peer `{}` answered without a valid signature{why}", peer.name));
    }
    if status != 200 {
        return Err(format!("peer `{}`: {}", peer.name, v["error"].as_str().unwrap_or("error")));
    }
    Ok(v)
}

/// Run `task` on peer `name` and return its report (the `ask_peer` tool, board tasks for peers).
pub fn ask(home: &Path, cfg: &Config, name: &str, task: &str) -> Result<String, String> {
    let peer = find(home, name)?;
    let task = redact(task.trim());
    if task.is_empty() {
        return Err("task is empty".into());
    }
    if task.len() > MAX_TASK {
        return Err("task is longer than 32 KB; put details in a file the peer can read".into());
    }
    eprintln!("  ⇄ asking peer {name}: {}", one_line(&task, 100));
    let v = call(home, &peer, "/ask", &json!({ "task": task }).to_string(), Duration::from_secs(cfg.peer.timeout_minutes.max(1) * 60))?;
    Ok(clip(&redact(v["report"].as_str().unwrap_or("").trim()), 8000))
}

pub fn ping(home: &Path, peer: &Peer) -> Result<String, String> {
    let v = call(home, peer, "/ping", "{}", Duration::from_secs(5))?;
    let busy = v["busy"].as_u64().unwrap_or(0);
    Ok(format!("{} v{}{}", v["name"].as_str().unwrap_or("?"), v["version"].as_str().unwrap_or("?"), if busy > 0 { " (busy)" } else { "" }))
}

/// `pagurus peer pair`: a one-time code for another pagurus's `pagurus peer add`.
pub fn pair_code(home: &Path, cfg: &Config) -> Result<String, String> {
    let r = random(4)?;
    let code = format!("{:06}", u32::from_be_bytes([r[0], r[1], r[2], r[3]]) % 1_000_000);
    crate::util::write_private(&home.join("pairing-secret.json"), &json!({ "code": code, "expires": now() + PAIR_SECS, "tries": 0 }).to_string())?;
    let url = if !cfg.peer.url.is_empty() { cfg.peer.url.clone() } else { format!("http://{}", if cfg.peer.listen.is_empty() { "<this machine>:7717" } else { &cfg.peer.listen }) };
    let mut s = format!(
        "Pairing code: {code} (valid {} min, one use)\nOn the other pagurus run:\n  pagurus peer add {} {url} {code}",
        PAIR_SECS / 60,
        self_name(cfg)
    );
    if cfg.peer.listen.is_empty() {
        s += "\n\nNote: peer.listen is not set in config.toml, so this pagurus doesn't accept peers yet. Set it (and peer.url), then restart `pagurus daemon`.";
    }
    Ok(s)
}

/// `pagurus peer add <name> <url> <code>`: pair with a pagurus that showed `code`.
pub fn add(home: &Path, cfg: &Config, name: &str, url: &str, code: &str) -> Result<String, String> {
    valid_name(name)?;
    let url = url.trim_end_matches('/');
    if !valid_url(url) {
        return Err("the url must start with http:// or https://".into());
    }
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err("the code is the 6 digits `pagurus peer pair` showed on the other machine".into());
    }
    let rng = ring::rand::SystemRandom::new();
    let mine = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).map_err(|_| "key generation failed")?;
    let pub_a = mine.compute_public_key().map_err(|_| "key generation failed")?.as_ref().to_vec();
    let me = self_name(cfg);
    let my_url = cfg.peer.url.trim().to_string();
    // `you_as`: what this pagurus calls the other one, so it can sign its own requests back
    let mac = sign(code.as_bytes(), &[b"pair1", &pub_a, me.as_bytes(), my_url.as_bytes(), name.as_bytes()]);
    let body = json!({ "name": me, "url": my_url, "pub": hex(&pub_a), "mac": mac, "you_as": name });
    let resp = http(Duration::from_secs(15)).post(&format!("{url}/pair")).send_json(body);
    let v: Value = match resp {
        Ok(r) => r.into_json().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(_, r)) => {
            let v: Value = r.into_json().unwrap_or_default();
            return Err(format!("pairing refused: {}", v["error"].as_str().unwrap_or("?")));
        }
        Err(e) => return Err(format!("can't reach {url}: {e}")),
    };
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let pub_b = unhex(&s("pub")).ok_or("bad answer")?;
    let key = agreement::agree_ephemeral(mine, &agreement::UnparsedPublicKey::new(&agreement::X25519, &pub_b), |sh| derive(sh, code, &pub_a, &pub_b))
        .map_err(|_| "bad key in the answer")?;
    let me_as = s("as");
    if valid_name(&me_as).is_err() || !verify(&key, &[b"pair2", &pub_a, &pub_b, me_as.as_bytes(), s("name").as_bytes()], &s("mac")) {
        return Err("the answer didn't prove knowledge of the code; not paired".into());
    }
    store(home, Peer { name: name.into(), url: url.into(), me_as: me_as.clone(), added: now() }, &key)?;
    let peer = find(home, name)?;
    let check = ping(home, &peer).map(|p| format!("reachable: {p}")).unwrap_or_else(|e| e);
    Ok(format!("paired with `{name}` (it calls this pagurus `{me_as}`) · {check}"))
}

/// Peers and whether they answer (pinged side by side).
pub fn list_text(home: &Path, cfg: &Config) -> String {
    let peers = load(home);
    let mut s = if cfg.peer.listen.is_empty() {
        "This pagurus doesn't accept peer tasks (peer.listen unset).".to_string()
    } else {
        format!("This pagurus: `{}`, accepting peer tasks on {}.", self_name(cfg), cfg.peer.listen)
    };
    if peers.is_empty() {
        return s + "\nNo peers yet: `pagurus peer pair` on one machine, `pagurus peer add <name> <url> <code>` on the other.";
    }
    let lines: Vec<String> = std::thread::scope(|sc| {
        let hs: Vec<_> = peers
            .iter()
            .map(|p| {
                sc.spawn(move || {
                    let state = if p.url.is_empty() {
                        "can ask this pagurus (no address to ask it back)".to_string()
                    } else {
                        match ping(home, p) {
                            Ok(x) => format!("✓ {x}"),
                            Err(e) => format!("✗ {e}"),
                        }
                    };
                    format!("- {} · {} · {state}", p.name, if p.url.is_empty() { "-" } else { &p.url })
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    s += "\n";
    s + &lines.join("\n")
}

/// `pagurus peer …`
pub fn cli(home: &Path, cfg: &Config, args: &[String]) -> Result<String, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] | ["list"] => Ok(list_text(home, cfg)),
        ["pair"] => pair_code(home, cfg),
        ["add", name, url, code] => add(home, cfg, name, url, code),
        ["remove" | "rm", name] => remove(home, name),
        ["ask", name, task @ ..] if !task.is_empty() => ask(home, cfg, name, &task.join(" ")),
        _ => Err("usage: pagurus peer [list] · pair · add <name> <url> <code> · remove <name> · ask <name> <task…>".into()),
    }
}

/// One line for `pagurus doctor`.
pub fn doctor_line(home: &Path, cfg: &Config) -> String {
    let n = load(home).len();
    let missing = load(home).iter().filter(|p| secret(home, &p.name).is_none()).count();
    let listen = if cfg.peer.listen.is_empty() {
        "not accepting tasks".to_string()
    } else if cfg.peer.listen.starts_with("0.0.0.0") || cfg.peer.listen.starts_with("[::]") {
        format!("listening on all interfaces ({}): keep that port off the internet", cfg.peer.listen)
    } else {
        format!("listening on {}", cfg.peer.listen)
    };
    format!(
        "{}peers      `{}` · {n} paired{} · {listen}",
        if missing > 0 { "!! " } else { "   " },
        self_name(cfg),
        if missing > 0 { format!(" ({missing} without a key: pair again)") } else { String::new() }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("pagurus-peer-{tag}-{}", crate::util::new_id()));
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    fn hooks() -> Arc<Hooks> {
        Arc::new(Hooks { notify: Box::new(|m| eprintln!("notify: {m}")), approve: Box::new(|_| false), can_ask: false })
    }

    #[test]
    fn signatures_bind_every_part() {
        let k = [7u8; 32];
        let sig = sign(&k, &[b"req", b"POST", b"/ask", b"body"]);
        assert!(verify(&k, &[b"req", b"POST", b"/ask", b"body"], &sig));
        assert!(!verify(&k, &[b"req", b"POST", b"/ask", b"bodx"], &sig));
        assert!(!verify(&k, &[b"req", b"POST", b"/as", b"kbody"], &sig), "parts can't be shifted");
        assert!(!verify(&[8u8; 32], &[b"req", b"POST", b"/ask", b"body"], &sig));
        assert!(!verify(&k, &[b"x"], "zz"));
        assert_eq!(unhex(&hex(&[0, 255, 16])).unwrap(), vec![0, 255, 16]);
    }

    #[test]
    fn requests_must_be_signed_fresh_and_unique() {
        let h = home("auth");
        crate::util::set_env_value(&h, &key_var("lap"), &hex(&[1u8; 32])).unwrap();
        let signed = |time: u64, nonce: &str, key: &[u8]| -> Headers {
            let t = time.to_string();
            let sig = sign(key, &[b"req", b"POST", b"/ask", t.as_bytes(), nonce.as_bytes(), b"{}"]);
            vec![
                ("x-pagurus-peer".into(), "lap".into()),
                ("x-pagurus-time".into(), t),
                ("x-pagurus-nonce".into(), nonce.into()),
                ("x-pagurus-sig".into(), sig),
            ]
        };
        let n1 = hex(&random(16).unwrap());
        let ok = signed(now(), &n1, &[1u8; 32]);
        assert!(authenticate(&h, &ok, "POST", "/ask", b"{}").is_ok());
        assert_eq!(authenticate(&h, &ok, "POST", "/ask", b"{}").unwrap_err(), "replayed request");
        let n2 = hex(&random(16).unwrap());
        assert_eq!(authenticate(&h, &signed(now(), &n2, &[1u8; 32]), "POST", "/ask", b"{\"x\":1}").unwrap_err(), "bad signature");
        assert!(authenticate(&h, &signed(now() - 600, &n2, &[1u8; 32]), "POST", "/ask", b"{}").unwrap_err().contains("timestamp"));
        assert_eq!(authenticate(&h, &signed(now(), &n2, &[2u8; 32]), "POST", "/ask", b"{}").unwrap_err(), "bad signature");
        let mut stranger = signed(now(), &n2, &[1u8; 32]);
        stranger[0].1 = "eve".into();
        assert_eq!(authenticate(&h, &stranger, "POST", "/ask", b"{}").unwrap_err(), "unknown peer");
        assert!(authenticate(&h, &signed(now(), &n2, &[1u8; 32]), "POST", "/ask", b"{}").is_ok(), "the untouched one still works");
    }

    #[test]
    fn oversized_requests_are_refused() {
        let big = format!("POST /ask HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(read_request(&mut big.as_bytes()).err().unwrap().0, 413);
        let long = format!("POST /ask HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(9000));
        assert_eq!(read_request(&mut long.as_bytes()).err().unwrap().0, 400);
    }

    /// Two pagurus agents on 127.0.0.1: pair with a code, then one runs a task for the other
    /// with its own (mock) model; a board task assigned to the peer goes the same way.
    #[test]
    fn two_agents_pair_and_work_together() {
        let (url, log) = crate::agent::tests::mock(|b| {
            let sys = b["messages"][0]["content"].as_str().unwrap_or("");
            let text = if sys.contains("another pagurus") { "uptime on B: 3 days" } else { "?" };
            json!({"role":"assistant","content": text})
        });
        let cfg_for = |listen: &str, name: &str| {
            let mut c = Config::default();
            c.provider.model = "test".into();
            c.provider.base_url = url.clone();
            c.provider.api_key_env = "PAGURUS_TEST_NO_KEY".into();
            c.peer.listen = listen.into();
            c.peer.name = name.into();
            c
        };
        let (ha, hb) = (home("a"), home("b"));
        let mut cb = cfg_for("127.0.0.1:0", "bee");
        let addr = serve(hb.clone(), cb.clone(), hooks()).unwrap().unwrap();
        cb.peer.url = format!("http://{addr}");
        let ca = cfg_for("", "ant");

        // a wrong code is refused and counts against the code
        let shown = pair_code(&hb, &cb).unwrap();
        let code = shown.split_whitespace().nth(2).unwrap().to_string();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(add(&ha, &ca, "bee", &cb.peer.url, wrong).unwrap_err().contains("wrong pairing code"));
        let msg = add(&ha, &ca, "bee", &cb.peer.url, &code).unwrap();
        assert!(msg.contains("it calls this pagurus `ant`") && msg.contains("reachable: bee"), "{msg}");
        assert!(add(&ha, &ca, "bee2", &cb.peer.url, &code).is_err(), "the code is one-time");
        assert_eq!(names(&hb), vec!["ant"]);
        assert_eq!(secret(&ha, "bee"), secret(&hb, "ant"), "both sides derived the same key");
        assert_eq!(load(&hb)[0].me_as, "bee", "B signs its own requests as the name A gave it");

        let report = ask(&ha, &ca, "bee", "how long has B been up?").unwrap();
        assert_eq!(report, "uptime on B: 3 days");
        let sent = log.lock().unwrap().last().unwrap().to_string();
        assert!(sent.contains("how long has B been up?"), "B's own model ran the task");

        // a board task for the peer
        let mut ca = ca;
        ca.peer.known = names(&ha);
        let id = crate::board::with(&ha, |b| b.create("B's uptime", "report it", "bee", &[], 1, now())).unwrap().unwrap();
        let task = crate::board::with(&ha, |b| b.claim("t", now(), |_| true)).unwrap().unwrap();
        assert_eq!(task.id, id);
        let r = crate::board::run_task(&ha, &ca, &hooks(), &task, "B's uptime, please").unwrap();
        assert!(r.contains("3 days"));

        // a forged peer gets nothing
        let fake = Peer { name: "bee".into(), url: cb.peer.url.clone(), me_as: "ant".into(), added: 0 };
        crate::util::set_env_value(&ha, &key_var("bee"), &hex(&[9u8; 32])).unwrap();
        assert!(call(&ha, &fake, "/ping", "{}", Duration::from_secs(5)).unwrap_err().contains("without a valid signature"));
        assert!(remove(&ha, "bee").is_ok() && secret(&ha, "bee").is_none());
    }
}

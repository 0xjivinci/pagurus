//! MCP (Model Context Protocol) client: tools from external servers.
//!
//! Two transports, both blocking: stdio (a child process speaking
//! newline-delimited JSON-RPC 2.0; a reader thread feeds a channel so every
//! request has a timeout) and Streamable HTTP (POST JSON-RPC, the answer is
//! JSON or a text/event-stream; `Mcp-Session-Id` is kept). Servers start on
//! first use (the first tool list of a session) with a short timeout, their
//! tool lists are cached, and a crashed stdio server is restarted once on its
//! next call. Tools reach the model as `mcp_<server>_<tool>` with schemas cut
//! down to what every OpenAI-compatible provider accepts.
//!
//! Not supported, on purpose: OAuth (use a token in an env var), sampling and
//! elicitation (a server must not drive the model or the user on its own),
//! resources/prompts (tools cover what a small agent needs), the legacy
//! HTTP+SSE transport.

use crate::config::Config;
use crate::tools::Ctx;
use crate::util::{clip, one_line};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "2025-06-18";
/// Connect + initialize + tools/list. `npx -y` fetching a package the first time is the slow case.
const STARTUP_SECS: u64 = 30;
const DESC_CHARS: usize = 300;
const PARAM_DESC_CHARS: usize = 150;
const SCHEMA_DEPTH: usize = 6;
const OUT_CHARS: usize = 10_000;
const MAX_NAME: usize = 64;

/// One `[[mcp]]` entry in config.toml.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Server {
    /// Letters, digits, `-`, `_`; tools appear as `mcp_<name>_<tool>`.
    pub name: String,
    /// stdio: the program to start (with `args`).
    pub command: String,
    pub args: Vec<String>,
    /// Streamable HTTP: the endpoint URL.
    pub url: String,
    /// HTTP: env var holding a bearer token (never the token itself).
    pub bearer_env: String,
    /// stdio: names of env vars passed to the server (besides PATH, HOME, LANG …).
    pub env: Vec<String>,
    pub enabled: bool,
    /// Only these tools (names or `*` globs; empty = all) …
    pub allow: Vec<String>,
    /// … minus these.
    pub deny: Vec<String>,
    /// Tools that run without asking (names or globs).
    pub read_only: Vec<String>,
    /// `always` (default): ask before every call except `read_only` tools.
    /// `writes`: also trust the server's own readOnlyHint. `never`: never ask.
    pub approve: String,
    pub timeout_secs: u64,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: vec![],
            url: String::new(),
            bearer_env: String::new(),
            env: vec![],
            enabled: true,
            allow: vec![],
            deny: vec![],
            read_only: vec![],
            approve: "always".into(),
            timeout_secs: 60,
        }
    }
}

impl Server {
    fn kind(&self) -> String {
        if self.url.is_empty() {
            format!("stdio: {} {}", self.command, self.args.join(" ")).trim_end().to_string()
        } else {
            format!("http: {}", self.url)
        }
    }
}

/// Config checks, run by `config::load`.
pub fn check(list: &[Server]) -> Result<(), String> {
    for (i, s) in list.iter().enumerate() {
        let n = &s.name;
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("[[mcp]] name `{n}`: use letters, digits, - or _"));
        }
        if list[..i].iter().any(|o| o.name == *n) {
            return Err(format!("[[mcp]] `{n}` is listed twice"));
        }
        if s.command.is_empty() == s.url.is_empty() {
            return Err(format!("[[mcp]] `{n}`: set either command (stdio) or url (HTTP)"));
        }
        if !s.url.is_empty() && !s.url.starts_with("http://") && !s.url.starts_with("https://") {
            return Err(format!("[[mcp]] `{n}`: url must start with http:// or https://"));
        }
        if !s.bearer_env.is_empty() && s.url.starts_with("http://") && !local(&s.url) {
            return Err(format!("[[mcp]] `{n}`: a bearer token is only sent over https (or to localhost)"));
        }
        if !matches!(s.approve.as_str(), "always" | "writes" | "never") {
            return Err(format!("[[mcp]] `{n}`: approve must be always, writes or never"));
        }
    }
    Ok(())
}

fn local(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or("");
    ["localhost:", "localhost/", "127.0.0.1:", "127.0.0.1/", "[::1]"].iter().any(|p| rest.starts_with(p))
}

// ---------- names, filters, schemas ----------

/// `*` matches any run of characters; everything else is literal.
fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

fn any_glob(pats: &[String], s: &str) -> bool {
    pats.iter().any(|p| glob(p, s))
}

fn wanted(s: &Server, tool: &str) -> bool {
    (s.allow.is_empty() || any_glob(&s.allow, tool)) && !any_glob(&s.deny, tool)
}

/// `mcp_<server>_<tool>`, only `[A-Za-z0-9_-]`, at most 64 chars (OpenAI's
/// limit); a long name keeps its start and gets a stable hash suffix.
pub fn tool_name(server: &str, tool: &str) -> String {
    let n: String = format!("mcp_{server}_{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if n.len() <= MAX_NAME {
        return n;
    }
    let h = n.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
    format!("{}_{h:08x}", &n[..MAX_NAME - 9])
}

/// A JSON schema reduced to the subset OpenAI, Gemini, OpenRouter and local
/// servers all accept: local `$ref`s inlined, `oneOf`→`anyOf`, nullable unions
/// collapsed, `const`→`enum`, non-string enums moved into the description,
/// `required` pruned to real properties, everything else (`$schema`,
/// `additionalProperties`, `default`, `format`, `title` …) dropped.
pub fn clean_schema(schema: &Value) -> Value {
    let mut v = clean(schema, schema, 0);
    if !v.is_object() || v["type"] != "object" {
        v = json!({"type": "object", "properties": {}});
    }
    if v.get("properties").is_none() {
        v["properties"] = json!({});
    }
    if v.get("required").is_none() {
        v["required"] = json!([]);
    }
    v
}

fn clean(node: &Value, root: &Value, depth: usize) -> Value {
    let Some(o) = node.as_object() else { return json!({}) };
    // inline a local $ref (depth-limited, so recursive schemas end)
    if let Some(r) = o.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix("#/$defs/")
            .map(|k| &root["$defs"][k])
            .or_else(|| r.strip_prefix("#/definitions/").map(|k| &root["definitions"][k]));
        let mut v = match target {
            Some(t) if t.is_object() && depth < SCHEMA_DEPTH => clean(t, root, depth + 1),
            _ => json!({}),
        };
        if let (Some(d), Some(m)) = (o.get("description"), v.as_object_mut()) {
            m.insert("description".into(), d.clone());
            return clean_desc(v);
        }
        return v;
    }
    let mut out = serde_json::Map::new();
    // a single allOf member (a common wrapper) is merged in
    let mut o = o.clone();
    if let Some(Value::Array(all)) = o.remove("allOf") {
        for a in all.iter().filter_map(Value::as_object) {
            for (k, v) in a {
                o.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    if let Some(d) = o.get("description").and_then(Value::as_str) {
        out.insert("description".into(), json!(d));
    }
    let union = o.get("anyOf").or_else(|| o.get("oneOf")).and_then(Value::as_array);
    if let Some(branches) = union {
        let kept: Vec<Value> =
            branches.iter().filter(|b| b["type"] != "null").map(|b| clean(b, root, depth + 1)).collect();
        match kept.len() {
            0 => {}
            1 => {
                for (k, v) in kept.into_iter().next().unwrap().as_object().cloned().unwrap_or_default() {
                    out.entry(k).or_insert(v);
                }
            }
            _ => {
                out.insert("anyOf".into(), Value::Array(kept));
            }
        }
        return clean_desc(Value::Object(out));
    }
    let mut ty = match o.get("type") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(ts)) => {
            ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("string").to_string()
        }
        _ if o.contains_key("properties") => "object".into(),
        _ if o.contains_key("items") => "array".into(),
        _ if o.contains_key("enum") || o.contains_key("const") => "string".into(),
        _ => String::new(),
    };
    let mut values: Vec<Value> = match (o.get("enum"), o.get("const")) {
        (Some(Value::Array(e)), _) => e.clone(),
        (_, Some(c)) => vec![c.clone()],
        _ => vec![],
    };
    values.retain(|v| !v.is_null());
    if !values.is_empty() {
        if ty == "string" && values.iter().all(Value::is_string) {
            out.insert("enum".into(), Value::Array(values));
        } else {
            // Gemini takes enums of strings only: say it in words instead
            let list: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            let d = out.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            out.insert("description".into(), json!(format!("{d} One of: {}.", list.join(", ")).trim().to_string()));
        }
    }
    match ty.as_str() {
        "object" if depth < SCHEMA_DEPTH => {
            let mut props = serde_json::Map::new();
            if let Some(p) = o.get("properties").and_then(Value::as_object) {
                for (k, v) in p {
                    props.insert(k.clone(), clean(v, root, depth + 1));
                }
            }
            let req: Vec<Value> = o
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter(|n| n.as_str().is_some_and(|n| props.contains_key(n))).cloned().collect())
                .unwrap_or_default();
            out.insert("properties".into(), Value::Object(props));
            if !req.is_empty() {
                out.insert("required".into(), Value::Array(req));
            }
        }
        "array" => {
            let items = match o.get("items") {
                Some(i @ Value::Object(_)) if depth < SCHEMA_DEPTH => clean(i, root, depth + 1),
                _ => json!({}),
            };
            // providers (Gemini) insist on a typed `items`
            let items = if items.get("type").is_none() && items.get("anyOf").is_none() {
                json!({"type": "string"})
            } else {
                items
            };
            out.insert("items".into(), items);
        }
        "integer" | "number" => {
            for k in ["minimum", "maximum"] {
                if let Some(n) = o.get(k).filter(|n| n.is_number()) {
                    out.insert(k.into(), n.clone());
                }
            }
        }
        "string" | "boolean" | "object" => {}
        _ => ty.clear(),
    }
    if !ty.is_empty() {
        out.insert("type".into(), json!(ty));
    }
    clean_desc(Value::Object(out))
}

fn clean_desc(mut v: Value) -> Value {
    if let Some(d) = v.get("description").and_then(Value::as_str) {
        let d = one_line(d, PARAM_DESC_CHARS);
        v["description"] = json!(d);
    }
    v
}

// ---------- JSON-RPC over the two transports ----------

/// What came back for request `id`, if `v` is that answer.
fn answer(v: &Value, id: u64) -> Option<Result<Value, String>> {
    if v.get("method").is_some() || v["id"].as_u64() != Some(id) {
        return None;
    }
    Some(match v.get("error") {
        Some(e) => Err(format!("server error: {} (code {})", e["message"].as_str().unwrap_or("?"), e["code"])),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    })
}

/// Our reply when the server sends us a request (pings and roots get answers;
/// sampling, elicitation and the rest are declined).
fn reply_to(v: &Value) -> Option<Value> {
    let (Some(m), Some(id)) = (v["method"].as_str(), v.get("id")) else { return None };
    Some(match m {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": []}}),
        _ => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("pagurus does not support {m}")}})
        }
    })
}

struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    fn spawn(s: &Server) -> Result<Proc, String> {
        let mut c = Command::new(crate::util::expand(&s.command));
        c.args(&s.args).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // a clean environment: the basics a program needs, plus what the user listed
        let basics = [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TMPDIR",
            "TERM",
            "SYSTEMROOT",
            "USERPROFILE",
            "APPDATA",
        ];
        for k in basics.iter().copied().chain(s.env.iter().map(String::as_str)) {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        let ws = crate::util::home_dir().join("workspace");
        if ws.is_dir() {
            c.current_dir(ws);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0); // so the whole tree can be stopped; Ctrl-C in the terminal doesn't reach it
        }
        let mut child = c.spawn().map_err(|e| format!("cannot start `{}`: {e}", s.command))?;
        let stdin = child.stdin.take();
        let (tx, lines) = channel();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match out.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim().to_string();
                        // only requests and answers carry an id: notifications (logs, progress) between
                        // calls would otherwise pile up until the next request reads them
                        if line.contains("\"id\"") && tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let (sink, mut err) = (stderr.clone(), child.stderr.take().unwrap());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut t) = sink.lock() {
                    t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if t.len() > 4000 {
                        let cut = t.len() - 2000;
                        let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                        t.drain(..cut);
                    }
                }
            }
        });
        Ok(Proc { child, stdin, lines, stderr })
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        let w = self.stdin.as_mut().ok_or("server stdin closed")?;
        let mut line = v.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).and_then(|_| w.flush()).map_err(|_| self.died())
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn died(&mut self) -> String {
        let code = match self.child.try_wait() {
            Ok(Some(s)) => format!("server exited ({s})"),
            _ => "server closed its output".into(),
        };
        let tail = self.stderr.lock().map(|t| t.trim().to_string()).unwrap_or_default();
        if tail.is_empty() {
            code
        } else {
            format!("{code}; stderr: {}", crate::util::tail(&tail, 600))
        }
    }

    fn request(&mut self, id: u64, msg: &Value, timeout: Duration) -> Result<Value, String> {
        self.send(msg)?;
        let deadline = Instant::now() + timeout;
        loop {
            if crate::util::cancelled() {
                self.cancel(id, "stopped by the user");
                return Err("stopped by the user".into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.cancel(id, "timeout");
                return Err(format!("no answer within {}s", timeout.as_secs()));
            }
            match self.lines.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(line) => {
                    // servers that log to stdout: anything that isn't JSON-RPC is skipped
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if let Some(r) = answer(&v, id) {
                        return r;
                    }
                    if let Some(rep) = reply_to(&v) {
                        self.send(&rep)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(50)); // let it finish exiting for the status
                    return Err(self.died());
                }
            }
        }
    }

    fn cancel(&mut self, id: u64, why: &str) {
        let _ = self.send(
            &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": why}}),
        );
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stdin = None; // EOF: the polite way to ask a stdio server to exit
        for _ in 0..10 {
            if !self.alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        #[cfg(unix)]
        let _ =
            Command::new("sh").arg("-c").arg(format!("kill -9 -{}", self.child.id())).stderr(Stdio::null()).status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Http {
    agent: ureq::Agent,
    url: String,
    token: Option<String>,
    session: Option<String>,
    version: Option<String>,
}

const EXPIRED: &str = "session expired";

impl Http {
    fn new(s: &Server) -> Result<Http, String> {
        let token = match s.bearer_env.as_str() {
            "" => None,
            e => Some(
                std::env::var(e)
                    .ok()
                    .filter(|t| !t.trim().is_empty())
                    .ok_or(format!("${e} (bearer_env) is not set"))?,
            ),
        };
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build();
        Ok(Http { agent, url: s.url.clone(), token, session: None, version: None })
    }

    fn post(&self, v: &Value, timeout: Duration) -> Result<ureq::Response, String> {
        let mut r = self
            .agent
            .post(&self.url)
            .timeout(timeout)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(t) = &self.token {
            r = r.set("Authorization", &format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            r = r.set("Mcp-Session-Id", s);
        }
        if let Some(p) = &self.version {
            r = r.set("MCP-Protocol-Version", p);
        }
        match r.send_string(&v.to_string()) {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(404, _)) if self.session.is_some() => Err(EXPIRED.into()),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let hint = if code == 401 || code == 403 { " (check bearer_env; OAuth is not supported)" } else { "" };
                Err(format!("HTTP {code}{hint}: {}", one_line(&body, 200)))
            }
            // the URL can carry a key: say what failed without it
            Err(ureq::Error::Transport(t)) => Err(format!("cannot reach the server: {}", t.kind())),
        }
    }

    fn notify(&self, v: &Value) {
        let _ = self.post(v, Duration::from_secs(10));
    }

    fn request(&mut self, id: u64, msg: &Value, timeout: Duration) -> Result<Value, String> {
        let resp = self.post(msg, timeout)?;
        if let Some(s) = resp.header("mcp-session-id") {
            self.session = Some(s.to_string());
        }
        if resp.content_type() == "text/event-stream" {
            let mut data = String::new();
            for line in BufReader::new(resp.into_reader()).lines() {
                let line = line.map_err(|e| format!("stream broke: {e}"))?;
                if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                    data.push('\n');
                } else if line.is_empty() && !data.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(&std::mem::take(&mut data)) {
                        if let Some(r) = answer(&v, id) {
                            return r;
                        }
                        if let Some(rep) = reply_to(&v) {
                            self.notify(&rep);
                        }
                    }
                }
            }
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if let Some(r) = answer(&v, id) {
                    return r;
                }
            }
            return Err("the event stream ended without an answer".into());
        }
        let body = resp.into_string().map_err(|e| format!("reading the answer: {e}"))?;
        let v: Value = serde_json::from_str(&body).map_err(|_| format!("not JSON: {}", one_line(&body, 120)))?;
        let found = match &v {
            Value::Array(batch) => batch.iter().find_map(|m| answer(m, id)),
            m => answer(m, id),
        };
        found.unwrap_or_else(|| Err(format!("unexpected answer: {}", one_line(&body, 120))))
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            let mut r = self.agent.delete(&self.url).timeout(Duration::from_secs(2)).set("Mcp-Session-Id", s);
            if let Some(t) = &self.token {
                r = r.set("Authorization", &format!("Bearer {t}"));
            }
            let _ = r.call();
        }
    }
}

enum Link {
    Stdio(Proc),
    Http(Http),
}

// ---------- a connection to one server ----------

#[derive(Clone, Debug)]
struct Tool {
    /// The server's name for it.
    name: String,
    /// What the model calls it.
    exposed: String,
    desc: String,
    schema: Value,
    /// The server marks it read-only (annotations.readOnlyHint).
    hint_ro: bool,
}

pub struct Conn {
    cfg: Server,
    link: Option<Link>,
    tools: Vec<Tool>,
    hidden: usize,
    next: u64,
    info: String,
    err: String,
    fails: u32,
    retry_at: Option<Instant>,
}

impl Conn {
    fn new(cfg: Server) -> Conn {
        Conn {
            cfg,
            link: None,
            tools: vec![],
            hidden: 0,
            next: 0,
            info: String::new(),
            err: String::new(),
            fails: 0,
            retry_at: None,
        }
    }

    fn rpc(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.link.as_mut().ok_or("not connected")? {
            Link::Stdio(p) => p.request(id, &msg, timeout),
            Link::Http(h) => h.request(id, &msg, timeout),
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match self.link.as_mut() {
            Some(Link::Stdio(p)) => {
                let _ = p.send(&msg);
            }
            Some(Link::Http(h)) => h.notify(&msg),
            None => {}
        }
    }

    fn alive(&mut self) -> bool {
        match &mut self.link {
            Some(Link::Stdio(p)) => p.alive(),
            Some(Link::Http(_)) => true,
            None => false,
        }
    }

    /// (Re)start the server, handshake and fetch its tools.
    fn connect(&mut self, timeout: Duration) -> Result<(), String> {
        self.link = None;
        let r = self.handshake(timeout);
        match &r {
            Ok(()) => {
                self.err.clear();
                self.fails = 0;
                self.retry_at = None;
            }
            Err(e) => {
                self.link = None;
                self.err = e.clone();
                self.fails += 1;
                // 2, 4, 8 … minutes, at most an hour: a broken server must not slow every turn
                self.retry_at = Some(Instant::now() + Duration::from_secs((60u64 << self.fails.min(6)).min(3600)));
            }
        }
        r
    }

    fn handshake(&mut self, timeout: Duration) -> Result<(), String> {
        let t0 = Instant::now();
        self.link = Some(if self.cfg.url.is_empty() {
            Link::Stdio(Proc::spawn(&self.cfg)?)
        } else {
            Link::Http(Http::new(&self.cfg)?)
        });
        let init = self.rpc(
            "initialize",
            json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "pagurus", "version": env!("CARGO_PKG_VERSION")}}),
            timeout,
        )?;
        let version = init["protocolVersion"].as_str().unwrap_or(PROTOCOL).to_string();
        if let Some(Link::Http(h)) = self.link.as_mut() {
            h.version = Some(version.clone());
        }
        let si = &init["serverInfo"];
        self.info = format!(
            "{} {} (protocol {version})",
            si["name"].as_str().unwrap_or("?"),
            si["version"].as_str().unwrap_or("")
        );
        self.notify("notifications/initialized");
        let (mut tools, mut cursor, mut hidden) = (Vec::new(), None::<String>, 0);
        for _ in 0..20 {
            let left = timeout.saturating_sub(t0.elapsed()).max(Duration::from_secs(2));
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.rpc("tools/list", params, left)?;
            for t in page["tools"].as_array().into_iter().flatten() {
                let Some(name) = t["name"].as_str() else { continue };
                if !wanted(&self.cfg, name) {
                    hidden += 1;
                    continue;
                }
                let desc = one_line(t["description"].as_str().unwrap_or(t["title"].as_str().unwrap_or("")), DESC_CHARS);
                tools.push(Tool {
                    name: name.to_string(),
                    exposed: tool_name(&self.cfg.name, name),
                    desc: if desc.is_empty() { format!("{name} (MCP server {})", self.cfg.name) } else { desc },
                    schema: clean_schema(&t["inputSchema"]),
                    hint_ro: t["annotations"]["readOnlyHint"] == true,
                });
            }
            cursor = page["nextCursor"].as_str().filter(|c| !c.is_empty()).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        // truncated names could collide: keep the first
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.exposed.clone()));
        self.tools = tools;
        self.hidden = hidden;
        Ok(())
    }

    fn due(&self) -> bool {
        self.link.is_none() && self.retry_at.is_none_or(|t| Instant::now() >= t)
    }

    /// Call a tool; a stdio server that died since is restarted once first.
    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, String> {
        let timeout = Duration::from_secs(self.cfg.timeout_secs.max(1));
        if !self.alive() {
            self.connect(Duration::from_secs(STARTUP_SECS)).map_err(|e| format!("server is down: {e}"))?;
        }
        let params = json!({"name": tool, "arguments": args});
        match self.rpc("tools/call", params.clone(), timeout) {
            Err(e) if e == EXPIRED => {
                self.connect(Duration::from_secs(STARTUP_SECS))?;
                self.rpc("tools/call", params, timeout)
            }
            r => r,
        }
    }
}

/// A tool result as text, plus any images in it as (label, data URL).
fn render(r: &Value) -> (String, Vec<(String, String)>) {
    let (mut parts, mut images) = (Vec::new(), Vec::new());
    for c in r["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or("") {
            "text" => parts.push(c["text"].as_str().unwrap_or("").to_string()),
            "image" => {
                let mime = c["mimeType"].as_str().unwrap_or("");
                let data = c["data"].as_str().unwrap_or("");
                let ok = matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
                    && data.len() <= crate::util::MAX_IMAGE_BYTES * 4 / 3;
                if ok && !data.is_empty() {
                    images.push((format!("image from MCP ({mime})"), format!("data:{mime};base64,{data}")));
                    parts.push(format!("[{mime} image, attached below]"));
                } else {
                    parts.push(format!("[{mime} image of {} KB, not shown]", data.len() * 3 / 4 / 1024));
                }
            }
            "resource" => {
                let res = &c["resource"];
                let uri = res["uri"].as_str().unwrap_or("");
                match res["text"].as_str() {
                    Some(t) => parts.push(format!("[resource {uri}]\n{t}")),
                    None => parts.push(format!(
                        "[binary resource {uri} ({}), not shown]",
                        res["mimeType"].as_str().unwrap_or("?")
                    )),
                }
            }
            "resource_link" => {
                parts.push(format!("[link: {} {}]", c["uri"].as_str().unwrap_or(""), c["name"].as_str().unwrap_or("")))
            }
            other => parts.push(format!("[{other} content, not shown]")),
        }
    }
    if parts.is_empty() {
        if let Some(s) = r.get("structuredContent") {
            parts.push(s.to_string());
        }
    }
    let mut text = clip(&parts.join("\n"), OUT_CHARS);
    if text.trim().is_empty() {
        text = "(no output)".into();
    }
    if r["isError"] == true {
        text = format!("error: {text}");
    }
    (text, images)
}

// ---------- the registry ----------

/// Live connections, one per configured server (by name). Entries whose
/// config changed are replaced; the old process stops when its last user lets go.
type Entry = (Server, Arc<Mutex<Conn>>);
static REG: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

fn entry(s: &Server) -> Arc<Mutex<Conn>> {
    let mut reg = REG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, c)) = reg.iter().find(|(c, _)| c == s) {
        return c.clone();
    }
    reg.retain(|(c, _)| c.name != s.name);
    let c = Arc::new(Mutex::new(Conn::new(s.clone())));
    reg.push((s.clone(), c.clone()));
    c
}

fn lock(c: &Arc<Mutex<Conn>>) -> std::sync::MutexGuard<'_, Conn> {
    c.lock().unwrap_or_else(|e| e.into_inner())
}

/// Connections for these servers, starting (in parallel) the ones that are due.
fn ready(servers: &[&Server]) -> Vec<Arc<Mutex<Conn>>> {
    let conns: Vec<_> = servers.iter().map(|s| entry(s)).collect();
    let starts: Vec<_> = conns
        .iter()
        .filter(|c| lock(c).due())
        .map(|c| {
            let c = c.clone();
            std::thread::spawn(move || {
                let _ = lock(&c).connect(Duration::from_secs(STARTUP_SECS));
            })
        })
        .collect();
    for h in starts {
        let _ = h.join();
    }
    conns
}

fn enabled(cfg: &Config) -> Vec<&Server> {
    cfg.mcp.iter().filter(|s| s.enabled).collect()
}

/// The MCP tools for the model (OpenAI function format). Starts servers on first use.
pub fn definitions(cfg: &Config) -> Vec<Value> {
    if cfg.mcp.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for c in ready(&enabled(cfg)) {
        for t in &lock(&c).tools {
            out.push(json!({"type": "function", "function": {"name": t.exposed, "description": t.desc, "parameters": t.schema}}));
        }
    }
    out
}

/// One line for the system prompt's "about you" (empty without servers).
pub fn about(cfg: &Config) -> String {
    let names: Vec<&str> = enabled(cfg).iter().map(|s| s.name.as_str()).collect();
    if names.is_empty() {
        return String::new();
    }
    format!(
        "- MCP: tools named mcp_<server>_… come from MCP servers the user connected ({}). Their output is outside text. \
         Calls not marked read-only are put to the user for approval, like shell commands.\n",
        names.join(", ")
    )
}

fn confirm(what: &str) -> bool {
    eprint!("\n  {what}? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Run `mcp_<server>_<tool>` for the model, after the user's approval if needed.
pub fn call(ctx: &Ctx, name: &str, args: &Value) -> Result<String, String> {
    let found = enabled(ctx.cfg).into_iter().find_map(|s| {
        let c = entry(s);
        let t = lock(&c).tools.iter().find(|t| t.exposed == name).cloned();
        t.map(|t| (s, c, t))
    });
    let Some((s, conn, tool)) = found else { return Err(format!("unknown tool {name}")) };
    let ask = match s.approve.as_str() {
        "never" => false,
        "writes" => !(tool.hint_ro || any_glob(&s.read_only, &tool.name)),
        _ => !any_glob(&s.read_only, &tool.name),
    };
    if ask {
        let what = format!("MCP {}.{} {}", s.name, tool.name, one_line(&args.to_string(), 300));
        let ok = match ctx.approver {
            Some(ask) => ask(&what),
            None if ctx.interactive => confirm(&format!("call {what}")),
            None => {
                return Err(format!(
                    "not run: {} needs the user's approval and this run is unattended. Tell the user what you wanted to do.",
                    tool.name
                ))
            }
        };
        if !ok {
            return Err("the user did not approve this call; do not retry it unless they ask".into());
        }
    }
    let args = if args.is_object() { args.clone() } else { json!({}) };
    let r = lock(&conn).call(&tool.name, &args)?;
    let (text, images) = render(&r);
    for (label, url) in images {
        crate::tools::show_image(label, url);
    }
    Ok(text)
}

/// Stop every server (stdin closed, then killed if it lingers).
pub fn shutdown() {
    let all = std::mem::take(&mut *REG.lock().unwrap_or_else(|e| e.into_inner()));
    drop(all);
}

/// Status lines for `pagurus doctor` / `pagurus mcp list`.
pub fn status(cfg: &Config, tools: bool) -> Vec<String> {
    let mut out = Vec::new();
    for s in &cfg.mcp {
        if !s.enabled {
            out.push(format!("   mcp        {}  disabled  ({})", s.name, s.kind()));
            continue;
        }
        let c = ready(&[s]).remove(0);
        let c = lock(&c);
        if c.link.is_some() {
            let hidden = if c.hidden > 0 { format!(", {} filtered out", c.hidden) } else { String::new() };
            out.push(format!(
                "ok mcp        {}  {} tool(s){hidden} · {} · {}",
                s.name,
                c.tools.len(),
                c.info,
                s.kind()
            ));
        } else {
            out.push(format!("!! mcp        {}  down: {} · {}", s.name, one_line(&c.err, 300), s.kind()));
        }
        if tools {
            for t in &c.tools {
                let free =
                    s.approve == "never" || any_glob(&s.read_only, &t.name) || (s.approve == "writes" && t.hint_ro);
                out.push(format!(
                    "     {} {:<36} {}",
                    if free { "  " } else { "🔐" },
                    t.exposed,
                    one_line(&t.desc, 70)
                ));
            }
        }
    }
    out
}

/// `pagurus mcp list` · `pagurus mcp test <name> [tool [json-args]]`
pub fn cli(cfg: &Config, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if cfg.mcp.is_empty() {
                println!("No MCP servers. Add a [[mcp]] entry to config.toml (see the example at its end).");
            }
            for l in status(cfg, true) {
                println!("{l}");
            }
            println!("🔐 = asks before each call");
            Ok(())
        }
        Some("test") => {
            let name = args.get(1).ok_or("usage: pagurus mcp test <name> [tool [json-args]]")?;
            let s = cfg.mcp.iter().find(|s| s.name == *name).ok_or(format!("no [[mcp]] named `{name}`"))?;
            let mut c = Conn::new(s.clone());
            let t0 = Instant::now();
            c.connect(Duration::from_secs(STARTUP_SECS))?;
            println!("connected to {} in {} ms: {}", s.name, t0.elapsed().as_millis(), c.info);
            let bytes: usize = c.tools.iter().map(|t| t.schema.to_string().len() + t.desc.len()).sum();
            println!("{} tool(s), {} filtered out, ~{bytes} bytes of tool definitions", c.tools.len(), c.hidden);
            for t in &c.tools {
                println!("  {:<40} {}", t.exposed, one_line(&t.desc, 80));
            }
            if let Some(tool) = args.get(2) {
                let a: Value = serde_json::from_str(args.get(3).map(String::as_str).unwrap_or("{}"))
                    .map_err(|e| format!("args: {e}"))?;
                let t = c
                    .tools
                    .iter()
                    .find(|t| t.name == *tool || t.exposed == *tool)
                    .ok_or(format!("no tool `{tool}`"))?
                    .name
                    .clone();
                let (text, images) = render(&c.call(&t, &a)?);
                println!("\n{text}");
                if !images.is_empty() {
                    println!("({} image(s))", images.len());
                }
            }
            Ok(())
        }
        Some(x) => Err(format!("unknown `pagurus mcp {x}`; use list or test <name>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_prefixed_sanitized_and_short() {
        assert_eq!(tool_name("fs", "read_file"), "mcp_fs_read_file");
        assert_eq!(tool_name("my-srv", "a.b/c d"), "mcp_my-srv_a_b_c_d");
        let long = tool_name("server", &"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert_ne!(long, tool_name("server", &"x".repeat(99)), "a hash keeps truncated names apart");
        assert_eq!(long, tool_name("server", &"x".repeat(100)), "stable");
    }

    #[test]
    fn globs_and_filters() {
        assert!(
            glob("read_*", "read_file")
                && glob("*", "x")
                && glob("a*c*e", "abcde")
                && !glob("a*c", "abd")
                && glob("x", "x")
        );
        let s = Server {
            allow: vec!["get_*".into(), "list".into()],
            deny: vec!["get_secret".into()],
            ..Default::default()
        };
        assert!(wanted(&s, "get_page") && wanted(&s, "list"));
        assert!(!wanted(&s, "get_secret") && !wanted(&s, "delete"));
    }

    #[test]
    fn config_checks() {
        let ok = Server { name: "fs".into(), command: "npx".into(), ..Default::default() };
        assert!(check(std::slice::from_ref(&ok)).is_ok());
        assert!(check(&[ok.clone(), ok.clone()]).is_err(), "duplicate");
        assert!(check(&[Server { url: "https://x".into(), ..ok.clone() }]).is_err(), "both command and url");
        assert!(check(&[Server { name: "a b".into(), ..ok.clone() }]).is_err());
        assert!(check(&[Server { approve: "maybe".into(), ..ok.clone() }]).is_err());
        let http = Server {
            command: String::new(),
            url: "http://example.com/mcp".into(),
            bearer_env: "T".into(),
            ..ok.clone()
        };
        assert!(check(std::slice::from_ref(&http)).is_err(), "no token over plain http");
        assert!(check(&[Server { url: "http://localhost:3000/mcp".into(), ..http }]).is_ok());
        let c: Config = toml::from_str(
            "[[mcp]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"-y\", \"x\"]\nread_only = [\"read_*\"]\n",
        )
        .unwrap();
        assert_eq!(c.mcp[0].approve, "always");
        assert!(c.mcp[0].enabled);
        assert_eq!(c.mcp[0].timeout_secs, 60);
    }

    #[test]
    fn schemas_are_cleaned_for_picky_providers() {
        let s = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "where", "format": "uri", "default": "/"},
                "mode": {"anyOf": [{"type": "string", "enum": ["a", "b"]}, {"type": "null"}], "default": null},
                "level": {"type": ["integer", "null"], "enum": [1, 2, 3], "minimum": 1},
                "kind": {"const": "x"},
                "tags": {"type": "array"},
                "opts": {"$ref": "#/definitions/Opts", "description": "options"},
                "either": {"oneOf": [{"type": "string"}, {"type": "number"}]}
            },
            "required": ["path", "ghost"],
            "definitions": {"Opts": {"type": "object", "properties": {"deep": {"type": "boolean"}}, "required": ["deep"]}}
        });
        let c = clean_schema(&s);
        let text = c.to_string();
        for gone in
            ["$schema", "additionalProperties", "default", "format", "definitions", "$ref", "null", "oneOf", "const"]
        {
            assert!(!text.contains(gone), "{gone} should be gone: {text}");
        }
        let p = &c["properties"];
        assert_eq!(c["required"], json!(["path"]), "required pruned to real properties");
        assert_eq!(p["mode"], json!({"type": "string", "enum": ["a", "b"]}), "nullable union collapsed");
        assert_eq!(p["level"]["type"], "integer");
        assert!(p["level"].get("enum").is_none() && p["level"]["description"].as_str().unwrap().contains("1, 2, 3"));
        assert_eq!(p["level"]["minimum"], 1);
        assert_eq!(p["kind"], json!({"type": "string", "enum": ["x"]}));
        assert_eq!(p["tags"]["items"]["type"], "string", "arrays get typed items");
        assert_eq!(p["opts"]["properties"]["deep"]["type"], "boolean", "refs are inlined");
        assert_eq!(p["opts"]["description"], "options");
        assert_eq!(p["either"]["anyOf"].as_array().unwrap().len(), 2);
        // no schema / odd root: still an object with properties
        assert_eq!(clean_schema(&Value::Null), json!({"type": "object", "properties": {}, "required": []}));
        // recursion ends
        let r = json!({"type": "object", "properties": {"n": {"$ref": "#/$defs/N"}}, "$defs": {"N": {"type": "object", "properties": {"next": {"$ref": "#/$defs/N"}}}}});
        assert!(clean_schema(&r).to_string().len() < 2000);
    }

    #[test]
    fn results_render_text_images_and_errors() {
        let r = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "mimeType": "image/png", "data": "iVBORw0K"},
            {"type": "image", "mimeType": "image/svg+xml", "data": "PHN2Zz4="},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
            {"type": "audio", "data": ""}
        ]});
        let (t, imgs) = render(&r);
        assert!(t.starts_with("hello\n[image/png image, attached below]"));
        assert!(
            t.contains("svg+xml image") && t.contains("[resource file:///a]\nbody") && t.contains("[audio content")
        );
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].1, "data:image/png;base64,iVBORw0K");
        let (t, _) = render(&json!({"isError": true, "content": [{"type": "text", "text": "nope"}]}));
        assert_eq!(t, "error: nope");
        let (t, _) = render(&json!({"content": [], "structuredContent": {"n": 1}}));
        assert_eq!(t, r#"{"n":1}"#);
    }

    /// A stdio MCP server in POSIX sh: logs junk to stdout, pages its tool
    /// list, pings the client mid-call, and has slow, crashing and env tools.
    #[cfg(unix)]
    const MOCK: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  r() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$1"; }
  case "$line" in
    *'"method":"initialize"'*) echo 'starting mock server...'; r '{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1.0"}}';;
    *'"method":"tools/list"'*'"cursor"'*) r '{"tools":[{"name":"slow","inputSchema":{"type":"object"}},{"name":"crash"},{"name":"env"},{"name":"secret"}]}';;
    *'"method":"tools/list"'*) r '{"tools":[{"name":"echo","description":"Echo  it\nback","annotations":{"readOnlyHint":true},"inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}],"nextCursor":"p2"}';;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":"s1","method":"ping"}\n'; echo '{oops'; r '{"content":[{"type":"text","text":"hello from mock"}]}';;
    *'"name":"slow"'*) sleep 2; r '{"content":[{"type":"text","text":"late"}]}';;
    *'"name":"crash"'*) echo 'boom' >&2; exit 3;;
    *'"name":"env"'*) r "{\"content\":[{\"type\":\"text\",\"text\":\"[$MCP_TEST_PASSED][$MCP_TEST_HIDDEN]\"}]}";;
    *'"method":"notifications/'*|*'"result":{}'*) ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no such method"}}\n' "$id";;
  esac
done
"#;

    #[cfg(unix)]
    fn mock(name: &str, extra: impl FnOnce(&mut Server)) -> (Config, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pagurus-mcp-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("mock.sh");
        std::fs::write(&script, MOCK).unwrap();
        let mut s = Server {
            name: name.into(),
            command: "sh".into(),
            args: vec![script.display().to_string()],
            timeout_secs: 1,
            ..Default::default()
        };
        extra(&mut s);
        (Config { mcp: vec![s], ..Default::default() }, dir)
    }

    #[cfg(unix)]
    #[test]
    fn stdio_server_lists_calls_and_survives_trouble() {
        std::env::set_var("MCP_TEST_PASSED", "yes");
        std::env::set_var("MCP_TEST_HIDDEN", "leak");
        let (cfg, home) = mock("mock1", |s| {
            s.env = vec!["MCP_TEST_PASSED".into()];
            s.deny = vec!["secret".into()];
            s.approve = "never".into();
        });
        let defs = definitions(&cfg);
        let names: Vec<&str> = defs.iter().map(|d| d["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["mcp_mock1_echo", "mcp_mock1_slow", "mcp_mock1_crash", "mcp_mock1_env"],
            "both pages, minus denied"
        );
        assert_eq!(defs[0]["function"]["description"], "Echo it back");
        assert_eq!(defs[1]["function"]["parameters"], json!({"type": "object", "properties": {}, "required": []}));
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: None };
        let run = |n: &str| call(&ctx, n, &json!({"text": "hi"})).unwrap_or_else(|e| format!("ERR {e}"));
        assert_eq!(run("mcp_mock1_echo"), "hello from mock", "junk lines skipped, ping answered");
        assert_eq!(run("mcp_mock1_env"), "[yes][]", "only listed env vars reach the server");
        let t0 = Instant::now();
        assert!(run("mcp_mock1_slow").contains("no answer within 1s"));
        assert!(t0.elapsed() < Duration::from_millis(1800));
        std::thread::sleep(Duration::from_millis(1300)); // its late answer arrives meanwhile
        assert_eq!(run("mcp_mock1_echo"), "hello from mock", "the late answer to the timed-out call is ignored");
        let e = run("mcp_mock1_crash");
        assert!(e.starts_with("ERR server exited") && e.contains("boom"), "{e}");
        assert_eq!(run("mcp_mock1_echo"), "hello from mock", "restarted after the crash");
        assert!(run("mcp_mock1_nope").contains("unknown tool"));
        let st = status(&cfg, true).join("\n");
        assert!(st.contains("ok mcp        mock1  4 tool(s), 1 filtered out · mock 1.0"), "{st}");
    }

    #[cfg(unix)]
    #[test]
    fn calls_need_approval_unless_read_only() {
        let (cfg, home) = mock("mock2", |s| s.read_only = vec!["env".into()]);
        assert_eq!(definitions(&cfg).len(), 5);
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let (yes, no) = (
            |w: &str| {
                asked.borrow_mut().push(w.to_string());
                true
            },
            |_: &str| false,
        );
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: None };
        assert!(call(&ctx, "mcp_mock2_echo", &json!({})).unwrap_err().contains("unattended"));
        assert!(call(&ctx, "mcp_mock2_env", &json!({})).is_ok(), "read_only runs freely");
        let ctx = Ctx { approver: Some(&no), ..ctx };
        assert!(call(&ctx, "mcp_mock2_echo", &json!({})).unwrap_err().contains("did not approve"));
        let ctx = Ctx { approver: Some(&yes), ..ctx };
        assert_eq!(call(&ctx, "mcp_mock2_echo", &json!({"text": "hi"})).unwrap(), "hello from mock");
        assert_eq!(asked.borrow().as_slice(), [r#"MCP mock2.echo {"text":"hi"}"#]);
        // `writes` trusts the server's readOnlyHint
        let mut cfg2 = cfg.clone();
        cfg2.mcp[0].approve = "writes".into();
        let ctx = Ctx { cfg: &cfg2, approver: None, ..ctx };
        definitions(&cfg2);
        assert!(call(&ctx, "mcp_mock2_echo", &json!({})).is_ok());
        assert!(call(&ctx, "mcp_mock2_slow", &json!({})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn broken_servers_report_and_back_off() {
        let (mut cfg, _home) = mock("mock3", |_| {});
        cfg.mcp[0].args = vec!["-c".into(), "echo not-json; echo 'cannot start: missing token' >&2; exit 1".into()];
        assert!(definitions(&cfg).is_empty());
        let st = status(&cfg, false).join("\n");
        assert!(st.contains("!! mcp        mock3  down: server exited") && st.contains("missing token"), "{st}");
        let t0 = Instant::now();
        definitions(&cfg);
        assert!(t0.elapsed() < Duration::from_millis(100), "no retry before the back-off");
        cfg.mcp[0].command = "/nonexistent/mcp-server".into();
        assert!(definitions(&cfg).is_empty());
        assert!(status(&cfg, false)[0].contains("cannot start"));
    }

    /// Streamable HTTP against a local server: JSON and event-stream answers,
    /// the session id and bearer token on every later request.
    #[test]
    fn http_server_with_sessions_and_event_streams() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", l.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let (mut head, mut len) = (String::new(), 0);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head += &line;
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let v: Value = serde_json::from_slice(&body).unwrap_or_default();
                let lower = head.to_lowercase();
                log.lock().unwrap().push(format!(
                    "{} sid={} auth={}",
                    if head.starts_with("DELETE") { "DELETE" } else { v["method"].as_str().unwrap_or("?") },
                    lower.contains("mcp-session-id: s-42"),
                    lower.contains("authorization: bearer tok-1")
                ));
                let id = v["id"].clone();
                let (ctype, extra, body) = match v["method"].as_str().unwrap_or("") {
                    "initialize" => ("application/json", "Mcp-Session-Id: s-42\r\n", json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-03-26","serverInfo":{"name":"web","version":"2"}}}).to_string()),
                    "tools/list" => ("text/event-stream", "", format!(
                        "event: message\ndata: {}\n\ndata: {}\n\n",
                        json!({"jsonrpc":"2.0","method":"notifications/message","params":{}}),
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"time","inputSchema":{"type":"object"}}]}})
                    )),
                    "tools/call" => ("application/json", "", json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"12:00"}]}}).to_string()),
                    _ => ("", "", String::new()),
                };
                let status = if ctype.is_empty() { "202 Accepted" } else { "200 OK" };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        std::env::set_var("MCP_TEST_TOKEN", "tok-1");
        let s = Server {
            name: "web".into(),
            url,
            bearer_env: "MCP_TEST_TOKEN".into(),
            approve: "never".into(),
            ..Default::default()
        };
        let cfg = Config { mcp: vec![s], ..Default::default() };
        let defs = definitions(&cfg);
        assert_eq!(defs[0]["function"]["name"], "mcp_web_time");
        let home = std::env::temp_dir();
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: None };
        assert_eq!(call(&ctx, "mcp_web_time", &json!({})).unwrap(), "12:00");
        // a fresh connection says goodbye with DELETE
        let mut c = Conn::new(cfg.mcp[0].clone());
        c.connect(Duration::from_secs(5)).unwrap();
        assert!(c.info.starts_with("web 2 (protocol 2025-03-26)"));
        drop(c);
        let log = seen.lock().unwrap().clone();
        assert_eq!(log[0], "initialize sid=false auth=true");
        assert_eq!(log[1], "notifications/initialized sid=true auth=true");
        assert_eq!(log[2], "tools/list sid=true auth=true");
        assert_eq!(log[3], "tools/call sid=true auth=true");
        assert_eq!(log.last().unwrap(), "DELETE sid=true auth=true");
    }
}

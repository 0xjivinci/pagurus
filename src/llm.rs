use crate::config::Provider;
use crate::util::clip;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

mod anthropic;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Msg {
    pub role: String,
    /// Redacted whenever it is serialized: to the provider and to transcripts.
    #[serde(default, serialize_with = "redacted")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider data that must be echoed back (Gemini's thought signatures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_content: Option<Value>,
    /// Images as data: URLs, sent to vision models with this message. Never
    /// written to transcripts (they'd bloat the SD card).
    #[serde(skip)]
    pub images: Vec<String>,
}

/// Secrets (pagurus's keys, token-shaped strings) never leave in a message, to
/// the provider or to a transcript on disk.
fn redacted<S: serde::Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(t) => s.serialize_some(&crate::util::redact(t)),
        None => s.serialize_none(),
    }
}

/// A message in the provider's wire format: with images, `content` becomes the
/// OpenAI multi-part form (text + image_url parts) that vision models accept.
fn wire(m: &Msg) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or_default();
    // Anthropic thinking blocks are for Anthropic only
    if let Some(x) = v.get_mut("extra_content").and_then(Value::as_object_mut) {
        x.remove(anthropic::THINKING);
        if x.is_empty() {
            v.as_object_mut().map(|o| o.remove("extra_content"));
        }
    }
    if !m.images.is_empty() {
        let mut parts = vec![json!({ "type": "text", "text": crate::util::redact(m.content.as_deref().unwrap_or_default()) })];
        parts.extend(m.images.iter().map(|u| json!({ "type": "image_url", "image_url": { "url": u } })));
        v["content"] = Value::Array(parts);
    }
    v
}

impl Msg {
    fn new(role: &str, content: &str) -> Self {
        Self { role: role.into(), content: Some(content.into()), ..Default::default() }
    }
    pub fn system(s: &str) -> Self {
        Self::new("system", s)
    }
    pub fn user(s: &str) -> Self {
        Self::new("user", s)
    }
    pub fn user_with_images(s: &str, images: Vec<String>) -> Self {
        Self { images, ..Self::new("user", s) }
    }
    pub fn assistant(s: &str) -> Self {
        Self::new("assistant", s)
    }
    pub fn tool(id: &str, s: &str) -> Self {
        Self { tool_call_id: Some(id.into()), ..Self::new("tool", s) }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FnCall,
    /// Anything else the provider put on the call (e.g. Gemini 3's
    /// `extra_content.google.thought_signature`) is kept and sent back as is,
    /// since some providers reject tool results whose call lost it.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

fn function_type() -> String {
    "function".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FnCall {
    pub name: String,
    #[serde(default, serialize_with = "redacted_str")]
    pub arguments: String,
}

fn redacted_str<S: serde::Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&crate::util::redact(v))
}

/// Largest provider response read (an answer is a few KB; 16 MiB leaves room for anything sane).
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

struct Fail {
    msg: String,
    /// Credentials problem: no other model on this endpoint will work either.
    fatal: bool,
    /// Seconds to skip this model for (0 = no cooldown).
    cool: u64,
}

impl Fail {
    fn soft(msg: String) -> Self {
        Self { msg, fatal: false, cool: 0 }
    }
}

/// Models that recently rate-limited or failed, shared by all agents in the
/// process (chat, heartbeat, reflection): "url model" → unix time it is usable again.
/// key → (usable again at, failures in a row). Shared by every agent in the process.
static COOLDOWN: std::sync::Mutex<Vec<(String, u64, u32)>> = std::sync::Mutex::new(Vec::new());

/// Skip a failing model for `secs`, doubled for each failure in a row (max 1 h),
/// so a provider that keeps refusing stops costing a wasted request every few
/// minutes. `secs == 0` (a success) clears it.
fn cool(r: &Route, secs: u64) {
    let key = format!("{} {}", r.url, r.model);
    if let Ok(mut v) = COOLDOWN.lock() {
        let strikes = v.iter().find(|(k, _, _)| *k == key).map_or(0, |e| e.2);
        v.retain(|(k, _, _)| *k != key);
        if secs > 0 {
            let wait = (secs << strikes.min(4)).min(3600).max(secs);
            v.push((key, crate::util::now() + wait, strikes + 1));
        }
    }
}

fn cooling(r: &Route) -> bool {
    let key = format!("{} {}", r.url, r.model);
    COOLDOWN.lock().is_ok_and(|v| v.iter().any(|(k, until, _)| *k == key && *until > crate::util::now()))
}

/// Models currently cooling down, for `/health`.
pub fn cooldowns() -> Vec<String> {
    let now = crate::util::now();
    COOLDOWN
        .lock()
        .map(|v| {
            v.iter()
                .filter(|(_, u, _)| *u > now)
                .map(|(k, u, n)| format!("{} ({}s, {n} failure(s) in a row)", k.rsplit(' ').next().unwrap_or(k), u - now))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Api {
    /// OpenAI-compatible `/chat/completions`.
    OpenAi,
    /// Anthropic's native `/messages`.
    Anthropic,
}

/// One model on one endpoint.
struct Route {
    url: String,
    model: String,
    key: Option<String>,
    api: Api,
}

impl Route {
    fn new(r: &crate::config::Route) -> Result<Self, String> {
        let base = r.base_url.trim_end_matches('/');
        let api = match r.api.to_lowercase().as_str() {
            "anthropic" => Api::Anthropic,
            "openai" => Api::OpenAi,
            "" if r.host() == "api.anthropic.com" => Api::Anthropic,
            "" => Api::OpenAi,
            other => return Err(format!("unknown api `{other}` for {}: use openai or anthropic", r.model)),
        };
        let url = match api {
            Api::Anthropic if r.host() == "api.anthropic.com" && !base.ends_with("/v1") => format!("{base}/v1/messages"),
            Api::Anthropic => format!("{base}/messages"),
            Api::OpenAi => format!("{base}/chat/completions"),
        };
        let key = std::env::var(&r.api_key_env).ok().filter(|k| !k.trim().is_empty());
        Ok(Self { url, model: r.model.clone(), key, api })
    }

    /// OpenRouter passes `cache_control` on to Anthropic and Gemini models,
    /// which cache nothing without it.
    fn wants_cache_marks(&self) -> bool {
        self.api == Api::OpenAi
            && self.url.contains("openrouter.ai")
            && (self.model.starts_with("anthropic/") || self.model.starts_with("google/gemini"))
    }
}

pub struct Llm {
    http: ureq::Agent,
    p: Provider,
    routes: Vec<Route>,
}

impl Llm {
    pub fn new(p: &Provider) -> Result<Self, String> {
        if p.model.trim().is_empty() {
            return Err("no model configured — set provider.model in config.toml".into());
        }
        let routes = p.chain().iter().map(Route::new).collect::<Result<Vec<_>, _>>()?;
        let http = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(p.timeout_secs))
            .build();
        Ok(Self { http, p: p.clone(), routes })
    }

    /// Try the main model, then each fallback, within `call_budget_secs` in
    /// total. A credentials problem (401/403) skips the rest of that endpoint;
    /// a rate-limited or failing model is skipped for a while (cooldown), by
    /// every agent in this process.
    pub fn chat(&self, msgs: &[Msg], tools: &Value) -> Result<Msg, String> {
        let deadline = Instant::now() + Duration::from_secs(self.p.call_budget_secs.max(10));
        let mut errors = Vec::new();
        let mut dead: Vec<&str> = Vec::new();
        // cooling routes go last instead of being dropped, so something is always tried
        let mut order: Vec<(usize, &Route)> = self.routes.iter().enumerate().collect();
        order.sort_by_key(|(i, r)| (cooling(r), *i));
        let total = order.len();
        for (pos, (i, r)) in order.into_iter().enumerate() {
            if dead.contains(&r.url.as_str()) {
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left < Duration::from_secs(3) {
                errors.push(format!("out of time after {}s", self.p.call_budget_secs));
                break;
            }
            if crate::util::cancelled() {
                errors.push("stopped by the user".into());
                break;
            }
            // a hanging model must not eat the time the fallbacks need
            let share = if pos + 1 < total { left * 2 / 3 } else { left };
            match self.chat_model(r, msgs, tools, share) {
                Ok(m) => {
                    cool(r, 0);
                    if i > 0 {
                        eprintln!("pagurus: answered by fallback model {}", r.model);
                    }
                    return Ok(m);
                }
                Err(f) => {
                    errors.push(format!("{}: {}", r.model, f.msg));
                    if f.fatal {
                        dead.push(&r.url);
                    }
                    if f.cool > 0 {
                        cool(r, f.cool);
                    }
                }
            }
        }
        let mut e = errors.join(" | ");
        if msgs.iter().any(|m| !m.images.is_empty()) {
            e += " — this request had an image: if your models can't see images, pick one that can with /model \
                  (e.g. gemini-3.5-flash-lite or a VL model)";
        }
        Err(crate::util::redact(&e)) // a 401 body can echo the key back
    }

    /// One model. Server errors get one quick retry; a rate limit moves on at
    /// once (honouring Retry-After for the cooldown).
    fn chat_model(&self, r: &Route, msgs: &[Msg], tools: &Value, left: Duration) -> Result<Msg, Fail> {
        let body = match r.api {
            Api::Anthropic => anthropic::body(&r.model, self.p.max_tokens, msgs, tools),
            Api::OpenAi => {
                let mut wired: Vec<Value> = msgs.iter().map(wire).collect();
                if r.wants_cache_marks() {
                    mark_system_cache(&mut wired);
                }
                let mut body = json!({ "model": r.model, "messages": wired, "max_tokens": self.p.max_tokens });
                if tools.as_array().is_some_and(|a| !a.is_empty()) {
                    body["tools"] = tools.clone();
                }
                body
            }
        };
        let started = Instant::now();
        let mut last = String::new();
        for attempt in 0..2u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2));
            }
            let remaining = left.saturating_sub(started.elapsed());
            if remaining < Duration::from_secs(3) {
                break;
            }
            let mut req = self.http.post(&r.url).timeout(remaining.min(Duration::from_secs(self.p.timeout_secs)));
            match (&r.key, r.api) {
                (Some(k), Api::OpenAi) => req = req.set("Authorization", &format!("Bearer {k}")),
                (k, Api::Anthropic) => {
                    req = req.set("anthropic-version", anthropic::VERSION);
                    if let Some(k) = k {
                        req = req.set("x-api-key", k);
                    }
                }
                (None, Api::OpenAi) => {}
            }
            match req.send_json(&body) {
                Ok(resp) => {
                    // capped: a broken or hostile endpoint must not fill a 1 GB Pi's memory
                    let v: Value = serde_json::from_reader(std::io::Read::take(resp.into_reader(), MAX_RESPONSE))
                        .map_err(|e| Fail::soft(format!("unreadable response: {e}")))?;
                    if std::env::var_os("PAGURUS_DEBUG").is_some() {
                        if let Some(u) = usage_note(&v) {
                            eprintln!("  [{}: {u}]", r.model);
                        }
                    }
                    if r.api == Api::Anthropic {
                        let (msg, cut) = match anthropic::parse(&v) {
                            Ok(x) => x,
                            Err(e) => {
                                last = e;
                                continue;
                            }
                        };
                        if cut && msg.tool_calls.is_none() && msg.content.as_deref().is_none_or(|c| c.trim().is_empty()) {
                            return Err(self.out_of_tokens());
                        }
                        return Ok(msg);
                    }
                    // some gateways answer 200 with an error object
                    let Some(m) = v.pointer("/choices/0/message").cloned() else {
                        last = format!("no message in response: {}", clip(&v.to_string(), 300));
                        continue;
                    };
                    let msg: Msg = serde_json::from_value(m).map_err(|e| Fail::soft(format!("unexpected message shape: {e}")))?;
                    // reasoning models can spend the whole budget thinking and return nothing
                    let empty = msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
                        && msg.tool_calls.as_ref().is_none_or(|t| t.is_empty());
                    if empty && v.pointer("/choices/0/finish_reason").and_then(Value::as_str) == Some("length") {
                        return Err(self.out_of_tokens());
                    }
                    return Ok(msg);
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let retry_after = resp.header("retry-after").and_then(|h| h.trim().parse::<u64>().ok());
                    last = format!("HTTP {code}: {}", clip(&resp.into_string().unwrap_or_default(), 300));
                    match code {
                        401 | 403 => return Err(Fail { msg: last, fatal: true, cool: 0 }),
                        // a daily quota won't come back in minutes: don't keep spending tries on it
                        429 if last.contains("PerDay") || last.to_lowercase().contains("per day") => {
                            return Err(Fail { msg: last, fatal: false, cool: 3600 })
                        }
                        429 => return Err(Fail { msg: last, fatal: false, cool: retry_after.unwrap_or(300).clamp(30, 3600) }),
                        // Anthropic: overloaded. Another model is a better bet than a retry.
                        529 => return Err(Fail { msg: last, fatal: false, cool: retry_after.unwrap_or(60).clamp(30, 3600) }),
                        500..=599 => continue,
                        _ => return Err(Fail::soft(last)), // 400/404 can be model-specific: try the next one
                    }
                }
                Err(e) => {
                    // network trouble or a timeout: retrying the same model wastes the
                    // time a fallback could use
                    return Err(Fail { msg: e.to_string(), fatal: false, cool: 120 });
                }
            }
        }
        Err(Fail { msg: last, fatal: false, cool: 120 })
    }

    fn out_of_tokens(&self) -> Fail {
        Fail::soft(format!(
            "ran out of provider.max_tokens ({}) before answering, probably while thinking; raise it in config.toml",
            self.p.max_tokens
        ))
    }
}

/// OpenRouter form of a prompt-cache breakpoint: the system prompt as text
/// parts, `cache_control` on its stable start.
fn mark_system_cache(wired: &mut [Value]) {
    let Some(m) = wired.first_mut().filter(|m| m["role"] == "system") else { return };
    let Some(t) = m["content"].as_str().map(String::from) else { return };
    let (stable, tail) = anthropic::split_system(&t);
    let mut parts = vec![json!({ "type": "text", "text": stable, "cache_control": { "type": "ephemeral" } })];
    if !tail.trim().is_empty() {
        parts.push(json!({ "type": "text", "text": tail }));
    }
    m["content"] = Value::Array(parts);
}

/// Token usage with prompt-cache hits, for PAGURUS_DEBUG: Anthropic's
/// `cache_read_input_tokens`, OpenAI/OpenRouter's `prompt_tokens_details.cached_tokens`,
/// DeepSeek's `prompt_cache_hit_tokens`.
fn usage_note(v: &Value) -> Option<String> {
    let u = v.get("usage")?;
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let (input, output) = match n("/input_tokens") {
        // Anthropic counts cached tokens apart from input_tokens
        Some(i) => (i + n("/cache_read_input_tokens").unwrap_or(0) + n("/cache_creation_input_tokens").unwrap_or(0), n("/output_tokens")),
        None => (n("/prompt_tokens")?, n("/completion_tokens")),
    };
    let cached = n("/cache_read_input_tokens").or(n("/prompt_tokens_details/cached_tokens")).or(n("/prompt_cache_hit_tokens"));
    let written = n("/cache_creation_input_tokens").or(n("/prompt_tokens_details/cache_write_tokens"));
    let mut s = format!("{input} tokens in");
    if let Some(c) = cached {
        s += &format!(" ({c} cached");
        if let Some(w) = written.filter(|w| *w > 0) {
            s += &format!(", {w} written to cache");
        }
        s += ")";
    }
    if let Some(o) = output {
        s += &format!(", {o} out");
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_call_round_trips_provider_fields() {
        let raw = r#"{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"shell","arguments":"{}"},"extra_content":{"google":{"thought_signature":"sig"}}}]}"#;
        let m: super::Msg = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_string(&m).unwrap();
        assert!(back.contains("\"thought_signature\":\"sig\""), "{back}");
        let plain = serde_json::to_string(&super::Msg::user("hi")).unwrap();
        assert_eq!(plain, r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn images_go_out_as_multipart_but_never_to_disk() {
        let m = super::Msg::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]);
        let w = super::wire(&m);
        assert_eq!(w["content"][0]["text"], "what is this?");
        assert_eq!(w["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
        let stored = serde_json::to_string(&m).unwrap();
        assert!(!stored.contains("base64"), "{stored}");
    }

    #[test]
    fn secrets_never_reach_the_provider_or_transcripts() {
        let key = concat!("sk", "-or-v1-0123456789abcdef0123456789abcdef");
        let mut m = super::Msg::user_with_images(&format!("my key is {key}"), vec!["data:image/png;base64,AAAA".into()]);
        assert!(!super::wire(&m).to_string().contains(key));
        m.images.clear();
        m.tool_calls = Some(vec![super::ToolCall {
            id: "c".into(),
            kind: "function".into(),
            function: super::FnCall { name: "shell".into(), arguments: format!("{{\"command\":\"echo {key}\"}}") },
            extra: Default::default(),
        }]);
        let stored = serde_json::to_string(&m).unwrap();
        assert!(!stored.contains(key) && stored.contains("[REDACTED]"), "{stored}");
        let back: super::Msg = serde_json::from_str(&stored).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&back.tool_calls.unwrap()[0].function.arguments).is_ok());
    }

    /// A local mock endpoint answering one request per reply in order; each raw
    /// request (headers + body) comes out of the receiver.
    fn serve(replies: Vec<(&'static str, Vec<u8>)>) -> (u16, std::sync::mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for (status, body) in replies {
                let Ok((mut c, _)) = l.accept() else { return };
                // read the whole request: closing with unread data resets the connection
                let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
                while let Ok(n @ 1..) = c.read(&mut buf) {
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let Some(h) = text.find("\r\n\r\n") else { continue };
                    let len = text.split("content-length:").nth(1).and_then(|r| r.split("\r\n").next()?.trim().parse().ok());
                    if req.len() >= h + 4 + len.unwrap_or(0) {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&req).into_owned());
                let _ = write!(c, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = c.write_all(&body);
            }
        });
        (port, rx)
    }

    /// A local mock provider answering one request with `status` and `body`; returns a Provider for it.
    fn mock(status: &'static str, body: Vec<u8>, key_env: &str) -> crate::config::Provider {
        let (port, _) = serve(vec![(status, body)]);
        toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"\napi_key_env = \"{key_env}\"")).unwrap()
    }

    fn body_of(req: &str) -> serde_json::Value {
        serde_json::from_str(&req[req.find("\r\n\r\n").unwrap() + 4..]).unwrap()
    }

    #[test]
    fn anthropic_native_round_trip() {
        std::env::set_var("PAGURUS_TEST_ANTHROPIC_KEY", concat!("sk", "-ant-test-key-0123456789"));
        let reply = br#"{"content":[{"type":"tool_use","id":"toolu_9","name":"shell","input":{"command":"uptime"}}],
            "stop_reason":"tool_use","usage":{"input_tokens":12,"cache_read_input_tokens":2000,"output_tokens":7}}"#;
        let (port, rx) = serve(vec![("200 OK", reply.to_vec())]);
        let p: crate::config::Provider = toml::from_str(&format!(
            "base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"claude-test\"\napi = \"anthropic\"\napi_key_env = \"PAGURUS_TEST_ANTHROPIC_KEY\""
        ))
        .unwrap();
        let tools = serde_json::json!([{"type":"function","function":{"name":"shell","description":"run","parameters":{"type":"object"}}}]);
        let msgs = [super::Msg::system("be brief\n\nSession started now."), super::Msg::user("how long has it been up?")];
        let m = super::Llm::new(&p).unwrap().chat(&msgs, &tools).unwrap();
        let call = &m.tool_calls.unwrap()[0];
        assert_eq!((call.id.as_str(), call.function.name.as_str(), call.function.arguments.as_str()), ("toolu_9", "shell", r#"{"command":"uptime"}"#));
        let req = rx.recv().unwrap();
        let head = req.to_lowercase();
        assert!(head.starts_with("post /v1/messages "), "{head}");
        assert!(head.contains(concat!("x-api-key: sk", "-ant-test-key-0123456789")) && head.contains("anthropic-version: 2023-06-01"));
        assert!(!head.contains("authorization:"));
        let b = body_of(&req);
        assert_eq!(b["model"], "claude-test");
        assert_eq!(b["system"][0]["text"], "be brief");
        assert_eq!(b["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"][0]["text"], "how long has it been up?");
        assert_eq!(super::usage_note(&serde_json::from_slice(reply).unwrap()).unwrap(), "2012 tokens in (2000 cached), 7 out");
    }

    #[test]
    fn anthropic_out_of_tokens_is_explained() {
        let (port, _) = serve(vec![("200 OK", br#"{"content":[],"stop_reason":"max_tokens"}"#.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"c\"\napi = \"anthropic\"")).unwrap();
        let e = super::Llm::new(&p).unwrap().chat(&[super::Msg::user("hi")], &serde_json::json!([])).unwrap_err();
        assert!(e.contains("ran out of provider.max_tokens"), "{e}");
    }

    #[test]
    fn overloaded_anthropic_falls_back_to_openai_compatible() {
        let (a, arx) = serve(vec![("529 Overloaded", br#"{"type":"error","error":{"type":"overloaded_error"}}"#.to_vec())]);
        let (o, orx) = serve(vec![("200 OK", br#"{"choices":[{"message":{"role":"assistant","content":"hello from b"}}]}"#.to_vec())]);
        let p: crate::config::Provider = toml::from_str(&format!(
            "base_url = \"http://127.0.0.1:{a}/v1\"\nmodel = \"claude-a\"\napi = \"anthropic\"\n\
             [[fallback]]\nbase_url = \"http://127.0.0.1:{o}/v1\"\nmodel = \"open-b\"\n"
        ))
        .unwrap();
        let m = super::Llm::new(&p).unwrap().chat(&[super::Msg::system("s"), super::Msg::user("hi")], &serde_json::json!([])).unwrap();
        assert_eq!(m.content.as_deref(), Some("hello from b"));
        assert!(arx.recv().unwrap().starts_with("POST /v1/messages "));
        let req = orx.recv().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions "), "the fallback keeps its own wire format");
        assert_eq!(body_of(&req)["messages"][0]["role"], "system");
        let r = super::Route::new(&p.chain()[0]).unwrap();
        assert!(super::cooling(&r), "an overloaded model cools down");
        super::cool(&r, 0);
    }

    #[test]
    fn api_mode_is_picked_by_host_or_setting() {
        let route = |base: &str, api: &str| crate::config::Route { model: "m".into(), base_url: base.into(), api_key_env: "X".into(), api: api.into() };
        let r = super::Route::new(&route("https://api.anthropic.com/v1", "")).unwrap();
        assert_eq!((r.api, r.url.as_str()), (super::Api::Anthropic, "https://api.anthropic.com/v1/messages"));
        assert_eq!(super::Route::new(&route("https://api.anthropic.com", "")).unwrap().url, "https://api.anthropic.com/v1/messages");
        assert_eq!(super::Route::new(&route("https://api.anthropic.com/v1", "openai")).unwrap().api, super::Api::OpenAi);
        assert_eq!(super::Route::new(&route("https://openrouter.ai/api/v1", "")).unwrap().api, super::Api::OpenAi);
        assert!(super::Route::new(&route("https://x/v1", "gemini")).is_err());
        // OpenRouter gets a cache breakpoint on the stable system prompt for Claude and Gemini models
        let mut or = super::Route::new(&route("https://openrouter.ai/api/v1", "")).unwrap();
        or.model = "anthropic/claude-test".into();
        assert!(or.wants_cache_marks());
        let mut w = vec![super::wire(&super::Msg::system("stable rules\n\nSession started now."))];
        super::mark_system_cache(&mut w);
        assert_eq!(w[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(w[0]["content"][1]["text"], "Session started now.");
    }

    #[test]
    fn provider_errors_are_redacted() {
        // the provider echoes the key back in a 401
        std::env::set_var("PAGURUS_TEST_MOCK_KEY", "sk-test-0123456789abcdefghij");
        let body = b"{\"error\":\"Incorrect API key provided: sk-test-0123456789abcdefghij\"}".to_vec();
        let p = mock("401 Unauthorized", body, "PAGURUS_TEST_MOCK_KEY");
        let e = super::Llm::new(&p).unwrap().chat(&[super::Msg::user("hi")], &serde_json::json!([])).unwrap_err();
        assert!(e.contains("401") && !e.contains("0123456789abcdefghij"), "{e}");
    }

    #[test]
    fn oversized_responses_are_not_buffered() {
        let mut body = b"{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"".to_vec();
        body.resize(super::MAX_RESPONSE as usize + 1024, b'a');
        body.extend_from_slice(b"\"}}]}");
        let p = mock("200 OK", body, "PAGURUS_TEST_NO_KEY");
        let e = super::Llm::new(&p).unwrap().chat(&[super::Msg::user("hi")], &serde_json::json!([])).unwrap_err();
        assert!(e.contains("unreadable response"), "{e}");
    }

    #[test]
    fn repeated_failures_back_off() {
        let r = super::Route { url: "http://test-backoff".into(), model: "m".into(), key: None, api: super::Api::OpenAi };
        let until = || super::COOLDOWN.lock().unwrap().iter().find(|e| e.0.starts_with("http://test-backoff")).map(|e| e.1 - crate::util::now());
        super::cool(&r, 300);
        assert!(super::cooling(&r));
        assert!(until().unwrap() <= 300);
        super::cool(&r, 300);
        assert!(until().unwrap() > 590, "second failure waits twice as long");
        for _ in 0..6 {
            super::cool(&r, 300);
        }
        assert!(until().unwrap() <= 3600, "capped at an hour");
        super::cool(&r, 0);
        assert!(!super::cooling(&r), "a success clears it");
    }
}

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
    // a cut-off call's arguments are broken JSON, and some servers (LM Studio)
    // fail the whole request on them: send an empty object instead
    if let Some(calls) = v.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for c in calls {
            let ok = c["function"]["arguments"]
                .as_str()
                .is_some_and(|a| serde_json::from_str::<Value>(a).is_ok_and(|v| v.is_object()));
            if !ok && c.get("function").is_some() {
                c["function"]["arguments"] = json!("{}");
            }
        }
    }
    if !m.images.is_empty() {
        let mut parts =
            vec![json!({ "type": "text", "text": crate::util::redact(m.content.as_deref().unwrap_or_default()) })];
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

thread_local! {
    /// Tokens used by this thread's calls since the last `take_usage`: (in, out).
    static USAGE: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// Tokens in and out since the last call (this thread: the terminal's own turns).
pub fn take_usage() -> (u64, u64) {
    USAGE.with(|u| u.replace((0, 0)))
}

fn add_usage(u: &Value) {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let input =
        n("prompt_tokens") + n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    let output = n("completion_tokens") + n("output_tokens");
    USAGE.with(|c| {
        let (i, o) = c.get();
        c.set((i + input, o + output));
    });
}

/// Streaming (the rich terminal): text deltas go to the sink as they arrive.
#[cfg(feature = "tui")]
mod stream {
    use super::{Msg, ToolCall, MAX_RESPONSE};
    use serde_json::{json, Value};
    use std::cell::{Cell, RefCell};
    use std::io::{BufRead, Read};

    /// Where streamed text goes.
    pub type Sink = Box<dyn FnMut(&str)>;

    thread_local! {
        static SINK: RefCell<Option<Sink>> = RefCell::new(None);
        static STREAMED: Cell<bool> = const { Cell::new(false) };
    }

    /// Show answers live on this thread (None: stop).
    pub fn set(f: Option<Sink>) {
        SINK.with(|s| *s.borrow_mut() = f);
    }

    pub fn on() -> bool {
        SINK.with(|s| s.borrow().is_some())
    }

    /// Whether the last reply's text was already shown while it streamed.
    pub fn take_streamed() -> bool {
        STREAMED.with(|s| s.replace(false))
    }

    fn emit(t: &str) {
        STREAMED.with(|s| s.set(true));
        SINK.with(|s| {
            if let Some(f) = s.borrow_mut().as_mut() {
                f(t)
            }
        });
    }

    pub enum Sse {
        /// The message, its finish reason.
        Done(Msg, Option<String>),
        /// Not a stream after all: the plain JSON answer.
        Json(Value),
        Err(String),
    }

    pub fn read(resp: ureq::Response) -> Sse {
        STREAMED.with(|s| s.set(false));
        let mut r = std::io::BufReader::new(resp.into_reader().take(MAX_RESPONSE));
        let (mut content, mut calls, mut extra, mut finish) = (String::new(), Vec::<Value>::new(), None, None);
        let mut line = String::new();
        let mut first = true;
        loop {
            if crate::util::cancelled() {
                return Sse::Err("stopped by the user".into()); // Ctrl-C: drop the rest of the answer
            }
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Sse::Err(format!("stream broke off: {e}")),
            }
            let l = line.trim();
            if l.is_empty() || l.starts_with(':') {
                continue;
            }
            if first && !l.starts_with("data:") && !l.starts_with("event:") {
                let mut rest = String::new();
                let _ = r.read_to_string(&mut rest);
                return match serde_json::from_str(&format!("{l}{rest}")) {
                    Ok(v) => Sse::Json(v),
                    Err(e) => Sse::Err(format!("unreadable response: {e}")),
                };
            }
            first = false;
            let Some(d) = l.strip_prefix("data:").map(str::trim) else { continue };
            if d == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(d) else { continue };
            if let Some(e) = v.get("error") {
                return Sse::Err(format!("provider error: {}", crate::util::clip(&e.to_string(), 300)));
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                super::add_usage(u);
            }
            let ch = &v["choices"][0];
            let delta = &ch["delta"];
            if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                content.push_str(t);
                emit(t);
            }
            for (pos, tc) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
                let i = tc["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
                while calls.len() <= i {
                    calls.push(json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } }));
                }
                let c = &mut calls[i];
                if let Some(id) = tc["id"].as_str().filter(|x| !x.is_empty()) {
                    c["id"] = json!(id);
                }
                if let Some(n) = tc["function"]["name"].as_str().filter(|x| !x.is_empty()) {
                    if c["function"]["name"].as_str().is_none_or(str::is_empty) {
                        c["function"]["name"] = json!(n);
                    }
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    let joined = format!("{}{a}", c["function"]["arguments"].as_str().unwrap_or(""));
                    c["function"]["arguments"] = json!(joined);
                }
                for (k, x) in tc.as_object().into_iter().flatten() {
                    if !matches!(k.as_str(), "index" | "id" | "type" | "function") {
                        c[k] = x.clone(); // e.g. Gemini's thought signature
                    }
                }
            }
            if let Some(x) = delta.get("extra_content").filter(|x| !x.is_null()) {
                extra = Some(x.clone());
            }
            if let Some(f) = ch["finish_reason"].as_str() {
                finish = Some(f.to_string());
            }
        }
        let mut tool_calls = Vec::new();
        for (n, c) in calls.into_iter().enumerate() {
            let mut c = c;
            if c["id"].as_str().is_none_or(str::is_empty) {
                c["id"] = json!(format!("call_{n}_{}", crate::util::new_id()));
            }
            match serde_json::from_value::<ToolCall>(c) {
                Ok(tc) => tool_calls.push(tc),
                Err(e) => return Sse::Err(format!("unexpected tool call in stream: {e}")),
            }
        }
        if first {
            return Sse::Err("empty response".into());
        }
        let msg = Msg {
            role: "assistant".into(),
            content: (!content.is_empty() || tool_calls.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            extra_content: extra,
            ..Default::default()
        };
        Sse::Done(msg, finish)
    }
}

#[cfg(feature = "tui")]
pub use stream::{set as set_stream, take_streamed};

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
                .map(|(k, u, n)| {
                    format!("{} ({}s, {n} failure(s) in a row)", k.rsplit(' ').next().unwrap_or(k), u - now)
                })
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
            Api::Anthropic if r.host() == "api.anthropic.com" && !base.ends_with("/v1") => {
                format!("{base}/v1/messages")
            }
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
    /// Stream answers to the screen (the terminal's chat model only, not background work).
    live: bool,
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
        Ok(Self { http, p: p.clone(), routes, live: false })
    }

    /// Stream this model's answers to the screen while they're written (rich terminal).
    pub fn set_live(&mut self, on: bool) {
        self.live = on;
    }

    pub fn live(&self) -> bool {
        self.live
    }

    /// Try the main model, then each fallback, within `call_budget_secs` in
    /// total. A credentials problem (401/403) skips the rest of that endpoint;
    /// a rate-limited or failing model is skipped for a while (cooldown), by
    /// every agent in this process.
    pub fn chat(&self, msgs: &[Msg], tools: &Value) -> Result<Msg, String> {
        let first = self.chat_routes(msgs, tools);
        // A server that can't take one of the images (a format it can't decode,
        // too big, or no vision) fails the whole request. Send it again without
        // images, so the turn goes on and the model knows why it can't see them.
        match first {
            Err(e) if msgs.iter().any(|m| !m.images.is_empty()) && image_refused(&e) => {
                let plain: Vec<Msg> = msgs.iter().map(without_images).collect();
                self.chat_routes(&plain, tools).map_err(|e2| format!("{e2} (first try, with images: {e})"))
            }
            r => r,
        }
    }

    fn chat_routes(&self, msgs: &[Msg], tools: &Value) -> Result<Msg, String> {
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
        let mut body = match r.api {
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
                #[cfg(feature = "tui")]
                if self.live && stream::on() {
                    body["stream"] = json!(true);
                    body["stream_options"] = json!({ "include_usage": true });
                }
                body
            }
        };
        if r.api == Api::OpenAi && webp_as_png(&r.url) {
            relabel_webp(&mut body);
        }
        let mut relabelled = false;
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
                    #[cfg(feature = "tui")]
                    let resp = if body.get("stream").is_some() {
                        match stream::read(resp) {
                            stream::Sse::Done(msg, finish) => {
                                let empty = msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
                                    && msg.tool_calls.as_ref().is_none_or(|t| t.is_empty());
                                if empty && finish.as_deref() == Some("length") {
                                    return Err(self.out_of_tokens());
                                }
                                if relabelled {
                                    remember_webp_as_png(&r.url);
                                }
                                let mut msg = msg;
                                if finish.as_deref() == Some("length") {
                                    mark_cut(&mut msg, self.p.max_tokens);
                                }
                                return Ok(msg);
                            }
                            stream::Sse::Err(e) => {
                                last = e;
                                continue;
                            }
                            stream::Sse::Json(v) => Err(v),
                        }
                    } else {
                        Ok(resp)
                    };
                    #[cfg(not(feature = "tui"))]
                    let resp: Result<ureq::Response, Value> = Ok(resp);
                    // capped: a broken or hostile endpoint must not fill a 1 GB Pi's memory
                    let v: Value = match resp {
                        Ok(resp) => serde_json::from_reader(std::io::Read::take(resp.into_reader(), MAX_RESPONSE))
                            .map_err(|e| Fail::soft(format!("unreadable response: {e}")))?,
                        Err(v) => v,
                    };
                    if let Some(u) = v.get("usage") {
                        add_usage(u);
                    }
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
                        if cut && msg.tool_calls.is_none() && msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
                        {
                            return Err(self.out_of_tokens());
                        }
                        let mut msg = msg;
                        if cut {
                            mark_cut(&mut msg, self.p.max_tokens);
                        }
                        return Ok(msg);
                    }
                    // some gateways answer 200 with an error object
                    let Some(m) = v.pointer("/choices/0/message").cloned() else {
                        last = format!("no message in response: {}", clip(&v.to_string(), 300));
                        continue;
                    };
                    let mut msg: Msg =
                        serde_json::from_value(m).map_err(|e| Fail::soft(format!("unexpected message shape: {e}")))?;
                    // reasoning models can spend the whole budget thinking and return nothing
                    let empty = msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
                        && msg.tool_calls.as_ref().is_none_or(|t| t.is_empty());
                    let length = v.pointer("/choices/0/finish_reason").and_then(Value::as_str) == Some("length");
                    if empty && length {
                        return Err(self.out_of_tokens());
                    }
                    if length {
                        mark_cut(&mut msg, self.p.max_tokens);
                    }
                    if relabelled {
                        remember_webp_as_png(&r.url);
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
                        429 => {
                            return Err(Fail {
                                msg: last,
                                fatal: false,
                                cool: retry_after.unwrap_or(300).clamp(30, 3600),
                            })
                        }
                        // Anthropic: overloaded. Another model is a better bet than a retry.
                        529 => {
                            return Err(Fail {
                                msg: last,
                                fatal: false,
                                cool: retry_after.unwrap_or(60).clamp(30, 3600),
                            })
                        }
                        500..=599 => continue,
                        // LM Studio refuses the image/webp label but reads the same bytes
                        // under another one: try that once, and remember it for this server
                        400 if !relabelled && r.api == Api::OpenAi && relabel_webp(&mut body) => {
                            relabelled = true;
                            continue;
                        }
                        _ => return Err(Fail::soft(last)), // 400/404 can be model-specific: try the next one
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    // a kept-alive connection the server closed while we were idle (a local
                    // model server between two messages): the request never got there, so
                    // try once more on a fresh connection instead of benching the model
                    if attempt == 0 && stale_connection(&msg) {
                        last = msg;
                        continue;
                    }
                    // network trouble or a timeout: retrying the same model wastes the
                    // time a fallback could use
                    return Err(Fail { msg, fatal: false, cool: 120 });
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

/// Model servers that take WebP images only under another label (LM Studio).
static WEBP_AS_PNG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn webp_as_png(url: &str) -> bool {
    WEBP_AS_PNG.lock().is_ok_and(|v| v.iter().any(|u| u == url))
}

fn remember_webp_as_png(url: &str) {
    if let Ok(mut v) = WEBP_AS_PNG.lock() {
        if !v.iter().any(|u| u == url) {
            v.push(url.to_string());
        }
    }
}

/// Relabel the request's WebP images as PNG (the bytes stay WebP). False when there were none.
fn relabel_webp(body: &mut Value) -> bool {
    let mut any = false;
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        for part in m["content"].as_array_mut().into_iter().flatten() {
            if let Some(u) = part["image_url"]["url"].as_str().and_then(|u| u.strip_prefix("data:image/webp;")) {
                part["image_url"]["url"] = json!(format!("data:image/png;{u}"));
                any = true;
            }
        }
    }
    any
}

/// An error that a request's images probably caused (not a key, quota or network problem).
fn image_refused(e: &str) -> bool {
    let l = e.to_lowercase();
    (l.contains("http 400") || l.contains("http 413") || l.contains("http 422"))
        || (l.contains("image") && !l.contains("http 401"))
}

fn without_images(m: &Msg) -> Msg {
    if m.images.is_empty() {
        return m.clone();
    }
    let n = m.images.len();
    let note = format!(
        "[{n} image(s) left out: the model server rejected them (an unsupported format or size, or a model without vision). \
         Say so if it matters; don't describe what you couldn't see.]"
    );
    let c = m.content.as_deref().unwrap_or("");
    Msg { images: vec![], content: Some(if c.is_empty() { note } else { format!("{c}\n{note}") }), ..m.clone() }
}

/// The reply hit max_tokens: its tool calls are probably incomplete (a big
/// write_file is the usual case). Don't run them; tell the model why instead
/// (tools::execute answers `CUT` calls with that note).
fn mark_cut(msg: &mut Msg, max_tokens: u32) {
    for c in msg.tool_calls.iter_mut().flatten() {
        c.function.arguments = json!({ CUT: max_tokens }).to_string();
    }
}

/// Marks a tool call that was cut off by max_tokens (see `mark_cut`).
pub const CUT: &str = "pagurus_cut_off";

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
        Some(i) => (
            i + n("/cache_read_input_tokens").unwrap_or(0) + n("/cache_creation_input_tokens").unwrap_or(0),
            n("/output_tokens"),
        ),
        None => (n("/prompt_tokens")?, n("/completion_tokens")),
    };
    let cached =
        n("/cache_read_input_tokens").or(n("/prompt_tokens_details/cached_tokens")).or(n("/prompt_cache_hit_tokens"));
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

/// Errors that mean the connection was already dead before our request was sent.
fn stale_connection(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["connection reset", "reset by peer", "broken pipe", "connection closed", "connection aborted", "unexpected eof"]
        .iter()
        .any(|s| m.contains(s))
        && !m.contains("timed out")
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
    fn a_dead_kept_alive_connection_is_retried_not_benched() {
        assert!(super::stale_connection("Network Error: Connection reset by peer (os error 54)"));
        assert!(super::stale_connection("Broken pipe (os error 32)"));
        assert!(!super::stale_connection("Network Error: timed out reading response"));
        assert!(!super::stale_connection("Dns Failed: no such host"));
    }

    #[test]
    fn secrets_never_reach_the_provider_or_transcripts() {
        let key = concat!("sk", "-or-v1-0123456789abcdef0123456789abcdef");
        let mut m =
            super::Msg::user_with_images(&format!("my key is {key}"), vec!["data:image/png;base64,AAAA".into()]);
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
                    let len =
                        text.split("content-length:").nth(1).and_then(|r| r.split("\r\n").next()?.trim().parse().ok());
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
        toml::from_str(&format!(
            "base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"\napi_key_env = \"{key_env}\""
        ))
        .unwrap()
    }

    fn body_of(req: &str) -> serde_json::Value {
        serde_json::from_str(&req[req.find("\r\n\r\n").unwrap() + 4..]).unwrap()
    }

    #[cfg(feature = "tui")]
    #[test]
    fn streamed_answers_and_tool_calls_are_assembled() {
        use super::Msg;
        use serde_json::json;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Checking \"}}]}\n\n",
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"the disk.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"df -h\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":9}}\n\n",
            "data: [DONE]\n\n"
        );
        let (port, rx) = serve(vec![("200 OK", sse.as_bytes().to_vec()), ("200 OK", br#"{"choices":[{"message":{"role":"assistant","content":"plain"}}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"")).unwrap();
        let mut llm = super::Llm::new(&p).unwrap();
        llm.set_live(true);
        let seen = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let s2 = seen.clone();
        super::set_stream(Some(Box::new(move |t: &str| s2.borrow_mut().push_str(t))));
        let _ = super::take_usage();
        let m = llm.chat(&[Msg::user("disk?")], &json!([])).unwrap();
        assert_eq!(m.content.as_deref(), Some("Checking the disk."));
        assert_eq!(*seen.borrow(), "Checking the disk.");
        let calls = m.tool_calls.unwrap();
        assert_eq!(
            (calls[0].id.as_str(), calls[0].function.name.as_str(), calls[0].function.arguments.as_str()),
            ("c1", "shell", "{\"command\":\"df -h\"}")
        );
        assert!(super::take_streamed());
        assert!(body_of(&rx.recv().unwrap())["stream"] == json!(true));
        // a server that ignores "stream" and answers plain JSON still works
        let m = llm.chat(&[Msg::user("again")], &json!([])).unwrap();
        assert_eq!(m.content.as_deref(), Some("plain"));
        assert_eq!(super::take_usage(), (125, 10));
        super::set_stream(None);
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
        let msgs =
            [super::Msg::system("be brief\n\nSession started now."), super::Msg::user("how long has it been up?")];
        let m = super::Llm::new(&p).unwrap().chat(&msgs, &tools).unwrap();
        let call = &m.tool_calls.unwrap()[0];
        assert_eq!(
            (call.id.as_str(), call.function.name.as_str(), call.function.arguments.as_str()),
            ("toolu_9", "shell", r#"{"command":"uptime"}"#)
        );
        let req = rx.recv().unwrap();
        let head = req.to_lowercase();
        assert!(head.starts_with("post /v1/messages "), "{head}");
        assert!(
            head.contains(concat!("x-api-key: sk", "-ant-test-key-0123456789"))
                && head.contains("anthropic-version: 2023-06-01")
        );
        assert!(!head.contains("authorization:"));
        let b = body_of(&req);
        assert_eq!(b["model"], "claude-test");
        assert_eq!(b["system"][0]["text"], "be brief");
        assert_eq!(b["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"][0]["text"], "how long has it been up?");
        assert_eq!(
            super::usage_note(&serde_json::from_slice(reply).unwrap()).unwrap(),
            "2012 tokens in (2000 cached), 7 out"
        );
    }

    #[test]
    fn a_call_cut_off_by_max_tokens_is_not_run_and_never_sent_back_broken() {
        let reply = br#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"main.py\",\"content\":\"import"}}]}}]}"#;
        let (port, _rx) = serve(vec![("200 OK", reply.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"\nmax_tokens = 4096"))
                .unwrap();
        let m = super::Llm::new(&p).unwrap().chat(&[super::Msg::user("write it")], &serde_json::json!([])).unwrap();
        let args = &m.tool_calls.as_ref().unwrap()[0].function.arguments;
        let v: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(v[super::CUT], 4096, "{args}");
        // a broken call from history goes out as {} (LM Studio answers 500 otherwise)
        let mut broken = m.clone();
        broken.tool_calls.as_mut().unwrap()[0].function.arguments = "{\"path\":\"ma".into();
        assert_eq!(super::wire(&broken)["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(super::wire(&m)["tool_calls"][0]["function"]["arguments"], args.as_str());
    }

    #[test]
    fn a_refused_image_is_dropped_and_the_request_sent_again() {
        let refused = br#"{"error":"'url' field must be a base64 encoded image."}"#;
        let ok =
            br#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"I couldn't see it."}}]}"#;
        let (port, rx) = serve(vec![("400 Bad Request", refused.to_vec()), ("200 OK", ok.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"")).unwrap();
        let msgs = [super::Msg::user_with_images("what is this?", vec!["data:image/png;base64,iVBORw==".into()])];
        let m = super::Llm::new(&p).unwrap().chat(&msgs, &serde_json::json!([])).unwrap();
        assert_eq!(m.content.as_deref(), Some("I couldn't see it."));
        let first = body_of(&rx.recv().unwrap());
        assert_eq!(first["messages"][0]["content"][1]["type"], "image_url");
        let second = body_of(&rx.recv().unwrap());
        let text = second["messages"][0]["content"].as_str().unwrap();
        assert!(text.starts_with("what is this?") && text.contains("image(s) left out"), "{text}");
    }

    #[test]
    fn webp_refused_by_label_is_relabelled_once_and_remembered() {
        let refused = br#"{"error":"'url' field must be a base64 encoded image."}"#;
        let ok = br#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"a crab"}}]}"#;
        let (port, rx) =
            serve(vec![("400 Bad Request", refused.to_vec()), ("200 OK", ok.to_vec()), ("200 OK", ok.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"m\"")).unwrap();
        let llm = super::Llm::new(&p).unwrap();
        let msgs = [super::Msg::user_with_images("what is this?", vec!["data:image/webp;base64,UklGRg==".into()])];
        assert_eq!(llm.chat(&msgs, &serde_json::json!([])).unwrap().content.as_deref(), Some("a crab"));
        let url =
            |req: String| body_of(&req)["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap().to_string();
        assert!(url(rx.recv().unwrap()).starts_with("data:image/webp;"));
        assert!(url(rx.recv().unwrap()).starts_with("data:image/png;base64,UklGRg"), "same bytes, another label");
        llm.chat(&msgs, &serde_json::json!([])).unwrap();
        assert!(url(rx.recv().unwrap()).starts_with("data:image/png;"), "remembered: no failed request first");
    }

    #[test]
    fn anthropic_out_of_tokens_is_explained() {
        let (port, _) = serve(vec![("200 OK", br#"{"content":[],"stop_reason":"max_tokens"}"#.to_vec())]);
        let p: crate::config::Provider =
            toml::from_str(&format!("base_url = \"http://127.0.0.1:{port}/v1\"\nmodel = \"c\"\napi = \"anthropic\""))
                .unwrap();
        let e = super::Llm::new(&p).unwrap().chat(&[super::Msg::user("hi")], &serde_json::json!([])).unwrap_err();
        assert!(e.contains("ran out of provider.max_tokens"), "{e}");
    }

    #[test]
    fn overloaded_anthropic_falls_back_to_openai_compatible() {
        let (a, arx) =
            serve(vec![("529 Overloaded", br#"{"type":"error","error":{"type":"overloaded_error"}}"#.to_vec())]);
        let (o, orx) = serve(vec![(
            "200 OK",
            br#"{"choices":[{"message":{"role":"assistant","content":"hello from b"}}]}"#.to_vec(),
        )]);
        let p: crate::config::Provider = toml::from_str(&format!(
            "base_url = \"http://127.0.0.1:{a}/v1\"\nmodel = \"claude-a\"\napi = \"anthropic\"\n\
             [[fallback]]\nbase_url = \"http://127.0.0.1:{o}/v1\"\nmodel = \"open-b\"\n"
        ))
        .unwrap();
        let m = super::Llm::new(&p)
            .unwrap()
            .chat(&[super::Msg::system("s"), super::Msg::user("hi")], &serde_json::json!([]))
            .unwrap();
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
        let route = |base: &str, api: &str| crate::config::Route {
            model: "m".into(),
            base_url: base.into(),
            api_key_env: "X".into(),
            api: api.into(),
        };
        let r = super::Route::new(&route("https://api.anthropic.com/v1", "")).unwrap();
        assert_eq!((r.api, r.url.as_str()), (super::Api::Anthropic, "https://api.anthropic.com/v1/messages"));
        assert_eq!(
            super::Route::new(&route("https://api.anthropic.com", "")).unwrap().url,
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            super::Route::new(&route("https://api.anthropic.com/v1", "openai")).unwrap().api,
            super::Api::OpenAi
        );
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
        let r =
            super::Route { url: "http://test-backoff".into(), model: "m".into(), key: None, api: super::Api::OpenAi };
        let until = || {
            super::COOLDOWN
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.0.starts_with("http://test-backoff"))
                .map(|e| e.1 - crate::util::now())
        };
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

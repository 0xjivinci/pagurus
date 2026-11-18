//! Anthropic's native Messages API: pagurus's OpenAI-shaped messages and tool
//! definitions in, Anthropic content blocks out, with prompt-cache breakpoints
//! on the stable parts (tools, the frozen system prompt, the latest messages).

use super::{FnCall, Msg, ToolCall};
use crate::util::redact;
use serde_json::{json, Value};

pub const VERSION: &str = "2023-06-01";

/// Where a reply's thinking blocks are kept (in `Msg::extra_content`): current
/// Claude models think on every turn and expect the blocks back unchanged.
pub const THINKING: &str = "anthropic_thinking";

fn cache() -> Value {
    json!({ "type": "ephemeral" })
}

fn text(t: &str) -> Value {
    json!({ "type": "text", "text": t })
}

/// pagurus's system prompt is stable except for a short last paragraph
/// ("Session started …"): split it there so the stable part can be cached
/// across sessions.
pub fn split_system(s: &str) -> (&str, &str) {
    let body = s.trim_end();
    match body.rfind("\n\n") {
        Some(i) if i > 0 && body.len() - i <= 200 => (&s[..i], &s[i + 2..]),
        _ => (s, ""),
    }
}

fn image(url: &str) -> Value {
    if let Some((media, data)) = url.strip_prefix("data:").and_then(|r| r.split_once(";base64,")) {
        return json!({ "type": "image", "source": { "type": "base64", "media_type": media, "data": data } });
    }
    json!({ "type": "image", "source": { "type": "url", "url": url } })
}

/// Anthropic accepts only `[A-Za-z0-9_-]` in tool ids; ids made by other
/// providers earlier in the conversation (fallback chains) are mapped the same
/// way on both the call and its result.
fn tool_id(id: &str, n: usize) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() {
        format!("toolu_{n}")
    } else {
        s
    }
}

/// The request body for `POST /v1/messages`.
pub fn body(model: &str, max_tokens: u32, msgs: &[Msg], tools: &Value) -> Value {
    let mut system: Vec<Value> = Vec::new();
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    // tool_use ids still waiting for their tool_result
    let mut pending: Vec<String> = Vec::new();
    let push = |out: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>| match out.last_mut() {
        Some((r, b)) if r == role => b.extend(blocks),
        _ => out.push((role.into(), blocks)),
    };
    for (n, m) in msgs.iter().enumerate() {
        let t = redact(m.content.as_deref().unwrap_or_default());
        if m.role != "tool" && !pending.is_empty() {
            // a call whose result never got recorded: the API rejects a dangling tool_use
            let r: Vec<Value> = pending
                .drain(..)
                .map(|id| json!({ "type": "tool_result", "tool_use_id": id, "content": "(no result recorded)" }))
                .collect();
            push(&mut out, "user", r);
        }
        match m.role.as_str() {
            "system" if out.is_empty() => {
                let (stable, tail) = split_system(&t);
                if !stable.trim().is_empty() {
                    let mut b = text(stable);
                    if system.is_empty() {
                        b["cache_control"] = cache();
                    }
                    system.push(b);
                }
                if !tail.trim().is_empty() {
                    system.push(text(tail));
                }
            }
            "system" => push(&mut out, "user", vec![text(&format!("[system note] {t}"))]),
            "assistant" => {
                let mut b: Vec<Value> =
                    m.extra_content.as_ref().and_then(|x| x[THINKING].as_array()).cloned().unwrap_or_default();
                let thought = b.len();
                if !t.trim().is_empty() {
                    b.push(text(&t));
                }
                for c in m.tool_calls.iter().flatten() {
                    let id = tool_id(&c.id, n);
                    let input = serde_json::from_str::<Value>(&redact(&c.function.arguments))
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    b.push(json!({ "type": "tool_use", "id": id, "name": c.function.name, "input": input }));
                    pending.push(id);
                }
                if b.len() == thought {
                    b.push(text("(no answer)"));
                }
                push(&mut out, "assistant", b);
            }
            "tool" => {
                let id = tool_id(m.tool_call_id.as_deref().unwrap_or_default(), n);
                let mut content = vec![text(if t.is_empty() { "(no output)" } else { &t })];
                content.extend(m.images.iter().map(|u| image(u)));
                if let Some(i) = pending.iter().position(|p| *p == id) {
                    pending.remove(i);
                    push(&mut out, "user", vec![json!({ "type": "tool_result", "tool_use_id": id, "content": content })]);
                } else {
                    // its call fell out of the window: keep the output as plain text
                    content[0] = text(&format!("[tool result] {t}"));
                    push(&mut out, "user", content);
                }
            }
            _ => {
                let mut b = Vec::new();
                if !t.trim().is_empty() || m.images.is_empty() {
                    b.push(text(if t.trim().is_empty() { "(empty)" } else { &t }));
                }
                b.extend(m.images.iter().map(|u| image(u)));
                push(&mut out, "user", b);
            }
        }
    }
    if !pending.is_empty() {
        let r = pending.drain(..).map(|id| json!({ "type": "tool_result", "tool_use_id": id, "content": "(no result recorded)" }));
        push(&mut out, "user", r.collect());
    }
    if out.first().is_some_and(|(r, _)| r == "assistant") {
        out.insert(0, ("user".into(), vec![text("(continuing our conversation)")]));
    }
    // breakpoints (at most 4): last tool, the stable system prompt, the last two
    // messages — so each round re-reads the conversation so far from the cache
    let len = out.len();
    for (_, blocks) in out.iter_mut().skip(len.saturating_sub(2)) {
        if let Some(b) = blocks.last_mut() {
            b["cache_control"] = cache();
        }
    }
    let messages: Vec<Value> = out.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect();
    let mut body = json!({ "model": model, "max_tokens": max_tokens, "messages": messages });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let mut defs: Vec<Value> = tools
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            let f = d.get("function").unwrap_or(d);
            json!({
                "name": f["name"],
                "description": f.get("description").cloned().unwrap_or_else(|| json!("")),
                "input_schema": f.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            })
        })
        .collect();
    if let Some(last) = defs.last_mut() {
        last["cache_control"] = cache();
        body["tools"] = Value::Array(defs);
    }
    body
}

/// The reply as pagurus's message, and whether it stopped at max_tokens.
pub fn parse(v: &Value) -> Result<(Msg, bool), String> {
    let Some(blocks) = v["content"].as_array() else {
        return Err(format!("no content in response: {}", crate::util::clip(&v.to_string(), 300)));
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut thinking = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => text += b["text"].as_str().unwrap_or_default(),
            Some("tool_use") => calls.push(ToolCall {
                id: b["id"].as_str().unwrap_or_default().into(),
                kind: "function".into(),
                function: FnCall {
                    name: b["name"].as_str().unwrap_or_default().into(),
                    arguments: b.get("input").map_or_else(|| "{}".into(), Value::to_string),
                },
                extra: Default::default(),
            }),
            Some("thinking" | "redacted_thinking") => thinking.push(b.clone()),
            _ => {}
        }
    }
    let msg = Msg {
        role: "assistant".into(),
        content: (!text.is_empty() || calls.is_empty()).then_some(text),
        tool_calls: (!calls.is_empty()).then_some(calls),
        extra_content: (!thinking.is_empty()).then(|| json!({ THINKING: thinking })),
        ..Default::default()
    };
    Ok((msg, v["stop_reason"] == "max_tokens"))
}

#[cfg(test)]
mod tests {
    use super::super::{FnCall, Msg, ToolCall};
    use serde_json::json;

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), kind: "function".into(), function: FnCall { name: name.into(), arguments: args.into() }, extra: Default::default() }
    }

    #[test]
    fn maps_messages_tools_images_and_cache_breakpoints() {
        let sys = format!("{}\n\nSession started Monday 09:00.\n", "You are pagurus. ".repeat(50));
        let mut asst = Msg::assistant("");
        asst.tool_calls = Some(vec![call("functions.shell:0", "shell", r#"{"command":"uptime"}"#), call("c2", "read_file", "{}")]);
        let msgs = vec![
            Msg::system(&sys),
            Msg::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]),
            asst,
            Msg::tool("functions.shell:0", "up 3 days"),
            Msg::tool("c2", "file text"),
            Msg::tool("gone", "orphan output"),
            Msg::user("thanks"),
        ];
        let tools = json!([{ "type": "function", "function": { "name": "shell", "description": "run", "parameters": { "type": "object" } } },
                           { "type": "function", "function": { "name": "read_file", "description": "read", "parameters": { "type": "object" } } }]);
        let b = super::body("claude-x", 1000, &msgs, &tools);
        // system: cached stable part, uncached session time
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(b["system"][1]["text"].as_str().unwrap().starts_with("Session started"));
        assert!(b["system"][1].get("cache_control").is_none());
        // tools in Anthropic form, the last one cached
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert!(b["tools"][0].get("cache_control").is_none());
        assert_eq!(b["tools"][1]["cache_control"]["type"], "ephemeral");
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "user, assistant, user (tool results + text merged): {m:?}");
        assert_eq!(m[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(m[0]["content"][1]["source"]["data"], "AAAA");
        assert_eq!(m[1]["content"][0]["type"], "tool_use");
        assert_eq!(m[1]["content"][0]["id"], "functions_shell_0", "ids sanitized");
        assert_eq!(m[1]["content"][0]["input"]["command"], "uptime");
        let last = m[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["tool_use_id"], "functions_shell_0");
        assert_eq!(last[1]["tool_use_id"], "c2");
        assert!(last[2]["text"].as_str().unwrap().starts_with("[tool result] orphan"));
        // the last two messages carry the rolling breakpoints, the first doesn't
        assert!(m[0]["content"][1].get("cache_control").is_none());
        assert_eq!(m[1]["content"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(last[3]["cache_control"]["type"], "ephemeral");
        assert_eq!(b.to_string().matches("cache_control").count(), 4, "at most 4 breakpoints");
    }

    #[test]
    fn dangling_calls_get_results_and_replies_parse() {
        let mut asst = Msg::assistant("let me check");
        asst.tool_calls = Some(vec![call("a", "shell", "not json")]);
        let b = super::body("m", 10, &[asst, Msg::user("never mind")], &json!([]));
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "user", "starts with a user turn");
        assert_eq!(m[1]["content"][1]["input"], json!({}));
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert!(b.get("tools").is_none() && b.get("system").is_none());

        let r = json!({ "content": [{ "type": "text", "text": "Checking." },
                                    { "type": "tool_use", "id": "toolu_1", "name": "shell", "input": { "command": "df -h" } }],
                        "stop_reason": "tool_use" });
        let (msg, cut) = super::parse(&r).unwrap();
        assert!(!cut);
        assert_eq!(msg.content.as_deref(), Some("Checking."));
        let c = &msg.tool_calls.unwrap()[0];
        assert_eq!((c.id.as_str(), c.function.name.as_str()), ("toolu_1", "shell"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&c.function.arguments).unwrap()["command"], "df -h");
        // thinking blocks come back on the next request, ahead of the text, never cached
        let r = json!({ "content": [{ "type": "thinking", "thinking": "", "signature": "sig" }, { "type": "text", "text": "Hi." }] });
        let (msg, _) = super::parse(&r).unwrap();
        let wired = super::super::wire(&msg);
        assert!(wired.get("extra_content").is_none(), "not sent to OpenAI-compatible routes: {wired}");
        let b = super::body("m", 10, &[Msg::user("hello"), msg, Msg::user("again")], &json!([]));
        assert_eq!(b["messages"][1]["content"][0]["signature"], "sig");
        assert_eq!(b["messages"][1]["content"][1]["text"], "Hi.");
        assert!(b["messages"][1]["content"][0].get("cache_control").is_none());
        let (_, cut) = super::parse(&json!({ "content": [], "stop_reason": "max_tokens" })).unwrap();
        assert!(cut);
        assert!(super::parse(&json!({ "type": "error" })).is_err());
    }
}

use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub provider: Provider,
    pub agent: AgentCfg,
    pub learning: Learning,
    pub tools: ToolsCfg,
    pub daemon: DaemonCfg,
    pub telegram: Option<TelegramCfg>,
    /// More chat channels (`[discord]`, `[slack]`, `[email]`), like Telegram.
    pub discord: Option<crate::channel::discord::DiscordCfg>,
    pub slack: Option<crate::channel::slack::SlackCfg>,
    pub email: Option<crate::channel::email::EmailCfg>,
    /// `[[mcp]]`: external MCP servers whose tools the agent may use.
    pub mcp: Vec<crate::mcp::Server>,
    /// web_search engine: DuckDuckGo with no account, or Brave / Tavily / SearXNG.
    pub search: crate::search::SearchCfg,
    pub voice: crate::voice::VoiceCfg,
    /// `[[host]]`: machines the shell tool may run commands on over ssh.
    #[serde(rename = "host")]
    pub hosts: Vec<crate::remote::Host>,
    /// `[sandbox]`: run commands in a throwaway docker container.
    pub sandbox: crate::remote::Sandbox,
    /// The task board's background worker (inside `pagurus daemon`).
    pub board: BoardCfg,
    /// Other pagurus agents this one works with (`pagurus peer …`).
    pub peer: PeerCfg,
    /// `[art]`: your wallets and weekly budget for art proposals (uge).
    #[cfg(feature = "art")]
    pub art: crate::art_taste::ArtCfg,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct BoardCfg {
    /// Board tasks the daemon runs at once (0 = no board).
    pub workers: u32,
    /// Runs per task before it counts as failed.
    pub max_attempts: u32,
    /// A running task whose worker hasn't checked in for this long is reclaimed.
    pub claim_timeout_minutes: u64,
}

impl Default for BoardCfg {
    fn default() -> Self {
        Self { workers: 1, max_attempts: 2, claim_timeout_minutes: 30 }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PeerCfg {
    /// Where `pagurus daemon` accepts tasks from paired pagurus agents, e.g.
    /// "192.168.1.20:7717" or a Tailscale address. Empty = off.
    pub listen: String,
    /// How this pagurus introduces itself when pairing (default: hostname).
    pub name: String,
    /// How the others reach this pagurus (sent while pairing), e.g. "http://pi.local:7717".
    pub url: String,
    /// Peer tasks run at once here; more get "busy".
    pub max_tasks: u32,
    /// How long to wait for a peer's report.
    pub timeout_minutes: u64,
    /// Paired peers' names, read from peers.json when the config loads.
    #[serde(skip)]
    pub known: Vec<String>,
}

impl Default for PeerCfg {
    fn default() -> Self {
        Self {
            listen: String::new(),
            name: String::new(),
            url: String::new(),
            max_tasks: 1,
            timeout_minutes: 15,
            known: vec![],
        }
    }
}

/// Any OpenAI-compatible `/chat/completions` endpoint (OpenRouter, OpenAI,
/// Ollama/llama.cpp on another machine, …) or Anthropic's native Messages API.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Provider {
    pub base_url: String,
    pub model: String,
    /// Tried in order when the main model is rate-limited, down or missing.
    pub fallback_models: Vec<String>,
    /// Further fallbacks on other providers, tried after `fallback_models`:
    /// `[[provider.fallback]]` with `model` and optional `base_url` / `api_key_env`.
    pub fallback: Vec<Fallback>,
    /// More models you can switch to with /model (not used as fallbacks).
    #[serde(rename = "option")]
    pub options: Vec<Fallback>,
    /// Model for background work (reflection, summaries, heartbeat, tidy,
    /// curator). Unset = the chat model.
    pub background: Option<Fallback>,
    /// Model for delegated sub-agents (hard investigations). Unset = the chat model.
    pub delegate: Option<Fallback>,
    /// The /model choices in config-file order, so numbers stay put after a switch.
    #[serde(skip)]
    pub all: Vec<Route>,
    pub api_key_env: String,
    /// Wire format: "openai" (chat completions), "anthropic" (Messages API), or
    /// empty = auto (anthropic for api.anthropic.com, openai otherwise).
    pub api: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    /// Total time for one model call across all fallbacks.
    pub call_budget_secs: u64,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Fallback {
    pub model: String,
    /// Defaults to the main provider's.
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// Defaults to the main provider's when the endpoint is the same, else auto.
    pub api: Option<String>,
}

/// One model on one endpoint, fully spelled out.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    /// "" (auto), "openai" or "anthropic".
    pub api: String,
}

impl Route {
    pub fn fallback(&self) -> Fallback {
        Fallback {
            model: self.model.clone(),
            base_url: Some(self.base_url.clone()),
            api_key_env: Some(self.api_key_env.clone()),
            api: Some(self.api.clone()),
        }
    }

    pub fn host(&self) -> &str {
        self.base_url.split("://").nth(1).unwrap_or(&self.base_url).split('/').next().unwrap_or("")
    }
}

impl Provider {
    fn spell(&self, f: &Fallback) -> Route {
        let base_url = f.base_url.clone().unwrap_or_else(|| self.base_url.clone());
        let api =
            f.api.clone().unwrap_or_else(|| if base_url == self.base_url { self.api.clone() } else { String::new() });
        Route {
            model: f.model.clone(),
            api_key_env: f.api_key_env.clone().unwrap_or_else(|| self.api_key_env.clone()),
            base_url,
            api,
        }
    }

    pub fn main_route(&self) -> Route {
        Route {
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            api: self.api.clone(),
        }
    }

    /// The model chain in order: main, then fallbacks.
    pub fn chain(&self) -> Vec<Route> {
        let mut v = vec![self.main_route()];
        v.extend(self.fallback_models.iter().map(|m| self.spell(&Fallback { model: m.clone(), ..Default::default() })));
        v.extend(self.fallback.iter().map(|f| self.spell(f)));
        v.retain(|r| !r.model.trim().is_empty());
        v
    }

    /// Everything /model can pick from: the chain plus options, no duplicates.
    pub fn choices(&self) -> Vec<Route> {
        if !self.all.is_empty() {
            return self.all.clone();
        }
        let mut v = self.chain();
        for o in &self.options {
            let r = self.spell(o);
            if !r.model.trim().is_empty() && !v.contains(&r) {
                v.push(r);
            }
        }
        v
    }

    /// This provider with `r` as the chat model; the rest of the chain follows as fallbacks.
    pub fn with_main(&self, r: &Route) -> Provider {
        let mut p = self.clone();
        let rest: Vec<Route> = self.chain().into_iter().filter(|x| x != r).collect();
        p.model = r.model.clone();
        p.base_url = r.base_url.clone();
        p.api_key_env = r.api_key_env.clone();
        p.api = r.api.clone();
        p.fallback_models = vec![];
        p.fallback = rest.iter().map(Route::fallback).collect();
        p
    }

    pub fn background_route(&self) -> Option<Route> {
        self.background.as_ref().filter(|b| !b.model.trim().is_empty()).map(|b| self.spell(b))
    }

    pub fn delegate_route(&self) -> Option<Route> {
        self.delegate.as_ref().filter(|b| !b.model.trim().is_empty()).map(|b| self.spell(b))
    }
}

impl Config {
    /// The config sub-agents use: the delegate model first, the chat chain as fallbacks.
    pub fn for_delegate(&self) -> Config {
        let mut c = self.clone();
        if let Some(r) = self.provider.delegate_route() {
            c.provider = self.provider.with_main(&r);
        }
        c
    }

    /// The config background work should use: the background model first, the
    /// chat chain as its fallbacks.
    pub fn background(&self) -> Config {
        let mut c = self.clone();
        if let Some(r) = self.provider.background_route() {
            c.provider = self.provider.with_main(&r);
        }
        c
    }
}

/// `/model` choices persist in `$PAGURUS_HOME/model`: "chat <base_url> <model>"
/// and "background <base_url> <model>" lines, applied over config.toml.
pub fn save_model(
    home: &Path,
    chat: Option<&Route>,
    background: Option<&Route>,
    delegate: Option<&Route>,
) -> Result<(), String> {
    let mut lines: Vec<String> = Vec::new();
    for (kind, r) in [("chat", chat), ("background", background), ("delegate", delegate)] {
        if let Some(r) = r {
            lines.push(format!("{kind} {} {} {}", r.base_url, r.api_key_env, r.model));
        }
    }
    crate::util::write_atomic(&home.join("model"), &(lines.join("\n") + "\n"))
}

fn apply_model_file(home: &Path, c: &mut Config) {
    let text = std::fs::read_to_string(home.join("model")).unwrap_or_default();
    for line in text.lines() {
        let mut w = line.split_whitespace();
        let (Some(kind), Some(base), Some(env), Some(model)) = (w.next(), w.next(), w.next(), w.next()) else {
            continue;
        };
        // the configured choice carries the api mode, which the file doesn't record
        let Some(r) =
            c.provider.choices().into_iter().find(|r| r.model == model && r.base_url == base && r.api_key_env == env)
        else {
            continue; // removed from config.toml since: ignore
        };
        match kind {
            "chat" => c.provider = c.provider.with_main(&r),
            "background" => c.provider.background = Some(r.fallback()),
            "delegate" => c.provider.delegate = Some(r.fallback()),
            _ => {}
        }
    }
}

/// `/model …`: list the choices, or switch the chat or background model.
/// Returns the text for the user and, on a switch, the reloaded config.
pub fn model_command(home: &Path, cfg: &Config, args: &str) -> Result<(String, Option<Config>), String> {
    let choices = cfg.provider.choices();
    let current = cfg.provider.chain().into_iter().next();
    let bg = cfg.provider.background_route();
    let sub = cfg.provider.delegate_route();
    let mut words = args.split_whitespace();
    let first = words.next().unwrap_or("");
    let (role, pick) = match first {
        "background" | "bg" => ("background", words.collect::<Vec<_>>().join(" ")),
        "delegate" | "sub" | "subagent" => ("delegate", words.collect::<Vec<_>>().join(" ")),
        _ => ("chat", args.trim().to_string()),
    };
    if pick.is_empty() {
        let mut s = String::from(
            "Models — /model <n>: chat · /model background <n>: reflection, summaries, heartbeat · /model delegate <n>: sub-agents\n",
        );
        for (i, r) in choices.iter().enumerate() {
            let mut tags = Vec::new();
            if Some(r) == current.as_ref() {
                tags.push("chat ✓");
            }
            if Some(r) == bg.as_ref() || (bg.is_none() && Some(r) == current.as_ref()) {
                tags.push("background");
            }
            if Some(r) == sub.as_ref() || (sub.is_none() && Some(r) == current.as_ref()) {
                tags.push("sub-agents");
            }
            let tags = if tags.is_empty() { String::new() } else { format!("  [{}]", tags.join(", ")) };
            s += &format!("{}. {} · {}{tags}\n", i + 1, r.model, r.host());
        }
        s += "The models after the chat model are tried in order if it fails. More choices: [[provider.option]] in config.toml.";
        return Ok((s, None));
    }
    let chosen = match pick.parse::<usize>() {
        Ok(n) if n >= 1 && n <= choices.len() => choices[n - 1].clone(),
        Ok(_) => return Err(format!("pick 1–{}", choices.len())),
        Err(_) => {
            let hits: Vec<&Route> = choices.iter().filter(|r| r.model == pick || r.model.contains(&pick)).collect();
            match hits.as_slice() {
                [one] => (*one).clone(),
                [] => {
                    return Err(format!(
                        "no model matches `{pick}`; /model lists them (add more as [[provider.option]] in config.toml)"
                    ))
                }
                many => match many.iter().find(|r| r.model == pick) {
                    Some(exact) => (*exact).clone(),
                    None => return Err(format!("`{pick}` matches {} models; use the number", many.len())),
                },
            }
        }
    };
    let (mut chat, mut back, mut dele) = (current.clone(), bg.clone(), sub.clone());
    match role {
        "background" => back = Some(chosen.clone()),
        "delegate" => dele = Some(chosen.clone()),
        _ => chat = Some(chosen.clone()),
    }
    save_model(home, chat.as_ref(), back.as_ref(), dele.as_ref())?;
    let new = load(home)?;
    let msg = match role {
        "background" => format!("Background work (reflection, summaries, heartbeat, tidying) now uses {} · {}.", chosen.model, chosen.host()),
        "delegate" => format!(
            "Sub-agents now use {} · {} (the chat chain is their fallback). I'll delegate harder investigations to them.",
            chosen.model,
            chosen.host()
        ),
        _ => format!("Now chatting with {} · {}. The others are fallbacks.", chosen.model, chosen.host()),
    };
    Ok((msg, Some(new)))
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".into(),
            model: String::new(),
            fallback_models: vec![],
            fallback: vec![],
            options: vec![],
            background: None,
            delegate: None,
            all: vec![],
            api_key_env: "PAGURUS_API_KEY".into(),
            api: String::new(),
            max_tokens: 4096,
            timeout_secs: 120,
            call_budget_secs: 150,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AgentCfg {
    pub max_iterations: u32,
    /// Wall-clock limit for one message, all tool rounds included.
    pub turn_budget_secs: u64,
    /// Tool rounds a delegated sub-agent gets.
    pub delegate_iterations: u32,
    /// Sub-agents one `delegate` call runs at once.
    pub delegate_parallel: u32,
    pub max_history: usize,
    /// Fold messages that fall out of the window into a short running summary.
    pub summarize: bool,
    /// About how many characters of conversation the model sees (~4 chars per token).
    pub context_chars: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_iterations: 16,
            turn_budget_secs: 600,
            delegate_iterations: 8,
            delegate_parallel: 3,
            max_history: 24,
            summarize: true,
            context_chars: 24_000,
        }
    }
}

/// Learning. The agent learns on its own; the user governs the policy and
/// audits the results. Modes:
/// - `auto` (default): changes apply immediately, up to `auto_daily_limit`
///   per 24h; each one is shown in a digest and can be undone; locked files
///   and skills are never touched.
/// - `propose`: every change is queued for `pagurus learn approve`.
/// - `off`: no memory/skill tools, no review pass.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Learning {
    pub mode: String,
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
    pub review_every_turns: u32,
    pub review_after_tool_calls: u32,
    /// In auto mode, also apply skills without asking.
    pub auto_skills: bool,
    /// Max self-applied changes per rolling 24h; extra ones queue as proposals. 0 = no cap.
    pub auto_daily_limit: u32,
    /// Daemon: reflect on a chat after it has been quiet this long. 0 = off.
    pub idle_review_minutes: u64,
    /// Daemon: tidy MEMORY.md/USER.md (at most daily) once either is this full. 0 = off.
    pub consolidate_at_percent: u32,
}

impl Default for Learning {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            memory_char_limit: 2200,
            user_char_limit: 1375,
            review_every_turns: 8,
            review_after_tool_calls: 12,
            auto_skills: true,
            auto_daily_limit: 12,
            idle_review_minutes: 20,
            consolidate_at_percent: 70,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ToolsCfg {
    /// `off` | `allowlist` | `ask` (allowlisted run freely; others need a y/N in the terminal)
    pub shell: String,
    pub shell_allow: Vec<String>,
    /// Commands blocked even when approved, on top of the built-in safety floor.
    pub shell_deny: Vec<String>,
    /// `ask` (approve everything not read-only) | `auto` (also run `auto_allow` commands without asking).
    pub autonomy: String,
    /// `auto` mode: commands that may run without asking even though they write (`cargo test`, `make`).
    pub auto_allow: Vec<String>,
    pub shell_timeout_secs: u64,
    pub read_roots: Vec<String>,
    pub write_roots: Vec<String>,
    /// `web_fetch`: read one URL as text.
    pub web: bool,
    /// The skills hub index (`pagurus skills search`, uge): an https URL, or "off".
    #[cfg(feature = "hub")]
    pub skills_hub: String,
    /// Browser automation (uge): "auto" (Lightpanda if installed, else Chromium/Chrome/Brave), "lightpanda", "chromium", a path, or "off".
    #[cfg(feature = "browser")]
    pub browser: String,
}

/// Read-only commands that run without asking (also joined with | and &&).
/// Two-word entries allow only that subcommand.
#[cfg_attr(windows, allow(dead_code))]
pub const SAFE_COMMANDS: &[&str] = &[
    "uptime",
    "df",
    "du",
    "free",
    "date",
    "uname",
    "whoami",
    "id",
    "groups",
    "hostname",
    "ps",
    "pgrep",
    "ls",
    "stat",
    "file",
    "which",
    "cat",
    "head",
    "tail",
    "grep",
    "wc",
    "sort",
    "cut",
    "tr",
    "echo",
    "vcgencmd",
    "lsusb",
    "lsblk",
    "ip",
    "ss",
    "ping",
    "journalctl",
    "systemctl status",
    "systemctl is-active",
    "systemctl is-enabled",
    "systemctl is-failed",
    "systemctl show",
    "systemctl list-units",
    "systemctl list-unit-files",
    "systemctl list-timers",
    "systemctl cat",
    "systemctl --user status",
    "systemctl --user is-active",
    "systemctl --user show",
    "systemctl --user list-units",
    "systemctl --user list-timers",
    "top -b",
];

/// Extra read-only commands on macOS (no `free`, `ip`, `ss` or systemd there).
#[cfg(target_os = "macos")]
pub const SAFE_COMMANDS_MACOS: &[&str] = &[
    "sw_vers",
    "vm_stat",
    "sysctl",
    "memory_pressure",
    "top -l",
    "diskutil list",
    "diskutil info",
    "pmset -g",
    "launchctl list",
    "system_profiler",
    "netstat",
    "ifconfig",
    "lsof -i",
    "log show",
];

/// On Windows the read-only commands are different ones.
#[cfg(windows)]
pub const SAFE_COMMANDS_WINDOWS: &[&str] = &[
    "whoami",
    "hostname",
    "ver",
    "systeminfo",
    "tasklist",
    "ipconfig",
    "netstat",
    "ping",
    "dir",
    "type",
    "findstr",
    "where",
    "echo",
    "date /t",
    "time /t",
    "wmic logicaldisk get",
    "sc query",
    "Get-Process",
];

impl Default for ToolsCfg {
    fn default() -> Self {
        Self {
            shell: "ask".into(),
            #[cfg(not(any(windows, target_os = "macos")))]
            shell_allow: SAFE_COMMANDS.iter().map(|s| s.to_string()).collect(),
            #[cfg(target_os = "macos")]
            shell_allow: SAFE_COMMANDS.iter().chain(SAFE_COMMANDS_MACOS).map(|s| s.to_string()).collect(),
            #[cfg(windows)]
            shell_allow: SAFE_COMMANDS_WINDOWS.iter().map(|s| s.to_string()).collect(),
            shell_deny: vec![],
            autonomy: "ask".into(),
            auto_allow: vec![],
            shell_timeout_secs: 30,
            read_roots: vec!["/".into()],
            web: true,
            #[cfg(feature = "hub")]
            skills_hub: crate::hub_index::HERMES_INDEX.into(),
            #[cfg(feature = "browser")]
            browser: "auto".into(),
            write_roots: vec![],
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct DaemonCfg {
    pub heartbeat_minutes: u64,
}

impl Default for DaemonCfg {
    fn default() -> Self {
        Self { heartbeat_minutes: 60 }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct TelegramCfg {
    #[serde(default = "default_tg_env")]
    pub token_env: String,
    pub allowed_chat_ids: Vec<i64>,
}

fn default_tg_env() -> String {
    "PAGURUS_TELEGRAM_TOKEN".into()
}

pub fn load(home: &Path) -> Result<Config, String> {
    let p = home.join("config.toml");
    let s =
        std::fs::read_to_string(&p).map_err(|_| format!("no config at {} — run `pagurus init` first", p.display()))?;
    let mut c: Config = toml::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))?;
    c.provider.all = c.provider.choices();
    apply_model_file(home, &mut c);
    if !matches!(c.learning.mode.as_str(), "off" | "propose" | "auto") {
        return Err(format!("learning.mode must be off, propose or auto (got `{}`)", c.learning.mode));
    }
    if !matches!(c.tools.shell.as_str(), "off" | "allowlist" | "ask") {
        return Err(format!("tools.shell must be off, allowlist or ask (got `{}`)", c.tools.shell));
    }
    if !matches!(c.tools.autonomy.as_str(), "ask" | "auto") {
        return Err(format!(
            "tools.autonomy must be ask or auto (got `{}`); yolo is switched on for a while with /yolo, not in the config",
            c.tools.autonomy
        ));
    }
    crate::mcp::check(&c.mcp)?;
    crate::remote::check(home, &c)?;
    c.peer.known = crate::peer::names(home);
    Ok(c)
}

pub const DEFAULT_TOML: &str = r#"# pagurus configuration

[provider]
# Any OpenAI-compatible endpoint. Examples:
#   OpenRouter     https://openrouter.ai/api/v1
#   OpenAI         https://api.openai.com/v1
#   Anthropic      https://api.anthropic.com/v1   (native Messages API, prompt caching; api_key_env = "ANTHROPIC_API_KEY")
#   Ollama on LAN  http://192.168.1.20:11434/v1   (leave the key env var unset)
base_url = "https://openrouter.ai/api/v1"
model = ""                     # REQUIRED: the model id your endpoint expects
fallback_models = []           # tried in order if the model is rate-limited or down
api_key_env = "PAGURUS_API_KEY" # env var with the key; also read from ~/.pagurus/env (never put the key here)
# api = "anthropic"   # wire format: auto (default: anthropic for api.anthropic.com) | openai | anthropic
max_tokens = 4096      # per answer; reasoning models (Qwen, Gemini, DeepSeek…) think first, so keep it roomy
timeout_secs = 120     # one request
call_budget_secs = 150 # one answer, across all fallbacks
# Fallbacks on another provider, e.g. Google AI Studio (Gemini) after OpenRouter:
# [[provider.fallback]]
# base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
# model = "gemini-3.5-flash-lite"
# api_key_env = "GEMINI_API_KEY"
#
# More models to switch to with /model (not fallbacks), and per-role models:
# [[provider.option]]
# model = "some/other-model"
# [provider.background]          # reflection, summaries, heartbeat, tidying
# model = "a-fast-cheap-model"
# [provider.delegate]            # sub-agents for hard investigations
# model = "a-stronger-model"

[agent]
max_iterations = 16   # tool rounds per message
turn_budget_secs = 600 # wall-clock limit per message
delegate_iterations = 8 # tool rounds for a delegated sub-agent
delegate_parallel = 3 # sub-agents one delegate call runs at once (1 on a Pi Zero)
max_history = 24      # messages kept in the context window
summarize = true      # older messages become a short summary instead of being dropped
context_chars = 24000 # about how much conversation the model sees (~6k tokens); old tool output is trimmed first

[learning]
# auto     : learns on its own; you audit with `pagurus learn` (digest, undo, lock)
# propose  : the agent only proposes; you approve each change with `pagurus learn`
# off      : never learns
mode = "auto"
auto_skills = true             # auto mode also writes skills (false = skills wait for approval)
auto_daily_limit = 12          # self-applied changes per 24h; the rest queue for you (0 = no cap)
memory_char_limit = 2200       # MEMORY.md budget (~800 tokens)
user_char_limit = 1375         # USER.md budget (~500 tokens)
review_every_turns = 8         # reflection pass every N user turns (0 = off)
review_after_tool_calls = 12   # ...or after N tool calls (0 = off)
idle_review_minutes = 20       # daemon: reflect when a chat goes quiet (0 = off)
consolidate_at_percent = 70    # daemon: tidy memory (max daily) once a file is this full (0 = off)

[tools]
shell = "ask"                  # off | allowlist | ask
# read-only commands that run without asking; leave this out to get pagurus's default list
# shell_allow = ["uptime", "df", "ps", "journalctl", "systemctl status", …]
# shell_deny = ["git push", "npm publish"]   # never run these, even when approved (on top of the safety floor)
autonomy = "ask"               # ask | auto: auto also runs the auto_allow commands without asking
# auto_allow = ["cargo build", "cargo test", "make", "npm test"]
# Never allowed, whoever approves: root (sudo), wiping folders, disk tools, piping downloads into a shell,
# uploading local files, and changing pagurus's own settings, keys or service. `/yolo 30m` lifts approvals
# (not the floor) for a while; every command is logged (`pagurus audit`).
shell_timeout_secs = 30
read_roots = ["/"]             # where read_file may read (files that look like secrets are always refused)
write_roots = []               # extra dirs write_file may write
web = true                     # web_search + web_fetch: find pages and read them as text
# skills_hub = "off"             # the skills index for `pagurus skills search` (default: the public Hermes index)
# browser = "auto"               # uge: Lightpanda if installed, else Chromium/Chrome/Brave; "lightpanda", "chromium", a path, or "off"

[search]                         # web_search engine
provider = "auto"                # auto: Brave or Tavily if their key is in the env file, else SearXNG if set, else DuckDuckGo (no account)
# searxng_url = "http://nas.local:8888"
results = 6

# [art]                        # uge build: your taste, wishlist and weekly art proposals
# wallets = ["tz1…", "0x…"]    # your public addresses (never a seed phrase or private key)
# weekly_budget_xtz = 100
# weekly_budget_eth = 0.1
# picks_per_week = 5
# dashboard = "127.0.0.1:7720" # the art dashboard, served by the daemon on this machine only ("off" to switch off)

[daemon]
heartbeat_minutes = 60         # 0 disables HEARTBEAT.md checks

[board]                          # task board (`pagurus board`, /board): the daemon works ready tasks in the background
workers = 1                      # tasks at once (0 = no board)
max_attempts = 2                 # runs per task before it fails
claim_timeout_minutes = 30       # a task whose worker went silent this long is run again

# Swarm: pagurus agents on other devices hand each other tasks (`pagurus peer pair` / `pagurus peer add`).
# Off unless listen is set; use a LAN or Tailscale address, never a public one.
# [peer]
# listen = "192.168.1.20:7717"   # where `pagurus daemon` accepts tasks from paired pagurus agents
# url = "http://192.168.1.20:7717"   # how the others reach this one
# name = "pi"                    # how this pagurus introduces itself (default: hostname)
# max_tasks = 1                  # peer tasks at once here
# timeout_minutes = 15           # how long to wait for a peer's report

# [telegram]
# token_env = "PAGURUS_TELEGRAM_TOKEN"
# allowed_chat_ids = [123456789]   # required: only these chats are answered
#                                  # (in a group, the sender's own id must be listed too)

# More chat channels, same commands and approvals. Secrets only via env var NAMES
# (put the values in ~/.pagurus/env). Each answers only its allowlist.
# [discord]                        # bot: Gateway WebSocket; enable MESSAGE CONTENT INTENT
# token_env = "PAGURUS_DISCORD_TOKEN"
# allowed_user_ids = [123456789012345678]   # DMs, and @mentions in servers
# channel_ids = []                 # server channels where no @mention is needed
#
# [slack]                          # Socket Mode app (no public URL needed)
# bot_token_env = "PAGURUS_SLACK_BOT_TOKEN"  # xoxb-…
# app_token_env = "PAGURUS_SLACK_APP_TOKEN"  # xapp-… (connections:write)
# allowed_user_ids = ["U012ABCDEF"]         # DMs, and @mentions in channels
#
# [email]                          # plain text; only mail from allowed_senders is read or answered
# imap_host = "imap.fastmail.com"  # TLS, port 993
# smtp_host = "smtp.fastmail.com"
# smtp_port = 465                  # 465 TLS · 587 STARTTLS
# username = "pagurus@example.org"
# password_env = "PAGURUS_EMAIL_PASSWORD"    # an app password
# allowed_senders = ["you@example.org"]
# poll_seconds = 30
# require_auth = true              # only mail your provider marks dmarc/dkim/spf pass for the sender

# MCP servers: their tools appear as mcp_<name>_<tool> (`pagurus mcp list` shows them).
# Each call asks you first (y/N, or /yes on Telegram) unless the tool is in read_only.
# [[mcp]]
# name = "fs"
# command = "npx"                     # stdio: a program and its args …
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/pi/notes"]
# env = []                            # names of env vars to pass (values from the environment / ~/.pagurus/env)
# read_only = ["read_*", "list_*", "search_files", "get_file_info"]   # run without asking
# deny = ["move_file"]                # hide these tools (allow = [...] keeps only those)
# approve = "always"                  # always (except read_only) | writes (also trust the server's read-only hint) | never
# timeout_secs = 60
# enabled = true
#
# [[mcp]]
# name = "web"
# url = "https://example.com/mcp"     # … or a Streamable HTTP endpoint
# bearer_env = "EXAMPLE_MCP_TOKEN"    # env var holding the token (never the token itself)

# Voice notes on Telegram (off unless set). With a Gemini key, one line does both
# (needs ffmpeg for the spoken replies):
# [voice]
# provider = "gemini"                          # key from GEMINI_API_KEY (key_env = "…" to change)
# tts_voice = "Kore"                           # Gemini voices: Kore, Puck, Charon, Aoede, Fenrir…
#
# … or any OpenAI-compatible audio endpoint:
# [voice]
# stt_url = "https://api.groq.com/openai/v1"   # speech-to-text: <stt_url>/audio/transcriptions
# stt_model = "whisper-large-v3-turbo"
# stt_key_env = "GROQ_API_KEY"                 # env var with the key (empty for a local server)
# tts_url = "https://api.openai.com/v1"        # optional spoken replies: <tts_url>/audio/speech
# tts_model = "tts-1"
# tts_voice = "alloy"
# tts_key_env = "OPENAI_API_KEY"
# speak = "voice"                              # voice: answer voice notes by voice · always · off
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn model_switch_reorders_the_chain_and_persists() {
        let home = std::env::temp_dir().join(format!("pagurus-model-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[provider]\nbase_url = \"https://g/v1\"\nmodel = \"lite\"\napi_key_env = \"G\"\n\
             [[provider.fallback]]\nbase_url = \"https://or/v1\"\nmodel = \"free\"\napi_key_env = \"OR\"\n\
             [[provider.option]]\nmodel = \"flash\"\n",
        )
        .unwrap();
        let c = super::load(&home).unwrap();
        let names = |p: &super::Provider| p.chain().iter().map(|r| r.model.clone()).collect::<Vec<_>>();
        assert_eq!(names(&c.provider), vec!["lite", "free"], "options are not fallbacks");
        let choices = c.provider.choices();
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[2].base_url, "https://g/v1", "an option inherits the main endpoint");
        super::save_model(&home, Some(&choices[2]), Some(&choices[0]), Some(&choices[1])).unwrap();
        let c = super::load(&home).unwrap();
        assert_eq!(names(&c.provider), vec!["flash", "lite", "free"]);
        assert_eq!(c.provider.choices()[2].model, "flash", "numbering stays in config order");
        assert_eq!(c.provider.background_route().unwrap().model, "lite");
        assert_eq!(names(&c.background().provider)[0], "lite");
        assert_eq!(
            names(&c.for_delegate().provider),
            vec!["free", "flash", "lite"],
            "sub-agents fall back to the chat chain"
        );
    }

    #[test]
    fn api_mode_follows_its_route() {
        let home = std::env::temp_dir().join(format!("pagurus-api-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[provider]\nbase_url = \"https://or/v1\"\nmodel = \"a\"\napi = \"openai\"\nfallback_models = [\"b\"]\n\
             [[provider.fallback]]\nbase_url = \"https://proxy/v1\"\nmodel = \"claude\"\napi = \"anthropic\"\n\
             [[provider.fallback]]\nbase_url = \"https://g/v1\"\nmodel = \"g\"\n",
        )
        .unwrap();
        let c = super::load(&home).unwrap();
        let apis: Vec<String> = c.provider.chain().iter().map(|r| r.api.clone()).collect();
        assert_eq!(apis, vec!["openai", "openai", "anthropic", ""], "same endpoint inherits, another one is auto");
        let claude = c.provider.choices()[2].clone();
        super::save_model(&home, Some(&claude), None, None).unwrap();
        let c = super::load(&home).unwrap();
        assert_eq!(c.provider.chain()[0].api, "anthropic", "a /model switch keeps the api mode");
        assert_eq!(c.provider.chain()[1].api, "openai");
    }

    #[test]
    fn cross_provider_fallback_parses() {
        let c: super::Config = toml::from_str(
            "[provider]\nmodel = \"a\"\n[[provider.fallback]]\nmodel = \"g\"\nbase_url = \"https://x/v1\"\napi_key_env = \"GEMINI_API_KEY\"\n",
        )
        .unwrap();
        assert_eq!(c.provider.fallback.len(), 1);
        assert_eq!(c.provider.fallback[0].api_key_env.as_deref(), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn default_toml_parses() {
        let c: super::Config = toml::from_str(super::DEFAULT_TOML).unwrap();
        assert_eq!(c.learning.mode, "auto");
        assert_eq!(c.learning.auto_daily_limit, 12);
        assert_eq!(c.learning.memory_char_limit, 2200);
        assert!(c.telegram.is_none());
    }
}

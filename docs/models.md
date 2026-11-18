# Models

[← back to the README](../README.md)

## Models

pagurus works with any OpenAI-compatible endpoint and can mix providers. `pagurus setup` offers OpenRouter, OrcaRouter,
Google AI Studio, OpenAI, **LM Studio**, Ollama or any other endpoint, and lists the free (or locally installed) models to pick from.

**Local models (LM Studio):** start LM Studio's server (Developer tab, or `lms server start`), load a model that supports
tool calls (Qwen, Llama 3.1+, Mistral…), then choose *LM Studio* in `pagurus setup`. No key needed. Models that think before answering need a roomy `max_tokens`; setup
gives local models 8192, and pagurus tells you plainly if a model runs out mid-thought.


```toml
[provider]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
model = "gemini-3.5-flash-lite"
api_key_env = "GEMINI_API_KEY"          # the key itself goes in ~/.pagurus/env

[[provider.fallback]]                   # tried in order when the one before fails
base_url = "https://api.orcarouter.ai/v1"
model = "deepseek/deepseek-v4.1-flash-free"
api_key_env = "ORCA_API_KEY"

[[provider.option]]                     # extra choices for /model, not fallbacks
model = "gemini-3.5-flash"
```

- **Three roles:** a *chat* model, a *background* model (reflection, summaries, heartbeat, tidying), and a *delegate*
  model for sub-agents. Switch any of them with `/model` (`pagurus model` in the shell). The choice persists in
  `~/.pagurus/model`. A typical setup is a fast free model for chat and background work, plus a stronger one for sub-agents.
- **Fallbacks and cooldowns:** a model that is rate-limited (HTTP 429, honouring `Retry-After`) or failing is skipped for
  a while by every agent in the process. A daily quota cools it for an hour. One answer has `call_budget_secs` (150 s) across
  all models, and a hanging model may use at most two thirds of what's left while others are waiting.
- Provider extras that must be echoed back, such as Gemini 3's tool-call thought signatures and Claude's thinking
  blocks, are preserved.

**Anthropic (native API):** a route on `api.anthropic.com` speaks the Messages API directly (or set `api = "anthropic"`
on any route, e.g. a proxy; `api = "openai"` forces the compatibility endpoint). Routes of both kinds mix freely in one
fallback chain.

```toml
[provider]
base_url = "https://api.anthropic.com/v1"
model = "claude-sonnet-5-5"
api_key_env = "ANTHROPIC_API_KEY"       # sent as x-api-key; the key itself lives in ~/.pagurus/env

[[provider.fallback]]                   # OpenAI-compatible, tried if Claude is rate-limited or overloaded
base_url = "https://openrouter.ai/api/v1"
model = "deepseek/deepseek-v4.1-flash-free"
api_key_env = "OPENROUTER_API_KEY"
```

- **Prompt caching:** the system prompt is built stable-first (personality, rules, skills, memory; the session time
  last). On Anthropic pagurus marks cache breakpoints on the tool list, the stable system prompt and the two latest
  messages, so each tool round re-reads the conversation from the cache. OpenRouter's Claude and Gemini models get a
  breakpoint on the system prompt; OpenAI, Gemini and DeepSeek cache the unchanged start on their own.
  `PAGURUS_DEBUG=1` shows tokens in and out per call and how many came from the cache.
- An overloaded Claude (HTTP 529) is skipped for a while like a rate-limited model; running out of `max_tokens` gives
  the same plain message as elsewhere.

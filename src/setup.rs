//! `pagurus setup`: interactive first-run wizard. Secrets go to the
//! chmod-600 `$PAGURUS_HOME/env`, never into config.toml.

use crate::config;
use crate::llm::{Llm, Msg};
use crate::util::{expand, now, one_line, read, set_env_value, write_atomic};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn line() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

fn ask(q: &str, default: &str) -> String {
    if default.is_empty() {
        eprint!("{q}: ");
    } else {
        eprint!("{q} [{default}]: ");
    }
    let _ = std::io::stderr().flush();
    let s = line();
    if s.is_empty() {
        default.to_string()
    } else {
        s
    }
}

fn yes(q: &str, default: bool) -> bool {
    eprint!("{q} {} ", if default { "[Y/n]" } else { "[y/N]" });
    let _ = std::io::stderr().flush();
    match line().to_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    }
}

/// Read without echo (stty); falls back to plain input when stdin isn't a terminal.
fn secret(q: &str) -> String {
    eprint!("{q}: ");
    let _ = std::io::stderr().flush();
    // hide typing where stty exists (Linux, macOS); on Windows input stays visible
    let stty = |arg: &str| Command::new("stty").arg(arg).stderr(Stdio::null()).status();
    let _ = stty("-echo");
    let s = line();
    let _ = stty("echo");
    eprintln!();
    s
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set `key = value` inside `[section]` of config.toml, adding the line (or the
/// section) when missing. `value` is a TOML literal. Comments elsewhere survive.
fn set_toml(home: &Path, section: &str, key: &str, value: &str) -> Result<(), String> {
    let p = home.join("config.toml");
    let mut lines: Vec<String> = read(&p).lines().map(String::from).collect();
    let header = format!("[{section}]");
    let mut current = String::new();
    let mut header_at = None;
    for (i, l) in lines.iter_mut().enumerate() {
        let t = l.trim();
        if t.starts_with('[') {
            current = t.to_string();
            if t == header {
                header_at = Some(i);
            }
            continue;
        }
        let is_key = t.strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('='));
        if current == header && is_key {
            *l = format!("{key} = {value}");
            return write_atomic(&p, &(lines.join("\n") + "\n"));
        }
    }
    match header_at {
        Some(i) => lines.insert(i + 1, format!("{key} = {value}")),
        None => {
            lines.push(String::new());
            lines.push(header);
            lines.push(format!("{key} = {value}"));
        }
    }
    write_atomic(&p, &(lines.join("\n") + "\n"))
}

/// OpenRouter's free models that support tool calling, newest first.
/// Free models with tool support, newest first, for the providers that list them.
fn free_tool_models(base: &str, key: Option<&str>) -> Vec<String> {
    if base.contains("generativelanguage.googleapis.com") {
        // Google AI Studio's free tier: Flash-Lite has the most generous limits
        return vec!["gemini-3.5-flash-lite".into(), "gemini-3.5-flash".into()];
    }
    let mut req = ureq::get(&format!("{base}/models")).timeout(Duration::from_secs(20));
    if let Some(k) = key {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let Ok(resp) = req.call() else { return vec![] };
    let Ok(v) = resp.into_json::<Value>() else { return vec![] };
    let local =
        base.contains("localhost") || base.contains("127.0.0.1") || base.contains(":1234") || base.contains(":11434");
    if local {
        // LM Studio / Ollama: everything that's installed, minus embedding models
        return v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str())
            .filter(|id| !id.contains("embed"))
            .take(15)
            .map(String::from)
            .collect();
    }
    let mut models: Vec<(u64, String)> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        // OpenRouter marks free models `:free` and lists tool support; OrcaRouter uses `-free`
        .filter(|m| m["id"].as_str().is_some_and(|id| id.ends_with(":free") || id.ends_with("-free")))
        .filter(|m| m["supported_parameters"].as_array().is_none_or(|a| a.iter().any(|p| p == "tools")))
        .map(|m| (m["created"].as_u64().unwrap_or(0), m["id"].as_str().unwrap_or("").to_string()))
        .collect();
    models.sort_by_key(|m| std::cmp::Reverse(m.0));
    models.into_iter().take(10).map(|m| m.1).collect()
}

fn telegram(home: &Path) -> Result<(), String> {
    eprintln!("  In Telegram, message @BotFather, send /newbot, and paste the token it gives you.");
    let token = secret("  bot token (hidden)");
    if token.is_empty() {
        return Ok(());
    }
    let http = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).build();
    let call = |m: &str| -> Result<Value, String> {
        http.get(&format!("https://api.telegram.org/bot{token}/{m}"))
            .timeout(Duration::from_secs(70))
            .call()
            .map_err(|e| e.to_string().replace(&token, "***"))?
            .into_json()
            .map_err(|e| e.to_string())
    };
    let me = call("getMe").map_err(|e| format!("Telegram rejected the token: {e}"))?;
    let bot = me["result"]["username"].as_str().unwrap_or("your bot").to_string();
    eprintln!("  ✓ bot @{bot}. Now send any message to @{bot} from your own account (waiting up to 3 minutes)…");
    let (deadline, mut offset, mut found) = (now() + 180, 0i64, None);
    while found.is_none() && now() < deadline {
        let v = call(&format!("getUpdates?timeout=25&offset={offset}"))?;
        for u in v["result"].as_array().into_iter().flatten() {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let c = &u["message"]["chat"];
            if let Some(id) = c["id"].as_i64() {
                let name = c["username"].as_str().or(c["first_name"].as_str()).unwrap_or("?").to_string();
                found = Some((id, name));
            }
        }
    }
    let Some((id, name)) = found else {
        eprintln!("  no message arrived; run `pagurus setup` again when ready");
        return Ok(());
    };
    let _ = call(&format!("getUpdates?timeout=0&offset={offset}")); // acknowledge, so the daemon starts clean
    if !yes(&format!("  got a message from {name} (chat {id}). Is that you?"), true) {
        eprintln!("  skipped: pagurus only ever answers the chat ids you confirm");
        return Ok(());
    }
    set_env_value(home, "PAGURUS_TELEGRAM_TOKEN", &token)?;
    set_toml(home, "telegram", "token_env", &quote("PAGURUS_TELEGRAM_TOKEN"))?;
    set_toml(home, "telegram", "allowed_chat_ids", &format!("[{id}]"))?;
    eprintln!("  ✓ Telegram connected; only chat {id} will be answered");
    Ok(())
}

const LAUNCHD_LABEL: &str = "ai.pagurus.daemon";

/// Discord / Slack / email: secrets go to the env file, only their names to config.toml.
fn more_channels(home: &Path) -> Result<(), String> {
    let list = |s: &str, num: bool| -> String {
        let items: Vec<String> = s
            .split([',', ' '])
            .filter(|x| !x.is_empty() && (!num || x.parse::<u64>().is_ok()))
            .map(|x| if num { x.to_string() } else { quote(x) })
            .collect();
        format!("[{}]", items.join(", "))
    };
    if yes("  Discord? (developer portal → bot token; enable MESSAGE CONTENT INTENT; invite the bot)", false) {
        let token = secret("  bot token");
        let ids = ask("  your Discord user id(s) (Developer Mode → Copy User ID)", "");
        if token.is_empty() || !ids.split([',', ' ']).any(|x| x.parse::<u64>().is_ok_and(|n| n > 0)) {
            return Err("discord needs a token and your numeric user id".into());
        }
        crate::util::set_env_value(home, "PAGURUS_DISCORD_TOKEN", &token)?;
        set_toml(home, "discord", "token_env", &quote("PAGURUS_DISCORD_TOKEN"))?;
        set_toml(home, "discord", "allowed_user_ids", &list(&ids, true))?;
        eprintln!("  ✓ Discord configured; only those users are answered (DMs and @mentions)");
    }
    if yes("  Slack? (a Socket Mode app: bot token xoxb-… and app token xapp-…)", false) {
        let (bot, app) = (secret("  bot token (xoxb-…)"), secret("  app-level token (xapp-…)"));
        let ids = ask("  your Slack member id(s) (U…)", "");
        if !bot.starts_with("xoxb-") || !app.starts_with("xapp-") || ids.trim().is_empty() {
            return Err("slack needs an xoxb- bot token, an xapp- app token and your member id".into());
        }
        crate::util::set_env_value(home, "PAGURUS_SLACK_BOT_TOKEN", &bot)?;
        crate::util::set_env_value(home, "PAGURUS_SLACK_APP_TOKEN", &app)?;
        set_toml(home, "slack", "allowed_user_ids", &list(&ids, false))?;
        eprintln!("  ✓ Slack configured; only those members are answered (DMs and @mentions)");
    }
    if yes("  Email? (a mailbox for pagurus with IMAP + SMTP and an app password)", false) {
        let user = ask("  pagurus's email address / login", "");
        let imap = ask("  IMAP host (TLS, port 993)", "");
        let smtp = ask("  SMTP host", &imap.replacen("imap.", "smtp.", 1));
        let port = ask("  SMTP port (465 TLS / 587 STARTTLS)", "465");
        let pass = secret("  password / app password");
        let from = ask("  your own address(es) pagurus should answer", "");
        if user.is_empty() || imap.is_empty() || pass.is_empty() || !from.contains('@') || port.parse::<u16>().is_err()
        {
            return Err("email needs a login, hosts, a password and your address".into());
        }
        crate::util::set_env_value(home, "PAGURUS_EMAIL_PASSWORD", &pass)?;
        for (k, v) in [
            ("username", quote(&user)),
            ("imap_host", quote(&imap)),
            ("smtp_host", quote(&smtp)),
            ("smtp_port", port),
            ("allowed_senders", list(&from, false)),
        ] {
            set_toml(home, "email", k, &v)?;
        }
        eprintln!("  ✓ email configured; only mail from those senders is read or answered");
    }
    Ok(())
}

fn launchd_plist() -> std::path::PathBuf {
    expand(&format!("~/Library/LaunchAgents/{LAUNCHD_LABEL}.plist"))
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// macOS: a launchd agent that starts `pagurus daemon` at login and restarts it
/// if it stops (not after a clean exit). Logs go to $PAGURUS_HOME/daemon.log.
fn service_macos(home: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let esc = |p: &Path| p.display().to_string().replace('&', "&amp;").replace('<', "&lt;");
    let log = home.join("daemon.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>PAGURUS_HOME</key><string>{home}</string>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>15</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = esc(&exe),
        home = esc(home),
        log = esc(&log),
    );
    let path = launchd_plist();
    write_atomic(&path, &plist)?;
    let domain = format!("gui/{}", uid());
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .stderr(Stdio::null())
        .status();
    let ok = Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).status().is_ok_and(|s| s.success());
    if !ok {
        return Err(format!("wrote {} but launchctl couldn't load it; start it with `pagurus daemon`", path.display()));
    }
    std::thread::sleep(Duration::from_secs(2));
    if service_running() {
        eprintln!("  ✓ running, and starts at every login (log: {})", log.display());
        Ok(())
    } else {
        Err(format!("loaded, but it isn't running; see {}", log.display()))
    }
}

/// Is the background service up (systemd on Linux, launchd on macOS)?
fn service_running() -> bool {
    if cfg!(target_os = "macos") {
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("state = running"))
    } else {
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "pagurus.service"])
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// `pagurus service install|remove|status`
pub fn service_cmd(home: &Path, args: &[String]) -> Result<(), String> {
    let mac = cfg!(target_os = "macos");
    match args.first().map(String::as_str).unwrap_or("status") {
        "install" if mac => service_macos(home),
        "install" if cfg!(target_os = "linux") => service(home),
        "install" => {
            Err("auto-start is available on Linux (systemd) and macOS (launchd); elsewhere run `pagurus daemon`".into())
        }
        "remove" if mac => {
            let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid())]).status();
            let _ = std::fs::remove_file(launchd_plist());
            eprintln!("  removed: pagurus no longer starts at login");
            Ok(())
        }
        "remove" => {
            let _ = Command::new("systemctl").args(["--user", "disable", "--now", "pagurus.service"]).status();
            let _ = std::fs::remove_file(expand("~/.config/systemd/user/pagurus.service"));
            eprintln!("  removed: pagurus no longer starts at boot");
            Ok(())
        }
        _ => {
            let installed =
                if mac { launchd_plist().exists() } else { expand("~/.config/systemd/user/pagurus.service").exists() };
            println!(
                "auto-start: {} · running: {}",
                if installed { "on" } else { "off (pagurus service install)" },
                if service_running() { "yes" } else { "no" }
            );
            Ok(())
        }
    }
}

fn service(home: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let unit = format!(
        "[Unit]\nDescription=pagurus personal agent\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nEnvironment=PAGURUS_HOME={home}\nEnvironmentFile=-{home}/env\nExecStart={exe} daemon\n\
         Restart=on-failure\nRestartSec=15\nNice=10\nMemoryMax=64M\nNoNewPrivileges=true\n\n\
         [Install]\nWantedBy=default.target\n",
        home = home.display(),
        exe = exe.display()
    );
    let path = expand("~/.config/systemd/user/pagurus.service");
    write_atomic(&path, &unit)?;
    let sc = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status().is_ok_and(|s| s.success());
    if !(sc(&["daemon-reload"]) && sc(&["enable", "pagurus.service"]) && sc(&["restart", "pagurus.service"])) {
        return Err(format!("wrote {} but systemctl --user failed; start it with `pagurus daemon`", path.display()));
    }
    eprintln!("  ✓ service running (logs: journalctl --user -u pagurus -f)");
    let linger = Command::new("loginctl")
        .args(["show-user", &std::env::var("USER").unwrap_or_default(), "-p", "Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false);
    if !linger {
        eprintln!("  ! to keep it running after you log out, run once: sudo loginctl enable-linger $USER");
    }
    Ok(())
}

pub fn run(home: &Path) -> Result<(), String> {
    eprintln!("pagurus setup · {}\n", home.display());
    crate::init(home)?;

    eprintln!(
        "\n1) Model provider (the first three have free models)\n   1. OpenRouter                 openrouter.ai\n   \
         2. OrcaRouter                 orcarouter.ai\n   3. Google AI Studio (Gemini)  aistudio.google.com\n   4. OpenAI\n   \
         5. LM Studio (local model, no key; start its server first)\n   6. Ollama / llama.cpp (local model, no key)\n   \
         7. Other OpenAI-compatible endpoint"
    );
    let previous = config::load(home)?.provider;
    let choice = ask("choose", "1");
    let base = match choice.as_str() {
        "2" => "https://api.orcarouter.ai/v1".to_string(),
        "3" => "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
        "4" => "https://api.openai.com/v1".to_string(),
        "5" => format!("http://{}/v1", ask("LM Studio host:port", "localhost:1234")),
        "6" => format!("http://{}/v1", ask("Ollama host:port", "localhost:11434")),
        "7" => ask("base URL (ending in /v1)", ""),
        _ => "https://openrouter.ai/api/v1".to_string(),
    };
    let same_provider = previous.base_url.trim_end_matches('/') == base.trim_end_matches('/');
    set_toml(home, "provider", "base_url", &quote(&base))?;
    // models that think before answering need room; local ones cost nothing, so give them more
    let want_tokens = if matches!(choice.as_str(), "5" | "6") { 8192 } else { 4096 };
    if previous.max_tokens < want_tokens {
        set_toml(home, "provider", "max_tokens", &want_tokens.to_string())?;
    }
    if !same_provider {
        // a model id from another provider would only fail here
        set_toml(home, "provider", "fallback_models", "[]")?;
    }
    if choice != "5" && choice != "6" {
        let has = std::env::var("PAGURUS_API_KEY").is_ok_and(|k| !k.is_empty());
        let k = secret(if has { "API key (hidden; Enter keeps the current one)" } else { "API key (hidden)" });
        if !k.is_empty() {
            set_env_value(home, "PAGURUS_API_KEY", &k)?;
            std::env::set_var("PAGURUS_API_KEY", &k);
        }
    }

    let key = std::env::var("PAGURUS_API_KEY").ok().filter(|k| !k.is_empty());
    let local = matches!(choice.as_str(), "5" | "6");
    let mut suggestions = free_tool_models(&base, key.as_deref());
    // a local server that isn't running is the usual snag: say so, and let the user fix it and retry
    while local && suggestions.is_empty() {
        let how = if choice == "5" {
            "Open LM Studio, load a model that supports tool calls (e.g. Qwen), and start the server: Developer tab → \
             Start Server, or run `lms server start` in a terminal."
        } else {
            "Start Ollama (`ollama serve`) and pull a model that supports tool calls, e.g. `ollama pull qwen3`."
        };
        eprintln!("\n   ✗ No models found at {base}: the server isn't running or has no model.\n   {how}");
        if !yes("   Try again?", true) {
            break;
        }
        suggestions = free_tool_models(&base, key.as_deref());
    }
    if !suggestions.is_empty() {
        eprintln!(
            "\n   {}",
            if base.contains("localhost") || base.contains("127.0.0.1") {
                "Models on this server (pick one that supports tool calls, e.g. Qwen, Llama 3.1+, Mistral):"
            } else {
                "Free models with tool support (free endpoints may log prompts; check their data policy):"
            }
        );
        for (i, m) in suggestions.iter().enumerate() {
            eprintln!("   {:>2}. {m}", i + 1);
        }
    }
    let current = if same_provider { previous.model } else { String::new() };
    let default = if current.is_empty() { suggestions.first().cloned().unwrap_or_default() } else { current };
    let mut picked = ask("model id or number", &default);
    for _ in 0..3 {
        if !picked.trim().is_empty() {
            break;
        }
        eprintln!("   a model is needed: type its id (or a number from the list above)");
        picked = ask("model id or number", &default);
    }
    if picked.trim().is_empty() {
        return Err("no model chosen; run `pagurus setup` again once your model server is running".into());
    }
    let model = match picked.parse::<usize>() {
        Ok(n) if (1..=suggestions.len()).contains(&n) => suggestions[n - 1].clone(),
        _ => picked,
    };
    set_toml(home, "provider", "model", &quote(&model))?;
    let fallbacks: Vec<String> = suggestions.iter().filter(|m| **m != model).take(2).cloned().collect();
    if !fallbacks.is_empty()
        && yes(&format!("   use {} as fallbacks when it's rate-limited?", fallbacks.join(" and ")), true)
    {
        let list = fallbacks.iter().map(|m| quote(m)).collect::<Vec<_>>().join(", ");
        set_toml(home, "provider", "fallback_models", &format!("[{list}]"))?;
    }

    eprint!("   testing the model… ");
    let cfg = config::load(home)?;
    let test = Llm::new(&cfg.provider)
        .and_then(|l| l.chat(&[Msg::system("Reply with exactly: ok"), Msg::user("ping")], &json!([])));
    match test {
        Ok(r) => eprintln!("✓ it answered: {}", one_line(r.content.as_deref().unwrap_or(""), 60)),
        Err(e) => eprintln!("✗ {}\n   fix base_url/model/key, then run `pagurus doctor`", one_line(&e, 200)),
    }

    if yes("\n2) Connect a Telegram bot so you can talk to pagurus from your phone?", true) {
        if let Err(e) = telegram(home) {
            eprintln!("  ✗ {e}");
        }
    }

    if yes("\n   Also connect Discord, Slack or email? (optional)", false) {
        if let Err(e) = more_channels(home) {
            eprintln!("  ✗ {e}");
        }
    }

    let has_systemd =
        cfg!(target_os = "linux") && Command::new("systemctl").arg("--version").stdout(Stdio::null()).status().is_ok();
    if has_systemd && yes("\n3) Run pagurus in the background as a service (starts on boot)?", true) {
        if let Err(e) = service(home) {
            eprintln!("  ✗ {e}");
        }
    }
    if cfg!(target_os = "macos")
        && yes("\n3) Start pagurus automatically when you log in (and restart it if it stops)?", true)
    {
        if let Err(e) = service_macos(home) {
            eprintln!("  ✗ {e}");
        }
    }
    if base.contains("localhost:1234") {
        eprintln!(
            "\n   Note: LM Studio must be running for pagurus to think. In LM Studio's settings, let it start at login, \
             start its server automatically and load your model, or run `lms server start` and `lms load <model>` after a restart."
        );
    }
    eprintln!(
        "\nDone. Next: `pagurus` to chat · `pagurus doctor` to check · `pagurus eval` to test learning with your model"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_toml_edits_in_place_and_adds_sections() {
        let home = std::env::temp_dir().join(format!("pagurus-setup-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), crate::config::DEFAULT_TOML).unwrap();
        super::set_toml(&home, "provider", "model", "\"x/y:free\"").unwrap();
        super::set_toml(&home, "provider", "fallback_models", "[\"a\", \"b\"]").unwrap();
        super::set_toml(&home, "telegram", "allowed_chat_ids", "[42]").unwrap();
        let c = crate::config::load(&home).unwrap();
        assert_eq!(c.provider.model, "x/y:free");
        assert_eq!(c.provider.fallback_models, vec!["a", "b"]);
        assert_eq!(c.telegram.unwrap().allowed_chat_ids, vec![42]);
        assert_eq!(c.learning.mode, "auto", "other settings untouched");
        // a config written before fallback_models existed gets the key inserted
        std::fs::write(home.join("config.toml"), "[provider]\nmodel = \"m\"\n\n[agent]\nmax_iterations = 5\n").unwrap();
        super::set_toml(&home, "provider", "fallback_models", "[\"z\"]").unwrap();
        let c = crate::config::load(&home).unwrap();
        assert_eq!((c.provider.fallback_models, c.agent.max_iterations), (vec!["z".to_string()], 5));
    }
}

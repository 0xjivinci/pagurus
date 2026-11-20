mod agent;
#[cfg(feature = "art")]
mod art;
#[cfg(feature = "art")]
mod art_eth;
mod ask;
mod board;
mod bundled;
mod channel;
mod config;
mod daemon;
mod eval;
mod files;
mod hub;
mod import;
mod learn;
mod llm;
mod mcp;
mod peer;
mod search;
mod procs;
mod remote;
mod setup;
mod skills;
mod store;
mod tasks;
mod tools;
mod util;
mod voice;

use std::io::{IsTerminal, Read};
use std::path::Path;

const USAGE: &str = "pagurus — a tiny personal agent with user-governed learning

usage:
  pagurus setup                 guided setup: model, key, Telegram, service
  pagurus init                  create ~/.pagurus (or $PAGURUS_HOME) non-interactively
  pagurus [chat]                interactive session
  pagurus run <prompt…>         one-shot (also reads the prompt from stdin)
  pagurus daemon                heartbeat + chat gateway: Telegram, Discord, Slack, email (the background service)
  pagurus service [install|remove|status]   start pagurus automatically (Linux: systemd, macOS: launchd)
  pagurus learn [list|show|approve|reject|history|undo|lock|tidy] …
                               audit and govern what it learned
  pagurus skills [install]      list skills with use counts / add missing bundled skills
  pagurus skills install <github url | https url | path> [--yes] [--force]
                               add one skill from elsewhere (scanned, undoable)
  pagurus rollback [n]          list file checkpoints / restore a file pagurus wrote or edited
  pagurus skills archive|restore <name>   take a skill out of the prompt / bring it back
  pagurus skills curate         review the skill library: fix failing skills, merge duplicates
  pagurus model [n | background n | delegate n]   list or switch the chat / background / sub-agent model
  pagurus import <dir> [--apply]  bring memory + skills over from Hermes Agent or any agentskills.io folder
  pagurus eval [cases.json]     test the learning loop with your model
  pagurus mcp [list | test <name> [tool [json]]]   MCP servers: status and tools / try one
  pagurus board [list | <id> | add <title> [-- body] | unblock|block|done|comment <id> …]
                               the task board the daemon works through in the background
  pagurus peer [list | pair | add <name> <url> <code> | remove <name> | ask <name> <task…>]
                               pagurus agents on other devices that hand each other tasks
  pagurus host [list | test <name>]   machines the shell tool reaches over ssh / try one
  pagurus doctor                check config, budgets and footprint
";

fn main() {
    util::alias_legacy_env(); // HERMIT_* from before the rename count as PAGURUS_*
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = run(&args);
    mcp::shutdown();
    if let Err(e) = r {
        eprintln!("pagurus: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let home = util::home_dir();
    util::load_env(&home);
    let cmd = args.first().map(String::as_str).unwrap_or("chat");
    match cmd {
        "init" => init(&home),
        "setup" => setup::run(&home),
        "eval" => eval::run(&home, &args[1..]),
        #[cfg(feature = "art")]
        "art" => art::cli(&args[1..]).map(|out| println!("{}", out.trim_end())),
        "service" => setup::service_cmd(&home, &args[1..]),
        "model" => {
            let (msg, _) = config::model_command(&home, &config::load(&home)?, &args[1..].join(" "))?;
            println!("{msg}");
            Ok(())
        }
        "import" => import::run(&home, &config::load(&home)?, &args[1..]),
        "chat" => agent::repl(home.clone(), config::load(&home)?),
        "run" => {
            let mut prompt = args[1..].join(" ");
            if prompt.trim().is_empty() && !std::io::stdin().is_terminal() {
                std::io::stdin().read_to_string(&mut prompt).map_err(|e| e.to_string())?;
            }
            if prompt.trim().is_empty() {
                return Err("usage: pagurus run <prompt>".into());
            }
            let mut a = agent::Agent::new(home.clone(), config::load(&home)?, false, "run")?;
            let t0 = util::now();
            println!("{}", a.turn(prompt.trim())?.trim());
            if a.has_unreviewed() {
                a.review();
            }
            if let Some(n) = learn::activity_note(&home, t0) {
                eprintln!("{n} (`pagurus learn`)");
            }
            Ok(())
        }
        "daemon" => daemon::run(home.clone(), config::load(&home)?),
        "learn" => learn::cli(&home, &args[1..]),
        "skills" if args.get(1).is_some_and(|a| a == "install") && args.len() > 2 => hub::cli(&home, &config::load(&home)?, &args[2..]),
        "rollback" => {
            println!("{}", files::rollback(&home, &args[1..].join(" "))?);
            Ok(())
        }
        "skills" if args.get(1).is_some_and(|a| a == "install") => {
            let (installed, kept) = bundled::install(&home)?;
            println!("installed: {}", if installed.is_empty() { "nothing new".into() } else { installed.join(", ") });
            if !kept.is_empty() {
                println!("kept your existing: {}", kept.join(", "));
            }
            Ok(())
        }
        "skills" if args.get(1).is_some_and(|a| a == "curate") => {
            println!("{}", agent::curate(&home, &config::load(&home)?)?);
            Ok(())
        }
        "skills" if matches!(args.get(1).map(String::as_str), Some("archive" | "restore")) => {
            let name = args.get(2).ok_or("usage: pagurus skills archive|restore <name>")?;
            let r = if args[1] == "archive" { skills::archive(&home, name) } else { skills::restore(&home, name) };
            println!("{}", r?);
            Ok(())
        }
        "skills" => {
            print!("{}", skills::list_text(&home));
            Ok(())
        }
        "doctor" => doctor(&home),
        "mcp" => mcp::cli(&config::load(&home)?, &args[1..]),
        "board" => {
            let a: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            println!("{}", board::command(&home, &config::load(&home)?, &a)?);
            Ok(())
        }
        "peer" | "peers" => {
            println!("{}", peer::cli(&home, &config::load(&home)?, &args[1..])?);
            Ok(())
        }
        "host" => remote::cli(&home, &config::load(&home)?, &args[1..]),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        "version" | "-V" | "--version" => {
            println!("pagurus {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(format!("unknown command `{cmd}`\n\n{USAGE}")),
    }
}

const SOUL: &str = "# Soul

You are pagurus, a small, patient personal agent living on the user's own hardware.
- Be direct and brief: you run on a tiny computer and every token costs time.
- Prefer doing over describing: use a tool when it answers the question.
- When unsure, ask one short question.
- You learn as you go, and the user can see, undo or lock anything you learn. Trust the tool result about whether something was saved.
";

const HEARTBEAT: &str = "# Heartbeat

Standing tasks the daemon checks every `daemon.heartbeat_minutes`.
Each line starting with `- ` is a task; everything else is ignored.
If nothing needs attention the agent answers HEARTBEAT_OK and stays quiet.

Examples (remove the `#` to enable):
# - Run `df -h`; warn me if any filesystem is above 85% full.
# - Run `vcgencmd measure_temp`; warn me if the SoC is above 70'C.
";

fn init(home: &Path) -> Result<(), String> {
    for d in ["workspace", "skills", "sessions", "learn/pending", "learn/rejected"] {
        std::fs::create_dir_all(home.join(d)).map_err(|e| format!("{}: {e}", home.join(d).display()))?;
    }
    // keys, transcripts and memory: for this user only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    }
    let files = [
        ("config.toml", config::DEFAULT_TOML),
        ("SOUL.md", SOUL),
        ("HEARTBEAT.md", HEARTBEAT),
        ("USER.md", ""),
        ("MEMORY.md", ""),
    ];
    for (name, body) in files {
        let p = home.join(name);
        if p.exists() {
            println!("  kept     {}", p.display());
        } else {
            util::write_atomic(&p, body)?;
            println!("  created  {}", p.display());
        }
    }
    let (installed, _) = bundled::install(home)?;
    if !installed.is_empty() {
        println!("  skills   {}", installed.join(", "));
    }
    if std::env::args().nth(1).as_deref() != Some("setup") {
        println!("\nNext: `pagurus setup` for a guided setup, or edit config.toml and put PAGURUS_API_KEY=… in {}", home.join("env").display());
    }
    Ok(())
}

fn doctor(home: &Path) -> Result<(), String> {
    let cfg = config::load(home)?;
    let ok = |b: bool| if b { "ok " } else { "!! " };
    let key = std::env::var(&cfg.provider.api_key_env).is_ok_and(|k| !k.is_empty());
    let mem = util::read(&learn::file_for(home, learn::Target::Memory)).chars().count();
    let user = util::read(&learn::file_for(home, learn::Target::User)).chars().count();
    let (n, bytes) = std::fs::read_dir(home.join("sessions"))
        .map(|rd| rd.filter_map(|e| e.ok()?.metadata().ok()).fold((0, 0), |(n, b), m| (n + 1, b + m.len())))
        .unwrap_or((0, 0));
    println!("pagurus {}  ·  home {}", env!("CARGO_PKG_VERSION"), home.display());
    println!("{}model      {}", ok(!cfg.provider.model.is_empty()), if cfg.provider.model.is_empty() { "(unset)" } else { &cfg.provider.model });
    println!("   endpoint   {}", cfg.provider.base_url);
    if !cfg.provider.fallback_models.is_empty() {
        println!("   fallbacks  {}", cfg.provider.fallback_models.join(", "));
    }
    for f in &cfg.provider.fallback {
        let env = f.api_key_env.as_deref().unwrap_or(&cfg.provider.api_key_env);
        let has = std::env::var(env).is_ok_and(|k| !k.trim().is_empty());
        println!("   fallback   {} @ {} (key {env}: {})", f.model, f.base_url.as_deref().unwrap_or(&cfg.provider.base_url), if has { "set" } else { "MISSING" });
    }
    println!("{}api key    ${} {}", ok(key), cfg.provider.api_key_env, if key { "set" } else { "not set (fine for a local Ollama)" });
    println!("   learning   {}  ·  {} pending", cfg.learning.mode, learn::pending(home).len());
    println!("{}MEMORY.md  {mem}/{} chars", ok(mem <= cfg.learning.memory_char_limit), cfg.learning.memory_char_limit);
    println!("{}USER.md    {user}/{} chars", ok(user <= cfg.learning.user_char_limit), cfg.learning.user_char_limit);
    println!("   skills     {}", tools::skill_index(home).len());
    println!("   sessions   {n} files, {} KiB", bytes / 1024);
    println!("   shell      {} (allow: {})", cfg.tools.shell, cfg.tools.shell_allow.join(" "));
    println!("   heartbeat  {} task(s) every {}m", daemon::heartbeat_tasks(home).len(), cfg.daemon.heartbeat_minutes);
    if let Some(t) = &cfg.telegram {
        let tok = std::env::var(&t.token_env).is_ok();
        println!("{}telegram   ${} {} · {} allowed chat(s)", ok(tok && !t.allowed_chat_ids.is_empty()), t.token_env, if tok { "set" } else { "NOT set" }, t.allowed_chat_ids.len());
    }
    println!("   board      {} · {} worker(s)", board::summary(home), cfg.board.workers);
    println!("{}", peer::doctor_line(home, &cfg));
    for l in remote::status(&cfg) {
        println!("{l}");
    }
    let set = |n: &str| std::env::var(n).is_ok_and(|v| !v.trim().is_empty());
    if let Some(d) = &cfg.discord {
        let tok = set(&d.token_env);
        println!("{}discord    ${} {} · {} allowed user(s)", ok(tok && !d.allowed_user_ids.is_empty()), d.token_env, if tok { "set" } else { "NOT set" }, d.allowed_user_ids.len());
    }
    if let Some(s) = &cfg.slack {
        let tok = set(&s.bot_token_env) && set(&s.app_token_env);
        println!("{}slack      ${} + ${} {} · {} allowed user(s)", ok(tok && !s.allowed_user_ids.is_empty()), s.bot_token_env, s.app_token_env, if tok { "set" } else { "NOT set" }, s.allowed_user_ids.len());
    }
    if let Some(e) = &cfg.email {
        let pw = set(&e.password_env);
        println!(
            "{}email      {} via {} / {}:{} · ${} {} · {} allowed sender(s){}",
            ok(pw && !e.allowed_senders.is_empty()),
            e.username,
            e.imap_host,
            e.smtp_host,
            e.smtp_port,
            e.password_env,
            if pw { "set" } else { "NOT set" },
            e.allowed_senders.len(),
            if e.require_auth { "" } else { " · require_auth off (From can be spoofed)" }
        );
    }
    for l in mcp::status(&cfg, false) {
        println!("{l}");
    }
    if let Some(kb) = util::rss_kb() {
        println!("   rss        {kb} KiB");
    }
    let risks = risks(home, &cfg);
    println!(
        "{}security   {}",
        ok(risks.is_empty()),
        if risks.is_empty() { "no risky settings found".to_string() } else { format!("{} issue(s):", risks.len()) }
    );
    for r in risks {
        println!("   · {r}");
    }
    Ok(())
}

/// Settings and file permissions that put the keys or the machine at risk.
fn risks(home: &Path, cfg: &config::Config) -> Vec<String> {
    let mut v = Vec::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).ok().map(|m| m.permissions().mode() & 0o777);
        let env = home.join("env");
        if let Some(m) = mode(&env).filter(|m| m & 0o077 != 0) {
            v.push(format!("{} is mode {m:o}: other users can read your API keys. Fix: chmod 600 {}", env.display(), env.display()));
        }
        if let Some(m) = mode(home).filter(|m| m & 0o077 != 0) {
            v.push(format!(
                "{} is mode {m:o}: other users can read your conversations and memory. Fix: chmod 700 {}",
                home.display(),
                home.display()
            ));
        }
        let conf = home.join("config.toml");
        if let Some(m) = mode(&conf).filter(|m| m & 0o022 != 0) {
            v.push(format!("{} is writable by others (mode {m:o}): they could widen what pagurus runs. Fix: chmod 600 it", conf.display()));
        }
    }
    let conf = util::read(&home.join("config.toml"));
    if util::redact(&conf) != conf {
        v.push("config.toml contains what looks like a key or token: move it to the env file and name the variable instead".into());
    }
    let bad = tools::unsafe_allow_entries(&cfg.tools.shell_allow);
    if !bad.is_empty() {
        v.push(format!("tools.shell_allow lets these change the system without asking: {}", bad.join(", ")));
    }
    let user_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    for r in &cfg.tools.write_roots {
        let p = util::expand(r);
        let p = p.canonicalize().unwrap_or(p);
        let covers = |x: &Path| x.starts_with(&p) || x.canonicalize().is_ok_and(|x| x.starts_with(&p));
        if covers(home) || p == Path::new("/") || p.starts_with("/etc") || user_home.as_deref().is_some_and(covers) {
            v.push(format!("tools.write_roots has {r}: write_file could overwrite pagurus's own config, ~/.ssh or system files"));
        }
    }
    let plain_http = |url: &str| {
        let host = url.strip_prefix("http://").map(|r| r.split(['/', ':']).next().unwrap_or(""));
        host.is_some_and(|h| {
            let local = h == "localhost" || h.ends_with(".local") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
                std::net::IpAddr::V4(a) => a.is_loopback() || a.is_private() || a.is_link_local(),
                std::net::IpAddr::V6(a) => a.is_loopback(),
            });
            !local
        })
    };
    let has_key = |env: &str| std::env::var(env).is_ok_and(|k| !k.trim().is_empty());
    if plain_http(&cfg.provider.base_url) && has_key(&cfg.provider.api_key_env) {
        v.push(format!("{} is plain http: your API key would cross the internet unencrypted. Use https", cfg.provider.base_url));
    }
    for f in &cfg.provider.fallback {
        let url = f.base_url.as_deref().unwrap_or(&cfg.provider.base_url);
        if plain_http(url) && has_key(f.api_key_env.as_deref().unwrap_or(&cfg.provider.api_key_env)) && url != cfg.provider.base_url {
            v.push(format!("fallback {url} is plain http: its API key would cross the internet unencrypted"));
        }
    }
    if let Some(t) = &cfg.telegram {
        if t.allowed_chat_ids.contains(&0) {
            v.push("telegram.allowed_chat_ids contains 0; the daemon refuses to start".into());
        }
        if t.allowed_chat_ids.iter().any(|id| *id < 0) && !t.allowed_chat_ids.iter().any(|id| *id > 0) {
            v.push("telegram.allowed_chat_ids has only groups: add your own user id, only listed people are answered in a group".into());
        }
    }
    if cfg.discord.as_ref().is_some_and(|d| d.allowed_user_ids.is_empty() || d.allowed_user_ids.contains(&0)) {
        v.push("discord.allowed_user_ids is empty or contains 0; the daemon refuses to start".into());
    }
    if cfg.slack.as_ref().is_some_and(|s| s.allowed_user_ids.iter().all(|u| u.trim().is_empty())) {
        v.push("slack.allowed_user_ids is empty; the daemon refuses to start".into());
    }
    if let Some(e) = &cfg.email {
        if e.allowed_senders.is_empty() {
            v.push("email.allowed_senders is empty; the daemon refuses to start".into());
        }
        if !e.require_auth {
            v.push("email.require_auth = false: anyone who forges an allowed From address can drive pagurus".into());
        }
    }
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn installer_downloads_only_over_https() {
        let s = include_str!("../install.sh");
        assert!(s.contains("--proto '=https' --proto-redir '=https'") && s.contains("--https-only"));
        assert!(s.contains("checksum mismatch"));
        #[cfg(unix)]
        assert!(std::process::Command::new("sh").args(["-n", concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")]).status().unwrap().success());
    }

    #[test]
    fn doctor_flags_risky_setups() {
        let home = std::env::temp_dir().join(format!("pagurus-doc-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut cfg = crate::config::Config::default();
        std::fs::write(home.join("config.toml"), "[provider]\nmodel = \"m\"\n").unwrap();
        crate::util::set_env_value(&home, "PAGURUS_API_KEY", "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(home.join("config.toml"), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(super::risks(&home, &cfg), Vec::<String>::new(), "a fresh setup is clean");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.join("env"), std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(home.join("config.toml"), concat!("[provider]\n# key sk", "-proj-abcdefghij0123456789\n")).unwrap();
        cfg.tools.shell_allow.extend(["bash".into(), "rm".into(), "systemctl".into(), "docker ps".into()]);
        cfg.tools.write_roots = vec!["/".into()];
        std::env::set_var("PAGURUS_TEST_DOC_KEY", "k");
        cfg.provider.base_url = "http://api.example.com/v1".into();
        cfg.provider.api_key_env = "PAGURUS_TEST_DOC_KEY".into();
        cfg.telegram = Some(crate::config::TelegramCfg { token_env: "T".into(), allowed_chat_ids: vec![-100123] });
        let r = super::risks(&home, &cfg).join("\n");
        let mut want = vec!["looks like a key", "bash, rm, systemctl", "write_roots", "plain http", "only groups"];
        if cfg!(unix) {
            want.extend(["chmod 600", "chmod 700"]);
        }
        for w in want {
            assert!(r.contains(w), "missing {w}:\n{r}");
        }
        assert!(!r.contains("docker ps"));
        cfg.provider.base_url = "http://192.168.1.20:11434/v1".into();
        assert!(!super::risks(&home, &cfg).join("\n").contains("plain http"), "a LAN Ollama is fine");
    }
}

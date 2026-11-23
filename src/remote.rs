//! Execution backends for the `shell` tool: other machines over the system `ssh`
//! (`[[host]]`) and a throwaway Docker container (`[sandbox]`). Same safety model
//! as local commands: the read-only gate decides what runs unasked, the approval
//! names where it runs, pagurus's environment (keys) never reaches ssh or docker,
//! and the model can only pick targets the user configured.

use crate::config::Config;
use crate::tools::{self, Ctx};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `[[host]]`: a machine the agent may run commands on.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Host {
    pub name: String,
    /// `user@host`, or an alias from ~/.ssh/config.
    pub target: String,
    pub port: Option<u16>,
    /// Path of the private key file (never the key itself).
    pub identity_file: Option<String>,
    /// Read-only commands for this host; unset = `tools.shell_allow`.
    pub shell_allow: Option<Vec<String>>,
    /// `writes` (default): read-only commands run unasked, others need approval. `always`: every command is asked.
    pub approve: String,
    /// StrictHostKeyChecking: `accept-new` (default) or `yes`.
    pub host_key_check: String,
    pub connect_timeout_secs: u64,
    /// Overall time limit; unset = `tools.shell_timeout_secs`.
    pub timeout_secs: Option<u64>,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            name: String::new(),
            target: String::new(),
            port: None,
            identity_file: None,
            shell_allow: None,
            approve: "writes".into(),
            host_key_check: "accept-new".into(),
            connect_timeout_secs: 10,
            timeout_secs: None,
        }
    }
}

/// `[sandbox]`: run commands in a fresh `docker run --rm` container.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Sandbox {
    /// Empty = no sandbox.
    pub docker_image: String,
    pub network: bool,
    pub memory: String,
    pub cpus: String,
    /// Extra bind mounts `host_path:container_path`, always read-only.
    pub mounts: Vec<String>,
    /// `auto`: sandboxed commands run without asking (the default when network is off); `ask`: like local commands.
    pub approve: Option<String>,
    /// Commands that would need approval on this machine go to the sandbox unless the model says `sandbox: false`.
    pub route_writes: bool,
    /// Run as root inside the container (e.g. to apt-get install); default: the workspace owner.
    pub as_root: bool,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            docker_image: String::new(),
            network: false,
            memory: "512m".into(),
            cpus: "1".into(),
            mounts: vec![],
            approve: None,
            route_writes: false,
            as_root: false,
        }
    }
}

impl Sandbox {
    pub fn on(&self) -> bool {
        !self.docker_image.trim().is_empty()
    }
    fn auto(&self) -> bool {
        self.approve.as_deref().map_or(!self.network, |a| a == "auto")
    }
}

fn word_ok(s: &str, extra: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Reject config that could smuggle options into ssh/docker or mount secrets.
pub fn check(home: &Path, c: &Config) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for h in &c.hosts {
        let bad = |why: &str| Err(format!("[[host]] `{}`: {why}", h.name));
        if !word_ok(&h.name, "_-.") || h.name.len() > 32 || !seen.insert(h.name.as_str()) {
            return bad("name must be unique, 1-32 letters, digits, - _ .");
        }
        if !word_ok(&h.target, "@._-") {
            return bad("target must be user@host or an ssh alias (letters, digits, @ . _ -)");
        }
        if !matches!(h.approve.as_str(), "writes" | "always") {
            return bad("approve must be writes or always");
        }
        if !matches!(h.host_key_check.as_str(), "accept-new" | "yes") {
            return bad("host_key_check must be accept-new or yes");
        }
        if h.identity_file
            .as_deref()
            .is_some_and(|p| p.trim().is_empty() || p.chars().any(char::is_control) || p.contains("PRIVATE KEY"))
        {
            return bad("identity_file must be the path of a key file");
        }
    }
    let s = &c.sandbox;
    if !s.on() {
        return Ok(());
    }
    let bad = |why: String| Err(format!("[sandbox]: {why}"));
    if !word_ok(&s.docker_image, "._/:@-") {
        return bad(format!("docker_image `{}` is not an image name", s.docker_image));
    }
    if !word_ok(&s.memory, "") || !s.memory.trim_end_matches(['b', 'k', 'm', 'g']).chars().all(|c| c.is_ascii_digit()) {
        return bad(format!("memory `{}` should look like 512m", s.memory));
    }
    if s.cpus.parse::<f32>().map_or(true, |n| n <= 0.0) {
        return bad(format!("cpus `{}` should be a number like 1 or 0.5", s.cpus));
    }
    if s.approve.as_deref().is_some_and(|a| !matches!(a, "auto" | "ask")) {
        return bad("approve must be auto or ask".into());
    }
    for m in &s.mounts {
        let parts: Vec<&str> = m.split(':').collect();
        let ok = parts.len() == 2
            && parts.iter().all(|p| p.starts_with('/') && !p.contains(',') && !p.chars().any(char::is_control));
        if !ok || parts[1] == "/workspace" {
            return bad(format!("mount `{m}` must be /host/path:/container/path (no , and not /workspace)"));
        }
        let src = Path::new(parts[0]);
        let user_home = std::env::var_os("HOME").map(PathBuf::from);
        let covers = |x: &Path| x.starts_with(src);
        if tools::looks_secret(home, parts[0])
            || covers(home)
            || src == Path::new("/")
            || user_home.as_deref().is_some_and(covers)
        {
            return bad(format!("mount `{m}` would expose pagurus's keys, ~/.ssh or the whole system"));
        }
    }
    Ok(())
}

/// The `shell` tool's extra parameters, only when there are hosts or a sandbox.
pub fn shell_params(cfg: &Config, params: &mut Value) {
    if !cfg.hosts.is_empty() {
        let names: Vec<&str> = cfg.hosts.iter().map(|h| h.name.as_str()).collect();
        params["properties"]["host"] =
            json!({"type":"string","enum":names,"description":"run over ssh on this machine instead"});
    }
    if cfg.sandbox.on() {
        params["properties"]["sandbox"] =
            json!({"type":"boolean","description":"run in a throwaway docker container (workspace at /workspace)"});
    }
}

/// System prompt lines, only when something is configured.
pub fn about(cfg: &Config) -> String {
    let mut s = String::new();
    if !cfg.hosts.is_empty() {
        let list: Vec<String> = cfg.hosts.iter().map(|h| format!("{} ({})", h.name, h.target)).collect();
        s += &format!(
            "- Other machines: shell with `host` runs the command over ssh there: {}. Same rules as here: read-only \
             commands run directly, the rest need the user's approval. It starts in the remote user's home.\n",
            list.join(", ")
        );
    }
    if cfg.sandbox.on() {
        let sb = &cfg.sandbox;
        s += &format!(
            "- Sandbox: shell with `sandbox: true` runs in a fresh {} container (workspace at /workspace, network {}, \
             nothing else of this machine; installs don't persist){}. Use it to build, test or try risky things.{}\n",
            sb.docker_image,
            if sb.network { "on" } else { "off" },
            if sb.auto() { " without asking the user" } else { "" },
            if sb.route_writes {
                " Commands that would need approval here go there unless you pass `sandbox: false`."
            } else {
                ""
            },
        );
    }
    s
}

/// Route a `shell` call: Some(result) when it runs on a host or in the sandbox, None for this machine.
pub fn shell(ctx: &Ctx, a: &Value, cmd: &str) -> Option<Result<String, String>> {
    let cfg = ctx.cfg;
    if let Some(name) = a["host"].as_str().filter(|n| !n.is_empty()) {
        let r = match cfg.hosts.iter().find(|h| h.name == name) {
            Some(h) => on_host(ctx, h, cmd),
            None => Err(format!(
                "no host `{name}`; configured: {}",
                cfg.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        };
        return Some(r);
    }
    let sandbox = match a["sandbox"].as_bool() {
        Some(b) => b,
        None => {
            cfg.sandbox.route_writes
                && cfg.sandbox.on()
                && tools::gate(ctx.home, &cfg.tools.shell_allow, cmd, false).is_some()
        }
    };
    if !sandbox {
        return None;
    }
    if !cfg.sandbox.on() {
        return Some(Err("no sandbox is configured ([sandbox] docker_image in config.toml)".into()));
    }
    Some(in_sandbox(ctx, cmd))
}

fn on_host(ctx: &Ctx, h: &Host, cmd: &str) -> Result<String, String> {
    tools::plain_command(ctx, cmd)?;
    tools::above_floor(ctx, cmd)?;
    let allow = h.shell_allow.as_ref().unwrap_or(&ctx.cfg.tools.shell_allow);
    let why = match tools::gate(ctx.home, allow, cmd, true) {
        None if h.approve == "always" => Some("this host asks for every command".to_string()),
        w => w,
    };
    if let Some(why) = why {
        tools::approve(ctx, &why, &format!("{cmd}\n(over ssh on host `{}` = {})", h.name, h.target))?;
    }
    let timeout = h.timeout_secs.unwrap_or(ctx.cfg.tools.shell_timeout_secs);
    let out =
        tools::run_command(argv_command(&ssh_argv(ctx.home, h, cmd)), &ctx.home.join("workspace"), timeout, &|| {})?;
    Ok(format!("[{}] {out}", h.name))
}

fn in_sandbox(ctx: &Ctx, cmd: &str) -> Result<String, String> {
    tools::plain_command(ctx, cmd)?;
    let sb = &ctx.cfg.sandbox;
    if !sb.auto() {
        if let Some(why) = tools::gate(ctx.home, &ctx.cfg.tools.shell_allow, cmd, false) {
            tools::approve(ctx, &why, &format!("{cmd}\n(in the docker sandbox {})", sb.docker_image))?;
        }
    }
    let ws = ctx.home.join("workspace");
    let _ = std::fs::create_dir_all(&ws);
    let name = format!("pagurus-sandbox-{}", crate::util::new_id());
    let argv = docker_argv(sb, &ws, &name, owner(&ws), cmd)?;
    let kill = || {
        let mut c = argv_command(&["docker".into(), "rm".into(), "-f".into(), name.clone()]);
        let _ = c.stdout(std::process::Stdio::null()).status();
    };
    let out = tools::run_command(argv_command(&argv), &ws, ctx.cfg.tools.shell_timeout_secs, &kill)?;
    Ok(format!("[sandbox] {out}"))
}

/// Quote for a POSIX shell: one single-quoted word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Where ssh keeps its shared connections; None when the path would be too long for a socket.
fn control_dir(home: &Path) -> Option<PathBuf> {
    let d = home.join("ssh");
    // sun_path is 104 bytes on macOS: dir + "/" + %C (40) + ssh's temporary suffix (17)
    (d.as_os_str().len() <= 45).then_some(d)
}

/// The full ssh command line. Only config values and the quoted command reach it:
/// the target comes after `--`, the command is a single argument for the remote `sh -c`.
pub fn ssh_argv(home: &Path, h: &Host, cmd: &str) -> Vec<String> {
    let mut v: Vec<String> = vec!["ssh".into(), "-T".into()];
    let mut o = |x: String| v.extend(["-o".to_string(), x]);
    o("BatchMode=yes".into());
    o(format!("ConnectTimeout={}", h.connect_timeout_secs.max(1)));
    o(format!("StrictHostKeyChecking={}", h.host_key_check));
    for x in ["LogLevel=ERROR", "ForwardAgent=no", "ForwardX11=no", "ClearAllForwardings=yes", "PermitLocalCommand=no"]
    {
        o(x.into());
    }
    if let Some(d) = control_dir(home) {
        if std::fs::create_dir_all(&d).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
            }
            o("ControlMaster=auto".into());
            o(format!("ControlPath={}/%C", d.display()));
            o("ControlPersist=120".into());
        }
    }
    if let Some(p) = h.port {
        v.extend(["-p".into(), p.to_string()]);
    }
    if let Some(id) = &h.identity_file {
        v.extend([
            "-i".into(),
            crate::util::expand(id).to_string_lossy().into_owned(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
        ]);
    }
    v.extend(["--".into(), h.target.clone(), format!("sh -c {}", sh_quote(cmd))]);
    v
}

/// The user and group that own the workspace, so files the sandbox writes stay theirs.
fn owner(ws: &Path) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(ws).ok().map(|m| (m.uid(), m.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = ws;
        None
    }
}

/// `docker run --rm …`: no capabilities beyond file ownership, no privilege gain,
/// limited memory/CPU/processes, only the workspace writable, the command after the image.
pub fn docker_argv(
    sb: &Sandbox,
    ws: &Path,
    name: &str,
    user: Option<(u32, u32)>,
    cmd: &str,
) -> Result<Vec<String>, String> {
    let ws = ws.to_string_lossy();
    if ws.contains([',', ':']) {
        return Err(format!("the workspace path {ws} can't be mounted (contains , or :)"));
    }
    let mut v: Vec<String> =
        ["docker", "run", "--rm", "--name", name, "--network", if sb.network { "bridge" } else { "none" }]
            .iter()
            .map(|s| s.to_string())
            .collect();
    for x in ["--memory", &sb.memory, "--cpus", &sb.cpus, "--pids-limit", "256", "--cap-drop", "ALL"] {
        v.push(x.into());
    }
    for cap in ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETUID", "SETGID"] {
        v.extend(["--cap-add".into(), cap.into()]);
    }
    v.extend(["--security-opt".into(), "no-new-privileges".into()]);
    match user {
        Some((u, g)) if !sb.as_root => v.extend(["--user".into(), format!("{u}:{g}"), "-e".into(), "HOME=/tmp".into()]),
        _ => {}
    }
    v.extend(["--mount".into(), format!("type=bind,source={ws},target=/workspace")]);
    for m in &sb.mounts {
        if let Some((src, dst)) = m.split_once(':') {
            v.extend(["--mount".into(), format!("type=bind,source={src},target={dst},readonly")]);
        }
    }
    v.extend([
        "-w".into(),
        "/workspace".into(),
        "--".into(),
        sb.docker_image.clone(),
        "sh".into(),
        "-c".into(),
        cmd.into(),
    ]);
    Ok(v)
}

/// Run argv directly (no shell parsing of its words) in its own process group, stderr
/// merged into stdout, with a clean environment plus what ssh/docker need to find agents and daemons.
fn argv_command(argv: &[String]) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg("exec 2>&1; exec \"$@\"").arg("pagurus").args(argv);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    tools::clean_env(&mut c);
    for k in ["SSH_AUTH_SOCK", "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    c
}

/// The remote side of the read-only gate: this machine's files say nothing about the
/// host's, so wildcards that could reach hidden files and wide recursive searches need approval.
pub(crate) fn touches_secret(home: &Path, words: &[String]) -> Option<&'static str> {
    let recursive = tools::recursive_search(words);
    let mut narrow_paths = 0;
    for w in &words[1..] {
        if w.starts_with('~') && !(w == "~" || w.starts_with("~/")) {
            return Some("it uses another user's ~ path");
        }
        if tools::looks_secret(home, w) {
            return Some("it mentions what looks like a secret");
        }
        let p = match w.split_once('=') {
            Some((o, v)) if o.starts_with('-') => v,
            _ => w,
        };
        if tools::has_glob(p) && p.split('/').any(|c| c.starts_with(['.', '[']) || c.contains("[.")) {
            return Some("its wildcard could match hidden files (keys) on that host");
        }
        if recursive && (p.starts_with(['~', '.']) || p.starts_with('/')) {
            let first = p.trim_start_matches('/').split('/').next().unwrap_or("");
            if !p.starts_with('/') || matches!(first, "" | "home" | "root" | "Users" | "etc") {
                return Some("a recursive search there could read keys on that host");
            }
            narrow_paths += 1;
        }
    }
    if recursive && narrow_paths == 0 {
        return Some("a recursive search without a path covers the remote home (keys)");
    }
    None
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// `pagurus doctor` lines.
pub fn status(cfg: &Config) -> Vec<String> {
    let (ssh, docker) = (on_path("ssh"), on_path("docker"));
    let mut v = vec![format!(
        "{}remote     ssh {} · docker {} · {} host(s)",
        if !cfg.hosts.is_empty() && !ssh { "!! " } else { "   " },
        if ssh { "found" } else { "not found" },
        if docker { "found" } else { "not found" },
        cfg.hosts.len()
    )];
    if cfg.sandbox.on() {
        let tmp = std::env::temp_dir();
        let argv: Vec<String> =
            ["docker", "info", "--format", "{{.ServerVersion}}"].iter().map(|s| s.to_string()).collect();
        let out = if docker {
            tools::run_command(argv_command(&argv), &tmp, 8, &|| {}).unwrap_or_else(|e| e)
        } else {
            "no docker".into()
        };
        let up = docker && !out.contains('[') && !out.to_lowercase().contains("error");
        v.push(format!(
            "{}sandbox    {} · network {} · {}",
            if up { "ok " } else { "!! " },
            cfg.sandbox.docker_image,
            if cfg.sandbox.network { "on" } else { "off" },
            if up {
                format!("docker {}", out.trim())
            } else {
                format!("docker not usable: {}", crate::util::one_line(&out, 100))
            }
        ));
    }
    v
}

/// `pagurus host list | test <name>`.
pub fn cli(home: &Path, cfg: &Config, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if cfg.hosts.is_empty() {
                println!("no hosts; add [[host]] entries to config.toml (see README: Other machines)");
            }
            for h in &cfg.hosts {
                let allow =
                    h.shell_allow.as_ref().map_or("tools.shell_allow".to_string(), |a| format!("{} commands", a.len()));
                let port = h.port.map(|p| format!(":{p}")).unwrap_or_default();
                println!("{:<12} {}{port}  approve {} · read-only: {allow}", h.name, h.target, h.approve);
            }
            if cfg.sandbox.on() {
                println!(
                    "{:<12} docker {} (network {})",
                    "(sandbox)",
                    cfg.sandbox.docker_image,
                    if cfg.sandbox.network { "on" } else { "off" }
                );
            }
            Ok(())
        }
        Some("test") => {
            let name = args.get(1).ok_or("usage: pagurus host test <name>")?;
            let h = cfg.hosts.iter().find(|h| &h.name == name).ok_or_else(|| format!("no host `{name}`"))?;
            let t = std::time::Instant::now();
            let out = tools::run_command(
                argv_command(&ssh_argv(home, h, "uname -srm")),
                &home.join("workspace"),
                30,
                &|| {},
            )?;
            let out = crate::util::redact(out.trim());
            if out.contains("[exit") || out.contains("[killed") {
                return Err(format!("{name}: {out}"));
            }
            println!("{name}: ok in {} ms · {out}", t.elapsed().as_millis());
            Ok(())
        }
        _ => Err("usage: pagurus host [list | test <name>]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host { name: "pi".into(), target: "pi@pi.local".into(), port: Some(2222), ..Default::default() }
    }

    fn gate(cmd: &str) -> Option<String> {
        let allow: Vec<String> =
            ["ls", "cat", "grep", "rg", "uptime", "df", "head"].iter().map(|s| s.to_string()).collect();
        tools::gate(Path::new("/h"), &allow, cmd, true)
    }

    #[test]
    fn remote_commands_pass_the_same_gate() {
        for ok in ["uptime", "df -h | head -3", "ls /var/log/*.log", "grep -r error /var/log", "cat /etc/hostname"] {
            assert_eq!(gate(ok), None, "{ok}");
        }
        for bad in [
            "rm -rf /",
            "uptime; reboot",
            "ls $(id)",
            "cat ~/.ssh/id_ed25519",
            "cat ~/.s*/id*",
            "cat /home/*/.[s]sh/*",
            "ls ~/.*",
            "grep -r KEY ~",
            "grep -r KEY .",
            "rg KEY",
            "grep -r x /home",
            "cat ~root/x",
            "cat /proc/1/environ",
            "ls > /tmp/x",
            "uptime & reboot",
            "cat ../x",
            "printenv",
        ] {
            assert!(gate(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn ssh_arguments_cannot_be_injected() {
        let home = Path::new("/h");
        let cmd = "ls '; rm -rf ~' \"$(x)\" -oProxyCommand=evil";
        let v = ssh_argv(home, &host(), cmd);
        let i = v.iter().position(|a| a == "--").unwrap();
        assert_eq!(&v[i + 1..], ["pi@pi.local".to_string(), format!("sh -c {}", sh_quote(cmd))]);
        assert!(!v[..i].iter().any(|a| a.contains("ProxyCommand") || a.contains("rm -rf")));
        assert!(v.contains(&"BatchMode=yes".into()) && v.contains(&"StrictHostKeyChecking=accept-new".into()));
        assert!(v.windows(2).any(|w| w == ["-p", "2222"]));
        // the remote `sh -c` gets exactly the original command back
        let out = Command::new("sh").arg("-c").arg(format!("printf %s {}", sh_quote(cmd))).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), cmd);
        let id = Host { identity_file: Some("~/k/id".into()), ..host() };
        assert!(ssh_argv(home, &id, "x").windows(2).any(|w| w[0] == "-i" && w[1].ends_with("/k/id")));
        // a long home skips connection sharing rather than overflowing the socket path
        let long = PathBuf::from(format!("/{}", "x".repeat(60)));
        assert!(!ssh_argv(&long, &host(), "x").iter().any(|a| a.starts_with("ControlPath")));
    }

    #[test]
    fn config_is_checked() {
        let home = Path::new("/h");
        let mut c = Config { hosts: vec![host()], ..Default::default() };
        assert!(check(home, &c).is_ok());
        for bad in [
            Host { target: "-oProxyCommand=x".into(), ..host() },
            Host { target: "pi@pi local".into(), ..host() },
            Host { name: "".into(), ..host() },
            Host { approve: "never".into(), ..host() },
            Host { host_key_check: "no".into(), ..host() },
            Host { identity_file: Some("-----BEGIN OPENSSH PRIVATE KEY-----".into()), ..host() },
        ] {
            c.hosts = vec![bad.clone()];
            assert!(check(home, &c).is_err(), "{bad:?}");
        }
        c.hosts = vec![host(), host()];
        assert!(check(home, &c).unwrap_err().contains("unique"));
        c.hosts = vec![];
        c.sandbox.docker_image = "debian:bookworm-slim".into();
        assert!(check(home, &c).is_ok());
        for (img, mount) in [
            ("--privileged", ""),
            ("x", "/:/host"),
            ("x", "/h:/h"),
            ("x", "/srv,x:/y"),
            ("x", "/srv:/workspace"),
            ("x", "/home/me/.ssh:/k"),
        ] {
            c.sandbox.docker_image = img.into();
            c.sandbox.mounts = if mount.is_empty() { vec![] } else { vec![mount.into()] };
            assert!(check(home, &c).is_err(), "{img} {mount}");
        }
    }

    #[test]
    fn sandbox_command_line() {
        let sb =
            Sandbox { docker_image: "alpine:3".into(), mounts: vec!["/srv/data:/data".into()], ..Default::default() };
        let cmd = "echo hi; rm -rf / --privileged";
        let v = docker_argv(&sb, Path::new("/w/ws"), "pagurus-sandbox-1", Some((1000, 1000)), cmd).unwrap();
        let i = v.iter().position(|a| a == "--").unwrap();
        assert_eq!(&v[i + 1..], ["alpine:3", "sh", "-c", cmd]);
        let opts = v[..i].join(" ");
        for want in [
            "--rm",
            "--network none",
            "--memory 512m",
            "--cpus 1",
            "--pids-limit 256",
            "--cap-drop ALL",
            "no-new-privileges",
            "--user 1000:1000",
            "type=bind,source=/w/ws,target=/workspace",
            "source=/srv/data,target=/data,readonly",
        ] {
            assert!(opts.contains(want), "{want} missing in {opts}");
        }
        assert!(!opts.contains("privileged") && !opts.contains("rm -rf"));
        let net = Sandbox { network: true, as_root: true, ..sb.clone() };
        let v = docker_argv(&net, Path::new("/w"), "n", Some((1, 1)), "x").unwrap().join(" ");
        assert!(v.contains("--network bridge") && !v.contains("--user"));
        assert!(docker_argv(&sb, Path::new("/a:b"), "n", None, "x").is_err());
        assert!(sb.auto() && !net.auto());
    }

    #[test]
    fn hosts_come_only_from_config_and_approval_names_them() {
        let home = std::env::temp_dir().join(format!("pagurus-remote-{}", crate::util::new_id()));
        std::fs::create_dir_all(home.join("workspace")).unwrap();
        // no ssh server: the command fails fast, which is all this test needs
        let cfg = Config {
            hosts: vec![Host { target: "nobody@127.0.0.1".into(), port: Some(1), connect_timeout_secs: 2, ..host() }],
            ..Default::default()
        };
        let asked = std::cell::RefCell::new(Vec::new());
        let no = |c: &str| {
            asked.borrow_mut().push(c.to_string());
            false
        };
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: Some(&no) };
        let err = shell(&ctx, &json!({"host":"evil.example"}), "uptime").unwrap().unwrap_err();
        assert!(err.contains("no host `evil.example`"));
        assert!(shell(&ctx, &json!({"host":"pi"}), "touch x").unwrap().unwrap_err().contains("did not approve"));
        assert!(asked.borrow()[0].contains("touch x") && asked.borrow()[0].contains("host `pi`"));
        // the safety floor holds on other machines too, and nobody is even asked
        assert!(shell(&ctx, &json!({"host":"pi"}), "sudo reboot").unwrap().unwrap_err().contains("safety floor"));
        assert_eq!(asked.borrow().len(), 1);
        assert!(shell(&ctx, &json!({"host":"pi"}), "ls\u{1b}[2K").unwrap().unwrap_err().contains("control"));
        // read-only: runs unasked (and fails to connect)
        let t = std::time::Instant::now();
        let out = shell(&ctx, &json!({"host":"pi"}), "uptime").unwrap().unwrap();
        assert!(out.starts_with("[pi]") && out.contains("[exit "), "{out}"); // 255 from ssh (127 without one)
        assert!(t.elapsed().as_secs() < 10);
        assert_eq!(asked.borrow().len(), 1);
        assert!(about(&Config::default()).is_empty() && about(&cfg).contains("pi (nobody@127.0.0.1)"));
        let mut p = json!({"properties":{}});
        shell_params(&cfg, &mut p);
        assert_eq!(p["properties"]["host"]["enum"], json!(["pi"]));
        assert!(p["properties"].get("sandbox").is_none());
        // no host, no sandbox: this machine
        assert!(shell(&ctx, &json!({}), "uptime").is_none());
        assert!(shell(&ctx, &json!({"sandbox":true}), "uptime").unwrap().is_err());
        let always = Config { hosts: vec![Host { approve: "always".into(), ..cfg.hosts[0].clone() }], ..cfg.clone() };
        let ctx = Ctx { cfg: &always, ..ctx };
        assert!(shell(&ctx, &json!({"host":"pi"}), "uptime").unwrap().is_err());
        assert_eq!(asked.borrow().len(), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn timeouts_kill_the_process_and_run_cleanup() {
        let tmp = std::env::temp_dir();
        let cleaned = std::cell::Cell::new(false);
        let argv: Vec<String> = ["sh", "-c", "echo started; sleep 30"].iter().map(|s| s.to_string()).collect();
        let t = std::time::Instant::now();
        let out = tools::run_command(argv_command(&argv), &tmp, 1, &|| cleaned.set(true)).unwrap();
        assert!(out.contains("started") && out.contains("[killed after 1s]"), "{out}");
        assert!(cleaned.get() && t.elapsed().as_secs() < 5);
    }

    #[test]
    fn backends_never_see_pagurus_environment() {
        std::env::set_var("PAGURUS_TEST_REMOTE_KEY", "sk-remote-secret");
        let argv: Vec<String> = ["sh", "-c", "env"].iter().map(|s| s.to_string()).collect();
        let out = tools::run_command(argv_command(&argv), &std::env::temp_dir(), 5, &|| {}).unwrap();
        assert!(!out.contains("sk-remote-secret") && out.contains("PATH="));
    }
}

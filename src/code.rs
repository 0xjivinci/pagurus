//! Coding (feature `code`, in the full/uge build): project mode and its tools.
//!
//! `/project ~/code/app` (or `pagurus code` inside a repo) points pagurus at one
//! folder: file tools may write there, the shell runs there, and four tools
//! appear: `grep`, `glob`, `check` (the project's tests, build or lint) and
//! `git` (status, diff, log, and commits on its own `agent/…` branch, never a
//! push). The project's AGENTS.md / CLAUDE.md joins the system prompt. Edits
//! must follow a fresh read of the file, every changed file is checkpointed,
//! and `/revert` puts back everything the last turn changed.

use crate::tools::Ctx;
use crate::util::{clip, now, read};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub git: bool,
    /// The project's instructions for agents (AGENTS.md, CLAUDE.md …), capped.
    pub rules: String,
    pub rules_file: String,
}

#[cfg(not(test))]
static PROJECT: Mutex<Option<Project>> = Mutex::new(None);

#[cfg(not(test))]
fn set(p: Option<Project>) -> Option<Project> {
    PROJECT.lock().ok().map(|mut g| std::mem::replace(&mut *g, p)).unwrap_or_default()
}

#[cfg(not(test))]
pub fn current() -> Option<Project> {
    PROJECT.lock().ok().and_then(|p| p.clone())
}

// tests run side by side: one test's project must not leak into another's tools
#[cfg(test)]
thread_local!(static PROJECT: std::cell::RefCell<Option<Project>> = const { std::cell::RefCell::new(None) });

#[cfg(test)]
fn set(p: Option<Project>) -> Option<Project> {
    PROJECT.with(|g| g.replace(p))
}

#[cfg(test)]
pub fn current() -> Option<Project> {
    PROJECT.with(|g| g.borrow().clone())
}

pub fn root() -> Option<PathBuf> {
    current().map(|p| p.root)
}

const RULE_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules", ".github/copilot-instructions.md", "CONTRIBUTING.md"];
const RULES_MAX: usize = 6000;

/// Files that mark a folder as a project (plain `pagurus` started there opens it).
const MARKERS: &[&str] = &[
    ".git", "AGENTS.md", "CLAUDE.md", "Cargo.toml", "package.json", "pyproject.toml", "requirements.txt", "setup.py",
    "go.mod", "Makefile", "CMakeLists.txt", "pom.xml", "build.gradle", "Gemfile", "composer.json", "deno.json",
];

/// `pagurus` started inside a project folder: open it. None anywhere else.
pub fn auto_open(home: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if !MARKERS.iter().any(|m| cwd.join(m).exists()) {
        return None;
    }
    open(home, &cwd.to_string_lossy()).ok().map(|m| format!("{m}\n(opened because this folder is a project; /project off leaves it)"))
}

/// Open a project folder. Refuses `/`, your home folder itself and pagurus's home.
pub fn open(home: &Path, arg: &str) -> Result<String, String> {
    let p = crate::util::expand(arg.trim());
    let root = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let user_home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok());
    if root.parent().is_none() || user_home.as_deref() == Some(root.as_path()) {
        return Err("pick a project folder, not / or your whole home folder: `cd` into a project first, or \
                    `pagurus code ~/path/to/project`; for a new one, `mkdir ~/my-tool && pagurus code ~/my-tool`"
            .into());
    }
    if home.canonicalize().is_ok_and(|h| h.starts_with(&root) || root.starts_with(&h)) {
        return Err("that's pagurus's own home; pick a project folder".into());
    }
    if crate::tools::looks_secret(home, &root.to_string_lossy()) {
        return Err("that folder looks like it holds secrets".into());
    }
    let git = root.join(".git").exists();
    let (rules_file, rules) = RULE_FILES
        .iter()
        .find_map(|f| {
            let t = read(&root.join(f));
            (!t.trim().is_empty()).then(|| (f.to_string(), clip(t.trim(), RULES_MAX)))
        })
        .unwrap_or_default();
    let pr = Project { root: root.clone(), git, rules, rules_file: rules_file.clone() };
    set(Some(pr));
    if let Ok(mut r) = READS.lock() {
        *r = None;
    }
    let checks = detect(&root);
    Ok(format!(
        "Project: {}{}{}\nchecks: {}",
        root.display(),
        if git { format!(" · git branch {}", git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default().trim()) } else { " · not a git repo".into() },
        if rules_file.is_empty() { String::new() } else { format!(" · following {rules_file}") },
        if checks.is_empty() { "none found (pass check a command)".to_string() } else { checks.iter().map(|(k, c)| format!("{k}: {c}")).collect::<Vec<_>>().join(" · ") }
    ))
}

pub fn close() -> String {
    let was = set(None);
    match was {
        Some(p) => format!("Closed {}. Back to the workspace.", p.root.display()),
        None => "No project is open.".into(),
    }
}

/// The system prompt part for an open project.
pub fn prompt_section() -> String {
    let Some(p) = current() else { return String::new() };
    let mut s = format!(
        "\n# Project: {}\n\
         You are working on this code base. Relative paths and the shell start here; you may edit files here.\n\
         - Understand first: glob and grep to find code, read_file (line-numbered) before editing.\n\
         - Make focused changes with edit_file (several edits at once via `edits`); keep the existing style.\n\
         - After changing code, run `check` (tests, or build/lint) and fix what fails before saying you're done. \
           Never claim something works without having checked it.\n\
         - `git` shows status and diffs; commit only when the user asks (it commits on an agent/ branch, never pushes).\n\
         - Say what you changed and why, briefly. The user can undo the last turn's file changes with /revert.\n",
        p.root.display()
    );
    if !p.rules.is_empty() {
        s += &format!("\n## The project's instructions ({})\n{}\n", p.rules_file, p.rules);
    }
    s
}

// ---------- fresh reads and per-turn tracking ----------

static READS: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, x| (h ^ *x as u64).wrapping_mul(0x100000001b3))
}

/// read_file saw this file as it is now.
pub fn note_read(p: &Path) {
    if let (Ok(b), Ok(mut r)) = (std::fs::read(p), READS.lock()) {
        r.get_or_insert_with(HashMap::new).insert(p.to_path_buf(), fnv(&b));
    }
}

/// Editing a project file needs a read of it as it is now (not a stale memory of it).
pub fn check_fresh(p: &Path) -> Result<(), String> {
    let Some(root) = root() else { return Ok(()) };
    if !p.starts_with(&root) || !p.exists() {
        return Ok(());
    }
    let now = std::fs::read(p).map(|b| fnv(&b)).unwrap_or(0);
    match READS.lock().ok().and_then(|r| r.as_ref().and_then(|m| m.get(p).copied())) {
        Some(h) if h == now => Ok(()),
        Some(_) => Err(format!("{} changed since you read it; read_file it again before editing", p.display())),
        None => Err(format!("read {} with read_file before editing it", p.display())),
    }
}

#[derive(Default)]
struct Turn {
    /// The newest checkpoint before this turn: later ones are this turn's changes.
    since: u64,
    edited: Vec<PathBuf>,
    checked: bool,
    nudged: bool,
}

static TURN: Mutex<Option<Turn>> = Mutex::new(None);

pub fn turn_begin(home: &Path) {
    if let Ok(mut t) = TURN.lock() {
        *t = Some(Turn { since: crate::files::last_checkpoint(home), ..Default::default() });
    }
}

/// A file tool changed `p`; re-reading it is no longer needed to edit it again.
pub fn edited(p: &Path) {
    note_read(p);
    if let Ok(mut t) = TURN.lock() {
        let t = t.get_or_insert_with(Default::default);
        if !t.edited.iter().any(|x| x == p) {
            t.edited.push(p.to_path_buf());
        }
        t.checked = false;
    }
}

/// The nudge for an answer that changed code without running `check` (once per turn).
pub fn unchecked_note() -> Option<String> {
    root()?;
    let mut g = TURN.lock().ok()?;
    let t = g.as_mut()?;
    if t.edited.is_empty() || t.checked || t.nudged || detect(&root()?).is_empty() {
        return None;
    }
    t.nudged = true;
    let names: Vec<String> = t.edited.iter().take(5).map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).collect();
    Some(format!(
        "[pagurus: you changed {} but haven't run `check`. Run it now; if it fails, fix it. Then give your answer.]",
        names.join(", ")
    ))
}

/// `/revert`: put back every file the last turn changed.
pub fn revert(home: &Path) -> Result<String, String> {
    let since = TURN.lock().ok().and_then(|t| t.as_ref().map(|t| t.since)).ok_or("nothing to revert yet")?;
    let done = crate::files::restore_since(home, since)?;
    if done.is_empty() {
        return Ok("The last turn didn't change any files.".into());
    }
    Ok(format!("↩️ put back {} file(s) as they were before the last turn:\n{}", done.len(), done.join("\n")))
}

// ---------- tools ----------

/// The `project` tool: open or close a project folder from a chat (opening asks the user).
pub fn project_definition() -> Value {
    json!({ "type": "function", "function": { "name": "project",
        "description": "Work inside a project folder: open gives you its rules and grep/glob/check/git and lets you edit files there (asks the user); close ends it.",
        "parameters": { "type": "object", "properties": {
            "action": { "type": "string", "enum": ["open", "close"] }, "path": { "type": "string" } }, "required": ["action"] } } })
}

pub fn definitions() -> Vec<Value> {
    if current().is_none() {
        return vec![project_definition()];
    }
    let f = |name: &str, desc: &str, params: Value| json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } });
    vec![
        project_definition(),
        f("grep", "Search the project's files for a regular expression (ripgrep, else grep). Returns path:line:text.",
          json!({ "type": "object", "properties": {
              "pattern": { "type": "string" }, "path": { "type": "string", "description": "file or folder, default the whole project" },
              "glob": { "type": "string", "description": "only files matching, e.g. *.rs" }, "ignore_case": { "type": "boolean" },
              "context": { "type": "integer", "description": "lines around each match" } }, "required": ["pattern"] })),
        f("glob", "List project files matching a pattern like src/**/*.rs or **/test_*.py (skips .git, node_modules, target …).",
          json!({ "type": "object", "properties": { "pattern": { "type": "string" } }, "required": ["pattern"] })),
        f("check", "Run the project's tests (default), build or lint, as found in its files (cargo, npm, pytest, go, make), or `command`. Returns pass/fail and the output's end.",
          json!({ "type": "object", "properties": { "what": { "type": "string", "enum": ["test", "build", "lint"] },
              "command": { "type": "string", "description": "only if the usual one is wrong, e.g. `cargo test parser`" } } })),
        f("git", "The project's git: status, diff (path?, staged?), log (n?), show (rev), commit (message; commits all changes on an agent/ branch, created if needed). Never pushes.",
          json!({ "type": "object", "properties": { "action": { "type": "string", "enum": ["status", "diff", "log", "show", "commit"] },
              "path": { "type": "string" }, "staged": { "type": "boolean" }, "n": { "type": "integer" }, "rev": { "type": "string" },
              "message": { "type": "string" } }, "required": ["action"] })),
    ]
}

/// The coding tools; None for any other tool name.
pub fn run(ctx: &Ctx, name: &str, a: &Value) -> Option<Result<String, String>> {
    if name == "project" {
        return Some(match a["action"].as_str().unwrap_or("") {
            "close" => Ok(close()),
            "open" => {
                let path = a["path"].as_str().unwrap_or("").trim();
                if path.is_empty() {
                    return Some(Err("give the folder: path".into()));
                }
                let full = crate::util::expand(path).canonicalize().map_err(|e| format!("{path}: {e}"));
                match full {
                    Ok(f) if root().as_deref() == Some(f.as_path()) => Ok(format!("{} is already open", f.display())),
                    Ok(f) => crate::tools::approve(ctx, "in a project pagurus edits files and runs its checks", &format!("open project {}", f.display()))
                        .and_then(|_| open(ctx.home, &f.to_string_lossy())),
                    Err(e) => Err(e),
                }
            }
            other => Err(format!("project action `{other}`: use open or close")),
        });
    }
    if !matches!(name, "grep" | "glob" | "check" | "git") {
        return None;
    }
    let Some(p) = current() else { return Some(Err(format!("{name} works in a project: open one with the project tool (the user approves)"))) };
    Some(match name {
        "grep" => grep(ctx, &p.root, a),
        "glob" => glob(&p.root, a["pattern"].as_str().unwrap_or("")),
        "check" => check(ctx, &p.root, a),
        _ => git(ctx, &p, a),
    })
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", "__pycache__", "dist", "build", ".next", ".cache", "vendor", ".idea", ".tox"];

fn under_root(root: &Path, p: &str) -> Result<PathBuf, String> {
    if p.is_empty() || p == "." {
        return Ok(root.to_path_buf());
    }
    let full = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };
    let c = full.canonicalize().map_err(|e| format!("{p}: {e}"))?;
    if c.starts_with(root) { Ok(c) } else { Err(format!("{p} is outside the project")) }
}

fn grep(ctx: &Ctx, root: &Path, a: &Value) -> Result<String, String> {
    let pattern = a["pattern"].as_str().filter(|s| !s.is_empty()).ok_or("missing pattern")?;
    let target = under_root(root, a["path"].as_str().unwrap_or(""))?;
    let ctxl = a["context"].as_u64().unwrap_or(0).min(5).to_string();
    let rg = Command::new("rg").arg("--version").output().is_ok_and(|o| o.status.success());
    let mut c = if rg {
        let mut c = Command::new("rg");
        c.args(["-n", "--no-heading", "--color=never", "--max-columns=300", "--max-count=50", "-C", &ctxl]);
        if a["ignore_case"].as_bool() == Some(true) {
            c.arg("-i");
        }
        if let Some(g) = a["glob"].as_str().filter(|g| !g.is_empty()) {
            c.args(["-g", g]);
        }
        c.arg("-e").arg(pattern).arg("--").arg(&target);
        c
    } else {
        let mut c = Command::new("grep");
        c.args(["-rnIE", "-m", "50", "-C", &ctxl]);
        for d in SKIP_DIRS {
            c.arg(format!("--exclude-dir={d}"));
        }
        if a["ignore_case"].as_bool() == Some(true) {
            c.arg("-i");
        }
        if let Some(g) = a["glob"].as_str().filter(|g| !g.is_empty()) {
            c.arg(format!("--include={g}"));
        }
        c.arg("-e").arg(pattern).arg("--").arg(&target);
        c
    };
    crate::tools::clean_env(&mut c);
    let out = c.current_dir(root).output().map_err(|e| format!("search failed: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let rel = format!("{}/", root.display());
    // no secrets: skip lines from files that look like they hold them
    let lines: Vec<String> = text
        .lines()
        .filter(|l| !crate::tools::looks_secret(ctx.home, l.split(':').next().unwrap_or("")))
        .map(|l| l.strip_prefix(&rel).unwrap_or(l).to_string())
        .take(300)
        .collect();
    if lines.is_empty() {
        return Ok(if out.status.code() == Some(1) || out.status.success() { "no matches".into() } else { format!("search error: {}", String::from_utf8_lossy(&out.stderr).trim()) });
    }
    let more = if lines.len() == 300 { "\n[first 300 lines; narrow the pattern, path or glob]" } else { "" };
    Ok(format!("{}{more}", lines.join("\n")))
}

/// `**` matches any number of folders; `*` and `?` stay within one name.
fn glob_match(pat: &[&str], path: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => glob_match(&pat[1..], path) || (!path.is_empty() && glob_match(pat, &path[1..])),
        (Some(p), Some(s)) => crate::tools::wildcard(p, s) && glob_match(&pat[1..], &path[1..]),
        _ => false,
    }
}

fn glob(root: &Path, pattern: &str) -> Result<String, String> {
    let pattern = pattern.trim().trim_start_matches("./");
    if pattern.is_empty() {
        return Err("missing pattern".into());
    }
    // a bare name like `*.rs` searches everywhere
    let full = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
    let pat: Vec<&str> = full.split('/').filter(|s| !s.is_empty()).collect();
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 200_000 {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(e.path());
                }
                continue;
            }
            let rel = e.path().strip_prefix(root).map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
            let parts: Vec<&str> = rel.split('/').collect();
            if glob_match(&pat, &parts) {
                hits.push(rel);
            }
        }
    }
    hits.sort();
    if hits.is_empty() {
        return Ok("no files match".into());
    }
    let n = hits.len();
    hits.truncate(400);
    Ok(format!("{}{}", hits.join("\n"), if n > 400 { format!("\n[{n} files; showing 400]") } else { String::new() }))
}

/// The usual commands for this project: (what, command).
pub fn detect(root: &Path) -> Vec<(&'static str, String)> {
    let has = |f: &str| root.join(f).exists();
    let mut v = Vec::new();
    if has("Cargo.toml") {
        v.push(("test", "cargo test".to_string()));
        v.push(("build", "cargo build".to_string()));
        v.push(("lint", "cargo clippy --all-targets".to_string()));
    } else if has("package.json") {
        let pkg: Value = serde_json::from_str(&read(&root.join("package.json"))).unwrap_or_default();
        let pm = if has("pnpm-lock.yaml") { "pnpm" } else if has("yarn.lock") { "yarn" } else if has("bun.lockb") || has("bun.lock") { "bun" } else { "npm" };
        for (what, script) in [("test", "test"), ("build", "build"), ("lint", "lint")] {
            if pkg["scripts"][script].is_string() {
                v.push((what, if pm == "npm" && script != "test" { format!("npm run {script}") } else { format!("{pm} {}{script}", if pm == "npm" { "" } else { "run " }) }));
            }
        }
    } else if has("go.mod") {
        v.push(("test", "go test ./...".to_string()));
        v.push(("build", "go build ./...".to_string()));
        v.push(("lint", "go vet ./...".to_string()));
    } else if has("pyproject.toml") || has("setup.py") || has("pytest.ini") || has("requirements.txt") {
        v.push(("test", "python3 -m pytest -q".to_string()));
        if has("pyproject.toml") && read(&root.join("pyproject.toml")).contains("ruff") {
            v.push(("lint", "ruff check .".to_string()));
        }
    }
    if v.is_empty() && has("Makefile") {
        let mk = read(&root.join("Makefile"));
        if mk.lines().any(|l| l.starts_with("test:")) {
            v.push(("test", "make test".to_string()));
        }
        v.push(("build", "make".to_string()));
        if mk.lines().any(|l| l.starts_with("lint:")) {
            v.push(("lint", "make lint".to_string()));
        }
    }
    v
}

/// Running a project's tests runs its code: the floor, then auto / yolo / the user decide.
fn allowed(ctx: &Ctx, cmd: &str, usual: bool) -> Result<(), String> {
    crate::tools::plain_command(ctx, cmd)?;
    crate::tools::above_floor(ctx, cmd)?;
    if usual && ctx.cfg.tools.autonomy == "auto" {
        crate::autonomy::audit(ctx.home, "auto", cmd);
        return Ok(());
    }
    crate::tools::approve(ctx, "it runs the project's code", &format!("{cmd}\n(in {})", root().map(|r| r.display().to_string()).unwrap_or_default()))
}

fn check(ctx: &Ctx, root: &Path, a: &Value) -> Result<String, String> {
    let what = a["what"].as_str().unwrap_or("test");
    let found = detect(root);
    let (cmd, usual) = match a["command"].as_str().filter(|c| !c.trim().is_empty()) {
        Some(c) => (c.trim().to_string(), found.iter().any(|(_, f)| c.trim().starts_with(f.as_str()))),
        None => match found.iter().find(|(k, _)| *k == what) {
            Some((_, c)) => (c.clone(), true),
            None => return Err(format!("no {what} command found for this project; pass `command`")),
        },
    };
    allowed(ctx, &cmd, usual)?;
    let t0 = std::time::Instant::now();
    let out = crate::tools::run_shell(&cmd, root, 600)?;
    if let Ok(mut t) = TURN.lock() {
        t.get_or_insert_with(Default::default).checked = true;
    }
    let failed = out.contains("\n[exit ") || out.contains("[killed after") || out.contains("[stopped by the user]");
    Ok(format!(
        "{} `{cmd}` in {:.0}s\n{}",
        if failed { "FAILED" } else { "PASSED" },
        t0.elapsed().as_secs_f64(),
        crate::util::tail(&out, 6000)
    ))
}

/// git with everything that could run code from the repository switched off:
/// no hooks, fsmonitor, pager, external diff, textconv or credential helpers;
/// no system config; no prompts; no network protocols.
fn git_cmd(root: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    crate::tools::clean_env(&mut c);
    c.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").env("GIT_PAGER", "cat").env("GIT_OPTIONAL_LOCKS", "0");
    c.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "core.pager=cat", "-c", "diff.external=",
            "-c", "credential.helper=", "-c", "protocol.allow=never", "-c", "core.sshCommand=false", "-c", "color.ui=false", "-C"]);
    c.arg(root).args(args);
    c
}

fn git_out(root: &Path, args: &[&str]) -> Result<String, String> {
    let o = git_cmd(root, args).output().map_err(|e| format!("git: {e}"))?;
    let mut s = String::from_utf8_lossy(&o.stdout).to_string();
    if !o.status.success() {
        s += &format!("\n{}", String::from_utf8_lossy(&o.stderr).trim());
        return Err(clip(s.trim(), 4000));
    }
    Ok(s)
}

fn git(ctx: &Ctx, p: &Project, a: &Value) -> Result<String, String> {
    if !p.git {
        return Err("this project isn't a git repository".into());
    }
    let root = &p.root;
    let path = a["path"].as_str().filter(|s| !s.is_empty() && !s.starts_with('-'));
    match a["action"].as_str().unwrap_or("status") {
        "status" => git_out(root, &["status", "--short", "--branch"]).map(|s| if s.trim().is_empty() { "clean".into() } else { s }),
        "diff" => {
            let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--stat", "--patch"];
            if a["staged"].as_bool() == Some(true) {
                args.push("--cached");
            }
            if let Some(p) = path {
                args.extend(["--", p]);
            }
            git_out(root, &args).map(|s| if s.trim().is_empty() { "no changes".into() } else { clip(&s, 20_000) })
        }
        "log" => {
            let n = format!("-{}", a["n"].as_u64().unwrap_or(10).clamp(1, 50));
            git_out(root, &["log", &n, "--format=%h %ad %an: %s", "--date=short"])
        }
        "show" => {
            let rev = a["rev"].as_str().unwrap_or("HEAD");
            if rev.starts_with('-') || rev.contains("..") && rev.len() > 80 {
                return Err("give a commit like HEAD~1 or a hash".into());
            }
            git_out(root, &["show", "--no-ext-diff", "--no-textconv", "--stat", "--patch", rev, "--"]).map(|s| clip(&s, 20_000))
        }
        "commit" => {
            let msg = a["message"].as_str().map(str::trim).filter(|m| !m.is_empty()).ok_or("say what the commit does (message)")?;
            if git_out(root, &["status", "--porcelain"])?.trim().is_empty() {
                return Ok("nothing to commit".into());
            }
            let branch = git_out(root, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
            let target = if branch.starts_with("agent/") { branch.clone() } else { format!("agent/{}", crate::util::fmt_local(now()).replace([':', ' '], "-")) };
            let what = format!("git commit on {target}{}: {msg}", if target == branch { String::new() } else { format!(" (new branch from {branch})") });
            crate::tools::approve(ctx, "it changes the project's git history", &what)?;
            crate::autonomy::audit(ctx.home, "git", &what);
            if target != branch {
                git_out(root, &["switch", "-c", &target])?;
            }
            git_out(root, &["add", "-A"])?;
            git_out(root, &["commit", "--no-verify", "-q", "-m", msg])?;
            let head = git_out(root, &["log", "-1", "--format=%h %s"])?;
            Ok(format!("committed on {target}: {} (not pushed)", head.trim()))
        }
        x => Err(format!("unknown git action `{x}`")),
    }
}

/// A short unified-style diff of an edit, for the tool result and the screen.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out = format!("@@ line {} @@\n", pre + 1);
    for l in a[pre.saturating_sub(2)..pre].iter() {
        out += &format!("  {l}\n");
    }
    for l in a_mid.iter().take(40) {
        out += &format!("- {l}\n");
    }
    for l in b_mid.iter().take(40) {
        out += &format!("+ {l}\n");
    }
    if a_mid.len() > 40 || b_mid.len() > 40 {
        out += "  …\n";
    }
    for l in a[a.len() - suf..].iter().take(2) {
        out += &format!("  {l}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_opens_a_project_only_with_the_users_ok() {
        let home = std::env::temp_dir().join(format!("pagurus-projtool-{}", crate::util::new_id()));
        let proj = std::env::temp_dir().join(format!("pagurus-projtool-p-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        let cfg = crate::config::Config::default();
        let (no, yes) = (|_: &str| false, |_: &str| true);
        let open_it = json!({"action":"open","path": proj.display().to_string()});
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: Some(&no) };
        assert!(run(&ctx, "project", &open_it).unwrap().is_err(), "refused");
        assert!(root().is_none());
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: None };
        assert!(run(&ctx, "project", &open_it).unwrap().unwrap_err().contains("unattended"), "nobody to ask");
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: Some(&yes) };
        assert!(run(&ctx, "project", &open_it).unwrap().is_ok());
        assert_eq!(root(), proj.canonicalize().ok());
        assert!(run(&ctx, "project", &open_it).unwrap().unwrap().contains("already open"));
        let home_dir = std::env::var("HOME").unwrap();
        assert!(run(&ctx, "project", &json!({"action":"open","path": home_dir})).unwrap().unwrap_err().contains("not / or your whole home"));
        run(&ctx, "project", &json!({"action":"close"})).unwrap().unwrap();
        assert!(root().is_none());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn globs_match_like_shells_do() {
        let m = |p: &str, s: &str| glob_match(&p.split('/').collect::<Vec<_>>(), &s.split('/').collect::<Vec<_>>());
        assert!(m("src/**/*.rs", "src/a/b/c.rs") && m("src/**/*.rs", "src/c.rs") && m("**/test_*.py", "x/test_a.py"));
        assert!(!m("src/*.rs", "src/a/c.rs") && !m("*.rs", "src/c.rs"));
    }

    #[test]
    fn diffs_show_only_what_changed() {
        let d = diff("a\nb\nc\nd\ne\n", "a\nb\nX\nd\ne\n");
        assert!(d.contains("- c\n+ X\n") && d.contains("  b\n") && d.contains("  d\n") && d.starts_with("@@ line 3 @@"), "{d}");
    }

    #[test]
    fn git_commits_on_an_agent_branch_and_never_runs_hooks() {
        let home = std::env::temp_dir().join(format!("pagurus-git-home-{}", crate::util::new_id()));
        let proj = std::env::temp_dir().join(format!("pagurus-git-proj-{}", crate::util::new_id()));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(home.join("workspace")).unwrap();
        let g = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&proj).args(args).output().unwrap().status.success(), "{args:?}");
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(proj.join("a.txt"), "one\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "init"]);
        // a hostile repository: a hook that would run code on commit
        let hook = proj.join(".git/hooks/pre-commit");
        let marker = proj.join("hook-ran");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        open(&home, &proj.display().to_string()).unwrap();
        let cfg = crate::config::Config::default();
        let yes = |_: &str| true;
        let ctx = Ctx { home: &home, cfg: &cfg, session: "t", interactive: false, approver: Some(&yes) };
        std::fs::write(proj.join("a.txt"), "two\n").unwrap();
        assert!(run(&ctx, "git", &json!({"action":"status"})).unwrap().unwrap().contains("a.txt"));
        assert!(run(&ctx, "git", &json!({"action":"diff"})).unwrap().unwrap().contains("+two"));
        let r = run(&ctx, "git", &json!({"action":"commit","message":"change a"})).unwrap().unwrap();
        assert!(r.contains("committed on agent/") && r.contains("not pushed"), "{r}");
        assert!(!marker.exists(), "the repository's hook must not run");
        assert!(run(&ctx, "git", &json!({"action":"log","n":1})).unwrap().unwrap().contains("change a"));
        assert!(run(&ctx, "git", &json!({"action":"show","rev":"--output=/tmp/x"})).unwrap().is_err());
        close();
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn projects_are_found_checked_and_guarded() {
        let home = std::env::temp_dir().join(format!("pagurus-code-home-{}", crate::util::new_id()));
        let proj = std::env::temp_dir().join(format!("pagurus-code-proj-{}", crate::util::new_id()));
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(proj.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        std::fs::write(proj.join("AGENTS.md"), "Always run cargo fmt.").unwrap();
        std::fs::write(proj.join("src/main.rs"), "fn main() {}\n").unwrap();
        assert!(open(&home, "/").is_err());
        assert!(open(&home, &home.display().to_string()).is_err());
        let r = open(&home, &proj.display().to_string()).unwrap();
        assert!(r.contains("AGENTS.md") && r.contains("cargo test"), "{r}");
        assert!(prompt_section().contains("Always run cargo fmt."));
        assert_eq!(glob(&proj.canonicalize().unwrap(), "*.rs").unwrap(), "src/main.rs");
        let f = proj.canonicalize().unwrap().join("src/main.rs");
        assert!(check_fresh(&f).unwrap_err().contains("read"));
        note_read(&f);
        assert!(check_fresh(&f).is_ok());
        std::fs::write(&f, "fn main() { changed }\n").unwrap();
        assert!(check_fresh(&f).unwrap_err().contains("changed since"));
        close();
        assert!(check_fresh(&f).is_ok(), "no project: no rule");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
    }
}

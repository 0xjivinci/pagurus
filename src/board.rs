//! A small durable Kanban board (`$PAGURUS_HOME/board.json`): tasks with
//! dependencies, comments, retries, claim heartbeats and a circuit breaker.
//! The model plans with the `board` tool; inside `pagurus daemon` a dispatcher
//! runs ready tasks in the background (here or on a peer pagurus) and tells the
//! user when one finishes or gets stuck. Hermes keeps its board in SQLite;
//! one person's board is a few KB of JSON.

use crate::config::Config;
use crate::util::{clip, now, one_line, read, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for its parents.
    #[default]
    Todo,
    Ready,
    Running,
    /// Needs the user (or a decision); `unblock` puts it back in the queue.
    Blocked,
    Done,
    Failed,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Ready => "ready",
            Status::Running => "running",
            Status::Blocked => "blocked",
            Status::Done => "done",
            Status::Failed => "failed",
        }
    }
    fn open(self) -> bool {
        !matches!(self, Status::Done | Status::Failed)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Comment {
    pub t: u64,
    pub by: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Task {
    pub id: u32,
    pub title: String,
    pub body: String,
    /// "" = this pagurus's own worker; otherwise the name of a paired peer.
    pub assignee: String,
    pub status: Status,
    pub parents: Vec<u32>,
    pub comments: Vec<Comment>,
    /// Claims so far (a crash that gets reclaimed counts too: a task that
    /// kills the daemon must not loop forever on a Pi).
    pub attempts: u32,
    pub max_attempts: u32,
    /// Which run holds the claim ("<daemon id>#<attempt>"); a reclaimed run can't finish it.
    pub claimed_by: String,
    pub heartbeat: u64,
    /// Retry back-off: not claimed before this time.
    pub not_before: u64,
    pub result: String,
    pub created: u64,
    pub updated: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Breaker {
    pub fails: u32,
    pub until: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(default)]
pub struct Board {
    pub next: u32,
    pub tasks: Vec<Task>,
    /// Per assignee ("local" or a peer): consecutive failed runs, and until when it is paused.
    pub breaker: HashMap<String, Breaker>,
}

pub enum Outcome {
    Done(String),
    Blocked(String),
    Failed(String),
}

const MAX_TASKS: usize = 300;
const MAX_COMMENTS: usize = 40;
/// Consecutive failures of one assignee after which it is paused.
pub const BREAKER_FAILS: u32 = 3;
const BREAKER_SECS: u64 = 15 * 60;

fn who(assignee: &str) -> &str {
    if assignee.is_empty() {
        "local"
    } else {
        assignee
    }
}

impl Board {
    pub fn get(&self, id: u32) -> Result<&Task, String> {
        self.tasks.iter().find(|t| t.id == id).ok_or_else(|| format!("no board task #{id}"))
    }

    fn get_mut(&mut self, id: u32) -> Result<&mut Task, String> {
        self.tasks.iter_mut().find(|t| t.id == id).ok_or_else(|| format!("no board task #{id}"))
    }

    pub fn create(
        &mut self,
        title: &str,
        body: &str,
        assignee: &str,
        parents: &[u32],
        max_attempts: u32,
        t: u64,
    ) -> Result<u32, String> {
        let title = one_line(title, 160);
        if title.is_empty() {
            return Err("a task needs a title".into());
        }
        if body.chars().count() > 8000 {
            return Err(
                "body is longer than 8000 characters; put details in a workspace file and mention its path".into()
            );
        }
        for p in parents {
            self.get(*p)?;
        }
        if self.tasks.len() >= MAX_TASKS {
            // make room: forget the oldest finished tasks
            let mut done: Vec<(u64, u32)> =
                self.tasks.iter().filter(|t| !t.status.open()).map(|t| (t.updated, t.id)).collect();
            done.sort();
            let drop: Vec<u32> = done.iter().take(self.tasks.len() + 1 - MAX_TASKS).map(|d| d.1).collect();
            if drop.is_empty() {
                return Err(format!("the board is full ({MAX_TASKS} open tasks)"));
            }
            self.tasks.retain(|t| !drop.contains(&t.id));
        }
        self.next = self.next.max(self.tasks.iter().map(|t| t.id).max().unwrap_or(0)) + 1;
        let id = self.next;
        let mut parents = parents.to_vec();
        parents.dedup();
        self.tasks.push(Task {
            id,
            title,
            body: body.trim().to_string(),
            assignee: assignee.trim().to_string(),
            parents,
            max_attempts: max_attempts.clamp(1, 10),
            created: t,
            updated: t,
            ..Default::default()
        });
        self.promote(t);
        Ok(id)
    }

    /// todo → ready once every parent is done; → blocked if a parent failed.
    pub fn promote(&mut self, t: u64) -> Vec<String> {
        let state: HashMap<u32, Status> = self.tasks.iter().map(|x| (x.id, x.status)).collect();
        let mut notes = Vec::new();
        for task in self.tasks.iter_mut().filter(|x| x.status == Status::Todo) {
            let ps: Vec<Status> = task.parents.iter().map(|p| state.get(p).copied().unwrap_or(Status::Done)).collect();
            if let Some(i) = ps.iter().position(|s| *s == Status::Failed) {
                task.status = Status::Blocked;
                task.updated = t;
                let why = format!("parent #{} failed", task.parents[i]);
                task.comments.push(Comment { t, by: "board".into(), text: why.clone() });
                notes.push(format!("⛔ board #{} blocked: {why} — fix it, then /board unblock {}", task.id, task.id));
            } else if ps.iter().all(|s| *s == Status::Done) {
                task.status = Status::Ready;
                task.updated = t;
            }
        }
        notes
    }

    /// The next ready task whose assignee isn't paused, claimed for `run_by`.
    pub fn claim(&mut self, daemon: &str, t: u64, can_run: impl Fn(&str) -> bool) -> Option<Task> {
        let paused: Vec<String> = self.breaker.iter().filter(|(_, b)| b.until > t).map(|(k, _)| k.clone()).collect();
        let task = self
            .tasks
            .iter_mut()
            .filter(|x| x.status == Status::Ready && x.not_before <= t)
            .filter(|x| !paused.iter().any(|p| p == who(&x.assignee)) && can_run(&x.assignee))
            .min_by_key(|x| x.id)?;
        task.status = Status::Running;
        task.attempts += 1;
        task.claimed_by = format!("{daemon}#{}", task.attempts);
        task.heartbeat = t;
        task.updated = t;
        Some(task.clone())
    }

    /// The running worker is alive. False if it lost its claim.
    pub fn beat(&mut self, id: u32, run: &str, t: u64) -> bool {
        match self.get_mut(id) {
            Ok(x) if x.status == Status::Running && x.claimed_by == run => {
                x.heartbeat = t;
                true
            }
            _ => false,
        }
    }

    /// A worker's result. Ignored (None) when the run no longer holds the claim.
    pub fn finish(&mut self, id: u32, run: &str, outcome: Outcome, t: u64) -> Option<Vec<String>> {
        let x = self.get_mut(id).ok().filter(|x| x.status == Status::Running && x.claimed_by == run)?;
        x.claimed_by.clear();
        x.updated = t;
        let key = who(&x.assignee).to_string();
        let mut notes = Vec::new();
        let (id, title, attempts, max) = (x.id, x.title.clone(), x.attempts, x.max_attempts);
        match outcome {
            Outcome::Done(r) => {
                x.status = Status::Done;
                x.result = clip(r.trim(), 6000);
                notes.push(format!("✅ board #{id} done: {title}\n{}", clip(r.trim(), 1500)));
                self.breaker.remove(&key);
                notes.extend(self.promote(t));
            }
            Outcome::Blocked(why) => {
                x.status = Status::Blocked;
                x.comments.push(Comment { t, by: "worker".into(), text: clip(why.trim(), 2000) });
                notes.push(format!(
                    "⛔ board #{id} blocked: {title}\n{}\n(/board unblock {id} when it's sorted)",
                    clip(why.trim(), 800)
                ));
                self.breaker.remove(&key);
            }
            Outcome::Failed(e) => {
                x.comments.push(Comment {
                    t,
                    by: "worker".into(),
                    text: format!("attempt {attempts} failed: {}", clip(e.trim(), 1000)),
                });
                if attempts >= max {
                    x.status = Status::Failed;
                    x.result = clip(e.trim(), 2000);
                    notes.push(format!(
                        "❌ board #{id} failed after {attempts} attempt(s): {title}\n{}",
                        one_line(&e, 400)
                    ));
                    notes.extend(self.promote(t));
                } else {
                    x.status = Status::Ready;
                    x.not_before = t + 60 * u64::from(attempts); // back off a little more each time
                    eprintln!("pagurus: board #{id} attempt {attempts}/{max} failed, retrying: {}", one_line(&e, 200));
                }
                let b = self.breaker.entry(key.clone()).or_default();
                b.fails += 1;
                if b.fails >= BREAKER_FAILS {
                    b.fails = 0;
                    b.until = t + BREAKER_SECS;
                    notes.push(format!(
                        "⏸ board: {BREAKER_FAILS} failures in a row for {key}; pausing its tasks for {} minutes",
                        BREAKER_SECS / 60
                    ));
                }
            }
        }
        Some(notes)
    }

    /// Running tasks whose worker died (`alive` says no) or stopped beating
    /// for `timeout` seconds go back to the queue, or fail once out of attempts.
    pub fn reclaim(&mut self, t: u64, timeout: u64, alive: impl Fn(&str) -> bool) -> Vec<String> {
        let mut notes = Vec::new();
        for x in self.tasks.iter_mut().filter(|x| x.status == Status::Running) {
            let stale = t.saturating_sub(x.heartbeat) > timeout;
            if alive(&x.claimed_by) && !stale {
                continue;
            }
            let why =
                if stale { format!("no heartbeat for {} min", timeout / 60) } else { "its worker stopped".to_string() };
            x.claimed_by.clear();
            x.updated = t;
            x.comments.push(Comment { t, by: "board".into(), text: format!("reclaimed: {why}") });
            if x.attempts >= x.max_attempts {
                x.status = Status::Failed;
                x.result = format!("reclaimed after {} attempt(s): {why}", x.attempts);
                notes.push(format!("❌ board #{} failed: {} ({why}, out of attempts)", x.id, x.title));
            } else {
                x.status = Status::Ready;
            }
        }
        if !notes.is_empty() {
            notes.extend(self.promote(t));
        }
        notes
    }

    pub fn comment(&mut self, id: u32, by: &str, text: &str, t: u64) -> Result<(), String> {
        let text = clip(text.trim(), 2000);
        if text.is_empty() {
            return Err("comment is empty".into());
        }
        let x = self.get_mut(id)?;
        x.comments.push(Comment { t, by: by.into(), text });
        if x.comments.len() > MAX_COMMENTS {
            x.comments.remove(0);
        }
        x.updated = t;
        Ok(())
    }

    /// Marked done by hand (the user, or the planning model itself).
    pub fn complete(&mut self, id: u32, result: &str, t: u64) -> Result<Vec<String>, String> {
        let x = self.get_mut(id)?;
        x.status = Status::Done;
        x.result = clip(result.trim(), 6000);
        x.claimed_by.clear();
        x.updated = t;
        Ok(self.promote(t))
    }

    pub fn block(&mut self, id: u32, reason: &str, t: u64) -> Result<(), String> {
        let x = self.get_mut(id)?;
        if !x.status.open() {
            return Err(format!("#{id} is already {}", x.status.name()));
        }
        x.status = Status::Blocked;
        x.claimed_by.clear();
        x.updated = t;
        x.comments.push(Comment { t, by: "user".into(), text: format!("blocked: {}", clip(reason.trim(), 1000)) });
        Ok(())
    }

    /// Back in the queue with fresh attempts (blocked or failed tasks).
    pub fn unblock(&mut self, id: u32, t: u64) -> Result<String, String> {
        let x = self.get_mut(id)?;
        if !matches!(x.status, Status::Blocked | Status::Failed) {
            return Err(format!("#{id} is {}, not blocked", x.status.name()));
        }
        x.status = Status::Todo;
        x.attempts = 0;
        x.not_before = 0;
        x.updated = t;
        let key = who(&x.assignee).to_string();
        let title = x.title.clone();
        self.breaker.remove(&key);
        self.promote(t);
        let s = self.get(id)?.status;
        Ok(format!(
            "#{id} {title}: {}",
            if s == Status::Ready { "back in the queue" } else { "waiting for its parents" }
        ))
    }
}

// ---------- storage ----------

fn path(home: &Path) -> std::path::PathBuf {
    home.join("board.json")
}

/// Load, change and save the board under a lock file, so the daemon, its
/// workers and `pagurus board` never overwrite each other.
pub fn with<R>(home: &Path, f: impl FnOnce(&mut Board) -> R) -> Result<R, String> {
    static LOCAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
    let lock = home.join("board.lock");
    let _ = std::fs::create_dir_all(home);
    let mut tries = 0;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(_) if tries < 250 => {
                // a lock left by a crashed process
                let old = std::fs::metadata(&lock).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok());
                if old.is_some_and(|d| d > Duration::from_secs(20)) {
                    let _ = std::fs::remove_file(&lock);
                }
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("board is locked ({}): {e}", lock.display())),
        }
    }
    let raw = read(&path(home));
    let res = (|| {
        let mut b: Board = if raw.trim().is_empty() {
            Board::default()
        } else {
            serde_json::from_str(&raw).map_err(|e| format!("board.json: {e}"))?
        };
        let r = f(&mut b);
        let out = serde_json::to_string_pretty(&b).map_err(|e| e.to_string())?;
        if out != raw {
            write_atomic(&path(home), &out)?;
        }
        Ok(r)
    })();
    let _ = std::fs::remove_file(&lock);
    res
}

// ---------- text ----------

fn line(t: &Task) -> String {
    let to = if t.assignee.is_empty() { String::new() } else { format!(" → {}", t.assignee) };
    let deps = if t.parents.is_empty() || t.status != Status::Todo {
        String::new()
    } else {
        format!(" (after {})", t.parents.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(", "))
    };
    format!("#{} [{}] {}{to}{deps}", t.id, t.status.name(), t.title)
}

pub fn list_text(home: &Path, filter: &str) -> String {
    let b = match with(home, |b| b.tasks.clone()) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let all = filter == "all";
    let shown: Vec<String> = b
        .iter()
        .filter(|t| {
            if all {
                true
            } else if filter.is_empty() {
                t.status.open() || now().saturating_sub(t.updated) < 86_400
            } else {
                t.status.name() == filter
            }
        })
        .map(line)
        .collect();
    if shown.is_empty() {
        return if b.is_empty() {
            "The board is empty.".into()
        } else {
            "Nothing open on the board (`all` lists finished tasks too).".into()
        };
    }
    shown.join("\n")
}

pub fn show_text(home: &Path, id: u32) -> String {
    let r = with(home, |b| {
        let t = b.get(id)?.clone();
        let parents: Vec<String> = t.parents.iter().filter_map(|p| b.get(*p).ok()).map(line).collect();
        Ok::<_, String>((t, parents))
    });
    let (t, parents) = match r.and_then(|x| x) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let mut s = format!("{}\nattempts {}/{}", line(&t), t.attempts, t.max_attempts);
    if !t.body.is_empty() {
        s += &format!("\n\n{}", clip(&t.body, 2000));
    }
    if !parents.is_empty() {
        s += &format!("\n\nDepends on:\n{}", parents.join("\n"));
    }
    if !t.result.is_empty() {
        s += &format!("\n\nResult:\n{}", clip(&t.result, 2500));
    }
    if !t.comments.is_empty() {
        s += "\n\nComments:";
        for c in t.comments.iter().rev().take(10).rev() {
            s += &format!("\n- {} {}: {}", crate::util::fmt_local(c.t), c.by, one_line(&c.text, 400));
        }
    }
    s
}

/// `/board …` and `pagurus board …`.
pub fn command(home: &Path, cfg: &Config, args: &[&str]) -> Result<String, String> {
    let id = |i: usize| {
        args.get(i)
            .map(|s| s.trim_start_matches('#'))
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or("which task? give its number")
    };
    let rest = |i: usize| args.get(i..).map(|r| r.join(" ")).unwrap_or_default();
    let t = now();
    match args.first().copied().unwrap_or("") {
        "" | "list" | "ls" => Ok(list_text(home, args.get(1).copied().unwrap_or(""))),
        "done" | "complete" if args.len() > 1 => with(home, |b| b.complete(id(1)?, &rest(2), t).map(|_| "marked done".to_string()))?,
        "all" | "todo" | "ready" | "running" | "blocked" | "done" | "failed" => Ok(list_text(home, args[0])),
        "show" => Ok(show_text(home, id(1)?)),
        "add" => {
            let text = rest(1);
            let (title, body) = text.split_once(" -- ").unwrap_or((&text, ""));
            let n = with(home, |b| b.create(title, body, "", &[], cfg.board.max_attempts, t))??;
            Ok(format!("added #{n}{}", if cfg.board.workers == 0 { " (board.workers is 0: nothing runs it)" } else { "" }))
        }
        "unblock" | "retry" => with(home, |b| b.unblock(id(1)?, t))?,
        "block" => with(home, |b| b.block(id(1)?, &rest(2), t).map(|_| "blocked".to_string()))?,
        "comment" => with(home, |b| b.comment(id(1)?, "user", &rest(2), t).map(|_| "noted".to_string()))?,
        s if s.trim_start_matches('#').parse::<u32>().is_ok() => Ok(show_text(home, id(0)?)),
        x => Err(format!(
            "unknown `{x}`. board [list|all|blocked|…] · board <id> · board add <title> [-- body] · board unblock|block|done|comment <id> …"
        )),
    }
}

// ---------- the model's tool ----------

pub fn tool(home: &Path, cfg: &Config, a: &Value) -> Result<String, String> {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    let id = || a["id"].as_u64().map(|i| i as u32).ok_or("missing integer argument `id`");
    let t = now();
    match a["action"].as_str().unwrap_or("") {
        "create" => {
            let assignee = s("assignee");
            let known = crate::peer::names(home);
            if !assignee.is_empty()
                && !matches!(assignee.as_str(), "local" | "me" | "self")
                && !known.contains(&assignee)
            {
                return Err(format!(
                    "no peer `{assignee}`; paired peers: {}",
                    if known.is_empty() { "none".into() } else { known.join(", ") }
                ));
            }
            let assignee = if matches!(assignee.as_str(), "local" | "me" | "self") { String::new() } else { assignee };
            let parents: Vec<u32> = a["parents"]
                .as_array()
                .map(|p| p.iter().filter_map(|x| x.as_u64()).map(|x| x as u32).collect())
                .unwrap_or_default();
            let max = a["max_attempts"].as_u64().map(|m| m as u32).unwrap_or(cfg.board.max_attempts);
            let body = crate::util::redact(&s("body"));
            let n = with(home, |b| b.create(&s("title"), &body, &assignee, &parents, max, t))??;
            let state = with(home, |b| b.get(n).map(|x| x.status.name()).unwrap_or("?"))?;
            let runs = if cfg.board.workers == 0 {
                "; board.workers is 0, so nothing will run it"
            } else {
                "; the daemon runs it in the background and tells the user"
            };
            Ok(format!("created #{n} [{state}]{runs}"))
        }
        "list" => Ok(list_text(home, &s("status"))),
        "show" => Ok(show_text(home, id()?)),
        "comment" => with(home, |b| b.comment(id()?, "pagurus", &s("text"), t).map(|_| "comment added".to_string()))?,
        "complete" => with(home, |b| {
            b.complete(id()?, &s("result"), t)
                .map(|n| format!("marked done{}", if n.is_empty() { "" } else { "; dependants updated" }))
        })?,
        "block" => with(home, |b| b.block(id()?, &s("reason"), t).map(|_| "blocked".to_string()))?,
        "unblock" => with(home, |b| b.unblock(id()?, t))?,
        x => Err(format!("unknown action `{x}`")),
    }
}

// ---------- dispatcher (inside `pagurus daemon`) ----------

/// What background work (board workers, peer requests) needs from the daemon:
/// a way to tell the owner something, and to ask them to approve a command.
pub struct Hooks {
    pub notify: Box<dyn Fn(String) + Send + Sync>,
    pub approve: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Someone can answer approvals (Telegram); otherwise commands that need one are refused as unattended.
    pub can_ask: bool,
}

/// The prompt a worker gets: the card, what its parents produced, the comments.
fn brief(b: &Board, t: &Task) -> String {
    let mut s = format!("Board task #{}: {}\n", t.id, t.title);
    if !t.body.is_empty() {
        s += &format!("\n{}\n", t.body);
    }
    let parents: Vec<String> = t
        .parents
        .iter()
        .filter_map(|p| b.get(*p).ok())
        .map(|p| format!("#{} {}:\n{}", p.id, p.title, clip(&p.result, 1500)))
        .collect();
    if !parents.is_empty() {
        s += &format!("\n[Results of the tasks this one builds on]\n{}\n", parents.join("\n\n"));
    }
    if !t.comments.is_empty() {
        s += "\n[Comments on this task]\n";
        for c in t.comments.iter().rev().take(8).rev() {
            s += &format!("- {}: {}\n", c.by, one_line(&c.text, 300));
        }
    }
    if t.attempts > 1 {
        s += &format!(
            "\n(This is attempt {} of {}; see the comments for what went wrong before.)\n",
            t.attempts, t.max_attempts
        );
    }
    s + "\nDo the task. Your final reply is stored as its result and the next tasks build on it, so include the concrete \
         outcome (paths, numbers, names). If you can't finish without a decision or something only the user can do, reply \
         with BLOCKED: and the reason instead."
}

fn outcome(r: Result<String, String>) -> Outcome {
    match r {
        Ok(text) => {
            let t = text.trim();
            match t.strip_prefix("BLOCKED:").or_else(|| t.strip_prefix("BLOCKED")) {
                Some(why) => Outcome::Blocked(why.trim().to_string()),
                None if t.starts_with("(stopped by the user") => Outcome::Blocked("stopped by the user".into()),
                None => Outcome::Done(t.to_string()),
            }
        }
        Err(e) => Outcome::Failed(e),
    }
}

pub(crate) fn run_task(home: &Path, cfg: &Config, hooks: &Hooks, task: &Task, prompt: &str) -> Result<String, String> {
    if task.assignee.is_empty() {
        let label = format!("(board task #{}: {})", task.id, one_line(&task.title, 60));
        let ask = |cmd: &str| (hooks.approve)(&format!("{cmd}\n{label}"));
        crate::agent::run_sub(
            home,
            cfg,
            prompt,
            crate::agent::BOARD_INTRO,
            if hooks.can_ask { Some(&ask) } else { None },
        )
        .map(|(r, _)| r)
    } else {
        crate::peer::ask(home, cfg, &task.assignee, prompt)
    }
}

/// Start the dispatcher thread. `workers` tasks run at once (1 suits a Pi).
pub fn spawn(home: std::path::PathBuf, cfg: Config, hooks: Arc<Hooks>) {
    if cfg.board.workers == 0 {
        return;
    }
    let me = format!("{}-{}", std::process::id(), now());
    let running = Arc::new(AtomicUsize::new(0));
    let timeout = cfg.board.claim_timeout_minutes.max(2) * 60;
    let _ = std::thread::Builder::new().name("pagurus-board".into()).stack_size(256 * 1024).spawn(move || loop {
        // a claim from an earlier daemon (crashed, restarted) is dead: it's ours or nobody's
        let alive = |run: &str| run.starts_with(&format!("{me}#"));
        let notes = with(&home, |b| {
            let mut n = b.reclaim(now(), timeout, alive);
            n.extend(b.promote(now()));
            n
        });
        match notes {
            Ok(n) => n.into_iter().for_each(|m| (hooks.notify)(m)),
            Err(e) => eprintln!("pagurus: board: {e}"),
        }
        while running.load(Ordering::SeqCst) < cfg.board.workers as usize {
            let known = crate::peer::names(&home);
            let claimed = with(&home, |b| {
                let t = b.claim(&me, now(), |a| a.is_empty() || known.iter().any(|k| k == a))?;
                Some((brief(b, &t), t))
            });
            let Ok(Some((prompt, task))) = claimed else { break };
            running.fetch_add(1, Ordering::SeqCst);
            let (home, cfg, hooks, running) = (home.clone(), cfg.clone(), hooks.clone(), running.clone());
            let run = task.claimed_by.clone();
            let r = std::thread::Builder::new()
                .name(format!("pagurus-board-{}", task.id))
                .stack_size(1024 * 1024)
                .spawn(move || {
                    eprintln!("pagurus: board #{} started: {}", task.id, one_line(&task.title, 80));
                    let stop = AtomicBool::new(false);
                    let result = std::thread::scope(|sc| {
                        sc.spawn(|| {
                            // keep the claim alive while the turn runs
                            let mut n = 0;
                            while !stop.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_secs(1));
                                n += 1;
                                if n % 60 == 0 && !with(&home, |b| b.beat(task.id, &run, now())).unwrap_or(true) {
                                    break;
                                }
                            }
                        });
                        let r = run_task(&home, &cfg, &hooks, &task, &prompt);
                        stop.store(true, Ordering::Relaxed);
                        r
                    });
                    match with(&home, |b| b.finish(task.id, &run, outcome(result), now())) {
                        Ok(Some(notes)) => notes.into_iter().for_each(|m| (hooks.notify)(m)),
                        Ok(None) => {
                            eprintln!("pagurus: board #{}: result dropped, the task was reclaimed meanwhile", task.id)
                        }
                        Err(e) => eprintln!("pagurus: board: {e}"),
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                });
            if r.is_err() {
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    });
}

/// One line for `pagurus doctor`.
pub fn summary(home: &Path) -> String {
    let tasks = with(home, |b| b.tasks.clone()).unwrap_or_default();
    let n = |s: Status| tasks.iter().filter(|t| t.status == s).count();
    format!(
        "{} open ({} ready, {} running, {} blocked, {} waiting) · {} done · {} failed",
        tasks.iter().filter(|t| t.status.open()).count(),
        n(Status::Ready),
        n(Status::Running),
        n(Status::Blocked),
        n(Status::Todo),
        n(Status::Done),
        n(Status::Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_gate_and_release() {
        let mut b = Board::default();
        let a = b.create("research", "", "", &[], 2, 100).unwrap();
        let c = b.create("write", "", "", &[a], 2, 100).unwrap();
        assert!(b.create("x", "", "", &[99], 2, 100).is_err(), "unknown parent");
        assert_eq!(b.get(a).unwrap().status, Status::Ready);
        assert_eq!(b.get(c).unwrap().status, Status::Todo);
        let t = b.claim("d1", 100, |_| true).unwrap();
        assert_eq!(t.id, a);
        assert!(b.claim("d1", 100, |_| true).is_none(), "child waits for its parent");
        let notes = b.finish(a, &t.claimed_by, Outcome::Done("found it".into()), 101).unwrap();
        assert!(notes[0].contains("done"));
        assert_eq!(b.get(c).unwrap().status, Status::Ready);
        let t2 = b.claim("d1", 102, |_| true).unwrap();
        assert!(brief(&b, &t2).contains("found it"), "the child sees its parent's result");
    }

    #[test]
    fn retries_then_fails_and_blocks_children() {
        let mut b = Board::default();
        let a = b.create("flaky", "", "", &[], 2, 0).unwrap();
        let c = b.create("after", "", "", &[a], 2, 0).unwrap();
        let t = b.claim("d", 0, |_| true).unwrap();
        b.finish(a, &t.claimed_by, Outcome::Failed("boom".into()), 10).unwrap();
        assert_eq!(b.get(a).unwrap().status, Status::Ready);
        assert!(b.claim("d", 20, |_| true).is_none(), "back-off");
        let t = b.claim("d", 100, |_| true).unwrap();
        assert_eq!(t.attempts, 2);
        let notes = b.finish(a, &t.claimed_by, Outcome::Failed("boom".into()), 101).unwrap();
        assert!(notes.iter().any(|n| n.contains("failed after 2")));
        assert_eq!(b.get(a).unwrap().status, Status::Failed);
        assert_eq!(b.get(c).unwrap().status, Status::Blocked);
        b.unblock(a, 200).unwrap();
        assert_eq!(b.get(a).unwrap().status, Status::Ready);
        assert_eq!(b.get(a).unwrap().attempts, 0);
    }

    #[test]
    fn crashed_and_stale_work_is_reclaimed_and_late_results_dropped() {
        let mut b = Board::default();
        let a = b.create("long", "", "", &[], 3, 0).unwrap();
        let old = b.claim("old-daemon", 0, |_| true).unwrap();
        // a new daemon: the old claim is dead
        b.reclaim(5, 600, |run| run.starts_with("new#"));
        assert_eq!(b.get(a).unwrap().status, Status::Ready);
        assert!(
            b.finish(a, &old.claimed_by, Outcome::Done("late".into()), 6).is_none(),
            "a reclaimed run can't finish"
        );
        let t = b.claim("new", 10, |_| true).unwrap();
        assert!(b.beat(a, &t.claimed_by, 300));
        assert!(b.reclaim(800, 600, |_| true).is_empty());
        assert_eq!(b.get(a).unwrap().status, Status::Running, "heartbeat kept it");
        b.reclaim(1000, 600, |_| true);
        assert_eq!(b.get(a).unwrap().status, Status::Ready, "silent for too long");
        let t = b.claim("new", 1000, |_| true).unwrap();
        assert_eq!(t.attempts, 3);
        let notes = b.reclaim(2000, 600, |_| false);
        assert!(notes[0].contains("out of attempts"));
        assert_eq!(b.get(a).unwrap().status, Status::Failed);
    }

    #[test]
    fn breaker_pauses_a_failing_assignee() {
        let mut b = Board::default();
        for i in 0..4 {
            b.create(&format!("t{i}"), "", "pi", &[], 1, 0).unwrap();
        }
        let local = b.create("mine", "", "", &[], 1, 0).unwrap();
        let mut notes = Vec::new();
        for _ in 0..3 {
            let t = b.claim("d", 0, |a| a == "pi").unwrap();
            notes.extend(b.finish(t.id, &t.claimed_by, Outcome::Failed("unreachable".into()), 1).unwrap());
        }
        assert!(notes.iter().any(|n| n.contains("pausing")), "{notes:?}");
        let t = b.claim("d", 2, |_| true).unwrap();
        assert_eq!(t.id, local, "the paused peer's task is skipped, others still run");
        assert!(b.claim("d", 2, |_| true).is_none());
        assert!(b.claim("d", 2 + BREAKER_SECS, |_| true).is_some(), "it closes again");
    }

    #[test]
    fn worker_replies_map_to_outcomes() {
        assert!(
            matches!(outcome(Ok("BLOCKED: need the wifi password".into())), Outcome::Blocked(w) if w.starts_with("need"))
        );
        assert!(matches!(outcome(Ok("all good".into())), Outcome::Done(_)));
        assert!(matches!(outcome(Err("model down".into())), Outcome::Failed(_)));
    }

    #[test]
    fn stored_on_disk_and_tool_works() {
        let home = std::env::temp_dir().join(format!("pagurus-board-{}", crate::util::new_id()));
        let cfg = Config::default();
        let out =
            tool(&home, &cfg, &serde_json::json!({"action":"create","title":"Check backups","body":"look at /srv"}))
                .unwrap();
        assert!(out.starts_with("created #1 [ready]"), "{out}");
        assert!(tool(&home, &cfg, &serde_json::json!({"action":"create","title":"x","assignee":"nobody"})).is_err());
        tool(&home, &cfg, &serde_json::json!({"action":"comment","id":1,"text":"use rsync logs"})).unwrap();
        assert!(show_text(&home, 1).contains("use rsync logs"));
        assert!(command(&home, &cfg, &["block", "1", "waiting"]).is_ok());
        assert!(list_text(&home, "").contains("#1 [blocked] Check backups"));
        assert!(command(&home, &cfg, &["unblock", "1"]).unwrap().contains("back in the queue"));
    }
}

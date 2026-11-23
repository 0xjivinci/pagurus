//! Background processes, after Hermes' process registry, in small: start a
//! long-running command (always with the user's approval), look at its output
//! later, stop it. At most `MAX_RUNNING` at once, each keeps the last
//! `BUF` bytes of output and is killed when its time limit is up, so nothing
//! started from a chat can quietly run forever on a 1 GB Pi. The daemon tells
//! the user when one ends.

use crate::util::{now, one_line};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

pub const MAX_RUNNING: usize = 3;
const BUF: usize = 64 * 1024;
pub const DEFAULT_MINUTES: u64 = 60;
pub const MAX_MINUTES: u64 = 24 * 60;
const KEEP_FINISHED: usize = 8;

struct Proc {
    id: u32,
    cmd: String,
    started: u64,
    deadline: u64,
    child: Option<Child>,
    /// Last BUF bytes of combined output, and how many bytes came in total.
    out: Arc<Mutex<(Vec<u8>, usize)>>,
    /// How far `output` has been read (total-bytes position).
    seen: usize,
    exit: Option<String>,
    /// The end was reported (to the daemon's owner chat).
    told: bool,
}

static PROCS: Mutex<Vec<Proc>> = Mutex::new(Vec::new());

#[cfg(not(windows))]
fn command(cmd: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("sh");
    c.arg("-c").arg(format!("exec 2>&1\n{cmd}")).process_group(0);
    crate::tools::clean_env(&mut c); // never pagurus's keys
    c
}

#[cfg(windows)]
fn command(cmd: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/C").arg(format!("{cmd} 2>&1"));
    crate::tools::clean_env(&mut c);
    c
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    let _ = Command::new("sh").arg("-c").arg(format!("kill -9 -{pid} 2>/dev/null")).status();
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).status();
}

/// Reap exited processes and kill those past their deadline.
fn refresh(v: &mut [Proc]) {
    for p in v.iter_mut() {
        let Some(child) = p.child.as_mut() else { continue };
        let status = match child.try_wait() {
            Ok(Some(s)) => Some(match s.code() {
                Some(0) => "exit 0".to_string(),
                Some(c) => format!("exit {c}"),
                None => "killed by a signal".to_string(),
            }),
            Ok(None) if now() >= p.deadline => {
                kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                Some(format!("killed at its {}-minute limit", (p.deadline - p.started) / 60))
            }
            Ok(None) => None,
            Err(e) => Some(format!("lost: {e}")),
        };
        if status.is_some() {
            p.exit = status;
            p.child = None;
        }
    }
}

fn line(p: &Proc) -> String {
    let state = match &p.exit {
        None => format!("running {}m (limit {}m)", now().saturating_sub(p.started) / 60, (p.deadline - p.started) / 60),
        Some(e) => format!("ended: {e}"),
    };
    let total = p.out.lock().map(|o| o.1).unwrap_or(0);
    let size = if total < 1024 { format!("{total} B") } else { format!("{} KB", total / 1024) };
    format!("#{} `{}` · {state} · {size} output", p.id, one_line(&p.cmd, 80))
}

/// Start `cmd` in `cwd`. The caller has already got the user's approval.
pub fn start(cmd: &str, cwd: &Path, minutes: u64) -> Result<String, String> {
    let mut v = PROCS.lock().map_err(|_| "process table unavailable")?;
    refresh(&mut v);
    if v.iter().filter(|p| p.exit.is_none()).count() >= MAX_RUNNING {
        return Err(format!("{MAX_RUNNING} background processes are already running; kill one first"));
    }
    let _ = std::fs::create_dir_all(cwd);
    let mut child = command(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let out = Arc::new(Mutex::new((Vec::new(), 0usize)));
    if let Some(mut so) = child.stdout.take() {
        let out = out.clone();
        let _ = std::thread::Builder::new().name("pagurus-proc".into()).stack_size(64 * 1024).spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = so.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut o) = out.lock() {
                    o.0.extend_from_slice(&chunk[..n]);
                    o.1 += n;
                    let extra = o.0.len().saturating_sub(BUF);
                    if extra > 0 {
                        o.0.drain(..extra);
                    }
                }
            }
        });
    }
    // drop finished entries beyond a few, so the table stays small
    while v.iter().filter(|p| p.exit.is_some()).count() >= KEEP_FINISHED {
        if let Some(i) = v.iter().position(|p| p.exit.is_some()) {
            v.remove(i);
        }
    }
    let id = v.iter().map(|p| p.id).max().unwrap_or(0) + 1;
    let minutes = minutes.clamp(1, MAX_MINUTES);
    let t = now();
    v.push(Proc {
        id,
        cmd: cmd.to_string(),
        started: t,
        deadline: t + minutes * 60,
        child: Some(child),
        out,
        seen: 0,
        exit: None,
        told: false,
    });
    Ok(format!(
        "started background process #{id} (killed after {minutes} min). Check it later with process action=output id={id}; \
         don't wait for it in a loop."
    ))
}

pub fn list() -> String {
    let Ok(mut v) = PROCS.lock() else { return "process table unavailable".into() };
    refresh(&mut v);
    if v.is_empty() {
        return "No background processes.".into();
    }
    v.iter().map(line).collect::<Vec<_>>().join("\n")
}

/// New output since the last look (or the last `BUF` bytes if more came in).
pub fn output(id: u32) -> Result<String, String> {
    let mut v = PROCS.lock().map_err(|_| "process table unavailable")?;
    refresh(&mut v);
    let p = v.iter_mut().find(|p| p.id == id).ok_or_else(|| format!("no background process #{id}"))?;
    let (bytes, total) = p.out.lock().map(|o| (o.0.clone(), o.1)).unwrap_or_default();
    let start_pos = total - bytes.len();
    let from = p.seen.max(start_pos) - start_pos;
    let skipped = p.seen.max(start_pos) - p.seen;
    p.seen = total;
    let text = String::from_utf8_lossy(&bytes[from..]).into_owned();
    let mut s = line(p);
    if skipped > 0 {
        s += &format!("\n[{} KB of older output dropped]", skipped / 1024);
    }
    s += &if text.trim().is_empty() { "\n(no new output)".to_string() } else { format!("\n{text}") };
    Ok(s)
}

pub fn kill(id: u32) -> Result<String, String> {
    let mut v = PROCS.lock().map_err(|_| "process table unavailable")?;
    refresh(&mut v);
    let p = v.iter_mut().find(|p| p.id == id).ok_or_else(|| format!("no background process #{id}"))?;
    let Some(mut child) = p.child.take() else {
        return Ok(format!("#{id} had already ended ({})", p.exit.clone().unwrap_or_default()));
    };
    kill_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
    p.exit = Some("killed on request".into());
    p.told = true;
    Ok(format!("killed #{id}"))
}

/// Processes that ended since the last call, as messages for the user, with
/// the last lines of output. Also enforces time limits while nobody looks.
pub fn take_finished() -> Vec<String> {
    let Ok(mut v) = PROCS.lock() else { return vec![] };
    refresh(&mut v);
    let mut msgs = Vec::new();
    for p in v.iter_mut().filter(|p| p.exit.is_some() && !p.told) {
        p.told = true;
        let tail = p.out.lock().map(|o| String::from_utf8_lossy(&o.0).into_owned()).unwrap_or_default();
        let last: Vec<&str> = tail.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
        msgs.push(format!(
            "⚙️ background #{} `{}` {}{}",
            p.id,
            one_line(&p.cmd, 80),
            p.exit.clone().unwrap_or_default(),
            if last.is_empty() { String::new() } else { format!("\n{}", crate::util::clip(&last.join("\n"), 600)) }
        ));
    }
    msgs
}

/// Wait a little for output in tests.
#[cfg(test)]
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(400));
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn start_read_kill_and_limits() {
        let tmp = std::env::temp_dir();
        let r = start("echo one; sleep 0.2; echo two; sleep 30", &tmp, 5).unwrap();
        let id: u32 = r.split('#').nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        settle();
        let o = output(id).unwrap();
        assert!(o.contains("one") && o.contains("two") && o.contains("running"), "{o}");
        assert!(output(id).unwrap().contains("(no new output)"), "output is incremental");
        assert!(list().contains(&format!("#{id}")));
        assert!(kill(id).unwrap().contains("killed"));
        assert!(output(id).unwrap().contains("killed on request"));
        // a quick one ends and is reported once
        let r = start("echo done-here", &tmp, 5).unwrap();
        let id2: u32 = r.split('#').nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        settle();
        let msgs = take_finished();
        assert!(
            msgs.iter().any(|m| m.contains(&format!("#{id2}")) && m.contains("exit 0") && m.contains("done-here")),
            "{msgs:?}"
        );
        assert!(take_finished().iter().all(|m| !m.contains(&format!("#{id2}"))));
        assert!(output(999).is_err());
    }
}

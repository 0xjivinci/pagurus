//! Who decides what runs: yolo (everything above the floor runs without asking,
//! for a limited time) and the audit log of every command and how it was decided.
//!
//! The terminal and `pagurus run --yolo` use the process-wide timer here; the
//! daemon keeps its own per-chat timer, so a yolo switched on in one chat never
//! covers another chat, a peer's request or a board task.

use crate::util::{now, one_line};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
#[cfg(not(test))]
use std::sync::atomic::{AtomicU64, Ordering};

/// The longest a yolo may last; the default is 30 minutes.
pub const MAX_YOLO_SECS: u64 = 12 * 3600;
const DEFAULT_YOLO_SECS: u64 = 30 * 60;

#[cfg(not(test))]
static YOLO_UNTIL: AtomicU64 = AtomicU64::new(0);
#[cfg(not(test))]
fn load() -> u64 {
    YOLO_UNTIL.load(Ordering::Relaxed)
}
#[cfg(not(test))]
fn store(t: u64) {
    YOLO_UNTIL.store(t, Ordering::Relaxed)
}

// tests run side by side: one test's yolo must not approve another's commands
#[cfg(test)]
thread_local!(static YOLO_UNTIL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });
#[cfg(test)]
fn load() -> u64 {
    YOLO_UNTIL.with(|u| u.get())
}
#[cfg(test)]
fn store(t: u64) {
    YOLO_UNTIL.with(|u| u.set(t))
}

pub fn set_yolo(secs: u64) {
    store(if secs == 0 { 0 } else { now() + secs.min(MAX_YOLO_SECS) });
}

/// Seconds of yolo left in this process (terminal / `run --yolo`), if it's on.
pub fn yolo_left() -> Option<u64> {
    let t = load();
    (t > now()).then(|| t - now())
}

/// `30m`, `2h`, `90s`, `45` (minutes); empty means the default.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Some(DEFAULT_YOLO_SECS);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().ok().filter(|n| *n > 0)?;
    let secs = match unit.trim() {
        "" | "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
        "h" | "hr" | "hour" | "hours" => n * 3600,
        "s" | "sec" | "secs" => n,
        _ => return None,
    };
    Some(secs.min(MAX_YOLO_SECS))
}

/// `23m`, `1h05m`.
pub fn fmt_left(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// The `/yolo [30m | off]` command. `left` is the current state, `set` applies a new
/// length in seconds (0 = off). Returns the reply.
pub fn command(arg: &str, left: Option<u64>, set: &mut dyn FnMut(u64)) -> String {
    let a = arg.trim().to_lowercase();
    match a.as_str() {
        "off" | "stop" | "end" => {
            set(0);
            if left.is_some() { "yolo off: commands need your approval again." } else { "yolo was already off." }.into()
        }
        "status" | "?" => match left {
            Some(s) => format!("⚡ yolo on, {} left. /yolo off ends it.", fmt_left(s)),
            None => "yolo is off. `/yolo 30m` turns it on for 30 minutes.".into(),
        },
        _ => match parse_duration(&a) {
            Some(secs) => {
                set(secs);
                format!(
                    "⚡ yolo on for {}: commands run without asking, except what the safety floor blocks \
                     (root, wiping folders, disks, piping downloads into a shell, uploads, pagurus's own settings). \
                     Everything is logged (/audit). /yolo off or /stop ends it.",
                    fmt_left(secs)
                )
            }
            None => "usage: /yolo [30m | 2h | off | status] (at most 12h)".into(),
        },
    }
}

const MAX_LOG: u64 = 512 * 1024;

/// Record how a command was decided. `how`: read-only, auto, you, all, yolo,
/// denied, timeout, blocked, unattended.
pub fn audit(home: &Path, how: &str, cmd: &str) {
    let p = home.join("audit.jsonl");
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(&p, home.join("audit.jsonl.1"));
    }
    let (origin, _) = crate::util::origin();
    let line = json!({ "t": now(), "how": how, "cmd": crate::util::redact(&one_line(cmd, 400)), "from": origin });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{line}");
    }
}

/// The last `n` entries, newest last. Without `all`, read-only commands are left out
/// (they are the bulk and can't change anything).
pub fn report(home: &Path, all: bool, n: usize) -> String {
    let mut lines: Vec<Value> = [home.join("audit.jsonl.1"), home.join("audit.jsonl")]
        .iter()
        .flat_map(|p| {
            crate::util::read(p).lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect::<Vec<_>>()
        })
        .filter(|v| all || v["how"] != "read-only")
        .collect();
    if lines.is_empty() {
        return if all {
            "Nothing in the audit log yet."
        } else {
            "No commands needed a decision yet (`/audit all` includes read-only ones)."
        }
        .into();
    }
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    let mut out = String::from("Recent commands (newest last):");
    for v in lines {
        let icon = match v["how"].as_str().unwrap_or("") {
            "yolo" => "⚡",
            "you" | "all" => "✅",
            "auto" => "🤖",
            "read-only" => "👁",
            "blocked" => "⛔",
            _ => "❌",
        };
        out += &format!(
            "\n{} {icon} {:<9} {}",
            crate::util::fmt_local(v["t"].as_u64().unwrap_or(0)),
            v["how"].as_str().unwrap_or(""),
            one_line(v["cmd"].as_str().unwrap_or(""), 160)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_durations_and_expiry() {
        assert_eq!(parse_duration(""), Some(1800));
        assert_eq!(parse_duration("45"), Some(2700));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("3 days"), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("48h"), Some(MAX_YOLO_SECS), "capped");
        let mut got = None;
        let r = command("1h", None, &mut |s| got = Some(s));
        assert_eq!(got, Some(3600));
        assert!(r.contains("1h00m") && r.contains("/audit"));
        assert!(command("off", Some(5), &mut |s| got = Some(s)).contains("off"));
        assert_eq!(got, Some(0));
        assert!(command("forever", None, &mut |_| panic!("no change")).starts_with("usage"));
        assert_eq!(fmt_left(23 * 60 + 5), "23m");
    }

    #[test]
    fn audit_logs_and_reports() {
        let home = std::env::temp_dir().join(format!("pagurus-audit-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        audit(&home, "read-only", "uptime");
        audit(&home, "yolo", "cargo build");
        audit(&home, "blocked", "sudo ls");
        let r = report(&home, false, 10);
        assert!(r.contains("cargo build") && r.contains("sudo ls") && !r.contains("uptime"), "{r}");
        assert!(report(&home, true, 10).contains("uptime"));
        assert!(!report(&home, true, 1).contains("cargo build"), "only the last n");
        let _ = std::fs::remove_dir_all(&home);
    }
}

//! Standing tasks in HEARTBEAT.md, each optionally with its own interval:
//!
//!   - Check disk usage; warn me above 85%.            (default interval)
//!   - [every 5m] Run vcgencmd measure_temp; alert me if above 60'C.
//!   - [at 2026-11-16 09:00] Remind me to check for updates.   (once, local time)
//!   - [daily 08:30] Give me a short morning status.            (every day, local time)
//!   - [weekly mon 09:00] Check for OS updates.                 (every week)
//!   - [every 5m] `vcgencmd measure_temp` > 70 Pi is hot         (a check: no model needed)
//!
//! A check is a command in backticks, `>` / `<` / `>=` / `<=` and a number, then an
//! optional label. The daemon runs it itself, compares the first number in the
//! output and alerts once when it crosses the line and once when it's back.
//!
//! The daemon runs due tasks unattended and messages the user only when one
//! needs attention. The agent manages them through the `heartbeat` tool.

use crate::util::{fmt_local, now, one_line, parse_local, read, write_atomic};
use std::path::{Path, PathBuf};

/// Floor for per-task intervals: each run of a task is a model call.
pub const MIN_EVERY: u64 = 5;
/// Checks cost no tokens, so they may run every minute.
pub const MIN_EVERY_CHECK: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub cmd: String,
    /// ">", "<", ">=" or "<="
    pub op: String,
    pub limit: f64,
    pub label: String,
}

impl Check {
    /// After an alert: is the value clearly back on the safe side? A margin of
    /// 5% of the threshold stops a value hovering at the line from paging
    /// "hot / normal / hot" every few minutes (a count with limit 0 needs none).
    pub fn recovered(&self, v: f64) -> bool {
        let margin = self.limit.abs() * 0.05;
        match self.op.as_str() {
            ">" | ">=" => v <= self.limit - margin,
            _ => v >= self.limit + margin,
        }
    }

    /// Did this value cross the line?
    pub fn tripped(&self, v: f64) -> bool {
        match self.op.as_str() {
            ">" => v > self.limit,
            "<" => v < self.limit,
            ">=" => v >= self.limit,
            _ => v <= self.limit,
        }
    }
}

/// "`cmd` > 70 optional label"
pub fn parse_check(text: &str) -> Option<Check> {
    let rest = text.trim().strip_prefix('`')?;
    let (cmd, rest) = rest.split_once('`')?;
    let rest = rest.trim_start();
    let op = [">=", "<=", ">", "<"].into_iter().find(|o| rest.starts_with(o))?;
    let rest = rest[op.len()..].trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    let limit: f64 = num.parse().ok()?;
    let label = rest[num.len()..].trim().trim_start_matches(['%', ':', '-', '\'']).trim().to_string();
    (!cmd.trim().is_empty()).then(|| Check { cmd: cmd.trim().to_string(), op: op.to_string(), limit, label })
}

/// First number in a command's output, e.g. 48.9 from "temp=48.9'C", 60 from "Use%\n 60%".
pub fn first_number(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let neg = b[i] == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit) && (i == 0 || !b[i - 1].is_ascii_alphanumeric());
        if b[i].is_ascii_digit() || neg {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) {
                i += 1;
            }
            return s[start..i].parse().ok();
        }
        i += 1;
    }
    None
}

/// Daily or weekly at a local time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cal {
    /// 0 = Monday … 6 = Sunday; None = every day.
    pub weekday: Option<u8>,
    /// Minutes after local midnight.
    pub minute: u32,
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

pub fn parse_weekday(s: &str) -> Option<u8> {
    let s = s.trim().to_lowercase();
    DAYS.iter().position(|d| s.starts_with(d)).map(|i| i as u8)
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Cal {
    /// Next time strictly after `t` (unix seconds) that this fires.
    pub fn next_after(&self, t: u64) -> u64 {
        let off = crate::util::utc_offset();
        let local = t as i64 + off;
        let day = local.div_euclid(86_400);
        for d in day..day + 8 {
            // 1970-01-01 was a Thursday (weekday 3 with Monday = 0)
            let wd = (d + 3).rem_euclid(7) as u8;
            if self.weekday.is_some_and(|w| w != wd) {
                continue;
            }
            let when = d * 86_400 + self.minute as i64 * 60 - off;
            if when > t as i64 {
                return when as u64;
            }
        }
        t + 86_400
    }

    fn render(&self) -> String {
        let hm = format!("{:02}:{:02}", self.minute / 60, self.minute % 60);
        match self.weekday {
            Some(w) => format!("weekly {} {hm}", DAYS[w as usize]),
            None => format!("daily {hm}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// Minutes; None = `daemon.heartbeat_minutes`.
    pub every: Option<u64>,
    /// One-off: unix time it is due; removed after it runs.
    pub at: Option<u64>,
    /// Daily/weekly at a local time.
    pub cal: Option<Cal>,
    pub text: String,
}

impl Task {
    pub fn check(&self) -> Option<Check> {
        parse_check(&self.text)
    }
}

fn file(home: &Path) -> PathBuf {
    home.join("HEARTBEAT.md")
}

fn parse_minutes(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u64 = digits.parse().ok()?;
    match s[digits.len()..].trim() {
        "" | "m" | "min" | "mins" | "minute" | "minutes" => Some(n),
        "h" | "hr" | "hour" | "hours" => Some(n * 60),
        _ => None,
    }
}

pub fn parse_line(l: &str) -> Option<Task> {
    let t = l.trim().strip_prefix("- ")?.trim();
    if t.is_empty() {
        return None;
    }
    let task = |every, at, cal, body: &str| {
        let body = body.trim();
        (!body.is_empty()).then(|| Task { every, at, cal, text: body.to_string() })
    };
    if let Some((spec, body)) = t.strip_prefix("[every ").and_then(|r| r.split_once(']')) {
        let floor = if parse_check(body).is_some() { MIN_EVERY_CHECK } else { MIN_EVERY };
        return task(Some(parse_minutes(spec)?.max(floor)), None, None, body);
    }
    if let Some((spec, body)) = t.strip_prefix("[at ").and_then(|r| r.split_once(']')) {
        return task(None, Some(parse_local(spec)?), None, body);
    }
    if let Some((spec, body)) = t.strip_prefix("[daily ").and_then(|r| r.split_once(']')) {
        return task(None, None, Some(Cal { weekday: None, minute: parse_hhmm(spec)? }), body);
    }
    if let Some((spec, body)) = t.strip_prefix("[weekly ").and_then(|r| r.split_once(']')) {
        let (day, hm) = spec.trim().split_once(' ')?;
        return task(None, None, Some(Cal { weekday: Some(parse_weekday(day)?), minute: parse_hhmm(hm)? }), body);
    }
    task(None, None, None, t)
}

pub fn list(home: &Path) -> Vec<Task> {
    read(&file(home)).lines().filter_map(parse_line).collect()
}

fn render(t: &Task) -> String {
    if let Some(c) = t.cal {
        return format!("- [{}] {}", c.render(), t.text);
    }
    match (t.at, t.every) {
        (Some(at), _) => format!("- [at {}] {}", fmt_local(at), t.text),
        (None, Some(m)) => format!("- [every {m}m] {}", t.text),
        (None, None) => format!("- {}", t.text),
    }
}

fn when(t: &Task, default_every: u64) -> String {
    if let Some(c) = t.cal {
        return c.render();
    }
    match t.at {
        Some(at) => format!("once at {}", fmt_local(at)),
        None => format!("every {}m", t.every.unwrap_or(default_every)),
    }
}

pub fn describe(home: &Path, default_every: u64) -> String {
    let v = list(home);
    if v.is_empty() {
        return "No standing tasks.".into();
    }
    v.iter()
        .enumerate()
        .map(|(i, t)| {
            let kind = if t.check().is_some() { " (check, no model)" } else { "" };
            format!("{}. {}{kind} — {}", i + 1, when(t, default_every), t.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn add(home: &Path, text: &str, every: Option<u64>, at: Option<u64>, cal: Option<Cal>) -> Result<String, String> {
    let text = one_line(text, 400);
    if text.trim().is_empty() {
        return Err("task text is empty".into());
    }
    if text.starts_with('[') {
        return Err("put the interval in every_minutes, not in the text".into());
    }
    if list(home).iter().any(|t| t.text == text) {
        return Err("that task already exists".into());
    }
    if at.is_some_and(|t| t + 60 < now()) {
        return Err("that time is in the past".into());
    }
    let floor = if parse_check(&text).is_some() { MIN_EVERY_CHECK } else { MIN_EVERY };
    let every = if at.is_some() || cal.is_some() { None } else { every.map(|m| m.max(floor)) };
    let task = Task { every, at, cal, text };
    let mut body = read(&file(home));
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body += &render(&task);
    body.push('\n');
    write_atomic(&file(home), &body)?;
    Ok(match task.at {
        _ if task.cal.is_some() => format!("added task ({}): {}", when(&task, 0), task.text),
        Some(t) => format!("added one-off reminder for {}: {}", fmt_local(t), task.text),
        None => format!(
            "added standing task: {}{}",
            task.text,
            task.every.map(|m| format!(" (every {m} min)")).unwrap_or_default()
        ),
    })
}

/// Drop a one-off task after it ran (exact text match).
pub fn finish(home: &Path, text: &str) {
    let body = read(&file(home));
    let kept: Vec<&str> = body.lines().filter(|l| parse_line(l).is_none_or(|t| t.at.is_none() || t.text != text)).collect();
    let _ = write_atomic(&file(home), &(kept.join("\n") + "\n"));
}

pub fn remove(home: &Path, needle: &str) -> Result<String, String> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Err("say which task to remove".into());
    }
    let body = read(&file(home));
    let lines: Vec<&str> = body.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| parse_line(l).is_some_and(|t| t.text.to_lowercase().contains(&needle.to_lowercase())))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => Err(format!("no task contains `{needle}`")),
        1 => {
            let gone = parse_line(lines[hits[0]]).unwrap().text;
            let kept: Vec<&str> = lines.iter().enumerate().filter(|(i, _)| *i != hits[0]).map(|(_, l)| *l).collect();
            write_atomic(&file(home), &(kept.join("\n") + "\n"))?;
            Ok(format!("removed standing task: {gone}"))
        }
        k => Err(format!("`{needle}` matches {k} tasks; be more specific")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_intervals() {
        assert_eq!(parse_line("- check disk"), Some(Task { every: None, at: None, cal: None, text: "check disk".into() }));
        let d = parse_line("- [daily 08:30] morning status").unwrap();
        assert_eq!(d.cal, Some(Cal { weekday: None, minute: 510 }));
        assert_eq!(render(&d), "- [daily 08:30] morning status");
        let w = parse_line("- [weekly Monday 09:00] updates").unwrap();
        assert_eq!(render(&w), "- [weekly mon 09:00] updates");
        let c = parse_line("- [every 1m] `vcgencmd measure_temp` > 70 Pi is hot").unwrap();
        assert_eq!(c.every, Some(1), "checks may run every minute");
        let chk = c.check().unwrap();
        assert_eq!((chk.cmd.as_str(), chk.op.as_str(), chk.limit, chk.label.as_str()), ("vcgencmd measure_temp", ">", 70.0, "Pi is hot"));
        assert!(chk.tripped(71.2) && !chk.tripped(70.0));
        assert!(!chk.recovered(68.0) && chk.recovered(66.4), "back to normal only clearly below 70");
        assert!(parse_check("`x` > 0").unwrap().recovered(0.0), "a count is back to normal at 0");
        assert_eq!(parse_check("`df --output=pcent /` >= 90%").unwrap().label, "");
        assert_eq!(first_number("temp=48.9'C"), Some(48.9));
        assert_eq!(first_number("Use%\n 60%"), Some(60.0));
        assert_eq!(first_number("load: -3"), Some(-3.0));
        assert_eq!(first_number("none"), None);
        let t = parse_line("- [at 2026-11-16 09:00] check updates").unwrap();
        assert_eq!((t.every, t.text.as_str()), (None, "check updates"));
        assert_eq!(render(&t), "- [at 2026-11-16 09:00] check updates");
        assert_eq!(parse_line("- [every 10m] temp").unwrap().every, Some(10));
        assert_eq!(parse_line("- [every 2h] temp").unwrap().every, Some(120));
        assert_eq!(parse_line("- [every 1m] temp").unwrap().every, Some(MIN_EVERY), "floored");
        assert_eq!(parse_line("# - commented"), None);
        assert_eq!(parse_line("- [every soon] x"), None);
    }

    #[test]
    fn add_list_remove_keeps_other_lines() {
        let home = std::env::temp_dir().join(format!("pagurus-tasks-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(file(&home), "# Heartbeat\n\n# - example\n").unwrap();
        add(&home, "Run vcgencmd measure_temp; alert me above 60'C", Some(2), None, None).unwrap();
        add(&home, "Check disk usage", None, None, None).unwrap();
        assert!(add(&home, "Check disk usage", None, None, None).is_err());
        assert!(add(&home, "too late", None, Some(1000), None).is_err());
        add(&home, "remind me", None, Some(now() + 3600), None).unwrap();
        assert_eq!(list(&home).len(), 3);
        finish(&home, "remind me");
        let v = list(&home);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].every, Some(MIN_EVERY));
        assert!(remove(&home, "nothing").is_err());
        remove(&home, "VCGENCMD").unwrap();
        assert_eq!(list(&home).len(), 1);
        assert!(read(&file(&home)).starts_with("# Heartbeat\n\n# - example\n"));
    }

    #[test]
    fn calendar_next_occurrence() {
        let c = Cal { weekday: None, minute: 9 * 60 };
        let t = crate::util::parse_local("2026-11-16 08:00").unwrap();
        assert_eq!(fmt_local(c.next_after(t)), "2026-11-16 09:00");
        assert_eq!(fmt_local(c.next_after(c.next_after(t))), "2026-11-17 09:00");
        // 2026-11-16 is a Monday; next Sunday 10:00
        let w = Cal { weekday: Some(6), minute: 600 };
        assert_eq!(fmt_local(w.next_after(t)), "2026-11-22 10:00");
    }
}

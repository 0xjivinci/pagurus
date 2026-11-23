//! Skill lifecycle: pagurus counts how
//! often each skill is loaded and when it was last used. Skills unused for
//! `COLD_DAYS` are flagged "cold". Archiving moves the skill to
//! skills/.archive/, out of the prompt's skill index, and the agent may not
//! re-create an archived skill until it is restored. Like Hermes's curator,
//! pagurus retires skills *it wrote itself* when they keep failing or go
//! unused for months (ledgered, undoable); bundled and user-written skills are
//! only ever archived by the user.
//!
//! Beyond Hermes (which counts views and patches), pagurus records whether a
//! turn that followed a skill succeeded. Skills earn "trusted" after working
//! repeatedly; recent failures are shown to the model and to reflection.

use crate::util::{ago, now, read, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const COLD_DAYS: u64 = 30;

#[derive(Serialize, Deserialize, Default, Clone, Copy)]
pub struct Usage {
    pub uses: u64,
    pub last: u64,
    /// Turns that followed the skill and ended without a failed tool call.
    #[serde(default)]
    pub ok: u64,
    #[serde(default)]
    pub fail: u64,
    /// Consecutive failures (reset by a success).
    #[serde(default)]
    pub streak: u64,
}

fn usage_path(home: &Path) -> PathBuf {
    home.join("skills").join(".usage.json")
}

pub fn archive_dir(home: &Path) -> PathBuf {
    home.join("skills").join(".archive")
}

pub fn usage(home: &Path) -> BTreeMap<String, Usage> {
    serde_json::from_str(&read(&usage_path(home))).unwrap_or_default()
}

/// Called when the agent loads a skill. Best effort: a failed write never
/// breaks a turn.
pub fn record_use(home: &Path, name: &str) {
    let mut u = usage(home);
    let e = u.entry(name.to_string()).or_default();
    e.uses += 1;
    e.last = now();
    if let Ok(s) = serde_json::to_string(&u) {
        let _ = write_atomic(&usage_path(home), &s);
    }
}

/// Outcome of a turn that loaded the skill (or a 👎 on it).
pub fn record_outcome(home: &Path, name: &str, ok: bool) {
    let mut u = usage(home);
    let e = u.entry(name.to_string()).or_default();
    if ok {
        e.ok += 1;
        e.streak = 0;
    } else {
        e.fail += 1;
        e.streak += 1;
    }
    if let Ok(s) = serde_json::to_string(&u) {
        let _ = write_atomic(&usage_path(home), &s);
    }
}

/// A skill was rewritten or patched: its failure streak was about the old
/// steps, so the new version starts with a clean slate (history is kept).
pub fn revised(home: &Path, name: &str) {
    let mut u = usage(home);
    if let Some(e) = u.get_mut(name) {
        if e.streak == 0 {
            return;
        }
        e.streak = 0;
        if let Ok(s) = serde_json::to_string(&u) {
            let _ = write_atomic(&usage_path(home), &s);
        }
    }
}

/// Failures in a row after which a self-made skill is retired, if it has
/// failed more often than it worked.
pub const RETIRE_STREAK: u64 = 3;

/// Why a self-made skill should be retired now, if it should: Hermes's
/// curator archives agent-made skills that went stale; pagurus also retires
/// ones that keep failing even after reflection had the chance to fix them.
pub fn retire_reason(home: &Path, name: &str, u: Option<&Usage>) -> Option<String> {
    if let Some(u) = u.filter(|u| u.streak >= RETIRE_STREAK && u.fail > u.ok) {
        return Some(format!("failed {} times in a row (worked {}, failed {})", u.streak, u.ok, u.fail));
    }
    let limit = 2 * COLD_DAYS * 86_400;
    let idle = match u {
        Some(u) if u.last > 0 => now().saturating_sub(u.last),
        _ => age(home, name),
    };
    (idle >= limit && age(home, name) >= limit).then(|| format!("unused for {}+ days", 2 * COLD_DAYS))
}

/// The deterministic part of curation: retire self-made skills that keep
/// failing or went unused for months. No model call. Returns what it did.
pub fn auto_retire(home: &Path) -> Vec<String> {
    let u = usage(home);
    crate::learn::self_made_skills(home)
        .into_iter()
        .filter_map(|n| {
            let why = retire_reason(home, &n, u.get(&n))?;
            crate::learn::retire_skill(home, &n, &why, "curator").ok().map(|_| format!("{n} ({why})"))
        })
        .collect()
}

/// Short label for the prompt's skill list, from its track record.
pub fn tag(home: &Path, name: &str, u: Option<&Usage>) -> &'static str {
    let bundled = crate::bundled::SKILLS.iter().chain(crate::bundled::EXTRA).any(|(n, _)| *n == name);
    match u {
        Some(u) if u.streak >= 2 => "failed the last 2+ times — read it critically and fix it",
        Some(u) if u.streak == 1 => "failed last time — check its steps",
        Some(u) if u.ok >= 3 && u.ok >= 2 * u.fail => "trusted",
        _ if bundled => "",
        Some(u) if u.ok + u.fail > 0 => "",
        _ if home.join("skills").join(name).join("SKILL.md").exists() => "new, untested",
        _ => "",
    }
}

/// Skills with a failure streak, for reflection and the curator.
pub fn failing(home: &Path) -> Vec<String> {
    usage(home)
        .into_iter()
        .filter(|(n, u)| u.streak > 0 && home.join("skills").join(n).join("SKILL.md").exists())
        .map(|(n, u)| format!("{n}: worked {} time(s), failed {} (last {} in a row)", u.ok, u.fail, u.streak))
        .collect()
}

pub fn is_archived(home: &Path, name: &str) -> bool {
    archive_dir(home).join(name).join("SKILL.md").exists()
}

/// Age of a skill file in seconds (mtime), used so a freshly written skill
/// isn't called cold just because nobody has needed it yet.
fn age(home: &Path, name: &str) -> u64 {
    std::fs::metadata(home.join("skills").join(name).join("SKILL.md"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| now().saturating_sub(d.as_secs()))
        .unwrap_or(0)
}

pub fn is_cold(home: &Path, name: &str, u: Option<&Usage>) -> bool {
    let limit = COLD_DAYS * 86_400;
    let idle = match u {
        Some(u) if u.last > 0 => now().saturating_sub(u.last),
        _ => age(home, name),
    };
    idle >= limit && age(home, name) >= limit
}

pub fn list_text(home: &Path) -> String {
    let skills = crate::tools::skill_index(home);
    let u = usage(home);
    let mut s = String::new();
    if skills.is_empty() {
        s += &format!("No skills yet. pagurus writes them to {}/skills/ as it learns.\n", home.display());
    }
    let mut cold = 0;
    for (n, d) in &skills {
        let use_ = u.get(n);
        let when = match use_ {
            Some(x) if x.last > 0 && x.ok + x.fail > 0 => format!("{}× {} ✓{} ✗{}", x.uses, ago(x.last), x.ok, x.fail),
            Some(x) if x.last > 0 => format!("{}× {}", x.uses, ago(x.last)),
            _ => "unused".into(),
        };
        let flag = if is_cold(home, n, use_) {
            cold += 1;
            " cold"
        } else {
            ""
        };
        s += &format!("{n:<22} {when:<22}{flag:<5} {}\n", short(d, 60));
    }
    let archived = archived(home);
    if !archived.is_empty() {
        s += &format!("archived: {}  (pagurus skills restore <name>)\n", archived.join(", "));
    }
    if cold > 0 {
        s += &format!("{cold} cold skill(s): unused for {COLD_DAYS}+ days. `pagurus skills archive <name>` takes one out of the prompt.\n");
    }
    s
}

fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

pub fn archived(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(archive_dir(home))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().join("SKILL.md").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn archive(home: &Path, name: &str) -> Result<String, String> {
    crate::learn::valid_name(name)?;
    let from = home.join("skills").join(name);
    if !from.join("SKILL.md").exists() {
        return Err(format!("no skill `{name}`"));
    }
    let to = archive_dir(home).join(name);
    if to.exists() {
        return Err(format!("an archived `{name}` already exists in {}", to.display()));
    }
    std::fs::create_dir_all(archive_dir(home)).map_err(|e| e.to_string())?;
    std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
    Ok(format!("archived {name}: out of the prompt from the next session, and pagurus won't re-create it. Restore with `skills restore {name}`."))
}

pub fn restore(home: &Path, name: &str) -> Result<String, String> {
    crate::learn::valid_name(name)?;
    let from = archive_dir(home).join(name);
    if !from.join("SKILL.md").exists() {
        return Err(format!("no archived skill `{name}`"));
    }
    let to = home.join("skills").join(name);
    if to.exists() {
        return Err(format!("a skill `{name}` exists again; remove one of them first"));
    }
    std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
    Ok(format!("restored {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_archive_restore() {
        let home = std::env::temp_dir().join(format!("pagurus-skills-{}", crate::util::new_id()));
        write_atomic(&home.join("skills/a/SKILL.md"), "---\nname: a\ndescription: d\n---\nbody\n").unwrap();
        assert!(!is_cold(&home, "a", None), "a new skill is not cold");
        record_use(&home, "a");
        record_use(&home, "a");
        assert_eq!(usage(&home)["a"].uses, 2);
        let old = Usage { uses: 1, last: now() - (COLD_DAYS + 1) * 86_400, ..Default::default() };
        assert_eq!(tag(&home, "a", usage(&home).get("a")), "new, untested");
        record_outcome(&home, "a", false);
        assert!(tag(&home, "a", usage(&home).get("a")).starts_with("failed last time"));
        assert_eq!(failing(&home).len(), 1);
        for _ in 0..3 {
            record_outcome(&home, "a", true);
        }
        assert_eq!(tag(&home, "a", usage(&home).get("a")), "trusted");
        assert!(failing(&home).is_empty());
        assert!(!is_cold(&home, "a", Some(&old)), "file is younger than the cold limit");
        archive(&home, "a").unwrap();
        assert!(is_archived(&home, "a"));
        assert!(crate::tools::skill_index(&home).is_empty());
        assert!(list_text(&home).contains("archived: a"));
        restore(&home, "a").unwrap();
        assert_eq!(crate::tools::skill_index(&home).len(), 1);
        assert!(archive(&home, "missing").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn retires_only_failing_self_made_skills_and_undo_restores() {
        let home = std::env::temp_dir().join(format!("pagurus-retire-{}", crate::util::new_id()));
        let mut cfg = crate::config::Config::default();
        cfg.learning.mode = "auto".into();
        let skill = |n: &str, d: &str| crate::learn::Change::SkillWrite {
            name: n.into(),
            description: d.into(),
            body: "1. step".into(),
        };
        crate::learn::submit(&home, &cfg, skill("flaky", "read the SoC temperature"), "", "s").unwrap();
        crate::learn::submit(&home, &cfg, skill("steady", "rotate the nginx logs"), "", "s").unwrap();
        // hand-written by the user: never retired by pagurus
        write_atomic(&home.join("skills/mine/SKILL.md"), "---\nname: mine\ndescription: d\n---\nbody\n").unwrap();
        for n in ["flaky", "mine"] {
            for _ in 0..RETIRE_STREAK {
                record_outcome(&home, n, false);
            }
        }
        record_outcome(&home, "steady", false);
        assert_eq!(crate::learn::self_made_skills(&home), vec!["flaky".to_string(), "steady".to_string()]);
        let done = auto_retire(&home);
        assert_eq!(done.len(), 1, "{done:?}");
        assert!(done[0].starts_with("flaky"));
        assert!(is_archived(&home, "flaky") && !is_archived(&home, "mine") && !is_archived(&home, "steady"));
        assert!(crate::learn::list_text(&home).contains("retire skill flaky"));
        // a fix resets the streak, so a revised skill isn't retired for its old failures
        for _ in 0..RETIRE_STREAK {
            record_outcome(&home, "steady", false);
        }
        let patch = crate::learn::Change::SkillPatch {
            name: "steady".into(),
            old_text: "1. step".into(),
            text: "1. better step".into(),
        };
        crate::learn::submit(&home, &cfg, patch, "", "s").unwrap();
        assert_eq!(usage(&home)["steady"].streak, 0);
        assert!(auto_retire(&home).is_empty());
        // undo brings the retired skill back
        let id = crate::learn::history_text(&home, 10)
            .lines()
            .find(|l| l.contains("retire skill flaky"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        crate::learn::rollback(&home, &id).unwrap();
        assert!(home.join("skills/flaky/SKILL.md").exists() && !is_archived(&home, "flaky"));
        let _ = std::fs::remove_dir_all(&home);
    }
}

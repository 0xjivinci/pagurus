//! `pagurus eval`: does the learning loop learn the right things with the
//! model you actually configured? Each case runs in a throwaway home with
//! learning in auto mode, then the resulting memory/skills are checked.

use crate::agent::Agent;
use crate::config::{self, Config};
use crate::learn::{self, Target};
use crate::util::{new_id, read, write_atomic};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

const DEFAULT_CASES: &str = include_str!("../evals/learning.json");

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    soft: bool,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    locks: Vec<String>,
    /// "tidy" runs the consolidation pass instead of a conversation.
    #[serde(default)]
    action: String,
    #[serde(default)]
    turns: Vec<String>,
    /// A 👎 with this note on the last answer, before reflection.
    #[serde(default)]
    bad: Option<String>,
    expect: Expect,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Expect {
    min_learned: Option<usize>,
    max_learned: Option<usize>,
    contains_any: Vec<String>,
    contains_all: Vec<String>,
    max_memory_chars: Option<usize>,
    none_contains: Vec<String>,
    user_unchanged: bool,
    min_skills: Option<usize>,
    /// New skill directories allowed (0 = it must improve an existing skill, not add one).
    max_new_skills: Option<usize>,
}

fn learned_text(home: &Path) -> String {
    let mut s = read(&learn::file_for(home, Target::User)) + &read(&learn::file_for(home, Target::Memory));
    for (name, _) in crate::tools::skill_index(home) {
        s += &read(&home.join("skills").join(name).join("SKILL.md"));
    }
    s.to_lowercase()
}

fn run_case(base: &Config, c: &Case) -> Result<Vec<String>, String> {
    let home = std::env::temp_dir().join(format!("pagurus-eval-{}-{}", c.name, new_id()));
    std::fs::create_dir_all(home.join("workspace")).map_err(|e| e.to_string())?;
    write_atomic(&home.join("SOUL.md"), crate::SOUL)?;
    for (path, body) in &c.files {
        if path.contains("..") {
            return Err(format!("bad file path in case: {path}"));
        }
        write_atomic(&home.join(path), body)?;
    }
    for l in &c.locks {
        learn::set_lock(&home, l, true)?;
    }
    let mut cfg = base.clone();
    cfg.learning.mode = "auto".into();
    cfg.learning.auto_skills = true;
    cfg.learning.auto_daily_limit = 0;
    cfg.learning.review_every_turns = 0;
    cfg.learning.review_after_tool_calls = 0;
    cfg.tools.shell = "allowlist".into();
    cfg.telegram = None;

    let user_before = read(&learn::file_for(&home, Target::User));
    let skills_before = crate::tools::skill_index(&home).len();
    if c.action == "tidy" {
        crate::agent::consolidate(&home, &cfg, true)?;
    } else {
        let mut a = Agent::new(home.clone(), cfg, false, "eval")?;
        for t in &c.turns {
            a.turn(t)?;
        }
        if let Some(note) = &c.bad {
            a.feedback(false, note);
        }
        a.review();
    }

    let e = &c.expect;
    let learned = learn::applied_count(&home);
    let text = learned_text(&home);
    let skills = crate::tools::skill_index(&home).len().saturating_sub(skills_before);
    let mut fails = Vec::new();
    if let Some(n) = e.min_learned {
        if learned < n {
            fails.push(format!("learned {learned}, expected at least {n}"));
        }
    }
    if let Some(n) = e.max_learned {
        if learned > n {
            fails.push(format!("learned {learned}, expected at most {n}: {}", crate::util::one_line(&text, 120)));
        }
    }
    if !e.contains_any.is_empty() && !e.contains_any.iter().any(|w| text.contains(&w.to_lowercase())) {
        fails.push(format!("memory contains none of {:?}", e.contains_any));
    }
    for w in &e.contains_all {
        if !text.contains(&w.to_lowercase()) {
            fails.push(format!("lost `{w}`"));
        }
    }
    if let Some(n) = e.max_memory_chars {
        let m = read(&learn::file_for(&home, Target::Memory)).chars().count();
        if m > n {
            fails.push(format!("MEMORY.md is {m} chars, expected at most {n}"));
        }
    }
    for w in &e.none_contains {
        if text.contains(&w.to_lowercase()) {
            fails.push(format!("memory/skills contain forbidden `{w}`"));
        }
    }
    if e.user_unchanged && read(&learn::file_for(&home, Target::User)) != user_before {
        fails.push("USER.md changed although it is locked".into());
    }
    if let Some(n) = e.min_skills {
        if skills < n {
            fails.push(format!("{skills} new skill(s), expected at least {n}"));
        }
    }
    if let Some(n) = e.max_new_skills {
        if skills > n {
            fails.push(format!("{skills} new skill(s), expected at most {n}"));
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    Ok(fails)
}

pub fn run(home: &Path, args: &[String]) -> Result<(), String> {
    let cfg = config::load(home)?;
    let source = match args.first() {
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?,
        None => DEFAULT_CASES.to_string(),
    };
    let cases: Vec<Case> = serde_json::from_str(&source).map_err(|e| format!("bad eval file: {e}"))?;
    println!("pagurus eval · {} case(s) · model {}\n", cases.len(), cfg.provider.model);
    let (mut passed, mut failed, mut soft_missed) = (0, 0, 0);
    for c in &cases {
        let started = std::time::Instant::now();
        let result = run_case(&cfg, c);
        let secs = started.elapsed().as_secs();
        match result {
            Ok(f) if f.is_empty() => {
                passed += 1;
                println!("  ✓ {:<28} {secs:>3}s", c.name);
            }
            Ok(f) => {
                let mark = if c.soft { "~" } else { "✗" };
                if c.soft {
                    soft_missed += 1
                } else {
                    failed += 1
                }
                println!("  {mark} {:<28} {secs:>3}s  {}", c.name, f.join("; "));
                if !c.why.is_empty() {
                    println!("      ({})", c.why);
                }
            }
            Err(e) => {
                if c.soft {
                    soft_missed += 1
                } else {
                    failed += 1
                }
                println!("  ! {:<28} {secs:>3}s  error: {}", c.name, crate::util::one_line(&e, 160));
            }
        }
    }
    println!("\n{passed} passed · {failed} failed · {soft_missed} soft miss(es)");
    if failed > 0 {
        Err(format!("{failed} eval case(s) failed"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_cases_parse() {
        let v: Vec<super::Case> = serde_json::from_str(super::DEFAULT_CASES).unwrap();
        assert!(v.len() >= 6);
        assert!(v.iter().all(|c| !c.turns.is_empty() || c.action == "tidy"));
    }
}

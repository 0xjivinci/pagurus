//! File editing after Hermes' patch tool and checkpoint manager, in small:
//! - `edit_file` replaces one exact snippet; if the model's copy is slightly
//!   off (indentation, spacing, curly quotes) a line-wise fuzzy match finds it,
//!   but only when that match is unique.
//! - Every write_file / edit_file first saves what it overwrites under
//!   `checkpoints/`, so `/rollback` can put a file back (the last `KEEP`).
//! - Tool output too long for the context is spilled to a workspace file the
//!   model can page through with read_file, instead of being cut off.

use crate::util::{now, read, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEEP: usize = 30;
const MAX_SNAPSHOT: u64 = 256 * 1024;

// ---------- edit ----------

/// Replace `old` with `new` in `text`. Returns the new text and a note on how
/// it matched. `all` replaces every exact occurrence.
pub fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<(String, String), String> {
    if old.is_empty() {
        return Err("old_string is empty; to create a file use write_file".into());
    }
    let n = text.matches(old).count();
    if n == 1 || (n > 1 && all) {
        let line = text[..text.find(old).unwrap_or(0)].lines().count().max(1);
        let what = if n == 1 { format!("at line {line}") } else { format!("{n} occurrences") };
        return Ok((text.replace(old, new), what));
    }
    if n > 1 {
        return Err(format!(
            "old_string occurs {n} times; include more surrounding lines so it is unique, or set replace_all"
        ));
    }
    // fuzzy: compare line by line with whitespace collapsed and quotes plain
    let want: Vec<String> = old.trim_matches('\n').lines().map(norm).collect();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if want.is_empty() || want.iter().all(|l| l.is_empty()) {
        return Err("old_string is only whitespace".into());
    }
    let hits: Vec<usize> = (0..lines.len().saturating_sub(want.len() - 1))
        .filter(|&i| (0..want.len()).all(|j| norm(lines[i + j]) == want[j]))
        .collect();
    match hits.as_slice() {
        [i] => {
            let end = i + want.len();
            let mut out: String = lines[..*i].concat();
            out += new.trim_end_matches('\n');
            if !new.is_empty() && lines[end - 1].ends_with('\n') {
                out.push('\n');
            }
            out += &lines[end..].concat();
            Ok((out, format!("at line {} (matched ignoring whitespace/quote differences)", i + 1)))
        }
        [] => Err("old_string not found (not even ignoring whitespace); read_file the area and copy the text exactly".into()),
        many => Err(format!(
            "old_string matches {} places ignoring whitespace (lines {}); include more context",
            many.len(),
            many.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn norm(l: &str) -> String {
    l.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// edit_file on an already-confined path.
pub fn edit(home: &Path, p: &Path, old: &str, new: &str, all: bool) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if bytes.iter().take(1024).any(|b| *b == 0) {
        return Err("binary file".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "not a UTF-8 text file; use write_file")?;
    let (out, how) = replace(&text, old, new, all)?;
    if out.len() > 1024 * 1024 {
        return Err("result larger than 1 MiB".into());
    }
    let id = checkpoint(home, p);
    std::fs::write(p, &out).map_err(|e| e.to_string())?;
    Ok(format!("edited {} {how}{}", p.display(), id.map(|i| format!(" (undo: /rollback {i})")).unwrap_or_default()))
}

// ---------- checkpoints ----------

#[derive(Serialize, Deserialize, Clone)]
struct Snap {
    id: u64,
    t: u64,
    path: String,
    /// false: the file didn't exist, so restoring deletes it.
    existed: bool,
}

fn dir(home: &Path) -> PathBuf {
    home.join("checkpoints")
}

fn index(home: &Path) -> Vec<Snap> {
    read(&dir(home).join("index.jsonl")).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn save_index(home: &Path, v: &[Snap]) {
    let s: String = v.iter().filter_map(|s| serde_json::to_string(s).ok()).map(|l| l + "\n").collect();
    let _ = write_atomic(&dir(home).join("index.jsonl"), &s);
}

/// Save `p` as it is now, before it gets overwritten. Best effort: returns the
/// checkpoint number, or None if the file is too big or unreadable.
pub fn checkpoint(home: &Path, p: &Path) -> Option<u64> {
    let existed = p.exists();
    if existed && std::fs::metadata(p).map(|m| !m.is_file() || m.len() > MAX_SNAPSHOT).unwrap_or(true) {
        return None;
    }
    let mut v = index(home);
    let id = v.last().map(|s| s.id + 1).unwrap_or(1);
    std::fs::create_dir_all(dir(home)).ok()?;
    if existed {
        std::fs::copy(p, dir(home).join(format!("{id}.bak"))).ok()?;
    }
    v.push(Snap { id, t: now(), path: p.display().to_string(), existed });
    while v.len() > KEEP {
        let old = v.remove(0);
        let _ = std::fs::remove_file(dir(home).join(format!("{}.bak", old.id)));
    }
    save_index(home, &v);
    Some(id)
}

/// `/rollback`: list checkpoints; `/rollback <n>`: restore one.
pub fn rollback(home: &Path, arg: &str) -> Result<String, String> {
    let v = index(home);
    let arg = arg.trim();
    if arg.is_empty() || arg == "list" {
        if v.is_empty() {
            return Ok("No file checkpoints yet (each write_file/edit_file saves one).".into());
        }
        let mut s = String::from("File checkpoints (newest first) — /rollback <n> restores one:\n");
        for c in v.iter().rev().take(15) {
            s += &format!(
                "{} · {} · {}{}\n",
                c.id,
                crate::util::ago(c.t),
                c.path,
                if c.existed { "" } else { " (was new: restoring deletes it)" }
            );
        }
        return Ok(s);
    }
    let id: u64 = arg.trim_start_matches('#').parse().map_err(|_| format!("usage: /rollback [n] (got `{arg}`)"))?;
    let c = v.iter().find(|c| c.id == id).ok_or_else(|| format!("no checkpoint {id} (/rollback lists them)"))?;
    let p = Path::new(&c.path);
    let back = checkpoint(home, p); // the restore itself can be undone
    if c.existed {
        let bytes = std::fs::read(dir(home).join(format!("{id}.bak"))).map_err(|e| format!("checkpoint {id}: {e}"))?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(p, bytes).map_err(|e| format!("{}: {e}", p.display()))?;
    } else if p.exists() {
        std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(format!(
        "↩️ {} {} as of checkpoint {id}{}",
        if c.existed { "restored" } else { "removed" },
        c.path,
        back.map(|b| format!(" (undo this with /rollback {b})")).unwrap_or_default()
    ))
}

// ---------- big outputs ----------

/// Above this a tool result is spilled to a file (the agent clips at 12k).
pub const SPILL_AT: usize = 12_000;
const SPILL_KEEP: usize = 20;

/// Long tool output: keep the head and tail in the reply, put the whole text in
/// `workspace/.tool-output/` for read_file paging.
pub fn spill(home: &Path, tool: &str, out: String) -> String {
    if out.len() <= SPILL_AT {
        return out;
    }
    let d = home.join("workspace").join(".tool-output");
    let p = d.join(format!("{}-{tool}.txt", crate::util::new_id()));
    let saved = std::fs::create_dir_all(&d).is_ok() && std::fs::write(&p, &out).is_ok();
    prune(&d);
    let lines = out.lines().count();
    let head = crate::util::clip(&out, 9500); // with the tail: as much as tools showed before spilling existed
    let tail = crate::util::tail(&out, 2500);
    let note = if saved {
        format!(
            "[output was {} chars / {lines} lines; the middle is cut here. Full text: {} — read_file it with offset/limit \
             to see the rest]",
            out.len(),
            p.display()
        )
    } else {
        format!("[output was {} chars; the middle is cut]", out.len())
    };
    format!("{head}\n…\n{note}\n…\n{tail}")
}

fn prune(d: &Path) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut v: Vec<(std::time::SystemTime, PathBuf)> =
        rd.filter_map(|e| e.ok()).filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if v.len() <= SPILL_KEEP {
        return;
    }
    v.sort();
    for (_, p) in &v[..v.len() - SPILL_KEEP] {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let h = std::env::temp_dir().join(format!("pagurus-files-{}", crate::util::new_id()));
        std::fs::create_dir_all(h.join("workspace")).unwrap();
        h
    }

    #[test]
    fn exact_then_fuzzy_replace() {
        let t = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        let (o, how) = replace(t, "let x = 1;", "let x = 10;", false).unwrap();
        assert_eq!(o, "fn a() {\n    let x = 10;\n    let y = 2;\n}\n");
        assert!(how.contains("line 2"), "{how}");
        // the model dropped the indentation and doubled a space: still unique
        let (o, how) = replace(t, "let x =  1;\nlet y = 2;", "    let z = 3;", false).unwrap();
        assert_eq!(o, "fn a() {\n    let z = 3;\n}\n");
        assert!(how.contains("ignoring whitespace"));
        // curly quotes from a chat UI
        let (o, _) = replace("say 'hi'\n", "say ‘hi’", "say 'yo'", false).unwrap();
        assert_eq!(o, "say 'yo'\n");
        assert!(replace("a\na\n", "a", "b", false).unwrap_err().contains("2 times"));
        assert_eq!(replace("a\na\n", "a", "b", true).unwrap().0, "b\nb\n");
        assert!(replace(" x\n x\n", "x ", "y", false).unwrap_err().contains("2 places"));
        assert!(replace("abc", "zzz", "y", false).unwrap_err().contains("not found"));
        assert!(replace("abc", "", "y", false).is_err());
    }

    #[test]
    fn checkpoints_restore_and_undo_restores() {
        let h = home();
        let f = h.join("workspace/notes.txt");
        let new_file = h.join("workspace/new.txt");
        std::fs::write(&f, "one\n").unwrap();
        let r = edit(&h, &f, "one", "two", false).unwrap();
        assert!(r.contains("/rollback 1"), "{r}");
        assert_eq!(read(&f), "two\n");
        assert_eq!(checkpoint(&h, &new_file), Some(2));
        std::fs::write(&new_file, "x").unwrap();
        assert!(rollback(&h, "").unwrap().contains("was new"));
        assert!(rollback(&h, "1").unwrap().contains("restored"));
        assert_eq!(read(&f), "one\n");
        assert!(rollback(&h, "2").unwrap().contains("removed"));
        assert!(!new_file.exists());
        // restoring made checkpoint 3 (of "two"): undo the undo
        rollback(&h, "3").unwrap();
        assert_eq!(read(&f), "two\n");
        assert!(rollback(&h, "99").is_err());
        for _ in 0..40 {
            checkpoint(&h, &f);
        }
        assert_eq!(index(&h).len(), KEEP);
        let baks = std::fs::read_dir(dir(&h)).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "bak")).count();
        assert!(baks <= KEEP, "old snapshots are pruned ({baks})");
    }

    #[test]
    fn long_output_spills_to_a_readable_file() {
        let h = home();
        assert_eq!(spill(&h, "shell", "short".into()), "short");
        let big: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let out = spill(&h, "shell", big.clone());
        assert!(out.len() < 12_500 && out.contains("line 0") && out.contains("line 2999"));
        let path = out.split("Full text: ").nth(1).unwrap().split(" — ").next().unwrap();
        assert_eq!(read(Path::new(path)), big);
        assert!(path.contains("/workspace/"), "readable with read_file");
        for _ in 0..25 {
            spill(&h, "x", big.clone());
        }
        assert!(std::fs::read_dir(h.join("workspace/.tool-output")).unwrap().count() <= SPILL_KEEP);
    }

    #[test]
    fn tools_wire_up_edit_spill_clarify_and_process() {
        use crate::tools::{definitions, execute, Ctx, Set};
        let h = home();
        let cfg = crate::config::Config::default();
        let names: Vec<String> =
            definitions(&cfg, Set::Full).as_array().unwrap().iter().map(|d| d["function"]["name"].as_str().unwrap().into()).collect();
        for t in ["edit_file", "clarify", "process"] {
            assert!(names.contains(&t.to_string()), "{t} offered");
        }
        let ctx = Ctx { home: &h, cfg: &cfg, session: "t", interactive: false, approver: None };
        execute(&ctx, Set::Full, "write_file", r#"{"path":"a.txt","content":"hello world\n"}"#);
        let r = execute(&ctx, Set::Full, "edit_file", r#"{"path":"a.txt","old_string":"world","new_string":"pi"}"#);
        assert!(r.starts_with("edited") && r.contains("/rollback 2"), "{r}");
        assert_eq!(read(&h.join("workspace/a.txt")), "hello pi\n");
        assert!(execute(&ctx, Set::Full, "edit_file", r#"{"path":"/etc/hosts","old_string":"a","new_string":"b"}"#).starts_with("error"));
        let r = execute(&ctx, Set::Full, "process", r#"{"action":"start","command":"sleep 5"}"#);
        assert!(r.contains("nobody can approve"), "unattended: never started ({r})");
        assert!(execute(&ctx, Set::Sub, "process", r#"{"action":"list"}"#).contains("not available"));
        assert!(execute(&ctx, Set::Full, "clarify", r#"{"question":"which?"}"#).contains("unattended"));
        let no = |_: &str| false;
        let ctx = Ctx { approver: Some(&no), ..ctx };
        assert!(execute(&ctx, Set::Full, "process", r#"{"action":"start","command":"sleep 5"}"#).contains("did not approve"));
        // a long listing is spilled, not cut off
        let big: String = (0..2000).map(|i| format!("row {i}\n")).collect();
        std::fs::write(h.join("workspace/big.txt"), &big).unwrap();
        let r = execute(&ctx, Set::Full, "shell", r#"{"command":"cat big.txt"}"#);
        assert!(r.contains("Full text: ") && r.contains("row 1999"), "{}", crate::util::clip(&r, 300));
    }
}

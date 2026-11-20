//! What the terminal chat shows, in one place: the rich version (feature `tui`,
//! see tui.rs) or the plain one. Only the interactive terminal uses the rich
//! parts; the daemon, `pagurus run` and sub-agents keep plain output.

#[cfg(all(feature = "tui", unix))]
use crate::tui;

#[cfg(all(feature = "tui", unix))]
static MD: std::sync::Mutex<Option<tui::Md>> = std::sync::Mutex::new(None);
#[cfg(all(feature = "tui", unix))]
static LAST_STREAMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Newlines in a row at the end of what streamed (to keep blank lines single).
#[cfg(all(feature = "tui", unix))]
static NEWLINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
#[cfg(all(feature = "tui", unix))]
static RICH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The rich chat is on (switched on by the terminal session).
#[cfg_attr(not(all(feature = "tui", unix)), allow(dead_code))]
pub fn rich() -> bool {
    #[cfg(all(feature = "tui", unix))]
    {
        RICH.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(all(feature = "tui", unix)))]
    {
        false
    }
}

/// Start the rich chat for this terminal session, if the build and terminal allow it.
/// Answers from this thread's chat model stream to the screen from now on.
pub fn start() -> bool {
    #[cfg(all(feature = "tui", unix))]
    {
        if !tui::enabled() {
            return false;
        }
        RICH.store(true, std::sync::atomic::Ordering::Relaxed);
        tui::catch_ctrl_c();
        crate::llm::set_stream(Some(Box::new(|t: &str| {
            // models often open with blank lines: show nothing until there are words
            let mut md = match MD.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            let t = if md.is_none() { t.trim_start() } else { t };
            if t.is_empty() {
                return;
            }
            tui::spinner_stop();
            let mut out = md.get_or_insert_with(Default::default).push(t);
            drop(md);
            // at most one blank line in a row
            let mut kept = String::with_capacity(out.len());
            for c in out.chars() {
                if c == '\n' {
                    let n = NEWLINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n >= 2 {
                        continue;
                    }
                } else if !c.is_whitespace() && c != '\x1b' {
                    NEWLINES.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                kept.push(c);
            }
            out = kept;
            print!("{out}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        })));
        true
    }
    #[cfg(not(all(feature = "tui", unix)))]
    {
        false
    }
}

/// Before a call to the chat model: a spinner while it thinks.
pub fn thinking() {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        tui::spinner_start("thinking");
    }
}

/// After a call to the chat model: was its text already shown while streaming?
pub fn reply_done() -> bool {
    #[cfg(all(feature = "tui", unix))]
    {
        if !rich() {
            return false;
        }
        tui::spinner_stop();
        let _ = crate::llm::take_streamed();
        // shown = some words reached the screen (not just blank lines)
        let shown = MD.lock().ok().and_then(|mut m| m.take());
        let streamed = shown.is_some();
        if let Some(mut m) = shown {
            let tail = m.finish();
            let tail = tail.trim_end_matches('\n');
            if !tail.is_empty() || NEWLINES.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                println!("{tail}");
            }
        }
        NEWLINES.store(1, std::sync::atomic::Ordering::Relaxed); // the next reply starts on a fresh line
        LAST_STREAMED.store(streamed, std::sync::atomic::Ordering::Relaxed);
        streamed
    }
    #[cfg(not(all(feature = "tui", unix)))]
    {
        false
    }
}

/// Show the final answer of a turn, unless it already streamed.
pub fn answer(text: &str) {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        if !LAST_STREAMED.swap(false, std::sync::atomic::Ordering::Relaxed) && !text.trim().is_empty() {
            println!("{}", tui::render(text.trim()));
        }
        return;
    }
    println!("{}", text.trim());
}

pub fn tool_start(name: &str, args: &str, parallel: bool) {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        return tui::tool_start(name, args, parallel);
    }
    let width = if parallel { 80 } else { 90 };
    eprintln!("  · {name} {}{}", crate::util::one_line(args, width), if parallel { " (in parallel)" } else { "" });
}

pub fn tool_done(out: &str, secs: f64) {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        tui::tool_done(out, secs);
    }
    let _ = (out, secs);
}

/// Dimmed text (plain without the rich chat).
pub fn dim(s: &str) -> String {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        return tui::style("2", s);
    }
    s.to_string()
}

/// A new message from the user is being answered.
pub fn turn_started() {
    #[cfg(all(feature = "tui", unix))]
    tui::turn_started();
}

/// The line under an answer: model, tokens, time, context, yolo.
pub fn status(parts: &[String]) {
    #[cfg(all(feature = "tui", unix))]
    if rich() {
        tui::status(parts);
    }
    let _ = parts;
}

/// Read the next message (None at end of input). The rich version has history,
/// completion of / commands and multi-line input.
pub struct Reader {
    #[cfg(all(feature = "tui", unix))]
    ed: Option<tui::Editor>,
}

impl Reader {
    pub fn new(home: &std::path::Path, commands: Vec<&'static str>) -> Reader {
        #[cfg(all(feature = "tui", unix))]
        {
            Reader { ed: rich().then(|| tui::Editor::new(home, commands)) }
        }
        #[cfg(not(all(feature = "tui", unix)))]
        {
            let _ = (home, commands);
            Reader {}
        }
    }

    pub fn read(&mut self) -> Option<String> {
        let yolo = crate::autonomy::yolo_left().map(|l| format!("⚡{} ", crate::autonomy::fmt_left(l))).unwrap_or_default();
        #[cfg(all(feature = "tui", unix))]
        if let Some(ed) = self.ed.as_mut() {
            let w = yolo.chars().count() + 2;
            let prompt = format!("{}{}", tui::style("33", &yolo), tui::style("1", "› "));
            return match ed.read(&prompt, w) {
                tui::Input::Line(l) => Some(l),
                tui::Input::Eof => None,
            };
        }
        print!("{yolo}› ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }
}

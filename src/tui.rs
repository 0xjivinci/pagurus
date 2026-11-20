//! The rich terminal chat (feature `tui`, Linux and macOS): a line editor with
//! history, completion and multi-line input; answers rendered as they stream;
//! a spinner while the model thinks; tool calls with their outcome and time;
//! a status line; Ctrl-C stops the running answer instead of killing pagurus.
//! No terminal library: raw mode and window size come from libc (already linked).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------- terminal ----------

/// Use the rich chat: both ends are a terminal, and TERM isn't "dumb".
pub fn enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
        && std::env::var_os("PAGURUS_PLAIN").is_none()
}

fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Wrap `s` in an SGR style (`1` bold, `2` dim, `36` cyan, `31` red …) unless NO_COLOR.
pub fn style(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 10 { ws.ws_col as usize } else { 80 }
}

/// Raw input mode for the line editor; the old mode comes back on drop.
struct Raw(libc::termios);

impl Raw {
    fn on() -> Option<Raw> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return None;
        }
        let old = t;
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        t.c_iflag &= !(libc::IXON | libc::ICRNL);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        print!("\x1b[?2004h"); // bracketed paste: pasted newlines stay in the message
        Some(Raw(old))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        print!("\x1b[?2004l");
        let _ = std::io::stdout().flush();
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

static INTERRUPTS: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_sigint(_: libc::c_int) {
    // a second Ctrl-C while the first is still stopping the answer: really quit
    if INTERRUPTS.fetch_add(1, Ordering::SeqCst) >= 1 {
        unsafe { libc::_exit(130) };
    }
    crate::util::CANCEL.store(true, Ordering::SeqCst);
}

/// Ctrl-C during an answer stops it (after the current step) instead of quitting.
pub fn catch_ctrl_c() {
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
}

/// A new turn: Ctrl-C counts from zero again.
pub fn turn_started() {
    INTERRUPTS.store(0, Ordering::SeqCst);
}

// ---------- the line editor ----------

pub struct Editor {
    history: Vec<String>,
    file: PathBuf,
    commands: Vec<&'static str>,
}

/// Width of a character on screen (wide CJK and emoji take two columns).
fn cw(c: char) -> usize {
    let u = c as u32;
    if u < 0x1100 {
        1
    } else if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
    {
        2
    } else {
        1
    }
}

/// Screen row and column after writing `text` from column `col`, wrapping at `w`.
fn advance(text: &str, mut row: usize, mut col: usize, w: usize, cont: usize) -> (usize, usize) {
    for c in text.chars() {
        if c == '\n' {
            row += 1;
            col = cont;
            continue;
        }
        let x = cw(c);
        if col + x > w {
            row += 1;
            col = 0;
        }
        col += x;
    }
    (row, col)
}

pub enum Input {
    Line(String),
    Eof,
}

impl Editor {
    pub fn new(home: &Path, commands: Vec<&'static str>) -> Editor {
        let file = home.join("history");
        let history = crate::util::read(&file)
            .lines()
            .filter_map(|l| serde_json::from_str::<String>(l).ok())
            .collect();
        Editor { history, file, commands }
    }

    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().is_some_and(|l| l == line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > 1000 {
            self.history.drain(..self.history.len() - 1000);
        }
        let body: String = self.history.iter().map(|l| format!("{}\n", serde_json::Value::from(l.as_str()))).collect();
        let _ = crate::util::write_private(&self.file, &body);
    }

    /// Read one message. `prompt` may carry styles; `prompt_w` is its visible width.
    pub fn read(&mut self, prompt: &str, prompt_w: usize) -> Input {
        let Some(_raw) = Raw::on() else { return plain_read(prompt) };
        let mut buf: Vec<char> = Vec::new();
        let mut pos = 0usize;
        let mut drawn_row = 0usize; // the cursor's row within what was drawn
        let mut hist_i = self.history.len();
        let mut hist_prefix: Option<String> = None;
        let mut stdin = std::io::stdin().lock();
        let mut ctrl_c = false;
        let mut paste = false;
        let cont = 2;
        let draw = |buf: &[char], pos: usize, drawn_row: &mut usize| {
            let w = width();
            let text: String = buf.iter().collect();
            let shown = text.replace('\n', "\n  ");
            let mut out = String::new();
            if *drawn_row > 0 {
                out += &format!("\x1b[{}A", *drawn_row);
            }
            out += "\r\x1b[J";
            out += prompt;
            out += &shown.replace('\n', "\r\n");
            let (end_r, end_c) = advance(&text, 0, prompt_w, w, cont);
            let before: String = buf[..pos].iter().collect();
            let (cur_r, cur_c) = advance(&before, 0, prompt_w, w, cont);
            // a line that ends exactly at the right edge: put the cursor on the next row
            if end_c >= w && end_r == cur_r && end_c == cur_c {
                out += "\r\n";
            }
            let end_r = if end_c >= w { end_r + 1 } else { end_r };
            if end_r > cur_r {
                out += &format!("\x1b[{}A", end_r - cur_r);
            }
            out += &format!("\r\x1b[{}C", cur_c.min(w - 1)).replace("\x1b[0C", "");
            *drawn_row = cur_r;
            print!("{out}");
            let _ = std::io::stdout().flush();
        };
        draw(&buf, pos, &mut drawn_row);
        loop {
            let Some(c) = read_char(&mut stdin) else { return Input::Eof };
            if paste {
                match c {
                    '\x1b' => {
                        let seq = read_seq(&mut stdin);
                        if seq == "[201~" {
                            paste = false;
                        }
                    }
                    '\r' => {
                        buf.insert(pos, '\n');
                        pos += 1;
                    }
                    c if !c.is_control() || c == '\n' || c == '\t' => {
                        buf.insert(pos, if c == '\t' { ' ' } else { c });
                        pos += 1;
                    }
                    _ => {}
                }
                if !paste {
                    draw(&buf, pos, &mut drawn_row);
                }
                continue;
            }
            if c != '\x03' {
                ctrl_c = false;
            }
            match c {
                '\r' => {
                    if pos > 0 && buf[pos - 1] == '\\' && pos == buf.len() {
                        buf[pos - 1] = '\n'; // trailing backslash: keep typing on the next line
                    } else {
                        pos = buf.len();
                        draw(&buf, pos, &mut drawn_row);
                        print!("\r\n");
                        let line: String = buf.iter().collect();
                        self.remember(&line);
                        return Input::Line(line);
                    }
                }
                '\n' => {
                    buf.insert(pos, '\n'); // Ctrl-J
                    pos += 1;
                }
                '\x03' => {
                    if buf.is_empty() && ctrl_c {
                        print!("\r\n");
                        return Input::Eof;
                    }
                    if buf.is_empty() {
                        print!("\r\n{}", style("2", "(Ctrl-C again or /exit to quit)\r\n"));
                        drawn_row = 0;
                        ctrl_c = true;
                    }
                    buf.clear();
                    pos = 0;
                }
                '\x04' if buf.is_empty() => {
                    print!("\r\n");
                    return Input::Eof;
                }
                '\x04' if pos < buf.len() => {
                    buf.remove(pos);
                }
                '\x7f' | '\x08' if pos > 0 => {
                    pos -= 1;
                    buf.remove(pos);
                }
                '\x01' => pos = 0,
                '\x05' => pos = buf.len(),
                '\x02' => pos = pos.saturating_sub(1),
                '\x06' => pos = (pos + 1).min(buf.len()),
                '\x0b' => buf.truncate(pos),
                '\x15' => {
                    buf.drain(..pos);
                    pos = 0;
                }
                '\x17' => {
                    let start = word_start(&buf, pos);
                    buf.drain(start..pos);
                    pos = start;
                }
                '\x0c' => {
                    print!("\x1b[2J\x1b[H");
                    drawn_row = 0;
                }
                '\t' => {
                    let text: String = buf.iter().collect();
                    if text.starts_with('/') && !text.contains(' ') {
                        let hits: Vec<&&str> = self.commands.iter().filter(|c| c.starts_with(text.as_str())).collect();
                        match hits.len() {
                            0 => {}
                            1 => {
                                buf = format!("{} ", hits[0]).chars().collect();
                                pos = buf.len();
                            }
                            _ => {
                                let common = common_prefix(&hits);
                                if common.len() > text.len() {
                                    buf = common.chars().collect();
                                    pos = buf.len();
                                } else {
                                    let list: Vec<&str> = hits.iter().map(|h| **h).collect();
                                    print!("\r\n{}\r\n", style("2", &list.join("  ")));
                                    drawn_row = 0;
                                }
                            }
                        }
                    }
                }
                '\x1b' => {
                    let seq = read_seq(&mut stdin);
                    match seq.as_str() {
                        "[D" => pos = pos.saturating_sub(1),
                        "[C" => pos = (pos + 1).min(buf.len()),
                        "[H" | "[1~" | "OH" => pos = 0,
                        "[F" | "[4~" | "OF" => pos = buf.len(),
                        "[3~" if pos < buf.len() => {
                            buf.remove(pos);
                        }
                        "b" | "[1;3D" | "[1;5D" => pos = word_start(&buf, pos),
                        "f" | "[1;3C" | "[1;5C" => pos = word_end(&buf, pos),
                        "\r" => {
                            buf.insert(pos, '\n'); // Alt-Enter
                            pos += 1;
                        }
                        "[200~" => paste = true,
                        "[A" | "[B" => {
                            // up/down: history, matching what was typed before browsing
                            let prefix = hist_prefix.get_or_insert_with(|| buf.iter().collect()).clone();
                            let up = seq == "[A";
                            let found = if up {
                                (0..hist_i).rev().find(|&i| self.history[i].starts_with(&prefix))
                            } else {
                                (hist_i + 1..self.history.len()).find(|&i| self.history[i].starts_with(&prefix))
                            };
                            match found {
                                Some(i) => {
                                    hist_i = i;
                                    buf = self.history[i].chars().collect();
                                }
                                None if !up => {
                                    hist_i = self.history.len();
                                    buf = prefix.chars().collect();
                                }
                                None => {}
                            }
                            pos = buf.len();
                        }
                        _ => {}
                    }
                }
                c if !c.is_control() => {
                    buf.insert(pos, c);
                    pos += 1;
                    hist_prefix = None;
                    hist_i = self.history.len();
                }
                _ => {}
            }
            draw(&buf, pos, &mut drawn_row);
        }
    }
}

fn plain_read(prompt: &str) -> Input {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => Input::Eof,
        Ok(_) => Input::Line(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn read_char(r: &mut impl Read) -> Option<char> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b[..1]).ok()?;
    let n = match b[0] {
        x if x < 0x80 => 1,
        x if x >> 5 == 0b110 => 2,
        x if x >> 4 == 0b1110 => 3,
        x if x >> 3 == 0b11110 => 4,
        _ => return Some('\u{fffd}'),
    };
    if n > 1 {
        r.read_exact(&mut b[1..n]).ok()?;
    }
    Some(std::str::from_utf8(&b[..n]).ok().and_then(|s| s.chars().next()).unwrap_or('\u{fffd}'))
}

/// The rest of an escape sequence after ESC: `[A`, `[1;5C`, `[200~`, `OH`, or one key (Alt-x).
fn read_seq(r: &mut impl Read) -> String {
    let Some(first) = read_char(r) else { return String::new() };
    let mut s = String::from(first);
    if first == '[' || first == 'O' {
        while let Some(c) = read_char(r) {
            s.push(c);
            if c.is_ascii_alphabetic() || c == '~' || s.len() > 8 {
                break;
            }
        }
    }
    s
}

fn word_start(b: &[char], mut p: usize) -> usize {
    while p > 0 && b[p - 1].is_whitespace() {
        p -= 1;
    }
    while p > 0 && !b[p - 1].is_whitespace() {
        p -= 1;
    }
    p
}

fn word_end(b: &[char], mut p: usize) -> usize {
    while p < b.len() && b[p].is_whitespace() {
        p += 1;
    }
    while p < b.len() && !b[p].is_whitespace() {
        p += 1;
    }
    p
}

fn common_prefix(words: &[&&str]) -> String {
    let first = words[0];
    let mut n = first.len();
    for w in words {
        n = n.min(first.bytes().zip(w.bytes()).take_while(|(a, b)| a == b).count());
    }
    first[..n].to_string()
}

// ---------- the spinner ----------

struct Spin {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

static SPINNER: Mutex<Option<Spin>> = Mutex::new(None);

/// "⠋ thinking… 3s" on stderr until `spinner_stop` (or the first streamed word).
pub fn spinner_start(label: &str) {
    spinner_stop();
    let stop = Arc::new(AtomicBool::new(false));
    let (s, label) = (stop.clone(), label.to_string());
    let handle = std::thread::spawn(move || {
        const F: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let t0 = Instant::now();
        let mut i = 0;
        while !s.load(Ordering::Relaxed) {
            let secs = t0.elapsed().as_secs();
            let tail = if secs >= 2 { format!(" {secs}s") } else { String::new() };
            eprint!("\r\x1b[2K{}", style("2", &format!("{} {label}…{tail}", F[i % F.len()])));
            let _ = std::io::stderr().flush();
            i += 1;
            std::thread::sleep(Duration::from_millis(100));
        }
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });
    if let Ok(mut g) = SPINNER.lock() {
        *g = Some(Spin { stop, handle });
    }
}

pub fn spinner_stop() {
    let spin = SPINNER.lock().ok().and_then(|mut g| g.take());
    if let Some(s) = spin {
        s.stop.store(true, Ordering::Relaxed);
        let _ = s.handle.join();
    }
}

// ---------- rendering answers as they stream ----------

/// Light markdown for the terminal, fed piece by piece: headings and **bold** in
/// bold, `code` and code blocks in cyan, bullets as •, everything else as is.
#[derive(Default)]
pub struct Md {
    code_block: bool,
    bold: bool,
    icode: bool,
    /// The start of the current line, held until its kind is known.
    head: String,
    started: bool,
    /// The current line is a heading.
    heading: bool,
    /// A `*` or a backtick that may be the start of `**` or ``` ``` ```.
    hold: String,
}

impl Md {
    pub fn push(&mut self, t: &str) -> String {
        let mut out = String::new();
        for c in t.chars() {
            self.char(c, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started && !self.head.is_empty() {
            let h = std::mem::take(&mut self.head);
            self.line_start(&h, &mut out);
        }
        out += &std::mem::take(&mut self.hold);
        if color() && (self.bold || self.icode || self.code_block || self.heading) {
            out += "\x1b[0m";
        }
        *self = Md::default();
        out
    }

    fn char(&mut self, c: char, out: &mut String) {
        if c == '\n' {
            if !self.started {
                let h = std::mem::take(&mut self.head);
                self.line_start(&h, out);
            }
            out.push_str(&std::mem::take(&mut self.hold));
            if self.heading && color() {
                out.push_str("\x1b[0m");
            }
            self.heading = false;
            self.started = false;
            out.push('\n');
            return;
        }
        if !self.started {
            self.head.push(c);
            let trimmed = self.head.trim_start();
            // enough of the line to know what it is
            if trimmed.len() >= 4 || (!trimmed.is_empty() && !"#-*`>0123456789".contains(trimmed.chars().next().unwrap_or(' '))) {
                let h = std::mem::take(&mut self.head);
                self.line_start(&h, out);
            }
            return;
        }
        self.inline(c, out);
    }

    fn line_start(&mut self, h: &str, out: &mut String) {
        self.started = true;
        let t = h.trim_start();
        let indent = &h[..h.len() - t.len()];
        if t.starts_with("```") {
            self.code_block = !self.code_block;
            out.push_str(&style("2", h));
            return;
        }
        if self.code_block {
            out.push_str(&if color() { format!("\x1b[36m{h}\x1b[0m") } else { h.to_string() });
            self.hold.clear();
            return;
        }
        if t.starts_with('#') {
            self.heading = true;
            if color() {
                out.push_str("\x1b[1m");
            }
            out.push_str(t.trim_start_matches('#').trim_start());
            return;
        }
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            out.push_str(indent);
            out.push_str("• ");
            for c in rest.chars() {
                self.inline(c, out);
            }
            return;
        }
        out.push_str(indent);
        for c in t.chars() {
            self.inline(c, out);
        }
    }

    fn inline(&mut self, c: char, out: &mut String) {
        if self.code_block {
            out.push_str(&if color() { format!("\x1b[36m{c}\x1b[0m") } else { c.to_string() });
            return;
        }
        if c == '*' && !self.icode {
            if self.hold == "*" {
                self.hold.clear();
                self.bold = !self.bold;
                if color() {
                    out.push_str(if self.bold { "\x1b[1m" } else { "\x1b[22m" });
                }
            } else {
                out.push_str(&std::mem::take(&mut self.hold));
                self.hold.push('*');
            }
            return;
        }
        out.push_str(&std::mem::take(&mut self.hold));
        if c == '`' {
            self.icode = !self.icode;
            if color() {
                out.push_str(if self.icode { "\x1b[36m" } else { "\x1b[39m" });
            }
            return;
        }
        out.push(c);
    }
}

/// Render a whole answer at once (when it didn't stream).
pub fn render(text: &str) -> String {
    let mut m = Md::default();
    let mut s = m.push(text);
    s += &m.finish();
    s
}

// ---------- tools and status ----------

pub fn tool_start(name: &str, args: &str, parallel: bool) {
    spinner_stop();
    let shown = tool_summary(name, args);
    let w = width().saturating_sub(6);
    let line = crate::util::one_line(&shown, w);
    eprintln!("{} {}{}", style("36", "⏺"), style("1", name), if line.is_empty() { String::new() } else { format!(" {}", style("2", &line)) });
    let _ = parallel;
}

/// The argument people care about: the command, the path, the query.
fn tool_summary(name: &str, args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    if v.as_object().is_some_and(|o| o.is_empty()) {
        return String::new();
    }
    for k in ["command", "path", "pattern", "query", "url", "task", "name", "action", "what", "question"] {
        if let Some(s) = v[k].as_str() {
            let extra = if name == "shell" { v["host"].as_str().map(|h| format!("  (on {h})")).unwrap_or_default() } else { String::new() };
            return format!("{s}{extra}");
        }
    }
    args.to_string()
}

pub fn tool_done(out: &str, secs: f64) {
    let failed = out.starts_with("error") || out.contains("blocked by pagurus's safety floor") || out.starts_with("not run");
    let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let n = out.lines().count();
    let summary = if failed {
        style("31", &crate::util::one_line(first, width().saturating_sub(16)))
    } else if n <= 1 {
        style("2", &crate::util::one_line(first, width().saturating_sub(16)))
    } else {
        style("2", &format!("{n} lines"))
    };
    let t = if secs >= 0.1 { style("2", &format!(" · {secs:.1}s")) } else { String::new() };
    eprintln!("  {} {summary}{t}", style("2", "⎿"));
}

pub fn status(parts: &[String]) {
    let line = parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
    eprintln!("{}", style("2", &format!("  {line}")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_streams_the_same_in_any_pieces() {
        let text = "# Title\nSome **bold** and `code`.\n- one\n- two\n```\nlet x = 1;\n```\nend *not bold*\n";
        let whole = render(text);
        for size in [1, 2, 3, 5, 7] {
            let mut m = Md::default();
            let chars: Vec<char> = text.chars().collect();
            let mut s = String::new();
            for chunk in chars.chunks(size) {
                s += &m.push(&chunk.iter().collect::<String>());
            }
            s += &m.finish();
            assert_eq!(s, whole, "chunks of {size}");
        }
        let plain: String = {
            let mut out = String::new();
            let mut esc = false;
            for c in whole.chars() {
                match (esc, c) {
                    (false, '\x1b') => esc = true,
                    (true, 'm') => esc = false,
                    (true, _) => {}
                    _ => out.push(c),
                }
            }
            out
        };
        assert!(plain.contains("Title\nSome bold and code.\n• one\n• two\n"), "{plain}");
        assert!(plain.contains("let x = 1;") && plain.contains("end *not bold*"), "{plain}");
    }

    #[test]
    fn cursor_math_wraps_and_counts_wide_characters() {
        assert_eq!(advance("abc", 0, 2, 80, 2), (0, 5));
        assert_eq!(advance("abcdef", 0, 2, 5, 2), (1, 3));
        assert_eq!(advance("ab\ncd", 0, 2, 80, 2), (1, 4));
        assert_eq!(advance("日本", 0, 0, 80, 2), (0, 4));
        assert_eq!(common_prefix(&[&"/learn", &"/lock"]), "/l");
        let b: Vec<char> = "hello big world".chars().collect();
        assert_eq!(word_start(&b, 9), 6);
        assert_eq!(word_end(&b, 6), 9);
    }

    #[test]
    fn tool_lines_show_what_matters() {
        assert_eq!(tool_summary("shell", r#"{"command":"df -h","host":"pi"}"#), "df -h  (on pi)");
        assert_eq!(tool_summary("read_file", r#"{"path":"/etc/hosts"}"#), "/etc/hosts");
    }
}

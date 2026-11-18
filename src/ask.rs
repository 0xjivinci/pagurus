//! `clarify`, after Hermes' clarify tool: the agent asks the user one short
//! question, optionally with choices, and waits for the answer. In the
//! terminal it is a prompt; on Telegram the choices are a reply keyboard and
//! the user's next message is the answer. Unattended runs can't ask.

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

/// How long a Telegram question waits for an answer.
pub const WAIT_SECS: u64 = 600;

/// Asks the user and returns their answer (None: no answer / cancelled).
pub type Asker = Box<dyn Fn(&str, &[String]) -> Option<String>>;

thread_local! {
    /// Set by the daemon's worker for the duration of one chat turn.
    static ASKER: RefCell<Option<Asker>> = const { RefCell::new(None) };
}

pub fn set_asker(a: Option<Asker>) {
    ASKER.with(|x| *x.borrow_mut() = a);
}

/// The clarify tool. `interactive`: a person is at this terminal.
pub fn clarify(interactive: bool, question: &str, options: &[String]) -> Result<String, String> {
    let q = question.trim();
    if q.is_empty() {
        return Err("question is empty".into());
    }
    let options: Vec<String> = options.iter().map(|o| crate::util::one_line(o, 60)).filter(|o| !o.is_empty()).take(6).collect();
    let answer = match ASKER.with(|x| x.borrow().as_ref().map(|ask| ask(q, &options))) {
        Some(a) => a,
        None if interactive => terminal(q, &options),
        None => return Err("nobody can answer right now (unattended run); decide sensibly yourself and say what you assumed".into()),
    };
    match answer {
        Some(a) if !a.trim().is_empty() => Ok(format!("the user answered: {}", a.trim())),
        _ => Err("no answer from the user; continue with a sensible default and say what you assumed".into()),
    }
}

/// A numbered pick resolves to that option's text.
pub fn pick(answer: &str, options: &[String]) -> String {
    let a = answer.trim();
    match a.parse::<usize>() {
        Ok(n) if n >= 1 && n <= options.len() => options[n - 1].clone(),
        _ => a.to_string(),
    }
}

fn terminal(q: &str, options: &[String]) -> Option<String> {
    use std::io::Write;
    eprintln!("\n  ? {q}");
    for (i, o) in options.iter().enumerate() {
        eprintln!("    {}. {o}", i + 1);
    }
    eprint!("  › ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(pick(&line, options))
}

// ---------- the daemon's side: one open question per chat ----------

static OPEN: Mutex<Vec<(i64, Sender<String>)>> = Mutex::new(Vec::new());

/// Register a question for `chat`; the answer arrives on the receiver.
pub fn open(chat: i64) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut v) = OPEN.lock() {
        v.retain(|(c, _)| *c != chat);
        v.push((chat, tx));
    }
    rx
}

/// A message from `chat`: if a question is waiting, it is the answer. True if consumed.
pub fn answer(chat: i64, text: &str) -> bool {
    let Ok(mut v) = OPEN.lock() else { return false };
    let Some(i) = v.iter().position(|(c, _)| *c == chat) else { return false };
    let (_, tx) = v.remove(i);
    tx.send(text.to_string()).is_ok()
}

/// Wait for the answer, giving up on timeout or /stop.
pub fn wait(chat: i64, rx: Receiver<String>) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(WAIT_SECS);
    let got = loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(a) => break Some(a),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                if std::time::Instant::now() < deadline && !crate::util::cancelled() => {}
            Err(_) => break None,
        }
    };
    if let Ok(mut v) = OPEN.lock() {
        v.retain(|(c, _)| *c != chat);
    }
    got
}

/// Telegram reply keyboard for the choices (None without choices).
pub fn keyboard(options: &[String]) -> serde_json::Value {
    if options.is_empty() {
        return serde_json::json!({ "remove_keyboard": true });
    }
    let rows: Vec<serde_json::Value> = options.iter().map(|o| serde_json::json!([{ "text": o }])).collect();
    serde_json::json!({ "keyboard": rows, "one_time_keyboard": true, "resize_keyboard": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_through_the_asker_and_picks_by_number() {
        let opts = vec!["nginx".to_string(), "caddy".to_string()];
        assert_eq!(pick("2", &opts), "caddy");
        assert_eq!(pick(" apache ", &opts), "apache");
        assert!(clarify(false, "which?", &opts).unwrap_err().contains("unattended"));
        set_asker(Some(Box::new(|q: &str, o: &[String]| Some(format!("{q}:{}", pick("1", o))))));
        assert_eq!(clarify(false, "which?", &opts).unwrap(), "the user answered: which?:nginx");
        set_asker(Some(Box::new(|_: &str, _: &[String]| None)));
        assert!(clarify(false, "which?", &opts).unwrap_err().contains("no answer"));
        set_asker(None);
    }

    #[test]
    fn daemon_routes_the_next_message_as_the_answer() {
        assert!(!answer(42, "hello"), "nothing open: a normal message");
        let rx = open(42);
        assert!(!answer(7, "other chat"));
        assert!(answer(42, "caddy"));
        assert_eq!(wait(42, rx).as_deref(), Some("caddy"));
        assert!(!answer(42, "again"), "answered once");
        assert_eq!(keyboard(&["a".into()])["keyboard"][0][0]["text"], "a");
    }
}

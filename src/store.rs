//! Append-only JSONL transcripts, and recall over them (session_search): a
//! tiny incremental BM25 index in the small build (`index`), SQLite FTS5 with
//! the `sqlite` feature (`fts`). Both index only user and assistant messages
//! and read the text back from the transcript for snippets.

use crate::llm::Msg;
use crate::util::{fmt_time, new_id, now, one_line};
use serde_json::{json, Value};
use std::io::{BufRead, Seek, Write};
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite")]
mod fts;
#[cfg(not(feature = "sqlite"))]
mod index;

pub struct Session {
    pub id: String,
    /// None = not recorded (heartbeat checks: frequent, repetitive, would wear the SD card).
    path: Option<PathBuf>,
}

impl Session {
    pub fn new(home: &Path, kind: &str) -> Self {
        let id = format!("{}-{}-{}", now(), kind, new_id());
        let path = (kind != "heartbeat").then(|| home.join("sessions").join(format!("{id}.jsonl")));
        Self { id, path }
    }

    /// An earlier session to continue (the daemon after a restart): None if its
    /// transcript is gone or older than `max_age_secs`.
    pub fn open(home: &Path, id: &str, max_age_secs: u64) -> Option<Self> {
        if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") {
            return None;
        }
        let path = home.join("sessions").join(format!("{id}.jsonl"));
        let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?.as_secs();
        (age <= max_age_secs).then(|| Self { id: id.to_string(), path: Some(path) })
    }

    /// The messages recorded so far (notes for the transcript reader left out).
    pub fn messages(&self) -> Vec<Msg> {
        let Some(path) = &self.path else { return Vec::new() };
        crate::util::read(path)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| serde_json::from_value::<Msg>(v["m"].clone()).ok())
            .filter(|m| m.role != "system")
            .collect()
    }

    pub fn append(&self, m: &Msg) {
        let Some(path) = &self.path else { return };
        let line = json!({ "t": now(), "m": m }).to_string();
        let res = (|| {
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(f, "{line}")
        })();
        if let Err(e) = res {
            eprintln!("pagurus: could not write transcript: {e}");
        }
    }
}

/// What session_search asks for.
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Unix-time bounds on the message.
    pub after: Option<u64>,
    pub before: Option<u64>,
    /// Session kind: cli, tg, run, sub, … ("telegram" is accepted for tg).
    pub channel: Option<String>,
    /// The current session, already in the model's context.
    pub skip: String,
}

impl Query {
    pub fn new(text: &str, limit: usize) -> Self {
        Self { text: text.into(), limit, after: None, before: None, channel: None, skip: String::new() }
    }

    /// Whether a message from session `sid` at `t` passes the filters.
    fn keeps(&self, sid: &str, t: u64) -> bool {
        sid != self.skip
            && self.after.is_none_or(|a| t >= a)
            && self.before.is_none_or(|b| t < b)
            && self.channel.as_deref().is_none_or(|c| channel(sid) == c)
    }
}

/// A message found: the hit itself and the messages around it in its session.
pub struct Hit {
    pub sid: String,
    pub t: u64,
    pub role: String,
    pub text: String,
    pub prev: Option<(String, String)>,
    pub next: Option<(String, String)>,
}

/// `1762178520-tg-ab12cd` → `tg`.
fn channel(sid: &str) -> &str {
    let rest = sid.split_once('-').map_or("", |x| x.1);
    rest.rsplit_once('-').map_or(rest, |x| x.0)
}

/// `2026-06-01`, `2026-06-01 14:00`, or a span back from now: `7d`, `24h`, `2w`, `3m`.
pub fn parse_when(s: &str) -> Option<u64> {
    let s = s.trim();
    let unit = match s.chars().last()? {
        'h' => 3600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        'm' => 30 * 86_400,
        'y' => 365 * 86_400,
        _ => return crate::util::parse_local(s),
    };
    let n: u64 = s[..s.len() - 1].trim().parse().ok()?;
    Some(now().saturating_sub(n * unit))
}

const STOP: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "that", "this", "with", "you", "your", "have", "has", "had", "not", "but",
    "from", "they", "what", "can", "will", "would", "there", "their", "its", "it's", "our", "out", "all", "any", "how",
    "about", "into", "than", "then", "them", "these", "those", "just", "also", "been", "being", "did", "does", "which",
    "who", "when", "where", "why", "a", "an", "is", "it", "of", "on", "or", "to", "in", "at", "be", "as", "by", "we",
    "me", "my", "so", "if", "do", "no", "i",
];

/// A light English stemmer: plurals and -ing/-ed, so "tunnels" finds "tunnel".
fn stem(w: &str) -> String {
    if !w.chars().all(|c| c.is_alphabetic()) || w.len() < 4 {
        return w.into();
    }
    for (suf, min, rep) in [("ies", 5, "y"), ("sses", 5, "ss"), ("ing", 6, ""), ("ed", 5, ""), ("es", 5, ""), ("s", 4, "")] {
        if w.len() >= min && w.ends_with(suf) && !(suf == "s" && (w.ends_with("ss") || w.ends_with("us") || w.ends_with("is"))) {
            if suf == "es" && !(w.ends_with("shes") || w.ends_with("ches") || w.ends_with("xes")) {
                continue; // "files" → "file" via the plain -s rule
            }
            return format!("{}{rep}", &w[..w.len() - suf.len()]);
        }
    }
    w.into()
}

/// Search terms in `text` with their byte positions: lowercased, stemmed, stop words dropped.
fn tokens(text: &str) -> impl Iterator<Item = (usize, String)> + '_ {
    words(text, true)
}

fn words(text: &str, stemmed: bool) -> impl Iterator<Item = (usize, String)> + '_ {
    let mut pos = 0;
    text.split(|c: char| !c.is_alphanumeric()).filter_map(move |w| {
        let at = pos;
        pos += w.len() + text[at + w.len()..].chars().next().map_or(0, char::len_utf8);
        let n = w.chars().count();
        if !(2..=40).contains(&n) {
            return None;
        }
        let low = w.to_lowercase();
        (!STOP.contains(&low.as_str())).then(|| (at, if stemmed { stem(&low) } else { low }))
    })
}

/// The query as (term, is_prefix) pairs: `deploy*` matches deployed, deployment…
fn query_terms(q: &str) -> Vec<(String, bool)> {
    query_words(q, true)
}

/// `stemmed: false` for SQLite, whose porter tokenizer stems on its own.
fn query_words(q: &str, stemmed: bool) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = Vec::new();
    for w in q.split_whitespace() {
        let prefix = w.ends_with('*');
        let toks: Vec<(usize, String)> = words(w, stemmed).collect();
        for (i, (at, t)) in toks.iter().enumerate() {
            // `deploy*`: the last word unstemmed, matched as a prefix
            let p = prefix && i + 1 == toks.len();
            let t = if p { w[*at..].chars().take_while(|c| c.is_alphanumeric()).collect::<String>().to_lowercase() } else { t.clone() };
            if !v.iter().any(|x| x.0 == t) {
                v.push((t, p));
            }
        }
    }
    v.truncate(16);
    v
}

/// Session transcripts: (file stem, size).
fn session_files(home: &Path) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = std::fs::read_dir(home.join("sessions"))
        .map(|rd| {
            rd.filter_map(|e| {
                let e = e.ok()?;
                let name = e.file_name().into_string().ok()?;
                Some((name.strip_suffix(".jsonl")?.to_string(), e.metadata().ok()?.len()))
            })
            .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn session_path(home: &Path, sid: &str) -> PathBuf {
    home.join("sessions").join(format!("{sid}.jsonl"))
}

/// One transcript line as (time, role, text), for user and assistant messages with text.
fn parse_line(line: &str) -> Option<(u64, String, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let role = v["m"]["role"].as_str()?;
    let text = v["m"]["content"].as_str().filter(|t| !t.trim().is_empty())?;
    matches!(role, "user" | "assistant").then(|| (v["t"].as_u64().unwrap_or(0), role.to_string(), text.to_string()))
}

/// Messages appended to a transcript since byte `from`: calls `f(offset, t, role, text)`
/// for each complete line and returns the offset after the last one.
fn scan_from(home: &Path, sid: &str, from: u64, mut f: impl FnMut(u64, u64, &str, &str)) -> u64 {
    let Ok(mut file) = std::fs::File::open(session_path(home, sid)) else { return from };
    if file.seek(std::io::SeekFrom::Start(from)).is_err() {
        return from;
    }
    let mut r = std::io::BufReader::new(file);
    let (mut off, mut line) = (from, String::new());
    while let Ok(n @ 1..) = r.read_line(&mut line) {
        if !line.ends_with('\n') {
            break; // still being written: index it next time
        }
        if let Some((t, role, text)) = parse_line(&line) {
            f(off, t, &role, &text);
        }
        off += n as u64;
        line.clear();
    }
    off
}

/// The message on the line starting at `off`.
#[cfg(not(feature = "sqlite"))]
fn read_at(home: &Path, sid: &str, off: u64) -> Option<(u64, String, String)> {
    use std::io::Read;
    let mut file = std::fs::File::open(session_path(home, sid)).ok()?;
    file.seek(std::io::SeekFrom::Start(off)).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file.take(4 << 20)).read_line(&mut line).ok()?;
    parse_line(&line)
}

/// Up to `max` chars of `text` around the first query term in it.
fn snippet(text: &str, terms: &[(String, bool)], max: usize) -> String {
    let at = tokens(text)
        .find(|(_, t)| terms.iter().any(|(q, p)| if *p { t.starts_with(q.as_str()) } else { t == q }))
        .map_or(0, |(i, _)| i);
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let before: Vec<char> = flat(&text[..at]).chars().collect();
    let lead = max / 3;
    let start = before.len().saturating_sub(lead);
    let mut s: String = before[start..].iter().collect();
    if !s.is_empty() && text[..at].ends_with(char::is_whitespace) {
        s.push(' ');
    }
    let room = max.saturating_sub(s.chars().count());
    let rest = flat(&text[at..]);
    let more = rest.chars().count() > room;
    s.extend(rest.chars().take(room));
    format!("{}{s}{}", if start > 0 { "…" } else { "" }, if more { "…" } else { "" })
}

/// Past sessions (ids, without duplicates) in which the user wrote something matching `text`.
pub fn user_sessions(home: &Path, text: &str, limit: usize, skip: &str) -> Vec<String> {
    let terms = query_terms(text);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut q = Query::new(text, limit);
    q.skip = skip.to_string();
    #[cfg(feature = "sqlite")]
    let hits = fts::search(home, &q, &query_words(text, false));
    #[cfg(not(feature = "sqlite"))]
    let hits = index::search(home, &q, &terms);
    let mut sids: Vec<String> = hits.unwrap_or_default().into_iter().filter(|h| h.role == "user").map(|h| h.sid).collect();
    sids.sort();
    sids.dedup();
    sids
}

pub fn search(home: &Path, q: &Query) -> String {
    let terms = query_terms(&q.text);
    if terms.is_empty() {
        return "error: give at least one search word (common words like \"the\" are ignored)".into();
    }
    #[cfg(feature = "sqlite")]
    let hits = fts::search(home, q, &query_words(&q.text, false));
    #[cfg(not(feature = "sqlite"))]
    let hits = index::search(home, q, &terms);
    let hits = match hits {
        Ok(h) => h,
        Err(e) => return format!("error: session index: {e}"),
    };
    let words: Vec<&str> = terms.iter().map(|t| t.0.as_str()).collect();
    if hits.is_empty() {
        return format!("no past messages match: {}", words.join(" "));
    }
    let mut out = String::new();
    for h in hits {
        out += &format!("[{} · {} · {}]\n", fmt_time(h.t), channel(&h.sid), h.sid);
        if let Some((role, text)) = &h.prev {
            out += &format!("  {role}: {}\n", one_line(text, 160));
        }
        out += &format!("» {}: {}\n", h.role, snippet(&h.text, &terms, 320));
        if let Some((role, text)) = &h.next {
            out += &format!("  {role}: {}\n", one_line(text, 200));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    type Lines<'a> = &'a [(u64, &'a str, &'a str)];

    pub fn home_with(sessions: &[(&str, Lines)]) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("pagurus-search-{}", crate::util::new_id()));
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        for (sid, msgs) in sessions {
            append(&home, sid, msgs);
        }
        home
    }

    pub fn append(home: &std::path::Path, sid: &str, msgs: Lines) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(home.join(format!("sessions/{sid}.jsonl"))).unwrap();
        for (t, role, text) in msgs {
            writeln!(f, "{}", serde_json::json!({"t": t, "m": {"role": role, "content": text}})).unwrap();
        }
    }

    fn q(text: &str) -> super::Query {
        super::Query::new(text, 5)
    }

    #[test]
    fn tokens_stem_and_drop_stop_words() {
        let t: Vec<String> = super::tokens("The tunnels were RESTARTED; files, boxes & running").map(|x| x.1).collect();
        assert_eq!(t, ["tunnel", "restart", "file", "box", "runn"]);
        assert_eq!(super::channel("1762178520-tg-ab12cd"), "tg");
        assert_eq!(super::query_terms("deploy* the Tunnels"), vec![("deploy".into(), true), ("tunnel".into(), false)]);
        let s = super::snippet(&format!("{} the cloudflare tunnel url changed {}", "x ".repeat(200), "y ".repeat(200)), &super::query_terms("tunnel"), 80);
        assert!(s.starts_with('…') && s.contains("cloudflare tunnel url") && s.ends_with('…'), "{s}");
        assert!(super::parse_when("7d").unwrap() + 7 * 86_400 >= crate::util::now());
        assert!(super::parse_when("2026-06-01").is_some() && super::parse_when("soon").is_none());
    }

    #[test]
    fn search_ranks_filters_and_shows_context() {
        let now = crate::util::now();
        let home = home_with(&[
            ("100-tg-a", &[(now - 900_000, "user", "the cloudflare tunnel url changed again"), (now - 899_990, "assistant", "I restarted cloudflared")]),
            ("200-cli-b", &[(now - 100, "user", "grafana dashboard loads slowly"), (now - 90, "assistant", "the tunnel is fine, grafana needed more memory"),
                            (now - 80, "user", "thanks")]),
        ]);
        let r = super::search(&home, &q("cloudflare tunnel"));
        assert!(r.starts_with('[') && r.contains("» user: the cloudflare tunnel url changed"), "{r}");
        assert!(r.contains("  assistant: I restarted cloudflared"), "context after the hit: {r}");
        assert!(r.find("cloudflare tunnel url").unwrap() < r.find("grafana needed").unwrap(), "both terms rank first: {r}");
        let r = super::search(&home, &q("grafana"));
        assert!(r.contains("  user: grafana dashboard loads slowly") || r.contains("» user: grafana dashboard"), "{r}");
        let mut f = q("tunnel");
        f.channel = Some("tg".into());
        let r = super::search(&home, &f);
        assert!(r.contains("cloudflare") && !r.contains("grafana"), "{r}");
        let mut f = q("tunnel");
        f.after = super::parse_when("2d");
        let r = super::search(&home, &f);
        assert!(r.contains("grafana") && !r.contains("cloudflared"), "{r}");
        let mut f = q("tunnel");
        f.skip = "200-cli-b".into();
        assert!(!super::search(&home, &f).contains("grafana"));
        assert!(super::search(&home, &q("kubernetes")).starts_with("no past messages match"));
        assert!(super::search(&home, &q("the")).starts_with("error"));
        // appended messages are found on the next search
        append(&home, "200-cli-b", &[(now - 10, "user", "set up kubernetes on the pi")]);
        assert!(super::search(&home, &q("kubernetes")).contains("set up kubernetes"));
        // a new session too
        append(&home, "300-tg-c", &[(now - 5, "assistant", "kubernetes is running")]);
        let r = super::search(&home, &q("kubernetes"));
        assert!(r.contains("set up kubernetes") && r.contains("kubernetes is running"), "{r}");
        // prefix search
        assert!(super::search(&home, &q("kube*")).contains("kubernetes"));
    }
}

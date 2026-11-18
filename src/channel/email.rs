//! Email: poll IMAP (TLS) for unseen mail from allowlisted senders, answer
//! over SMTP (TLS or STARTTLS, AUTH PLAIN/LOGIN) in the same thread. Plain
//! text only (HTML is never rendered); auto-replies, bulk and list mail are
//! ignored; mail from anyone else is never fetched or answered (no backscatter).

use super::{Channel, Incoming, Stream};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Mutex;
use std::time::Duration;

/// Bytes of one message fetched (headers + text; big attachments are cut).
const MAX_MAIL: usize = 256 * 1024;
/// Messages taken per poll.
const PER_POLL: usize = 5;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct EmailCfg {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    /// 465 = TLS from the start; 587 (or anything else) = STARTTLS.
    pub smtp_port: u16,
    pub username: String,
    /// Env var holding the password / app password (never the password itself).
    pub password_env: String,
    /// The From address of replies (default: username).
    pub address: String,
    /// Senders pagurus reads and answers. Empty = nobody.
    pub allowed_senders: Vec<String>,
    pub mailbox: String,
    pub poll_seconds: u64,
    /// Only take mail your provider marked as really from that sender
    /// (Authentication-Results: dmarc=pass, or aligned dkim/spf pass).
    pub require_auth: bool,
}

impl Default for EmailCfg {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            username: String::new(),
            password_env: "PAGURUS_EMAIL_PASSWORD".into(),
            address: String::new(),
            allowed_senders: vec![],
            mailbox: "INBOX".into(),
            poll_seconds: 30,
            require_auth: true,
        }
    }
}

// ---------- IMAP ----------

pub struct Imap<S: Read + Write> {
    s: BufReader<S>,
    tag: u32,
}

/// What one command returned: untagged lines and the literals ({n} data) in order.
#[derive(Default, Debug)]
pub struct Reply {
    pub lines: Vec<String>,
    pub literals: Vec<Vec<u8>>,
}

fn quote(s: &str) -> Result<String, String> {
    if s.contains(['\r', '\n', '\0']) {
        return Err("line breaks aren't allowed here".into());
    }
    Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn read_line<R: BufRead>(r: &mut R, max: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    r.by_ref().take(max as u64).read_until(b'\n', &mut buf).map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Err("connection closed".into());
    }
    if !buf.ends_with(b"\n") {
        return Err("line too long".into());
    }
    Ok(String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string())
}

impl<S: Read + Write> Imap<S> {
    /// Wrap a connected stream and read the server greeting.
    pub fn new(s: S) -> Result<Self, String> {
        let mut i = Self { s: BufReader::new(s), tag: 0 };
        let hello = read_line(&mut i.s, 8192)?;
        if !hello.starts_with("* OK") && !hello.starts_with("* PREAUTH") {
            return Err(format!("imap: {}", crate::util::one_line(&hello, 120)));
        }
        Ok(i)
    }

    pub fn cmd(&mut self, c: &str) -> Result<Reply, String> {
        self.tag += 1;
        let tag = format!("h{}", self.tag);
        let w = self.s.get_mut();
        w.write_all(format!("{tag} {c}\r\n").as_bytes()).and_then(|_| w.flush()).map_err(|e| format!("imap: {e}"))?;
        let mut r = Reply::default();
        loop {
            let mut line = read_line(&mut self.s, 64 * 1024).map_err(|e| format!("imap: {e}"))?;
            // a line ending in {n} is followed by n bytes of data, then the rest of the line
            while let Some(n) = line.strip_suffix('}').and_then(|l| l.rsplit_once('{')).and_then(|(_, n)| n.parse::<usize>().ok()) {
                if n > MAX_MAIL + 1024 {
                    return Err("imap: literal too large".into());
                }
                let mut lit = vec![0; n];
                self.s.read_exact(&mut lit).map_err(|e| format!("imap: {e}"))?;
                r.literals.push(lit);
                line = read_line(&mut self.s, 64 * 1024).map_err(|e| format!("imap: {e}"))?;
            }
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                if rest.starts_with("OK") {
                    return Ok(r);
                }
                // never echo the command: it may be the LOGIN line
                return Err(format!("imap: {}", crate::util::one_line(rest, 160)));
            }
            r.lines.push(line);
        }
    }

    pub fn login(&mut self, user: &str, pass: &str) -> Result<(), String> {
        self.cmd(&format!("LOGIN {} {}", quote(user)?, quote(pass)?)).map(|_| ()).map_err(|_| "imap: login failed (username / password?)".to_string())
    }

    /// UIDs of unseen mail since `since` from any of `senders`.
    pub fn search(&mut self, since: &str, senders: &[String]) -> Result<Vec<u32>, String> {
        let mut from = String::new();
        for (i, s) in senders.iter().enumerate() {
            if i + 1 < senders.len() {
                from.push_str("OR ");
            }
            from.push_str(&format!("FROM {} ", quote(s)?));
        }
        let r = self.cmd(&format!("UID SEARCH UNSEEN SINCE {since} {}", from.trim_end()))?;
        Ok(r.lines.iter().filter_map(|l| l.strip_prefix("* SEARCH")).flat_map(|l| l.split_whitespace().filter_map(|n| n.parse().ok())).collect())
    }

    /// The start of a message, without marking it seen.
    pub fn fetch(&mut self, uid: u32) -> Result<Vec<u8>, String> {
        let mut r = self.cmd(&format!("UID FETCH {uid} BODY.PEEK[]<0.{MAX_MAIL}>"))?;
        r.literals.pop().ok_or_else(|| "imap: no message body".into())
    }

    pub fn seen(&mut self, uid: u32) -> Result<(), String> {
        self.cmd(&format!("UID STORE {uid} +FLAGS.SILENT (\\Seen)")).map(|_| ())
    }
}

// ---------- reading mail ----------

#[derive(Debug, Default, PartialEq)]
pub struct Mail {
    pub from: String,
    pub subject: String,
    pub message_id: String,
    pub references: String,
    pub text: String,
    /// Auto-reply, bounce, bulk or list mail: never answered.
    pub auto: bool,
    /// The receiving server vouches the From address is genuine.
    pub authentic: bool,
}

fn split_head(raw: &[u8]) -> (String, &[u8]) {
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, 4)).or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
    match at {
        Some((i, n)) => (String::from_utf8_lossy(&raw[..i]).into_owned(), &raw[i + n..]),
        None => (String::from_utf8_lossy(raw).into_owned(), &[]),
    }
}

/// Unfolded headers, names lowercased, in order.
fn headers(head: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    for line in head.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = v.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
        } else if let Some((k, val)) = line.split_once(':') {
            v.push((k.trim().to_lowercase(), val.trim().to_string()));
        }
    }
    v
}

fn get<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
}

/// The bare address in "Name <a@b.c>" (lowercased).
pub fn address(s: &str) -> String {
    let a = match (s.rfind('<'), s.rfind('>')) {
        (Some(i), Some(j)) if j > i => &s[i + 1..j],
        _ => s.split_whitespace().find(|w| w.contains('@')).unwrap_or(""),
    };
    a.trim().trim_matches(['"', '\'']).to_lowercase()
}

/// A header parameter, e.g. boundary or charset.
fn param(v: &str, name: &str) -> String {
    v.split(';').skip(1).find_map(|p| {
        let (k, val) = p.split_once('=')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| val.trim().trim_matches('"').to_string())
    })
    .unwrap_or_default()
}

fn unbase64(s: &[u8]) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for v in s.iter().filter_map(|c| val(*c)) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

fn unqp(s: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'=' if s.get(i + 1) == Some(&b'\r') && s.get(i + 2) == Some(&b'\n') => i += 3,
            b'=' if s.get(i + 1) == Some(&b'\n') => i += 2,
            b'=' if i + 2 < s.len() && hex(s[i + 1]).is_some() && hex(s[i + 2]).is_some() => {
                out.push(hex(s[i + 1]).unwrap_or(0) << 4 | hex(s[i + 2]).unwrap_or(0));
                i += 3;
            }
            b'_' if header => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn decode_charset(b: &[u8], charset: &str) -> String {
    if charset.eq_ignore_ascii_case("iso-8859-1") || charset.eq_ignore_ascii_case("latin1") || charset.eq_ignore_ascii_case("us-ascii") {
        return b.iter().map(|c| *c as char).collect();
    }
    String::from_utf8_lossy(b).into_owned()
}

/// RFC 2047 encoded words (=?utf-8?B?…?= / =?utf-8?Q?…?=) in a header.
pub fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("=?") {
        let Some(parts) = rest[i + 2..].splitn(3, '?').collect::<Vec<_>>().get(..3).map(|p| p.to_vec()) else { break };
        let Some(end) = parts[2].find("?=") else { break };
        let (charset, enc, data) = (parts[0], parts[1], &parts[2][..end]);
        let bytes = if enc.eq_ignore_ascii_case("b") { unbase64(data.as_bytes()) } else { unqp(data.as_bytes(), true) };
        let before = &rest[..i];
        if !(before.trim().is_empty() && !out.is_empty()) {
            out.push_str(before); // whitespace between two encoded words is dropped
        }
        out.push_str(&decode_charset(&bytes, charset));
        let used = i + 2 + parts[0].len() + 1 + parts[1].len() + 1 + end + 2;
        rest = &rest[used..];
    }
    out.push_str(rest);
    out
}

/// The text/plain content of a MIME entity (None: there is none, e.g. HTML only).
fn plain_text(head: &[(String, String)], body: &[u8], depth: u8) -> Option<String> {
    let ctype = get(head, "content-type");
    let mime = ctype.split(';').next().unwrap_or("").trim().to_lowercase();
    if mime.starts_with("multipart/") {
        let boundary = param(ctype, "boundary");
        if boundary.is_empty() || depth > 4 {
            return None;
        }
        let delim = format!("--{boundary}");
        let text = String::from_utf8_lossy(body).into_owned();
        for part in text.split(delim.as_str()).skip(1) {
            if part.starts_with("--") {
                break;
            }
            let part = part.trim_start_matches(['\r', '\n']);
            let (ph, pb) = split_head(part.as_bytes());
            if let Some(t) = plain_text(&headers(&ph), pb, depth + 1) {
                return Some(t);
            }
        }
        return None;
    }
    if !(mime.is_empty() || mime == "text/plain") || get(head, "content-disposition").to_lowercase().starts_with("attachment") {
        return None;
    }
    let raw = match get(head, "content-transfer-encoding").to_lowercase().as_str() {
        "base64" => unbase64(body),
        "quoted-printable" => unqp(body, false),
        _ => body.to_vec(),
    };
    Some(decode_charset(&raw, &param(ctype, "charset")))
}

/// Drop the quoted conversation below a reply, and the signature.
fn strip_quotes(t: &str) -> String {
    let mut out = Vec::new();
    for line in t.lines() {
        let l = line.trim();
        let wrote = l.starts_with("On ") && l.ends_with("wrote:");
        if wrote || l == "-- " || line == "-- " || l.starts_with("-----Original Message-----") || l.starts_with("________________________________") {
            break;
        }
        if !l.starts_with('>') {
            out.push(line.trim_end());
        }
    }
    out.join("\n").trim().to_string()
}

pub fn parse_mail(raw: &[u8]) -> Mail {
    let (head, body) = split_head(raw);
    let h = headers(&head);
    let from = address(get(&h, "from"));
    let has = |n: &str| h.iter().any(|(k, _)| k == n);
    let auto_sub = get(&h, "auto-submitted").to_lowercase();
    let prec = get(&h, "precedence").to_lowercase();
    let local = from.split('@').next().unwrap_or("");
    let auto = (!auto_sub.is_empty() && auto_sub != "no")
        || matches!(prec.as_str(), "bulk" | "list" | "junk" | "auto_reply")
        || has("list-id")
        || has("list-unsubscribe")
        || has("x-autoreply")
        || has("x-autorespond")
        || get(&h, "x-auto-response-suppress").to_lowercase().contains("all")
        || get(&h, "return-path").trim() == "<>"
        || ["mailer-daemon", "postmaster", "noreply", "no-reply", "do-not-reply", "donotreply"].contains(&local);
    // only the topmost Authentication-Results: the receiving server's own
    let ar = get(&h, "authentication-results").to_lowercase();
    let domain = from.rsplit('@').next().unwrap_or("").to_string();
    let aligned = |method: &str, key: &str| {
        ar.split(';').any(|c| {
            let c = c.trim();
            c.starts_with(&format!("{method}=pass"))
                && c.split_whitespace().any(|w| w.strip_prefix(key).is_some_and(|v| v.trim_matches(['"', ')']).rsplit('@').next() == Some(domain.as_str())))
        })
    };
    let authentic = !domain.is_empty()
        && (ar.split(';').any(|c| c.trim().starts_with("dmarc=pass")) || aligned("dkim", "header.d=") || aligned("spf", "smtp.mailfrom="));
    Mail {
        subject: decode_words(get(&h, "subject")).replace(['\r', '\n'], " "),
        message_id: get(&h, "message-id").to_string(),
        references: get(&h, "references").to_string(),
        text: plain_text(&h, body, 0).map(|t| strip_quotes(&t)).unwrap_or_default(),
        from,
        auto,
        authentic,
    }
}

// ---------- sending mail ----------

/// Read one SMTP reply (all its lines): (code, text).
fn smtp_reply<S: Read>(s: &mut S) -> Result<(u16, String), String> {
    let mut text = String::new();
    loop {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while !line.ends_with(b"\n") {
            if line.len() > 4096 || s.read(&mut b).map_err(|e| format!("smtp: {e}"))? == 0 {
                return Err("smtp: connection closed".into());
            }
            line.push(b[0]);
        }
        let l = String::from_utf8_lossy(&line).trim_end().to_string();
        let code = l.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| format!("smtp: bad reply {}", crate::util::one_line(&l, 80)))?;
        text.push_str(l.get(4..).unwrap_or(""));
        text.push('\n');
        if l.as_bytes().get(3) != Some(&b'-') {
            return Ok((code, text));
        }
    }
}

fn smtp_cmd<S: Read + Write>(s: &mut S, c: &str, want: u16) -> Result<String, String> {
    s.write_all(format!("{c}\r\n").as_bytes()).and_then(|_| s.flush()).map_err(|e| format!("smtp: {e}"))?;
    let (code, text) = smtp_reply(s)?;
    if code / 100 != want / 100 {
        // AUTH lines carry credentials: never echo the command
        let what = c.split_whitespace().next().filter(|w| ["EHLO", "STARTTLS", "AUTH", "MAIL", "RCPT", "DATA", "QUIT"].contains(w)).unwrap_or("step");
        return Err(format!("smtp {what}: {code} {}", crate::util::one_line(&text, 160)));
    }
    Ok(text)
}

pub struct Envelope<'a> {
    pub user: &'a str,
    pub pass: &'a str,
    pub from: &'a str,
    pub to: &'a str,
}

/// The SMTP conversation after the greeting is read, on an already secure
/// stream: EHLO, AUTH, MAIL/RCPT/DATA, QUIT.
pub fn smtp_send<S: Read + Write>(s: &mut S, env: &Envelope, data: &str) -> Result<(), String> {
    let caps = smtp_cmd(s, "EHLO pagurus", 250)?.to_uppercase();
    let auth = caps.lines().find(|l| l.starts_with("AUTH")).unwrap_or("").to_string();
    if auth.contains("PLAIN") || !auth.contains("LOGIN") {
        let token = crate::util::base64(format!("\0{}\0{}", env.user, env.pass).as_bytes());
        smtp_cmd(s, &format!("AUTH PLAIN {token}"), 235)?;
    } else {
        smtp_cmd(s, "AUTH LOGIN", 334)?;
        smtp_cmd(s, &crate::util::base64(env.user.as_bytes()), 334)?;
        smtp_cmd(s, &crate::util::base64(env.pass.as_bytes()), 235)?;
    }
    smtp_cmd(s, &format!("MAIL FROM:<{}>", env.from), 250)?;
    smtp_cmd(s, &format!("RCPT TO:<{}>", env.to), 250)?;
    smtp_cmd(s, "DATA", 354)?;
    let mut body = String::with_capacity(data.len() + 16);
    for line in data.split("\r\n") {
        if line.starts_with('.') {
            body.push('.'); // dot-stuffing
        }
        body.push_str(line);
        body.push_str("\r\n");
    }
    body.push('.');
    smtp_cmd(s, &body, 250)?;
    let _ = smtp_cmd(s, "QUIT", 221);
    Ok(())
}

fn valid_address(a: &str) -> bool {
    a.len() <= 254 && a.matches('@').count() == 1 && !a.contains(|c: char| c.is_whitespace() || c.is_control() || "<>,;\"".contains(c))
}

/// RFC 5322 date, UTC.
fn date(t: u64) -> String {
    let days = (t / 86_400) as i64;
    let (y, m, d) = crate::util::civil(days);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let s = t % 86_400;
    format!("{}, {d:02} {} {y} {:02}:{:02}:{:02} +0000", WD[(days % 7) as usize], MO[m as usize - 1], s / 3600, s / 60 % 60, s % 60)
}

/// IMAP SEARCH date: 17-Nov-2026.
fn imap_date(t: u64) -> String {
    let (y, m, d) = crate::util::civil((t / 86_400) as i64);
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{d}-{}-{y}", MO[m as usize - 1])
}

#[derive(Clone, Default)]
struct Thread {
    subject: String,
    message_id: String,
    references: String,
}

/// A plain-text reply as an RFC 5322 message (CRLF line ends).
fn compose(from: &str, to: &str, th: &Thread, text: &str, t: u64) -> String {
    let clean = |s: &str| s.replace(['\r', '\n'], " ");
    let subject = match th.subject.trim() {
        "" => "pagurus".to_string(),
        s if s.to_lowercase().starts_with("re:") => clean(s),
        s => format!("Re: {}", clean(s)),
    };
    let subject = if subject.is_ascii() { subject } else { format!("=?UTF-8?B?{}?=", crate::util::base64(subject.as_bytes())) };
    let domain = from.rsplit('@').next().unwrap_or("pagurus.local");
    let mut m = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {}\r\nMessage-ID: <{:016x}.pagurus@{domain}>\r\n",
        date(t),
        super::random_u64()
    );
    if !th.message_id.is_empty() {
        let refs = format!("{} {}", clean(&th.references), clean(&th.message_id));
        m += &format!("In-Reply-To: {}\r\nReferences: {}\r\n", clean(&th.message_id), refs.trim());
    }
    m += "Auto-Submitted: auto-replied\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n";
    let b64 = crate::util::base64(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    for chunk in b64.as_bytes().chunks(76) {
        m += std::str::from_utf8(chunk).unwrap_or("");
        m += "\r\n";
    }
    m.trim_end().to_string()
}

// ---------- the channel ----------

pub struct Email {
    c: EmailCfg,
    pass: String,
    allowed: Vec<String>,
    started: u64,
    last_poll: Mutex<u64>,
    threads: Mutex<HashMap<String, Thread>>,
    /// UIDs looked at and left alone (failed checks): not fetched again.
    skipped: Mutex<HashSet<u32>>,
}

impl Email {
    pub fn new(c: &EmailCfg) -> Result<Self, String> {
        let pass = std::env::var(&c.password_env).map_err(|_| format!("email enabled but ${} is not set", c.password_env))?;
        let allowed: Vec<String> = c.allowed_senders.iter().map(|a| address(a)).filter(|a| valid_address(a)).collect();
        if allowed.is_empty() {
            return Err("email.allowed_senders is empty — refusing to read or answer anyone".into());
        }
        if c.imap_host.is_empty() || c.smtp_host.is_empty() || c.username.is_empty() {
            return Err("email needs imap_host, smtp_host and username".into());
        }
        let mut c = c.clone();
        if c.address.is_empty() {
            c.address = c.username.clone();
        }
        if !valid_address(&c.address) {
            return Err(format!("email.address {:?} isn't an email address", c.address));
        }
        Ok(Self {
            c,
            pass,
            allowed,
            started: crate::util::now(),
            last_poll: Mutex::new(0),
            threads: Mutex::default(),
            skipped: Mutex::default(),
        })
    }

    fn poll_imap(&self) -> Result<Vec<Incoming>, String> {
        let tls = super::connect_tls(&self.c.imap_host, self.c.imap_port, Duration::from_secs(60))?;
        let mut imap = Imap::new(tls)?;
        imap.login(&self.c.username, &self.pass)?;
        imap.cmd(&format!("SELECT {}", quote(&self.c.mailbox)?))?;
        // a day before startup: old unread mail isn't answered out of the blue
        let uids = imap.search(&imap_date(self.started.saturating_sub(86_400)), &self.allowed)?;
        let skipped = self.skipped.lock().map(|s| s.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for uid in uids.into_iter().filter(|u| !skipped.contains(u)).take(PER_POLL) {
            let mail = parse_mail(&imap.fetch(uid)?);
            let why = if !self.allowed.contains(&mail.from) {
                Some("sender not in allowed_senders") // FROM search matches substrings
            } else if mail.auto {
                Some("auto-reply / bulk mail")
            } else if self.c.require_auth && !mail.authentic {
                Some("not authenticated (no dmarc/dkim/spf pass for the sender's domain)")
            } else if mail.text.is_empty() && mail.subject.trim().is_empty() {
                Some("no plain-text content")
            } else {
                None
            };
            if let Some(why) = why {
                eprintln!("pagurus: email from {} ignored: {why}", mail.from);
                if let Ok(mut s) = self.skipped.lock() {
                    s.insert(uid);
                }
                continue;
            }
            imap.seen(uid)?;
            let text = if mail.text.is_empty() { mail.subject.clone() } else { mail.text.clone() };
            if let Ok(mut t) = self.threads.lock() {
                t.insert(mail.from.clone(), Thread { subject: mail.subject, message_id: mail.message_id, references: mail.references });
            }
            out.push((mail.from, super::clip_in(&text), None));
        }
        let _ = imap.cmd("LOGOUT");
        Ok(out)
    }

    fn send_mail(&self, to: &str, data: &str) -> Result<(), String> {
        let env = Envelope { user: &self.c.username, pass: &self.pass, from: &self.c.address, to };
        let tcp = super::tcp(&self.c.smtp_host, self.c.smtp_port, Duration::from_secs(60))?;
        if self.c.smtp_port == 465 {
            let mut s = super::tls(tcp, &self.c.smtp_host)?;
            greet(&mut s)?;
            smtp_send(&mut s, &env, data)
        } else {
            let mut plain: Box<dyn Stream> = Box::new(tcp);
            greet(&mut plain)?;
            smtp_cmd(&mut plain, "EHLO pagurus", 250)?;
            smtp_cmd(&mut plain, "STARTTLS", 220)?; // refuse to go on unencrypted
            let mut s = super::tls(plain, &self.c.smtp_host)?;
            smtp_send(&mut s, &env, data)
        }
    }
}

fn greet<S: Read>(s: &mut S) -> Result<(), String> {
    match smtp_reply(s)? {
        (220, _) => Ok(()),
        (c, t) => Err(format!("smtp: {c} {}", crate::util::one_line(&t, 120))),
    }
}

impl Channel for Email {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn receive(&self) -> Result<Vec<Incoming>, String> {
        let last = self.last_poll.lock().map(|l| *l).unwrap_or(0);
        let wait = (last + self.c.poll_seconds.max(10)).saturating_sub(crate::util::now());
        std::thread::sleep(Duration::from_secs(wait.min(600)));
        if let Ok(mut l) = self.last_poll.lock() {
            *l = crate::util::now();
        }
        self.poll_imap().map_err(|e| crate::util::redact(&e))
    }

    fn deliver(&self, to: &str, text: &str, _keys: Option<&Value>) -> bool {
        let to = address(to);
        if !self.allowed.contains(&to) {
            eprintln!("pagurus: not emailing {to}: not in allowed_senders");
            return true; // never retried either
        }
        let th = self.threads.lock().ok().and_then(|t| t.get(&to).cloned()).unwrap_or_default();
        let data = compose(&self.c.address, &to, &th, &super::clip_in(text), crate::util::now());
        match self.send_mail(&to, &data) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("pagurus: email send failed: {}", crate::util::redact(&e));
                false
            }
        }
    }

    fn owner(&self) -> String {
        self.allowed[0].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A scripted server on 127.0.0.1: for each line it reads, the reply
    /// whose key the line starts with. Returns its address and what it read.
    fn fake(greeting: &'static str, script: Vec<(&'static str, String)>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            w.write_all(greeting.as_bytes()).unwrap();
            let mut seen = Vec::new();
            let mut data = false;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim_end().to_string();
                seen.push(line.clone());
                if data {
                    if line == "." {
                        data = false;
                        w.write_all(b"250 queued\r\n").unwrap();
                    }
                    continue;
                }
                let key = line.split_whitespace().nth(if greeting.starts_with("* ") { 1 } else { 0 }).unwrap_or("").to_string();
                let Some((_, reply)) = script.iter().find(|(k, _)| line.contains(k) || key == *k) else {
                    w.write_all(b"500 unexpected\r\n").unwrap();
                    continue;
                };
                let tag = line.split_whitespace().next().unwrap_or("");
                w.write_all(reply.replace("TAG", tag).as_bytes()).unwrap();
                if line == "DATA" {
                    data = true;
                }
                if line == "QUIT" || line.ends_with("LOGOUT") {
                    break;
                }
            }
            seen
        });
        (port, h)
    }

    #[test]
    fn imap_login_search_fetch() {
        let mail = "From: Me <me@x.org>\r\nSubject: hi\r\n\r\nhello\r\n";
        let (port, h) = fake(
            "* OK ready\r\n",
            vec![
                ("LOGIN", "TAG OK logged in\r\n".into()),
                ("SELECT", "* 3 EXISTS\r\nTAG OK [READ-WRITE] done\r\n".into()),
                ("SEARCH", "* SEARCH 7 9\r\nTAG OK done\r\n".into()),
                ("FETCH", format!("* 1 FETCH (UID 7 BODY[]<0> {{{}}}\r\n{mail})\r\nTAG OK done\r\n", mail.len())),
                ("STORE", "TAG OK done\r\n".into()),
                ("LOGOUT", "* BYE\r\nTAG OK bye\r\n".into()),
            ],
        );
        let mut i = Imap::new(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();
        i.login("me@x.org", "p\"w").unwrap();
        i.cmd("SELECT \"INBOX\"").unwrap();
        assert_eq!(i.search("16-Nov-2026", &["a@x.org".into(), "b@y.org".into()]).unwrap(), vec![7, 9]);
        let raw = i.fetch(7).unwrap();
        assert_eq!(parse_mail(&raw).text, "hello");
        i.seen(7).unwrap();
        i.cmd("LOGOUT").unwrap();
        let seen = h.join().unwrap();
        assert_eq!(seen[0], "h1 LOGIN \"me@x.org\" \"p\\\"w\"", "quoted");
        assert_eq!(seen[2], "h3 UID SEARCH UNSEEN SINCE 16-Nov-2026 OR FROM \"a@x.org\" FROM \"b@y.org\"");
        assert!(seen[3].contains("BODY.PEEK[]<0.262144>"), "peek: not marked seen until accepted");
        assert!(quote("a\r\nb").is_err(), "no command injection");
    }

    #[test]
    fn imap_errors_never_echo_the_password() {
        let (port, h) = fake("* OK ready\r\n", vec![("LOGIN", "TAG NO [AUTHENTICATIONFAILED] invalid\r\n".into())]);
        let mut i = Imap::new(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();
        let e = i.login("me", "s3cret-pass").unwrap_err();
        assert!(!e.contains("s3cret") && e.contains("login failed"));
        drop(i);
        h.join().unwrap();
    }

    #[test]
    fn smtp_dialog_with_auth_plain_and_dot_stuffing() {
        let (port, h) = fake(
            "220 fake ESMTP\r\n",
            vec![
                ("EHLO", "250-fake\r\n250-AUTH LOGIN PLAIN\r\n250 8BITMIME\r\n".into()),
                ("AUTH", "235 ok\r\n".into()),
                ("MAIL", "250 ok\r\n".into()),
                ("RCPT", "250 ok\r\n".into()),
                ("DATA", "354 go\r\n".into()),
                ("QUIT", "221 bye\r\n".into()),
            ],
        );
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        greet(&mut s).unwrap();
        let env = Envelope { user: "me@x.org", pass: "pw", from: "me@x.org", to: "you@y.org" };
        smtp_send(&mut s, &env, "Subject: t\r\n\r\n.hidden\r\nok").unwrap();
        let seen = h.join().unwrap();
        assert_eq!(seen[1], format!("AUTH PLAIN {}", crate::util::base64(b"\0me@x.org\0pw")));
        assert_eq!(seen[2], "MAIL FROM:<me@x.org>");
        assert!(seen.contains(&"..hidden".to_string()), "dot-stuffed");
        assert_eq!(seen.last().unwrap(), "QUIT");
    }

    #[test]
    fn smtp_auth_login_and_rejection() {
        let (port, h) = fake(
            "220 fake\r\n",
            vec![("EHLO", "250-fake\r\n250 AUTH LOGIN\r\n".into()), ("AUTH LOGIN", "334 VXNlcm5hbWU6\r\n".into()), ("bWU=", "334 UGFzc3dvcmQ6\r\n".into()), ("cHc=", "535 bad credentials\r\n".into())],
        );
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        greet(&mut s).unwrap();
        let env = Envelope { user: "me", pass: "pw", from: "me@x.org", to: "you@y.org" };
        let e = smtp_send(&mut s, &env, "x").unwrap_err();
        assert!(e.contains("535") && !e.contains("cHc="), "{e}");
        drop(s);
        h.join().unwrap();
    }

    #[test]
    fn parses_mime_threads_and_spots_auto_replies() {
        let raw = b"Return-Path: <me@x.org>\r\nAuthentication-Results: mx.example; dkim=pass header.d=x.org; spf=pass smtp.mailfrom=me@x.org; dmarc=pass\r\n\
From: \"Me\" <Me@X.org>\r\nSubject: =?utf-8?B?R3LDvMOfZQ==?= =?utf-8?Q?_again?=\r\nMessage-ID: <m2@x.org>\r\nReferences: <m0@x.org>\r\n <m1@x.org>\r\n\
Content-Type: multipart/alternative; boundary=\"b1\"\r\n\r\npreamble\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
Is the disk f=C3=BCll?\r\n\r\nOn Mon, Nov 16, 2026 pagurus wrote:\r\n> old\r\n--b1\r\nContent-Type: text/html\r\n\r\n<p>Is the disk</p>\r\n--b1--\r\n";
        let m = parse_mail(raw);
        assert_eq!(m.from, "me@x.org");
        assert_eq!(m.subject, "Grüße again");
        assert_eq!(m.text, "Is the disk füll?");
        assert_eq!(m.references, "<m0@x.org> <m1@x.org>");
        assert!(m.authentic && !m.auto);
        let html = parse_mail(b"From: me@x.org\r\nContent-Type: text/html\r\n\r\n<script>x</script>");
        assert_eq!(html.text, "", "HTML is never used");
        for h in ["Auto-Submitted: auto-replied", "Precedence: bulk", "List-Id: <l.x.org>", "Return-Path: <>"] {
            assert!(parse_mail(format!("From: me@x.org\r\n{h}\r\n\r\nout of office").as_bytes()).auto, "{h}");
        }
        assert!(parse_mail(b"From: MAILER-DAEMON@x.org\r\n\r\nbounce").auto);
        // a spoofed From: the receiving server vouches only for evil.example
        let spoof = parse_mail(b"Authentication-Results: mx; spf=pass smtp.mailfrom=a@evil.example; dkim=pass header.d=evil.example; dmarc=fail\r\nFrom: me@x.org\r\n\r\n/yes");
        assert!(!spoof.authentic);
        assert!(!parse_mail(b"From: me@x.org\r\n\r\nno auth header").authentic);
        let b64 = parse_mail(b"From: me@x.org\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\naGVs\r\nbG8=\r\n");
        assert_eq!(b64.text, "hello");
    }

    #[test]
    fn replies_stay_in_the_thread() {
        let th = Thread { subject: "disk".into(), message_id: "<m2@x.org>".into(), references: "<m1@x.org>".into() };
        let m = compose("pagurus@x.org", "me@x.org", &th, "all fine\n.", 1_795_000_000);
        assert!(m.contains("\r\nSubject: Re: disk\r\n"));
        assert!(m.contains("\r\nIn-Reply-To: <m2@x.org>\r\nReferences: <m1@x.org> <m2@x.org>\r\n"));
        assert!(m.contains("Auto-Submitted: auto-replied") && m.contains("Content-Type: text/plain; charset=utf-8"));
        let body = m.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(String::from_utf8(unbase64(body.as_bytes())).unwrap(), "all fine\r\n.");
        let inj = Thread { subject: "x\r\nBcc: evil@e.org".into(), ..Default::default() };
        assert!(!compose("h@x.org", "me@x.org", &inj, "t", 0).contains("\r\nBcc:"), "no header injection");
        assert_eq!(date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(imap_date(1_795_000_000), "18-Nov-2026");
    }

    #[test]
    fn never_mails_strangers() {
        std::env::set_var("PAGURUS_TEST_MAIL_PW", "pw");
        let mut c = EmailCfg { imap_host: "i".into(), smtp_host: "s".into(), username: "h@x.org".into(), password_env: "PAGURUS_TEST_MAIL_PW".into(), ..Default::default() };
        assert!(Email::new(&c).is_err(), "empty allowlist");
        c.allowed_senders = vec!["Me <ME@x.org>".into()];
        let e = Email::new(&c).unwrap();
        assert_eq!(e.owner(), "me@x.org");
        assert!(e.deliver("stranger@evil.example", "hi", None), "dropped, not queued, and nothing sent");
    }
}

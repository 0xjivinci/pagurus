use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `$PAGURUS_HOME`, else `~/.pagurus`. Installs from before the rename keep
/// working: `$HERMIT_HOME` is honoured, and an existing `~/.hermit` is used
/// while there is no `~/.pagurus` yet.
pub fn home_dir() -> PathBuf {
    for var in ["PAGURUS_HOME", "HERMIT_HOME"] {
        if let Ok(h) = std::env::var(var) {
            if !h.is_empty() {
                return PathBuf::from(h);
            }
        }
    }
    let new = expand("~/.pagurus");
    let old = expand("~/.hermit");
    if !new.exists() && old.is_dir() {
        return old;
    }
    new
}

/// Settings from before the rename: every `HERMIT_X` also counts as `PAGURUS_X`
/// (unless that is set), so old env files and service units keep working.
pub fn alias_legacy_env() {
    let old: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k.starts_with("HERMIT_")).collect();
    for (k, v) in old {
        let new = format!("PAGURUS_{}", &k["HERMIT_".len()..]);
        if std::env::var_os(&new).is_none() {
            std::env::set_var(&new, v);
        }
        if let Ok(mut n) = SECRET_NAMES.lock() {
            if n.iter().any(|x| x == &k) {
                n.push(new);
            }
        }
    }
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => Path::new(&std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(rest),
        None => PathBuf::from(p),
    }
}

/// Load KEY=VALUE lines from $PAGURUS_HOME/env without overriding variables
/// already set, so secrets can live in one chmod-600 file for CLI and service alike.
pub fn load_env(home: &Path) {
    for line in read(&home.join("env")).lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"').trim_matches('\''));
            if let Ok(mut n) = SECRET_NAMES.lock() {
                n.push(k.to_string()); // everything in the env file is a secret, whatever its name
            }
            if !k.is_empty() && std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
    alias_legacy_env();
}

/// Names of variables that came from $PAGURUS_HOME/env.
static SECRET_NAMES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Values of pagurus's secret environment variables: everything from the env file
/// plus anything named like a key, token or password (12+ chars, so short
/// everyday words are never mangled).
fn secret_values() -> Vec<String> {
    let names = SECRET_NAMES.lock().map(|n| n.clone()).unwrap_or_default();
    let mut v: Vec<String> = std::env::vars()
        .filter(|(k, v)| {
            let k = k.to_uppercase();
            v.trim().len() >= 12
                && (names.iter().any(|n| n.eq_ignore_ascii_case(&k))
                    || ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"].iter().any(|p| k.contains(p)))
        })
        .map(|(_, v)| v.trim().to_string())
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.len())); // longest first, in case one contains another
    v
}

/// Whether a run of characters is a well-known credential: provider API keys,
/// GitHub/GitLab/Slack/AWS/Google/Hugging Face/Stripe tokens.
fn token_shaped(t: &str) -> bool {
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-", 20),
        ("ghp_", 30),
        ("gho_", 30),
        ("ghu_", 30),
        ("ghs_", 30),
        ("ghr_", 30),
        ("github_pat_", 30),
        ("glpat-", 20),
        ("xoxb-", 20),
        ("xoxp-", 20),
        ("xoxa-", 20),
        ("xoxr-", 20),
        ("xoxs-", 20),
        ("xapp-", 20),
        ("AIza", 35),
        ("hf_", 30),
        ("gsk_", 40),
        ("sk_live_", 20),
        ("rk_live_", 20),
        ("pk_live_", 20),
        ("AKIA", 20),
        ("ASIA", 20),
    ];
    PREFIXES.iter().any(|(p, n)| t.len() >= *n && t.starts_with(p) && t[p.len()..].chars().any(|c| c.is_ascii_digit()))
}

/// Replace pagurus's own secrets and anything shaped like a credential with
/// `[REDACTED]`. Applied to tool output, transcripts, provider requests,
/// error messages and Telegram replies.
pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for v in secret_values() {
        if out.contains(&v) {
            out = out.replace(&v, "[REDACTED]");
        }
    }
    // PEM private keys
    while let Some(i) = out.find("-----BEGIN") {
        let Some(k) = out[i..].find("PRIVATE KEY-----") else { break };
        let end = out[i + k..]
            .find("-----END")
            .and_then(|e| out[i + k + e + 8..].find("-----").map(|f| i + k + e + 8 + f + 5));
        out.replace_range(i..end.unwrap_or(out.len()), "[REDACTED PRIVATE KEY]");
    }
    let tok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut res = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find(tok) {
        res.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c| !tok(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        // Telegram bot tokens: 123456789:AA… (35 chars after the colon)
        let after = &rest[end..];
        let bot = word.len() >= 6
            && word.len() <= 12
            && word.bytes().all(|b| b.is_ascii_digit())
            && after.starts_with(':')
            && {
                let n = after[1..].find(|c| !tok(c)).unwrap_or(after.len() - 1);
                n >= 30
            };
        if bot {
            let n = 1 + after[1..].find(|c| !tok(c)).unwrap_or(after.len() - 1);
            res.push_str("[REDACTED]");
            rest = &rest[end + n..];
            continue;
        }
        res.push_str(if token_shaped(word) { "[REDACTED]" } else { word });
        rest = &rest[end..];
    }
    res.push_str(rest);
    res
}

/// Set KEY=VALUE in $PAGURUS_HOME/env (replacing an existing line), mode 600.
pub fn set_env_value(home: &Path, key: &str, value: &str) -> Result<(), String> {
    let p = home.join("env");
    let mut lines: Vec<String> =
        read(&p).lines().filter(|l| !l.trim_start().starts_with(&format!("{key}="))).map(String::from).collect();
    lines.push(format!("{key}={value}"));
    write_private(&p, &(lines.join("\n") + "\n"))
}

/// Like `write_atomic`, but the file is mode 600 from its first byte (writing
/// first and chmod-ing after left the keys world-readable for a moment, and
/// for good if the chmod failed).
pub fn write_private(p: &Path, s: &str) -> Result<(), String> {
    use std::io::Write;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("tmp~");
    let _ = std::fs::remove_file(&tmp); // a leftover could have other permissions (or be a symlink)
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(s.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Short, human-typeable id (6 hex chars). Callers check for collisions.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let x = (nanos ^ (N.fetch_add(1, Ordering::Relaxed) << 40) ^ std::process::id() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:06x}", x >> 40)
}

pub fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// Write via temp file + rename so a power cut on an SD card never leaves half a file.
pub fn write_atomic(p: &Path, s: &str) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("tmp~");
    std::fs::write(&tmp, s).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Truncate to `max` chars, noting how much was cut.
pub fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("… [{} more chars]", n - max));
    out
}

/// Keep the last `max` chars (for transcripts, where the end matters most).
pub fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!("[…]{}", s.chars().skip(n - max).collect::<String>())
}

/// "0.1.1-dev (uge, 31f947e, 2026-11-22)": the version, which build, and the
/// commit it was built from (see build.rs). Plain version where it's unknown.
pub fn version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let kind = if cfg!(feature = "art") { "uge" } else { "smol" };
        let parts: Vec<&str> =
            [kind, env!("PAGURUS_COMMIT"), env!("PAGURUS_DATE")].into_iter().filter(|s| !s.is_empty()).collect();
        format!("{} ({})", env!("CARGO_PKG_VERSION"), parts.join(", "))
    })
}

pub fn one_line(s: &str, max: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// Give freed memory back to the system. glibc keeps what a process once used
/// (a long-running daemon would stay at its peak); musl and macOS return it on their own.
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only returns free heap pages to the kernel; it touches no live allocation
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Decode standard base64 (padding optional); None on a bad character.
pub fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Standard base64 (no dependency for one small function).
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Largest image pagurus sends to a model (vision APIs cap around 5–20 MB; tokens grow with size).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// A data: URL for an image, after checking it really is PNG, JPEG, GIF or WebP.
pub fn image_data_url(bytes: &[u8]) -> Result<String, String> {
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err("not a PNG, JPEG, GIF or WebP image".into());
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("image is {} KB; the limit is {} KB", bytes.len() / 1024, MAX_IMAGE_BYTES / 1024));
    }
    Ok(format!("data:{mime};base64,{}", base64(bytes)))
}

/// Where learning comes from right now ("chat", "reflection", "tidy", …) and,
/// if the conversation read text from outside (a file, page, image or command
/// output), what that was. Learning after outside text waits for the user:
/// that text could be trying to plant instructions. One job runs at a time.
static ORIGIN: std::sync::Mutex<(String, String)> = std::sync::Mutex::new((String::new(), String::new()));

pub fn set_origin(origin: &str, taint: &str) {
    if let Ok(mut o) = ORIGIN.lock() {
        *o = (origin.to_string(), taint.to_string());
    }
}

/// Mark the current conversation as having read outside text (keeps the first source).
pub fn taint(source: &str) {
    if let Ok(mut o) = ORIGIN.lock() {
        if o.1.is_empty() {
            o.1 = one_line(source, 120);
        }
    }
}

/// (origin, taint source or "")
pub fn origin() -> (String, String) {
    ORIGIN.lock().map(|o| o.clone()).unwrap_or_default()
}

/// Set by `/stop`: the running turn stops after the current step and a
/// running shell command is killed. Only one turn runs at a time.
pub static CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn cancelled() -> bool {
    CANCEL.load(std::sync::atomic::Ordering::Relaxed)
}

/// UTC as ISO 8601, e.g. `2026-11-19T03:30:55Z` (for APIs).
#[allow(dead_code)] // used by optional features
pub fn fmt_utc_iso(secs: u64) -> String {
    let (y, m, d) = civil((secs / 86_400) as i64);
    let t = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, (t % 3600) / 60, t % 60)
}

/// Local wall-clock time with its UTC offset, e.g. `2026-11-15 09:30 +01:00`.
pub fn fmt_time(secs: u64) -> String {
    let off = utc_offset();
    let sign = if off < 0 { '-' } else { '+' };
    format!("{} {sign}{:02}:{:02}", fmt_local(secs), off.abs() / 3600, (off.abs() % 3600) / 60)
}

/// `YYYY-MM-DD HH:MM` in local time, no offset (the format tasks are written in).
pub fn fmt_local(secs: u64) -> String {
    let t = (secs as i64 + utc_offset()).max(0) as u64;
    let (y, m, d) = civil((t / 86_400) as i64);
    let rem = t % 86_400;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Parse `YYYY-MM-DD HH:MM` (local time) to unix seconds.
pub fn parse_local(s: &str) -> Option<u64> {
    let s = s.trim().replace('T', " ");
    let (date, time) = s.split_once(' ').unwrap_or((s.as_str(), "00:00"));
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.trim().split(':').map(|x| x.parse::<i64>());
    let (h, mi) = (t.next()?.ok()?, t.next().unwrap_or(Ok(0)).ok()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || !(0..24).contains(&h) || !(0..60).contains(&mi) {
        return None;
    }
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 - utc_offset();
    u64::try_from(secs).ok()
}

/// Seconds east of UTC for the machine's local zone, from `date +%z` (works
/// with GNU, BSD and busybox date). Cached; 0 if unavailable.
pub fn utc_offset() -> i64 {
    static OFF: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        #[cfg(not(windows))]
        let out = std::process::Command::new("date").arg("+%z").output().ok();
        #[cfg(windows)]
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "(Get-Date).ToString('zzz')"])
            .output()
            .ok();
        // "+0100" (date) or "+01:00" (PowerShell)
        let z = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().replace(':', "")).unwrap_or_default();
        let (sign, digits) = match z.as_bytes().first() {
            Some(b'-') => (-1, &z[1..]),
            Some(b'+') => (1, &z[1..]),
            _ => return 0,
        };
        if digits.len() != 4 {
            return 0;
        }
        let h: i64 = digits[..2].parse().unwrap_or(0);
        let m: i64 = digits[2..].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    })
}

// Inverse of `civil` (Howard Hinnant).
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

// Howard Hinnant's days-to-civil; avoids a date crate.
pub(crate) fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe as i64 + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Resident memory of this process in KiB (Linux only).
pub fn rss_kb() -> Option<u64> {
    read(Path::new("/proc/self/status"))
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates() {
        // shown in local time; the UTC date math is checked via a zone-free round trip
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(1_794_614_400 / 86_400), (2026, 11, 14));
        assert_eq!(days_from_civil(2026, 11, 14), 1_794_614_400 / 86_400);
        assert!(fmt_time(1_794_614_400).starts_with("2026-11-1"));
    }
    #[test]
    fn env_file_is_private_and_its_values_are_secrets() {
        let home = std::env::temp_dir().join(format!("pagurus-envf-{}", new_id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("env"), "OLD=1\n").unwrap(); // a pre-existing world-readable file
        set_env_value(&home, "PAGURUS_TEST_ODDNAME", "value-without-a-secret-looking-name").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.join("env")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(read(&home.join("env")), "OLD=1\nPAGURUS_TEST_ODDNAME=value-without-a-secret-looking-name\n");
        load_env(&home);
        assert_eq!(redact("x value-without-a-secret-looking-name y"), "x [REDACTED] y");
    }

    #[test]
    fn secrets_are_redacted() {
        std::env::set_var("PAGURUS_TEST_API_KEY", "abcdefghijklmnopqrstuv");
        // fake, test-only values, split so no complete key-shaped string sits in the source
        let s = concat!(
            "key=abcdefghijklmnopqrstuv openai sk",
            "-proj-Abc123def456ghi789jkl gh gh",
            "p_0123456789ZYXWVUTSRQPONMLKJIHGFEDCBA ",
            "bot 123456789:A",
            "AFakeFakeFakeFakeFakeFakeFakeFake12/getMe aws AKI",
            "AFAKE0EXAMPLE00000 ok sk-learn plain words 12:30"
        );
        let r = redact(s);
        for leaked in ["abcdefghijklmnopqrstuv", "sk-proj-Abc", "ghp_0123", "AAFakeFa", "AKIAFAKE"] {
            assert!(!r.contains(leaked), "{leaked} leaked: {r}");
        }
        assert!(
            r.contains("/getMe") && r.contains("sk-learn plain words 12:30") && r.starts_with("key=[REDACTED]"),
            "{r}"
        );
        let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----\nz";
        assert_eq!(redact(pem), "a\n[REDACTED PRIVATE KEY]\nz");
        assert_eq!(redact("-----BEGIN RSA PRIVATE KEY-----\nMIIE"), "[REDACTED PRIVATE KEY]");
        assert_eq!(redact("héllo wörld — fine"), "héllo wörld — fine");
    }

    #[test]
    fn clipping() {
        assert_eq!(clip("héllo", 10), "héllo");
        assert!(clip("héllo world", 3).starts_with("hél…"));
        assert_eq!(tail("abcdef", 3), "[…]def");
    }
}

#[cfg(test)]
mod time_tests {
    #[test]
    fn local_time_round_trips() {
        let t = super::parse_local("2026-11-16 09:05").unwrap();
        assert_eq!(super::fmt_local(t), "2026-11-16 09:05");
        assert_eq!(super::fmt_local(super::parse_local("2024-02-29 23:59").unwrap()), "2024-02-29 23:59");
        assert!(super::parse_local("tomorrow").is_none());
        assert!(super::parse_local("2026-13-01 10:00").is_none());
    }

    #[test]
    fn settings_from_before_the_rename_still_count() {
        std::env::set_var("HERMIT_TEST_LEGACY_ALIAS", "old-value");
        std::env::set_var("HERMIT_TEST_LEGACY_KEEP", "old");
        std::env::set_var("PAGURUS_TEST_LEGACY_KEEP", "new");
        super::alias_legacy_env();
        assert_eq!(std::env::var("PAGURUS_TEST_LEGACY_ALIAS").unwrap(), "old-value");
        assert_eq!(std::env::var("PAGURUS_TEST_LEGACY_KEEP").unwrap(), "new"); // a new name always wins
    }

    #[test]
    fn base64_and_image_sniffing() {
        assert_eq!(super::base64(b"pagurus"), "cGFndXJ1cw==");
        assert_eq!(super::base64(b"ab"), "YWI=");
        assert_eq!(super::base64(b"a"), "YQ==");
        assert!(super::image_data_url(b"\x89PNG\r\n\x1a\nrest").unwrap().starts_with("data:image/png;base64,"));
        assert!(super::image_data_url(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap().starts_with("data:image/jpeg"));
        assert!(super::image_data_url(b"#!/bin/sh").is_err());
    }
}

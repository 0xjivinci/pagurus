//! Browser automation (uge): drive a headless Chromium (or Chrome / Brave /
//! Edge) over the DevTools protocol, for pages that need JavaScript, clicking,
//! typing, screenshots and PDFs. `web_fetch` stays the cheap way to read a page.
//!
//! RAM: the browser starts on first use and is closed after IDLE_SECS without
//! use, so a Pi only pays for it during a task.
//!
//! Safety:
//! - a fresh, empty profile each time: no cookies, logins or history of the user's own browser
//! - only http(s) pages; cloud metadata addresses can't be resolved; downloads are denied
//! - page text is outside text (tools.rs marks it and checks it for injections)
//! - typing sends data to a site, so it asks the user (tools.rs) and refuses pagurus's own secrets

use crate::channel::ws::{Msg, Ws};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Close the browser after this long without use.
const IDLE_SECS: u64 = 180;
/// One command may take this long (a slow page load on a Pi).
const CALL_SECS: u64 = 45;
/// Page text returned to the model.
const TEXT_CHARS: usize = 8000;

struct Browser {
    child: Child,
    profile: PathBuf,
    ws: Ws<TcpStream>,
    session: String,
    next: u64,
    used: Instant,
}

static B: Mutex<Option<Browser>> = Mutex::new(None);

/// A Chromium-family browser: `tools.browser` when it's a path, else the first one found.
pub fn find(setting: &str) -> Option<PathBuf> {
    let s = setting.trim();
    if !matches!(s, "" | "auto" | "off") {
        let p = crate::util::expand(s);
        return p.is_file().then_some(p);
    }
    #[cfg(target_os = "macos")]
    for app in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ] {
        if Path::new(app).is_file() {
            return Some(app.into());
        }
    }
    let path = std::env::var_os("PATH")?;
    for name in ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "brave-browser", "microsoft-edge"] {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn launch(home: &Path, setting: &str) -> Result<Browser, String> {
    let exe = find(setting).ok_or(
        "no Chromium-family browser found (chromium, google-chrome, brave); install one or set tools.browser to its path",
    )?;
    clean_stale(home);
    let profile = home.join("cache").join(format!("browser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args([
        "--headless=new",
        "--remote-debugging-port=0",
        "--remote-debugging-address=127.0.0.1",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-sync",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-gpu",
        "--renderer-process-limit=2",
        "--mute-audio",
        "--window-size=1280,900",
        // instance credentials must stay out of reach, whatever a page links to
        "--host-resolver-rules=MAP 169.254.169.254 ~NOTFOUND, MAP metadata.google.internal ~NOTFOUND, MAP 100.100.100.200 ~NOTFOUND",
    ])
    .arg(format!("--user-data-dir={}", profile.display()))
    .arg("about:blank")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    crate::tools::clean_env(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let _ = std::fs::write(profile.join("pagurus-browser.pid"), child.id().to_string());
    // Chromium writes its port and browser path once it listens
    let port_file = profile.join("DevToolsActivePort");
    let t0 = Instant::now();
    let (port, path) = loop {
        let s = crate::util::read(&port_file);
        let mut l = s.lines();
        if let (Some(p), Some(path)) = (l.next().and_then(|p| p.trim().parse::<u16>().ok()), l.next()) {
            break (p, path.trim().to_string());
        }
        if t0.elapsed() > Duration::from_secs(30) || child.try_wait().ok().flatten().is_some() {
            kill(&mut child, &profile);
            return Err(format!("{} didn't start a debugging port", exe.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let sock = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("browser: {e}"))?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let ws = Ws::handshake(sock, &format!("127.0.0.1:{port}"), &path)?;
    let mut b = Browser { child, profile, ws, session: String::new(), next: 0, used: Instant::now() };
    let _ = b.call_browser("Browser.setDownloadBehavior", json!({ "behavior": "deny" }));
    let target = b.call_browser("Target.createTarget", json!({ "url": "about:blank" }))?;
    let id = target["targetId"].as_str().ok_or("browser: no target")?.to_string();
    let att = b.call_browser("Target.attachToTarget", json!({ "targetId": id, "flatten": true }))?;
    b.session = att["sessionId"].as_str().ok_or("browser: no session")?.to_string();
    b.call("Page.enable", json!({}))?;
    Ok(b)
}

fn kill(child: &mut Child, profile: &Path) {
    #[cfg(unix)]
    crate::tools::kill_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(profile);
}

/// Browsers left behind by a pagurus that ended without closing them.
fn clean_stale(home: &Path) {
    let Ok(rd) = std::fs::read_dir(home.join("cache")) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(owner) = name.strip_prefix("browser-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if owner == std::process::id() || alive(owner) {
            continue;
        }
        if let Ok(pid) = crate::util::read(&e.path().join("pagurus-browser.pid")).trim().parse::<u32>() {
            #[cfg(unix)]
            crate::tools::kill_tree(pid);
        }
        let _ = std::fs::remove_dir_all(e.path());
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    true
}

impl Browser {
    fn send(&mut self, method: &str, params: Value, session: bool) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if session {
            msg["sessionId"] = json!(self.session);
        }
        self.ws.send_text(&msg.to_string()).map_err(|e| format!("browser: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(CALL_SECS);
        loop {
            if crate::util::cancelled() {
                return Err("stopped by the user".into());
            }
            match self.ws.read().map_err(|e| format!("browser: {e}"))? {
                Some(Msg::Text(t)) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    if v["id"].as_u64() == Some(id) {
                        self.used = Instant::now();
                        if let Some(e) = v.get("error") {
                            return Err(format!("browser: {}", e["message"].as_str().unwrap_or("error")));
                        }
                        return Ok(v["result"].clone());
                    }
                }
                Some(Msg::Close(_)) => return Err("browser: the browser closed".into()),
                None => {}
            }
            if Instant::now() > deadline {
                return Err(format!("browser: {method} took over {CALL_SECS}s"));
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.send(method, params, true)
    }

    fn call_browser(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.send(method, params, false)
    }

    /// Run JavaScript in the page and return its (JSON) value.
    fn eval(&mut self, js: &str) -> Result<Value, String> {
        let r = self.call("Runtime.evaluate", json!({ "expression": js, "returnByValue": true, "awaitPromise": true }))?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!("browser: script error: {}", ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("?")));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Wait until the page has loaded (or up to `secs`), then a moment for scripts to draw.
    fn settle(&mut self, secs: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if self.eval("document.readyState").ok().and_then(|v| v.as_str().map(String::from)).as_deref() == Some("complete") {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        std::thread::sleep(Duration::from_millis(600));
    }

    /// Title, address, visible text and the numbered things one can click or type into.
    fn page(&mut self, text_chars: usize) -> Result<String, String> {
        let v = self.eval(&PAGE_JS.replace("LIMIT", &text_chars.to_string()))?;
        let v: Value = serde_json::from_str(v.as_str().unwrap_or("{}")).unwrap_or_default();
        let els: Vec<String> = v["els"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect();
        Ok(format!(
            "Page: {}\nURL: {}\n\n{}\n\nElements (click or type by number):\n{}",
            v["title"].as_str().unwrap_or(""),
            v["url"].as_str().unwrap_or(""),
            v["text"].as_str().unwrap_or("").trim(),
            if els.is_empty() { "(none)".to_string() } else { els.join("\n") }
        ))
    }

    fn url(&mut self) -> String {
        self.eval("location.href").ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }
}

/// Lists visible links, buttons and fields, numbers them (data-pagurus) and returns the page as JSON text.
const PAGE_JS: &str = r#"(() => {
  const sel = 'a[href],button,input:not([type=hidden]),textarea,select,[role=button],[role=link],[role=tab],[role=checkbox],[contenteditable=true],summary';
  const vis = e => { const r = e.getBoundingClientRect(), s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility != 'hidden' && s.display != 'none'; };
  document.querySelectorAll('[data-pagurus]').forEach(e => e.removeAttribute('data-pagurus'));
  const els = [...document.querySelectorAll(sel)].filter(vis).slice(0, 80);
  const out = els.map((e, i) => {
    e.setAttribute('data-pagurus', i + 1);
    const tag = e.tagName.toLowerCase();
    const kind = tag == 'input' ? '[' + (e.type || 'text') + ']' : '';
    const label = (e.innerText || e.getAttribute('aria-label') || e.placeholder || e.title || e.name || e.alt || '').trim().replace(/\s+/g, ' ').slice(0, 70);
    const val = (tag == 'input' || tag == 'textarea') && e.type != 'password' && e.value ? ' = "' + e.value.slice(0, 40) + '"' : '';
    const href = tag == 'a' ? ' -> ' + (e.getAttribute('href') || '').slice(0, 80) : '';
    return '[' + (i + 1) + '] ' + tag + kind + (label ? ' "' + label + '"' : '') + val + href;
  });
  const text = (document.body ? document.body.innerText : '').replace(/\n\s*\n\s*\n+/g, '\n\n').slice(0, LIMIT);
  return JSON.stringify({ title: document.title, url: location.href, text, els: out });
})()"#;

/// Finds an element by its number or its visible text; `then` runs with it as `e`.
fn element_js(target: &str, then: &str) -> String {
    let t = serde_json::to_string(target.trim()).unwrap_or_default();
    format!(
        r#"(() => {{
  const t = {t};
  let e = /^\d+$/.test(t) ? document.querySelector('[data-pagurus="' + t + '"]') : null;
  if (!e) {{
    const all = [...document.querySelectorAll('[data-pagurus]')];
    const low = t.toLowerCase();
    e = all.find(x => (x.innerText || x.getAttribute('aria-label') || x.placeholder || x.value || '').trim().toLowerCase() == low)
      || all.find(x => (x.innerText || x.getAttribute('aria-label') || x.placeholder || '').toLowerCase().includes(low));
  }}
  if (!e) return 'not found';
  e.scrollIntoView({{ block: 'center' }});
  {then}
}})()"#
    )
}

/// The page address must be http(s), carry no secret, and not resolve only to a blocked address.
fn check_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err("only http(s) pages".into());
    }
    if crate::util::redact(u) != u {
        return Err("refused: the URL contains what looks like a key or token".into());
    }
    let host = u.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
    let netloc = if host.contains(':') && !host.ends_with(']') { host.to_string() } else { format!("{host}:{}", if u.starts_with("https") { 443 } else { 80 }) };
    crate::tools::guarded_resolve(&netloc).map(|_| ()).map_err(|e| format!("refused: {e}"))
}

/// Run one browser action. `approve` asks the user before typing (tools.rs passes the approval).
pub fn run(home: &Path, setting: &str, a: &Value, approve: &dyn Fn(&str) -> Result<(), String>) -> Result<String, String> {
    let action = a["action"].as_str().unwrap_or("");
    let arg = |k: &str| a[k].as_str().unwrap_or("").to_string();
    if action == "close" {
        close();
        return Ok("browser closed".into());
    }
    if setting.trim() == "off" {
        return Err("the browser is off (tools.browser in config.toml)".into());
    }
    let mut guard = B.lock().map_err(|_| "browser busy")?;
    if guard.as_mut().is_some_and(|b| b.child.try_wait().ok().flatten().is_some()) {
        if let Some(mut old) = guard.take() {
            kill(&mut old.child, &old.profile);
        }
    }
    if guard.is_none() {
        if action != "open" {
            return Err("no page open: use action=open with a url first".into());
        }
        *guard = Some(launch(home, setting)?);
        reaper();
    }
    let b = guard.as_mut().ok_or("browser not running")?;
    match action {
        "open" => {
            let url = arg("url");
            check_url(&url)?;
            let r = b.call("Page.navigate", json!({ "url": url.trim() }))?;
            if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Err(format!("couldn't open {url}: {e}"));
            }
            b.settle(15);
            b.page(TEXT_CHARS)
        }
        "read" => b.page(TEXT_CHARS * 2),
        "click" => {
            let target = arg("target");
            let r = b.eval(&element_js(&target, "if (e.tagName == 'A') e.target = '_self'; e.click(); return 'ok';"))?;
            if r.as_str() == Some("not found") {
                return Err(format!("no element `{target}` on this page; use action=read to see the numbered list"));
            }
            b.settle(10);
            check_url(&b.url()).map_err(|e| format!("the click led somewhere pagurus won't go ({e})"))?;
            b.page(TEXT_CHARS)
        }
        "type" => {
            let (target, text) = (arg("target"), arg("text"));
            if crate::util::redact(&text) != text {
                return Err("refused: that text looks like one of pagurus's own keys or tokens".into());
            }
            let host = b.url().split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("").to_string();
            approve(&format!("type {:?} into `{target}` on {host}{}", crate::util::clip(&text, 120), if a["enter"].as_bool() == Some(true) { " and press Enter" } else { "" }))?;
            let r = b.eval(&element_js(&target, "e.focus(); if ('value' in e) { e.value = ''; e.dispatchEvent(new Event('input', { bubbles: true })); } return 'ok';"))?;
            if r.as_str() == Some("not found") {
                return Err(format!("no element `{target}` on this page; use action=read to see the numbered list"));
            }
            b.call("Input.insertText", json!({ "text": text }))?;
            if a["enter"].as_bool() == Some(true) {
                for kind in ["keyDown", "keyUp"] {
                    b.call("Input.dispatchKeyEvent", json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" }))?;
                }
                b.settle(10);
            }
            b.page(TEXT_CHARS / 2)
        }
        "scroll" => {
            let dy = if arg("direction") == "up" { -700 } else { 700 };
            b.eval(&format!("window.scrollBy(0, {dy}); 'ok'"))?;
            std::thread::sleep(Duration::from_millis(400));
            b.page(TEXT_CHARS / 2)
        }
        "back" => {
            b.eval("history.back(); 'ok'")?;
            b.settle(10);
            b.page(TEXT_CHARS)
        }
        "screenshot" => {
            let r = b.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 75 }))?;
            let data = r["data"].as_str().ok_or("browser: no screenshot")?;
            let bytes = crate::util::unbase64(data).ok_or("browser: bad screenshot data")?;
            let file = save(home, &b.url(), "jpg", &bytes)?;
            crate::tools::show_image(format!("screenshot of {}", b.url()), format!("data:image/jpeg;base64,{data}"));
            Ok(format!(
                "Screenshot attached below for you to look at, saved as {}. To give it to the user, send_file that path.",
                file.display()
            ))
        }
        "pdf" => {
            let r = b.call("Page.printToPDF", json!({ "printBackground": true, "transferMode": "ReturnAsStream" }))?;
            let stream = r["stream"].as_str().ok_or("browser: no PDF stream")?.to_string();
            let mut bytes = Vec::new();
            loop {
                let c = b.call("IO.read", json!({ "handle": stream, "size": 1 << 20 }))?;
                let chunk = c["data"].as_str().unwrap_or("");
                let part = if c["base64Encoded"].as_bool() == Some(true) { crate::util::unbase64(chunk).ok_or("browser: bad PDF data")? } else { chunk.as_bytes().to_vec() };
                bytes.extend_from_slice(&part);
                if c["eof"].as_bool() == Some(true) || bytes.len() > 50 << 20 {
                    break;
                }
            }
            let _ = b.call("IO.close", json!({ "handle": stream }));
            let file = save(home, &b.url(), "pdf", &bytes)?;
            Ok(format!("Saved the page as a PDF ({} KB): {}. To give it to the user, send_file that path.", bytes.len() / 1024, file.display()))
        }
        other => Err(format!("browser action `{other}`: use open, read, click, type, scroll, back, screenshot, pdf or close")),
    }
}

/// A file in workspace/browser named after the page's host.
fn save(home: &Path, url: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let host: String = url.split("://").nth(1).unwrap_or("page").split('/').next().unwrap_or("page").chars().map(|c| if c.is_alphanumeric() || c == '.' { c } else { '-' }).collect();
    let dir = home.join("workspace").join("browser");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let file = dir.join(format!("{host}-{stamp}.{ext}"));
    std::fs::write(&file, bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(file)
}

/// Close the browser now (also when pagurus exits normally).
pub fn close() {
    if let Ok(mut g) = B.lock() {
        if let Some(mut b) = g.take() {
            let _ = b.call_browser("Browser.close", json!({}));
            b.ws.close();
            kill(&mut b.child, &b.profile);
        }
    }
}

/// One background thread closes an idle browser.
fn reaper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("pagurus-browser-idle".into()).stack_size(64 * 1024).spawn(|| loop {
            std::thread::sleep(Duration::from_secs(15));
            let idle = B.try_lock().ok().and_then(|g| g.as_ref().map(|b| b.used.elapsed() > Duration::from_secs(IDLE_SECS))).unwrap_or(false);
            if idle {
                close();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_checked_before_the_browser_goes_there() {
        assert!(check_url("file:///etc/passwd").unwrap_err().contains("http"));
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("http://169.254.169.254/latest/meta-data/").unwrap_err().contains("refused"));
        assert!(check_url("http://127.0.0.1:8822/").is_ok(), "local services stay reachable, like web_fetch");
    }

    #[test]
    fn element_lookup_quotes_the_target_safely() {
        let js = element_js("Log in\"); alert(1); (\"", "return 'ok';");
        assert!(js.contains(r#"const t = "Log in\"); alert(1); (\"";"#), "{js}");
    }

    /// Drives a real browser against a local page: `cargo test --features uge -- --ignored browser`.
    #[test]
    #[ignore]
    fn browser_opens_clicks_types_and_screenshots() {
        let home = std::env::temp_dir().join(format!("pagurus-browser-{}", crate::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("GET /next") {
                    "<html><head><title>Next</title></head><body><h1>Second page</h1></body></html>".to_string()
                } else {
                    "<html><head><title>Hello</title></head><body><h1>Crab shop</h1><a href=\"/next\">Go next</a>\
                     <input id=q placeholder=\"Search shells\"><p id=out></p>\
                     <script>q.addEventListener('input',()=>out.textContent='typed: '+q.value)</script></body></html>"
                        .to_string()
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let yes = |_: &str| Ok(());
        let go = |a: Value| run(&home, "auto", &a, &yes);
        let page = go(json!({"action":"open","url":format!("http://127.0.0.1:{port}/")})).unwrap();
        assert!(page.contains("Crab shop") && page.contains("\"Go next\""), "{page}");
        let typed = go(json!({"action":"type","target":"Search shells","text":"hermit"})).unwrap();
        assert!(typed.contains("typed: hermit"), "{typed}");
        let shot = go(json!({"action":"screenshot"})).unwrap();
        assert!(shot.contains(".jpg"), "{shot}");
        let next = go(json!({"action":"click","target":"Go next"})).unwrap();
        assert!(next.contains("Second page"), "{next}");
        let pdf = go(json!({"action":"pdf"})).unwrap();
        assert!(pdf.contains(".pdf"), "{pdf}");
        close();
        assert!(!home.join(format!("cache/browser-{}", std::process::id())).exists(), "profile removed");
        let _ = std::fs::remove_dir_all(&home);
    }
}

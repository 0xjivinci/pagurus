//! Browser automation (uge): drive a headless browser over the DevTools
//! protocol (CDP), for pages that need JavaScript, clicking, typing,
//! screenshots and PDFs. `web_fetch` stays the cheap way to read a page.
//!
//! Engines: Lightpanda when it's installed (a small browser that runs pages
//! without drawing them: a fraction of Chromium's RAM and CPU), else Chromium /
//! Chrome / Brave / Edge. Lightpanda can't draw, so a screenshot or PDF starts
//! Chromium just for that capture and closes it again.
//!
//! Easy on a busy machine (a Pi running services and a kiosk): browsers run at
//! the lowest CPU priority, start on first use and close after IDLE_SECS idle.
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
const CALL_SECS: u64 = 60;
/// Page text returned to the model.
const TEXT_CHARS: usize = 8000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Engine {
    Chromium,
    Lightpanda,
}

fn engine_of(exe: &Path) -> Engine {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name.contains("lightpanda") {
        Engine::Lightpanda
    } else {
        Engine::Chromium
    }
}

struct Browser {
    engine: Engine,
    child: Child,
    profile: PathBuf,
    ws: Ws<TcpStream>,
    session: String,
    next: u64,
    used: Instant,
}

static B: Mutex<Option<Browser>> = Mutex::new(None);

/// The browser to use: `tools.browser` = a path, "lightpanda", "chromium", or
/// "auto" (Lightpanda if installed, else a Chromium-family browser).
pub fn find(home: &Path, setting: &str) -> Option<PathBuf> {
    match setting.trim() {
        "" | "auto" => find_lightpanda(home).or_else(find_chromium),
        "lightpanda" => find_lightpanda(home),
        "chromium" | "chrome" => find_chromium(),
        "off" => None,
        p => Some(crate::util::expand(p)).filter(|p| p.is_file()),
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

fn find_lightpanda(home: &Path) -> Option<PathBuf> {
    let user = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    on_path("lightpanda")
        .or_else(|| [user.join(".local/bin/lightpanda"), home.join("bin/lightpanda")].into_iter().find(|p| p.is_file()))
}

/// A browser that can draw (screenshots, PDFs).
fn find_chromium() -> Option<PathBuf> {
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
    ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "brave-browser", "microsoft-edge"]
        .into_iter()
        .find_map(on_path)
}

/// The command for `exe` at the lowest CPU priority, with a minimal environment.
fn low_priority(exe: &Path) -> Command {
    let nice = ["/usr/bin/nice", "/bin/nice"].into_iter().find(|p| Path::new(p).is_file());
    let mut cmd = match nice {
        Some(n) => {
            let mut c = Command::new(n);
            c.args(["-n", "19"]).arg(exe);
            c
        }
        None => Command::new(exe),
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    crate::tools::clean_env(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Start `exe` and attach to a fresh tab. `tag` keeps a second (picture) browser's files apart.
fn launch(home: &Path, exe: &Path, tag: &str) -> Result<Browser, String> {
    clean_stale(home);
    let profile = home.join("cache").join(format!("browser-{}{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let engine = engine_of(exe);
    let (mut child, port, path) = match engine {
        Engine::Lightpanda => start_lightpanda(exe, &profile)?,
        Engine::Chromium => start_chromium(exe, &profile)?,
    };
    // a slow machine (a Pi) can take seconds to answer the handshake: short reads only after it
    let ws = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("browser: {e}")).and_then(|sock| {
        let _ = sock.set_read_timeout(Some(Duration::from_secs(15)));
        let handle = sock.try_clone().map_err(|e| format!("browser: {e}"))?;
        let ws = Ws::handshake(sock, &format!("127.0.0.1:{port}"), &path)?;
        let _ = handle.set_read_timeout(Some(Duration::from_millis(500)));
        Ok(ws)
    });
    let ws = match ws {
        Ok(ws) => ws,
        Err(e) => {
            kill(&mut child, &profile);
            return Err(e);
        }
    };
    let mut b = Browser { engine, child, profile, ws, session: String::new(), next: 0, used: Instant::now() };
    b.attach()?; // on failure, dropping `b` stops the browser
    Ok(b)
}

/// Lightpanda serves CDP on a port we pick; no usage data is sent.
fn start_lightpanda(exe: &Path, profile: &Path) -> Result<(Child, u16, String), String> {
    let port =
        std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
    let mut cmd = low_priority(exe);
    cmd.args(["serve", "--host", "127.0.0.1", "--port", &port.to_string(), "--log-level", "err"])
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true");
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let _ = std::fs::write(profile.join("pagurus-browser.pid"), child.id().to_string());
    let t0 = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        if t0.elapsed() > Duration::from_secs(20) || child.try_wait().ok().flatten().is_some() {
            kill(&mut child, profile);
            return Err(format!("{} didn't start its CDP server", exe.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok((child, port, "/".into()))
}

/// Chromium picks its own port and writes it to DevToolsActivePort.
fn start_chromium(exe: &Path, profile: &Path) -> Result<(Child, u16, String), String> {
    let mut cmd = low_priority(exe);
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
    .arg("about:blank");
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let _ = std::fs::write(profile.join("pagurus-browser.pid"), child.id().to_string());
    let port_file = profile.join("DevToolsActivePort");
    let t0 = Instant::now();
    loop {
        let s = crate::util::read(&port_file);
        let mut l = s.lines();
        if let (Some(p), Some(path)) = (l.next().and_then(|p| p.trim().parse::<u16>().ok()), l.next()) {
            return Ok((child, p, path.trim().to_string()));
        }
        if t0.elapsed() > Duration::from_secs(45) || child.try_wait().ok().flatten().is_some() {
            kill(&mut child, profile);
            return Err(format!("{} didn't start a debugging port", exe.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.call_browser_quick("Browser.close");
        self.ws.close();
        kill(&mut self.child, &self.profile);
    }
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
        let Some(owner) = name.strip_prefix("browser-").and_then(|p| p.split('-').next()?.parse::<u32>().ok()) else {
            continue;
        };
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
    /// A tab to work in, with downloads off.
    fn attach(&mut self) -> Result<(), String> {
        let _ = self.call_browser("Browser.setDownloadBehavior", json!({ "behavior": "deny" }));
        let target = self.call_browser("Target.createTarget", json!({ "url": "about:blank" }))?;
        let id = target["targetId"].as_str().ok_or("browser: no target")?.to_string();
        let att = self.call_browser("Target.attachToTarget", json!({ "targetId": id, "flatten": true }))?;
        self.session = att["sessionId"].as_str().ok_or("browser: no session")?.to_string();
        self.call("Page.enable", json!({}))?;
        Ok(())
    }

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

    /// Fire and forget (closing): don't wait for an answer.
    fn call_browser_quick(&mut self, method: &str) -> std::io::Result<()> {
        self.next += 1;
        self.ws.send_text(&json!({ "id": self.next, "method": method, "params": {} }).to_string())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.send(method, params, true)
    }

    fn call_browser(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.send(method, params, false)
    }

    /// Run JavaScript in the page and return its (JSON) value.
    fn eval(&mut self, js: &str) -> Result<Value, String> {
        let r =
            self.call("Runtime.evaluate", json!({ "expression": js, "returnByValue": true, "awaitPromise": true }))?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!(
                "browser: script error: {}",
                ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("?")
            ));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Wait until the page has loaded (or up to `secs`), then a moment for scripts to draw.
    fn settle(&mut self, secs: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if self.eval("document.readyState").ok().and_then(|v| v.as_str().map(String::from)).as_deref()
                == Some("complete")
            {
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
        let els: Vec<String> =
            v["els"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect();
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
  const vis = e => { try { const r = e.getBoundingClientRect(), s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility != 'hidden' && s.display != 'none'; } catch (_) { return true; } };
  document.querySelectorAll('[data-pagurus]').forEach(e => e.removeAttribute('data-pagurus'));
  const all = [...document.querySelectorAll(sel)];
  let shown = all.filter(vis);
  // a browser without layout (Lightpanda) reports every box as empty: keep what isn't hidden
  if (!shown.length) shown = all.filter(e => !e.hidden && e.getAttribute('aria-hidden') != 'true');
  const els = shown.slice(0, 80);
  const out = els.map((e, i) => {
    e.setAttribute('data-pagurus', i + 1);
    const tag = e.tagName.toLowerCase();
    const kind = tag == 'input' ? '[' + (e.type || 'text') + ']' : '';
    const label = (e.innerText || e.textContent || e.getAttribute('aria-label') || e.placeholder || e.title || e.name || e.alt || '').trim().replace(/\s+/g, ' ').slice(0, 70);
    const val = (tag == 'input' || tag == 'textarea') && e.type != 'password' && e.value ? ' = "' + e.value.slice(0, 40) + '"' : '';
    const href = tag == 'a' ? ' -> ' + (e.getAttribute('href') || '').slice(0, 80) : '';
    return '[' + (i + 1) + '] ' + tag + kind + (label ? ' "' + label + '"' : '') + val + href;
  });
  const text = (document.body ? (document.body.innerText || document.body.textContent || '') : '').replace(/\n\s*\n\s*\n+/g, '\n\n').slice(0, LIMIT);
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
    const txt = x => (x.innerText || x.textContent || x.getAttribute('aria-label') || x.placeholder || x.value || '').trim().toLowerCase();
    e = all.find(x => txt(x) == low) || all.find(x => txt(x).includes(low));
  }}
  if (!e) return 'not found';
  if (e.scrollIntoView) e.scrollIntoView({{ block: 'center' }});
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
    let netloc = if host.contains(':') && !host.ends_with(']') {
        host.to_string()
    } else {
        format!("{host}:{}", if u.starts_with("https") { 443 } else { 80 })
    };
    crate::tools::guarded_resolve(&netloc).map(|_| ()).map_err(|e| format!("refused: {e}"))
}

/// Run one browser action. `approve` asks the user before typing (tools.rs passes the approval).
pub fn run(
    home: &Path,
    setting: &str,
    a: &Value,
    approve: &dyn Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
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
        *guard = None; // it died: dropping cleans up
    }
    if guard.is_none() {
        if action != "open" {
            return Err("no page open: use action=open with a url first".into());
        }
        let exe = find(home, setting).ok_or(
            "no browser found: install Lightpanda (light) or Chromium, or set tools.browser to a browser's path",
        )?;
        *guard = Some(launch(home, &exe, "")?);
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
            let r =
                b.eval(&element_js(&target, "if (e.tagName == 'A') e.target = '_self'; e.click(); return 'ok';"))?;
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
            approve(&format!(
                "type {:?} into `{target}` on {host}{}",
                crate::util::clip(&text, 120),
                if a["enter"].as_bool() == Some(true) { " and press Enter" } else { "" }
            ))?;
            let r = b.eval(&element_js(&target, "e.focus(); if ('value' in e) { e.value = ''; e.dispatchEvent(new Event('input', { bubbles: true })); } return 'ok';"))?;
            if r.as_str() == Some("not found") {
                return Err(format!("no element `{target}` on this page; use action=read to see the numbered list"));
            }
            // real key input where the browser has it; setting the value (with the events
            // a page listens for) where it doesn't
            if b.call("Input.insertText", json!({ "text": text })).is_err() {
                let js = format!(
                    "(() => {{ const e = document.activeElement; if (!e) return 'no field'; e.value = {}; \
                     e.dispatchEvent(new Event('input', {{ bubbles: true }})); e.dispatchEvent(new Event('change', {{ bubbles: true }})); return 'ok'; }})()",
                    serde_json::to_string(&text).unwrap_or_default()
                );
                b.eval(&js)?;
            }
            if a["enter"].as_bool() == Some(true) {
                let keys = ["keyDown", "keyUp"].iter().try_for_each(|kind| {
                    b.call("Input.dispatchKeyEvent", json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" })).map(|_| ())
                });
                if keys.is_err() {
                    b.eval("(() => { const f = document.activeElement && document.activeElement.form; if (f) { f.requestSubmit ? f.requestSubmit() : f.submit(); } return 'ok'; })()")?;
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
        "screenshot" | "pdf" => {
            if b.engine == Engine::Chromium {
                return picture(home, b, action);
            }
            // Lightpanda can't draw: a Chromium opens the same address just for this, then closes
            let url = b.url();
            check_url(&url)?;
            let exe = find_chromium()
                .ok_or("this light browser can't make screenshots or PDFs, and no Chromium is installed for them")?;
            let mut c = launch(home, &exe, "-picture")?;
            c.call("Page.navigate", json!({ "url": url }))?;
            c.settle(20);
            picture(home, &mut c, action).map(|m| format!("{m}\n(Captured by opening the same address again in Chromium, so anything typed or clicked here isn't in it.)"))
        }
        other => Err(format!(
            "browser action `{other}`: use open, read, click, type, scroll, back, screenshot, pdf or close"
        )),
    }
}

/// A screenshot (shown to the model) or a PDF of what `b` shows, saved under workspace/browser.
fn picture(home: &Path, b: &mut Browser, action: &str) -> Result<String, String> {
    if action == "screenshot" {
        let r = b.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 75 }))?;
        let data = r["data"].as_str().ok_or("browser: no screenshot")?;
        let bytes = crate::util::unbase64(data).ok_or("browser: bad screenshot data")?;
        let file = save(home, &b.url(), "jpg", &bytes)?;
        crate::tools::show_image(format!("screenshot of {}", b.url()), format!("data:image/jpeg;base64,{data}"));
        return Ok(format!(
            "Screenshot attached below for you to look at, saved as {}. To give it to the user, send_file that path.",
            file.display()
        ));
    }
    let bytes = pdf_bytes(b)?;
    let file = save(home, &b.url(), "pdf", &bytes)?;
    Ok(format!(
        "Saved the page as a PDF ({} KB): {}. To give it to the user, send_file that path.",
        bytes.len() / 1024,
        file.display()
    ))
}

/// What `b` shows, printed to PDF.
fn pdf_bytes(b: &mut Browser) -> Result<Vec<u8>, String> {
    let r = b.call("Page.printToPDF", json!({ "printBackground": true, "transferMode": "ReturnAsStream" }))?;
    let stream = r["stream"].as_str().ok_or("browser: no PDF stream")?.to_string();
    let mut bytes = Vec::new();
    loop {
        let c = b.call("IO.read", json!({ "handle": stream, "size": 1 << 20 }))?;
        let chunk = c["data"].as_str().unwrap_or("");
        let part = if c["base64Encoded"].as_bool() == Some(true) {
            crate::util::unbase64(chunk).ok_or("browser: bad PDF data")?
        } else {
            chunk.as_bytes().to_vec()
        };
        bytes.extend_from_slice(&part);
        if c["eof"].as_bool() == Some(true) || bytes.len() > 50 << 20 {
            break;
        }
    }
    let _ = b.call("IO.close", json!({ "handle": stream }));
    Ok(bytes)
}

/// Print a local HTML file pagurus wrote (a report) to `pdf`, in a Chromium of its
/// own that closes right after. Used whatever `tools.browser` says: it opens only
/// this file, never the web.
pub fn print_to_pdf(home: &Path, html: &Path, pdf: &Path) -> Result<(), String> {
    let exe = find_chromium().ok_or("no Chromium-family browser to print the PDF")?;
    let mut b = launch(home, &exe, "-print")?;
    b.call("Page.navigate", json!({ "url": format!("file://{}", html.display()) }))?;
    b.settle(30);
    let bytes = pdf_bytes(&mut b)?;
    std::fs::write(pdf, &bytes).map_err(|e| format!("{}: {e}", pdf.display()))
}

/// A file in workspace/browser named after the page's host.
fn save(home: &Path, url: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let host: String = url
        .split("://")
        .nth(1)
        .unwrap_or("page")
        .split('/')
        .next()
        .unwrap_or("page")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' { c } else { '-' })
        .collect();
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
        *g = None; // Drop stops it and removes its files
    }
}

/// One background thread closes an idle browser.
fn reaper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("pagurus-browser-idle".into()).stack_size(64 * 1024).spawn(|| loop {
            std::thread::sleep(Duration::from_secs(15));
            let idle = B
                .try_lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.used.elapsed() > Duration::from_secs(IDLE_SECS)))
                .unwrap_or(false);
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
        let engine = std::env::var("PAGURUS_TEST_BROWSER").unwrap_or_else(|_| "auto".into());
        let go = |a: Value| run(&home, &engine, &a, &yes);
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

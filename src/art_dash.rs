//! The art dashboard (uge): a local web page with this week's proposals, the
//! wishlist, the collection, the taste profile and recent activity. Served by
//! `pagurus daemon` (or `pagurus dashboard`) on `[art] dashboard`, by default
//! http://127.0.0.1:7720. Loopback only for now.
//!
//! Safety: only loopback addresses are accepted; the Host header must name that
//! address (so another website can't reach it through DNS tricks); every API
//! call carries a token that only the served page knows. Rating and the
//! wishlist are the only changes it can make. Nothing here buys.

use crate::util::{new_id, read};
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PAGE: &str = include_str!("art_dash.html");

struct Dash {
    home: PathBuf,
    token: String,
    hosts: Vec<String>,
    conns: AtomicUsize,
}

/// Start serving in the background. Ok(None) when switched off.
pub fn serve(home: PathBuf, addr: &str) -> Result<Option<SocketAddr>, String> {
    let addr = addr.trim();
    if addr.is_empty() || addr == "off" {
        return Ok(None);
    }
    let sa: SocketAddr =
        addr.parse().map_err(|_| format!("[art] dashboard `{addr}`: use host:port, e.g. 127.0.0.1:7720"))?;
    if !sa.ip().is_loopback() {
        return Err(format!("[art] dashboard `{addr}`: only this machine (127.0.0.1) for now"));
    }
    let l = TcpListener::bind(sa).map_err(|e| format!("dashboard {addr}: {e}"))?;
    let bound = l.local_addr().map_err(|e| e.to_string())?;
    let port = bound.port();
    let d = Arc::new(Dash {
        home,
        token: format!("{}{}", new_id(), new_id()),
        hosts: vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")],
        conns: AtomicUsize::new(0),
    });
    std::thread::Builder::new()
        .name("pagurus-dash".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            for s in l.incoming().flatten() {
                if d.conns.load(Ordering::SeqCst) >= 12 {
                    continue;
                }
                d.conns.fetch_add(1, Ordering::SeqCst);
                let d = d.clone();
                let ok = std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || {
                    d.handle(s);
                    d.conns.fetch_sub(1, Ordering::SeqCst);
                });
                if ok.is_err() {
                    eprintln!("pagurus: dashboard: could not start a thread");
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Some(bound))
}

fn respond(mut s: &TcpStream, status: u16, kind: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let _ = write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

impl Dash {
    fn handle(&self, s: TcpStream) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
        let req = match crate::peer::read_request(&mut BufReader::new(&s)) {
            Ok(r) => r,
            Err((code, m)) => return respond(&s, code, "text/plain", &m),
        };
        let header = |k: &str| req.headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.as_str()).unwrap_or("");
        if !self.hosts.iter().any(|h| h == header("host")) {
            return respond(&s, 403, "text/plain", "wrong host");
        }
        let path = req.path.split('?').next().unwrap_or("");
        if req.method == "GET" && (path == "/" || path == "/index.html") {
            return respond(&s, 200, "text/html; charset=utf-8", &PAGE.replace("{{TOKEN}}", &self.token));
        }
        if !path.starts_with("/api/") || header("x-pagurus") != self.token {
            return respond(&s, if path.starts_with("/api/") { 403 } else { 404 }, "text/plain", "not found");
        }
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let piece = body["piece"].as_str().unwrap_or("");
        let r: Result<Value, String> = match (req.method.as_str(), path) {
            ("GET", "/api/data") => Ok(crate::art_taste::data(&self.home)),
            ("GET", "/api/budget") => crate::art_taste::budget(&self.home).map(|b| json!({ "text": b })),
            ("GET", "/api/collection") => crate::art_taste::collection(&self.home).map(|c| json!({ "items": c })),
            ("GET", "/api/activity") => Ok(json!({ "items": activity(&self.home, 150) })),
            ("POST", "/api/rate") => crate::art_taste::rate(
                &self.home,
                piece,
                body["verdict"].as_str().unwrap_or(""),
                body["why"].as_str().unwrap_or(""),
            )
            .map(|m| json!({ "text": m })),
            ("POST", "/api/wish") => crate::art_taste::wish(&self.home, piece, body["note"].as_str().unwrap_or(""))
                .map(|m| json!({ "text": m })),
            ("POST", "/api/unwish") => crate::art_taste::unwish(&self.home, piece).map(|m| json!({ "text": m })),
            _ => return respond(&s, 404, "text/plain", "not found"),
        };
        match r {
            Ok(v) => respond(&s, 200, "application/json", &v.to_string()),
            Err(e) => respond(&s, 400, "application/json", &json!({ "error": e }).to_string()),
        }
    }
}

/// The newest audit entries (commands and who decided), newest first.
fn activity(home: &Path, n: usize) -> Vec<Value> {
    let mut v: Vec<Value> = [home.join("audit.jsonl.1"), home.join("audit.jsonl")]
        .iter()
        .flat_map(|p| read(p).lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect::<Vec<_>>())
        .collect();
    v.reverse();
    v.truncate(n);
    v
}

/// `pagurus dashboard`: serve in the foreground (or point at the daemon's).
pub fn cli(home: &Path) -> Result<String, String> {
    let addr = crate::config::load(home)?.art.dashboard;
    let addr = if addr.trim().is_empty() || addr == "off" { "127.0.0.1:7720".to_string() } else { addr };
    match serve(home.to_path_buf(), &addr) {
        Ok(Some(a)) => {
            println!("pagurus dashboard: http://{a}  (ctrl-c stops it; `pagurus daemon` serves it too)");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        Ok(None) => Err("the dashboard is off".into()),
        Err(e) if e.contains("in use") => Ok(format!("Already running (the daemon serves it): http://{addr}")),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn call(addr: SocketAddr, req: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn dashboard_guards_host_and_token() {
        let home = std::env::temp_dir().join(format!("pagurus-dash-{}", new_id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(serve(home.clone(), "0.0.0.0:0").unwrap_err().contains("only this machine"));
        let a = serve(home.clone(), "127.0.0.1:0").unwrap().unwrap();
        let host = format!("127.0.0.1:{}", a.port());
        let page = call(a, &format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("<title>"));
        let token =
            page.split("name=\"pagurus-token\" content=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        assert!(
            call(a, &format!("GET / HTTP/1.1\r\nHost: evil.example:{}\r\n\r\n", a.port())).starts_with("HTTP/1.1 403"),
            "DNS rebinding"
        );
        assert!(
            call(a, &format!("GET /api/data HTTP/1.1\r\nHost: {host}\r\n\r\n")).starts_with("HTTP/1.1 403"),
            "no token"
        );
        let ok = call(a, &format!("GET /api/data HTTP/1.1\r\nHost: {host}\r\nX-Pagurus: {token}\r\n\r\n"));
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("\"proposals\""), "{ok}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

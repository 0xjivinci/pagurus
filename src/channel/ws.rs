//! A minimal RFC 6455 WebSocket client (text messages, fragments, ping/pong,
//! close) over the TLS stream, shared by Discord and Slack. Small on purpose:
//! no extensions, no compression, client frames masked, messages size-capped.

use super::{Stream, Tls};
use std::io;
use std::time::Duration;

/// Largest message accepted from a server.
pub const MAX_MSG: usize = 4 << 20;

#[derive(Debug, PartialEq)]
pub enum Msg {
    Text(String),
    /// The server closed the connection, with its close code (0 if none).
    Close(u16),
}

pub struct Ws<S: Stream> {
    s: S,
    buf: Vec<u8>,
    frag: Vec<u8>,
}

/// Connect to a wss:// URL. Reads time out after `poll` so callers can do
/// other work (heartbeats) between messages.
pub fn connect(url: &str, poll: Duration) -> Result<Ws<Tls>, String> {
    let rest = url.strip_prefix("wss://").ok_or("only wss:// URLs are supported")?;
    let (hostport, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| "bad port in URL")?),
        None => (hostport, 443),
    };
    let tls = super::connect_tls(host, port, Duration::from_secs(20))?;
    let ws = Ws::handshake(tls, hostport, &path)?;
    let _ = ws.s.sock.set_read_timeout(Some(poll));
    Ok(ws)
}

impl<S: Stream> Ws<S> {
    pub fn handshake(mut s: S, host: &str, path: &str) -> Result<Self, String> {
        let key = crate::util::base64(&[super::random_u64().to_le_bytes(), super::random_u64().to_le_bytes()].concat());
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: pagurus/{}\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        s.write_all(req.as_bytes()).and_then(|_| s.flush()).map_err(|e| format!("websocket: {e}"))?;
        // the response head, byte by byte so nothing after it is swallowed
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 16 * 1024 || s.read(&mut b).map_err(|e| format!("websocket: {e}"))? == 0 {
                return Err("websocket: bad handshake response".into());
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let status = head.lines().next().unwrap_or("");
        if !status.split_whitespace().nth(1).is_some_and(|c| c == "101") {
            return Err(format!("websocket upgrade refused: {}", crate::util::one_line(status, 120)));
        }
        Ok(Self { s, buf: Vec::new(), frag: Vec::new() })
    }

    pub fn send_text(&mut self, t: &str) -> io::Result<()> {
        self.frame(0x1, t.as_bytes())
    }

    pub fn close(&mut self) {
        let _ = self.frame(0x8, &1000u16.to_be_bytes());
    }

    fn frame(&mut self, op: u8, payload: &[u8]) -> io::Result<()> {
        let out = encode(op, payload, super::random_u64() as u32);
        self.s.write_all(&out)?;
        self.s.flush()
    }

    /// The next message; None if the read timed out first.
    pub fn read(&mut self) -> io::Result<Option<Msg>> {
        loop {
            while let Some((fin, op, payload, used)) = parse(&self.buf)? {
                self.buf.drain(..used);
                match op {
                    0x9 => self.frame(0xA, &payload)?,
                    0xA => {}
                    0x8 => {
                        let code = if payload.len() >= 2 { u16::from_be_bytes([payload[0], payload[1]]) } else { 0 };
                        let _ = self.frame(0x8, &payload[..payload.len().min(2)]);
                        return Ok(Some(Msg::Close(code)));
                    }
                    0x0..=0x2 => {
                        if self.frag.len() + payload.len() > MAX_MSG {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "websocket message too large"));
                        }
                        self.frag.extend_from_slice(&payload);
                        if fin {
                            let m = std::mem::take(&mut self.frag);
                            return Ok(Some(Msg::Text(String::from_utf8_lossy(&m).into_owned())));
                        }
                    }
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown websocket opcode")),
                }
            }
            let mut tmp = [0u8; 8192];
            match self.s.read(&mut tmp) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "websocket closed")),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// (fin, opcode, unmasked payload, bytes used)
pub type Frame = (bool, u8, Vec<u8>, usize);

/// A masked client frame.
pub fn encode(op: u8, payload: &[u8], mask: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | op);
    match payload.len() {
        n if n < 126 => out.push(0x80 | n as u8),
        n if n <= 0xFFFF => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    let m = mask.to_be_bytes();
    out.extend_from_slice(&m);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ m[i % 4]));
    out
}

/// One frame from the front of `b`: (fin, opcode, unmasked payload, bytes used),
/// or None if it isn't all there yet.
pub fn parse(b: &[u8]) -> io::Result<Option<Frame>> {
    if b.len() < 2 {
        return Ok(None);
    }
    let (fin, op, masked) = (b[0] & 0x80 != 0, b[0] & 0x0F, b[1] & 0x80 != 0);
    let (len, mut at) = match b[1] & 0x7F {
        126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as u64, 4),
        127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap_or_default()), 10),
        126 | 127 => return Ok(None),
        n => (n as u64, 2),
    };
    if len > MAX_MSG as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "websocket frame too large"));
    }
    let mask = if masked {
        if b.len() < at + 4 {
            return Ok(None);
        }
        at += 4;
        Some([b[at - 4], b[at - 3], b[at - 2], b[at - 1]])
    } else {
        None
    };
    let end = at + len as usize;
    if b.len() < end {
        return Ok(None);
    }
    let mut payload = b[at..end].to_vec();
    if let Some(m) = mask {
        payload.iter_mut().enumerate().for_each(|(i, x)| *x ^= m[i % 4]);
    }
    Ok(Some((fin, op, payload, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn server_frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![if fin { 0x80 } else { 0 } | op];
        if payload.len() < 126 {
            v.push(payload.len() as u8);
        } else {
            v.push(126);
            v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn frames_round_trip() {
        for n in [0usize, 5, 125, 126, 300, 70_000] {
            let p = vec![b'x'; n];
            let f = encode(0x1, &p, 0xDEADBEEF);
            assert_eq!(f[1] & 0x80, 0x80, "client frames are masked");
            let (fin, op, got, used) = parse(&f).unwrap().unwrap();
            assert!(fin && op == 1 && got == p && used == f.len(), "len {n}");
            assert!(parse(&f[..f.len() - 1]).unwrap().is_none(), "partial frame waits");
        }
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert!(parse(&huge).is_err(), "oversized frames are refused");
    }

    /// A fake server stream: canned bytes in, everything written kept.
    struct Fake {
        input: std::io::Cursor<Vec<u8>>,
        out: Vec<u8>,
    }
    impl Read for Fake {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            match self.input.read(b)? {
                0 => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data")),
                n => Ok(n),
            }
        }
    }
    impl Write for Fake {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.out.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn handshake_fragments_ping_and_close() {
        let mut input = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
        input.extend(server_frame(false, 0x1, b"hel"));
        input.extend(server_frame(true, 0x9, b"pp")); // ping between fragments
        input.extend(server_frame(true, 0x0, b"lo"));
        input.extend(server_frame(true, 0x8, &4004u16.to_be_bytes()));
        let fake = Fake { input: std::io::Cursor::new(input), out: Vec::new() };
        let mut ws = Ws::handshake(fake, "gateway.example", "/?v=10").unwrap();
        let req = String::from_utf8_lossy(&ws.s.out).to_string();
        assert!(req.starts_with("GET /?v=10 HTTP/1.1\r\nHost: gateway.example\r\n") && req.contains("Sec-WebSocket-Key: "));
        ws.s.out.clear();
        assert_eq!(ws.read().unwrap(), Some(Msg::Text("hello".into())));
        let (_, op, payload, _) = parse(&ws.s.out).unwrap().unwrap();
        assert_eq!((op, payload.as_slice()), (0xA, &b"pp"[..]), "ping answered with pong");
        assert_eq!(ws.read().unwrap(), Some(Msg::Close(4004)));
        assert_eq!(ws.read().unwrap(), None, "timeout: no message yet");
        let refused = Fake { input: std::io::Cursor::new(b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec()), out: Vec::new() };
        assert!(Ws::handshake(refused, "h", "/").is_err());
    }
}

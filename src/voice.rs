//! Voice on Telegram, after Hermes' transcription and TTS tools: voice notes
//! are transcribed by any OpenAI-compatible `/audio/transcriptions` endpoint
//! (OpenAI, Groq, a local whisper server), and replies can be spoken through
//! an `/audio/speech` endpoint. Off unless configured in `[voice]`.

use serde::Deserialize;
use std::collections::HashSet;
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct VoiceCfg {
    /// Base URL for speech-to-text, e.g. https://api.groq.com/openai/v1. Empty = voice notes are not understood.
    pub stt_url: String,
    pub stt_model: String,
    /// Env var holding the key (empty: no key, e.g. a local server).
    pub stt_key_env: String,
    /// Base URL for text-to-speech, e.g. https://api.openai.com/v1. Empty = never speak.
    pub tts_url: String,
    pub tts_model: String,
    pub tts_voice: String,
    pub tts_key_env: String,
    /// `voice`: answer a voice note with a voice note too · `always` · `off`.
    pub speak: String,
}

impl Default for VoiceCfg {
    fn default() -> Self {
        Self {
            stt_url: String::new(),
            stt_model: "whisper-1".into(),
            stt_key_env: String::new(),
            tts_url: String::new(),
            tts_model: "tts-1".into(),
            tts_voice: "alloy".into(),
            tts_key_env: String::new(),
            speak: "voice".into(),
        }
    }
}

/// Marks a Telegram file id as a voice note in the daemon's message tuple.
pub const TAG: &str = "voice:";

/// The file id of a voice note or audio message, tagged.
pub fn voice_id(m: &serde_json::Value) -> Option<String> {
    let v = if m["voice"].is_object() { &m["voice"] } else { &m["audio"] };
    v["file_id"].as_str().map(|id| format!("{TAG}{id}"))
}

fn key(env: &str) -> Option<String> {
    (!env.is_empty()).then(|| std::env::var(env).ok()).flatten().filter(|k| !k.trim().is_empty())
}

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout(Duration::from_secs(120)).build()
}

/// A multipart/form-data body: text fields, then one file. Returns (content type, body).
pub fn multipart(fields: &[(&str, &str)], file_field: &str, filename: &str, mime: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("pagurus{}", crate::util::new_id());
    let mut body = Vec::with_capacity(bytes.len() + 512);
    for (k, v) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

/// Speech to text. `audio` is what Telegram sends (OGG/Opus).
pub fn transcribe(c: &VoiceCfg, audio: &[u8]) -> Result<String, String> {
    if c.stt_url.trim().is_empty() {
        return Err("I can't listen to voice messages yet: set [voice] stt_url in config.toml (any OpenAI-compatible \
                    /audio/transcriptions endpoint, e.g. Groq or a local whisper server)"
            .into());
    }
    let (ct, body) = multipart(&[("model", &c.stt_model), ("response_format", "json")], "file", "voice.ogg", "audio/ogg", audio);
    let mut req = http().post(&endpoint(&c.stt_url, "audio/transcriptions")).set("Content-Type", &ct);
    if let Some(k) = key(&c.stt_key_env) {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let v: serde_json::Value = match req.send_bytes(&body) {
        Ok(r) => r.into_json().map_err(|e| format!("transcription: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            return Err(format!("transcription failed: HTTP {code} {}", crate::util::one_line(&r.into_string().unwrap_or_default(), 160)))
        }
        Err(e) => return Err(format!("transcription failed: {e}")),
    };
    let text = v["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err("I couldn't make out any words in that voice message".into());
    }
    Ok(text)
}

/// Text to speech as OGG/Opus (what Telegram shows as a voice note).
pub fn speak(c: &VoiceCfg, text: &str) -> Result<Vec<u8>, String> {
    let body = serde_json::json!({ "model": c.tts_model, "voice": c.tts_voice, "input": speakable(text), "response_format": "opus" });
    let mut req = http().post(&endpoint(&c.tts_url, "audio/speech"));
    if let Some(k) = key(&c.tts_key_env) {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let r = req.send_json(body).map_err(|e| format!("speech failed: {}", crate::util::one_line(&e.to_string(), 160)))?;
    let mut buf = Vec::new();
    r.into_reader().take(20 * 1024 * 1024).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Markdown and links don't read well aloud; long answers are cut.
pub fn speakable(text: &str) -> String {
    let mut out = String::new();
    for l in text.lines() {
        let l = l.trim_start_matches(['#', '>', '-', '*', ' ']);
        if l.starts_with("```") {
            continue;
        }
        let words: Vec<&str> = l.split_whitespace().map(|w| if w.contains("://") { "(link)" } else { w }).collect();
        if !words.is_empty() {
            out += &words.join(" ").replace(['*', '`', '_'], "");
            out.push('\n');
        }
    }
    out.chars().take(1500).collect()
}

/// Chats whose last message was a voice note (they get a spoken reply).
static HEARD: Mutex<Option<HashSet<i64>>> = Mutex::new(None);

pub fn heard(chat: i64) {
    if let Ok(mut h) = HEARD.lock() {
        h.get_or_insert_with(HashSet::new).insert(chat);
    }
}

/// Should this reply also be spoken? Consumes the voice-note mark.
pub fn should_speak(c: &VoiceCfg, chat: i64) -> bool {
    let was_voice = HEARD.lock().ok().and_then(|mut h| h.as_mut().map(|s| s.remove(&chat))).unwrap_or(false);
    !c.tts_url.trim().is_empty() && (c.speak == "always" || (c.speak == "voice" && was_voice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_ids_and_speaking_rules() {
        let m = serde_json::json!({"voice": {"file_id": "abc", "duration": 3}});
        assert_eq!(voice_id(&m).as_deref(), Some("voice:abc"));
        assert_eq!(voice_id(&serde_json::json!({"text": "hi"})), None);
        let mut c = VoiceCfg::default();
        heard(5);
        assert!(!should_speak(&c, 5), "no tts endpoint: never speak");
        c.tts_url = "http://localhost:1/v1".into();
        heard(5);
        assert!(should_speak(&c, 5));
        assert!(!should_speak(&c, 5), "only the reply to the voice note");
        c.speak = "always".into();
        assert!(should_speak(&c, 6));
        assert!(transcribe(&VoiceCfg::default(), b"x").unwrap_err().contains("stt_url"));
        assert_eq!(speakable("## Disk\n- **/** is 80% full, see https://x.y/z\n```\ndf\n```"), "Disk\n/ is 80% full, see (link)\ndf\n");
        let (ct, body) = multipart(&[("model", "m")], "file", "a.ogg", "audio/ogg", b"OGG");
        let b = String::from_utf8_lossy(&body);
        assert!(ct.starts_with("multipart/form-data; boundary=") && b.contains("name=\"model\"\r\n\r\nm\r\n") && b.contains("OGG\r\n--"));
    }

    /// A fake OpenAI-compatible transcription server on localhost.
    #[test]
    fn transcribes_through_an_openai_compatible_endpoint() {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let (mut first, mut len) = (String::new(), 0usize);
            r.read_line(&mut first).unwrap();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if h.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let ok = first.starts_with("POST /v1/audio/transcriptions") && String::from_utf8_lossy(&body).contains("OggS");
            let reply = if ok { r#"{"text":" check the disk "}"# } else { r#"{"text":""}"# };
            write!(&s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).unwrap();
        });
        let c = VoiceCfg { stt_url: format!("http://127.0.0.1:{port}/v1/"), ..Default::default() };
        assert_eq!(transcribe(&c, b"OggS...").unwrap(), "check the disk");
        srv.join().unwrap();
    }
}

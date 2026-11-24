//! Voice on Telegram, after Hermes' transcription and TTS tools: voice notes
//! are transcribed by any OpenAI-compatible `/audio/transcriptions` endpoint
//! (OpenAI, Groq, a local whisper server), and replies can be spoken through
//! an `/audio/speech` endpoint. Off unless configured in `[voice]`.
//! `provider = "gemini"` does both with one Gemini key instead: Gemini listens to
//! the note, and its TTS voices speak, turned into a voice note by ffmpeg.

use serde::Deserialize;
use std::collections::HashSet;
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct VoiceCfg {
    /// `openai` (default): the OpenAI-compatible endpoints below · `gemini`: Google's Gemini API for both,
    /// with `key_env` (default GEMINI_API_KEY); no URLs needed.
    pub provider: String,
    /// Gemini: env var holding the key.
    pub key_env: String,
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
            provider: "openai".into(),
            key_env: "GEMINI_API_KEY".into(),
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
pub fn multipart(
    fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> (String, Vec<u8>) {
    let boundary = format!("pagurus{}", crate::util::new_id());
    let mut body = Vec::with_capacity(bytes.len() + 512);
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/models";

fn gemini(c: &VoiceCfg) -> bool {
    c.provider.eq_ignore_ascii_case("gemini")
}

/// Can voice notes be understood?
pub fn listening(c: &VoiceCfg) -> bool {
    gemini(c) || !c.stt_url.trim().is_empty()
}

/// Can replies be spoken?
pub fn speaking(c: &VoiceCfg) -> bool {
    gemini(c) || !c.tts_url.trim().is_empty()
}

/// The configured model, or Gemini's when the OpenAI default was left in place.
fn pick<'a>(set: &'a str, openai_default: &str, gemini_default: &'a str) -> &'a str {
    if set.trim().is_empty() || set == openai_default {
        gemini_default
    } else {
        set
    }
}

fn gemini_call(c: &VoiceCfg, model: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let k = key(&c.key_env).ok_or_else(|| format!("voice: set {} in ~/.pagurus/env", c.key_env))?;
    match http().post(&format!("{GEMINI}/{model}:generateContent")).set("x-goog-api-key", &k).send_json(body) {
        Ok(r) => r.into_json().map_err(|e| format!("gemini: {e}")),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "gemini {model}: HTTP {code} {}",
            crate::util::one_line(&r.into_string().unwrap_or_default(), 160)
        )),
        Err(e) => Err(format!("gemini {model}: {e}")),
    }
}

fn gemini_transcribe(c: &VoiceCfg, audio: &[u8]) -> Result<String, String> {
    let model = pick(&c.stt_model, "whisper-1", "gemini-flash-lite-latest");
    let body = serde_json::json!({ "contents": [{ "parts": [
        { "text": "Transcribe this voice message exactly, in the language it is spoken in. Reply with the transcript only; if there are no words, reply with nothing." },
        { "inline_data": { "mime_type": "audio/ogg", "data": crate::util::base64(audio) } }
    ]}]});
    let v = gemini_call(c, model, body)?;
    Ok(v["candidates"][0]["content"]["parts"][0]["text"].as_str().unwrap_or("").trim().to_string())
}

fn gemini_speak(c: &VoiceCfg, text: &str) -> Result<Vec<u8>, String> {
    let model = pick(&c.tts_model, "tts-1", "gemini-3.8-flash-lite-tts");
    let voice = pick(&c.tts_voice, "alloy", "Kore");
    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": speakable(text) }] }],
        "generationConfig": { "responseModalities": ["AUDIO"],
            "speechConfig": { "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } } } }
    });
    let v = gemini_call(c, model, body)?;
    let part = &v["candidates"][0]["content"]["parts"][0]["inlineData"];
    let audio = part["data"].as_str().and_then(crate::util::unbase64).ok_or("gemini sent no audio")?;
    to_voice_note(&audio, part["mimeType"].as_str().unwrap_or(""))
}

/// ffmpeg input flags for Gemini's audio: WAV is self-describing, raw L16 PCM needs its rate.
fn pcm_args(mime: &str) -> Vec<String> {
    if !mime.starts_with("audio/L16") && !mime.contains("pcm") {
        return vec![];
    }
    let rate = mime.split(';').find_map(|p| p.trim().strip_prefix("rate=")).unwrap_or("24000");
    ["-f", "s16le", "-ar", rate, "-ac", "1"].map(String::from).to_vec()
}

/// Any audio Gemini sends → OGG/Opus, which Telegram shows as a voice note.
fn to_voice_note(audio: &[u8], mime: &str) -> Result<Vec<u8>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut ff = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-nostdin"])
        .args(pcm_args(mime))
        .args(["-i", "pipe:0", "-c:a", "libopus", "-b:a", "32k", "-ac", "1", "-f", "ogg", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            "spoken replies with Gemini need ffmpeg (sudo apt install ffmpeg, or brew install ffmpeg)".to_string()
        })?;
    let mut stdin = ff.stdin.take().ok_or("ffmpeg: no stdin")?;
    let input = audio.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = ff.wait_with_output().map_err(|e| format!("ffmpeg: {e}"))?;
    let _ = feeder.join();
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!("ffmpeg: {}", crate::util::one_line(&String::from_utf8_lossy(&out.stderr), 160)));
    }
    Ok(out.stdout)
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

/// Speech to text. `audio` is what Telegram sends (OGG/Opus).
pub fn transcribe(c: &VoiceCfg, audio: &[u8]) -> Result<String, String> {
    if gemini(c) {
        let text = gemini_transcribe(c, audio)?;
        if text.is_empty() {
            return Err("I couldn't make out any words in that voice message".into());
        }
        return Ok(text);
    }
    if c.stt_url.trim().is_empty() {
        return Err(
            "I can't listen to voice messages yet: set [voice] provider = \"gemini\" or stt_url in config.toml \
                    (any OpenAI-compatible /audio/transcriptions endpoint, e.g. Groq or a local whisper server)"
                .into(),
        );
    }
    let (ct, body) =
        multipart(&[("model", &c.stt_model), ("response_format", "json")], "file", "voice.ogg", "audio/ogg", audio);
    let mut req = http().post(&endpoint(&c.stt_url, "audio/transcriptions")).set("Content-Type", &ct);
    if let Some(k) = key(&c.stt_key_env) {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let v: serde_json::Value = match req.send_bytes(&body) {
        Ok(r) => r.into_json().map_err(|e| format!("transcription: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            return Err(format!(
                "transcription failed: HTTP {code} {}",
                crate::util::one_line(&r.into_string().unwrap_or_default(), 160)
            ))
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
    if gemini(c) {
        return gemini_speak(c, text);
    }
    let body = serde_json::json!({ "model": c.tts_model, "voice": c.tts_voice, "input": speakable(text), "response_format": "opus" });
    let mut req = http().post(&endpoint(&c.tts_url, "audio/speech"));
    if let Some(k) = key(&c.tts_key_env) {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let r =
        req.send_json(body).map_err(|e| format!("speech failed: {}", crate::util::one_line(&e.to_string(), 160)))?;
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
    speaking(c) && (c.speak == "always" || (c.speak == "voice" && was_voice))
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
        assert_eq!(
            speakable("## Disk\n- **/** is 80% full, see https://x.y/z\n```\ndf\n```"),
            "Disk\n/ is 80% full, see (link)\ndf\n"
        );
        let (ct, body) = multipart(&[("model", "m")], "file", "a.ogg", "audio/ogg", b"OGG");
        let b = String::from_utf8_lossy(&body);
        assert!(
            ct.starts_with("multipart/form-data; boundary=")
                && b.contains("name=\"model\"\r\n\r\nm\r\n")
                && b.contains("OGG\r\n--")
        );
    }

    #[test]
    fn gemini_does_both_with_one_key() {
        let c = VoiceCfg { provider: "gemini".into(), ..Default::default() };
        assert!(listening(&c) && speaking(&c) && !listening(&VoiceCfg::default()));
        assert_eq!(pick(&c.tts_model, "tts-1", "gemini-3.8-flash-lite-tts"), "gemini-3.8-flash-lite-tts");
        assert_eq!(pick("gemini-2.5-flash-preview-tts", "tts-1", "x"), "gemini-2.5-flash-preview-tts");
        assert_eq!(pcm_args("audio/L16;codec=pcm;rate=16000"), ["-f", "s16le", "-ar", "16000", "-ac", "1"]);
        assert!(pcm_args("audio/wav").is_empty());
        let none = VoiceCfg { key_env: "PAGURUS_TEST_NO_SUCH_KEY".into(), ..c };
        assert!(transcribe(&none, b"x").unwrap_err().contains("PAGURUS_TEST_NO_SUCH_KEY"));
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
            let ok =
                first.starts_with("POST /v1/audio/transcriptions") && String::from_utf8_lossy(&body).contains("OggS");
            let reply = if ok { r#"{"text":" check the disk "}"# } else { r#"{"text":""}"# };
            write!(
                &s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        });
        let c = VoiceCfg { stt_url: format!("http://127.0.0.1:{port}/v1/"), ..Default::default() };
        assert_eq!(transcribe(&c, b"OggS...").unwrap(), "check the disk");
        srv.join().unwrap();
    }
}

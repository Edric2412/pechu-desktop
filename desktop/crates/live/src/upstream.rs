//! One connection to one live-captions service (Sarvam or Gemini), running in its own task.
//! It knows the wire format of the service and nothing about queues or fallback: it turns what the service says into
//! `UpEvent`s and writes whatever text frames the session task hands it. Port of `connectSarvam` / `connectGemini`.
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use crate::{live_reason_logged, mask_keys, LiveConfig, LiveMode};

const GEMINI_URL: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const SARVAM_URL: &str = "wss://api.sarvam.ai/speech-to-text-realtime/ws";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Sarvam,
    Gemini,
}

impl Provider {
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Sarvam => "Sarvam Saaras v4",
            Provider::Gemini => "Gemini 3.5 Transcribe Live",
        }
    }

    /// The text frame that carries one chunk of audio.
    pub fn audio_message(self, pcm: &[u8]) -> String {
        let audio = base64::engine::general_purpose::STANDARD.encode(pcm);
        match self {
            Provider::Sarvam => json!({ "event": "audio_input", "audio": audio }),
            Provider::Gemini => json!({ "realtimeInput": { "audio": { "data": audio, "mimeType": "audio/pcm;rate=16000" } } }),
        }
        .to_string()
    }

    /// The text frame that says "the speaker stopped, finish the last turn".
    pub fn end_message(self) -> String {
        match self {
            Provider::Sarvam => json!({ "event": "end" }),
            Provider::Gemini => json!({ "realtimeInput": { "audioStreamEnd": true } }),
        }
        .to_string()
    }

    /// "Sarvam" / "Google" as used in the log lines and error texts.
    fn who(self) -> &'static str {
        match self {
            Provider::Sarvam => "Sarvam",
            Provider::Gemini => "Google",
        }
    }
}

/// What an upstream tells the session task. Fail = could not start or broke down; Closed = ended normally.
#[derive(Debug)]
pub(crate) enum UpEvent {
    Ready,
    Interim(String),
    Final(String),
    Fail(String),
    Closed,
}

/// What the session task tells an upstream.
pub(crate) enum UpCmd {
    /// A finished text frame (the session task builds it with `audio_message` / `end_message`).
    Send(String),
    Close,
}

pub(crate) struct Upstream {
    cmds: UnboundedSender<UpCmd>,
}

impl Upstream {
    pub fn send(&self, cmd: UpCmd) {
        // The task may already be gone (it failed or the service closed); then there is nothing to send to.
        let _ = self.cmds.send(cmd);
    }
}

/// Starts the connection task. Its events arrive on `events` tagged with `id`.
pub(crate) fn spawn(
    provider: Provider,
    cfg: &LiveConfig,
    key: String,
    secrets: Vec<String>,
    id: u64,
    connect_timeout: Duration,
    events: UnboundedSender<(u64, UpEvent)>,
) -> Upstream {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let task = Task { provider, id, events, secrets, deadline: Instant::now() + connect_timeout };
    let request = build_request(provider, cfg, &key);
    let setup = (provider == Provider::Gemini).then(|| gemini_setup(cfg));
    tokio::spawn(task.run(request, setup, cmd_rx));
    Upstream { cmds: cmd_tx }
}

fn build_request(provider: Provider, cfg: &LiveConfig, key: &str) -> Result<Request<()>, String> {
    let bad_address = || "The live captions address is not valid.".to_string();
    match provider {
        Provider::Sarvam => {
            let mut url = url::Url::parse(cfg.sarvam_url.as_deref().unwrap_or(SARVAM_URL)).map_err(|_| bad_address())?;
            url.query_pairs_mut()
                // "auto" = detect, which handles Tamil with English mixed in
                .append_pair("language_code", cfg.langs.first().map(String::as_str).unwrap_or("auto"))
                .append_pair("model", "saaras:v4")
                // partials a little slower but more accurate: right for captions
                .append_pair("stream_type", "balanced")
                // applies to each finished sentence
                .append_pair("mode", if cfg.mode == LiveMode::Verbatim { "verbatim" } else { "codemix" })
                .append_pair("encoding", "linear16")
                .append_pair("sample_rate", "16000");
            let mut request = url.as_str().into_client_request().map_err(|_| bad_address())?;
            let value = HeaderValue::from_str(key).map_err(|_| "The Sarvam API key has characters that are not allowed.".to_string())?;
            request.headers_mut().insert("api-subscription-key", value);
            Ok(request)
        }
        Provider::Gemini => {
            let mut url = url::Url::parse(cfg.gemini_url.as_deref().unwrap_or(GEMINI_URL)).map_err(|_| bad_address())?;
            url.query_pairs_mut().append_pair("key", key);
            url.as_str().into_client_request().map_err(|_| bad_address())
        }
    }
}

fn gemini_setup(cfg: &LiveConfig) -> String {
    json!({
        "setup": {
            "model": "models/gemini-3.5-transcribe-live",
            "generationConfig": { "responseModalities": ["TEXT"] },
            "inputAudioTranscription": {
                "languageCodes": cfg.langs,
                "mode": if cfg.mode == LiveMode::Verbatim { "VERBATIM" } else { "SMART" },
            },
        }
    })
    .to_string()
}

/// rustls needs to be told which crypto library to use. We name it here instead of relying on a process-wide default,
/// so this crate works no matter what the rest of the app links in. Roots are the bundled Mozilla list.
fn tls_connector() -> Connector {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

struct Task {
    provider: Provider,
    id: u64,
    events: UnboundedSender<(u64, UpEvent)>,
    secrets: Vec<String>,
    /// The service must be ready by now (connecting + its "begin" message), or we give up.
    deadline: Instant,
}

impl Task {
    /// Returns false when the session task is gone, so there is no point in carrying on.
    fn emit(&self, e: UpEvent) -> bool {
        self.events.send((self.id, e)).is_ok()
    }

    fn fail(&self, reason: impl Into<String>) {
        self.emit(UpEvent::Fail(reason.into()));
    }

    fn timeout_message(&self) -> String {
        format!("Timed out connecting to {}'s live transcription.", self.provider.who())
    }

    fn unreachable_message(&self) -> String {
        format!("Could not reach {}'s live transcription.", self.provider.who())
    }

    async fn run(self, request: Result<Request<()>, String>, setup: Option<String>, mut cmds: UnboundedReceiver<UpCmd>) {
        let request = match request {
            Ok(r) => r,
            Err(why) => return self.fail(why),
        };

        // Connect. Any command that arrives meanwhile can only be Close (nothing is sent before ready).
        let connecting = tokio::time::timeout_at(self.deadline, connect_async_tls_with_config(request, None, false, Some(tls_connector())));
        let mut ws = tokio::select! {
            result = connecting => match result {
                Ok(Ok((ws, _response))) => ws,
                Ok(Err(e)) => {
                    // The error text can name the address, which holds the key for Gemini, so it is masked.
                    tracing::error!("[live] {} connect failed: {}", self.provider.who(), mask_keys(&e.to_string(), &self.secrets));
                    return self.fail(self.unreachable_message());
                }
                Err(_) => return self.fail(self.timeout_message()),
            },
            _ = cmds.recv() => return,
        };

        if let Some(setup) = setup {
            if ws.send(Message::text(setup)).await.is_err() {
                return self.fail(self.unreachable_message());
            }
        }

        let mut ready = false;
        loop {
            tokio::select! {
                incoming = ws.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(t))) => t.as_str().to_string(),
                        // Google may send its JSON as a binary frame
                        Some(Ok(Message::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
                        Some(Ok(Message::Close(frame))) => {
                            let (code, reason) = match frame {
                                Some(f) => (u16::from(f.code), f.reason.to_string()),
                                None => (1005, String::new()), // 1005 = no code given
                            };
                            self.on_close(code, reason);
                            return;
                        }
                        Some(Ok(_)) => continue, // ping, pong: the library answers them
                        Some(Err(_)) => return self.fail(self.unreachable_message()),
                        None => return self.on_close(1006, String::new()), // the connection dropped without a close frame
                    };
                    for event in self.parse(&text, ready) {
                        let stop = matches!(event, UpEvent::Fail(_));
                        if matches!(event, UpEvent::Ready) {
                            ready = true;
                        }
                        if !self.emit(event) || stop {
                            let _ = ws.close(None).await;
                            return;
                        }
                    }
                }
                cmd = cmds.recv() => match cmd {
                    Some(UpCmd::Send(text)) => {
                        if ws.send(Message::text(text)).await.is_err() {
                            return self.fail(self.unreachable_message());
                        }
                    }
                    // Close, or the session task dropped us
                    Some(UpCmd::Close) | None => {
                        let _ = ws.close(None).await;
                        return;
                    }
                },
                _ = tokio::time::sleep_until(self.deadline), if !ready => {
                    self.fail(self.timeout_message());
                    let _ = ws.close(None).await;
                    return;
                }
            }
        }
    }

    /// Turns one JSON message from the service into events. Anything that is not JSON is ignored.
    fn parse(&self, text: &str, ready: bool) -> Vec<UpEvent> {
        let Ok(m) = serde_json::from_str::<Value>(text) else { return Vec::new() };
        let mut out = Vec::new();
        let non_empty = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
        match self.provider {
            Provider::Sarvam => match m["event"].as_str() {
                Some("session.begin") => out.push(UpEvent::Ready),
                Some("transcript.partial") => out.extend(non_empty(&m["text"]).map(UpEvent::Interim)),
                Some("transcript.final") => out.extend(non_empty(&m["text"]).map(UpEvent::Final)),
                Some("error") if m["is_fatal"].as_bool() == Some(true) => {
                    let raw = m["message"].as_str().unwrap_or("Sarvam live transcription failed.");
                    out.push(UpEvent::Fail(live_reason_logged(raw, "Sarvam", &self.secrets)));
                }
                _ => {}
            },
            Provider::Gemini => {
                if !m["setupComplete"].is_null() && !ready {
                    out.push(UpEvent::Ready);
                }
                let content = &m["serverContent"];
                out.extend(non_empty(&content["interimInputTranscription"]["text"]).map(UpEvent::Interim));
                out.extend(non_empty(&content["inputTranscription"]["text"]).map(UpEvent::Final));
                if !m["error"].is_null() {
                    let raw = m["error"]["message"].as_str().unwrap_or("Live transcription failed.");
                    out.push(UpEvent::Fail(live_reason_logged(raw, "Google", &self.secrets)));
                }
            }
        }
        out
    }

    /// The service closed the connection. 1000 = normal end, 1005 = no code given.
    fn on_close(&self, code: u16, reason: String) {
        if code == 1000 || code == 1005 {
            self.emit(UpEvent::Closed);
            return;
        }
        match self.provider {
            Provider::Sarvam => {
                let why = match code {
                    1003 => "the key was refused or the quota is used up".to_string(),
                    4000 => "a setting was refused".to_string(),
                    other => format!("code {other}"),
                };
                self.fail(format!("Sarvam live transcription ended ({why})."));
            }
            Provider::Gemini => {
                let raw = if reason.is_empty() { format!("connection ended (code {code})") } else { reason };
                let raw: String = mask_keys(&raw, &self.secrets).chars().take(300).collect();
                self.fail(live_reason_logged(&raw, "Google", &self.secrets));
            }
        }
    }
}

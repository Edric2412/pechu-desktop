//! Live captions. The app sends 16 kHz 16-bit mono PCM and gets events back, whichever service does the listening: Sarvam when
//! its key is set (better for Tamil mixed with English); if it cannot connect or fails during the call, the same session moves
//! to Gemini's live transcription. Port of server/live.ts (the part that talks to the services; the browser-facing WebSocket
//! belongs to pechu-server, which turns `LiveEvent`s into the JSON messages the web UI already understands).
//! CONTRACT: the signatures below are used by other crates. You may add functions and fields; do not change these.
//!
//! How it is built: one background "session" task owns all the state (queue, ready, ending). Each connection to a service
//! runs in its own small "upstream" task and talks to the session task through channels. Only the session task decides
//! when audio goes out, so the rule "never send audio before the service is ready, nothing after end/close" lives in
//! exactly one place. This is the part that is billed by audio received.
mod upstream;

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use upstream::{Provider, UpCmd, UpEvent, Upstream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveMode {
    Smart,
    Verbatim,
}

#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub mode: LiveMode,
    /// Empty = auto-detect. Otherwise like "ta" or "en-IN" (only the first is used for Sarvam).
    pub langs: Vec<String>,
    pub sarvam_key: Option<String>,
    pub gemini_key: Option<String>,
    /// Tests point these at a local fake WebSocket server (the web version reads SARVAM_LIVE_URL / GEMINI_LIVE_URL).
    pub sarvam_url: Option<String>,
    pub gemini_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveEvent {
    /// `provider` is the display name: "Sarvam Saaras v4" or "Gemini 3.5 Transcribe Live".
    Ready { provider: String },
    Interim(String),
    Final(String),
    /// The session carries on, but the person should be told (for example: Sarvam failed, Gemini took over).
    Notice(String),
    /// Fatal: the session is over. Always followed by `Closed`.
    Error(String),
    Closed,
}

/// What the handle can ask the session task to do.
enum Cmd {
    Pcm(Vec<u8>),
    End,
    Close,
}

/// Send audio and control the session. Cheap to clone. Dropping every handle closes the session.
#[derive(Clone)]
pub struct LiveHandle {
    cmds: UnboundedSender<Cmd>,
}

impl LiveHandle {
    /// Audio that arrives before the service is ready is queued (at most 600 chunks / 2 MiB, oldest dropped) and sent when ready.
    pub fn send_pcm(&self, pcm: Vec<u8>) {
        // If the session already ended the send fails; that is fine, there is nobody left to listen.
        let _ = self.cmds.send(Cmd::Pcm(pcm));
    }
    /// The speaker stopped: tell the service to finish the last turn; the session then ends with `Closed`.
    pub fn end(&self) {
        let _ = self.cmds.send(Cmd::End);
    }
    /// Close at once. The receiver still gets one final `Closed`.
    pub fn close(&self) {
        let _ = self.cmds.send(Cmd::Close);
    }
}

/// How long we wait for a service to say it is ready (the same 15 s as the web version).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// After `end()` the service gets this long to send the last caption and close. The real Sarvam sends the last caption but
/// then stays connected for good (measured 2026-11-17), so after this time we close it ourselves instead of waiting for ever.
pub const END_GRACE: Duration = Duration::from_secs(8);
const MAX_QUEUE: usize = 600; // about 60 s of audio
const MAX_QUEUE_BYTES: usize = 2 * 1024 * 1024; // the real bound: a chunk can be any size, so the count alone is not enough

/// Starts a session in the background and returns at once. Needs a running tokio runtime.
pub fn start(cfg: LiveConfig) -> (LiveHandle, UnboundedReceiver<LiveEvent>) {
    start_with_timeout(cfg, CONNECT_TIMEOUT)
}

/// Same as `start`, with a shorter connect timeout (tests do not want to wait 15 s).
pub fn start_with_timeout(cfg: LiveConfig, connect_timeout: Duration) -> (LiveHandle, UnboundedReceiver<LiveEvent>) {
    start_with_timeouts(cfg, connect_timeout, END_GRACE)
}

/// Same as `start_with_timeout`, with a chosen grace period after `end()` as well.
pub fn start_with_timeouts(cfg: LiveConfig, connect_timeout: Duration, end_grace: Duration) -> (LiveHandle, UnboundedReceiver<LiveEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let session = Session::new(cfg, connect_timeout, end_grace, event_tx);
    tokio::spawn(session.run(cmd_rx));
    (LiveHandle { cmds: cmd_tx }, event_rx)
}

/// Treat an empty key like no key.
fn non_empty(key: &Option<String>) -> Option<&str> {
    key.as_deref().map(str::trim).filter(|k| !k.is_empty())
}

struct Session {
    cfg: LiveConfig,
    connect_timeout: Duration,
    end_grace: Duration,
    /// Set by `end()`: when this moment passes and the service has not closed, the session is closed from our side.
    end_deadline: Option<tokio::time::Instant>,
    events: UnboundedSender<LiveEvent>,
    /// Upstream tasks report here. We keep one sender so the receiver never reports "closed".
    up_tx: UnboundedSender<(u64, UpEvent)>,
    up_rx: UnboundedReceiver<(u64, UpEvent)>,
    /// Bumps on every (re)start so events from an abandoned upstream are ignored.
    session: u64,
    provider: Option<Provider>,
    up: Option<Upstream>,
    ready: bool,
    ending: bool,
    queue: VecDeque<Vec<u8>>, // audio that arrived before the service was ready
    queue_bytes: usize,
}

impl Session {
    fn new(cfg: LiveConfig, connect_timeout: Duration, end_grace: Duration, events: UnboundedSender<LiveEvent>) -> Self {
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        Session {
            cfg,
            connect_timeout,
            end_grace,
            end_deadline: None,
            events,
            up_tx,
            up_rx,
            session: 0,
            provider: None,
            up: None,
            ready: false,
            ending: false,
            queue: VecDeque::new(),
            queue_bytes: 0,
        }
    }

    fn tell(&self, e: LiveEvent) {
        // The receiver may be gone (the app left); nothing to do then.
        let _ = self.events.send(e);
    }

    /// Both keys, for hiding them in any text that might leave this crate.
    fn secrets(&self) -> Vec<String> {
        [&self.cfg.sarvam_key, &self.cfg.gemini_key].into_iter().filter_map(non_empty).map(String::from).collect()
    }

    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>) {
        let first = if non_empty(&self.cfg.sarvam_key).is_some() {
            Provider::Sarvam
        } else if non_empty(&self.cfg.gemini_key).is_some() {
            Provider::Gemini
        } else {
            self.tell(LiveEvent::Error("Live captions need a Sarvam or Gemini API key.".into()));
            self.tell(LiveEvent::Closed);
            return;
        };
        self.start_upstream(first);

        loop {
            let deadline = self.end_deadline; // copied out, so the select below can borrow `self` for the other branches
            tokio::select! {
                // the person stopped speaking and the service never closed: leave (the last caption has had its time)
                _ = grace_over(deadline) => {
                    self.shutdown();
                    return;
                }
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Pcm(pcm)) => self.on_pcm(pcm),
                    Some(Cmd::End) => self.on_end(),
                    // Close, or every handle was dropped.
                    Some(Cmd::Close) | None => {
                        self.shutdown();
                        return;
                    }
                },
                Some((id, event)) = self.up_rx.recv() => {
                    // An event from an upstream we gave up on is ignored.
                    if id != self.session {
                        continue;
                    }
                    if self.on_upstream_event(event) {
                        return;
                    }
                }
            }
        }
    }

    /// Opens a connection to `provider`. The old one (if any) must already be closed.
    fn start_upstream(&mut self, provider: Provider) {
        self.session += 1;
        self.ready = false;
        self.provider = Some(provider);
        let key = match provider {
            Provider::Sarvam => non_empty(&self.cfg.sarvam_key),
            Provider::Gemini => non_empty(&self.cfg.gemini_key),
        }
        .unwrap_or_default()
        .to_string();
        self.up = Some(upstream::spawn(
            provider,
            &self.cfg,
            key,
            self.secrets(),
            self.session,
            self.connect_timeout,
            self.up_tx.clone(),
        ));
    }

    fn on_pcm(&mut self, pcm: Vec<u8>) {
        // After end() nothing more goes out: the service bills by the audio it receives.
        if self.ending {
            return;
        }
        if self.ready {
            self.forward(pcm);
            return;
        }
        self.queue_bytes += pcm.len();
        self.queue.push_back(pcm);
        while self.queue.len() > MAX_QUEUE || self.queue_bytes > MAX_QUEUE_BYTES {
            match self.queue.pop_front() {
                Some(old) => self.queue_bytes -= old.len(),
                None => break,
            }
        }
    }

    fn on_end(&mut self) {
        if self.ending {
            return;
        }
        self.ending = true;
        self.end_deadline = Some(tokio::time::Instant::now() + self.end_grace);
        // Before the service is ready, the end message is sent once it is (after the queued audio).
        if self.ready {
            self.send_end();
        }
    }

    fn forward(&self, pcm: Vec<u8>) {
        if let (Some(up), Some(provider)) = (&self.up, self.provider) {
            up.send(UpCmd::Send(provider.audio_message(&pcm)));
        }
    }

    fn send_end(&self) {
        if let (Some(up), Some(provider)) = (&self.up, self.provider) {
            up.send(UpCmd::Send(provider.end_message()));
        }
    }

    /// Returns true when the session is over and the task should stop.
    fn on_upstream_event(&mut self, event: UpEvent) -> bool {
        let provider = match self.provider {
            Some(p) => p,
            None => return false,
        };
        match event {
            UpEvent::Ready => {
                if self.ready {
                    return false;
                }
                self.ready = true;
                self.tell(LiveEvent::Ready { provider: provider.display_name().to_string() });
                while let Some(pcm) = self.queue.pop_front() {
                    self.forward(pcm);
                }
                self.queue_bytes = 0;
                if self.ending {
                    self.send_end();
                }
                false
            }
            UpEvent::Interim(text) => {
                self.tell(LiveEvent::Interim(text));
                false
            }
            UpEvent::Final(text) => {
                self.tell(LiveEvent::Final(text));
                false
            }
            UpEvent::Closed => {
                self.tell(LiveEvent::Closed);
                self.close_upstream();
                true
            }
            UpEvent::Fail(reason) => {
                let reason = mask_keys(&reason, &self.secrets());
                self.session += 1; // ignore anything else this upstream says
                self.close_upstream();
                self.ready = false;
                // Sarvam failed: Gemini takes over the same session, unless there is no Gemini key or the speaker already stopped.
                if provider == Provider::Sarvam && non_empty(&self.cfg.gemini_key).is_some() && !self.ending {
                    tracing::error!("[live] Sarvam failed ({reason}); switching to Gemini");
                    self.tell(LiveEvent::Notice(format!(
                        "{reason} Gemini live captions are taking over; the recording is not affected."
                    )));
                    self.start_upstream(Provider::Gemini);
                    return false;
                }
                self.tell(LiveEvent::Error(reason));
                self.tell(LiveEvent::Closed);
                true
            }
        }
    }

    fn close_upstream(&mut self) {
        if let Some(up) = self.up.take() {
            up.send(UpCmd::Close);
        }
    }

    fn shutdown(&mut self) {
        self.session += 1;
        self.close_upstream();
        self.queue.clear();
        self.queue_bytes = 0;
        self.tell(LiveEvent::Closed);
    }
}

/// Waits until `deadline` (never, when there is none).
async fn grace_over(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Replaces every secret in `text` so a key can never reach the screen or the log.
pub(crate) fn mask_keys(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s.as_str(), "[key hidden]");
    }
    out
}

/// What the person reads when live captions fail (port of `liveReason`). The raw text goes to the log, never the screen.
pub fn live_reason(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if pechu_common::is_network_error(raw) || lower.starts_with("connection ended") || has_code_number(&lower) {
        return "Live captions lost their connection.".into();
    }
    if ["quota", "resource_exhausted", "rate limit"].iter().any(|p| lower.contains(p)) || has_word(&lower, "429") {
        return "Live captions hit a usage limit.".into();
    }
    if ["api key", "permission_denied", "unauthenticated"].iter().any(|p| lower.contains(p))
        || has_word(&lower, "401")
        || has_word(&lower, "403")
    {
        return "The live captions service did not accept the API key.".into();
    }
    if raw.chars().count() > 120 || raw.contains('{') || raw.contains('}') {
        return "Live captions had a problem.".into();
    }
    raw.to_string()
}

/// Like `live_reason`, and writes the raw text to the log first (keys hidden). `who` is "Sarvam" or "Google".
pub(crate) fn live_reason_logged(raw: &str, who: &str, secrets: &[String]) -> String {
    tracing::error!("[live] {who} said: {}", mask_keys(raw, secrets));
    live_reason(raw)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Regex `\bword\b` for a word made of digits/letters: it must not touch another word character on either side.
fn has_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + word.len()..].chars().next();
        !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
    })
}

/// Regex `\bcode \d{4}\b` (on already lower-cased text), e.g. "connection ended (code 1006)".
fn has_code_number(lower: &str) -> bool {
    lower.match_indices("code ").any(|(i, _)| {
        if lower[..i].chars().next_back().is_some_and(is_word_char) {
            return false;
        }
        let rest = &lower[i + 5..];
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        digits == 4 && !rest[digits..].chars().next().is_some_and(is_word_char)
    })
}

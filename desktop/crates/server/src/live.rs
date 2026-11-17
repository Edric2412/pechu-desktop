//! The WebSocket `/api/live`: joins the browser to `pechu_live`. The browser sends 16 kHz PCM audio as binary frames and the
//! text "end"; it gets JSON messages back that the web UI already understands. (Port of the browser-facing part of
//! server/live.ts and server/usage.ts.)
use axum::extract::ws::{Message, WebSocket};
use pechu_live::{LiveEvent, LiveHandle};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

/// The JSON text the browser gets for one event. This is the whole "protocol" between server and page.
pub fn event_json(event: &LiveEvent) -> String {
    let value = match event {
        LiveEvent::Ready { provider } => json!({ "type": "ready", "provider": provider }),
        LiveEvent::Interim(text) => json!({ "type": "interim", "text": text }),
        LiveEvent::Final(text) => json!({ "type": "final", "text": text }),
        LiveEvent::Notice(message) => json!({ "type": "notice", "message": message }),
        LiveEvent::Error(message) => json!({ "type": "error", "message": message }),
        LiveEvent::Closed => json!({ "type": "closed" }),
    };
    value.to_string()
}

/// `auto`, or a language code like `ta` or `en-IN`. (The same rule as the web version.)
pub fn lang_ok(lang: &str) -> bool {
    if lang == "auto" {
        return true;
    }
    let mut parts = lang.split('-');
    let first = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    (2..=3).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_alphabetic())
        && rest.len() <= 3
        && rest.iter().all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The browser's id for one recording: `^[A-Za-z0-9-]{8,64}$`.
pub fn rid_ok(rid: &str) -> bool {
    (8..=64).contains(&rid.len()) && rid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

// ----- counting the audio each service received (it is billed by the second) -----

#[derive(Clone, Copy, Default)]
struct Bytes {
    sarvam: u64,
    gemini: u64,
}

/// Live captions are started before the meeting exists, so what they used waits here under the browser's recording id (`rid`)
/// and is attached to the meeting when the browser creates it.
#[derive(Default)]
pub struct LiveUsage {
    by_rid: Mutex<HashMap<String, Bytes>>,
}

const BYTES_PER_SECOND: f64 = 16_000.0 * 2.0; // 16 kHz, 16-bit mono

impl LiveUsage {
    fn add(&self, rid: &str, sarvam: u64, gemini: u64) {
        let mut map = self.by_rid.lock().unwrap();
        let entry = map.entry(rid.to_string()).or_default();
        entry.sarvam += sarvam;
        entry.gemini += gemini;
    }

    /// `{sarvamLiveSec?, geminiLiveSec?}` (only the non-zero ones, to a tenth of a second), and forgets the recording.
    pub fn take(&self, rid: &str) -> Value {
        let Some(bytes) = self.by_rid.lock().unwrap().remove(rid) else { return json!({}) };
        let mut out = serde_json::Map::new();
        for (name, count) in [("sarvamLiveSec", bytes.sarvam), ("geminiLiveSec", bytes.gemini)] {
            let seconds = (count as f64 / BYTES_PER_SECOND * 10.0).round() / 10.0;
            if seconds > 0.0 {
                out.insert(name.to_string(), json!(seconds));
            }
        }
        Value::Object(out)
    }
}

/// Works out which service each audio chunk went to: the one that last said "ready". Audio sent before that is counted
/// for whichever service becomes ready first.
struct Meter {
    usage: Arc<LiveUsage>,
    rid: Option<String>,
    provider_is_sarvam: Option<bool>,
    waiting: u64,
}

impl Meter {
    fn on_event(&mut self, event: &LiveEvent) {
        if let LiveEvent::Ready { provider } = event {
            self.provider_is_sarvam = Some(provider.to_lowercase().contains("sarvam"));
            let waiting = std::mem::take(&mut self.waiting);
            self.count(waiting);
        }
    }

    fn on_audio(&mut self, bytes: usize) {
        if self.provider_is_sarvam.is_some() {
            self.count(bytes as u64);
        } else {
            self.waiting += bytes as u64;
        }
    }

    fn count(&self, bytes: u64) {
        let (Some(rid), Some(sarvam)) = (&self.rid, self.provider_is_sarvam) else { return };
        if sarvam {
            self.usage.add(rid, bytes, 0);
        } else {
            self.usage.add(rid, 0, bytes);
        }
    }
}

/// Counts open live sessions so there can never be more than `max`. The slot is given back when the guard is dropped.
pub struct SessionSlots {
    open: AtomicUsize,
    max: usize,
}

pub struct SlotGuard(Arc<SessionSlots>);

impl SessionSlots {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(SessionSlots { open: AtomicUsize::new(0), max })
    }

    /// None when all slots are taken.
    pub fn take(self: &Arc<Self>) -> Option<SlotGuard> {
        let taken = self.open.fetch_add(1, Ordering::SeqCst);
        if taken >= self.max {
            self.open.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(SlotGuard(self.clone()))
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs one browser connection until either side ends it.
pub async fn bridge(
    mut socket: WebSocket,
    handle: LiveHandle,
    mut events: UnboundedReceiver<LiveEvent>,
    usage: Arc<LiveUsage>,
    rid: Option<String>,
    mut stop: watch::Receiver<bool>,
    _slot: SlotGuard,
) {
    let mut meter = Meter { usage, rid, provider_is_sarvam: None, waiting: 0 };
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break }; // the service side is gone
                meter.on_event(&event);
                let closed = event == LiveEvent::Closed;
                if socket.send(Message::Text(event_json(&event).into())).await.is_err() || closed {
                    break;
                }
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(pcm))) => {
                    meter.on_audio(pcm.len());
                    handle.send_pcm(pcm.to_vec());
                }
                Some(Ok(Message::Text(text))) if text.as_str() == "end" => handle.end(),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break, // the browser left
                Some(Ok(_)) => {} // pings are answered by the library; anything else is ignored
            },
            _ = stop.changed() => break, // the app is closing
        }
    }
    handle.close();
    let _ = socket.send(Message::Close(None)).await; // a polite goodbye; the socket closes when it is dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_become_the_messages_the_page_expects() {
        let j = |e: LiveEvent| serde_json::from_str::<Value>(&event_json(&e)).unwrap();
        assert_eq!(j(LiveEvent::Ready { provider: "Sarvam Saaras v4".into() }), json!({"type": "ready", "provider": "Sarvam Saaras v4"}));
        assert_eq!(j(LiveEvent::Interim("hel".into())), json!({"type": "interim", "text": "hel"}));
        assert_eq!(j(LiveEvent::Final("hello".into())), json!({"type": "final", "text": "hello"}));
        assert_eq!(j(LiveEvent::Notice("switched".into())), json!({"type": "notice", "message": "switched"}));
        assert_eq!(j(LiveEvent::Error("bad".into())), json!({"type": "error", "message": "bad"}));
        assert_eq!(j(LiveEvent::Closed), json!({"type": "closed"}));
    }

    #[test]
    fn text_is_escaped_not_pasted() {
        let text = event_json(&LiveEvent::Final("say \"hi\"\nதமிழ்".into()));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["text"], "say \"hi\"\nதமிழ்");
    }

    #[test]
    fn language_codes() {
        for ok in ["auto", "ta", "en", "en-IN", "zh-Hans-CN", "fil"] {
            assert!(lang_ok(ok), "{ok}");
        }
        for bad in ["", "e", "english", "en_IN", "en-", "ta;rm", "../x", "en-IN-a1-b2-c3", "t1"] {
            assert!(!lang_ok(bad), "{bad}");
        }
    }

    #[test]
    fn recording_ids() {
        assert!(rid_ok("abcd1234"));
        assert!(rid_ok("a1b2c3d4-e5f6-4789-aabb-ccddeeff0011"));
        assert!(!rid_ok("short"));
        assert!(!rid_ok("has space in it"));
        assert!(!rid_ok(&"a".repeat(65)));
    }

    #[test]
    fn session_slots_run_out_and_come_back() {
        let slots = SessionSlots::new(2);
        let a = slots.take().expect("first");
        let _b = slots.take().expect("second");
        assert!(slots.take().is_none(), "a third is refused");
        drop(a);
        assert!(slots.take().is_some(), "a freed slot can be used again");
    }

    #[test]
    fn audio_is_counted_for_the_service_that_was_ready() {
        let usage = Arc::new(LiveUsage::default());
        let mut meter = Meter { usage: usage.clone(), rid: Some("rid-12345".into()), provider_is_sarvam: None, waiting: 0 };
        meter.on_audio(32_000); // before any service is ready: held back
        meter.on_event(&LiveEvent::Ready { provider: "Sarvam Saaras v4".into() });
        meter.on_audio(64_000);
        meter.on_event(&LiveEvent::Ready { provider: "Gemini 3.5 Transcribe Live".into() }); // Sarvam failed, Gemini took over
        meter.on_audio(16_000);
        assert_eq!(usage.take("rid-12345"), json!({"sarvamLiveSec": 3.0, "geminiLiveSec": 0.5}));
        assert_eq!(usage.take("rid-12345"), json!({}), "taken once");
    }
}

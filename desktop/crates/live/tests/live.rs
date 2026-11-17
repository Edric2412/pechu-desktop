//! Offline tests: the "services" are small WebSocket servers on 127.0.0.1 that this file starts itself.
//! No real key and no real service is ever involved. The main thing proved here is the billing rule: the bytes the fake
//! services receive are exactly the bytes the app sent (nothing early, nothing after end/close, nothing lost or doubled).
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use pechu_live::{live_reason, start, start_with_timeout, start_with_timeouts, LiveConfig, LiveEvent, LiveHandle, LiveMode};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

const SARVAM_KEY: &str = "sk_test_SARVAM_secret_123";
const GEMINI_KEY: &str = "AIza_test_GEMINI_secret_456";

// ---------- the fake service ----------

/// What the fake sends back.
#[derive(Clone)]
enum Out {
    Text(String),
    Close(u16, &'static str),
    /// A close frame with no code at all (what browsers and libraries report as 1005).
    CloseNoCode,
}

fn text(v: Value) -> Out {
    Out::Text(v.to_string())
}

type Reply = Box<dyn Fn(&Value) -> Vec<Out> + Send + Sync>;

struct Script {
    /// Wait for `Fake::open()` before sending `on_connect` (frames that arrive meanwhile are still recorded).
    gated: bool,
    on_connect: Vec<Out>,
    /// Called with every JSON text frame the app sends.
    reply: Reply,
}

#[derive(Default)]
struct State {
    frames: Mutex<Vec<String>>,
    uris: Mutex<Vec<String>>,
    api_keys: Mutex<Vec<String>>,
    connections: AtomicUsize,
    client_gone: AtomicBool,
    gate: Notify,
}

struct Fake {
    url: String,
    state: Arc<State>,
}

impl Fake {
    async fn start(script: Script) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let state = Arc::new(State::default());
        let script = Arc::new(script);
        let st = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, script.clone(), st.clone()));
            }
        });
        Fake { url, state }
    }

    fn open(&self) {
        self.state.gate.notify_one();
    }

    fn frames(&self) -> Vec<Value> {
        self.state.frames.lock().unwrap().iter().map(|f| serde_json::from_str(f).unwrap()).collect()
    }

    /// The audio the fake received, one entry per chunk, decoded back to bytes.
    fn audio(&self) -> Vec<Vec<u8>> {
        self.frames().iter().filter_map(decode_audio).collect()
    }

    fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }
}

fn decode_audio(frame: &Value) -> Option<Vec<u8>> {
    let b64 = frame["audio"].as_str().or_else(|| frame["realtimeInput"]["audio"]["data"].as_str())?;
    Some(base64::engine::general_purpose::STANDARD.decode(b64).unwrap())
}

// The handshake callback's Err type is the library's, and clippy cannot change it.
#[allow(clippy::result_large_err)]
async fn serve(stream: tokio::net::TcpStream, script: Arc<Script>, st: Arc<State>) {
    let st2 = st.clone();
    let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
        st2.uris.lock().unwrap().push(req.uri().to_string());
        if let Some(k) = req.headers().get("api-subscription-key") {
            st2.api_keys.lock().unwrap().push(k.to_str().unwrap().to_string());
        }
        Ok(resp)
    })
    .await
    .unwrap();
    st.connections.fetch_add(1, Ordering::SeqCst);
    let (mut tx, mut rx) = ws.split();

    let record = |t: &str| st.frames.lock().unwrap().push(t.to_string());

    if script.gated {
        loop {
            tokio::select! {
                _ = st.gate.notified() => break,
                msg = rx.next() => match msg {
                    Some(Ok(Message::Text(t))) => record(t.as_str()),
                    Some(Ok(_)) => {}
                    _ => { st.client_gone.store(true, Ordering::SeqCst); return; }
                },
            }
        }
    }
    for out in &script.on_connect {
        send_out(&mut tx, out).await;
    }
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => {
                record(t.as_str());
                for out in (script.reply)(&serde_json::from_str(t.as_str()).unwrap()) {
                    send_out(&mut tx, &out).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    st.client_gone.store(true, Ordering::SeqCst);
}

async fn send_out<S>(tx: &mut S, out: &Out)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let msg = match out {
        Out::Text(t) => Message::text(t.clone()),
        Out::Close(code, reason) => Message::Close(Some(CloseFrame { code: CloseCode::from(*code), reason: (*reason).into() })),
        Out::CloseNoCode => Message::Close(None),
    };
    let _ = tx.send(msg).await;
}

// ---------- ready-made fake services ----------

fn sarvam_begin() -> Out {
    text(json!({ "event": "session.begin" }))
}

/// A Sarvam that says hello, answers each audio chunk with a partial and a final, and closes normally after "end".
fn sarvam_friendly(gated: bool) -> Script {
    Script {
        gated,
        on_connect: vec![sarvam_begin()],
        reply: Box::new(|m| match m["event"].as_str() {
            Some("audio_input") => vec![
                text(json!({ "event": "transcript.partial", "text": "hel" })),
                text(json!({ "event": "transcript.final", "text": "hello" })),
            ],
            Some("end") => vec![Out::Close(1000, "")],
            _ => vec![],
        }),
    }
}

/// A Gemini that accepts setup and closes normally after audioStreamEnd.
fn gemini_friendly() -> Script {
    Script {
        gated: false,
        on_connect: vec![],
        reply: Box::new(|m| {
            if !m["setup"].is_null() {
                vec![text(json!({ "setupComplete": {} }))]
            } else if !m["realtimeInput"]["audio"].is_null() {
                vec![
                    text(json!({ "serverContent": { "interimInputTranscription": { "text": "vanak" } } })),
                    text(json!({ "serverContent": { "inputTranscription": { "text": "vanakkam" } } })),
                ]
            } else if m["realtimeInput"]["audioStreamEnd"] == json!(true) {
                vec![Out::Close(1000, "")]
            } else {
                vec![]
            }
        }),
    }
}

fn silent_script(on_connect: Vec<Out>, gated: bool) -> Script {
    Script { gated, on_connect, reply: Box::new(|_| vec![]) }
}

// ---------- helpers ----------

fn cfg(sarvam: Option<&Fake>, gemini: Option<&Fake>) -> LiveConfig {
    LiveConfig {
        mode: LiveMode::Smart,
        langs: vec!["ta".into(), "en-IN".into()],
        sarvam_key: sarvam.map(|_| SARVAM_KEY.to_string()),
        gemini_key: gemini.map(|_| GEMINI_KEY.to_string()),
        sarvam_url: sarvam.map(|f| f.url.clone()),
        gemini_url: gemini.map(|f| f.url.clone()),
    }
}

async fn next(rx: &mut UnboundedReceiver<LiveEvent>) -> LiveEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("no event within 5 s").expect("event channel closed early")
}

/// Everything up to and including `Closed` (or until the channel ends).
async fn until_closed(rx: &mut UnboundedReceiver<LiveEvent>) -> Vec<LiveEvent> {
    let mut all = Vec::new();
    loop {
        let e = next(rx).await;
        let done = e == LiveEvent::Closed;
        all.push(e);
        if done {
            return all;
        }
    }
}

async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("gave up waiting for: {what}");
}

/// Time for the app to (wrongly) send something, so "nothing arrived" means something.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// A chunk whose first two bytes say which chunk it is, so order can be checked.
fn chunk(index: u16, len: usize) -> Vec<u8> {
    let mut v = vec![7u8; len];
    v[..2].copy_from_slice(&index.to_be_bytes());
    v
}

fn tag(c: &[u8]) -> u16 {
    u16::from_be_bytes([c[0], c[1]])
}

fn send(h: &LiveHandle, pcm: &[u8]) {
    h.send_pcm(pcm.to_vec());
}

// ---------- Sarvam ----------

#[tokio::test]
async fn sarvam_happy_path() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));

    assert_eq!(next(&mut rx).await, LiveEvent::Ready { provider: "Sarvam Saaras v4".into() });
    send(&h, &chunk(1, 320));
    assert_eq!(next(&mut rx).await, LiveEvent::Interim("hel".into()));
    assert_eq!(next(&mut rx).await, LiveEvent::Final("hello".into()));
    h.end();
    assert_eq!(next(&mut rx).await, LiveEvent::Closed);

    // The request: key in the header (not in the address), and the documented settings in the query.
    assert_eq!(sarvam.state.api_keys.lock().unwrap().as_slice(), [SARVAM_KEY]);
    let uri = sarvam.state.uris.lock().unwrap()[0].clone();
    for part in [
        "language_code=ta",
        "model=saaras%3Av4",
        "stream_type=balanced",
        "mode=codemix",
        "encoding=linear16",
        "sample_rate=16000",
    ] {
        assert!(uri.contains(part), "{uri} should contain {part}");
    }
    assert!(!uri.contains(SARVAM_KEY));

    let frames = sarvam.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["event"], "audio_input");
    assert_eq!(frames[1], json!({ "event": "end" }));
}

#[tokio::test]
async fn sarvam_auto_language_and_verbatim() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let mut c = cfg(Some(&sarvam), None);
    c.langs.clear();
    c.mode = LiveMode::Verbatim;
    let (h, mut rx) = start(c);
    next(&mut rx).await; // Ready
    h.close();
    let uri = sarvam.state.uris.lock().unwrap()[0].clone();
    assert!(uri.contains("language_code=auto") && uri.contains("mode=verbatim"), "{uri}");
}

#[tokio::test]
async fn audio_before_ready_is_queued_and_delivered_in_order() {
    let sarvam = Fake::start(sarvam_friendly(true)).await; // will not say hello until opened
    let (h, mut rx) = start(cfg(Some(&sarvam), None));

    let mut sent: Vec<Vec<u8>> = (0..10).map(|i| chunk(i, 100 + i as usize * 13)).collect();
    for c in &sent {
        send(&h, c);
    }
    settle().await;
    assert!(sarvam.audio().is_empty(), "no audio may be sent before the service is ready");

    sarvam.open();
    assert_eq!(next(&mut rx).await, LiveEvent::Ready { provider: "Sarvam Saaras v4".into() });
    // and audio sent after ready goes straight through, after the queued part
    let late = chunk(99, 500);
    send(&h, &late);
    sent.push(late);

    wait_until("all chunks arrive", || sarvam.audio().len() == sent.len()).await;
    let got = sarvam.audio();
    // billing check: same chunks, same order, same total bytes
    assert_eq!(got, sent);
    assert_eq!(got.iter().map(Vec::len).sum::<usize>(), sent.iter().map(Vec::len).sum::<usize>());
}

#[tokio::test]
async fn queue_keeps_at_most_600_chunks_and_drops_the_oldest() {
    let sarvam = Fake::start(sarvam_friendly(true)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    for i in 0..700 {
        send(&h, &chunk(i, 100));
    }
    settle().await;
    sarvam.open();
    next(&mut rx).await; // Ready
    wait_until("600 chunks arrive", || sarvam.audio().len() == 600).await;
    settle().await;
    let tags: Vec<u16> = sarvam.audio().iter().map(|c| tag(c)).collect();
    assert_eq!(tags.len(), 600, "no more than 600 may be delivered");
    assert_eq!(tags, (100..700).collect::<Vec<u16>>());
}

#[tokio::test]
async fn queue_keeps_at_most_2_mib_and_drops_the_oldest() {
    let sarvam = Fake::start(sarvam_friendly(true)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    const MIB: usize = 1024 * 1024;
    for i in 0..5 {
        send(&h, &chunk(i, MIB)); // five 1 MiB chunks: only the last two fit
    }
    settle().await;
    sarvam.open();
    next(&mut rx).await; // Ready
    wait_until("2 chunks arrive", || sarvam.audio().len() == 2).await;
    settle().await;
    let audio = sarvam.audio();
    assert_eq!(audio.iter().map(|c| tag(c)).collect::<Vec<_>>(), vec![3, 4]);
    assert_eq!(audio.iter().map(Vec::len).sum::<usize>(), 2 * MIB);
}

#[tokio::test]
async fn sarvam_fatal_error_hands_over_to_gemini_with_a_notice() {
    let sarvam = Fake::start(Script {
        gated: false,
        on_connect: vec![sarvam_begin()],
        reply: Box::new(|m| match m["event"].as_str() {
            Some("audio_input") => vec![
                // not fatal: must be ignored
                text(json!({ "event": "error", "is_fatal": false, "message": "just a hiccup" })),
                text(json!({ "event": "error", "is_fatal": true, "message": "Quota exceeded for this key" })),
            ],
            _ => vec![],
        }),
    })
    .await;
    let gemini = Fake::start(gemini_friendly()).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), Some(&gemini)));

    assert_eq!(next(&mut rx).await, LiveEvent::Ready { provider: "Sarvam Saaras v4".into() });
    let first = chunk(1, 640);
    send(&h, &first);
    let notice = next(&mut rx).await;
    assert_eq!(
        notice,
        LiveEvent::Notice(
            "Live captions hit a usage limit. Gemini live captions are taking over; the recording is not affected.".into()
        )
    );

    // Audio sent while Gemini is starting is queued and reaches Gemini, never Sarvam.
    let second = chunk(2, 640);
    send(&h, &second);
    let mut events = vec![];
    for _ in 0..3 {
        events.push(next(&mut rx).await);
    }
    assert_eq!(events[0], LiveEvent::Ready { provider: "Gemini 3.5 Transcribe Live".into() });
    assert!(events.contains(&LiveEvent::Interim("vanak".into())));
    assert!(events.contains(&LiveEvent::Final("vanakkam".into())));

    assert_eq!(sarvam.audio(), vec![first]);
    assert_eq!(gemini.audio(), vec![second]);

    // The Gemini request carries the settings in the first message.
    let setup = &gemini.frames()[0]["setup"];
    assert_eq!(setup["model"], "models/gemini-3.5-transcribe-live");
    assert_eq!(setup["inputAudioTranscription"]["languageCodes"], json!(["ta", "en-IN"]));
    assert_eq!(setup["inputAudioTranscription"]["mode"], "SMART");
    assert_eq!(gemini.frames()[1]["realtimeInput"]["audio"]["mimeType"], "audio/pcm;rate=16000");

    h.end();
    assert_eq!(next(&mut rx).await, LiveEvent::Closed);
    assert_eq!(gemini.frames().last().unwrap()["realtimeInput"]["audioStreamEnd"], json!(true));
}

#[tokio::test]
async fn sarvam_close_codes_are_explained() {
    for (code, why) in [(1003u16, "the key was refused or the quota is used up"), (4000, "a setting was refused"), (1011, "code 1011")] {
        let sarvam = Fake::start(silent_script(vec![Out::Close(code, "raw text that must not be shown")], false)).await;
        let (_h, mut rx) = start(cfg(Some(&sarvam), None));
        assert_eq!(
            until_closed(&mut rx).await,
            vec![LiveEvent::Error(format!("Sarvam live transcription ended ({why}).")), LiveEvent::Closed]
        );
    }
}

#[tokio::test]
async fn sarvam_normal_close_codes_end_the_session_quietly() {
    for close in [Out::Close(1000, ""), Out::CloseNoCode] {
        let sarvam = Fake::start(silent_script(vec![sarvam_begin(), close], false)).await;
        let (_h, mut rx) = start(cfg(Some(&sarvam), None));
        assert_eq!(until_closed(&mut rx).await, vec![LiveEvent::Ready { provider: "Sarvam Saaras v4".into() }, LiveEvent::Closed]);
    }
}

#[tokio::test]
async fn sarvam_connect_timeout_without_gemini_key_is_an_error() {
    // The service accepts the connection but never says "session.begin".
    let sarvam = Fake::start(silent_script(vec![sarvam_begin()], true)).await;
    let (h, mut rx) = start_with_timeout(cfg(Some(&sarvam), None), Duration::from_millis(300));
    send(&h, &chunk(1, 100));
    assert_eq!(
        until_closed(&mut rx).await,
        vec![LiveEvent::Error("Timed out connecting to Sarvam's live transcription.".into()), LiveEvent::Closed]
    );
    assert!(sarvam.audio().is_empty(), "audio must never be sent to a service that is not ready");
}

#[tokio::test]
async fn sarvam_connect_timeout_with_gemini_key_falls_back() {
    let sarvam = Fake::start(silent_script(vec![sarvam_begin()], true)).await;
    let gemini = Fake::start(gemini_friendly()).await;
    let (h, mut rx) = start_with_timeout(cfg(Some(&sarvam), Some(&gemini)), Duration::from_millis(400));
    let c = chunk(1, 200);
    send(&h, &c);
    assert_eq!(
        next(&mut rx).await,
        LiveEvent::Notice(
            "Timed out connecting to Sarvam's live transcription. Gemini live captions are taking over; the recording is not affected."
                .into()
        )
    );
    assert_eq!(next(&mut rx).await, LiveEvent::Ready { provider: "Gemini 3.5 Transcribe Live".into() });
    wait_until("chunk reaches Gemini", || gemini.audio().len() == 1).await;
    assert_eq!(gemini.audio(), vec![c]);
    assert!(sarvam.audio().is_empty());
}

#[tokio::test]
async fn nothing_listening_is_an_error_not_a_hang() {
    // Port 9 (discard) on localhost: nothing there, the connection is refused at once.
    let mut c = cfg(None, None);
    c.sarvam_key = Some(SARVAM_KEY.into());
    c.sarvam_url = Some("ws://127.0.0.1:9/ws".into());
    let (_h, mut rx) = start(c);
    assert_eq!(
        until_closed(&mut rx).await,
        vec![LiveEvent::Error("Could not reach Sarvam's live transcription.".into()), LiveEvent::Closed]
    );
}

// ---------- Gemini ----------

#[tokio::test]
async fn gemini_happy_path_when_there_is_no_sarvam_key() {
    let gemini = Fake::start(gemini_friendly()).await;
    let mut c = cfg(None, Some(&gemini));
    c.mode = LiveMode::Verbatim;
    let (h, mut rx) = start(c);
    assert_eq!(next(&mut rx).await, LiveEvent::Ready { provider: "Gemini 3.5 Transcribe Live".into() });
    send(&h, &chunk(1, 320));
    assert_eq!(next(&mut rx).await, LiveEvent::Interim("vanak".into()));
    assert_eq!(next(&mut rx).await, LiveEvent::Final("vanakkam".into()));
    h.end();
    assert_eq!(next(&mut rx).await, LiveEvent::Closed);
    assert_eq!(gemini.frames()[0]["setup"]["inputAudioTranscription"]["mode"], "VERBATIM");
    // the key goes in the address, as the Google protocol wants
    assert!(gemini.state.uris.lock().unwrap()[0].contains(&format!("key={GEMINI_KEY}")));
}

#[tokio::test]
async fn gemini_failure_ends_the_session_with_plain_words_and_never_shows_the_key() {
    let gemini = Fake::start(silent_script(vec![Out::Close(1011, "internal problem with AIza_test_GEMINI_secret_456 inside")], false)).await;
    let (_h, mut rx) = start(cfg(None, Some(&gemini)));
    let events = until_closed(&mut rx).await;
    assert_eq!(events.len(), 2);
    let LiveEvent::Error(msg) = &events[0] else { panic!("expected an error, got {:?}", events[0]) };
    assert!(!msg.contains(GEMINI_KEY), "the key leaked: {msg}");
    assert!(msg.contains("internal problem"));

    // an error message sent by the service itself is turned into plain words too
    let gemini = Fake::start(silent_script(
        vec![text(json!({ "error": { "message": "RESOURCE_EXHAUSTED: quota" } }))],
        false,
    ))
    .await;
    let (_h, mut rx) = start(cfg(None, Some(&gemini)));
    assert_eq!(
        until_closed(&mut rx).await,
        vec![LiveEvent::Error("Live captions hit a usage limit.".into()), LiveEvent::Closed]
    );
}

// ---------- no keys ----------

#[tokio::test]
async fn no_keys_is_an_error_then_closed_and_nothing_is_contacted() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let mut c = cfg(None, None);
    c.sarvam_url = Some(sarvam.url.clone());
    c.sarvam_key = Some("   ".into()); // blank counts as no key
    let (h, mut rx) = start(c);
    let events = until_closed(&mut rx).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], LiveEvent::Error(m) if m.contains("API key")));
    send(&h, &chunk(1, 100));
    settle().await;
    assert_eq!(sarvam.connections(), 0);
}

// ---------- end() and close() ----------

#[tokio::test]
async fn after_end_no_more_audio_is_sent() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    next(&mut rx).await; // Ready
    let a = chunk(1, 320);
    send(&h, &a);
    h.end();
    send(&h, &chunk(2, 320)); // too late: must be dropped
    h.end(); // a second end is harmless
    until_closed(&mut rx).await;
    settle().await;
    let frames = sarvam.frames();
    assert_eq!(frames.iter().filter_map(decode_audio).collect::<Vec<_>>(), vec![a]);
    assert_eq!(frames.iter().filter(|f| f["event"] == "end").count(), 1);
    assert_eq!(frames.last().unwrap()["event"], "end");
}

/// The real Sarvam, measured on 2026-11-17 with a 5 s clip: it answers the audio, replies to "end" with the last caption,
/// and then simply stays connected (it never closes the socket itself).
fn sarvam_that_stays_open() -> Script {
    Script {
        gated: false,
        on_connect: vec![sarvam_begin()],
        reply: Box::new(|m| match m["event"].as_str() {
            Some("audio_input") => vec![text(json!({ "event": "transcript.final", "text": "hello" }))],
            _ => vec![],
        }),
    }
}

#[tokio::test]
async fn a_service_that_never_closes_after_end_is_closed_by_us_after_a_grace_period() {
    let sarvam = Fake::start(sarvam_that_stays_open()).await;
    let (h, mut rx) = start_with_timeouts(cfg(Some(&sarvam), None), Duration::from_secs(5), Duration::from_millis(300));
    next(&mut rx).await; // Ready
    send(&h, &chunk(1, 320));
    h.end();
    // without the grace period this waits for ever (the helper gives up after 5 s)
    let events = until_closed(&mut rx).await;
    assert!(events.contains(&LiveEvent::Final("hello".into())), "the last caption still arrives before we leave");
    assert_eq!(events.last(), Some(&LiveEvent::Closed));
    wait_until("the fake sees us leave", || sarvam.state.client_gone.load(Ordering::SeqCst)).await;
    assert_eq!(sarvam.audio().len(), 1, "and nothing more was sent");
}

#[tokio::test]
async fn end_before_ready_waits_for_the_queue_then_ends() {
    let sarvam = Fake::start(sarvam_friendly(true)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    let sent = vec![chunk(1, 100), chunk(2, 200)];
    for c in &sent {
        send(&h, c);
    }
    h.end();
    send(&h, &chunk(3, 100)); // after end: dropped even though it was never sent
    settle().await;
    assert!(sarvam.frames().is_empty());
    sarvam.open();
    let events = until_closed(&mut rx).await;
    assert_eq!(events[0], LiveEvent::Ready { provider: "Sarvam Saaras v4".into() });
    let kinds: Vec<String> = sarvam.frames().iter().map(|f| f["event"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["audio_input", "audio_input", "end"]);
    assert_eq!(sarvam.audio(), sent);
}

#[tokio::test]
async fn a_failure_after_end_does_not_start_gemini() {
    let sarvam = Fake::start(silent_script(vec![Out::Close(1011, "")], true)).await;
    let gemini = Fake::start(gemini_friendly()).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), Some(&gemini)));
    h.end();
    settle().await;
    sarvam.open();
    let events = until_closed(&mut rx).await;
    assert_eq!(
        events,
        vec![LiveEvent::Error("Sarvam live transcription ended (code 1011).".into()), LiveEvent::Closed]
    );
    settle().await;
    assert_eq!(gemini.connections(), 0);
}

#[tokio::test]
async fn close_stops_everything_and_later_audio_goes_nowhere() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    next(&mut rx).await; // Ready
    let a = chunk(1, 320);
    send(&h, &a);
    wait_until("first chunk arrives", || sarvam.audio().len() == 1).await;
    h.close();
    send(&h, &chunk(2, 320));
    h.end();
    assert_eq!(until_closed(&mut rx).await.last(), Some(&LiveEvent::Closed));
    wait_until("service sees the connection close", || sarvam.state.client_gone.load(Ordering::SeqCst)).await;
    settle().await;
    assert_eq!(sarvam.audio(), vec![a]);
    assert!(sarvam.frames().iter().all(|f| f["event"] != "end"));
}

#[tokio::test]
async fn dropping_every_handle_closes_the_connection() {
    let sarvam = Fake::start(sarvam_friendly(false)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    next(&mut rx).await; // Ready
    drop(h);
    wait_until("service sees the connection close", || sarvam.state.client_gone.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn close_while_connecting_never_sends_audio() {
    let sarvam = Fake::start(sarvam_friendly(true)).await;
    let (h, _rx) = start(cfg(Some(&sarvam), None));
    send(&h, &chunk(1, 100));
    h.close();
    settle().await;
    sarvam.open();
    settle().await;
    assert!(sarvam.audio().is_empty());
}

// ---------- billing accounting over a whole call ----------

#[tokio::test]
async fn bytes_received_equal_bytes_sent() {
    let sarvam = Fake::start(sarvam_friendly(true)).await;
    let (h, mut rx) = start(cfg(Some(&sarvam), None));
    let mut sent: Vec<Vec<u8>> = Vec::new();
    // some before ready, some after, awkward sizes
    for i in 0..40u16 {
        if i == 15 {
            sarvam.open();
            next(&mut rx).await; // Ready
        }
        let c = chunk(i, 2 + (i as usize * 37) % 900);
        send(&h, &c);
        sent.push(c);
    }
    h.end();
    until_closed(&mut rx).await;
    let got = sarvam.audio();
    assert_eq!(got.iter().map(Vec::len).sum::<usize>(), sent.iter().map(Vec::len).sum::<usize>());
    assert_eq!(got, sent);
}

// ---------- live_reason ----------

#[test]
fn live_reason_matches_the_web_version() {
    assert_eq!(live_reason("connection ended (code 1006)"), "Live captions lost their connection.");
    assert_eq!(live_reason("read ECONNRESET"), "Live captions lost their connection.");
    assert_eq!(live_reason("Quota exceeded for quota metric 'generate_content_free_tier_requests'"), "Live captions hit a usage limit.");
    assert!(live_reason("API key not valid. Please pass a valid API key.").contains("did not accept the API key"));
    assert_eq!(live_reason(r#"{"error":{"code":500,"message":"Internal error encountered."}}"#), "Live captions had a problem.");
    assert_eq!(live_reason(&"x".repeat(200)), "Live captions had a problem.");
    assert_eq!(live_reason("Live transcription failed."), "Live transcription failed.");

    // the rest of the patterns in server/live.ts
    assert_eq!(live_reason("Connection ended unexpectedly"), "Live captions lost their connection.");
    assert_eq!(live_reason("closed with code 1011 by peer"), "Live captions lost their connection.");
    assert_eq!(live_reason("RESOURCE_EXHAUSTED"), "Live captions hit a usage limit.");
    assert_eq!(live_reason("Rate limit reached"), "Live captions hit a usage limit.");
    assert_eq!(live_reason("HTTP 429"), "Live captions hit a usage limit.");
    assert_eq!(live_reason("PERMISSION_DENIED"), "The live captions service did not accept the API key.");
    assert_eq!(live_reason("UNAUTHENTICATED"), "The live captions service did not accept the API key.");
    assert_eq!(live_reason("error 401"), "The live captions service did not accept the API key.");
    assert_eq!(live_reason("403 forbidden"), "The live captions service did not accept the API key.");
    // a number inside a longer number is not a status code
    assert_eq!(live_reason("order 14290 failed"), "order 14290 failed");
    assert_eq!(live_reason("code 12345"), "code 12345");
    assert_eq!(live_reason("a {brace}"), "Live captions had a problem.");
    // exactly 120 characters is still short enough to show
    let s = "y".repeat(120);
    assert_eq!(live_reason(&s), s);
}

//! Offline tests: a small fake of Google's API runs on a random local port and `Gemini` talks to it instead of the
//! real service. No key, no internet. The fake records every request so the tests can check what was sent.
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pechu_common::{friendly, no_progress, progress, Progress, TranscribeMode};
use pechu_gemini::{Gemini, KeyCheck};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "AIzaSyFakeGeminiKey1234567890";

// --- the fake ---

/// What the fake answers to one `generateContent` call: after `delay_ms`, this status and this body.
struct Reply {
    delay_ms: u64,
    status: u16,
    body: Value,
}

/// A normal answer with this text.
fn text_reply(text: &str) -> Reply {
    Reply { delay_ms: 0, status: 200, body: json!({ "candidates": [{ "content": { "parts": [{ "text": text }] } }] }) }
}

fn error_reply(status: u16, message: &str) -> Reply {
    Reply { delay_ms: 0, status, body: json!({ "error": { "code": status, "message": message } }) }
}

/// Decides the answer from the request body and the number of this call (0, 1, 2 ...).
type GenerateFn = Box<dyn Fn(&Value, usize) -> Reply + Send + Sync>;

struct Fake {
    generate: GenerateFn,
    generate_calls: AtomicUsize,
    live: AtomicUsize,
    peak: AtomicUsize,
    models_status: u16,
    model_list_calls: AtomicUsize,
    /// A new file says PROCESSING this many times before it is ACTIVE.
    processing_polls: usize,
    polls: AtomicUsize,
    /// Answers of the interactions endpoint, one per call (the last one repeats).
    interactions: Vec<Reply>,
    interaction_calls: AtomicUsize,
    /// (method, path, body or header info) of every request, in order.
    log: Mutex<Vec<(String, String, Value)>>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            generate: Box::new(|_, _| text_reply("ok")),
            generate_calls: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            models_status: 200,
            model_list_calls: AtomicUsize::new(0),
            processing_polls: 0,
            polls: AtomicUsize::new(0),
            interactions: vec![Reply { delay_ms: 0, status: 200, body: json!({ "output_text": "hello" }) }],
            interaction_calls: AtomicUsize::new(0),
            log: Mutex::new(Vec::new()),
        }
    }
    fn on_generate(mut self, f: impl Fn(&Value, usize) -> Reply + Send + Sync + 'static) -> Self {
        self.generate = Box::new(f);
        self
    }
    fn record(&self, method: &str, path: &str, info: Value) {
        self.log.lock().unwrap().push((method.into(), path.into(), info));
    }
    /// Every logged request with this method and a path that starts with `prefix`.
    fn requests(&self, method: &str, prefix: &str) -> Vec<(String, Value)> {
        let log = self.log.lock().unwrap();
        log.iter().filter(|(m, p, _)| m == method && p.starts_with(prefix)).map(|(_, p, b)| (p.clone(), b.clone())).collect()
    }
    /// The bodies of the generateContent calls.
    fn generate_bodies(&self) -> Vec<Value> {
        self.requests("POST", "/v1beta/models/").into_iter().filter(|(p, _)| p.ends_with(":generateContent")).map(|(_, b)| b).collect()
    }
}

fn key_ok(headers: &HeaderMap) -> bool {
    headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()) == Some(KEY)
}

fn denied() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": { "code": 403, "message": "API key not valid. Please pass a valid API key." } }))).into_response()
}

type Shared = State<Arc<Fake>>;

async fn list_models(State(f): Shared, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    f.model_list_calls.fetch_add(1, Ordering::SeqCst);
    f.record("GET", "/v1beta/models", json!(q));
    if !key_ok(&headers) {
        return denied();
    }
    if f.models_status != 200 {
        return (StatusCode::from_u16(f.models_status).unwrap(), "no").into_response();
    }
    // two pages, to check that the next page is asked for
    let (models, next) = match q.get("pageToken") {
        None => (
            json!([
                { "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"] },
                { "name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"] },
            ]),
            Some("page2"),
        ),
        Some(_) => (json!([{ "name": "models/gemini-3-flash", "supportedGenerationMethods": ["generateContent"] }]), None),
    };
    Json(json!({ "models": models, "nextPageToken": next })).into_response()
}

async fn generate(State(f): Shared, headers: HeaderMap, Path(model): Path<String>, Json(body): Json<Value>) -> Response {
    f.record("POST", &format!("/v1beta/models/{model}"), body.clone());
    if !key_ok(&headers) {
        return denied();
    }
    let n = f.generate_calls.fetch_add(1, Ordering::SeqCst);
    let live = f.live.fetch_add(1, Ordering::SeqCst) + 1;
    f.peak.fetch_max(live, Ordering::SeqCst);
    let reply = (f.generate)(&body, n);
    tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
    f.live.fetch_sub(1, Ordering::SeqCst);
    (StatusCode::from_u16(reply.status).unwrap(), Json(reply.body)).into_response()
}

async fn upload_start(State(f): Shared, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    f.record(
        "POST",
        "/upload/v1beta/files",
        json!({ "command": header("x-goog-upload-command"), "protocol": header("x-goog-upload-protocol"),
                "length": header("x-goog-upload-header-content-length"), "type": header("x-goog-upload-header-content-type"),
                "key_ok": key_ok(&headers), "body": body }),
    );
    let n = f.requests("POST", "/upload/v1beta/files").len();
    let url = format!("http://{}/upload-target/{n}", header("host"));
    ([("x-goog-upload-url", url)], Json(json!({}))).into_response()
}

async fn upload_bytes(State(f): Shared, Path(n): Path<usize>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let command = headers.get("x-goog-upload-command").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    f.record("POST", &format!("/upload-target/{n}"), json!({ "command": command, "bytes": body.len() }));
    let file = json!({ "name": format!("files/f{n}"), "uri": format!("https://files.example/f{n}"), "mimeType": "audio/wav", "state": "PROCESSING" });
    ([("x-goog-upload-status", "final")], Json(json!({ "file": file }))).into_response()
}

async fn get_file(State(f): Shared, Path(id): Path<String>) -> Response {
    f.record("GET", &format!("/v1beta/files/{id}"), json!({}));
    let seen = f.polls.fetch_add(1, Ordering::SeqCst);
    // `processing_polls` counts the polls over all files, which is enough for these tests
    let state = if seen < f.processing_polls { "PROCESSING" } else { "ACTIVE" };
    Json(json!({ "name": format!("files/{id}"), "uri": format!("https://files.example/{id}"), "mimeType": "audio/wav", "state": state })).into_response()
}

async fn delete_file(State(f): Shared, Path(id): Path<String>) -> Response {
    f.record("DELETE", &format!("/v1beta/files/{id}"), json!({}));
    Json(json!({})).into_response()
}

async fn interactions(State(f): Shared, Json(body): Json<Value>) -> Response {
    f.record("POST", "/v1beta/interactions", body);
    let n = f.interaction_calls.fetch_add(1, Ordering::SeqCst).min(f.interactions.len() - 1);
    let reply = &f.interactions[n];
    (StatusCode::from_u16(reply.status).unwrap(), Json(reply.body.clone())).into_response()
}

/// Starts the fake and returns it with a `Gemini` that talks to it. Waits are a few milliseconds instead of a minute.
async fn start(fake: Fake) -> (Arc<Fake>, Gemini) {
    let fake = Arc::new(fake);
    let app = Router::new()
        .route("/v1beta/models", get(list_models))
        .route("/v1beta/models/{model}", post(generate))
        .route("/upload/v1beta/files", post(upload_start))
        .route("/upload-target/{n}", post(upload_bytes))
        .route("/v1beta/files/{id}", get(get_file).delete(delete_file))
        .route("/v1beta/interactions", post(interactions))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let gemini = Gemini::new(KEY.to_string())
        .with_base_url(&base)
        .with_retry_policy(6, Duration::from_millis(20))
        .with_poll_interval(Duration::from_millis(5));
    (fake, gemini)
}

// --- helpers ---

/// A progress callback that keeps everything it hears.
fn collect() -> (Progress, Arc<Mutex<Vec<String>>>) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    (progress(move |s| sink.lock().unwrap().push(s)), heard)
}

/// The user message of a generateContent body.
fn contents(body: &Value) -> String {
    body["contents"][0]["parts"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn system(body: &Value) -> String {
    body["systemInstruction"]["parts"][0]["text"].as_str().unwrap_or_default().to_string()
}

/// `n` turns of 150 words each, with time stamps every 20 seconds.
fn stamped(turns: usize) -> String {
    let words = "word ".repeat(150);
    (0..turns)
        .map(|i| format!("[{:02}:{:02}] Speaker {}: {}", i * 20 / 60, i * 20 % 60, i % 2 + 1, words.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn notes_json() -> Value {
    json!({
        "title": "T", "summary": "S",
        "sections": [{ "heading": "H", "bullets": [{ "text": "x", "at": 75 }] }],
        "keyPoints": [{ "text": "k", "at": 75 }],
        "decisions": [], "actions": [],
        "flow": { "label": "Release", "kind": "root", "at": 5, "children": [{ "label": "Build", "kind": "topic" }] },
    })
}

fn temp_wav(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pechu-gemini-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

// --- key check and models ---

#[tokio::test]
async fn a_good_key_is_valid_and_a_refused_key_is_invalid() {
    let (fake, gemini) = start(Fake::new()).await;
    assert_eq!(gemini.check_key().await.unwrap(), KeyCheck::Valid);
    assert_eq!(fake.requests("GET", "/v1beta/models")[0].1["pageSize"], "1");

    let wrong = Gemini::new("not-the-key".into()).with_base_url(gemini.base_url());
    assert_eq!(wrong.check_key().await.unwrap(), KeyCheck::Invalid);
}

#[tokio::test]
async fn a_server_problem_or_no_connection_is_a_readable_error() {
    let mut fake = Fake::new();
    fake.models_status = 500;
    let (_fake, gemini) = start(fake).await;
    let msg = friendly(&gemini.check_key().await.unwrap_err());
    assert_eq!(msg, "Google could not check the key right now (status 500).");

    // nothing listens on port 9: the connection is refused
    let offline = Gemini::new(KEY.into()).with_base_url("http://127.0.0.1:9/v1beta");
    let msg = friendly(&offline.check_key().await.unwrap_err());
    assert!(msg.contains("could not reach the service"), "{msg}");
}

#[tokio::test]
async fn the_notes_model_is_discovered_from_all_pages_and_cached() {
    let (fake, gemini) = start(Fake::new()).await;
    assert_eq!(gemini.notes_model().await.unwrap().as_deref(), Some("gemini-3-flash")); // from page 2
    assert_eq!(gemini.notes_model().await.unwrap().as_deref(), Some("gemini-3-flash"));
    assert_eq!(fake.model_list_calls.load(Ordering::SeqCst), 2); // two pages, once
    // an override needs no request at all
    let (fake, gemini) = start(Fake::new()).await;
    let gemini = gemini.with_notes_model(Some("my-model".into()));
    assert_eq!(gemini.notes_model().await.unwrap().as_deref(), Some("my-model"));
    assert_eq!(fake.model_list_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn when_models_cannot_be_listed_there_is_no_model_and_notes_say_so() {
    let mut fake = Fake::new();
    fake.models_status = 500;
    let (_fake, gemini) = start(fake).await;
    assert_eq!(gemini.notes_model().await.unwrap(), None);
    let err = gemini.translate_to_english("hello", no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "No suitable Gemini model was found for notes. Set NOTES_MODEL in .env.");
}

// --- translation ---

#[tokio::test]
async fn translation_in_three_parts_keeps_the_order_and_runs_them_together() {
    // each paragraph is its own part; the first part is the slowest, so the parts finish in reverse order
    let transcript = ["ONE", "TWO", "THREE"].iter().map(|w| format!("{w} {}", "x".repeat(6000))).collect::<Vec<_>>().join("\n\n");
    let fake = Fake::new().on_generate(|body, _| {
        let text = contents(body);
        let delay_ms = if text.starts_with("ONE") { 150 } else if text.starts_with("TWO") { 80 } else { 10 };
        let mut reply = text_reply(&format!("EN {text}")); // as long as the original, so it is not "too short"
        reply.delay_ms = delay_ms;
        reply
    });
    let (fake, gemini) = start(fake).await;
    let (progress, heard) = collect();
    let english = gemini.translate_to_english(&transcript, progress).await.unwrap();

    let parts: Vec<&str> = english.split("\n\n").collect();
    assert_eq!(parts.len(), 3);
    assert!(parts[0].starts_with("EN ONE") && parts[1].starts_with("EN TWO") && parts[2].starts_with("EN THREE"));
    assert_eq!(fake.peak.load(Ordering::SeqCst), 3);

    let heard = heard.lock().unwrap();
    assert_eq!(heard[0], "Translating 3 parts");
    assert_eq!(heard[1], "Translating: 1 of 3 parts done");
    assert_eq!(heard[3], "Translating: 3 of 3 parts done");

    // what was sent: the translate prompt as system text, temperature 0.2, no JSON mode
    let body = &fake.generate_bodies()[0];
    assert!(system(body).starts_with("You translate part of a meeting transcript into clear English."));
    assert_eq!(body["generationConfig"]["temperature"], 0.2);
    assert!(body["generationConfig"].get("responseSchema").is_none());
}

#[tokio::test]
async fn a_single_short_part_says_translating_to_english() {
    let (_fake, gemini) = start(Fake::new().on_generate(|_, _| text_reply("Hello there"))).await;
    let (progress, heard) = collect();
    assert_eq!(gemini.translate_to_english("vanakkam", progress).await.unwrap(), "Hello there");
    assert_eq!(*heard.lock().unwrap(), ["Translating to English"]);
}

#[tokio::test]
async fn a_far_too_short_translation_is_asked_for_once_more_with_the_insist_prompt() {
    let transcript = "y".repeat(3000);
    let fake = Fake::new().on_generate(|body, _| {
        if system(body).contains("IMPORTANT: your last answer was far too short. Translate every sentence this time.") {
            text_reply(&"z".repeat(2900))
        } else {
            text_reply("tiny")
        }
    });
    let (fake, gemini) = start(fake).await;
    let english = gemini.translate_to_english(&transcript, no_progress()).await.unwrap();
    assert_eq!(english.len(), 2900);
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_empty_translation_is_an_error_the_person_can_read() {
    let (_fake, gemini) = start(Fake::new().on_generate(|_, _| text_reply("  "))).await;
    let err = gemini.translate_to_english("hello", no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "Gemini returned an empty translation. Try again.");
}

// --- retries and errors ---

#[tokio::test]
async fn a_rate_limit_waits_and_then_works() {
    let fake = Fake::new().on_generate(|_, n| {
        if n == 0 { error_reply(429, "Rate limit: tokens per minute exceeded (RESOURCE_EXHAUSTED)") } else { text_reply("Hello there") }
    });
    let (fake, gemini) = start(fake).await;
    let (progress, heard) = collect();
    assert_eq!(gemini.translate_to_english("vanakkam", progress).await.unwrap(), "Hello there");
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), 2);
    assert!(heard.lock().unwrap().iter().any(|p| p.starts_with("Gemini is busy, trying again in ")));
}

#[tokio::test]
async fn a_busy_503_is_tried_again_up_to_the_attempt_limit() {
    let (fake, gemini) = start(Fake::new().on_generate(|_, _| error_reply(503, "The model is overloaded."))).await;
    let err = gemini.translate_to_english("vanakkam", no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "Gemini is busy right now. Try again in a minute.");
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn the_daily_limit_fails_at_once_and_says_when_it_resets() {
    let message = "Quota exceeded for metric: generate_content_free_tier_requests per day. Please retry in 14h21m22.5s.";
    let (fake, gemini) = start(Fake::new().on_generate(move |_, _| error_reply(429, message))).await;
    let err = gemini.translate_to_english("vanakkam", no_progress()).await.unwrap_err();
    assert_eq!(
        friendly(&err),
        "Today's free limit for this Gemini model is used up. It resets in about 14 h 21 min. A paid Google plan removes the limit."
    );
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), 1); // no waiting, no second try
}

#[tokio::test]
async fn a_refused_key_is_explained_and_the_key_never_appears() {
    let body = format!("API key {KEY} is not valid (PERMISSION_DENIED)");
    let (_fake, gemini) = start(Fake::new().on_generate(move |_, _| error_reply(403, &body))).await;
    let msg = friendly(&gemini.translate_to_english("vanakkam", no_progress()).await.unwrap_err());
    assert_eq!(msg, "Google did not accept the API key, or the key cannot use this model. Check the key and try again.");
    assert!(!msg.contains(KEY));
}

// --- notes ---

#[tokio::test]
async fn notes_come_back_validated_with_json_mode_and_without_made_up_times() {
    let fake = Fake::new().on_generate(|_, _| text_reply(&notes_json().to_string()));
    let (fake, gemini) = start(fake).await;
    let (progress, heard) = collect();
    // no stamps in this transcript, so the `at` values the model sent anyway must be removed
    let english = "Speaker 1: ".to_string() + &"we should release on Friday. ".repeat(10);
    let notes = gemini.notes_from_english(&english, progress).await.unwrap();
    assert_eq!(notes["title"], "T");
    assert!(!notes.to_string().contains("\"at\""));
    assert_eq!(*heard.lock().unwrap(), ["Writing the summary and flow charts"]);

    let bodies = fake.generate_bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(system(body).starts_with("You write meeting notes from a transcript."));
    assert!(contents(body).starts_with("Transcript:\n\"\"\"\nSpeaker 1: we should"));
    assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
    assert_eq!(body["generationConfig"]["responseSchema"]["type"], "OBJECT");
    assert!(body["generationConfig"].get("temperature").is_none());
    // the path names the discovered model
    let paths: Vec<String> = fake.requests("POST", "/v1beta/models/").into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, ["/v1beta/models/gemini-3-flash:generateContent"]);
}

#[tokio::test]
async fn a_stamped_transcript_keeps_its_times() {
    let fake = Fake::new().on_generate(|_, _| text_reply(&notes_json().to_string()));
    let (_fake, gemini) = start(fake).await;
    let notes = gemini.notes_from_english(&stamped(5), no_progress()).await.unwrap();
    assert_eq!(notes["sections"][0]["bullets"][0]["at"], 75);
    assert_eq!(notes["flow"]["at"], 5);
}

#[tokio::test]
async fn unreadable_notes_and_too_little_speech_are_plain_errors() {
    let (fake, gemini) = start(Fake::new().on_generate(|_, _| text_reply("sorry, no JSON today"))).await;
    let err = gemini.notes_from_english(&stamped(3), no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "Gemini sent back notes that could not be read. Try again.");
    let before = fake.generate_calls.load(Ordering::SeqCst);
    let err = gemini.notes_from_english("too short", no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "There is not enough speech to write notes.");
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), before); // nothing was sent
}

#[tokio::test]
async fn a_long_transcript_is_drafted_in_parts_first_and_the_final_call_reads_only_the_drafts() {
    let english = stamped(120); // about 94,000 characters
    let fake = Fake::new().on_generate(|body, _| {
        if system(body).starts_with("You take detailed notes on one part of a longer meeting") {
            let mut reply = text_reply(&format!("[00:20] DRAFT of {}", &contents(body)[..8]));
            reply.delay_ms = 15;
            reply
        } else {
            text_reply(&notes_json().to_string())
        }
    });
    let (fake, gemini) = start(fake).await;
    let (progress, heard) = collect();
    let notes = gemini.notes_from_english(&english, progress).await.unwrap();

    let bodies = fake.generate_bodies();
    let (drafts, finals): (Vec<&Value>, Vec<&Value>) = bodies.iter().partition(|b| system(b).starts_with("You take detailed notes"));
    assert!(drafts.len() > 3);
    assert_eq!(finals.len(), 1);
    assert!(fake.peak.load(Ordering::SeqCst) <= 3);
    let last = contents(finals[0]);
    assert!(last.starts_with("Detailed notes on the parts of the meeting, in order:"));
    assert!(last.contains("=== Part 1 of "));
    assert!(last.contains(&format!("=== Part {n} of {n} ===", n = drafts.len())));
    assert!(last.len() < english.len() / 5);
    // the final system text carries the extra note about drafts
    assert!(system(finals[0]).contains("IMPORTANT: this meeting was long"));
    assert!(heard.lock().unwrap().iter().any(|p| p == "Summarising part 2 of 10" || p.starts_with("Summarising part 2 of ")));
    assert_eq!(notes["keyPoints"][0]["at"], 75);
}

#[tokio::test]
async fn the_flow_only_call_returns_flow_and_process_and_sends_the_title() {
    let step = |id: &str| json!({ "id": id, "label": format!("Do {id}"), "kind": "step", "next": [] });
    let answer = json!({ "flow": { "label": "Release", "kind": "root" }, "process": { "steps": [step("a"), step("b"), step("c")] } });
    let (fake, gemini) = start(Fake::new().on_generate(move |_, _| text_reply(&answer.to_string()))).await;
    let (progress, heard) = collect();
    let out = gemini.flow_from_english(&stamped(3), "My meeting", progress).await.unwrap();
    assert_eq!(out["process"]["steps"].as_array().unwrap().len(), 3);
    assert!(contents(&fake.generate_bodies()[0]).starts_with("Meeting title: My meeting\n\nTranscript:"));
    assert!(system(&fake.generate_bodies()[0]).starts_with("You draw flow charts of a meeting"));
    assert_eq!(*heard.lock().unwrap(), ["Drawing the flow chart"]);
}

#[tokio::test]
async fn generate_notes_translates_first_tells_about_the_english_and_then_writes_the_notes() {
    let fake = Fake::new().on_generate(|body, _| {
        if system(body).starts_with("You translate") {
            text_reply("Speaker 1: we should release the build on Friday after the tests pass.")
        } else {
            text_reply(&notes_json().to_string())
        }
    });
    let (fake, gemini) = start(fake).await;
    let told = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = told.clone();
    let (notes, english) = gemini
        .generate_notes("Speaker 1: Friday-la release pannalam, tests pass aanaa.", no_progress(), &move |e| sink.lock().unwrap().push(e))
        .await
        .unwrap();
    assert_eq!(notes["title"], "T");
    assert!(english.starts_with("Speaker 1: we should release"));
    assert_eq!(*told.lock().unwrap(), [english]);
    assert_eq!(fake.generate_calls.load(Ordering::SeqCst), 2);
}

// --- transcription with Gemini's file model ---

#[tokio::test]
async fn parts_are_uploaded_polled_transcribed_and_deleted() {
    let mut fake = Fake::new();
    fake.processing_polls = 2;
    fake.interactions = vec![
        Reply { delay_ms: 0, status: 200, body: json!({ "output_text": " first part " }) },
        Reply { delay_ms: 0, status: 200, body: json!({ "steps": [{ "content": [{ "type": "text", "text": "second part" }] }] }) },
    ];
    let (fake, gemini) = start(fake).await;
    let parts = [temp_wav("a.wav", b"RIFF-first-part"), temp_wav("b.wav", b"RIFF-second")];
    let (progress, heard) = collect();
    let text = gemini.transcribe_parts(&parts, TranscribeMode::Smart, progress).await.unwrap();
    assert_eq!(text, "first part\n\nsecond part");
    assert_eq!(*heard.lock().unwrap(), ["Part 1 of 2", "Part 2 of 2"]);

    // upload: asked for an address, then sent the bytes in one finalized request
    let starts = fake.requests("POST", "/upload/v1beta/files");
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].1["command"], "start");
    assert_eq!(starts[0].1["protocol"], "resumable");
    assert_eq!(starts[0].1["length"], "15");
    assert_eq!(starts[0].1["type"], "audio/wav");
    assert_eq!(starts[0].1["key_ok"], true);
    assert_eq!(starts[0].1["body"]["file"]["mimeType"], "audio/wav");
    let sends = fake.requests("POST", "/upload-target/");
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].1, json!({ "command": "upload, finalize", "bytes": 15 }));
    // waited until the file was ACTIVE
    assert!(fake.requests("GET", "/v1beta/files/").len() >= 2);
    // the transcription request
    let asks = fake.requests("POST", "/v1beta/interactions");
    assert_eq!(asks.len(), 2);
    assert_eq!(asks[0].1["model"], "gemini-3.5-transcribe");
    assert_eq!(asks[0].1["input"][0], json!({ "type": "audio", "uri": "https://files.example/f1", "mime_type": "audio/wav" }));
    assert_eq!(asks[0].1["generation_config"]["transcription_config"], json!({ "language_codes": [], "mode": "smart" }));
    // both files were deleted afterwards
    let deletes: Vec<String> = fake.requests("DELETE", "/v1beta/files/").into_iter().map(|(p, _)| p).collect();
    assert_eq!(deletes, ["/v1beta/files/f1", "/v1beta/files/f2"]);
}

#[tokio::test]
async fn the_speaker_mode_asks_for_diarization_and_builds_turns_from_the_word_list() {
    let words = json!([
        { "type": "word_info", "text": "Hi", "speaker": "spk_1" }, { "type": "word_info", "text": "there", "speaker": "spk_1" },
        { "type": "word_info", "text": "Hello", "speaker": "spk_2" },
    ]);
    let mut fake = Fake::new();
    fake.interactions = vec![Reply {
        delay_ms: 0,
        status: 200,
        body: json!({ "steps": [{ "content": [{ "type": "text", "text": "Hi there Hello", "annotations": words }] }] }),
    }];
    let (fake, gemini) = start(fake).await;
    let text = gemini.transcribe_parts(&[temp_wav("s.wav", b"RIFFx")], TranscribeMode::Speakers, no_progress()).await.unwrap();
    assert!(text.starts_with("Speaker 1: Hi there"), "{text}");
    assert!(text.contains("Speaker 2: Hello"), "{text}");
    let ask = &fake.requests("POST", "/v1beta/interactions")[0].1;
    assert_eq!(ask["generation_config"]["transcription_config"]["mode"], json!({ "type": "verbatim", "diarization_mode": "speaker" }));
}

#[tokio::test]
async fn the_speaker_mode_falls_back_to_plain_text_when_there_is_no_word_list() {
    let mut fake = Fake::new();
    fake.interactions = vec![Reply { delay_ms: 0, status: 200, body: json!({ "output_text": "just text" }) }];
    let (_fake, gemini) = start(fake).await;
    let text = gemini.transcribe_parts(&[temp_wav("t.wav", b"RIFFx")], TranscribeMode::Speakers, no_progress()).await.unwrap();
    assert_eq!(text, "just text");
}

#[tokio::test]
async fn a_rate_limit_during_transcription_waits_and_uploads_again() {
    let mut fake = Fake::new();
    fake.interactions = vec![
        Reply { delay_ms: 0, status: 429, body: json!({ "error": { "message": "Rate limit reached (RESOURCE_EXHAUSTED)" } }) },
        Reply { delay_ms: 0, status: 200, body: json!({ "output_text": "done" }) },
    ];
    let (fake, gemini) = start(fake).await;
    let (progress, heard) = collect();
    let text = gemini.transcribe_parts(&[temp_wav("r.wav", b"RIFFx")], TranscribeMode::Verbatim, progress).await.unwrap();
    assert_eq!(text, "done");
    assert_eq!(fake.requests("POST", "/upload/v1beta/files").len(), 2); // the whole part was sent again
    assert_eq!(fake.requests("DELETE", "/v1beta/files/").len(), 2); // and each upload was cleaned up
    assert!(heard.lock().unwrap().iter().any(|p| p == "Transcribing, waiting for Google"));
    assert_eq!(fake.requests("POST", "/v1beta/interactions")[0].1["generation_config"]["transcription_config"]["mode"], "verbatim");
}

#[tokio::test]
async fn a_failed_transcription_still_deletes_the_file_and_explains() {
    let mut fake = Fake::new();
    fake.interactions = vec![Reply { delay_ms: 0, status: 400, body: json!({ "error": { "message": "Request contains an invalid argument." } }) }];
    let (fake, gemini) = start(fake).await;
    let err = gemini.transcribe_parts(&[temp_wav("bad.wav", b"RIFFx")], TranscribeMode::Smart, no_progress()).await.unwrap_err();
    assert_eq!(friendly(&err), "Google could not read this audio. Try exporting it as MP3 or WAV, then upload it again.");
    assert_eq!(fake.requests("DELETE", "/v1beta/files/").len(), 1);
}

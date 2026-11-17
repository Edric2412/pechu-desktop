//! The `/api` routes. Same paths, status codes and error texts as server/index.ts, because the web UI is not changed.
use crate::keys::{key_shape_ok, normalize_key};
use crate::live::{self, lang_ok, rid_ok};
use crate::range::{parse_range, ByteRange};
use crate::App;
use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{FromRequest, Multipart, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use pechu_common::TranscribeMode;
use pechu_gemini::{validate_notes, Gemini, KeyCheck};
use pechu_jobs::Keys;
use pechu_store::Store;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

/// The most audio one upload may carry. (The web version's limit is what Cloudflare lets through; kept the same.)
pub const MAX_BYTES: u64 = 100 * 1000 * 1000;
/// After this much extra we stop reading a too-large upload and just answer.
const DRAIN_SLACK: u64 = 5 * 1024 * 1024;
/// Far more than any meeting; keeps one job from costing thousands of model calls.
const MAX_TRANSCRIPT_CHARS: usize = 2_000_000;
/// A JSON body is never bigger than a few transcripts.
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

// ----- answers -----

/// An error shown to the person: always `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        ApiError(StatusCode::from_u16(status).expect("a valid status code"), message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(self.0, json!({ "error": self.1 }))
    }
}

pub type ApiResult = Result<Response, ApiError>;

pub fn json_response(status: StatusCode, value: Value) -> Response {
    let mut res = (status, axum::Json(value)).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn ok(value: Value) -> ApiResult {
    Ok(json_response(StatusCode::OK, value))
}

/// Anything unexpected: the details go to the log, the person sees one fixed sentence.
fn internal(what: &str, e: impl std::fmt::Display) -> ApiError {
    tracing::error!("{what}: {e}");
    ApiError::new(500, "Something went wrong on the server.")
}

// ----- small helpers -----

/// The body must be a JSON object, and the request must say so: a plain-text or form body would let another website send
/// this request without a browser preflight.
async fn read_json(req: Request) -> Result<Map<String, Value>, ApiError> {
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let is_json = content_type
        .strip_prefix("application/json")
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'));
    if !is_json {
        return Err(ApiError::new(415, "Send a JSON object."));
    }
    let bytes = axum::body::to_bytes(req.into_body(), MAX_JSON_BYTES).await.map_err(|_| ApiError::new(400, "Send a JSON object."))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ApiError::new(400, "Send a JSON object.")),
    }
}

/// JavaScript counts text in UTF-16 units, so the limits mean the same thing here as in the web version.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn need_key(app: &App) -> Result<(), ApiError> {
    match app.keys.gemini_key() {
        Some(_) => Ok(()),
        None => Err(ApiError::new(401, "Add your Gemini API key first.")),
    }
}

fn need_meeting(app: &App, id: &str) -> Result<Value, ApiError> {
    if !Store::id_ok(id) {
        return Err(ApiError::new(404, "Meeting not found."));
    }
    app.store.get_meeting(id).ok_or_else(|| ApiError::new(404, "Meeting not found."))
}

fn str_field<'a>(m: &'a Value, name: &str) -> &'a str {
    m.get(name).and_then(Value::as_str).unwrap_or("")
}

// ----- small routes -----

pub async fn status(State(app): State<Arc<App>>) -> ApiResult {
    let has_key = app.keys.gemini_key().is_some();
    // The page asks this before it shows anything and does not use the model name, so a slow Google must not delay it: wait a
    // moment for the (usually already finished) background lookup, otherwise answer without the name.
    let notes_model = if has_key { tokio::time::timeout(Duration::from_millis(300), app.notes_model()).await.ok().flatten() } else { None };
    ok(json!({
        "hasKey": has_key,
        "notesModel": notes_model,
        "transcriber": if app.keys.sarvam_key().is_some() { "sarvam" } else { "gemini" },
        "desktop": true,
    }))
}

/// Receives the summary line of `assets/probe.js` (only sent when the program was started with PECHU_PROBE=1) and prints it.
pub async fn probe(body: String) -> ApiResult {
    if std::env::var_os("PECHU_PROBE").is_some() {
        eprintln!("PROBE {}", body.chars().take(2000).collect::<String>());
    }
    ok(json!({ "ok": true }))
}

/// What the page asks of its window (F11, and the minimize / maximize / close buttons and the drag handle the page draws
/// itself): passes the request on to the window shell. With nobody listening (tests, or the page opened in a browser) there is
/// simply nothing to do, which is not an error.
pub async fn window_command(State(app): State<Arc<App>>, Path(name): Path<String>, req: Request) -> ApiResult {
    use crate::WindowCommand::*;
    read_json(req).await?; // like every other POST: it must say it is JSON, so another website cannot send it without a preflight
    let command = match name.as_str() {
        "fullscreen" => ToggleFullscreen,
        "minimize" => Minimize,
        "maximize" => ToggleMaximize,
        "close" => Close,
        "drag" => StartDrag,
        _ => return Err(ApiError::new(404, "Not found.")),
    };
    let _ = app.window_tx.send(command);
    ok(json!({ "ok": true }))
}

/// There is no password in the desktop app: the launch token and the session cookie are the login.
pub async fn auth() -> ApiResult {
    ok(json!({ "authRequired": false, "authed": true }))
}

pub async fn login_or_logout() -> ApiResult {
    ok(json!({ "ok": true }))
}

pub async fn post_key(State(app): State<Arc<App>>, req: Request) -> ApiResult {
    let body = read_json(req).await?;
    let rejected = || ApiError::new(400, "That key was not accepted by Google.");
    let text = |name: &str| body.get(name).and_then(Value::as_str).map(normalize_key);
    let gemini_key = text("key");
    let sarvam_key = text("sarvamKey").filter(|k| !k.is_empty());

    // A Sarvam key can come alone (from a settings screen) or together with the Gemini key.
    if gemini_key.is_none() && sarvam_key.is_none() {
        return Err(rejected());
    }
    if let Some(key) = &sarvam_key {
        if !key_shape_ok(key) {
            return Err(ApiError::new(400, "That key was not accepted by Sarvam."));
        }
    }
    if let Some(key) = &gemini_key {
        // an empty "key" next to a Sarvam key just means "leave the Gemini key alone"
        if !(key.is_empty() && sarvam_key.is_some()) {
            if !key_shape_ok(key) {
                return Err(rejected());
            }
            match app.gemini_for(key.clone()).check_key().await {
                Err(_) => {
                    return Err(ApiError::new(502, "Could not check the key with Google right now. Check your internet connection and try again."))
                }
                Ok(KeyCheck::Invalid) => return Err(rejected()),
                Ok(KeyCheck::Valid) => {}
            }
        }
    }

    // The keychain can be slow, so it is not asked on the async threads.
    let keys = app.keys.clone();
    let save = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if let Some(key) = gemini_key.filter(|k| !k.is_empty()) {
            keys.save_gemini(&key)?;
        }
        if let Some(key) = sarvam_key {
            keys.save_sarvam(&key)?;
        }
        Ok(())
    })
    .await;
    match save {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(internal("saving a key", format!("{e:#}"))),
        Err(e) => return Err(internal("saving a key", e)),
    }
    ok(json!({ "ok": true, "notesModel": app.notes_model().await }))
}

// ----- meetings -----

pub async fn list_meetings(State(app): State<Arc<App>>) -> ApiResult {
    ok(Value::Array(app.store.list_meetings()))
}

pub async fn create_meeting(State(app): State<Arc<App>>, req: Request) -> ApiResult {
    let b = read_json(req).await?;
    if let Some(source) = b.get("source") {
        if source != "live" {
            return Err(ApiError::new(400, "source must be \"live\"."));
        }
    }
    let Some(transcript) = b.get("transcript").and_then(Value::as_str) else {
        return Err(ApiError::new(400, "transcript must be text."));
    };
    if js_len(transcript) > MAX_TRANSCRIPT_CHARS {
        return Err(ApiError::new(413, "That transcript is too long."));
    }
    let title = match b.get("title") {
        None => "",
        Some(Value::String(t)) => t.as_str(),
        Some(_) => return Err(ApiError::new(400, "title must be text.")),
    };
    let duration = match b.get("durationSec") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Number(n)) if n.as_f64().is_some_and(|d| d >= 0.0) => Value::Number(n.clone()),
        Some(_) => return Err(ApiError::new(400, "durationSec must be a number.")),
    };

    let title: String = Some(title.trim()).filter(|t| !t.is_empty()).unwrap_or("Untitled meeting").chars().take(200).collect();
    let mut init = json!({
        "source": "live",
        "status": "ready",
        "transcript": transcript,
        "title": title,
        "durationSec": duration,
    });
    // what the live captions of this recording used
    if let Some(rid) = b.get("rid").and_then(Value::as_str) {
        let used = app.live_usage.take(rid);
        if used.as_object().is_some_and(|o| !o.is_empty()) {
            init["usage"] = used;
        }
    }
    // which service wrote the live captions, shown as "Made with" until a second listen replaces it
    if let Some(name) = b.get("transcriber").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
        let name: String = name.chars().take(60).collect();
        init["transcriber"] = json!(format!("{name} (live)"));
    }
    let meeting = app.store.create_meeting(init).map_err(|e| internal("creating a meeting", format!("{e:#}")))?;
    ok(meeting)
}

pub async fn get_meeting(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    ok(need_meeting(&app, &id)?)
}

pub async fn delete_meeting(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    if !Store::id_ok(&id) || !app.store.delete_meeting(&id) {
        return Err(ApiError::new(404, "Meeting not found."));
    }
    ok(json!({ "ok": true }))
}

pub async fn patch_meeting(State(app): State<Arc<App>>, Path(id): Path<String>, req: Request) -> ApiResult {
    need_meeting(&app, &id)?;
    let b = read_json(req).await?;
    let mut patch = Map::new();

    if let Some(title) = b.get("title") {
        let Some(title) = title.as_str() else { return Err(ApiError::new(400, "title must be text.")) };
        let title: String = Some(title.trim()).filter(|t| !t.is_empty()).unwrap_or("Untitled meeting").chars().take(200).collect();
        patch.insert("title".into(), json!(title));
    }
    for (name, label) in [("transcript", "transcript"), ("transcriptEnglish", "transcriptEnglish")] {
        if let Some(text) = b.get(name) {
            let Some(text) = text.as_str() else { return Err(ApiError::new(400, format!("{label} must be text."))) };
            if js_len(text) > MAX_TRANSCRIPT_CHARS {
                return Err(ApiError::new(413, "That transcript is too long."));
            }
            patch.insert(name.into(), json!(text));
        }
    }
    if let Some(notes) = b.get("notes") {
        match validate_notes(notes) {
            Ok(valid) => patch.insert("notes".into(), valid), // old or new shape in, new shape stored
            Err(e) => {
                tracing::warn!("notes.invalid id={id}: {e:#}"); // the detail is for the log, not the screen
                return Err(ApiError::new(
                    400,
                    "Those notes could not be saved because they are not in the expected format. Reload the page and try again.",
                ));
            }
        };
    }
    if patch.is_empty() {
        return Err(ApiError::new(400, "Send title, transcript, transcriptEnglish and/or notes."));
    }
    match app.store.update_meeting(&id, Value::Object(patch)) {
        Some(m) => ok(m),
        None => Err(ApiError::new(404, "Meeting not found.")),
    }
}

// ----- audio -----

/// A file being written from a request body. Counts every byte, keeps only the first MAX_BYTES, and is deleted when dropped.
struct TempUpload {
    file: tokio::fs::File,
    path: tempfile::TempPath,
    seen: u64,
}

impl TempUpload {
    fn new(dir: &std::path::Path) -> Result<Self, ApiError> {
        std::fs::create_dir_all(dir).map_err(|e| internal("making the temp folder", e))?;
        let (file, path) = tempfile::Builder::new()
            .prefix("upload-")
            .tempfile_in(dir)
            .map_err(|e| internal("making a temp file", e))?
            .into_parts();
        Ok(TempUpload { file: tokio::fs::File::from_std(file), path, seen: 0 })
    }

    async fn push(&mut self, chunk: &[u8]) -> Result<(), ApiError> {
        self.seen += chunk.len() as u64;
        if !self.too_big() {
            self.file.write_all(chunk).await.map_err(|e| internal("writing the upload", e))?;
        }
        Ok(())
    }

    fn too_big(&self) -> bool {
        self.seen > MAX_BYTES
    }

    /// Past this point the rest of the upload is not worth reading.
    fn give_up(&self) -> bool {
        self.seen > MAX_BYTES + DRAIN_SLACK
    }

    /// Makes sure everything is on disk and returns the path (the file is still deleted when the guard is dropped).
    async fn finish(&mut self) -> Result<&std::path::Path, ApiError> {
        self.file.flush().await.map_err(|e| internal("writing the upload", e))?;
        Ok(&self.path)
    }
}

/// A browser recording gets its length written in, as the web version does when it saves the file (see webm.rs).
/// It never fails the upload: if the file cannot be fixed it is saved as it came.
async fn finish_audio(upload: &mut TempUpload, ext: &str) -> Result<std::path::PathBuf, ApiError> {
    let path = upload.finish().await?.to_path_buf();
    if ext == "webm" {
        let to_fix = path.clone();
        let _ = tokio::task::spawn_blocking(move || crate::webm::add_duration(&to_fix)).await;
    }
    Ok(path)
}

pub async fn put_audio(State(app): State<Arc<App>>, Path(id): Path<String>, req: Request) -> ApiResult {
    need_meeting(&app, &id)?;
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Some(ext) = Store::audio_ext(&content_type, "") else {
        return Err(ApiError::new(415, "Send audio with an audio Content-Type, for example audio/webm."));
    };

    // The audio goes straight to a file, never all into memory.
    let mut upload = TempUpload::new(&app.store.tmp_dir())?;
    let mut body = req.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ApiError::new(400, "The upload was interrupted."))?;
        upload.push(&chunk).await?;
        if upload.give_up() {
            break;
        }
    }
    if upload.seen == 0 {
        return Err(ApiError::new(400, "The audio was empty."));
    }
    if upload.too_big() {
        return Err(ApiError::new(413, "That recording is larger than 100 MB."));
    }
    let path = finish_audio(&mut upload, ext).await?;
    match app.store.save_audio_file(&id, &path, ext) {
        Ok(Some(_)) => ok(json!({ "ok": true })),
        Ok(None) => Err(ApiError::new(404, "Meeting not found.")),
        Err(e) => Err(internal("saving audio", format!("{e:#}"))),
    }
}

pub async fn get_audio(State(app): State<Arc<App>>, Path(id): Path<String>, req: Request) -> ApiResult {
    let meeting = need_meeting(&app, &id)?;
    let no_audio = || ApiError::new(404, "This meeting has no audio.");
    let path = app.store.audio_path(&meeting).ok_or_else(no_audio)?;
    let mime = meeting.pointer("/audio/mime").and_then(Value::as_str).ok_or_else(no_audio)?.to_string();

    let mut file = tokio::fs::File::open(&path).await.map_err(|_| no_audio())?;
    let size = file.metadata().await.map_err(|e| internal("reading the audio", e))?.len();
    let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(|r| parse_range(r, size));

    let builder = Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, mime.as_str())
        .header(header::ACCEPT_RANGES, "bytes");
    let built = match range {
        None => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, size)
            .body(Body::from_stream(ReaderStream::new(file))),
        Some(ByteRange::NotSatisfiable) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty()),
        Some(ByteRange::Partial { start, end }) => {
            file.seek(SeekFrom::Start(start)).await.map_err(|e| internal("reading the audio", e))?;
            let length = end - start + 1;
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                .header(header::CONTENT_LENGTH, length)
                .body(Body::from_stream(ReaderStream::new(file.take(length))))
        }
    };
    built.map_err(|e| internal("building the audio response", e))
}

// ----- uploads -----

fn mode_from(form_mode: &str, speakers: &str) -> TranscribeMode {
    // speakers = as-spoken text with a "Speaker N:" label on every turn
    if speakers == "1" {
        TranscribeMode::Speakers
    } else if form_mode == "verbatim" {
        TranscribeMode::Verbatim
    } else {
        TranscribeMode::Smart
    }
}

/// "talk.final.wav" -> "talk.final" (the text before the last dot).
fn name_without_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) => &name[..dot],
        None => name,
    }
}

pub async fn post_upload(State(app): State<Arc<App>>, req: Request) -> ApiResult {
    need_key(&app)?;
    let bad_form = || ApiError::new(400, "Send a multipart form with an audio file in the field named file.");
    let mut form = Multipart::from_request(req, &()).await.map_err(|_| bad_form())?;

    let mut audio: Option<(TempUpload, String, String)> = None; // (the file, its Content-Type, its name)
    let (mut title, mut mode, mut speakers) = (String::new(), String::new(), String::new());
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(ApiError::new(413, "That file is larger than 100 MB.")),
            Err(_) => return Err(bad_form()),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "file" && field.file_name().is_some() {
            if audio.is_some() {
                continue; // only the first file counts
            }
            let content_type = field.content_type().unwrap_or("").to_string();
            let file_name = field.file_name().unwrap_or("").to_string();
            let mut upload = TempUpload::new(&app.store.tmp_dir())?;
            let mut field = field;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        upload.push(&chunk).await?;
                        if upload.give_up() {
                            return Err(ApiError::new(413, "That file is larger than 100 MB."));
                        }
                    }
                    Ok(None) => break,
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(ApiError::new(413, "That file is larger than 100 MB.")),
                    Err(_) => return Err(bad_form()),
                }
            }
            audio = Some((upload, content_type, file_name));
        } else if matches!(name.as_str(), "title" | "mode" | "speakers") {
            let text = field.text().await.map_err(|_| bad_form())?;
            match name.as_str() {
                "title" => title = text,
                "mode" => mode = text,
                _ => speakers = text,
            }
        }
    }

    let Some((mut upload, content_type, file_name)) = audio.filter(|(u, _, _)| u.seen > 0) else {
        return Err(ApiError::new(400, "No audio file was received."));
    };
    if upload.too_big() {
        return Err(ApiError::new(413, "That file is larger than 100 MB."));
    }
    let Some(ext) = Store::audio_ext(&content_type, &file_name) else {
        return Err(ApiError::new(415, "That does not look like an audio file."));
    };

    let title = Some(title.trim())
        .filter(|t| !t.is_empty())
        .or_else(|| Some(name_without_extension(&file_name).trim()).filter(|t| !t.is_empty()))
        .unwrap_or("Untitled meeting");
    let title: String = title.chars().take(200).collect();

    let meeting = app
        .store
        .create_meeting(json!({ "source": "upload", "status": "transcribing", "transcript": "", "title": title }))
        .map_err(|e| internal("creating a meeting", format!("{e:#}")))?;
    let id = str_field(&meeting, "id").to_string();

    let path = finish_audio(&mut upload, ext).await?;
    let saved = matches!(app.store.save_audio_file(&id, &path, ext), Ok(Some(_)));
    if !saved {
        app.store.delete_meeting(&id);
        return Err(ApiError::new(500, "Could not save the audio file."));
    }
    app.jobs.start_upload(&id, mode_from(&mode, &speakers));
    ok(app.store.get_meeting(&id).unwrap_or(meeting))
}

// ----- jobs on a meeting -----

fn job_started(meeting: Option<Value>) -> ApiResult {
    match meeting {
        Some(m) => ok(m),
        None => Err(ApiError::new(404, "Meeting not found.")),
    }
}

pub async fn post_notes(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let m = need_meeting(&app, &id)?;
    let only = q.get("only");
    if only.is_some_and(|o| o != "notes") {
        return Err(ApiError::new(400, "only must be \"notes\"."));
    }
    let no_english = str_field(&m, "transcriptEnglish").trim().is_empty();
    if only.is_some() && no_english {
        return Err(ApiError::new(400, "There is no English transcript yet. Write the full notes first."));
    }
    need_key(&app)?;
    job_started(if only.is_some() { app.jobs.start_notes_only(&id) } else { app.jobs.start_notes(&id) })
}

pub async fn post_flow(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let m = need_meeting(&app, &id)?;
    if m.get("notes").is_none_or(Value::is_null) {
        return Err(ApiError::new(400, "Write the notes first."));
    }
    if str_field(&m, "transcriptEnglish").trim().is_empty() {
        return Err(ApiError::new(400, "There is no English transcript yet. Write the full notes first."));
    }
    need_key(&app)?;
    job_started(app.jobs.start_flow(&id))
}

pub async fn post_transcribe(State(app): State<Arc<App>>, Path(id): Path<String>, req: Request) -> ApiResult {
    let m = need_meeting(&app, &id)?;
    need_key(&app)?;
    if m.get("audio").is_none_or(Value::is_null) {
        return Err(ApiError::new(400, "This meeting has no saved audio to transcribe."));
    }
    let body = read_json(req).await.unwrap_or_default(); // no body or a bad one just means "no speakers"
    let mode = if body.get("speakers") == Some(&Value::Bool(true)) { TranscribeMode::Speakers } else { TranscribeMode::Smart };
    job_started(app.jobs.retranscribe(&id, mode))
}

// ----- live captions -----

pub async fn live_socket(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> ApiResult {
    // live captions work with either key
    let (gemini_key, sarvam_key) = (app.keys.gemini_key(), app.keys.sarvam_key());
    if gemini_key.is_none() && sarvam_key.is_none() {
        need_key(&app)?;
    }
    let lang = q.get("lang").map(String::as_str).filter(|l| !l.is_empty()).unwrap_or("auto");
    if !lang_ok(lang) {
        return Err(ApiError::new(400, "Unknown language code."));
    }
    let mode = if q.get("mode").is_some_and(|m| m.eq_ignore_ascii_case("verbatim")) { pechu_live::LiveMode::Verbatim } else { pechu_live::LiveMode::Smart };
    let rid = q.get("rid").filter(|r| rid_ok(r)).cloned();
    let Ok(upgrade) = upgrade else { return Err(ApiError::new(400, "Expected a WebSocket connection.")) };
    let Some(slot) = app.live_slots.take() else {
        return Err(ApiError::new(503, "All live caption sessions are busy. Try again shortly."));
    };

    let config = pechu_live::LiveConfig {
        mode,
        langs: if lang == "auto" { vec![] } else { vec![lang.to_string()] },
        sarvam_key,
        gemini_key,
        sarvam_url: app.overrides.sarvam_live_url.clone(),
        gemini_url: app.overrides.gemini_live_url.clone(),
    };
    let usage = app.live_usage.clone();
    let stop = app.stop.subscribe();
    // The browser sends small chunks (about 3 KB); a bigger frame is refused.
    Ok(upgrade.max_message_size(64 * 1024).max_frame_size(64 * 1024).on_upgrade(move |socket| async move {
        let (handle, events) = pechu_live::start(config);
        live::bridge(socket, handle, events, usage, rid, stop, slot).await;
    }))
}

// ----- anything else -----

pub async fn not_found_or_page(State(app): State<Arc<App>>, req: Request) -> Response {
    let path = req.uri().path();
    let is_api = path == "/api" || path.starts_with("/api/");
    if is_api {
        // The web version reads "/api/meetings/" as a meeting with an empty id, so it says so (not just "Not found.").
        let method = req.method();
        let empty_id = path == "/api/meetings/" && [Method::GET, Method::DELETE, Method::PATCH].contains(method);
        return ApiError::new(404, if empty_id { "Meeting not found." } else { "Not found." }).into_response();
    }
    if req.method() == Method::GET || req.method() == Method::HEAD {
        return app.ui.serve(path);
    }
    (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")], "Not found.").into_response()
}

impl App {
    /// A Gemini client for `key`, pointed at the test server when tests ask for it.
    pub fn gemini_for(&self, key: String) -> Gemini {
        let mut gemini = Gemini::new(key).with_notes_model(self.keys.notes_model());
        if let Some(url) = &self.overrides.gemini_base_url {
            gemini = gemini.with_base_url(url);
        }
        gemini
    }

    /// The model used for notes, found once per key and then remembered. None when it cannot be found right now.
    pub async fn notes_model(&self) -> Option<String> {
        let key = self.keys.gemini_key()?;
        if let Some((cached_for, model)) = self.model_cache.lock().unwrap().as_ref() {
            if *cached_for == key {
                return Some(model.clone());
            }
        }
        // The page asks for this while it starts, so a slow answer must not hold it up.
        let found = tokio::time::timeout(Duration::from_secs(10), self.gemini_for(key.clone()).notes_model()).await.ok()?.ok()??;
        *self.model_cache.lock().unwrap() = Some((key, found.clone()));
        Some(found)
    }
}

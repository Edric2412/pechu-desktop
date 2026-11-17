//! Everything that talks to Google's Gemini API over HTTPS: key check, model discovery, translation to English, notes, flow
//! charts, and (when there is no Sarvam key) transcription of audio parts. Port of server/gemini.ts + server/prompts.ts.
//! CONTRACT: the signatures below are used by other crates. You may add functions and fields; do not change these.
//!
//! How errors work here: inside, a failed call is an `ApiError` (status + Google's text) so that `with_retry` can decide
//! whether waiting helps. Every public method ends by turning whatever is left into a `user_err` with a plain-words
//! message (`errors::into_user_error`), so callers only ever see text that is safe to show.
mod errors;
mod models;
mod notes;
mod prompts;

pub use errors::{describe_error, friendly_message, ApiError};
pub use models::{pick_notes_model, ModelInfo};
pub use notes::{parse_flow, parse_notes, validate_notes, validate_process, without_times};

use errors::{into_user_error, is_busy, is_rate_limit, status_and_text};
use futures_util::future::try_join_all;
use pechu_common::{user_err, Progress, TranscribeMode};
use pechu_text::{chunk_text, extract_words, turns_from_words};
use prompts::{DRAFTS_NOTE, FLOW_PROMPT, NOTES_PROMPT, PART_PROMPT, TRANSLATE_PROMPT};
use regex::Regex;
use serde_json::{json, Value};
use std::error::Error as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const KEY_HEADER: &str = "x-goog-api-key";
const TRANSCRIBE_MODEL: &str = "gemini-3.5-transcribe";
const NO_MODEL: &str = "No suitable Gemini model was found for notes. Set NOTES_MODEL in .env.";

const CHUNK_CHARS: usize = 10_000; // translation and part drafts work on pieces of about this size
const LONG_CHARS: usize = 12_000; // longer English transcripts are written up in parts first
const AT_ONCE: usize = 3; // how many pieces are sent to Google at the same time
const POLL_TRIES: usize = 150; // a file is checked up to this many times while Google processes it

// Lengths are counted in characters (the TypeScript version counted UTF-16 units; the difference only matters for emoji).
fn len(s: &str) -> usize {
    s.chars().count()
}

/// How long to wait before trying again, and how many tries in all (`withRetry`).
#[derive(Clone, Copy)]
struct RetryPolicy {
    attempts: u32,
    wait: Duration,
}

#[derive(Clone)]
pub struct Gemini {
    key: String,
    http: reqwest::Client,
    /// e.g. https://generativelanguage.googleapis.com/v1beta (no trailing slash)
    base: String,
    notes_model: Option<String>,
    /// The model found by `discover_notes_model`, shared by every clone of this client.
    discovered: Arc<Mutex<Option<String>>>,
    retry: RetryPolicy,
    poll_every: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    Valid,
    Invalid,
}

// A call that goes through `with_retry` tells this function how many seconds it is about to wait.
type OnWait<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// A network failure (no answer at all). The text includes the causes so that "connection refused" and "timed out" are
/// recognised later by `friendly_message`. Our requests never contain the key in the URL, so the text cannot leak it.
fn network_error(e: reqwest::Error) -> anyhow::Error {
    let mut message = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        message.push_str(&format!(": {c}"));
        cause = c.source();
    }
    anyhow::Error::new(ApiError { status: None, message })
}

impl Gemini {
    pub fn new(api_key: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("the HTTPS client could not be created");
        Gemini {
            key: api_key.trim().to_string(),
            http,
            base: DEFAULT_BASE.to_string(),
            notes_model: None,
            discovered: Arc::new(Mutex::new(None)),
            retry: RetryPolicy { attempts: 6, wait: Duration::from_secs(65) },
            poll_every: Duration::from_secs(2),
        }
    }
    /// Tests point this at a local fake server instead of https://generativelanguage.googleapis.com/v1beta.
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base = url.trim_end_matches('/').to_string();
        self
    }
    /// The address requests go to (the real one, or a test server).
    pub fn base_url(&self) -> &str {
        &self.base
    }
    /// NOTES_MODEL override (the web version reads it from the environment).
    pub fn with_notes_model(mut self, model: Option<String>) -> Self {
        self.notes_model = model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        self
    }
    /// How many tries a rate-limited or busy call gets, and how long to wait between them (the web version: 6 and 65 s).
    /// A "busy" wait is 2, 4, 8 ... seconds but never longer than `wait`. Tests use a few milliseconds.
    pub fn with_retry_policy(mut self, attempts: u32, wait: Duration) -> Self {
        self.retry = RetryPolicy { attempts, wait };
        self
    }
    /// How often to ask Google whether an uploaded file is ready (the web version: every 2 s).
    pub fn with_poll_interval(mut self, every: Duration) -> Self {
        self.poll_every = every;
        self
    }

    // --- plumbing ---

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.http.request(method, url).header(KEY_HEADER, &self.key)
    }

    /// Sends a request. A network failure or a status outside 200-299 becomes an `ApiError`.
    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let res = req.send().await.map_err(network_error)?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let message = res.text().await.unwrap_or_default();
        Err(anyhow::Error::new(ApiError { status: Some(status.as_u16()), message }))
    }

    async fn send_json(&self, req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        self.send(req).await?.json().await.map_err(network_error)
    }

    /// Waits and tries again for the per-minute token cap and for "busy"; gives up at once on anything else
    /// (including the daily cap, which a wait cannot fix). `withRetry`.
    async fn with_retry<T, Fut>(&self, on_wait: OnWait<'_>, mut call: impl FnMut() -> Fut) -> anyhow::Result<T>
    where
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let mut attempt = 1;
        loop {
            let e = match call().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            let (status, raw) = status_and_text(&e);
            let wait = if is_rate_limit(status, &raw) {
                self.retry.wait
            } else if is_busy(status, &raw) {
                // busy usually clears fast: 2, 4, 8, 16 s
                self.retry.wait.min(Duration::from_secs(1 << attempt.min(16)))
            } else {
                return Err(e);
            };
            if attempt >= self.retry.attempts {
                return Err(e);
            }
            tracing::warn!("[gemini] rate limit or busy, trying again in {} s (attempt {attempt} of {})", wait.as_secs(), self.retry.attempts);
            on_wait(wait.as_secs());
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// One `generateContent` call; returns the text of the answer. With a `schema` the answer is JSON in that shape,
    /// without one it is free text at temperature 0.2.
    async fn generate(&self, model: &str, system: &str, contents: &str, schema: Option<&Value>, on_wait: OnWait<'_>) -> anyhow::Result<String> {
        let config = match schema {
            Some(s) => json!({ "responseMimeType": "application/json", "responseSchema": s }),
            None => json!({ "temperature": 0.2 }),
        };
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": contents }] }],
            "systemInstruction": { "role": "user", "parts": [{ "text": system }] },
            "generationConfig": config,
        });
        let url = format!("{}/models/{model}:generateContent", self.base);
        let (this, url, body) = (self, &url, &body);
        let answer = self
            .with_retry(on_wait, || async move {
                // writing long notes can take minutes, so the limit is generous
                let req = this.request(reqwest::Method::POST, url).json(body).timeout(Duration::from_secs(600));
                this.send_json(req).await
            })
            .await?;
        Ok(answer_text(&answer))
    }

    async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self.request(reqwest::Method::GET, &format!("{}/models", self.base)).query(&[("pageSize", "200")]);
            if let Some(t) = &token {
                req = req.query(&[("pageToken", t.as_str())]);
            }
            let page = self.send_json(req.timeout(Duration::from_secs(15))).await?;
            if let Some(models) = page.get("models") {
                out.extend(serde_json::from_value::<Vec<ModelInfo>>(models.clone())?);
            }
            token = page["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(String::from);
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    fn cached_model(&self) -> Option<String> {
        self.discovered.lock().expect("model cache lock").clone()
    }

    async fn model_or_error(&self) -> anyhow::Result<String> {
        self.notes_model().await?.ok_or_else(|| user_err(NO_MODEL))
    }

    // --- public: key and models ---

    /// GET /models?pageSize=1. Network trouble -> a `user_err`, a rejected key -> Ok(Invalid).
    pub async fn check_key(&self) -> anyhow::Result<KeyCheck> {
        let url = format!("{}/models", self.base);
        let req = self.request(reqwest::Method::GET, &url).query(&[("pageSize", "1")]).timeout(Duration::from_secs(15));
        let result = async {
            let status = req.send().await.map_err(network_error)?.status();
            match status.as_u16() {
                200..=299 => Ok(KeyCheck::Valid),
                400 | 401 | 403 => Ok(KeyCheck::Invalid),
                other => Err(user_err(format!("Google could not check the key right now (status {other})."))),
            }
        };
        result.await.map_err(|e| into_user_error(e, &self.key))
    }

    /// The model used for notes: the override, else discovered from the models list (`pickNotesModel`), cached.
    pub async fn notes_model(&self) -> anyhow::Result<Option<String>> {
        if let Some(m) = &self.notes_model {
            return Ok(Some(m.clone()));
        }
        if let Some(m) = self.cached_model() {
            return Ok(Some(m));
        }
        self.discover_notes_model().await
    }

    /// Asks Google for the models list again and remembers the pick. A failure is not an error here: the answer is
    /// `None` ("no model known") and the next call tries again.
    pub async fn discover_notes_model(&self) -> anyhow::Result<Option<String>> {
        let found = match self.list_models().await {
            Ok(models) => pick_notes_model(&models),
            Err(e) => {
                tracing::warn!("[gemini] could not list models: {}", describe_error(&e, &[&self.key]));
                None
            }
        };
        *self.discovered.lock().expect("model cache lock") = found.clone();
        Ok(found)
    }

    // --- public: translation and notes ---

    /// Translates the transcript to English in parts (3 at a time, in order). `translateToEnglish`.
    ///
    /// Translating the whole transcript inside the notes request made Gemini condense it (a 28-minute meeting came back
    /// as a short summary). So the English version is made first, in parts, and the notes are written from that.
    pub async fn translate_to_english(&self, transcript: &str, progress: Progress) -> anyhow::Result<String> {
        self.translate_inner(transcript, progress).await.map_err(|e| into_user_error(e, &self.key))
    }

    async fn translate_inner(&self, transcript: &str, progress: Progress) -> anyhow::Result<String> {
        let model = self.model_or_error().await?;
        let chunks = chunk_text(transcript, CHUNK_CHARS);
        let total = chunks.len();
        progress(if total > 1 { format!("Translating {total} parts") } else { "Translating to English".to_string() });
        let waiting = |sec: u64| progress(format!("Gemini is busy, trying again in {sec} s"));
        let done = AtomicUsize::new(0);
        // The parts are independent, so a few are translated at the same time: every part is started, but each one waits for one
        // of AT_ONCE permits before it calls Google. `try_join_all` keeps the results in order and stops at the first failure.
        // (An earlier version used `stream.map(closure).buffered()`; the compiler cannot prove such a future is `Send`, and the
        // app needs `Send` to run jobs in the background. See tests/send_check.rs.)
        let gate = Semaphore::new(AT_ONCE);
        let parts: Vec<String> = try_join_all(
            chunks.iter().map(|chunk| self.translate_part(&model, chunk, total, &gate, &waiting, &done, &progress)),
        )
        .await?;
        Ok(parts.join("\n\n"))
    }

    /// One part of the transcript, translated; waits for its turn first (see `translate_inner`).
    #[allow(clippy::too_many_arguments)]
    async fn translate_part(
        &self,
        model: &str,
        chunk: &str,
        total: usize,
        gate: &Semaphore,
        on_wait: OnWait<'_>,
        done: &AtomicUsize,
        progress: &Progress,
    ) -> anyhow::Result<String> {
        let _turn = gate.acquire().await?; // released when this function ends
        let mut english = self.translate_chunk(model, chunk, false, on_wait).await?;
        // came back far too short: ask once more, with a stronger reminder
        if len(chunk) > 1500 && (len(&english) as f64) < len(chunk) as f64 * 0.3 {
            english = self.translate_chunk(model, chunk, true, on_wait).await?;
        }
        if total > 1 {
            progress(format!("Translating: {} of {total} parts done", done.fetch_add(1, Ordering::SeqCst) + 1));
        }
        Ok(english)
    }

    async fn translate_chunk(&self, model: &str, text: &str, insist: bool, on_wait: OnWait<'_>) -> anyhow::Result<String> {
        let system = if insist {
            format!("{}\nIMPORTANT: your last answer was far too short. Translate every sentence this time.", TRANSLATE_PROMPT)
        } else {
            TRANSLATE_PROMPT.to_string()
        };
        let out = self.generate(model, &system, text, None, on_wait).await?;
        let out = out.trim();
        if out.is_empty() {
            return Err(user_err("Gemini returned an empty translation. Try again."));
        }
        Ok(out.to_string())
    }

    /// What the final call reads: the transcript itself, or, for a long one, a draft of each part (so detail is not lost).
    /// The bool says whether drafts were used.
    async fn source_for(&self, model: &str, english: &str, progress: &Progress) -> anyhow::Result<(String, bool)> {
        if len(english) <= LONG_CHARS {
            return Ok((format!("Transcript:\n\"\"\"\n{english}\n\"\"\""), false));
        }
        let parts = chunk_text(english, CHUNK_CHARS);
        let total = parts.len();
        let waiting = |sec: u64| progress(format!("Gemini is busy, trying again in {sec} s"));
        let gate = Semaphore::new(AT_ONCE); // same idea as in `translate_inner`
        let drafts: Vec<String> = try_join_all(
            parts.iter().enumerate().map(|(i, part)| self.draft_part(model, part, i, total, &gate, &waiting, progress)),
        )
        .await?;
        let contents = format!("Detailed notes on the parts of the meeting, in order:\n\"\"\"\n{}\n\"\"\"", drafts.join("\n\n"));
        Ok((contents, true))
    }

    /// A draft of one part of a long meeting; waits for its turn first.
    #[allow(clippy::too_many_arguments)]
    async fn draft_part(
        &self,
        model: &str,
        part: &str,
        i: usize,
        total: usize,
        gate: &Semaphore,
        on_wait: OnWait<'_>,
        progress: &Progress,
    ) -> anyhow::Result<String> {
        let _turn = gate.acquire().await?;
        progress(format!("Summarising part {} of {total}", i + 1));
        let contents = format!("Part {} of {total}:\n\"\"\"\n{part}\n\"\"\"", i + 1);
        let draft = self.generate(model, PART_PROMPT.as_str(), &contents, None, on_wait).await?;
        let draft = draft.trim();
        if draft.is_empty() {
            return Err(user_err("Gemini returned an empty summary of one part. Try again."));
        }
        Ok(format!("=== Part {} of {total} ===\n{draft}", i + 1))
    }

    /// The notes object (see `Notes` in server/store.ts) from an English transcript, already validated. `notesFromEnglish`.
    pub async fn notes_from_english(&self, english: &str, progress: Progress) -> anyhow::Result<Value> {
        self.notes_inner(english, progress).await.map_err(|e| into_user_error(e, &self.key))
    }

    async fn notes_inner(&self, english: &str, progress: Progress) -> anyhow::Result<Value> {
        need_speech(english)?;
        let model = self.model_or_error().await?;
        let (contents, long) = self.source_for(&model, english.trim(), &progress).await?;
        progress("Writing the summary and flow charts".to_string());
        let system = format!("{}{}", *NOTES_PROMPT, if long { DRAFTS_NOTE } else { "" });
        let waiting = |sec: u64| progress(format!("Gemini is busy, trying again in {sec} s"));
        let raw = self.generate(&model, &system, &contents, Some(&notes::notes_schema()), &waiting).await?;
        let mut notes = parse_notes(&raw)?;
        if !has_stamps(english) {
            without_times(&mut notes);
        }
        Ok(notes)
    }

    /// `{ "flow": ..., "process": ... (optional) }` from an English transcript. `flowFromEnglish`.
    pub async fn flow_from_english(&self, english: &str, title: &str, progress: Progress) -> anyhow::Result<Value> {
        self.flow_inner(english, title, progress).await.map_err(|e| into_user_error(e, &self.key))
    }

    async fn flow_inner(&self, english: &str, title: &str, progress: Progress) -> anyhow::Result<Value> {
        need_speech(english)?;
        let model = self.model_or_error().await?;
        let (contents, long) = self.source_for(&model, english.trim(), &progress).await?;
        progress("Drawing the flow chart".to_string());
        let system = format!("{}{}", *FLOW_PROMPT, if long { DRAFTS_NOTE } else { "" });
        let contents = format!("Meeting title: {title}\n\n{contents}");
        let waiting = |sec: u64| progress(format!("Gemini is busy, trying again in {sec} s"));
        let raw = self.generate(&model, &system, &contents, Some(&notes::flow_only_schema()), &waiting).await?;
        let mut out = parse_flow(&raw)?;
        if !has_stamps(english) {
            without_times(&mut out);
        }
        Ok(out)
    }

    /// Translate, tell `on_english` as soon as the English text exists, then write the notes. Returns (notes, english).
    pub async fn generate_notes(
        &self,
        transcript: &str,
        progress: Progress,
        on_english: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<(Value, String)> {
        need_speech(transcript).map_err(|e| into_user_error(e, &self.key))?;
        let english = self.translate_to_english(transcript, progress.clone()).await?;
        on_english(english.clone());
        let notes = self.notes_from_english(&english, progress).await?;
        Ok((notes, english))
    }

    // --- public: transcription with Gemini's file model ---

    /// Transcribes WAV parts (already cut by `pechu_audio::prepare_parts`) with Gemini's file model, one part after another,
    /// joined with a blank line. Handles upload, processing wait, delete, and rate-limit retries (`withRetry`).
    pub async fn transcribe_parts(&self, parts: &[PathBuf], mode: TranscribeMode, progress: Progress) -> anyhow::Result<String> {
        let total = parts.len();
        let mut texts: Vec<String> = Vec::new();
        for (i, file) in parts.iter().enumerate() {
            let label = if total > 1 { format!("Part {} of {total}", i + 1) } else { "Transcribing".to_string() };
            progress(label.clone());
            let waiting = |_sec: u64| progress(format!("{label}, waiting for Google"));
            let text = self
                .with_retry(&waiting, || self.transcribe_one(file, mode))
                .await
                .map_err(|e| into_user_error(e, &self.key))?;
            texts.push(text);
        }
        Ok(texts.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n"))
    }

    /// Upload, wait until Google has processed the file, transcribe, and always try to delete the file again.
    async fn transcribe_one(&self, path: &Path, mode: TranscribeMode) -> anyhow::Result<String> {
        let file = self.upload(path, "audio/wav").await?;
        let result = self.transcribe_uploaded(file.clone(), mode).await;
        // best effort; Google also expires files itself
        if let Err(e) = self.request(reqwest::Method::DELETE, &format!("{}/{}", self.base, file.name)).send().await {
            tracing::debug!("[gemini] could not delete {}: {e}", file.name);
        }
        result
    }

    async fn transcribe_uploaded(&self, mut file: UploadedFile, mode: TranscribeMode) -> anyhow::Result<String> {
        for _ in 0..POLL_TRIES {
            if file.state.as_deref() != Some("PROCESSING") {
                break;
            }
            tokio::time::sleep(self.poll_every).await;
            let url = format!("{}/{}", self.base, file.name);
            file = UploadedFile::from_json(&self.send_json(self.request(reqwest::Method::GET, &url)).await?)?;
        }
        match file.state.as_deref() {
            Some("FAILED") => return Err(user_err("Google could not process this audio file.")),
            Some("PROCESSING") => return Err(user_err("Google took too long to process this audio file.")),
            _ => {}
        }
        // "speakers" = as-spoken text with a speaker label on every turn (Google only allows labels in the as-spoken style).
        let mode_json = match mode {
            TranscribeMode::Speakers => json!({ "type": "verbatim", "diarization_mode": "speaker" }),
            TranscribeMode::Verbatim => json!("verbatim"),
            TranscribeMode::Smart => json!("smart"),
        };
        let body = json!({
            "model": TRANSCRIBE_MODEL,
            "input": [{ "type": "audio", "uri": file.uri, "mime_type": file.mime_type }],
            "generation_config": { "transcription_config": { "language_codes": [], "mode": mode_json } },
        });
        let req = self.request(reqwest::Method::POST, &format!("{}/interactions", self.base)).json(&body).timeout(Duration::from_secs(1800));
        let res = self.send_json(req).await?;
        // With speaker labels the speakers come on the word list; without them (or if it is missing) use the plain text.
        if mode == TranscribeMode::Speakers {
            let turns = turns_from_words(&extract_words(&res));
            if !turns.is_empty() {
                return Ok(turns);
            }
        }
        Ok(output_text(&res).trim().to_string())
    }

    /// Google's resumable upload: first ask for an upload address, then send all the bytes to it in one request.
    async fn upload(&self, path: &Path, mime: &str) -> anyhow::Result<UploadedFile> {
        let bytes = tokio::fs::read(path).await?;
        // The upload address lives next to the API, under /upload/<version>/ (not under the API's own path).
        let root = self.base.strip_suffix("/v1beta").unwrap_or(&self.base);
        let start = self
            .request(reqwest::Method::POST, &format!("{root}/upload/v1beta/files"))
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", bytes.len().to_string())
            .header("X-Goog-Upload-Header-Content-Type", mime)
            .json(&json!({ "file": { "mimeType": mime, "sizeBytes": bytes.len().to_string() } }));
        let started = self.send(start).await?;
        let upload_url = started
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("Failed to get upload url. Server did not return the x-google-upload-url in the headers"))?;
        let send_bytes = self
            .request(reqwest::Method::POST, &upload_url)
            .header("X-Goog-Upload-Command", "upload, finalize")
            .header("X-Goog-Upload-Offset", "0")
            .body(bytes)
            .timeout(Duration::from_secs(600));
        let done = self.send_json(send_bytes).await?;
        UploadedFile::from_json(&done)
    }
}

/// A file as Google describes it after an upload or when asked about it.
#[derive(Clone, Debug)]
struct UploadedFile {
    name: String,
    uri: String,
    mime_type: String,
    state: Option<String>,
}

impl UploadedFile {
    /// Reads `{ "file": {...} }` (the upload answer) or the bare file object (the answer to GET files/<id>).
    fn from_json(v: &Value) -> anyhow::Result<Self> {
        let f = if v["file"].is_object() { &v["file"] } else { v };
        let name = f["name"].as_str().ok_or_else(|| anyhow::anyhow!("Google's answer to the upload had no file name"))?;
        Ok(UploadedFile {
            name: name.to_string(),
            uri: f["uri"].as_str().unwrap_or_default().to_string(),
            mime_type: f["mimeType"].as_str().unwrap_or("audio/wav").to_string(),
            state: f["state"].as_str().map(String::from),
        })
    }
}

/// The text of the first answer of a `generateContent` reply, leaving out the model's "thought" parts.
fn answer_text(res: &Value) -> String {
    res["candidates"][0]["content"]["parts"]
        .as_array()
        .map(|parts| parts.iter().filter(|p| p["thought"] != json!(true)).filter_map(|p| p["text"].as_str()).collect::<String>())
        .unwrap_or_default()
}

/// The text of an interactions reply: `output_text`, or all the text pieces of all steps.
fn output_text(res: &Value) -> String {
    if let Some(t) = res["output_text"].as_str() {
        return t.to_string();
    }
    res["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .collect()
}

fn need_speech(english: &str) -> anyhow::Result<()> {
    if len(english.trim()) < 20 {
        return Err(user_err("There is not enough speech to write notes."));
    }
    Ok(())
}

static STAMP_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\[(?:\d+:)?\d{1,2}:\d{2}\]\s+").unwrap());

/// Does the transcript have [mm:ss] time stamps at the start of a line?
fn has_stamps(text: &str) -> bool {
    STAMP_LINE.is_match(text)
}

// `validate_notes`, `parse_notes` and `parse_flow` live in notes.rs and are re-exported at the top of this file.

/// Shows only the first and last characters of a key.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_masked() {
        assert_eq!(mask_key("short"), "****");
        assert_eq!(mask_key("AIzaSyFakeGeminiKey1234567890"), "AIza...90");
    }

    #[test]
    fn stamps_are_found_only_at_the_start_of_a_line() {
        assert!(has_stamps("[00:20] Speaker 1: hi"));
        assert!(has_stamps("intro\n\n[1:02:05] Speaker 2: hi"));
        assert!(!has_stamps("Speaker 1: we met at [00:20] yesterday"));
        assert!(!has_stamps("no stamps here"));
    }

    #[test]
    fn answer_text_skips_thoughts_and_output_text_reads_both_shapes() {
        let res = json!({ "candidates": [{ "content": { "parts": [{ "text": "plan", "thought": true }, { "text": "Hello " }, { "text": "world" }] } }] });
        assert_eq!(answer_text(&res), "Hello world");
        assert_eq!(answer_text(&json!({})), "");
        assert_eq!(output_text(&json!({ "output_text": "plain" })), "plain");
        let steps = json!({ "steps": [{ "content": [{ "text": "a" }, { "type": "x" }] }, { "content": [{ "text": "b" }] }] });
        assert_eq!(output_text(&steps), "ab");
    }
}

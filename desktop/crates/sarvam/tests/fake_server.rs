//! Runs the whole Sarvam flow (create job -> upload -> start -> poll -> download) against a fake Sarvam on this machine.
//! Nothing here talks to the real service and no real key is used.
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use axum::body::Bytes;
use pechu_audio::{wav_bytes, RATE};
use pechu_common::{friendly, progress};
use pechu_sarvam::{transcribe_with_sarvam, SarvamEndpoint, SarvamMode, SarvamOpts};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEY: &str = "test-key-not-real";
const CONTEXT_FAILURE: &str = r#"400:{"error":{"message":"Input length (4192) exceeds model's maximum context length (4096).","code":"invalid_request_error"}}"#;

#[derive(Default)]
struct Fake {
    base: String,
    /// A file bigger than this many bytes is refused with the "context length" failure (0 = nothing is refused).
    refuse_over_bytes: usize,
    /// Every file fails with this text, whatever its size.
    fail_all_with: Option<String>,
    jobs: HashMap<String, Job>,
    create_bodies: Vec<Value>,
    /// What the storage links were called with: (file name, had the Sarvam key header, x-ms-blob-type, bytes received, content-length).
    uploads: Vec<(String, bool, String, usize, String)>,
    started: Vec<String>,
}

#[derive(Default)]
struct Job {
    files: HashMap<String, usize>, // input file name -> bytes
    polls: u32,
}

type Shared = Arc<Mutex<Fake>>;

fn key_ok(headers: &HeaderMap) -> bool {
    headers.get("api-subscription-key").and_then(|v| v.to_str().ok()) == Some(KEY)
}

fn unauthorized() -> Response {
    (StatusCode::FORBIDDEN, r#"{"error":{"message":"Invalid API key"}}"#).into_response()
}

async fn create(State(s): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !key_ok(&headers) {
        return unauthorized();
    }
    let mut fake = s.lock().unwrap();
    fake.create_bodies.push(body);
    let id = format!("job-{}", fake.jobs.len() + 1);
    fake.jobs.insert(id.clone(), Job::default());
    Json(json!({ "job_id": id, "job_state": "Accepted", "storage_container_type": "Azure" })).into_response()
}

async fn upload_links(State(s): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !key_ok(&headers) {
        return unauthorized();
    }
    let mut fake = s.lock().unwrap();
    let job_id = body["job_id"].as_str().unwrap().to_string();
    let mut urls = serde_json::Map::new();
    for name in body["files"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        fake.jobs.get_mut(&job_id).unwrap().files.insert(name.to_string(), 0);
        urls.insert(name.to_string(), json!({ "file_url": format!("{}/blob/{job_id}/{name}", fake.base) }));
    }
    Json(json!({ "job_id": job_id, "job_state": "Accepted", "upload_urls": urls, "storage_container_type": "Azure" })).into_response()
}

async fn blob_put(State(s): State<Shared>, UrlPath((job, name)): UrlPath<(String, String)>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let mut fake = s.lock().unwrap();
    let header = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    fake.uploads.push((name.clone(), headers.contains_key("api-subscription-key"), header("x-ms-blob-type"), body.len(), header("content-length")));
    fake.jobs.get_mut(&job).unwrap().files.insert(name, body.len());
    StatusCode::CREATED
}

async fn start(State(s): State<Shared>, UrlPath(job): UrlPath<String>, headers: HeaderMap) -> Response {
    if !key_ok(&headers) {
        return unauthorized();
    }
    s.lock().unwrap().started.push(job.clone());
    Json(json!({ "job_id": job, "job_state": "Running" })).into_response()
}

async fn status(State(s): State<Shared>, UrlPath(job_id): UrlPath<String>, headers: HeaderMap) -> Response {
    if !key_ok(&headers) {
        return unauthorized();
    }
    let mut fake = s.lock().unwrap();
    let (refuse, fail_all) = (fake.refuse_over_bytes, fake.fail_all_with.clone());
    let job = fake.jobs.get_mut(&job_id).unwrap();
    job.polls += 1;
    if job.polls == 1 {
        return Json(json!({ "job_id": job_id, "job_state": "Running" })).into_response(); // the first poll: not finished yet
    }
    let mut names: Vec<&String> = job.files.keys().collect();
    names.sort();
    let details: Vec<Value> = names
        .iter()
        .map(|name| {
            let failure = fail_all.clone().or_else(|| (refuse > 0 && job.files[*name] > refuse).then(|| CONTEXT_FAILURE.to_string()));
            match failure {
                Some(text) => json!({ "inputs": [{ "file_name": name, "file_id": "x" }], "state": "API Error", "error_message": text }),
                None => json!({
                    "inputs": [{ "file_name": name, "file_id": "x" }],
                    "outputs": [{ "file_name": format!("out-{name}"), "file_id": "y" }],
                    "state": "Success",
                }),
            }
        })
        .collect();
    Json(json!({ "job_id": job_id, "job_state": "Completed", "job_details": details })).into_response()
}

async fn download_links(State(s): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !key_ok(&headers) {
        return unauthorized();
    }
    let fake = s.lock().unwrap();
    let job_id = body["job_id"].as_str().unwrap();
    let mut urls = serde_json::Map::new();
    for name in body["files"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        urls.insert(name.to_string(), json!({ "file_url": format!("{}/out/{job_id}/{name}", fake.base) }));
    }
    Json(json!({ "job_id": job_id, "job_state": "Completed", "download_urls": urls })).into_response()
}

/// The output file for `out-<input>`: Sarvam's times start at 0.5 s inside each file, and there is one speaker sentence.
async fn blob_get(UrlPath((_job, file)): UrlPath<(String, String)>) -> Json<Value> {
    let input = file.trim_start_matches("out-");
    let text = format!("said {input}");
    Json(json!({
        "transcript": text,
        "timestamps": { "words": [text], "start_time_seconds": [0.5], "end_time_seconds": [2.0] },
        "diarized_transcript": { "entries": [{ "transcript": text, "speaker_id": "0", "start_time_seconds": 1.0, "end_time_seconds": 2.0 }] },
    }))
}

async fn start_fake(setup: impl FnOnce(&mut Fake)) -> (String, Shared) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut fake = Fake { base: base.clone(), ..Fake::default() };
    setup(&mut fake);
    let shared: Shared = Arc::new(Mutex::new(fake));
    let app = Router::new()
        .route("/speech-to-text/job/v1", post(create))
        .route("/speech-to-text/job/v1/upload-files", post(upload_links))
        .route("/speech-to-text/job/v1/download-files", post(download_links))
        .route("/speech-to-text/job/v1/{id}/start", post(start))
        .route("/speech-to-text/job/v1/{id}/status", get(status))
        .route("/blob/{job}/{name}", put(blob_put))
        .route("/out/{job}/{file}", get(blob_get))
        .layer(axum::extract::DefaultBodyLimit::disable()) // audio uploads are bigger than axum's 2 MB default
        .with_state(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, shared)
}

/// A tone, so there is real audio to decode and cut. 16 kHz mono 16-bit.
fn write_tone(dir: &std::path::Path, name: &str, seconds: usize) -> std::path::PathBuf {
    let pcm: Vec<i16> = (0..seconds * RATE as usize).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 8000.0) as i16).collect();
    let path = dir.join(name);
    std::fs::write(&path, wav_bytes(&pcm)).unwrap();
    path
}

fn notes() -> (pechu_common::Progress, Arc<Mutex<Vec<String>>>) {
    let list = Arc::new(Mutex::new(Vec::new()));
    let list_in = list.clone();
    (progress(move |n| list_in.lock().unwrap().push(n)), list)
}

const OPTS: SarvamOpts = SarvamOpts { mode: SarvamMode::Verbatim, speakers: true };

fn endpoint(base: &str) -> SarvamEndpoint {
    SarvamEndpoint { base_url: Some(base.to_string()) }
}

#[tokio::test]
async fn the_happy_path_creates_uploads_starts_polls_and_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_tone(dir.path(), "meeting.wav", 20);
    let (base, fake) = start_fake(|_| {}).await;
    let (progress, notes) = notes();

    let text = transcribe_with_sarvam(KEY, &file, OPTS, progress, endpoint(&base)).await.unwrap();

    // speakers were asked for, so the diarized sentence comes back as a speaker turn
    assert_eq!(text, "[00:01] Speaker 1: said meeting.wav");
    let fake = fake.lock().unwrap();
    assert_eq!(fake.create_bodies.len(), 1);
    assert_eq!(
        fake.create_bodies[0],
        json!({ "job_parameters": { "language_code": "unknown", "model": "saaras:v4", "with_diarization": true, "with_timestamps": true, "mode": "verbatim" } })
    );
    assert_eq!(fake.started, vec!["job-1"]);
    // the file was put on the storage link with the blob header, its full length stated, and without the Sarvam key
    assert_eq!(fake.uploads.len(), 1);
    let (name, had_key, blob_type, received, content_length) = &fake.uploads[0];
    assert_eq!(name, "meeting.wav");
    assert!(!had_key);
    assert_eq!(blob_type, "BlockBlob");
    assert_eq!(*received, std::fs::metadata(&file).unwrap().len() as usize);
    assert_eq!(content_length, &received.to_string());
    assert_eq!(*notes.lock().unwrap(), vec!["Sending the recording to Sarvam", "Transcribing with Sarvam"]);
}

#[tokio::test]
async fn a_whole_file_sarvam_refuses_is_cut_into_pieces_and_stitched() {
    let dir = tempfile::tempdir().unwrap();
    // 200 s = 6.4 MB is refused; pieces of about 120 s (3.8 MB) are taken
    let file = write_tone(dir.path(), "long.wav", 200);
    let (base, fake) = start_fake(|f| f.refuse_over_bytes = 5_000_000).await;
    let (progress, notes) = notes();

    let text = transcribe_with_sarvam(KEY, &file, OPTS, progress, endpoint(&base)).await.unwrap();

    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    assert_eq!(paragraphs.len(), 2, "{text}");
    assert_eq!(paragraphs[0], "[00:00] said part-01.wav");
    // the second piece starts about two minutes in, and its own 0.5 s offset is added to that
    let second = paragraphs[1].strip_prefix('[').unwrap();
    let (minutes, rest) = second.split_once(':').unwrap();
    assert!(rest.contains("said part-02.wav"));
    assert!(matches!(minutes, "01" | "02"), "second piece starts at {second}");

    let fake = fake.lock().unwrap();
    assert_eq!(fake.create_bodies.len(), 2); // the whole file, then one job for both pieces
    assert_eq!(fake.create_bodies[0]["job_parameters"]["with_diarization"], true);
    assert_eq!(fake.create_bodies[1]["job_parameters"]["with_diarization"], false); // no speaker labels in piece mode
    assert_eq!(fake.uploads.iter().map(|u| u.0.as_str()).collect::<Vec<_>>(), vec!["long.wav", "part-01.wav", "part-02.wav"]);
    assert_eq!(
        *notes.lock().unwrap(),
        vec![
            "Sending the recording to Sarvam",
            "Transcribing with Sarvam",
            "Sarvam could not take the whole recording, so it is being cut into parts. Speaker labels are not available this way.",
            "Transcribing parts 1 to 2 of 2 with Sarvam",
            "Sending 2 parts to Sarvam",
            "Transcribing 2 parts with Sarvam",
        ]
    );
}

#[tokio::test]
async fn another_failure_for_the_whole_file_is_not_retried_by_cutting() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_tone(dir.path(), "meeting.wav", 5);
    let (base, fake) = start_fake(|f| f.fail_all_with = Some("503:upstream connect error".to_string())).await;
    let err = transcribe_with_sarvam(KEY, &file, OPTS, pechu_common::no_progress(), endpoint(&base)).await.unwrap_err();
    assert!(friendly(&err).contains("trouble on its side"), "{}", friendly(&err));
    assert_eq!(fake.lock().unwrap().create_bodies.len(), 1);
}

#[tokio::test]
async fn a_key_sarvam_does_not_accept_is_explained_in_plain_words() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_tone(dir.path(), "meeting.wav", 5);
    let (base, fake) = start_fake(|_| {}).await;
    let err = transcribe_with_sarvam("a-wrong-key", &file, OPTS, pechu_common::no_progress(), endpoint(&base)).await.unwrap_err();
    assert!(friendly(&err).contains("did not accept the API key"), "{}", friendly(&err));
    assert!(fake.lock().unwrap().uploads.is_empty());
}

#[tokio::test]
async fn a_recording_that_cannot_be_read_fails_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("broken.wav");
    std::fs::write(&file, b"this is not audio at all").unwrap();
    let (base, fake) = start_fake(|_| {}).await;
    let err = transcribe_with_sarvam(KEY, &file, OPTS, pechu_common::no_progress(), endpoint(&base)).await.unwrap_err();
    assert!(friendly(&err).contains("could not read this audio file"), "{}", friendly(&err));
    assert!(fake.lock().unwrap().create_bodies.is_empty());
}

#[tokio::test]
async fn a_server_that_is_not_there_reads_as_a_connection_problem() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_tone(dir.path(), "meeting.wav", 5);
    // nothing listens on port 9
    let err = transcribe_with_sarvam(KEY, &file, OPTS, pechu_common::no_progress(), endpoint("http://127.0.0.1:9")).await.unwrap_err();
    assert!(friendly(&err).contains("could not reach Sarvam"), "{}", friendly(&err));
}

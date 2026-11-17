//! The REST calls behind one Sarvam batch job: create -> get upload links -> upload -> start -> poll -> download.
//! Learned from the `sarvamai` JS SDK (node_modules/sarvamai, `speechToTextJob`), which the web version uses.
use crate::errors::sarvam_message;
use crate::pieces::BatchResult;
use crate::{SarvamMode, SarvamOpts};
use anyhow::Result;
use pechu_common::{user_err, Progress};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_BASE_URL: &str = "https://api.sarvam.ai";
const MODEL: &str = "saaras:v4";
/// Give up waiting for a job after this long (the web version also waits 3 hours).
const JOB_TIMEOUT: Duration = Duration::from_secs(3 * 3600);
const JSON_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(900);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// Like the SDK: a call that failed because of a hiccup (network, 408, 429, 5xx) is tried again twice.
const RETRIES: u32 = 2;

/// Logs the raw failure text (for the log only) and returns the plain message the person is allowed to read.
fn failure(status: Option<u16>, raw: String) -> anyhow::Error {
    tracing::warn!("[sarvam] {raw}");
    user_err(sarvam_message(&raw, status))
}

pub struct SarvamClient {
    http: reqwest::Client,
    base: String,
    key: String,
    /// How long to wait between two checks of a running job (one second; tests use less).
    pub poll_interval: Duration,
}

impl SarvamClient {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build()?;
        let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/').to_string();
        Ok(Self { http, base, key: api_key.to_string(), poll_interval: Duration::from_secs(1) })
    }

    /// One call to Sarvam's own API (not to a presigned storage link): the key goes in this header and nowhere else.
    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}/{path}", self.base);
        let mut attempt = 0;
        loop {
            let mut req = self.http.request(method.clone(), &url).header("api-subscription-key", &self.key).timeout(JSON_TIMEOUT);
            if let Some(b) = body {
                req = req.json(b);
            }
            let (status, raw) = match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    return resp.json::<Value>().await.map_err(|e| failure(None, format!("unreadable answer from Sarvam: {e}")));
                }
                Ok(resp) => {
                    let status = resp.status();
                    // Sarvam's failure text looks like `400:{"error":...}`, which is what the message mapping reads.
                    (Some(status), format!("{}:{}", status.as_u16(), resp.text().await.unwrap_or_default()))
                }
                Err(e) => (None, format!("{e}")),
            };
            let worth_retrying = match status {
                None => true, // could not reach Sarvam at all
                Some(s) => s == StatusCode::REQUEST_TIMEOUT || s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error(),
            };
            if worth_retrying && attempt < RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                continue;
            }
            return Err(failure(status.map(|s| s.as_u16()), raw));
        }
    }

    /// Runs one batch job for these files and returns what worked (the output JSON by input file name) and what failed
    /// (the failure text by input file name). `sending` and `working` are the two progress notes.
    pub async fn run_job(&self, files: &[PathBuf], opts: SarvamOpts, progress: &Progress, sending: &str, working: &str) -> Result<BatchResult> {
        progress(sending.to_string());
        let job_id = self.create_job(opts).await?;
        self.upload_files(&job_id, files).await?;
        self.call(Method::POST, &format!("speech-to-text/job/v1/{job_id}/start"), None).await?;
        progress(working.to_string());
        let status = self.wait_until_complete(&job_id).await?;
        self.collect_results(&job_id, &status).await
    }

    async fn create_job(&self, opts: SarvamOpts) -> Result<String> {
        let mode = match opts.mode {
            SarvamMode::Codemix => "codemix",
            SarvamMode::Verbatim => "verbatim",
        };
        let body = json!({
            "job_parameters": {
                // "unknown" = detect the language by itself, which handles Tamil with English mixed in
                "language_code": "unknown",
                "model": MODEL,
                "with_diarization": opts.speakers,
                "with_timestamps": true,
                "mode": mode,
            }
        });
        let answer = self.call(Method::POST, "speech-to-text/job/v1", Some(&body)).await?;
        match answer["job_id"].as_str() {
            Some(id) => Ok(id.to_string()),
            None => Err(failure(None, "Sarvam did not return a job id".to_string())),
        }
    }

    /// Asks for one upload link per file, then PUTs each file to its link.
    async fn upload_files(&self, job_id: &str, files: &[PathBuf]) -> Result<()> {
        let names: Vec<String> = files.iter().map(|f| file_name(f)).collect();
        let links = self.call(Method::POST, "speech-to-text/job/v1/upload-files", Some(&json!({ "job_id": job_id, "files": names }))).await?;
        for (path, name) in files.iter().zip(&names) {
            let Some(url) = links["upload_urls"][name.as_str()]["file_url"].as_str() else {
                return Err(failure(None, format!("Sarvam gave no upload link for {name}")));
            };
            let file = tokio::fs::File::open(path).await?;
            let length = file.metadata().await?.len();
            // The link is presigned cloud storage (Azure blob): no Sarvam key goes there, and the length must be stated
            // up front because the body is streamed from disk instead of held in memory.
            let resp = self
                .http
                .put(url)
                .header("x-ms-blob-type", "BlockBlob")
                .header("Content-Type", mime_type(path))
                .header("Content-Length", length)
                .timeout(UPLOAD_TIMEOUT)
                .body(file)
                .send()
                .await
                .map_err(|e| failure(None, format!("{e}")))?;
            // the SDK counts 200 to 226 as stored
            let code = resp.status().as_u16();
            if !(200..=226).contains(&code) {
                return Err(failure(Some(code), format!("Upload failed for {name}: {code}")));
            }
        }
        Ok(())
    }

    /// Checks the job until it is Completed or Failed, then returns its last status.
    async fn wait_until_complete(&self, job_id: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            let status = self.call(Method::GET, &format!("speech-to-text/job/v1/{job_id}/status"), None).await?;
            let state = status["job_state"].as_str().unwrap_or("").to_lowercase();
            if state == "completed" || state == "failed" {
                return Ok(status);
            }
            if started.elapsed() > JOB_TIMEOUT {
                return Err(failure(None, format!("Job {job_id} did not complete within {} seconds.", JOB_TIMEOUT.as_secs())));
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Splits the finished job into files that worked (output downloaded) and files that did not (failure text).
    async fn collect_results(&self, job_id: &str, status: &Value) -> Result<BatchResult> {
        let mut result = BatchResult::default();
        let mut to_download: Vec<(String, String)> = Vec::new(); // (input file name, output file name)
        for detail in status["job_details"].as_array().into_iter().flatten() {
            // a detail without an input file name is skipped, like the SDK
            let Some(input) = detail["inputs"][0]["file_name"].as_str() else { continue };
            if detail["state"].as_str() == Some("Success") {
                // a file that worked but has no output file is left out of both lists
                if let Some(output) = detail["outputs"][0]["file_name"].as_str() {
                    to_download.push((input.to_string(), output.to_string()));
                }
            } else {
                let text = detail["error_message"].as_str().unwrap_or("");
                result.failed.insert(input.to_string(), text.to_string());
            }
        }
        if to_download.is_empty() {
            return Ok(result);
        }
        let outputs: Vec<&String> = to_download.iter().map(|(_, output)| output).collect();
        let links = self.call(Method::POST, "speech-to-text/job/v1/download-files", Some(&json!({ "job_id": job_id, "files": outputs }))).await?;
        for (input, output) in &to_download {
            let Some(url) = links["download_urls"][output.as_str()]["file_url"].as_str() else {
                return Err(failure(None, format!("Sarvam gave no download link for {output}")));
            };
            let resp = self.http.get(url).timeout(DOWNLOAD_TIMEOUT).send().await.map_err(|e| failure(None, format!("{e}")))?;
            let code = resp.status().as_u16();
            if !(200..=226).contains(&code) {
                return Err(failure(Some(code), format!("Download failed for {output}: {code}")));
            }
            let bytes = resp.bytes().await.map_err(|e| failure(None, format!("{e}")))?;
            let json: Value = serde_json::from_slice(&bytes).map_err(|e| failure(None, format!("unreadable transcript file {output}: {e}")))?;
            result.ok.insert(input.clone(), json);
        }
        Ok(result)
    }
}

/// The name Sarvam knows a file by: its last path part.
pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Same table as the SDK; anything else is sent as WAV.
fn mime_type(path: &Path) -> &'static str {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "audio/wav",
    }
}

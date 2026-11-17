//! Sarvam Saaras v4 batch transcription over HTTPS (create job, upload, start, wait, download), including cutting a recording
//! into short pieces when Sarvam refuses it whole. Port of server/sarvam.ts. The web version uses the `sarvamai` JS SDK;
//! its source is in node_modules/sarvamai at the repository root, read it to learn the REST calls it makes.
//! CONTRACT: the signatures below are used by other crates. You may add functions; do not change these.
//!
//! Layout: `errors` (messages the person reads), `output` (reading Sarvam's JSON), `api` (the REST calls of one job),
//! `pieces` (piece mode: cutting, batching, stitching).
mod api;
mod errors;
mod output;
mod pieces;

use anyhow::Result;
use api::{file_name, SarvamClient};
use pechu_audio::{audio_seconds, prepare_parts, RATE};
use pechu_common::{user_err, Progress, UserError};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub use errors::{is_sarvam_context_error, sarvam_message};
pub use pieces::{transcribe_pieces, BatchResult, Piece};

/// The first cut: pieces of about two minutes.
const PIECE_SECONDS: u32 = 120;

/// "codemix" keeps English words in English letters inside Tamil sentences; "verbatim" also keeps filler words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SarvamMode {
    Codemix,
    Verbatim,
}

#[derive(Clone, Copy, Debug)]
pub struct SarvamOpts {
    pub mode: SarvamMode,
    pub speakers: bool,
}

/// Tests point this at a local fake server instead of https://api.sarvam.ai.
#[derive(Clone, Debug, Default)]
pub struct SarvamEndpoint {
    pub base_url: Option<String>,
}

/// Transcribes one recording and returns the transcript text (timed paragraphs, or speaker turns when `opts.speakers`).
/// Checks the length first (over the limit: fail before sending anything), tries the whole file, and on Sarvam's
/// "context length" refusal cuts the recording into ~2 minute pieces and sends them in batches of at most 20 files.
/// Runs the blocking audio work through `tokio::task::spawn_blocking`. Errors are `user_err` messages (`sarvam_message`).
pub async fn transcribe_with_sarvam(
    api_key: &str,
    path: &Path,
    opts: SarvamOpts,
    progress: Progress,
    endpoint: SarvamEndpoint,
) -> Result<String> {
    let client = SarvamClient::new(api_key, endpoint.base_url.as_deref()).map_err(as_user_error)?;
    transcribe(&client, path, opts, &progress).await.map_err(as_user_error)
}

/// Anything that is not already written for the person becomes one of Sarvam's plain messages; the details go to the log.
fn as_user_error(e: anyhow::Error) -> anyhow::Error {
    if e.chain().any(|c| c.downcast_ref::<UserError>().is_some()) {
        return e;
    }
    let raw = format!("{e:#}");
    tracing::warn!("[sarvam] {raw}");
    user_err(sarvam_message(&raw, None))
}

/// The same as `transcribe_with_sarvam`, with the client made by the caller (tests use a faster poll).
async fn transcribe(client: &SarvamClient, path: &Path, opts: SarvamOpts, progress: &Progress) -> Result<String> {
    // Too long? Fail now, before anything is uploaded (decoding is blocking work).
    let for_length = path.to_path_buf();
    let seconds = tokio::task::spawn_blocking(move || audio_seconds(&for_length)).await??;
    tracing::info!("[sarvam] recording is {seconds:.0} s long");

    let whole = client.run_job(&[path.to_path_buf()], opts, progress, "Sending the recording to Sarvam", "Transcribing with Sarvam").await?;
    let name = file_name(path);
    if let Some(out) = whole.ok.get(&name) {
        let text = parse_sarvam_output(out, opts.speakers);
        if text.is_empty() {
            return Err(user_err("Sarvam found no speech in this recording."));
        }
        return Ok(text);
    }
    let Some(raw) = whole.failed.get(&name) else {
        return Err(user_err("Sarvam finished but returned no transcript."));
    };
    if !is_sarvam_context_error(raw) {
        tracing::warn!("[sarvam] {raw}");
        return Err(user_err(sarvam_message(raw, None)));
    }

    // Sarvam could not take the whole recording (a failed job is not charged): cut it into short pieces instead.
    tracing::warn!("[sarvam] whole recording refused, splitting into pieces: {}", raw.chars().take(200).collect::<String>());
    progress(if opts.speakers {
        "Sarvam could not take the whole recording, so it is being cut into parts. Speaker labels are not available this way.".to_string()
    } else {
        "Sarvam could not take the whole recording, so it is being cut into parts".to_string()
    });
    let for_cutting = path.to_path_buf();
    // `parts` owns the temporary folder with the pieces; they are deleted when it goes out of scope at the end of this function.
    let parts = tokio::task::spawn_blocking(move || prepare_parts(&for_cutting, PIECE_SECONDS)).await??;
    let mut start = 0.0;
    let mut pieces = Vec::new();
    for file in &parts.files {
        // a 44-byte WAV header, then 16-bit mono samples
        let seconds = (std::fs::metadata(file)?.len().saturating_sub(44)) as f64 / (f64::from(RATE) * 2.0);
        pieces.push(Piece { name: file_name(file), path: file.clone(), start, seconds });
        start += seconds;
    }
    // Speaker labels are not asked for: each piece is labelled on its own, so the names would not match between pieces.
    let piece_opts = SarvamOpts { speakers: false, ..opts };
    transcribe_pieces(
        pieces,
        move |files: Vec<Piece>| async move {
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
            client
                .run_job(&paths, piece_opts, progress, &format!("Sending {} parts to Sarvam", files.len()), &format!("Transcribing {} parts with Sarvam", files.len()))
                .await
        },
        progress,
    )
    .await
}

/// Turns one batch output JSON into the transcript Pechu stores (speaker turns, else timed paragraphs, else plain text).
pub fn parse_sarvam_output(out: &Value, speakers: bool) -> String {
    output::parse_sarvam_output(out, speakers)
}

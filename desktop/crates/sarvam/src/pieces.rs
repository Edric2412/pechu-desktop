//! Piece mode: a recording Sarvam could not take whole is cut into short WAV pieces, sent in batches, and put back together
//! with the right times. Port of `transcribePieces` and `halve` in server/sarvam.ts.
//! The "send these files to Sarvam" step is passed in, so tests can replace it with a fake and send nothing anywhere.
use crate::errors::{is_sarvam_context_error, sarvam_message};
use crate::output::parse_sarvam_output;
use anyhow::{bail, Result};
use pechu_audio::{choose_cuts, read_wav_pcm, wav_bytes, RATE};
use pechu_common::{user_err, Progress};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

const MIN_PIECE_SECONDS: f64 = 30.0; // a piece shorter than twice this is not cut again
const FILES_PER_JOB: usize = 20; // Sarvam takes at most 20 files in one batch job

/// One WAV file cut out of the recording.
#[derive(Clone, Debug)]
pub struct Piece {
    pub name: String,
    pub path: PathBuf,
    /// Where this piece starts in the whole recording, in seconds.
    pub start: f64,
    pub seconds: f64,
}

/// What one batch job gave back, by file name: the output for each file that worked, the failure text for each that did not.
#[derive(Debug, Default)]
pub struct BatchResult {
    pub ok: HashMap<String, Value>,
    pub failed: HashMap<String, String>,
}

/// Sarvam's times start at 0 inside each file; add where the file starts in the whole recording.
fn shifted(out: &Value, secs: f64) -> Value {
    let mut out = out.clone();
    if secs == 0.0 {
        return out;
    }
    for key in ["start_time_seconds", "end_time_seconds"] {
        if let Some(times) = out["timestamps"][key].as_array_mut() {
            for t in times.iter_mut() {
                if let Some(x) = t.as_f64() {
                    *t = Value::from(x + secs);
                }
            }
        }
    }
    out
}

/// Cuts a piece into halves (at the quietest moment near the middle), as new files next to it. Blocking file work.
fn halve_blocking(p: &Piece) -> Result<Vec<Piece>> {
    let pcm = read_wav_pcm(&std::fs::read(&p.path)?)?;
    let mut bounds = vec![0];
    // keep only cuts that really fall inside the piece, so no half is empty
    bounds.extend(choose_cuts(&pcm, (p.seconds / 2.0).ceil() as u32).into_iter().filter(|&c| c > 0 && c < pcm.len()));
    bounds.push(pcm.len());
    if bounds.len() < 3 {
        // sending the same piece again would fail again and never stop, so give up here
        bail!("could not find a place to cut {}", p.name);
    }
    let stem = p.name.trim_end_matches(".wav");
    let dir = p.path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let mut halves = Vec::new();
    for (i, pair) in bounds.windows(2).enumerate() {
        let (from, to) = (pair[0], pair[1]);
        let name = format!("{stem}-{}.wav", char::from(b'a' + i as u8)); // -a, -b, ...
        let path = dir.join(&name);
        std::fs::write(&path, wav_bytes(&pcm[from..to]))?;
        halves.push(Piece { name, path, start: p.start + from as f64 / f64::from(RATE), seconds: (to - from) as f64 / f64::from(RATE) });
    }
    Ok(halves)
}

async fn halve(p: Piece) -> Result<Vec<Piece>> {
    tokio::task::spawn_blocking(move || halve_blocking(&p)).await?
}

/// Sends the pieces to Sarvam (at most 20 files per job) and joins what comes back. A piece Sarvam still cannot take is cut in
/// half and only that piece is sent again; any other failure stops here. Speaker labels are not asked for: each file is
/// labelled on its own, so the names would not match from piece to piece.
pub async fn transcribe_pieces<F, Fut>(pieces: Vec<Piece>, mut run_batch: F, progress: &Progress) -> Result<String>
where
    F: FnMut(Vec<Piece>) -> Fut,
    Fut: Future<Output = Result<BatchResult>>,
{
    let mut texts: Vec<(f64, String)> = Vec::new(); // (start time, text); sorted at the end so the pieces come out in order
    let mut pending = pieces;
    let mut round = 0;
    while !pending.is_empty() {
        let mut too_long: Vec<Piece> = Vec::new();
        for (n, batch) in pending.chunks(FILES_PER_JOB).enumerate() {
            let first = n * FILES_PER_JOB;
            progress(if round == 0 {
                format!("Transcribing parts {} to {} of {} with Sarvam", first + 1, first + batch.len(), pending.len())
            } else {
                format!("Cutting {} parts shorter and trying again", pending.len())
            });
            let result = run_batch(batch.to_vec()).await?;
            for f in batch {
                if let Some(out) = result.ok.get(&f.name) {
                    let text = parse_sarvam_output(&shifted(out, f.start), false);
                    if !text.is_empty() {
                        texts.push((f.start, text));
                    }
                    continue;
                }
                let raw = result.failed.get(&f.name).map(String::as_str).unwrap_or("");
                if is_sarvam_context_error(raw) && f.seconds >= 2.0 * MIN_PIECE_SECONDS {
                    too_long.push(f.clone());
                    continue;
                }
                tracing::warn!("[sarvam] piece {} failed: {raw}", f.name);
                return Err(user_err(sarvam_message(raw, None)));
            }
        }
        // the pieces cut in halves are written one after another, each on a blocking thread
        pending = Vec::new();
        for p in too_long {
            pending.extend(halve(p).await?);
        }
        round += 1;
    }
    texts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let text = texts.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n\n");
    if text.is_empty() {
        return Err(user_err("Sarvam found no speech in this recording."));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pechu_common::{friendly, no_progress, progress};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const CONTEXT_FAILURE: &str = r#"400:{"error":{"message":"Input length (4192) exceeds model's maximum context length (4096).","code":"invalid_request_error"}}"#;

    /// Pieces of real (tone) audio on disk, because cutting a piece in half reads its samples.
    fn pieces_on_disk(dir: &std::path::Path, n: usize, seconds: usize) -> Vec<Piece> {
        (0..n)
            .map(|i| {
                let name = format!("piece-{n}-{seconds}-{i}.wav");
                let path = dir.join(&name);
                std::fs::write(&path, wav_bytes(&vec![8000i16; seconds * RATE as usize])).unwrap();
                Piece { name, path, start: (i * seconds) as f64, seconds: seconds as f64 }
            })
            .collect()
    }

    /// Pieces that only exist as names, for tests that never open the files.
    fn pieces_by_name(n: usize, seconds: usize) -> Vec<Piece> {
        (0..n)
            .map(|i| Piece { name: format!("p{i}.wav"), path: PathBuf::from(format!("/nowhere/p{i}.wav")), start: (i * seconds) as f64, seconds: seconds as f64 })
            .collect()
    }

    /// What Sarvam returns for one file: its times start at 0 inside that file.
    fn output(text: &str) -> Value {
        json!({ "timestamps": { "words": [text], "start_time_seconds": [0], "end_time_seconds": [1] } })
    }

    fn all_ok(files: &[Piece]) -> BatchResult {
        BatchResult { ok: files.iter().map(|f| (f.name.clone(), output(&format!("said {}", f.name)))).collect(), failed: HashMap::new() }
    }

    /// The `[mm:ss]` stamps of a transcript, in seconds.
    fn stamps(text: &str) -> Vec<u32> {
        text.split("\n\n")
            .filter_map(|para| {
                let inner = para.strip_prefix('[')?.split_once(']')?.0;
                let (m, s) = inner.split_once(':')?;
                Some(m.parse::<u32>().ok()? * 60 + s.parse::<u32>().ok()?)
            })
            .collect()
    }

    #[tokio::test]
    async fn the_pieces_are_stitched_together_with_their_start_times_added() {
        let p = pieces_by_name(3, 120);
        let text = transcribe_pieces(p.clone(), |files| async move { Ok(all_ok(&files)) }, &no_progress()).await.unwrap();
        assert_eq!(stamps(&text), vec![0, 120, 240]);
        assert_eq!(text, "[00:00] said p0.wav\n\n[02:00] said p1.wav\n\n[04:00] said p2.wav");
    }

    #[tokio::test]
    async fn pieces_come_out_in_time_order_even_if_given_out_of_order() {
        let mut p = pieces_by_name(3, 120);
        p.reverse();
        let text = transcribe_pieces(p, |files| async move { Ok(all_ok(&files)) }, &no_progress()).await.unwrap();
        assert_eq!(stamps(&text), vec![0, 120, 240]);
    }

    #[tokio::test]
    async fn at_most_20_files_go_into_one_sarvam_job_and_progress_says_which() {
        let p = pieces_by_name(45, 10);
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let sizes_in = sizes.clone();
        let notes_in = notes.clone();
        let text = transcribe_pieces(
            p,
            move |files| {
                sizes_in.lock().unwrap().push(files.len());
                async move { Ok(all_ok(&files)) }
            },
            &progress(move |n| notes_in.lock().unwrap().push(n)),
        )
        .await
        .unwrap();
        assert_eq!(*sizes.lock().unwrap(), vec![20, 20, 5]);
        assert_eq!(stamps(&text).len(), 45);
        assert_eq!(
            *notes.lock().unwrap(),
            vec![
                "Transcribing parts 1 to 20 of 45 with Sarvam",
                "Transcribing parts 21 to 40 of 45 with Sarvam",
                "Transcribing parts 41 to 45 of 45 with Sarvam",
            ]
        );
    }

    #[tokio::test]
    async fn a_piece_sarvam_cannot_take_is_cut_in_half_and_only_that_piece_is_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let p = pieces_on_disk(dir.path(), 3, 120);
        let bad = p[1].name.clone();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let calls_in = calls.clone();
        let notes = Arc::new(Mutex::new(Vec::new()));
        let notes_in = notes.clone();
        let text = transcribe_pieces(
            p.clone(),
            move |files| {
                calls_in.lock().unwrap().push(files.iter().map(|f| f.name.clone()).collect());
                let bad = bad.clone();
                async move {
                    let mut result = all_ok(&files);
                    if let Some(out) = result.ok.remove(&bad) {
                        drop(out);
                        result.failed.insert(bad, CONTEXT_FAILURE.to_string());
                    }
                    Ok(result)
                }
            },
            &progress(move |n| notes_in.lock().unwrap().push(n)),
        )
        .await
        .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 3);
        assert!(calls[1].len() >= 2); // only the pieces cut from the failed one
        for name in &calls[1] {
            assert!(!p.iter().any(|piece| &piece.name == name), "{name} was already sent");
        }
        // the note counts the pieces cut from the failed one (how many depends on where choose_cuts cuts)
        assert_eq!(notes.lock().unwrap()[1], format!("Cutting {} parts shorter and trying again", calls[1].len()));

        let s = stamps(&text);
        assert_eq!(s.len(), 2 + calls[1].len());
        assert!(s.windows(2).all(|w| w[0] <= w[1]), "still in order: {s:?}");
        assert_eq!(s[0], 0);
        assert_eq!(s[1], 120); // the first half starts where the failed piece started
        for t in &s[2..s.len() - 1] {
            assert!(*t > 120 && *t < 240, "{t} is not inside the cut piece");
        }
        assert_eq!(*s.last().unwrap(), 240);
    }

    #[tokio::test]
    async fn a_piece_too_short_to_cut_again_gives_a_plain_error() {
        let p = pieces_by_name(1, 20);
        let err = transcribe_pieces(
            p,
            |files| async move {
                Ok(BatchResult { ok: HashMap::new(), failed: files.iter().map(|f| (f.name.clone(), CONTEXT_FAILURE.to_string())).collect() })
            },
            &no_progress(),
        )
        .await
        .unwrap_err();
        assert!(friendly(&err).contains("could not process this recording in one piece"));
    }

    #[tokio::test]
    async fn any_other_failure_stops_at_once_and_is_not_retried_by_cutting() {
        let p = pieces_by_name(2, 120);
        let calls = Arc::new(Mutex::new(0));
        let calls_in = calls.clone();
        let err = transcribe_pieces(
            p,
            move |files| {
                *calls_in.lock().unwrap() += 1;
                async move { Ok(BatchResult { ok: HashMap::new(), failed: files.iter().map(|f| (f.name.clone(), "503:upstream connect error".to_string())).collect() }) }
            },
            &no_progress(),
        )
        .await
        .unwrap_err();
        assert!(friendly(&err).contains("trouble on its side"));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_failing_batch_runner_stops_everything() {
        let err = transcribe_pieces(pieces_by_name(2, 120), |_| async { Err(user_err("The Sarvam account is out of credits.")) }, &no_progress()).await.unwrap_err();
        assert_eq!(friendly(&err), "The Sarvam account is out of credits.");
    }

    #[tokio::test]
    async fn a_piece_with_no_speech_adds_nothing_and_no_speech_at_all_is_reported() {
        let p = pieces_by_name(2, 60);
        let first = p[0].name.clone();
        let text = transcribe_pieces(
            p.clone(),
            move |files| {
                let first = first.clone();
                async move {
                    let ok = files.iter().map(|f| (f.name.clone(), if f.name == first { output("hello") } else { json!({ "transcript": "" }) })).collect();
                    Ok(BatchResult { ok, failed: HashMap::new() })
                }
            },
            &no_progress(),
        )
        .await
        .unwrap();
        assert_eq!(text, "[00:00] hello");

        let err = transcribe_pieces(
            p,
            |files| async move { Ok(BatchResult { ok: files.iter().map(|f| (f.name.clone(), json!({ "transcript": "" }))).collect(), failed: HashMap::new() }) },
            &no_progress(),
        )
        .await
        .unwrap_err();
        assert!(friendly(&err).contains("no speech"));
    }

    #[test]
    fn shifting_adds_the_start_to_every_time_and_leaves_other_text_alone() {
        let out = json!({ "transcript": "x", "timestamps": { "words": ["a"], "start_time_seconds": [1.5, "odd"], "end_time_seconds": [2.5] } });
        let s = shifted(&out, 100.0);
        assert_eq!(s["timestamps"]["start_time_seconds"], json!([101.5, "odd"]));
        assert_eq!(s["timestamps"]["end_time_seconds"], json!([102.5]));
        assert_eq!(s["transcript"], "x");
        assert_eq!(shifted(&out, 0.0), out);
    }
}

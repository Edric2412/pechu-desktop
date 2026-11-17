//! Turns any uploaded audio into clean 16 kHz mono 16-bit WAV that the speech services accept, measures it, and cuts it into
//! parts at quiet moments. Port of server/audio.ts, but decoding is done in Rust (symphonia + a resampler) instead of GStreamer.
//! CONTRACT: the signatures below are used by other crates. You may add functions; do not change these.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use pechu_common::user_err;

pub use pechu_common::RATE;

mod decode;
#[cfg(test)]
mod tests;

pub const PART_SECONDS: u32 = 10 * 60; // Gemini parts are about 10 minutes
pub const SPEAKER_PART_SECONDS: u32 = 25 * 60; // speaker labels work up to 30 minutes per request

const SEARCH_SECONDS: usize = 25; // look this far back from a part boundary for a quiet moment
const FRAME: usize = 1600; // 100 ms of samples
const DEFAULT_MAX_MINUTES: u32 = 120;

/// The longest recording Pechu transcribes: 120 minutes unless the MAX_AUDIO_MINUTES environment variable says otherwise.
pub fn max_audio_minutes() -> u32 {
    std::env::var("MAX_AUDIO_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_MINUTES)
}

/// The "recording is too long" message, shown to the person as it is.
fn too_long_error() -> anyhow::Error {
    let n = max_audio_minutes();
    user_err(format!(
        "That recording is longer than {n} minute{}, which is the most Pechu transcribes at a time. Split it into shorter recordings and upload each one.",
        if n == 1 { "" } else { "s" }
    ))
}

/// Fails with the "too long" message once `samples` (16 kHz mono) is more than the limit allows.
fn check_length(samples: u64) -> anyhow::Result<()> {
    if samples > u64::from(max_audio_minutes()) * 60 * u64::from(RATE) {
        return Err(too_long_error());
    }
    Ok(())
}

/// 44-byte header + samples, 16 kHz mono 16-bit little endian.
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // size of the fmt chunk
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes per sample frame
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Finds the PCM samples inside a WAV file by walking its chunks (extra chunks like LIST are skipped).
pub fn read_wav_pcm(file: &[u8]) -> anyhow::Result<Vec<i16>> {
    if file.len() < 12 || &file[0..4] != b"RIFF" || &file[8..12] != b"WAVE" {
        bail!("Not a WAV file");
    }
    let mut pos = 12;
    while pos + 8 <= file.len() {
        let id = &file[pos..pos + 4];
        let size = u32::from_le_bytes([file[pos + 4], file[pos + 5], file[pos + 6], file[pos + 7]])
            as usize;
        if id == b"data" {
            let start = pos + 8;
            // a file cut short claims more data than it has: use what is there
            let end = file.len().min(start.saturating_add(size));
            let whole_samples = (end - start) / 2 * 2;
            let (pairs, _) = file[start..start + whole_samples].as_chunks::<2>();
            return Ok(pairs.iter().map(|b| i16::from_le_bytes(*b)).collect());
        }
        // chunks are padded to an even length
        pos = pos
            .saturating_add(8)
            .saturating_add(size)
            .saturating_add(size & 1);
    }
    bail!("WAV file has no audio data")
}

/// Sample positions at which to cut so each part is about `part_seconds` long but never mid-speech: each cut moves to the
/// quietest 100 ms within the last 25 s before the target. Same algorithm as `chooseCuts` in audio.ts.
pub fn choose_cuts(pcm: &[i16], part_seconds: u32) -> Vec<usize> {
    let part = part_seconds as usize * RATE as usize;
    let mut cuts = Vec::new();
    if part == 0 {
        return cuts; // a part of zero length would never end
    }
    let mut from = 0;
    // a cut is only made while what is left is clearly longer than one part, so the last part is never tiny
    while (pcm.len() - from) as f64 > part as f64 * 1.25 {
        let target = from + part;
        let lo = (from + FRAME).max(target.saturating_sub(SEARCH_SECONDS * RATE as usize));
        let mut best = target; // if there is no room to look, cut exactly at the target
        let mut best_energy = u64::MAX;
        let mut s = lo;
        while s + FRAME <= target {
            let energy: u64 = pcm[s..s + FRAME]
                .iter()
                .map(|&x| (i64::from(x) * i64::from(x)) as u64)
                .sum();
            // strictly less, so the earliest of equally quiet moments wins (same as the web version)
            if energy < best_energy {
                best_energy = energy;
                best = s + FRAME / 2; // cut in the middle of the quiet frame
            }
            s += FRAME;
        }
        cuts.push(best);
        from = best;
    }
    cuts
}

/// BLOCKING (call from `spawn_blocking`). Decodes any common format (mp3, wav, flac, ogg/opus/vorbis, m4a/aac/mp4, webm/opus,
/// aiff) to 16 kHz mono 16-bit samples. Errors are `pechu_common::user_err` messages with the same wording as the web
/// version: too long -> "That recording is longer than N minutes, which is the most Pechu transcribes at a time. Split it
/// into shorter recordings and upload each one."; unreadable -> "Pechu could not read this audio file. Try exporting it as
/// MP3 or WAV, then upload it again.". Stop decoding as soon as the limit is passed (do not decode a 10-hour file).
///
/// Memory: the whole result is held in memory, 32 KB per second (a 2 hour recording is about 230 MB).
pub fn decode_to_pcm(path: &Path) -> anyhow::Result<Vec<i16>> {
    let mut pcm: Vec<i16> = Vec::new();
    decode::decode_stream(path, |samples| {
        pcm.extend_from_slice(samples);
        check_length(pcm.len() as u64)
    })?;
    Ok(pcm)
}

/// BLOCKING. Length of the recording in seconds (this is `assertWithinLimit`: it also fails when over the limit).
/// Should not need to hold the whole decoded file in memory.
pub fn audio_seconds(path: &Path) -> anyhow::Result<f64> {
    // only count: each piece of decoded audio is dropped as soon as it has been counted
    let mut total: u64 = 0;
    decode::decode_stream(path, |samples| {
        total += samples.len() as u64;
        check_length(total)
    })?;
    Ok(total as f64 / f64::from(RATE))
}

/// WAV parts in a temporary folder that is deleted when `Parts` is dropped.
pub struct Parts {
    pub files: Vec<PathBuf>,
    pub seconds: f64,
    _dir: tempfile::TempDir,
}

/// BLOCKING. Decodes, then cuts into parts of about `part_seconds` at quiet moments, writing `part-01.wav`, `part-02.wav`...
///
/// Memory: like `decode_to_pcm`, the whole recording is held in memory while it is cut.
pub fn prepare_parts(path: &Path, part_seconds: u32) -> anyhow::Result<Parts> {
    let pcm = decode_to_pcm(path)?;
    // under half a second there is nothing to transcribe (the web version refuses these too)
    if pcm.len() < RATE as usize / 2 {
        return Err(user_err(
            "That recording is too short to transcribe. Try a longer one.",
        ));
    }
    let dir = tempfile::Builder::new()
        .prefix("pechu-")
        .tempdir()
        .context("creating a temporary folder for the audio parts")?;
    let mut bounds = vec![0];
    bounds.extend(choose_cuts(&pcm, part_seconds));
    bounds.push(pcm.len());
    let mut files = Vec::new();
    for (i, pair) in bounds.windows(2).enumerate() {
        let file = dir.path().join(format!("part-{:02}.wav", i + 1));
        std::fs::write(&file, wav_bytes(&pcm[pair[0]..pair[1]]))
            .with_context(|| format!("writing {}", file.display()))?;
        files.push(file);
    }
    Ok(Parts {
        files,
        seconds: pcm.len() as f64 / f64::from(RATE),
        _dir: dir,
    })
}

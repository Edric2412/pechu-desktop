//! Reading an audio file of (almost) any format, in pure Rust: `symphonia` unpacks the container and decodes the codec,
//! `symphonia-opus` adds Opus (which `symphonia` itself cannot decode yet), and `rubato` resamples to 16 kHz.
//! Nothing here needs GStreamer, ffmpeg or any other program installed on the computer.
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use pechu_common::user_err;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::common::Tier;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia_opus::OpusDecoder;

use crate::RATE;

const UNREADABLE: &str =
    "Pechu could not read this audio file. Try exporting it as MP3 or WAV, then upload it again.";

/// The message the person sees. The technical detail goes to the log, because the person cannot act on it.
fn unreadable(detail: impl std::fmt::Display) -> anyhow::Error {
    tracing::warn!("audio file could not be read: {detail}");
    user_err(UNREADABLE)
}

/// Turns samples of any rate into 16 kHz, one chunk at a time, so a long recording never has to sit in memory twice.
/// A resampler needs a fixed number of input samples per step, and it answers a few milliseconds late, so this
/// struct hides both: `push` accepts any amount, `finish` flushes what is left.
struct To16k {
    /// `None` when the file is already 16 kHz and nothing has to be done.
    fft: Option<Fft<f32>>,
    in_rate: u32,
    /// Input that does not fill a whole step yet.
    pending: Vec<f32>,
    /// Scratch space for one step of output.
    step_out: Vec<f32>,
    /// The resampler's own delay: its first output samples are silence that must not reach the result.
    delay_left: usize,
    frames_in: u64,
    frames_out: u64,
}

impl To16k {
    fn new(in_rate: u32) -> anyhow::Result<Self> {
        let fft = if in_rate == RATE {
            None
        } else {
            // `Both` fixes the step size on both sides: simple to drive, and the fastest mode.
            let f = Fft::<f32>::new(in_rate as usize, RATE as usize, 1024, 1, FixedSync::Both)
                .map_err(|e| unreadable(format!("cannot resample from {in_rate} Hz: {e}")))?;
            Some(f)
        };
        let (step_out, delay_left) = match &fft {
            Some(f) => (vec![0.0; f.output_frames_max()], f.output_delay()),
            None => (Vec::new(), 0),
        };
        Ok(Self {
            fft,
            in_rate,
            pending: Vec::new(),
            step_out,
            delay_left,
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Adds mono samples at the file's own rate; appends whatever 16 kHz samples are ready to `out`.
    fn push(&mut self, mono: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        self.frames_in += mono.len() as u64;
        let Some(fft) = self.fft.as_mut() else {
            out.extend_from_slice(mono);
            self.frames_out += mono.len() as u64;
            return Ok(());
        };
        self.pending.extend_from_slice(mono);
        loop {
            let need = fft.input_frames_next();
            if self.pending.len() < need {
                return Ok(());
            }
            let input = InterleavedSlice::new(&self.pending[..need], 1, need)?;
            let mut output =
                InterleavedSlice::new_mut(&mut self.step_out[..], 1, fft.output_frames_next())?;
            let (used, made) = fft.process_into_buffer(&input, &mut output, None)?;
            self.pending.drain(..used);
            Self::keep(
                &self.step_out[..made],
                &mut self.delay_left,
                &mut self.frames_out,
                out,
            );
        }
    }

    /// The end of the file: the last few samples were waiting for a full step, and the resampler still holds
    /// its delay worth of audio. Feed it silence until all of the real audio has come out.
    fn finish(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        let Some(fft) = self.fft.as_mut() else {
            return Ok(());
        };
        let expected = (self.frames_in * RATE as u64).div_ceil(self.in_rate as u64);
        let mut first = true;
        while self.frames_out < expected {
            let need = fft.input_frames_next();
            // the leftover input first (padded with zeros to a full step), then pure silence
            let mut padded = std::mem::take(&mut self.pending);
            let real = if first { padded.len() } else { 0 };
            padded.resize(need, 0.0);
            first = false;
            let input = InterleavedSlice::new(&padded[..], 1, need)?;
            let mut output =
                InterleavedSlice::new_mut(&mut self.step_out[..], 1, fft.output_frames_next())?;
            let indexing = Indexing::new().partial_len(real);
            let (_, made) = fft.process_into_buffer(&input, &mut output, Some(&indexing))?;
            Self::keep(
                &self.step_out[..made],
                &mut self.delay_left,
                &mut self.frames_out,
                out,
            );
            if made == 0 {
                break; // cannot happen with these settings; this only guards against looping forever
            }
        }
        // the last step may reach past the true end of the audio: cut it off
        let extra = self.frames_out.saturating_sub(expected) as usize;
        out.truncate(out.len().saturating_sub(extra));
        self.frames_out -= extra as u64;
        Ok(())
    }

    /// Appends resampler output to `out`, first dropping the resampler's delay.
    fn keep(made: &[f32], delay_left: &mut usize, frames_out: &mut u64, out: &mut Vec<f32>) {
        let skip = (*delay_left).min(made.len());
        *delay_left -= skip;
        out.extend_from_slice(&made[skip..]);
        *frames_out += (made.len() - skip) as u64;
    }
}

/// 32-bit float samples (-1.0 to 1.0) to 16-bit integers, clipping anything louder instead of wrapping around.
fn to_i16(samples: &[f32], out: &mut Vec<i16>) {
    out.extend(
        samples
            .iter()
            .map(|s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16),
    );
}

/// Opus is registered as a fallback: if Symphonia learns Opus itself one day, its own decoder wins automatically.
fn codec_registry() -> CodecRegistry {
    let mut codecs = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut codecs);
    codecs.register_audio_decoder_at_tier::<OpusDecoder>(Tier::Fallback);
    codecs
}

/// Decodes the file and hands 16 kHz mono samples to `on_samples` piece by piece, as they come out of the decoder.
/// If `on_samples` returns an error (for example "too long"), decoding stops at once and that error is returned.
pub(crate) fn decode_stream(
    path: &Path,
    mut on_samples: impl FnMut(&[i16]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // A missing or unopenable file is a bug in the app, not something the person can fix, so it is not a user error.
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(unreadable)?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| unreadable("no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| unreadable("track has no audio parameters"))?;
    let mut params = params.clone();
    if params.codec == CODEC_ID_OPUS {
        // Opus can decode straight to 16 kHz (that is how libopus is meant to be used for speech), which is
        // faster and simpler than decoding at 48 kHz and resampling afterwards.
        params.with_sample_rate(RATE);
    }
    let mut decoder = codec_registry()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(unreadable)?;

    let mut resampler: Option<To16k> = None;
    let mut interleaved: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    let mut ready: Vec<f32> = Vec::new();
    let mut samples: Vec<i16> = Vec::new();
    let mut produced = false; // has any sound come out yet?

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A recording that was cut off (the app closed, the battery died) ends in the middle of a packet.
            // Keep what was read: losing a whole meeting for a damaged last second would be worse.
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(e) if produced => {
                tracing::warn!("audio file is damaged, using the part that could be read: {e}");
                break;
            }
            Err(e) => return Err(unreadable(e)),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = match decoder.decode(&packet) {
            Ok(b) => b,
            // one damaged packet is a click in the audio, not a reason to give up on the whole file
            Err(SymphoniaError::DecodeError(msg)) => {
                tracing::warn!("skipped a damaged audio packet: {msg}");
                continue;
            }
            Err(e) if produced => {
                tracing::warn!("decoding stopped early, using what was decoded: {e}");
                break;
            }
            Err(e) => return Err(unreadable(e)),
        };
        let channels = buffer.spec().channels().count();
        let rate = buffer.spec().rate();
        if channels == 0 || buffer.frames() == 0 {
            continue;
        }

        // all channels into one: speech services want mono, and averaging keeps a voice that sits in only one channel
        buffer.copy_to_vec_interleaved(&mut interleaved);
        mono.clear();
        mono.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32),
        );

        // the rate is read from the decoded audio itself, because some files (HE-AAC) state a different one in the header
        if resampler.as_ref().is_none_or(|r| r.in_rate != rate) {
            if let Some(mut old) = resampler.take() {
                ready.clear();
                old.finish(&mut ready)?;
                samples.clear();
                to_i16(&ready, &mut samples);
                on_samples(&samples)?;
            }
            resampler = Some(To16k::new(rate)?);
        }
        let Some(r) = resampler.as_mut() else {
            continue;
        };
        ready.clear();
        r.push(&mono, &mut ready)?;
        if !ready.is_empty() {
            produced = true;
            samples.clear();
            to_i16(&ready, &mut samples);
            on_samples(&samples)?;
        }
    }

    if let Some(mut r) = resampler {
        ready.clear();
        r.finish(&mut ready)?;
        samples.clear();
        to_i16(&ready, &mut samples);
        if !samples.is_empty() {
            produced = true;
            on_samples(&samples)?;
        }
    }
    if !produced {
        return Err(unreadable("the file contains no audio"));
    }
    Ok(())
}

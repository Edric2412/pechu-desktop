//! Sound-format maths for the Windows backend, kept free of any Windows code so it can be tested on every system.
//!
//! Windows hands us whatever format the speakers use (usually 32-bit float, any number of channels). The page wants
//! interleaved little-endian signed 16-bit stereo in chunks of 20 ms. Three small jobs get us there:
//!   1. `to_stereo_s16`: decode the samples, mix the channels down to two, convert to 16-bit.
//!   2. `Chunker`: cut the stream into whole 20 ms chunks.
//!   3. `silence_needed`: Windows sends nothing while nothing is playing, so work out how much silence to fill in.
//!
//! Only the Windows backend uses this outside of tests, so on other systems the compiler would call it unused.
#![cfg_attr(not(windows), allow(dead_code))]
use std::time::Duration;

/// How one sample is stored in the bytes Windows gives us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    /// 24-bit samples packed in 3 bytes each.
    I24,
    /// 32-bit integers (a 24-bit sound card often stores its samples this way).
    I32,
}

impl SampleKind {
    /// Picks the kind from what the device says about its format; `None` for anything we do not know how to read.
    pub fn from_format(is_float: bool, bits_per_sample: u16) -> Option<SampleKind> {
        match (is_float, bits_per_sample) {
            (true, 32) => Some(SampleKind::F32),
            (false, 16) => Some(SampleKind::I16),
            (false, 24) => Some(SampleKind::I24),
            (false, 32) => Some(SampleKind::I32),
            _ => None,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            SampleKind::I16 => 2,
            SampleKind::I24 => 3,
            SampleKind::F32 | SampleKind::I32 => 4,
        }
    }

    /// One sample as a number between -1.0 and 1.0. `b` holds exactly `self.bytes()` bytes.
    fn decode(self, b: &[u8]) -> f32 {
        match self {
            SampleKind::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            SampleKind::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0,
            // put the 3 bytes at the top of an i32 so the sign comes out right, then scale like a 32-bit sample
            SampleKind::I24 => i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / 2_147_483_648.0,
            SampleKind::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }
}

/// Turns a number between -1.0 and 1.0 into 16-bit; louder than full scale is cut off instead of wrapping around into a loud click.
fn to_s16(x: f32) -> i16 {
    (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

/// Decodes `input` (whole frames of `channels` samples each; a cut-off last frame is ignored), mixes it down to two
/// channels and appends interleaved left/right 16-bit samples to `out`.
///
/// Mixing follows the usual Windows channel order: front left, front right, centre, bass (LFE), then rear and side
/// speakers alternating left/right. Centre goes to both sides at 70 %, rear and side speakers to their own side at 70 %.
/// The bass channel is left out, as ordinary stereo downmixes do.
pub fn to_stereo_s16(input: &[u8], kind: SampleKind, channels: usize, out: &mut Vec<i16>) {
    if channels == 0 {
        return;
    }
    const SIDE: f32 = 0.707;
    let width = kind.bytes();
    let mut frame = vec![0f32; channels];
    for bytes in input.chunks_exact(width * channels) {
        for (value, sample) in frame.iter_mut().zip(bytes.chunks_exact(width)) {
            *value = kind.decode(sample);
        }
        let (left, right) = match channels {
            1 => (frame[0], frame[0]),
            2 => (frame[0], frame[1]),
            _ => {
                let (mut l, mut r) = (frame[0], frame[1]);
                l += frame[2] * SIDE;
                r += frame[2] * SIDE;
                // index 3 is the bass channel: skipped on purpose
                for (i, value) in frame.iter().enumerate().skip(4) {
                    if i % 2 == 0 {
                        l += value * SIDE;
                    } else {
                        r += value * SIDE;
                    }
                }
                (l, r)
            }
        };
        out.push(to_s16(left));
        out.push(to_s16(right));
    }
}

/// Collects stereo samples and hands them out in whole 20 ms chunks as little-endian bytes.
pub struct Chunker {
    /// Samples (not frames) per chunk: 20 ms of both channels.
    chunk_samples: usize,
    pending: Vec<i16>,
    /// Frames pushed so far, counting silence, so the caller can compare it with the clock.
    frames_in: u64,
}

impl Chunker {
    pub fn new(sample_rate: u32) -> Self {
        Chunker { chunk_samples: (sample_rate / 50) as usize * 2, pending: Vec::new(), frames_in: 0 }
    }

    /// Adds interleaved stereo samples (an even count).
    pub fn push(&mut self, stereo: &[i16]) {
        self.pending.extend_from_slice(stereo);
        self.frames_in += (stereo.len() / 2) as u64;
    }

    pub fn push_silence(&mut self, frames: usize) {
        self.pending.resize(self.pending.len() + frames * 2, 0);
        self.frames_in += frames as u64;
    }

    pub fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// The next full chunk, or `None` while less than 20 ms is waiting.
    pub fn pop_chunk(&mut self) -> Option<Vec<u8>> {
        if self.pending.len() < self.chunk_samples {
            return None;
        }
        let bytes = self.pending.drain(..self.chunk_samples).flat_map(i16::to_le_bytes).collect();
        Some(bytes)
    }
}

/// Windows delivers no sound at all while nothing is playing. If we only passed on what arrives, a quiet minute would
/// vanish from the recording and everything after it would be out of step with the microphone. So: how many frames of
/// silence are missing compared with the clock? Small gaps are normal (packets arrive in bursts), so only a lag of more
/// than 100 ms counts, and then the whole gap is filled.
pub fn silence_needed(elapsed: Duration, sample_rate: u32, frames_in: u64) -> usize {
    let expected = (elapsed.as_secs_f64() * sample_rate as f64) as u64;
    let lag = expected.saturating_sub(frames_in);
    if lag > (sample_rate / 10) as u64 {
        lag as usize
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn convert(input: &[u8], kind: SampleKind, channels: usize) -> Vec<i16> {
        let mut out = Vec::new();
        to_stereo_s16(input, kind, channels, &mut out);
        out
    }

    #[test]
    fn float_stereo_scales_to_16_bit() {
        let out = convert(&f32_bytes(&[0.5, -0.5, 1.0, -1.0]), SampleKind::F32, 2);
        assert_eq!(out, vec![16_384, -16_384, 32_767, -32_768]);
    }

    #[test]
    fn too_loud_is_cut_off_not_wrapped() {
        let out = convert(&f32_bytes(&[3.0, -3.0]), SampleKind::F32, 2);
        assert_eq!(out, vec![32_767, -32_768]);
    }

    #[test]
    fn i16_stereo_passes_through_unchanged() {
        let bytes: Vec<u8> = [100i16, -200, 300, -400].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(convert(&bytes, SampleKind::I16, 2), vec![100, -200, 300, -400]);
    }

    #[test]
    fn i24_and_i32_are_scaled_to_16_bit() {
        // 24-bit: 0x400000 is half of full scale, 0xC00000 is minus half
        let out = convert(&[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], SampleKind::I24, 2);
        assert_eq!(out, vec![16_384, -16_384]);
        let bytes: Vec<u8> = [0x4000_0000i32, i32::MIN].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(convert(&bytes, SampleKind::I32, 2), vec![16_384, -32_768]);
    }

    #[test]
    fn mono_is_copied_to_both_sides() {
        let out = convert(&f32_bytes(&[0.25, -0.25]), SampleKind::F32, 1);
        assert_eq!(out, vec![8_192, 8_192, -8_192, -8_192]);
    }

    #[test]
    fn surround_5_1_is_mixed_down_without_the_bass_channel() {
        // front left, front right, centre, bass, rear left, rear right
        let frame = [0.1, 0.2, 0.2, 0.9, 0.1, 0.0];
        let out = convert(&f32_bytes(&frame), SampleKind::F32, 6);
        let left = 0.1 + 0.2 * 0.707 + 0.1 * 0.707;
        let right = 0.2 + 0.2 * 0.707;
        assert_eq!(out, vec![to_s16(left), to_s16(right)]);
    }

    #[test]
    fn a_cut_off_last_frame_is_ignored() {
        let mut bytes = f32_bytes(&[0.5, 0.5]);
        bytes.extend_from_slice(&[1, 2, 3]); // half a sample of a second frame
        assert_eq!(convert(&bytes, SampleKind::F32, 2).len(), 2);
    }

    #[test]
    fn only_known_formats_are_accepted() {
        assert_eq!(SampleKind::from_format(true, 32), Some(SampleKind::F32));
        assert_eq!(SampleKind::from_format(false, 16), Some(SampleKind::I16));
        assert_eq!(SampleKind::from_format(false, 24), Some(SampleKind::I24));
        assert_eq!(SampleKind::from_format(false, 32), Some(SampleKind::I32));
        assert_eq!(SampleKind::from_format(true, 64), None);
        assert_eq!(SampleKind::from_format(false, 8), None);
    }

    #[test]
    fn chunks_are_exactly_20_ms_and_keep_the_order() {
        let mut chunker = Chunker::new(48_000); // 960 frames = 1920 samples = 3840 bytes
        let samples: Vec<i16> = (0..2_000).map(|i| i as i16).collect(); // a bit more than one chunk
        chunker.push(&samples);
        let first = chunker.pop_chunk().expect("one full chunk is waiting");
        assert_eq!(first.len(), 3_840);
        assert_eq!(&first[..4], &[0, 0, 1, 0]); // samples 0 and 1, little-endian
        assert!(chunker.pop_chunk().is_none(), "80 samples are not a whole chunk");
        // the rest is kept, and the next chunk carries on where the first stopped
        chunker.push(&vec![7i16; 1_840]);
        let second = chunker.pop_chunk().unwrap();
        assert_eq!(&second[..2], &(1_920i16).to_le_bytes());
        assert_eq!(chunker.frames_in(), 1_000 + 920);
    }

    #[test]
    fn silence_fills_a_gap_only_when_it_is_large() {
        // 44.1 kHz: 100 ms is 4410 frames
        assert_eq!(silence_needed(Duration::from_millis(50), 44_100, 0), 0, "50 ms late is normal");
        assert_eq!(silence_needed(Duration::from_secs(1), 44_100, 44_100), 0, "on time");
        assert_eq!(silence_needed(Duration::from_secs(1), 44_100, 0), 44_100, "a whole second of nothing");
        assert_eq!(silence_needed(Duration::from_secs(1), 44_100, 50_000), 0, "ahead of the clock");
    }

    #[test]
    fn padding_with_silence_counts_as_frames() {
        let mut chunker = Chunker::new(48_000);
        chunker.push_silence(960);
        assert_eq!(chunker.frames_in(), 960);
        assert_eq!(chunker.pop_chunk().unwrap(), vec![0u8; 3_840]);
    }
}

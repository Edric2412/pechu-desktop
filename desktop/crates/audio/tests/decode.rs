//! Decodes the small real files in tests/fixtures (made by make-fixtures.sh) and checks that what comes out is
//! 16 kHz mono audio of the right length that still contains the tone and the gap. Needs no decoder program installed.
use std::path::{Path, PathBuf};

use pechu_audio::{audio_seconds, decode_to_pcm, prepare_parts, wav_bytes, PART_SECONDS, RATE};

const UNREADABLE: &str =
    "Pechu could not read this audio file. Try exporting it as MP3 or WAV, then upload it again.";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Samples between two moments (in seconds) of the decoded audio.
fn window(pcm: &[i16], from: f64, to: f64) -> &[i16] {
    let rate = RATE as f64;
    &pcm[(from * rate) as usize..(to * rate) as usize]
}

fn rms(samples: &[i16]) -> f64 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// Times the signal goes from below zero to above zero: a pure tone crosses once per cycle.
fn rising_crossings(samples: &[i16]) -> usize {
    samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count()
}

/// Every fixture is 2 s: 440 Hz tone (amplitude 12000), silence from 0.8 s to 1.2 s, tone again.
fn check_tone_and_gap(name: &str) {
    let pcm = decode_to_pcm(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    let seconds = pcm.len() as f64 / RATE as f64;
    // lossy formats add or drop a few ms at the start and end (encoder delay), so allow 0.1 s either way
    assert!(
        (seconds - 2.0).abs() < 0.1,
        "{name}: expected about 2 s, got {seconds} s ({} samples)",
        pcm.len()
    );

    let tone = window(&pcm, 0.2, 0.6);
    let level = rms(tone);
    // a sine of amplitude 12000 has an RMS of 8485
    assert!(
        (6500.0..10500.0).contains(&level),
        "{name}: tone level {level}"
    );
    // 0.4 s of 440 Hz is 176 cycles
    let cycles = rising_crossings(tone);
    assert!(
        (165..=187).contains(&cycles),
        "{name}: {cycles} cycles in 0.4 s, expected about 176"
    );
    let gap = rms(window(&pcm, 0.9, 1.1));
    assert!(gap < 800.0, "{name}: the gap is not quiet (level {gap})");
    let after = rms(window(&pcm, 1.4, 1.8));
    assert!(
        (6500.0..10500.0).contains(&after),
        "{name}: level after the gap {after}"
    );

    let counted = audio_seconds(&fixture(name)).unwrap();
    assert!(
        (counted - seconds).abs() < 1e-9,
        "{name}: audio_seconds {counted} differs from decoded length {seconds}"
    );
}

#[test]
fn wav_at_8_khz_is_upsampled_to_16_khz() {
    check_tone_and_gap("tone-8k-mono.wav");
}

#[test]
fn wav_at_16_khz_passes_through() {
    check_tone_and_gap("tone-16k-mono.wav");
    let pcm = decode_to_pcm(&fixture("tone-16k-mono.wav")).unwrap();
    assert_eq!(pcm.len(), 2 * RATE as usize);
}

/// A 16-bit mono WAV at any sample rate (`wav_bytes` can only write 16 kHz).
fn wav_at(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

#[test]
fn a_long_recording_is_resampled_without_glitches_between_chunks() {
    // 30 s is many resampler steps, so every join between two steps is exercised
    for rate in [44_100u32, 48_000, 22_050] {
        let seconds = 30;
        let tone: Vec<i16> = (0..rate as usize * seconds)
            .map(|i| {
                (12000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(rate)).sin())
                    as i16
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.wav");
        std::fs::write(&path, wav_at(rate, &tone)).unwrap();

        let pcm = decode_to_pcm(&path).unwrap();
        assert_eq!(
            pcm.len(),
            seconds * RATE as usize,
            "{rate} Hz: the length must be exact"
        );
        // every second must look the same: same level, same pitch (a glitch at a join would break one of them)
        for second in 1..seconds - 1 {
            let part = window(&pcm, second as f64, second as f64 + 1.0);
            let level = rms(part);
            assert!(
                (8300.0..8700.0).contains(&level),
                "{rate} Hz, second {second}: level {level}"
            );
            let cycles = rising_crossings(part);
            assert!(
                (439..=441).contains(&cycles),
                "{rate} Hz, second {second}: {cycles} cycles"
            );
            let biggest_step = part
                .windows(2)
                .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
                .max()
                .unwrap();
            assert!(
                biggest_step < 2250,
                "{rate} Hz, second {second}: a jump of {biggest_step} between neighbours"
            );
        }
    }
}

#[test]
fn aiff() {
    check_tone_and_gap("tone-8k-mono.aiff");
}

#[test]
fn flac_stereo_44_khz() {
    check_tone_and_gap("tone-44k-stereo.flac");
}

#[test]
fn mp3_stereo_44_khz() {
    check_tone_and_gap("tone-44k-stereo.mp3");
}

#[test]
fn ogg_vorbis() {
    check_tone_and_gap("tone-44k-stereo.ogg");
}

#[test]
fn m4a_aac() {
    check_tone_and_gap("tone-44k-stereo.m4a");
}

#[test]
fn webm_opus() {
    check_tone_and_gap("tone-48k-mono.webm");
}

#[test]
fn webm_opus_written_live_like_the_browser_recorder() {
    check_tone_and_gap("tone-48k-mono-live.webm");
}

#[test]
fn ogg_opus() {
    check_tone_and_gap("tone-48k-mono.opus");
}

#[test]
fn the_extension_does_not_matter() {
    // a browser upload may arrive with the wrong name: the content decides
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("audio.bin");
    std::fs::copy(fixture("tone-48k-mono.webm"), &copy).unwrap();
    let pcm = decode_to_pcm(&copy).unwrap();
    assert!((pcm.len() as f64 / RATE as f64 - 2.0).abs() < 0.1);
}

#[test]
fn a_recording_that_was_cut_off_keeps_the_part_that_exists() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "tone-48k-mono-live.webm",
        "tone-44k-stereo.mp3",
        "tone-44k-stereo.flac",
    ] {
        let whole = std::fs::read(fixture(name)).unwrap();
        let cut = dir.path().join(format!("cut-{name}"));
        std::fs::write(&cut, &whole[..whole.len() * 6 / 10]).unwrap();
        let pcm = decode_to_pcm(&cut).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let seconds = pcm.len() as f64 / RATE as f64;
        assert!(
            (0.5..1.9).contains(&seconds),
            "{name}: kept {seconds} s of the 60% that exists"
        );
    }
}

#[test]
fn prepare_parts_works_on_a_compressed_file() {
    let parts = prepare_parts(&fixture("tone-48k-mono.webm"), PART_SECONDS).unwrap();
    assert_eq!(parts.files.len(), 1);
    assert!((parts.seconds - 2.0).abs() < 0.1);
    let wav = std::fs::read(&parts.files[0]).unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(wav.len(), 44 + (parts.seconds * RATE as f64) as usize * 2);
}

#[test]
fn files_that_are_not_audio_give_the_readable_message() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("notes.mp3");
    std::fs::write(&text, "this is a text file, not an mp3 ".repeat(200)).unwrap();
    let empty = dir.path().join("empty.wav");
    std::fs::write(&empty, b"").unwrap();
    // a valid WAV header with no samples after it
    let silent = dir.path().join("none.wav");
    std::fs::write(&silent, wav_bytes(&[])).unwrap();
    for path in [text, empty, silent] {
        let e = decode_to_pcm(&path).unwrap_err();
        assert_eq!(pechu_common::friendly(&e), UNREADABLE, "{}", path.display());
        assert_eq!(
            pechu_common::friendly(&audio_seconds(&path).unwrap_err()),
            UNREADABLE
        );
    }
}

#[test]
fn a_missing_file_is_an_error_but_not_one_for_the_screen() {
    let e = decode_to_pcm(Path::new("/definitely/not/here.mp3")).unwrap_err();
    assert_ne!(pechu_common::friendly(&e), UNREADABLE);
}

/// Manual check with a real recording: `PECHU_TEST_AUDIO=/path/to/audio.webm cargo test -p pechu-audio -- --ignored --nocapture`
#[test]
#[ignore]
fn a_real_recording() {
    let path = std::env::var("PECHU_TEST_AUDIO").expect("set PECHU_TEST_AUDIO to an audio file");
    let started = std::time::Instant::now();
    let seconds = audio_seconds(Path::new(&path)).unwrap();
    println!(
        "{path}: {seconds:.1} s of audio, decoded in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    assert!(seconds > 0.0);
}

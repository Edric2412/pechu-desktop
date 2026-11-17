//! Tests that need no audio files: the WAV helpers, where parts are cut, and the length limit.
//! (tests/decode.rs decodes real files in every format.)
use super::*;
use std::sync::Mutex;

// Tests that change MAX_AUDIO_MINUTES must not run at the same time (the environment is shared by all threads),
// so each of them holds this lock while it runs.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Runs `run` with MAX_AUDIO_MINUTES set to `minutes` (or removed for `None`), then puts the old value back.
fn with_limit(minutes: Option<&str>, run: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()); // a failed test must not block the others
    let before = std::env::var("MAX_AUDIO_MINUTES").ok();
    match minutes {
        Some(m) => std::env::set_var("MAX_AUDIO_MINUTES", m),
        None => std::env::remove_var("MAX_AUDIO_MINUTES"),
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    match before {
        Some(b) => std::env::set_var("MAX_AUDIO_MINUTES", b),
        None => std::env::remove_var("MAX_AUDIO_MINUTES"),
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A tone-filled signal with short silences (0.3 s) at the given seconds.
fn signal(seconds: usize, quiet_at: &[f64]) -> Vec<i16> {
    let mut pcm = vec![8000i16; seconds * RATE as usize];
    for &q in quiet_at {
        let start = (q * RATE as f64) as usize;
        let end = ((q + 0.3) * RATE as f64) as usize;
        pcm[start..end].fill(0);
    }
    pcm
}

fn write_wav(dir: &Path, name: &str, pcm: &[i16]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, wav_bytes(pcm)).unwrap();
    path
}

/// What the person would read for this error.
fn message(e: &anyhow::Error) -> String {
    pechu_common::friendly(e)
}

#[test]
fn a_wav_written_by_wav_bytes_reads_back_to_the_same_samples() {
    let pcm = [0, 1, -1, 32767, -32768, 1234];
    assert_eq!(read_wav_pcm(&wav_bytes(&pcm)).unwrap(), pcm);
    assert_eq!(wav_bytes(&pcm).len(), 44 + pcm.len() * 2);
}

#[test]
fn read_wav_pcm_skips_extra_chunks_before_the_data_and_rejects_non_wav_input() {
    let plain = wav_bytes(&[5, 6, 7]);
    let mut extra = plain[..36].to_vec();
    extra.extend_from_slice(b"LIST\x04\x00\x00\x00abcd");
    extra.extend_from_slice(&plain[36..]);
    assert_eq!(read_wav_pcm(&extra).unwrap(), [5, 6, 7]);
    assert!(read_wav_pcm(b"not a wav file at all").is_err());
    assert!(read_wav_pcm(b"").is_err());
    // a RIFF/WAVE header with no data chunk
    assert!(read_wav_pcm(&plain[..36]).is_err());
}

#[test]
fn short_audio_is_not_split() {
    assert!(choose_cuts(&signal(5 * 60, &[]), PART_SECONDS).is_empty());
    // under 1.25 parts: keep it whole
    assert!(choose_cuts(&signal(PART_SECONDS as usize + 60, &[]), PART_SECONDS).is_empty());
}

#[test]
fn long_audio_is_cut_at_the_quiet_moment_before_each_boundary_not_mid_speech() {
    let part = PART_SECONDS as f64;
    let quiet = part - 7.0; // silence 7 s before the first target
    let cuts = choose_cuts(&signal(25 * 60, &[quiet, 2.0 * part - 12.0]), PART_SECONDS);
    assert_eq!(cuts.len(), 2);
    let first = cuts[0] as f64 / RATE as f64;
    assert!(
        (first - (quiet + 0.15)).abs() < 0.2,
        "first cut at {first} s"
    );
    assert!(cuts[1] as f64 / RATE as f64 > first + part - 30.0);
}

#[test]
fn with_no_quiet_moment_the_cut_lands_on_the_boundary_itself() {
    let cuts = choose_cuts(&signal(25 * 60, &[]), PART_SECONDS);
    let first = cuts[0] as f64 / RATE as f64;
    assert!(
        first > PART_SECONDS as f64 - 26.0 && first <= PART_SECONDS as f64,
        "first cut at {first} s"
    );
}

#[test]
fn a_part_length_of_zero_does_not_loop_forever() {
    assert!(choose_cuts(&signal(10, &[]), 0).is_empty());
}

#[test]
fn the_default_longest_recording_is_2_hours() {
    with_limit(None, || assert_eq!(max_audio_minutes(), 120));
}

#[test]
fn a_bad_limit_setting_falls_back_to_2_hours() {
    with_limit(Some("abc"), || assert_eq!(max_audio_minutes(), 120));
    with_limit(Some("0"), || assert_eq!(max_audio_minutes(), 120));
    with_limit(Some("30"), || assert_eq!(max_audio_minutes(), 30));
}

#[test]
fn a_recording_over_the_limit_is_refused_with_a_clear_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(dir.path(), "long.wav", &signal(75, &[]));
    with_limit(Some("1"), || {
        let e = audio_seconds(&path).unwrap_err();
        assert_eq!(
            message(&e),
            "That recording is longer than 1 minute, which is the most Pechu transcribes at a time. Split it into shorter recordings and upload each one."
        );
        assert!(message(&decode_to_pcm(&path).unwrap_err()).contains("longer than 1 minute"));
        assert!(message(&prepare_parts(&path, PART_SECONDS).err().unwrap())
            .contains("Split it into shorter recordings"));
    });
    // 75 s is fine for a 2-minute limit
    with_limit(Some("2"), || assert!(audio_seconds(&path).is_ok()));
}

#[test]
fn the_message_uses_the_plural_for_other_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(dir.path(), "long.wav", &signal(130, &[]));
    with_limit(Some("2"), || {
        assert!(message(&audio_seconds(&path).unwrap_err()).contains("longer than 2 minutes,"));
    });
}

#[test]
fn a_recording_within_the_limit_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(dir.path(), "ok.wav", &signal(30, &[]));
    with_limit(Some("1"), || {
        assert_eq!(audio_seconds(&path).unwrap(), 30.0);
        let parts = prepare_parts(&path, PART_SECONDS).unwrap();
        assert_eq!(parts.files.len(), 1);
        assert_eq!(parts.seconds, 30.0);
    });
}

#[test]
fn prepare_parts_cuts_at_the_quiet_moment_and_loses_no_audio() {
    // decoding reads the limit setting, so it must not overlap with the tests that change it
    with_limit(None, || {
        let dir = tempfile::tempdir().unwrap();
        // 70 s with a silence at 33 s; parts of about 40 s, so the quiet moment is within the 25 s before the target
        let original = signal(70, &[33.0]);
        let path = write_wav(dir.path(), "meeting.wav", &original);
        let parts = prepare_parts(&path, 40).unwrap();
        assert_eq!(parts.files.len(), 2);
        assert_eq!(parts.seconds, 70.0);
        let names: Vec<_> = parts
            .files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["part-01.wav", "part-02.wav"]);
        let joined: Vec<i16> = parts
            .files
            .iter()
            .flat_map(|f| read_wav_pcm(&std::fs::read(f).unwrap()).unwrap())
            .collect();
        assert_eq!(
            joined, original,
            "16 kHz mono input must come out sample for sample the same"
        );
        let first_len = read_wav_pcm(&std::fs::read(&parts.files[0]).unwrap())
            .unwrap()
            .len();
        assert!(
            (first_len as f64 / RATE as f64 - 33.15).abs() < 0.2,
            "first part is {first_len} samples"
        );
        // the temporary folder goes away with `Parts`
        let folder = parts.files[0].parent().unwrap().to_path_buf();
        drop(parts);
        assert!(!folder.exists());
    });
}

#[test]
fn a_recording_under_half_a_second_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(dir.path(), "tiny.wav", &[1; 4000]);
    with_limit(None, || {
        assert!(message(&prepare_parts(&path, PART_SECONDS).err().unwrap()).contains("too short"))
    });
}

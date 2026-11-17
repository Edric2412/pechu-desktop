//! Manual check on a real Linux desktop: captures the computer's output with the real `pw-record` while a ONE-SECOND, QUIET
//! tone is played with `pw-cat`, and asserts that the captured sound is not silent. Run it once:
//!   cargo run -p pechu-capture --example tone_check
//! (It plays a short beep through the speakers, at 50% stream volume, on top of whatever the system volume is.)
use pechu_capture::default_backend;
use std::time::{Duration, Instant};

/// A mono 48 kHz, 16-bit WAV file of a 440 Hz tone, 1 second, at 30% of full scale.
fn tone_wav() -> Vec<u8> {
    let (rate, seconds) = (48_000u32, 1u32);
    let samples: Vec<i16> = (0..rate * seconds)
        .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.3 * 32767.0) as i16)
        .collect();
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // size of the fmt block
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// The loudest sample in a chunk of interleaved little-endian 16-bit sound.
fn peak(chunk: &[u8]) -> i32 {
    chunk.as_chunks::<2>().0.iter().map(|b| (i16::from_le_bytes(*b) as i32).abs()).max().unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let backend = default_backend();
    backend.available().expect("system audio should be available on this machine");

    let wav_path = std::env::temp_dir().join(format!("pechu-tone-{}.wav", std::process::id()));
    std::fs::write(&wav_path, tone_wav()).unwrap();

    let mut stream = backend.start().expect("capture starts");
    println!("capturing: {:?}", stream.format);
    let (mut quiet_peak, mut tone_peak, mut chunks) = (0, 0, 0);

    // 0.4 s of capture before the tone (should be near silence), then play the tone once and keep listening for 1.6 s more
    let began = Instant::now();
    let mut player = None;
    while began.elapsed() < Duration::from_millis(2000) {
        if player.is_none() && began.elapsed() >= Duration::from_millis(400) {
            player = Some(std::process::Command::new("pw-cat").args(["--playback", "--volume", "0.5"]).arg(&wav_path).spawn().expect("pw-cat starts"));
            println!("playing the tone (1 s, quiet)");
        }
        let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(500), stream.next_chunk()).await else { break };
        chunks += 1;
        if player.is_none() {
            quiet_peak = quiet_peak.max(peak(&chunk));
        } else {
            tone_peak = tone_peak.max(peak(&chunk));
        }
    }
    if let Some(mut p) = player {
        let _ = p.wait();
    }
    drop(stream);
    let _ = std::fs::remove_file(&wav_path);

    println!("{chunks} chunks; loudest sample before the tone: {quiet_peak}; during/after the tone: {tone_peak} (of 32767)");
    assert!(chunks > 50, "about 100 chunks of 20 ms were expected in 2 s");
    assert!(tone_peak > 50, "the captured sound is silent: the tone was not heard");
    assert!(tone_peak > quiet_peak * 4, "the tone should stand out from what was captured before it");
    println!("OK: the captured sound is not silent");
}

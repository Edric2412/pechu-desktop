//! Manual check against the REAL Sarvam live-captions service (spends a little: it bills by the audio it receives).
//!   SARVAM_API_KEY=... cargo run -p pechu-live --example real_check -- path/to/short-clip.wav
//! The clip must be a 16 kHz mono 16-bit WAV file (a 44-byte header, then samples). It is sent in 100 ms pieces at the speed
//! of real time, then `end()` is called. Prints only the shape of what comes back, not what was said. Never prints the key.
use pechu_live::{start, LiveConfig, LiveEvent, LiveMode};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let key = std::env::var("SARVAM_API_KEY").expect("set SARVAM_API_KEY");
    let path = std::env::args().nth(1).expect("give the path of a 16 kHz mono WAV file");
    let wav = std::fs::read(&path).expect("the file can be read");
    let pcm = &wav[44..]; // skip the WAV header
    println!("clip: {} bytes = {:.1} s of audio", pcm.len(), pcm.len() as f32 / 32_000.0);

    // Only a Sarvam key: if Sarvam fails there is no Gemini to take over, so nothing else is ever billed.
    let (handle, mut events) = start(LiveConfig {
        mode: LiveMode::Smart,
        langs: vec![],
        sarvam_key: Some(key.clone()),
        gemini_key: None,
        sarvam_url: None,
        gemini_url: None,
    });

    let began = Instant::now();
    let sender = {
        let handle = handle.clone();
        let pcm = pcm.to_vec();
        tokio::spawn(async move {
            let mut sent = 0usize;
            for piece in pcm.chunks(3200) {
                // 3200 bytes = 100 ms
                handle.send_pcm(piece.to_vec());
                sent += piece.len();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            handle.end();
            sent
        })
    };

    let (mut ready, mut interim, mut finals, mut final_chars, mut closed) = (false, 0, 0, 0, false);
    let deadline = Duration::from_secs(40);
    while began.elapsed() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), events.recv()).await else { break };
        match event {
            LiveEvent::Ready { provider } => {
                ready = true;
                println!("[{:>4.1}s] ready ({provider})", began.elapsed().as_secs_f32());
            }
            LiveEvent::Interim(_) => interim += 1,
            LiveEvent::Final(text) => {
                finals += 1;
                final_chars += text.chars().count();
                println!("[{:>4.1}s] final #{finals}: {} characters", began.elapsed().as_secs_f32(), text.chars().count());
            }
            LiveEvent::Notice(text) => println!("[{:>4.1}s] notice: {}", began.elapsed().as_secs_f32(), text.replace(&key, "[key]")),
            LiveEvent::Error(text) => println!("[{:>4.1}s] ERROR: {}", began.elapsed().as_secs_f32(), text.replace(&key, "[key]")),
            LiveEvent::Closed => {
                closed = true;
                println!("[{:>4.1}s] closed", began.elapsed().as_secs_f32());
                break;
            }
        }
    }
    let sent = sender.await.unwrap_or(0);
    println!("summary: ready={ready} interim={interim} finals={finals} ({final_chars} chars) closed={closed}; audio sent {sent} bytes");
}

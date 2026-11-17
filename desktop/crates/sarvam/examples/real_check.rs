//! Manual check against the REAL Sarvam service (this spends a little: Sarvam bills by audio length, about ₹0.5 for 5 seconds).
//!   SARVAM_API_KEY=... cargo run -p pechu-sarvam --example real_check -- path/to/short-clip.wav
//! Prints only the SHAPE of the answer (length, time stamps, number of paragraphs), not what was said. Never prints the key.
use pechu_sarvam::{transcribe_with_sarvam, SarvamEndpoint, SarvamMode, SarvamOpts};
use std::path::PathBuf;
use std::time::Instant;

#[tokio::main]
async fn main() {
    let key = std::env::var("SARVAM_API_KEY").expect("set SARVAM_API_KEY");
    let path: PathBuf = std::env::args().nth(1).expect("give the path of a short audio file").into();
    let progress = pechu_common::progress(|note| println!("  progress: {note}"));
    let started = Instant::now();
    let opts = SarvamOpts { mode: SarvamMode::Codemix, speakers: false };
    match transcribe_with_sarvam(&key, &path, opts, progress, SarvamEndpoint::default()).await {
        Ok(text) => {
            let paragraphs: Vec<&str> = text.split("\n\n").collect();
            let stamped = paragraphs.iter().filter(|p| p.starts_with('[') && p.get(1..2).is_some_and(|c| c.chars().all(|c| c.is_ascii_digit()))).count();
            println!("sarvam batch: ok in {:.1} s", started.elapsed().as_secs_f32());
            println!("  {} characters, {} paragraph(s), {} start with a [mm:ss] stamp", text.chars().count(), paragraphs.len(), stamped);
            println!("  contains Tamil script: {}", text.chars().any(|c| ('\u{0B80}'..='\u{0BFF}').contains(&c)));
        }
        Err(e) => println!("sarvam batch: FAILED: {}\n  (technical detail for the log: {})", pechu_common::friendly(&e), pechu_common::describe(&e).chars().take(300).collect::<String>().replace(&key, "[key]")),
    }
}

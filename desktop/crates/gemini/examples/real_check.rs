//! Manual check against the REAL Google service. Run it with your key in the environment:
//!   GEMINI_API_KEY=... cargo run -p pechu-gemini --example real_check          (free, read-only: key check and model lookup)
//!   GEMINI_API_KEY=... cargo run -p pechu-gemini --example real_check -- --ai  (also translates a short text and writes notes:
//!                                                                              a few paise of Gemini usage)
//! Never prints the key.
use pechu_common::{friendly, no_progress};
use pechu_gemini::Gemini;

#[tokio::main]
async fn main() {
    let key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY");
    // NOTES_MODEL=<name> picks a model by hand, like the web app's setting (useful when the newest one is overloaded)
    let gemini = Gemini::new(key).with_notes_model(std::env::var("NOTES_MODEL").ok().filter(|m| !m.is_empty()));
    println!("key check:   {:?}", gemini.check_key().await.map_err(|e| friendly(&e)));
    println!("notes model: {:?}", gemini.notes_model().await.map_err(|e| friendly(&e)));

    // a wrong key must be reported as invalid, not as a crash
    let wrong = Gemini::new("not-a-real-key-0000000000000000000000".to_string());
    println!("wrong key:   {:?}", wrong.check_key().await.map_err(|e| friendly(&e)));

    if !std::env::args().any(|a| a == "--ai") {
        return;
    }

    // A made-up meeting: Tamil mixed with English, with time stamps like the real transcripts.
    let transcript = "[00:00] Good morning everyone, இன்னைக்கு release plan பத்தி பேசலாம்.\n\n\
                      [00:20] New build staging-ku வியாழக்கிழமை போகும், production அடுத்த வாரம்.\n\n\
                      [01:05] Ravi payment flow-ஐ வியாழக்கிழமைக்கு முன்னாடி check பண்ணுவார்.\n\n\
                      [02:10] பழைய report-ஐ இன்னும் ஒரு மாசம் வச்சிருக்கலாம்னு முடிவு பண்ணோம்.";
    let progress = pechu_common::progress(|note| println!("  progress: {note}"));

    let english = match gemini.translate_to_english(transcript, progress.clone()).await {
        Ok(text) => text,
        Err(e) => return println!("translate:   FAILED: {}", friendly(&e)),
    };
    println!("translate:   ok, {} characters of English:\n{}\n", english.chars().count(), english);

    match gemini.notes_from_english(&english, progress.clone()).await {
        Ok(notes) => {
            println!("notes:       ok, title = {:?}", notes["title"]);
            for field in ["sections", "keyPoints", "decisions", "actions"] {
                println!("             {field}: {} item(s)", notes[field].as_array().map_or(0, |a| a.len()));
            }
            println!("             flow root: {:?}, has process chart: {}", notes["flow"]["label"], !notes["process"].is_null());
        }
        Err(e) => {
            // the technical detail (what the log would hold), with the key hidden
            let detail: String = pechu_common::describe(&e).chars().take(600).collect();
            println!("notes:       FAILED: {}\n  detail: {}", friendly(&e), detail.replace(&std::env::var("GEMINI_API_KEY").unwrap_or_default(), "[key]"));
        }
    }

    let _ = no_progress; // (kept imported for quick experiments)
}

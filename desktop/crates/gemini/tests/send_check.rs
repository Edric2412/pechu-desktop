//! The app runs these calls inside `tokio::spawn`, which only accepts futures that can move between threads (`Send`).
//! This test does not run anything: it only has to COMPILE, and it stops compiling the day a public method returns a
//! future that is not `Send` (this happened once with `stream.map(closure).buffered()`).
use pechu_common::{no_progress, TranscribeMode};
use pechu_gemini::Gemini;

fn assert_send<T: Send>(_: &T) {}

#[test]
fn every_public_future_can_move_between_threads() {
    let g = Gemini::new("not-a-real-key".into());
    assert_send(&g.check_key());
    assert_send(&g.notes_model());
    assert_send(&g.translate_to_english("text", no_progress()));
    assert_send(&g.notes_from_english("text", no_progress()));
    assert_send(&g.flow_from_english("text", "title", no_progress()));
    let on_english = |_english: String| {};
    assert_send(&g.generate_notes("text", no_progress(), &on_english));
    assert_send(&g.transcribe_parts(&[], TranscribeMode::Smart, no_progress()));
}

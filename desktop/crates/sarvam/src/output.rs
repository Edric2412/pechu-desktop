//! Reads the JSON Sarvam returns for one file. Port of `parseSarvamOutput` and `timedParagraphs` in server/sarvam.ts.
use pechu_text::{entries_from_json, fmt_stamp, turns_from_entries};
use serde_json::Value;

const PARAGRAPH_SECONDS: f64 = 30.0; // start a new paragraph this long after the paragraph began...
const PAUSE_SECONDS: f64 = 2.0; // ...or after a silence this long

/// Turns one batch output JSON into the transcript Pechu stores: timed speaker turns when speakers are wanted, otherwise timed
/// paragraphs built from the chunk timestamps, otherwise the plain transcript string.
pub fn parse_sarvam_output(out: &Value, speakers: bool) -> String {
    if speakers {
        let turns = turns_from_entries(&entries_from_json(&out["diarized_transcript"]["entries"]));
        if !turns.is_empty() {
            return turns;
        }
    }
    let paragraphs = timed_paragraphs(&out["timestamps"]);
    if !paragraphs.is_empty() {
        return paragraphs;
    }
    out["transcript"].as_str().unwrap_or("").trim().to_string()
}

/// One entry of the text array as text ("" for null, so it is skipped later).
fn chunk_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.trim().to_string(),
        other => other.to_string().trim().to_string(),
    }
}

/// Closes the paragraph being built, putting the time it started in front.
fn flush(buf: &mut Vec<String>, para_start: f64, paragraphs: &mut Vec<String>) {
    if !buf.is_empty() {
        paragraphs.push(format!("{} {}", fmt_stamp(para_start), buf.join(" ")));
        buf.clear();
    }
}

fn timed_paragraphs(ts: &Value) -> String {
    // The real output names the text array "words" (each entry is a sentence-sized piece); "chunks" is the documented name.
    let chunks = match ts.get("chunks") {
        Some(v) if !v.is_null() => v,
        _ => &ts["words"],
    };
    let (Some(chunks), Some(starts)) = (chunks.as_array(), ts["start_time_seconds"].as_array()) else {
        return String::new();
    };
    let ends = ts["end_time_seconds"].as_array();

    let mut paragraphs: Vec<String> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let (mut para_start, mut prev_end) = (0.0_f64, 0.0_f64);

    for (i, chunk) in chunks.iter().enumerate() {
        let text = chunk_text(chunk);
        // an entry with no text or no start time is skipped, like the web version
        let Some(start) = starts.get(i).and_then(Value::as_f64) else { continue };
        if text.is_empty() {
            continue;
        }
        if !buf.is_empty() && (start - para_start >= PARAGRAPH_SECONDS || start - prev_end >= PAUSE_SECONDS) {
            flush(&mut buf, para_start, &mut paragraphs);
        }
        if buf.is_empty() {
            para_start = start;
        }
        buf.push(text);
        prev_end = ends.and_then(|e| e.get(i)).and_then(Value::as_f64).unwrap_or(start);
    }
    flush(&mut buf, para_start, &mut paragraphs);
    paragraphs.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn timestamps() -> Value {
        json!({
            "chunks": ["Hello everyone.", "Let's start.", "First topic is budget.", "It grew.", "After a pause.", "Much later on.", "And a long one."],
            "start_time_seconds": [0.2, 3.5, 10, 28, 31.5, 70, 3725],
            "end_time_seconds": [3.0, 9.8, 27.5, 31, 33, 72, 3730],
        })
    }

    #[test]
    fn with_speakers_the_diarized_entries_become_timed_speaker_turns() {
        let out = json!({
            "transcript": "plain",
            "timestamps": timestamps(),
            "diarized_transcript": { "entries": [
                { "transcript": "Hi.", "speaker_id": "0", "start_time_seconds": 1.2, "end_time_seconds": 2 },
                { "transcript": "Hello.", "speaker_id": "1", "start_time_seconds": 4.9, "end_time_seconds": 6 },
            ] },
        });
        assert_eq!(parse_sarvam_output(&out, true), "[00:01] Speaker 1: Hi.\n\n[00:04] Speaker 2: Hello.");
    }

    #[test]
    fn without_speakers_chunks_are_grouped_into_paragraphs_of_about_30_seconds() {
        let text = parse_sarvam_output(&json!({ "transcript": "plain", "timestamps": timestamps() }), false);
        let paras: Vec<&str> = text.split("\n\n").collect();
        assert_eq!(paras[0], "[00:00] Hello everyone. Let's start. First topic is budget. It grew."); // 28 s: still inside 30 s of 0.2
        assert_eq!(paras[1], "[00:31] After a pause.");
        assert_eq!(paras[2], "[01:10] Much later on.");
        assert_eq!(paras[3], "[1:02:05] And a long one."); // more than an hour
        assert_eq!(paras.len(), 4);
    }

    #[test]
    fn a_pause_of_2_seconds_or_more_starts_a_new_paragraph() {
        // 3 - 2.5 = 0.5 s gap and 5.5 - 5 = 0.5 s gap: same paragraph
        let close = json!({ "timestamps": { "chunks": ["One.", "Two.", "Three."], "start_time_seconds": [0, 3, 5.5], "end_time_seconds": [2.5, 5, 7] } });
        assert_eq!(parse_sarvam_output(&close, false), "[00:00] One. Two. Three.");
        let gap = json!({ "timestamps": { "chunks": ["One.", "Two."], "start_time_seconds": [0, 5], "end_time_seconds": [2, 6] } });
        assert_eq!(parse_sarvam_output(&gap, false), "[00:00] One.\n\n[00:05] Two.");
    }

    #[test]
    fn missing_or_unusable_timestamps_fall_back_to_the_plain_transcript() {
        assert_eq!(parse_sarvam_output(&json!({ "transcript": "  just text  " }), false), "just text");
        assert_eq!(parse_sarvam_output(&json!({ "transcript": "just text", "timestamps": {} }), false), "just text");
        assert_eq!(parse_sarvam_output(&json!({ "transcript": "just text", "timestamps": { "chunks": ["a"] } }), true), "just text");
        assert_eq!(parse_sarvam_output(&json!({}), false), "");
        assert_eq!(parse_sarvam_output(&Value::Null, true), "");
    }

    #[test]
    fn speakers_wanted_but_no_diarized_entries_uses_timed_paragraphs() {
        let out = json!({
            "timestamps": { "chunks": ["Hi."], "start_time_seconds": [4], "end_time_seconds": [5] },
            "diarized_transcript": { "entries": [] },
        });
        assert_eq!(parse_sarvam_output(&out, true), "[00:04] Hi.");
    }

    #[test]
    fn the_text_array_may_be_called_words_instead_of_chunks() {
        let out = json!({ "timestamps": { "words": ["One.", "Two."], "start_time_seconds": [0.4, 12.8], "end_time_seconds": [12, 20] } });
        assert_eq!(parse_sarvam_output(&out, false), "[00:00] One. Two.");
    }
}

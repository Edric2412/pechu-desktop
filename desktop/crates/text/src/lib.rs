//! Small pure helpers for long transcripts and speaker turns. Port of server/text.ts (see tests/text.test.ts).
//! CONTRACT: the signatures below are used by other crates. You may add functions; do not change these.
//!
//! Lengths are counted in chars (Unicode letters), not bytes and not UTF-16 units like JS. So a Tamil letter
//! counts as 1, and a `max` of 10_000 means 10_000 chars.
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Matches a leading "[mm:ss] " or "[h:mm:ss] " (at least one space or newline after the "]").
fn stamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[(?:([0-9]+):)?([0-9]{1,2}):([0-9]{2})\]\s+").expect("valid regex"))
}

/// Matches a run of whitespace. Used to find sentence ends.
fn space_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("valid regex"))
}

/// Length in chars (see the note at the top of this file).
fn len(s: &str) -> usize {
    s.chars().count()
}

/// 65.0 -> "[01:05]", 3725.0 -> "[1:02:05]". Negative or non-finite -> "[00:00]".
pub fn fmt_stamp(seconds: f64) -> String {
    // Floor to whole seconds. Negative and non-finite values become 0.
    let total = if seconds.is_finite() { seconds.floor().max(0.0) as u64 } else { 0 };
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("[{h}:{m:02}:{s:02}]")
    } else {
        format!("[{m:02}:{s:02}]")
    }
}

/// "[01:05] Speaker 1: Hi" -> Some((65.0, "Speaker 1: Hi")); None when the line has no stamp.
pub fn parse_stamp(line: &str) -> Option<(f64, String)> {
    let m = stamp_re().captures(line)?;
    // Group 1 (the hours) is optional, so a missing one counts as 0.
    let num = |i: usize| m.get(i).map_or(0.0, |g| g.as_str().parse::<f64>().unwrap_or(0.0));
    let at = num(1) * 3600.0 + num(2) * 60.0 + num(3);
    let rest = &line[m.get(0)?.end()..];
    Some((at, rest.to_string()))
}

/// Removes the stamp from the start of every line.
pub fn strip_stamps(text: &str) -> String {
    text.split('\n')
        .map(|line| stamp_re().replace(line, "").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits text into pieces of at most `max` characters (see the note at the top of this file for what a "character" is),
/// breaking at paragraph ends first, then sentence ends (. ? ! and the Tamil danda), then spaces.
pub fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let clean = text.trim();
    if clean.is_empty() {
        return Vec::new();
    }
    if len(clean) <= max {
        return vec![clean.to_string()];
    }

    // Step 1: make units that each fit in one piece. A paragraph that is too long is split further.
    let mut units: Vec<String> = Vec::new();
    for para in clean.split('\n').map(str::trim).filter(|p| !p.is_empty()) {
        if len(para) <= max {
            units.push(para.to_string());
        } else {
            units.extend(split_long(para, max));
        }
    }

    // Step 2: join small units back together with a blank line, as long as they still fit.
    let mut out = Vec::new();
    let mut cur = String::new();
    for u in units {
        if !cur.is_empty() && len(&cur) + len(&u) + 2 > max {
            out.push(std::mem::take(&mut cur));
            cur = u;
        } else if cur.is_empty() {
            cur = u;
        } else {
            cur = format!("{cur}\n\n{u}");
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Splits one long paragraph: sentence by sentence, then word by word when one sentence is still too long.
fn split_long(paragraph: &str, max: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    for sentence in split_sentences(paragraph) {
        if len(&sentence) <= max {
            push_piece(&mut pieces, &mut cur, &sentence, max);
            continue;
        }
        let mut words: Vec<String> = sentence.split_whitespace().map(str::to_string).collect();
        // Never leave a lone "[mm:ss]" at the end of a piece: glue it to the word after it.
        if words.len() > 1
            && stamp_re().is_match(&format!("{} ", words[0]))
            && len(&words[0]) + len(&words[1]) < max
        {
            let glued = format!("{} {}", words[0], words[1]);
            words.splice(0..2, [glued]);
        }
        // No punctuation at all: break at spaces.
        for word in &words {
            push_piece(&mut pieces, &mut cur, word, max);
        }
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

/// Splits after ". ", "? ", "! " or "। " (the whitespace is dropped). The JS version uses a lookbehind, which Rust's
/// regex crate does not have, so this checks the character just before each run of whitespace instead.
fn split_sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in space_re().find_iter(paragraph) {
        let before = paragraph[..m.start()].chars().next_back();
        if matches!(before, Some('.' | '?' | '!' | '।')) {
            out.push(paragraph[last..m.start()].to_string());
            last = m.end();
        }
    }
    out.push(paragraph[last..].to_string());
    out
}

/// Adds `s` to the current piece. Starts a new piece first when adding it would go over `max`.
fn push_piece(pieces: &mut Vec<String>, cur: &mut String, s: &str, max: usize) {
    if !cur.is_empty() && len(cur) + len(s) + 1 > max {
        pieces.push(std::mem::take(cur));
        cur.push_str(s);
    } else if cur.is_empty() {
        cur.push_str(s);
    } else {
        cur.push(' ');
        cur.push_str(s);
    }
}

/// One word from Google's word list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub text: Option<String>,
    pub speaker: Option<String>,
}

/// "spk_2" -> "Speaker 2"; no digits -> the trimmed raw text, or "Speaker 1" when empty.
pub fn speaker_label(raw: Option<&str>) -> String {
    // The first run of digits is the speaker number. It stays as text, so "spk_02" gives "Speaker 02".
    if let Some(r) = raw {
        let digits: String = r
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return format!("Speaker {digits}");
        }
    }
    match raw.map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => "Speaker 1".to_string(),
    }
}

/// Writes the turn collected so far as one "head: words" line, then clears the words.
fn push_turn(lines: &mut Vec<String>, head: Option<&str>, buf: &mut Vec<String>) {
    if let Some(h) = head {
        if !buf.is_empty() {
            lines.push(format!("{h}: {}", buf.join(" ")));
        }
    }
    buf.clear();
}

/// "Speaker 1: ...\n\nSpeaker 2: ..." from Google's word list; "" when no word carries a speaker.
pub fn turns_from_words(words: &[Word]) -> String {
    // No speaker tags at all: the caller uses the plain text instead.
    if !words.iter().any(|w| w.speaker.as_deref().is_some_and(|s| !s.is_empty())) {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut head: Option<String> = None;
    let mut buf: Vec<String> = Vec::new();
    for w in words {
        let t = w.text.as_deref().unwrap_or("").trim();
        if t.is_empty() {
            continue;
        }
        let label = speaker_label(w.speaker.as_deref());
        // A new speaker starts a new turn.
        if head.as_deref() != Some(label.as_str()) {
            push_turn(&mut lines, head.as_deref(), &mut buf);
            head = Some(label);
        }
        buf.push(t.to_string());
    }
    push_turn(&mut lines, head.as_deref(), &mut buf);
    lines.join("\n\n")
}

/// Word annotations live under steps[].content[].annotations[] with type "word_info" in Google's interactions response.
pub fn extract_words(response: &Value) -> Vec<Word> {
    let mut words = Vec::new();
    // Each level may be missing. A missing level just means there is nothing below it.
    for step in response.get("steps").and_then(Value::as_array).into_iter().flatten() {
        for content in step.get("content").and_then(Value::as_array).into_iter().flatten() {
            for a in content.get("annotations").and_then(Value::as_array).into_iter().flatten() {
                if a.get("type").and_then(Value::as_str) == Some("word_info") {
                    words.push(Word { text: str_field(a, "text"), speaker: str_field(a, "speaker") });
                }
            }
        }
    }
    words
}

/// Reads a string field from a JSON object. None when the field is missing or not a string.
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

/// One sentence of Sarvam's diarized output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeakerEntry {
    pub transcript: Option<String>,
    /// "0" or 0: any JSON string or number, kept as text.
    pub speaker_id: Option<String>,
    pub start_time_seconds: Option<f64>,
}

/// Reads Sarvam's `diarized_transcript.entries` array. Pass that array itself, not the whole response.
/// Anything that is not an array gives an empty list.
pub fn entries_from_json(entries: &Value) -> Vec<SpeakerEntry> {
    let Some(items) = entries.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|e| SpeakerEntry {
            transcript: str_field(e, "transcript"),
            speaker_id: match e.get("speaker_id") {
                Some(Value::String(s)) => Some(s.clone()),
                // Keep whole numbers as "1", not "1.0".
                Some(Value::Number(n)) => Some(n.as_i64().map_or_else(|| n.to_string(), |i| i.to_string())),
                _ => None,
            },
            start_time_seconds: e.get("start_time_seconds").and_then(Value::as_f64),
        })
        .collect()
}

/// Speakers numbered in the order they first speak; neighbouring sentences by one speaker become one turn, each starting
/// with the stamp of its first entry when the entries carry times.
pub fn turns_from_entries(entries: &[SpeakerEntry]) -> String {
    // Give each raw speaker id a number, in the order the ids first appear.
    let mut order: HashMap<String, usize> = HashMap::new();
    let mut lines = Vec::new();
    let mut current: Option<String> = None; // the speaker label, e.g. "Speaker 2"
    let mut head: Option<String> = None; // the line start: the stamp (if any) plus the label
    let mut buf: Vec<String> = Vec::new();
    for e in entries {
        let t = e.transcript.as_deref().unwrap_or("").trim();
        if t.is_empty() {
            continue;
        }
        let id = e.speaker_id.clone().unwrap_or_else(|| "0".to_string());
        let next = order.len() + 1;
        let n = *order.entry(id).or_insert(next);
        let label = format!("Speaker {n}");
        if current.as_deref() != Some(label.as_str()) {
            push_turn(&mut lines, head.as_deref(), &mut buf);
            // Only a finite time gives a stamp.
            head = Some(match e.start_time_seconds.filter(|s| s.is_finite()) {
                Some(s) => format!("{} {label}", fmt_stamp(s)),
                None => label.clone(),
            });
            current = Some(label);
        }
        buf.push(t.to_string());
    }
    push_turn(&mut lines, head.as_deref(), &mut buf);
    lines.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn w(text: &str, speaker: &str) -> Word {
        Word { text: Some(text.to_string()), speaker: Some(speaker.to_string()) }
    }

    #[test]
    fn short_text_stays_in_one_piece() {
        assert_eq!(chunk_text("hello there", 10_000), vec!["hello there"]);
        assert!(chunk_text("   ", 10_000).is_empty());
    }

    #[test]
    fn long_text_is_split_at_paragraph_and_sentence_ends_losing_nothing() {
        let sentence = "This is one sentence about the release date. ";
        let text = (sentence.repeat(30) + "\n\n").repeat(4); // about 5,400 chars
        let parts = chunk_text(&text, 2000);
        assert!(parts.len() > 2);
        for p in &parts {
            assert!(len(p) <= 2000);
        }
        let words = |t: &str| t.split_whitespace().count();
        assert_eq!(parts.iter().map(|p| words(p)).sum::<usize>(), words(&text));
    }

    #[test]
    fn one_huge_paragraph_with_no_punctuation_is_broken_at_spaces() {
        let blob = "word ".repeat(1500).trim().to_string(); // 7,499 chars, no full stops
        let parts = chunk_text(&blob, 3000);
        assert_eq!(parts.len(), 3);
        for p in &parts {
            assert!(len(p) <= 3000);
        }
        assert_eq!(parts.join(" ").split(' ').count(), 1500);
    }

    #[test]
    fn tamil_danda_is_a_sentence_end() {
        // Each sentence is 35 chars and two together are over 50. So the text must break after each danda: 3 pieces.
        let s = "நான் வீட்டுக்கு போகிறேன் இன்று மாலை। ";
        let text = s.repeat(3).trim().to_string();
        let parts = chunk_text(&text, 50);
        assert_eq!(parts.len(), 3);
        for p in &parts {
            assert!(p.ends_with('।'));
        }
    }

    #[test]
    fn google_spk_tags_become_speaker_turns() {
        assert_eq!(speaker_label(Some("spk_2")), "Speaker 2");
        assert_eq!(speaker_label(None), "Speaker 1");
        let words = vec![
            w("Hello", "spk_1"), w("everyone.", "spk_1"),
            w("வணக்கம்", "spk_2"), w("sir", "spk_2"),
            w("Let's", "spk_1"), w("start", "spk_1"),
        ];
        assert_eq!(
            turns_from_words(&words),
            "Speaker 1: Hello everyone.\n\nSpeaker 2: வணக்கம் sir\n\nSpeaker 1: Let's start"
        );
    }

    #[test]
    fn no_speaker_tags_means_no_turns() {
        let plain = vec![Word { text: Some("Hello".into()), speaker: None }, Word { text: Some("there".into()), speaker: None }];
        assert_eq!(turns_from_words(&plain), "");
        assert_eq!(turns_from_words(&[]), "");
    }

    #[test]
    fn extract_words_finds_word_info_annotations_in_googles_response_shape() {
        let response = json!({ "steps": [{ "type": "model_output", "content": [{ "type": "text", "text": "Hi", "annotations": [
            { "type": "word_info", "text": "Hi", "speaker": "spk_1", "start_offset": "0.1s" }, { "type": "other" },
        ] }] }] });
        assert_eq!(extract_words(&response), vec![w("Hi", "spk_1")]);
        assert!(extract_words(&json!({})).is_empty());
    }

    #[test]
    fn sarvam_entries_become_numbered_speaker_turns_merging_neighbouring_sentences() {
        // speaker_id is a string in some entries and a number in others: both must count as the same speaker.
        let entries = entries_from_json(&json!([
            { "transcript": "Hello.", "speaker_id": "1" }, { "transcript": "How are you?", "speaker_id": "1" },
            { "transcript": "நல்லா இருக்கேன்.", "speaker_id": "0" },
            { "transcript": "  ", "speaker_id": "0" },
            { "transcript": "Good to hear.", "speaker_id": 1 },
        ]));
        // Speakers are numbered in the order they first speak, whatever ids Sarvam used.
        assert_eq!(
            turns_from_entries(&entries),
            "Speaker 1: Hello. How are you?\n\nSpeaker 2: நல்லா இருக்கேன்.\n\nSpeaker 1: Good to hear."
        );
        assert_eq!(turns_from_entries(&[]), "");
    }

    #[test]
    fn entries_from_json_reads_string_and_number_speaker_ids() {
        let entries = entries_from_json(&json!([
            { "transcript": "a", "speaker_id": "0", "start_time_seconds": 1.5 },
            { "transcript": "b", "speaker_id": 2 },
            { "transcript": "c" },
        ]));
        assert_eq!(
            entries,
            vec![
                SpeakerEntry { transcript: Some("a".into()), speaker_id: Some("0".into()), start_time_seconds: Some(1.5) },
                SpeakerEntry { transcript: Some("b".into()), speaker_id: Some("2".into()), start_time_seconds: None },
                SpeakerEntry { transcript: Some("c".into()), speaker_id: None, start_time_seconds: None },
            ]
        );
        assert!(entries_from_json(&json!({ "not": "an array" })).is_empty());
    }

    #[test]
    fn fmt_stamp_floors_to_whole_seconds_and_switches_to_h_mm_ss_from_one_hour() {
        assert_eq!(fmt_stamp(0.0), "[00:00]");
        assert_eq!(fmt_stamp(75.9), "[01:15]");
        assert_eq!(fmt_stamp(3599.0), "[59:59]");
        assert_eq!(fmt_stamp(3725.0), "[1:02:05]");
        assert_eq!(fmt_stamp(36000.0), "[10:00:00]");
    }

    #[test]
    fn parse_stamp_and_strip_stamps_round_trip() {
        assert_eq!(parse_stamp("[01:15] Speaker 2: hi"), Some((75.0, "Speaker 2: hi".to_string())));
        assert_eq!(parse_stamp("[1:02:05] hello"), Some((3725.0, "hello".to_string())));
        assert_eq!(parse_stamp("Speaker 2: hi"), None);
        assert_eq!(parse_stamp("[01:15]"), None); // a stamp needs text after it
        for at in [0.0, 59.0, 61.0, 3600.0, 7322.0] {
            assert_eq!(parse_stamp(&format!("{} x", fmt_stamp(at))).map(|(a, _)| a), Some(at));
        }
        let timed = "[00:05] Speaker 1: Hello\n\n[1:00:00] Speaker 2: Bye\n\nplain old paragraph";
        assert_eq!(strip_stamps(timed), "Speaker 1: Hello\n\nSpeaker 2: Bye\n\nplain old paragraph");
        assert_eq!(strip_stamps("no stamps here"), "no stamps here");
    }

    #[test]
    fn sarvam_entries_with_times_start_each_turn_with_the_stamp_of_its_first_entry() {
        let entry = |t: &str, id: &str, start: f64| SpeakerEntry {
            transcript: Some(t.into()),
            speaker_id: Some(id.into()),
            start_time_seconds: Some(start),
        };
        let entries = vec![
            entry("Hello.", "1", 0.4),
            entry("How are you?", "1", 2.9),
            entry("Fine.", "0", 65.2),
            entry("Late one.", "1", 3725.8),
        ];
        assert_eq!(
            turns_from_entries(&entries),
            "[00:00] Speaker 1: Hello. How are you?\n\n[01:05] Speaker 2: Fine.\n\n[1:02:05] Speaker 1: Late one."
        );
    }

    #[test]
    fn chunk_text_keeps_every_stamp_with_its_text_and_splits_timed_transcripts_between_paragraphs() {
        let sentence = "This is one sentence about the release date. ";
        let para = |at: f64| format!("{} Speaker 1: {}", fmt_stamp(at), sentence.repeat(10).trim());
        let text = [0.0, 30.0, 60.0, 90.0, 3600.0, 3630.0].map(para).join("\n\n");
        let parts = chunk_text(&text, 1200);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(len(p) <= 1200);
            for line in p.split("\n\n") {
                assert!(parse_stamp(line).is_some()); // every paragraph still starts with its stamp
            }
        }
        assert_eq!(parts.join("\n\n"), text);
        // A single over-long paragraph: the stamp stays attached to the words after it.
        let long = format!("[00:10] {}", "word ".repeat(1500).trim());
        let pieces = chunk_text(&long, 3000);
        for p in &pieces {
            assert_ne!(p.as_str(), "[00:10]");
        }
        assert!(pieces[0].starts_with("[00:10] word"));
    }
}

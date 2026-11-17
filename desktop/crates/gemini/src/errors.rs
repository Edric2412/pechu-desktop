//! What can go wrong when talking to Google, how to tell the kinds apart, and the plain-words message for each.
//! Port of the error parts of server/gemini.ts (`isDailyLimit`, `isRateLimit`, `isBusy`, `friendlyError`, `describeError`).
use crate::mask_key;
use pechu_common::{is_network_error, user_err, UserError};
use regex::Regex;
use std::sync::LazyLock;

/// A failed call to Google: the HTTP status (None when we never got an answer) and what Google or the network said.
/// This text is for the log and for deciding what to do; it is never shown on screen (see `friendly_message`).
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

/// The status and the full text of an error, wherever the `ApiError` sits in the chain.
pub fn status_and_text(e: &anyhow::Error) -> (Option<u16>, String) {
    let status = e.chain().find_map(|c| c.downcast_ref::<ApiError>()).and_then(|a| a.status);
    (status, format!("{e:#}"))
}

fn has(raw: &str, words: &[&str]) -> bool {
    let lower = raw.to_lowercase();
    words.iter().any(|w| lower.contains(&w.to_lowercase()))
}

// Google's free tier has three different 429s: a daily request cap, a per-minute token cap, and "busy" (503).
// Waiting helps with the last two only; a daily cap resets hours later, so retrying would just hang.
pub fn is_daily_limit(raw: &str) -> bool {
    has(raw, &["per day", "daily"])
}

pub fn is_rate_limit(status: Option<u16>, raw: &str) -> bool {
    !is_daily_limit(raw) && (status == Some(429) || has(raw, &["rate limit", "RESOURCE_EXHAUSTED"]))
}

pub fn is_busy(status: Option<u16>, raw: &str) -> bool {
    status == Some(503) || has(raw, &["overloaded", "UNAVAILABLE"])
}

static RESET_IN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)retry in\s+(?:(\d+)h)?\s*(?:(\d+)m)?").unwrap());

/// "Please retry in 14h21m22s" -> "in about 14 h 21 min"; no hours or minutes given -> "later".
fn reset_in(raw: &str) -> String {
    let Some(m) = RESET_IN.captures(raw) else { return "later".into() };
    let hours = m.get(1).map(|x| format!("{} h", x.as_str()));
    let minutes = m.get(2).map(|x| format!("{} min", x.as_str()));
    let parts: Vec<String> = [hours, minutes].into_iter().flatten().collect();
    if parts.is_empty() {
        return "later".into();
    }
    format!("in about {}", parts.join(" "))
}

/// The sentence the person reads for a failure (`friendlyError` for everything that is not already a `UserError`).
/// The key is masked, and the result is cut to 300 characters.
pub fn friendly_message(status: Option<u16>, raw: &str, key: &str) -> String {
    let msg = if is_daily_limit(raw) {
        format!(
            "Today's free limit for this Gemini model is used up. It resets {}. A paid Google plan removes the limit.",
            reset_in(raw)
        )
    } else if matches!(status, Some(401) | Some(403)) || has(raw, &["API key not valid", "PERMISSION_DENIED", "UNAUTHENTICATED"]) {
        "Google did not accept the API key, or the key cannot use this model. Check the key and try again.".into()
    } else if status == Some(400) && has(raw, &["invalid argument"]) {
        "Google could not read this audio. Try exporting it as MP3 or WAV, then upload it again.".into()
    } else if status == Some(429) || has(raw, &["quota", "RESOURCE_EXHAUSTED"]) {
        "The Gemini quota or rate limit was reached. Wait a little, or check your Google plan, then try again.".into()
    } else if status == Some(503) || has(raw, &["overloaded", "UNAVAILABLE"]) {
        "Gemini is busy right now. Try again in a minute.".into()
    } else if is_network_error(raw) {
        "Pechu could not reach the service. Check the internet connection and try again.".into()
    } else if raw.contains("ENOSPC") {
        "The disk on this computer is full. Free some space and try again.".into()
    } else if status.is_some_and(|s| s >= 500) {
        "The service had a problem on its side. Try again in a few minutes.".into()
    } else if has(raw, &["Audio conversion failed", "No audio was found"]) {
        "Pechu could not read this audio file. Try exporting it as MP3 or WAV, then upload it again.".into()
    } else {
        // never show raw error text
        "Something went wrong. Please try again. If it keeps happening, the details are in Pechu's log.".into()
    };
    let msg = if key.is_empty() { msg } else { msg.replace(key, &mask_key(key)) };
    msg.chars().take(300).collect()
}

/// The last step of every public call: an error that is already a `UserError` stays as it is; anything else becomes a
/// `UserError` with the plain-words message, and the technical text goes to the log (key masked).
pub fn into_user_error(e: anyhow::Error, key: &str) -> anyhow::Error {
    if e.chain().any(|c| c.downcast_ref::<UserError>().is_some()) {
        return e;
    }
    let (status, raw) = status_and_text(&e);
    tracing::warn!("[gemini] {}", describe_error(&e, &[key]));
    user_err(friendly_message(status, &raw, key))
}

/// The full technical text of an error, for Pechu's own log and never for the screen. Keys are masked, the cause is kept.
pub fn describe_error(e: &anyhow::Error, secrets: &[&str]) -> String {
    let mut raw = format!("{e:#}");
    for key in secrets.iter().filter(|k| !k.is_empty()) {
        raw = raw.replace(key, &mask_key(key));
    }
    raw.chars().take(800).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Text that must never reach the screen: JSON, status codes, stack traces, Node error codes.
    fn messy(s: &str) -> bool {
        Regex::new(r"[{}]|request_id|invalid_request|\b[45]\d\d:|Error:|at \S+ \(|ECONN|ETIMEDOUT").unwrap().is_match(s)
    }

    #[test]
    fn network_failures_read_as_a_connection_problem() {
        for raw in [
            "fetch failed",
            "read ECONNRESET",
            "connect ETIMEDOUT 1.2.3.4:443",
            "getaddrinfo ENOTFOUND api.example.com",
            "socket hang up",
            "error sending request for url (https://x)",
        ] {
            let msg = friendly_message(None, raw, "");
            assert!(msg.contains("could not reach the service"), "{raw}");
            assert!(!messy(&msg));
        }
        assert!(!is_network_error("flow node has no label"));
    }

    #[test]
    fn an_error_nobody_planned_for_shows_a_calm_generic_message() {
        for raw in [
            "Unexpected token < in JSON at position 0",
            "flow node has no label",
            r#"{"error":{"code":500,"message":"boom"}}"#,
            "TypeError: undefined is not an object (evaluating 'x.y')",
        ] {
            let msg = friendly_message(None, raw, "");
            assert!(msg.contains("Something went wrong"));
            assert!(!msg.contains(raw));
        }
    }

    #[test]
    fn other_known_failures_keep_their_plain_messages() {
        assert!(friendly_message(None, "write failed ENOSPC", "").contains("disk"));
        assert!(friendly_message(Some(502), "bad gateway", "").contains("problem on its side"));
        assert!(friendly_message(None, "Audio conversion failed: gst error", "").contains("could not read this audio file"));
        assert!(friendly_message(Some(401), "nope", "").contains("did not accept the API key"));
        assert!(friendly_message(Some(400), "Request contains an invalid argument.", "").contains("could not read this audio"));
        assert!(friendly_message(Some(429), "slow down", "").contains("quota or rate limit"));
        assert!(friendly_message(Some(503), "The model is overloaded", "").contains("busy right now"));
    }

    #[test]
    fn the_daily_limit_says_when_it_resets() {
        let raw = "Quota exceeded: requests per day. Please retry in 14h21m22.5s.";
        let msg = friendly_message(Some(429), raw, "");
        assert_eq!(
            msg,
            "Today's free limit for this Gemini model is used up. It resets in about 14 h 21 min. A paid Google plan removes the limit."
        );
        assert!(friendly_message(Some(429), "daily limit, retry in 45m", "").contains("in about 45 min"));
        assert!(friendly_message(Some(429), "daily limit, no hint", "").contains("It resets later."));
        // a daily cap is never retried; the per-minute one is
        assert!(!is_rate_limit(Some(429), raw));
        assert!(is_rate_limit(Some(429), "tokens per minute"));
        assert!(is_busy(Some(503), "") && is_busy(None, "model overloaded") && !is_busy(Some(500), "boom"));
    }

    #[test]
    fn a_key_in_the_text_is_masked() {
        let key = "AIzaSyFakeGeminiKey1234567890";
        // the generic message does not contain the raw text at all, so only a key inside a fixed sentence could leak
        let msg = friendly_message(Some(500), &format!("boom {key}"), key);
        assert!(!msg.contains(key));
        let e = anyhow::anyhow!("failed with key={key}").context("outer");
        let log = describe_error(&e, &[key]);
        assert!(log.contains("outer") && !log.contains(key));
    }

    #[test]
    fn a_user_error_passes_through_and_others_become_user_errors() {
        let kept = into_user_error(user_err("Write the notes first."), "k");
        assert_eq!(pechu_common::friendly(&kept), "Write the notes first.");
        let api = anyhow::Error::new(ApiError { status: Some(503), message: "overloaded".into() });
        assert_eq!(pechu_common::friendly(&into_user_error(api, "k")), "Gemini is busy right now. Try again in a minute.");
    }
}

//! What the person reads when Sarvam (or the connection to it) fails. Port of `sarvamMessage` and `isSarvamContextError`.
//! The raw text goes to the log only; it is never shown.
use pechu_common::is_network_error;

/// Sarvam's own failure text starts with the status, for example `400:{"error":...}`. Reads that status, if there is one.
fn status_of(raw: &str) -> Option<u16> {
    let digits: String = raw.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    // exactly three digits, so "4192 tokens" is not mistaken for a status
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

/// The wording Sarvam uses when audio is too long for the model to take in one piece.
fn is_length_limit(raw: &str) -> bool {
    let r = raw.to_lowercase();
    ["context length", "input length", "too long", "exceeds"].iter().any(|p| r.contains(p))
}

/// Sarvam could not take this audio in one piece (400 and a "context length"/"input length"/"too long"/"exceeds" text).
/// Cutting the recording into shorter pieces can help.
pub fn is_sarvam_context_error(raw: &str) -> bool {
    status_of(raw) == Some(400) && is_length_limit(raw)
}

/// What the person reads for anything Sarvam, or the connection to it, reports. 401/403 key, 402 credits, 429 rate limit,
/// 413 too large, 400+context length, 5xx, network. The status is read from the start of `raw` when it is not given.
pub fn sarvam_message(raw: &str, status: Option<u16>) -> String {
    let code = status.or_else(|| status_of(raw));
    let lower = raw.to_lowercase();
    let msg = if code == Some(401) || code == Some(403) {
        "Sarvam did not accept the API key. Check SARVAM_API_KEY in .env."
    } else if code == Some(402) || ["insufficient", "credit", "balance"].iter().any(|p| lower.contains(p)) {
        "The Sarvam account is out of credits. Add credits at dashboard.sarvam.ai and try again."
    } else if code == Some(429) {
        "Sarvam is rate limiting this key. Wait a minute and try again."
    } else if code == Some(413) || lower.contains("too large") || lower.contains("file size") {
        "Sarvam says this recording is too large. Try a shorter recording."
    } else if code == Some(400) && is_length_limit(raw) {
        "Sarvam could not process this recording in one piece. This can happen when a long recording has constant background noise. Try a shorter section of it."
    } else if code.is_some_and(|c| c >= 500) {
        "Sarvam is having trouble on its side. Try again in a few minutes."
    } else if is_network_error(raw) {
        "Pechu could not reach Sarvam. Check the internet connection and try again."
    } else {
        "Sarvam could not transcribe this recording. Try again, and if it keeps failing, try a shorter recording."
    };
    msg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real text Sarvam sent for the first 50-minute recording.
    const CONTEXT: &str = r#"400:{"error":{"message":"Input length (4192) exceeds model's maximum context length (4096).","code":"invalid_request_error","request_id":"20261112_5e7cd723"}}"#;

    /// Raw JSON, status codes or error text must never reach the screen.
    fn messy(s: &str) -> bool {
        ["{", "}", "request_id", "invalid_request", "ECONN", "ETIMEDOUT", "Error:"].iter().any(|p| s.contains(p))
    }

    #[test]
    fn the_context_length_failure_is_explained_in_plain_words() {
        let msg = sarvam_message(CONTEXT, None);
        assert!(msg.contains("could not process this recording in one piece"));
        assert!(!messy(&msg));
    }

    #[test]
    fn each_problem_gets_its_own_message() {
        assert!(sarvam_message("", Some(401)).contains("did not accept the API key"));
        assert!(sarvam_message("", Some(403)).contains("did not accept the API key"));
        assert!(sarvam_message("", Some(402)).contains("out of credits"));
        assert!(sarvam_message("insufficient balance", None).contains("out of credits"));
        assert!(sarvam_message("", Some(429)).contains("rate limiting"));
        assert!(sarvam_message("413:payload too large", None).contains("too large"));
        assert!(sarvam_message("503:upstream connect error", None).contains("trouble on its side"));
        assert!(sarvam_message("fetch failed", None).contains("could not reach Sarvam"));
        assert!(sarvam_message("error sending request for url (https://api.sarvam.ai/x)", None).contains("could not reach Sarvam"));
    }

    #[test]
    fn an_unknown_failure_never_shows_its_raw_text() {
        let msg = sarvam_message(r#"418:{"error":{"message":"something nobody expected","request_id":"abc"}}"#, None);
        assert_eq!(msg, "Sarvam could not transcribe this recording. Try again, and if it keeps failing, try a shorter recording.");
        assert!(!messy(&msg));
    }

    #[test]
    fn the_length_failure_is_recognised_and_other_failures_are_not() {
        assert!(is_sarvam_context_error(CONTEXT));
        assert!(!is_sarvam_context_error("413:payload too large"));
        assert!(!is_sarvam_context_error(r#"400:{"error":{"message":"language not supported"}}"#));
        assert!(!is_sarvam_context_error(""));
        // a length text without the 400 status in front is not enough
        assert!(!is_sarvam_context_error("Input length exceeds the maximum"));
    }

    #[test]
    fn a_status_needs_exactly_three_digits() {
        assert_eq!(status_of("  503:oops"), Some(503));
        assert_eq!(status_of("4192 tokens"), None);
        assert_eq!(status_of("no status"), None);
    }
}

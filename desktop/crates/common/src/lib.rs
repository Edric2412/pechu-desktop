//! Small things every Pechu crate shares: the error a person is allowed to read, progress callbacks,
//! and the three ways audio can be transcribed. (Ports server/errors.ts and the TranscribeMode type.)
use std::sync::Arc;

/// Receives short notes such as "Part 2 of 3" while a job runs. Cheap to clone, safe to call from any thread.
pub type Progress = Arc<dyn Fn(String) + Send + Sync>;

pub fn progress(f: impl Fn(String) + Send + Sync + 'static) -> Progress {
    Arc::new(f)
}

/// A progress callback that ignores everything (tests, or jobs nobody is watching).
pub fn no_progress() -> Progress {
    Arc::new(|_| {})
}

/// An error whose message is already written for the person using Pechu, so it can be shown as it is.
/// Anything that is not a `UserError` is shown as a generic message; its details go to the log only.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UserError(pub String);

/// Builds an `anyhow::Error` carrying a message that is safe and clear enough to show on screen.
pub fn user_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UserError(msg.into()))
}

const GENERIC: &str = "Something went wrong. Please try again. If it keeps happening, the details are in Pechu's log.";

/// What the person reads for an error (ports `friendlyError`): a `UserError` anywhere in the chain is shown
/// (cut to 300 characters); anything else becomes the generic message, never raw error text.
pub fn friendly(e: &anyhow::Error) -> String {
    match e.chain().find_map(|c| c.downcast_ref::<UserError>()) {
        Some(u) => u.0.chars().take(300).collect(),
        None => GENERIC.to_string(),
    }
}

/// The full technical text of an error, for the log and never for the screen (keys must be masked by the caller).
pub fn describe(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

/// "fetch failed", reset connections, DNS failures and timeouts: the service could not be reached at all.
pub fn is_network_error(raw: &str) -> bool {
    let r = raw.to_lowercase();
    [
        "fetch failed", "econnreset", "econnrefused", "etimedout", "enotfound", "eai_again", "epipe", "socket hang up",
        "network error", "timed out", "timeout", "connection refused", "connection reset", "dns error", "error sending request",
    ]
    .iter()
    .any(|p| r.contains(p))
}

/// How a recording is turned into text. `Speakers` = as spoken, with a speaker label on every turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscribeMode {
    Smart,
    Verbatim,
    Speakers,
}

impl TranscribeMode {
    /// "verbatim" | "speakers" | anything else = Smart.
    pub fn parse(s: &str) -> Self {
        match s {
            "verbatim" => Self::Verbatim,
            "speakers" => Self::Speakers,
            _ => Self::Smart,
        }
    }
}

/// Every transcription path works on 16 kHz, 16-bit, mono audio.
pub const RATE: u32 = 16_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_error_is_shown_even_when_wrapped() {
        let e = user_err("The Sarvam account is out of credits.").context("job 1 failed");
        assert_eq!(friendly(&e), "The Sarvam account is out of credits.");
    }

    #[test]
    fn any_other_error_is_never_shown_raw() {
        let e = anyhow::anyhow!("secret-key-abc leaked in raw text");
        assert!(!friendly(&e).contains("secret"));
        assert!(describe(&e).contains("secret"));
    }

    #[test]
    fn long_messages_are_cut() {
        assert_eq!(friendly(&user_err("x".repeat(500))).chars().count(), 300);
    }

    #[test]
    fn network_errors_are_recognised() {
        assert!(is_network_error("error sending request for url"));
        assert!(is_network_error("ECONNRESET"));
        assert!(!is_network_error("400 invalid argument"));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(TranscribeMode::parse("speakers"), TranscribeMode::Speakers);
        assert_eq!(TranscribeMode::parse("verbatim"), TranscribeMode::Verbatim);
        assert_eq!(TranscribeMode::parse("whatever"), TranscribeMode::Smart);
    }
}

//! macOS (and any other system that is neither Linux nor Windows): capturing the computer's own sound is not built yet.
//! The page simply does not offer the option.
//!
//! TODO macOS: ScreenCaptureKit (an audio-only stream of the whole system; it asks for the Screen Recording permission).
use crate::{CaptureStream, SystemAudio};

#[derive(Debug, Default)]
pub struct Unsupported;

const NOT_YET: &str = "System audio capture is not available on this system yet.";

impl SystemAudio for Unsupported {
    fn available(&self) -> Result<(), String> {
        Err(NOT_YET.to_string())
    }

    fn start(&self) -> Result<CaptureStream, String> {
        Err(NOT_YET.to_string())
    }
}

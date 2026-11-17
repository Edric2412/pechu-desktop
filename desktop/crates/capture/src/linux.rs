//! Linux: PipeWire's command line tools (they come with every PipeWire desktop, and there is no pactl/parec on such systems).
//!   - `pw-dump` lists what PipeWire knows, as JSON: we only check that a sound output (a "sink") exists.
//!   - `pw-record -P stream.capture.sink=true` records a sink's "monitor", i.e. what is being played. With no `--target` it
//!     follows the default output, so plugging in headphones in the middle of a meeting keeps working.
//!
//! The program names are fields, so tests can point them at fake scripts.
use crate::{CaptureFormat, CaptureStream, SystemAudio};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

/// What we ask pw-record for, and what the page is told. Raw 16-bit stereo at 48 kHz is PipeWire's own native rate, so nothing
/// needs converting on the way.
const FORMAT: CaptureFormat = CaptureFormat { sample_rate: 48_000, channels: 2 };

#[derive(Clone, Debug)]
pub struct Pipewire {
    pub record_program: OsString,
    pub dump_program: OsString,
    /// How long `pw-dump` / `pw-record --version` may take before we say PipeWire does not answer.
    pub tool_timeout: Duration,
}

impl Default for Pipewire {
    fn default() -> Self {
        Pipewire { record_program: "pw-record".into(), dump_program: "pw-dump".into(), tool_timeout: Duration::from_secs(5) }
    }
}

/// The few variables PipeWire's tools need to find the sound server. Everything else (API keys in the app's environment,
/// for example) is not handed to a program we did not write.
fn tool_environment() -> Vec<(&'static str, OsString)> {
    ["PATH", "HOME", "XDG_RUNTIME_DIR", "PIPEWIRE_REMOTE", "PIPEWIRE_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect()
}

impl Pipewire {
    /// Runs a short program and returns what it printed. A program that hangs (a stuck sound server) is killed after
    /// `tool_timeout`, so asking "is it available?" can never freeze the app.
    fn run(&self, program: &OsStr, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut child = std::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(tool_environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "System audio capture needs PipeWire's tools (pw-record and pw-dump), which were not found.".to_string())?;
        let mut stdout = child.stdout.take().expect("stdout was piped");
        // Read on another thread: pw-dump prints a lot, and a full pipe would stop it before it could exit.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = stdout.read_to_end(&mut text);
            let _ = tx.send(text);
        });
        match rx.recv_timeout(self.tool_timeout) {
            Ok(text) => match child.wait() {
                Ok(status) if status.success() => Ok(text),
                _ => Err("The sound system (PipeWire) did not answer. Is it running?".to_string()),
            },
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err("The sound system (PipeWire) did not answer in time.".to_string())
            }
        }
    }
}

impl SystemAudio for Pipewire {
    fn available(&self) -> Result<(), String> {
        self.run(&self.record_program, &["--version"])?;
        let dump = self.run(&self.dump_program, &["--no-colors"])?;
        if has_sink(&dump) {
            Ok(())
        } else {
            Err("No sound output (speakers or headphones) was found on this computer.".to_string())
        }
    }

    fn start(&self) -> Result<CaptureStream, String> {
        let mut command = tokio::process::Command::new(&self.record_program);
        command
            .args(["--raw", "--rate", "48000", "--channels", "2", "--format", "s16", "--latency", "40ms"])
            .args(["-P", "stream.capture.sink=true node.name=pechu-system-audio"])
            .arg("-") // raw PCM on standard output
            .env_clear()
            .envs(tool_environment());
        let stream = CaptureStream::spawn(command, FORMAT)?;
        tracing::info!("system audio capture started");
        Ok(stream)
    }
}

/// True when the `pw-dump` JSON lists at least one node of class "Audio/Sink".
fn has_sink(dump: &[u8]) -> bool {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_slice(dump) else { return false };
    objects.iter().any(|object| object["info"]["props"]["media.class"] == "Audio/Sink")
}

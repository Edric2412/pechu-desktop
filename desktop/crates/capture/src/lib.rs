//! Capturing "everything this computer plays" (the sound going to the speakers or headphones), so a meeting held in Zoom,
//! Meet or Teams can be recorded without asking the person to share a tab.
//!
//! The rest of the app only sees the `SystemAudio` trait: ask `available()`, then `start()` to get a `CaptureStream` that hands
//! out raw PCM chunks until it is dropped. What sits behind the trait depends on the operating system:
//!   - Linux: PipeWire's command line tools (`pw-record`), see `linux.rs`.
//!   - Windows: WASAPI loopback, see `windows.rs` (the sound-format maths is in `convert.rs`).
//!   - macOS: not built yet, see `unsupported.rs`.
//!
//! Nothing here ever writes audio to disk.
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

mod convert;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::Pipewire;
#[cfg(not(any(target_os = "linux", windows)))]
pub use unsupported::Unsupported;
#[cfg(windows)]
pub use windows::Wasapi;

/// How the PCM bytes are laid out: interleaved, little-endian, signed 16-bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// One way of capturing the computer's output sound. `Debug` is required only so test settings that hold one can be printed.
pub trait SystemAudio: Send + Sync + std::fmt::Debug {
    /// `Ok` when `start` can be expected to work; otherwise a plain sentence that says why not.
    /// This may run a program and wait for it, so call it from a thread that may block.
    fn available(&self) -> Result<(), String>;
    /// Begins capturing. Must be called inside a tokio runtime. The capture ends when the stream is dropped.
    fn start(&self) -> Result<CaptureStream, String>;
}

/// The capture for this operating system.
pub fn default_backend() -> Arc<dyn SystemAudio> {
    #[cfg(target_os = "linux")]
    return Arc::new(Pipewire::default());
    #[cfg(windows)]
    return Arc::new(Wasapi);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Arc::new(Unsupported);
}

/// How many chunks may wait for a slow reader (about one second) before the capture program is made to wait.
const BACKLOG: usize = 50;

/// What produces the chunks, and so what has to be stopped when the stream is dropped.
// one of these exists per running capture, so the size difference between the variants does not matter
#[allow(clippy::large_enum_variant)]
enum Source {
    /// A program that writes raw PCM to its standard output (Linux).
    #[cfg_attr(windows, allow(dead_code))]
    Process { reader: JoinHandle<()>, child: Child },
    /// A thread of our own that checks the flag now and then and quits when it is set (Windows).
    Thread { stop: Arc<AtomicBool> },
}

/// A running capture, read in small chunks of raw PCM.
/// Dropping it stops the capture at once, so nothing keeps listening when nobody is.
pub struct CaptureStream {
    pub format: CaptureFormat,
    chunks: mpsc::Receiver<Vec<u8>>,
    source: Source,
}

impl CaptureStream {
    /// Starts `command` (which must write raw PCM in `format` to stdout) and reads it in chunks.
    /// The caller has already set the program, arguments and environment.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn spawn(mut command: Command, format: CaptureFormat) -> Result<Self, String> {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| format!("Could not start the sound recorder: {e}"))?;
        let mut stdout = child.stdout.take().ok_or("Could not read from the sound recorder.")?;
        let (tx, chunks) = mpsc::channel(BACKLOG);
        let reader = tokio::spawn(async move {
            // 20 ms per chunk (3840 bytes at 48 kHz stereo): small enough to feel live, big enough not to flood the socket.
            // read_exact keeps every chunk a whole number of frames, so the two channels can never swap places.
            let mut buf = vec![0u8; (format.sample_rate / 50) as usize * format.channels as usize * 2];
            while stdout.read_exact(&mut buf).await.is_ok() {
                if tx.send(buf.clone()).await.is_err() {
                    break; // nobody is listening any more
                }
            }
            // end of data (the program stopped): dropping `tx` tells the reader
        });
        Ok(CaptureStream { format, chunks, source: Source::Process { reader, child } })
    }

    /// Wraps a capture thread that sends finished chunks (in `format`) into `chunks`. `stop` is the flag that thread checks;
    /// dropping the stream sets it, and also drops `chunks`, which makes a thread blocked on a full channel give up.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn from_thread(format: CaptureFormat, chunks: mpsc::Receiver<Vec<u8>>, stop: Arc<AtomicBool>) -> Self {
        CaptureStream { format, chunks, source: Source::Thread { stop } }
    }

    /// The next chunk of sound, or `None` once the capture has stopped.
    pub async fn next_chunk(&mut self) -> Option<Vec<u8>> {
        self.chunks.recv().await
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        match &mut self.source {
            Source::Process { reader, child } => {
                reader.abort();
                // `kill_on_drop` would do this too; saying it here makes the "stops at once" promise easy to see
                let _ = child.start_kill();
            }
            // not joined: if Windows' sound service is stuck, dropping a stream must not freeze the caller
            Source::Thread { stop } => stop.store(true, Ordering::Relaxed),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// The thread-based stream is used on Windows only, but its behaviour is the same everywhere, so it is tested everywhere.
#[cfg(test)]
mod thread_stream_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn dropping_a_thread_stream_stops_the_thread() {
        let format = CaptureFormat { sample_rate: 48_000, channels: 2 };
        let (tx, rx) = mpsc::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (thread_stop, thread_finished) = (stop.clone(), finished.clone());
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if tx.blocking_send(vec![1, 2, 3, 4]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            thread_finished.store(true, Ordering::Relaxed);
        });
        let mut stream = CaptureStream::from_thread(format, rx, stop.clone());
        assert_eq!(stream.format, format);
        assert_eq!(stream.next_chunk().await, Some(vec![1, 2, 3, 4]));
        drop(stream);
        assert!(stop.load(Ordering::Relaxed), "dropping must set the stop flag");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !finished.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(finished.load(Ordering::Relaxed), "the thread must end soon after the drop");
    }
}

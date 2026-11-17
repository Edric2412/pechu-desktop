//! Windows: WASAPI loopback. Windows lets a program "record" an output device: open the default speakers in capture mode with
//! the loopback flag and you receive exactly what is being played. No driver, no extra software, no permission prompt.
//!
//! The work is done by the `wasapi` crate (a safe wrapper around the Windows calls). Its objects belong to the thread that
//! made them (COM, Windows' object system, wants that), so the whole capture lives on one thread of its own, which
//! sends finished 20 ms chunks into the channel that the `CaptureStream` reads. Dropping the stream sets a flag
//! and drops the channel; the thread notices within about 10 ms and lets go of the device.
//!
//! The device's own format is used as it is (shared mode must always accept it) and converted by `convert.rs`.
//! This file has been checked by the compiler for Windows but has not yet been run on a real Windows machine.
use crate::convert::{silence_needed, to_stereo_s16, Chunker, SampleKind};
use crate::{CaptureFormat, CaptureStream, SystemAudio, BACKLOG};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode};

#[derive(Clone, Copy, Debug, Default)]
pub struct Wasapi;

/// Windows asks for the buffer length in units of 100 ns. 200 ms leaves plenty of room for us being a little late.
const BUFFER_100NS: i64 = 2_000_000;
/// How often we look for new sound. This is also how quickly a stop request is noticed.
const POLL: Duration = Duration::from_millis(10);
/// Opening the device should take milliseconds; this only stops a stuck sound service from freezing the app.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

impl SystemAudio for Wasapi {
    fn available(&self) -> Result<(), String> {
        // A short-lived thread of its own, so COM is set up and torn down cleanly whatever the calling thread did before.
        std::thread::spawn(|| {
            let _ = wasapi::initialize_mta();
            let result = default_output_name().map(|_| ());
            wasapi::deinitialize();
            result
        })
        .join()
        .unwrap_or_else(|_| Err("Checking the sound output failed unexpectedly.".to_string()))
    }

    fn start(&self) -> Result<CaptureStream, String> {
        let (tx, chunks) = mpsc::channel(BACKLOG);
        let stop = Arc::new(AtomicBool::new(false));
        // The thread reports back once the device is open (or failed to open), so errors reach the caller right away.
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("pechu-wasapi-capture".into())
            .spawn(move || capture_thread(ready_tx, tx, thread_stop))
            .map_err(|e| format!("Could not start the sound capture: {e}"))?;
        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(format)) => Ok(CaptureStream::from_thread(format, chunks, stop)),
            Ok(Err(message)) => Err(message),
            Err(_) => {
                stop.store(true, Ordering::Relaxed); // if it ever wakes up, it will see this and quit
                Err("The sound output did not answer.".to_string())
            }
        }
    }
}

/// The friendly name of the default output device, or a plain sentence saying there is none.
fn default_output_name() -> Result<String, String> {
    let device = DeviceEnumerator::new()
        .and_then(|e| e.get_default_device(Direction::Render))
        .map_err(|_| "No sound output (speakers or headphones) was found on this computer.".to_string())?;
    Ok(device.get_friendlyname().unwrap_or_default())
}

/// Everything that must be let go of before COM is shut down on this thread.
struct Open {
    client: AudioClient,
    capture: wasapi::AudioCaptureClient,
    kind: SampleKind,
    channels: usize,
    format: CaptureFormat,
}

/// Opens the default output device for loopback in shared mode, in the device's own mix format.
fn open_loopback() -> Result<Open, String> {
    let describe = |what: &str| {
        let what = what.to_string();
        move |e: wasapi::WasapiError| format!("Could not capture the computer's sound ({what}): {e}")
    };
    let device = DeviceEnumerator::new()
        .and_then(|e| e.get_default_device(Direction::Render))
        .map_err(|_| "No sound output (speakers or headphones) was found on this computer.".to_string())?;
    let mut client = device.get_iaudioclient().map_err(describe("open the output"))?;
    let mix = client.get_mixformat().map_err(describe("read its format"))?;
    let is_float = mix.get_subformat().map_err(describe("read its format"))? == SampleType::Float;
    let kind = SampleKind::from_format(is_float, mix.get_bitspersample())
        .ok_or_else(|| format!("This sound output uses a format that is not supported ({} bits).", mix.get_bitspersample()))?;
    // Asking for a Capture stream on a Render device is how loopback is requested; the crate sets the loopback flag for us.
    // Polling (looking every 10 ms) rather than waiting for events: loopback events can stay silent while nothing plays.
    let mode = StreamMode::PollingShared { autoconvert: false, buffer_duration_hns: BUFFER_100NS };
    client.initialize_client(&mix, Direction::Capture, mode, None).map_err(describe("start listening"))?;
    let capture = client.get_audiocaptureclient().map_err(describe("start listening"))?;
    let format = CaptureFormat { sample_rate: mix.get_samplespersec(), channels: 2 };
    Ok(Open { client, capture, kind, channels: mix.get_nchannels() as usize, format })
}

/// The body of the capture thread: set up COM, open the device, report, capture until told to stop, clean up.
fn capture_thread(ready: std_mpsc::SyncSender<Result<CaptureFormat, String>>, tx: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>) {
    let _ = wasapi::initialize_mta();
    match open_loopback() {
        Err(message) => {
            let _ = ready.send(Err(message));
        }
        Ok(open) => {
            // `start` may have given up waiting already; then nobody wants the sound and we just leave.
            if ready.send(Ok(open.format)).is_ok() {
                if let Err(e) = capture_loop(&open, &tx, &stop) {
                    // e.g. the headphones were unplugged: the stream ends and the page is told
                    tracing::warn!("system audio capture stopped: {e}");
                }
            }
            let _ = open.client.stop_stream();
        } // `open` is dropped here, which releases the device
    }
    wasapi::deinitialize();
}

/// Reads sound until `stop` is set or nobody is listening. `Err` means the device stopped working.
fn capture_loop(open: &Open, tx: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool) -> Result<(), String> {
    let err = |e: wasapi::WasapiError| e.to_string();
    let block = open.kind.bytes() * open.channels; // bytes per frame as the device delivers them
    let rate = open.format.sample_rate;
    let mut chunker = Chunker::new(rate);
    let (mut raw, mut stereo) = (Vec::new(), Vec::new());
    open.client.start_stream().map_err(err)?;
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // take every packet that is waiting
        while let Some(frames) = open.capture.get_next_packet_size().map_err(err)? {
            if frames == 0 {
                break;
            }
            raw.resize(frames as usize * block, 0);
            let (read, _) = open.capture.read_from_device(&mut raw).map_err(err)?;
            stereo.clear();
            to_stereo_s16(&raw[..read as usize * block], open.kind, open.channels, &mut stereo);
            chunker.push(&stereo);
        }
        // nothing playing means no packets: fill in the quiet so the recording keeps its place on the clock
        let missing = silence_needed(started.elapsed(), rate, chunker.frames_in());
        if missing > 0 {
            chunker.push_silence(missing);
        }
        while let Some(chunk) = chunker.pop_chunk() {
            // blocks while the reader is slow (like a full pipe would), and fails the moment the stream is dropped
            if tx.blocking_send(chunk).is_err() {
                return Ok(());
            }
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

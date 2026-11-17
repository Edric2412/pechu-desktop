//! "Device audio" for the desktop app: the sound this computer plays, streamed to the page over a WebSocket. The page cannot
//! get it by itself (the web view does not list "Monitor of ..." inputs), so `native-audio.js` asks these two routes instead.
//!   GET /api/native/system-audio/status   `{available: bool, reason?: string}`
//!   GET /api/native/system-audio          WebSocket: first a text message `{"type":"format","sampleRate":48000,"channels":2}`,
//!                                         then binary messages of interleaved little-endian signed 16-bit PCM (about 20 ms each).
//! Both sit behind the same guard as every other /api route (cookie, Host, Origin). Capture runs only while the socket is open
//! and is never written to disk. Only one capture can run at a time.
use crate::api::{json_response, ApiError, ApiResult};
use crate::App;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::StatusCode;
use pechu_capture::SystemAudio;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// Remembers whether a capture is running. The "taken" mark is given back when the guard is dropped.
#[derive(Default)]
pub struct CaptureSlot(Arc<AtomicBool>);

pub struct SlotGuard(Arc<AtomicBool>);

impl CaptureSlot {
    /// None when a capture is already running.
    fn take(&self) -> Option<SlotGuard> {
        (!self.0.swap(true, Ordering::SeqCst)).then(|| SlotGuard(self.0.clone()))
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub async fn status(State(app): State<Arc<App>>) -> ApiResult {
    let backend = app.system_audio.clone();
    // asking may run a program and wait for it, so not on the async threads
    let answer = tokio::task::spawn_blocking(move || backend.available()).await.unwrap_or_else(|_| Err("Could not check.".to_string()));
    Ok(json_response(
        StatusCode::OK,
        match answer {
            Ok(()) => json!({ "available": true }),
            Err(reason) => json!({ "available": false, "reason": reason }),
        },
    ))
}

pub async fn socket(State(app): State<Arc<App>>, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> ApiResult {
    let Ok(upgrade) = upgrade else { return Err(ApiError::new(400, "Expected a WebSocket connection.")) };
    let Some(slot) = app.system_audio_slot.take() else {
        return Err(ApiError::new(409, "System audio is already being captured."));
    };
    let backend = app.system_audio.clone();
    let stop = app.stop.subscribe();
    // The page only ever receives; anything it sends is ignored, so even a small limit is plenty.
    Ok(upgrade.max_message_size(4 * 1024).max_frame_size(4 * 1024).on_upgrade(move |socket| bridge(socket, backend, stop, slot)))
}

/// Captures and forwards until the page leaves, the capture stops, or the app is closing.
async fn bridge(mut socket: WebSocket, backend: Arc<dyn SystemAudio>, mut stop: watch::Receiver<bool>, _slot: SlotGuard) {
    // Capture starts here, because the socket is open; not earlier.
    let mut capture = match backend.start() {
        Ok(capture) => capture,
        Err(reason) => {
            tracing::warn!("system audio could not start: {reason}");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let format = json!({ "type": "format", "sampleRate": capture.format.sample_rate, "channels": capture.format.channels });
    if socket.send(Message::Text(format.to_string().into())).await.is_ok() {
        loop {
            tokio::select! {
                chunk = capture.next_chunk() => {
                    let Some(pcm) = chunk else { break }; // the recorder program stopped
                    if socket.send(Message::Binary(pcm.into())).await.is_err() {
                        break;
                    }
                }
                message = socket.recv() => match message {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break, // the page left
                    Some(Ok(_)) => {} // nothing is expected from the page; pings are answered by the library
                },
                _ = stop.changed() => break, // the app is closing
            }
        }
    }
    drop(capture); // kills the recorder program now, before the slot is given back
    let _ = socket.send(Message::Close(None)).await;
}

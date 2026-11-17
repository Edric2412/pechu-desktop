//! The local HTTP server inside the desktop app. It implements the SAME `/api` contract as the web version (server/index.ts),
//! so the web UI in ../../public runs unchanged, and serves that UI. It listens on 127.0.0.1 only.
//! CONTRACT: the signatures below are used by the app shell (src-tauri). You may add functions and fields; do not change these.
mod api;
mod assets;
mod guard;
mod keys;
mod live;
mod range;
mod security;
mod system_audio;
mod webm;

use assets::UiFiles;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{middleware, Router};
use tower_http::catch_panic::CatchPanicLayer;
use keys::KeyStore;
use live::{LiveUsage, SessionSlots};
use pechu_capture::SystemAudio;
use pechu_jobs::{Endpoints, Jobs, Keys};
use pechu_store::Store;
use security::{RateLimiter, Sessions};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Folder that holds one folder per meeting (created if missing).
    pub data_dir: PathBuf,
    /// Folder for settings and the key fallback file.
    pub config_dir: PathBuf,
    /// Try this port first on 127.0.0.1; if it is taken, use any free one. 0 = any free port.
    pub preferred_port: u16,
    /// Serve the UI from this folder instead of the copy built into the program (development).
    pub static_dir: Option<PathBuf>,
    /// Settings that tests use. Leave `None` in the real app.
    pub test_overrides: Option<TestOverrides>,
}

#[derive(Clone, Debug, Default)]
pub struct TestOverrides {
    pub gemini_base_url: Option<String>,
    pub sarvam_base_url: Option<String>,
    pub sarvam_live_url: Option<String>,
    pub gemini_live_url: Option<String>,
    /// Keep keys in memory only (no OS keychain, no files).
    pub memory_keys: bool,
    /// A Gemini key that is already saved when the server starts (as it is for a person who set Pechu up before).
    pub gemini_key: Option<String>,
}

/// Something the page asks of the window (through the local server, because the page has no native powers of its own).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    /// F11: switch between a normal window and fullscreen.
    ToggleFullscreen,
    /// The window's own buttons (the window has no title bar, so the page draws them).
    Minimize,
    /// Maximize, or restore when it is already maximized.
    ToggleMaximize,
    /// Close the window, which ends the app the same way the old title bar's X did (running jobs finish first).
    Close,
    /// Start moving the window with the mouse (sent when the person presses on the page's drag handle).
    StartDrag,
}

/// Everything the routes share. One `Arc<App>` is handed to every request.
pub(crate) struct App {
    pub store: Store,
    pub jobs: Jobs,
    pub keys: Arc<KeyStore>,
    pub overrides: TestOverrides,
    pub sessions: Sessions,
    pub ui: UiFiles,
    pub port: u16,
    pub csp: String,
    pub started: Instant,
    /// Set when the app is closing: new requests are turned away.
    pub stopping: AtomicBool,
    /// Tells open live-caption sockets (and the server itself) to end.
    pub stop: watch::Sender<bool>,
    pub api_limit: RateLimiter,
    pub upload_limit: RateLimiter,
    pub live_limit: RateLimiter,
    pub live_slots: Arc<SessionSlots>,
    pub live_usage: Arc<LiveUsage>,
    /// How the computer's own sound is captured ("Device audio"), and the mark that only one capture runs at a time.
    pub system_audio: Arc<dyn SystemAudio>,
    pub system_audio_slot: system_audio::CaptureSlot,
    /// The notes model found for one key (key, model).
    pub model_cache: Mutex<Option<(String, String)>>,
    /// Requests for the window shell (see `Running::window_commands`). Nobody listening is fine.
    pub window_tx: tokio::sync::broadcast::Sender<WindowCommand>,
}

/// A running server.
pub struct Running {
    pub port: u16,
    /// Single-use token: the first request to `/?k=<token>` (from the window) sets the session cookie and redirects to `/`.
    pub launch_token: String,
    app: Arc<App>,
    server: JoinHandle<()>,
}

impl Running {
    /// What the page asks of the window (F11 for fullscreen). The window shell listens here; each call gives its own receiver.
    pub fn window_commands(&self) -> tokio::sync::broadcast::Receiver<WindowCommand> {
        self.app.window_tx.subscribe()
    }

    /// Stops accepting requests, waits for running jobs, then returns.
    pub async fn shutdown(self) {
        self.app.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.app.stop.send(true); // ends the live sockets and tells the server to stop accepting
        let mut server = self.server;
        // a request that is still running gets a few seconds, then the connection is dropped
        if tokio::time::timeout(Duration::from_secs(5), &mut server).await.is_err() {
            server.abort();
        }
        // A transcription should not be cut in half just because the window closed.
        if tokio::time::timeout(Duration::from_secs(30), self.app.jobs.wait_idle()).await.is_err() {
            tracing::warn!("closing with {} job(s) still running", self.app.jobs.active_jobs());
        }
    }
}

/// Starts the server and returns once it is listening.
pub async fn serve(cfg: ServeConfig) -> anyhow::Result<Running> {
    serve_with(cfg, pechu_capture::default_backend()).await
}

/// Like `serve`, with a chosen way of capturing the computer's own sound (tests give it a fake one).
pub async fn serve_with(cfg: ServeConfig, system_audio: Arc<dyn SystemAudio>) -> anyhow::Result<Running> {
    let overrides = cfg.test_overrides.clone().unwrap_or_default();

    let store = Store::new(cfg.data_dir.clone());
    let interrupted = store.fail_interrupted();
    if interrupted > 0 {
        tracing::info!("{interrupted} meeting(s) were interrupted by the last shutdown");
    }

    // Reading the keychain can take a moment, so it is not done on the async threads.
    let (config_dir, memory_keys) = (cfg.config_dir.clone(), overrides.memory_keys);
    let keys = Arc::new(tokio::task::spawn_blocking(move || KeyStore::open(config_dir, memory_keys)).await?);
    if let Some(key) = &overrides.gemini_key {
        keys.save_gemini(key)?; // tests: the key a person saved on an earlier day
    }
    let jobs = Jobs::new(
        store.clone(),
        keys.clone() as Arc<dyn Keys>,
        Endpoints { gemini_base_url: overrides.gemini_base_url.clone(), sarvam_base_url: overrides.sarvam_base_url.clone() },
    );

    // The preferred port keeps the address the same between launches; any free port is fine when it is taken.
    let listener = match cfg.preferred_port {
        0 => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
        port => match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
        },
    };
    let port = listener.local_addr()?.port();

    let launch_token = security::random_hex(32);
    let (stop, mut stop_rx) = watch::channel(false);
    let app = Arc::new(App {
        store,
        jobs,
        keys,
        overrides,
        sessions: Sessions::new(launch_token.clone()),
        ui: match &cfg.static_dir {
            Some(dir) => UiFiles::Folder(dir.clone()),
            None => UiFiles::Embedded,
        },
        port,
        csp: security::csp(port),
        started: Instant::now(),
        stopping: AtomicBool::new(false),
        stop,
        api_limit: RateLimiter::new(600, Duration::from_secs(60)),
        upload_limit: RateLimiter::new(30, Duration::from_secs(3600)),
        live_limit: RateLimiter::new(20, Duration::from_secs(60)),
        live_slots: SessionSlots::new(3),
        live_usage: Arc::new(LiveUsage::default()),
        system_audio,
        system_audio_slot: system_audio::CaptureSlot::default(),
        model_cache: Mutex::new(None),
        window_tx: tokio::sync::broadcast::channel(8).0,
    });

    // A saved key means the page's first call (/api/status) wants the notes model, which Google has to look up. Start that now,
    // in the background, so it is usually ready by then; the status call itself never waits long for it (see `api::status`).
    if app.keys.gemini_key().is_some() {
        let warm_up = app.clone();
        tokio::spawn(async move {
            let _ = warm_up.notes_model().await;
        });
    }

    let routes = router(app.clone());
    let server = tokio::spawn(async move {
        let stop_accepting = async move {
            let _ = stop_rx.changed().await;
        };
        if let Err(e) = axum::serve(listener, routes).with_graceful_shutdown(stop_accepting).await {
            tracing::error!("the server stopped: {e}");
        }
    });
    tracing::info!("Pechu server listening on 127.0.0.1:{port}");
    Ok(Running { port, launch_token, app, server })
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/status", get(api::status))
        .route("/api/key", post(api::post_key))
        .route("/api/live", get(api::live_socket))
        .route("/api/native/system-audio/status", get(system_audio::status))
        .route("/api/native/system-audio", get(system_audio::socket))
        .route("/api/uploads", post(api::post_upload))
        .route("/api/auth", get(api::auth))
        .route("/api/native/probe", post(api::probe))
        .route("/api/native/window/{command}", post(api::window_command))
        .route("/api/login", post(api::login_or_logout))
        .route("/api/logout", post(api::login_or_logout))
        .route("/api/meetings", get(api::list_meetings).post(api::create_meeting))
        .route("/api/meetings/{id}", get(api::get_meeting).patch(api::patch_meeting).delete(api::delete_meeting))
        .route("/api/meetings/{id}/audio", put(api::put_audio).get(api::get_audio))
        .route("/api/meetings/{id}/notes", post(api::post_notes))
        .route("/api/meetings/{id}/flow", post(api::post_flow))
        .route("/api/meetings/{id}/transcribe", post(api::post_transcribe))
        // anything else: an unknown /api path is a JSON 404 (also for a wrong method), everything else is the UI
        .method_not_allowed_fallback(api::not_found_or_page)
        .fallback(api::not_found_or_page)
        // uploads are limited by our own code; this only keeps the multipart reader from stopping at axum's 2 MB default
        .layer(DefaultBodyLimit::max((api::MAX_BYTES + 6 * 1024 * 1024) as usize))
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(middleware::from_fn_with_state(app.clone(), guard::guard))
        .with_state(app)
}

/// A bug in a route must not drop the connection: the page gets the same plain sentence as for any other server error.
fn panic_response(_what: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a route panicked");
    api::ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on the server.".to_string()).into_response()
}

//! The Pechu desktop app shell: starts the local server, then opens a window on it.
//! The window shows the same web UI as the browser version; all work goes through the local HTTP API.
// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod settings;
mod spike;
mod webview;

use anyhow::Context;
use pechu_server::{Running, ServeConfig};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, RunEvent, Url};

/// Holds the running server so we can stop it cleanly when the app closes.
/// `Option` because shutting down needs to take it out (it is used up by `shutdown`).
struct ServerState(Mutex<Option<Running>>);

/// An environment variable as a folder path, if it is set and not empty.
fn path_from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Brings the existing window to the front (used when the user starts the app a second time).
fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Starts the server, then opens the window. The window comes second so it never shows a dead page.
async fn start(app: AppHandle) -> anyhow::Result<()> {
    let data_dir = path_from_env("PECHU_DATA_DIR").unwrap_or_else(|| {
        dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Pechu").join("meetings")
    });
    let config_dir = path_from_env("PECHU_CONFIG_DIR")
        .unwrap_or_else(|| dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("Pechu"));

    let mut saved = settings::load(&config_dir);
    let running = pechu_server::serve(ServeConfig {
        data_dir,
        config_dir: config_dir.clone(),
        preferred_port: saved.preferred_port,
        // Developers can run the UI straight from ../public without rebuilding.
        static_dir: path_from_env("PECHU_STATIC_DIR"),
        test_overrides: None,
    })
    .await
    .context("could not start the local server")?;

    // The preferred port was taken: the server chose another one, so remember it.
    if running.port != saved.preferred_port {
        saved.preferred_port = running.port;
        if let Err(err) = settings::save(&config_dir, &saved) {
            eprintln!("could not save settings: {err}");
        }
    }

    let port = running.port;
    let url = Url::parse(&format!("http://127.0.0.1:{port}/?k={}", running.launch_token))?;
    // F11 and other requests from the page reach the window through here (the receiver must exist before `running` is moved).
    let mut window_commands = running.window_commands();
    app.manage(ServerState(Mutex::new(Some(running))));
    webview::create_main_window(&app, url, port)?;

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match window_commands.recv().await {
                Ok(command) => webview::run_window_command(&handle, command),
                Err(RecvError::Lagged(_)) => continue, // a burst of requests: nothing is lost that matters
                Err(RecvError::Closed) => break,       // the server has stopped
            }
        }
    });
    Ok(())
}

/// Spike mode (`PECHU_SPIKE=1`): a throwaway page that tests audio in the webview.
async fn start_spike(app: AppHandle) -> anyhow::Result<()> {
    let port = spike::start().await?;
    let url = Url::parse(&format!("http://127.0.0.1:{port}/"))?;
    webview::create_main_window(&app, url, port)?;
    Ok(())
}

fn main() {
    let spike_mode = std::env::var("PECHU_SPIKE").is_ok_and(|v| v == "1");

    let app = tauri::Builder::default()
        // Must be the first plugin: a second launch ends here and just focuses the first window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| focus_main_window(app)))
        .setup(move |app| {
            let handle = app.handle().clone();
            // Server and window are created in the background so setup returns quickly.
            tauri::async_runtime::spawn(async move {
                let result = if spike_mode { start_spike(handle.clone()).await } else { start(handle.clone()).await };
                if let Err(err) = result {
                    eprintln!("Pechu could not start: {err:#}");
                    handle.exit(1);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not build the Pechu app");

    app.run(|app, event| {
        // The window was closed (or exit was requested): stop the server first.
        if let RunEvent::ExitRequested { api, .. } = event {
            let Some(state) = app.try_state::<ServerState>() else { return };
            // Taking the server out means the second ExitRequested (from `exit` below) finds nothing and goes through.
            let running = state.0.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(running) = running {
                api.prevent_exit();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    running.shutdown().await; // waits for running jobs, up to 30 s
                    handle.exit(0);
                });
            }
        }
    });
}

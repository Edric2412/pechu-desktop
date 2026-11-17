//! Rules for the window's web page: which origin it may use, who may use the microphone,
//! and where links to other sites go. Kept in one place so the security rules are easy to find.
use tauri::{Url, WebviewWindow};

/// The only address the window may show: our own local server.
fn our_origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// True when `url` belongs to our local server (blob: URLs made by our own page count too).
pub fn is_our_origin(url: &Url, port: u16) -> bool {
    url.origin().ascii_serialization() == our_origin(port)
}

/// The microphone rule. Allowed only if ALL of these hold:
/// the page is ours, the request is for audio, and it is NOT for a camera or screen.
/// Pure function (no window needed) so it can be tested.
// Only the Linux webview hook calls it; elsewhere WebView2 asks the user itself, so it would be reported as unused.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn microphone_allowed(page_uri: Option<&str>, port: u16, audio: bool, video: bool) -> bool {
    let page_is_ours = page_uri
        .and_then(|uri| Url::parse(uri).ok())
        .is_some_and(|url| is_our_origin(&url, port));
    page_is_ours && audio && !video
}

/// Does what the page asked of the window (the page cannot do it itself, see `pechu_server::WindowCommand`).
pub fn run_window_command(app: &tauri::AppHandle, command: pechu_server::WindowCommand) {
    use tauri::Manager;
    let Some(window) = app.get_webview_window("main") else { return };
    use pechu_server::WindowCommand::*;
    let result = match command {
        ToggleFullscreen => window.set_fullscreen(!window.is_fullscreen().unwrap_or(false)),
        Minimize => window.minimize(),
        ToggleMaximize => {
            if window.is_maximized().unwrap_or(false) { window.unmaximize() } else { window.maximize() }
        }
        // closing the last window ends the app, and `main.rs` then waits for running jobs before it exits
        Close => window.close(),
        StartDrag => window.start_dragging(),
    };
    if let Err(err) = result {
        eprintln!("the window could not do {command:?}: {err}");
    }
}

/// Opens a link to another site in the system browser. Only http(s) links are passed on,
/// so a page can never make the app launch other programs.
pub fn open_in_browser(url: &Url) {
    if url.scheme() != "http" && url.scheme() != "https" {
        return;
    }
    if let Err(err) = tauri_plugin_opener::open_url(url.as_str(), None::<&str>) {
        eprintln!("could not open {url} in the browser: {err}");
    }
}

/// Creates the "main" window showing `url` (an address on our own server at `port`).
/// Navigation is locked: only our origin may load inside the window; links to other
/// sites open in the system browser instead (the UI uses target=_blank for those).
pub fn create_main_window(
    app: &tauri::AppHandle,
    url: Url,
    port: u16,
) -> tauri::Result<WebviewWindow> {
    use tauri::webview::NewWindowResponse;

    let window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
        .title("Pechu")
        // No title bar: the page fills the whole window. (So there are no minimize/maximize/close buttons: close with Alt+F4,
        // move with Super+drag, fullscreen with F11, see `run_window_command`.)
        .decorations(false)
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .on_navigation(move |target| {
            if is_our_origin(target, port) {
                return true;
            }
            open_in_browser(target);
            false
        })
        .on_new_window(move |target, _features| {
            // target=_blank links and window.open() end up here.
            if !is_our_origin(&target, port) {
                open_in_browser(&target);
            }
            NewWindowResponse::Deny
        })
        .build()?;
    setup_media(&window, port);
    Ok(window)
}

/// Linux only: tune WebKitGTK for recording and decide who gets the microphone.
/// (Tauri's own permission hook does not tell us which page asked, so we use WebKit's signal directly.)
#[cfg(target_os = "linux")]
pub fn setup_media(window: &WebviewWindow, port: u16) {
    use webkit2gtk::glib::prelude::Cast;
    use webkit2gtk::{
        PermissionRequestExt, SettingsExt, UserMediaPermissionRequest,
        UserMediaPermissionRequestExt, WebViewExt,
    };

    let result = window.with_webview(move |platform| {
        let webview = platform.inner();

        // Turn on the browser features that recording needs.
        if let Some(settings) = WebViewExt::settings(&webview) {
            settings.set_enable_media_stream(true);
            settings.set_enable_media(true);
            settings.set_enable_mediasource(true);
            settings.set_enable_media_capabilities(true);
            settings.set_enable_webaudio(true);
        }

        // Every permission request is answered here: microphone for our page, no for everything else.
        webview.connect_permission_request(move |view, request| {
            let allow = match request.downcast_ref::<UserMediaPermissionRequest>() {
                Some(media) => microphone_allowed(
                    view.uri().as_deref(),
                    port,
                    media.is_for_audio_device(),
                    media.is_for_video_device(),
                ),
                None => false,
            };
            if allow {
                request.allow();
            } else {
                request.deny();
            }
            true // we handled it: WebKit must not show its own prompt
        });
    });
    if let Err(err) = result {
        eprintln!("could not set up the webview: {err}");
    }
}

/// Other systems: the webview's default rules apply (the real shell is built and tested on Linux first).
#[cfg(not(target_os = "linux"))]
pub fn setup_media(_window: &WebviewWindow, _port: u16) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_only_for_our_page_and_audio_only() {
        let ours = Some("http://127.0.0.1:47317/#/record");
        assert!(microphone_allowed(ours, 47317, true, false));
        assert!(!microphone_allowed(ours, 47317, true, true)); // camera too
        assert!(!microphone_allowed(ours, 47317, false, false)); // screen
        assert!(!microphone_allowed(ours, 5000, true, false)); // other port
        assert!(!microphone_allowed(Some("https://evil.example/"), 47317, true, false));
        assert!(!microphone_allowed(Some("http://127.0.0.1:47317.evil.example/"), 47317, true, false));
        assert!(!microphone_allowed(None, 47317, true, false));
    }

    #[test]
    fn origin_check_accepts_blob_urls_of_our_page() {
        let blob = Url::parse("blob:http://127.0.0.1:47317/abc-123").unwrap();
        assert!(is_our_origin(&blob, 47317));
        let other = Url::parse("https://aistudio.google.com/apikey").unwrap();
        assert!(!is_our_origin(&other, 47317));
    }
}

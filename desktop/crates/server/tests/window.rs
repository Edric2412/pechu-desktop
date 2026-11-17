//! F11 in the desktop window: the page cannot call native commands (by design), so a small script asks the local server
//! (`POST /api/native/window/fullscreen`) and the server hands the request to the window shell through `window_commands()`.
//! Proved here: the request reaches the shell, it needs the session cookie, and a cross-site style plain-text POST is refused.
use pechu_server::{serve, Running, ServeConfig, TestOverrides, WindowCommand};
use std::time::Duration;
use tokio::sync::broadcast::error::TryRecvError;

struct Harness {
    running: Running,
    cookie: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let running = serve(ServeConfig {
        data_dir: dir.path().join("meetings"),
        config_dir: dir.path().join("config"),
        preferred_port: 0,
        static_dir: None,
        test_overrides: Some(TestOverrides { memory_keys: true, ..Default::default() }),
    })
    .await
    .expect("the server starts");
    // the window's first request: it trades the one-time token for the session cookie
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
    let res = http.get(format!("http://127.0.0.1:{}/?k={}", running.port, running.launch_token)).send().await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    Harness { running, cookie, http, _dir: dir }
}

impl Harness {
    fn post(&self, content_type: &str, with_cookie: bool) -> reqwest::RequestBuilder {
        self.post_to("fullscreen", content_type, with_cookie)
    }

    fn post_to(&self, command: &str, content_type: &str, with_cookie: bool) -> reqwest::RequestBuilder {
        let req = self
            .http
            .post(format!("http://127.0.0.1:{}/api/native/window/{command}", self.running.port))
            .header("content-type", content_type)
            .body("{}");
        if with_cookie { req.header("cookie", &self.cookie) } else { req }
    }
}

#[tokio::test]
async fn the_f11_request_reaches_the_window_shell() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    let res = h.post("application/json", true).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let got = tokio::time::timeout(Duration::from_secs(1), commands.recv()).await.expect("a command in time").unwrap();
    assert_eq!(got, WindowCommand::ToggleFullscreen);
}

#[tokio::test]
async fn every_window_button_reaches_the_window_shell() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    for (name, expected) in [
        ("fullscreen", WindowCommand::ToggleFullscreen),
        ("minimize", WindowCommand::Minimize),
        ("maximize", WindowCommand::ToggleMaximize),
        ("close", WindowCommand::Close),
        ("drag", WindowCommand::StartDrag),
    ] {
        let res = h.post_to(name, "application/json", true).send().await.unwrap();
        assert_eq!(res.status(), 200, "{name}");
        let got = tokio::time::timeout(Duration::from_secs(1), commands.recv()).await.expect("a command in time").unwrap();
        assert_eq!(got, expected, "{name}");
    }
}

#[tokio::test]
async fn an_unknown_window_command_is_refused_and_nothing_reaches_the_shell() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    assert_eq!(h.post_to("explode", "application/json", true).send().await.unwrap().status(), 404);
    assert_eq!(h.post_to("close/../x", "application/json", true).send().await.unwrap().status(), 404);
    assert_eq!(commands.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[tokio::test]
async fn closing_the_window_needs_the_session_cookie_too() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    assert_eq!(h.post_to("close", "application/json", false).send().await.unwrap().status(), 401);
    assert_eq!(h.post_to("close", "text/plain", true).send().await.unwrap().status(), 415);
    assert_eq!(commands.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[tokio::test]
async fn without_the_session_cookie_nothing_happens() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    assert_eq!(h.post("application/json", false).send().await.unwrap().status(), 401);
    assert_eq!(commands.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[tokio::test]
async fn a_plain_text_post_is_refused_like_every_other_post() {
    let h = start().await;
    let mut commands = h.running.window_commands();
    assert_eq!(h.post("text/plain", true).send().await.unwrap().status(), 415);
    assert_eq!(commands.try_recv().unwrap_err(), TryRecvError::Empty);
}

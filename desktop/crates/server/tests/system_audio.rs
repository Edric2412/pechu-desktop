//! The "Device audio" routes (/api/native/system-audio): who may use them, what the page receives, that only one capture runs at a
//! time, and that the recorder program is killed the moment the page leaves. The real PipeWire tools are replaced by two tiny
//! shell scripts, so nothing is recorded from this computer.
#![cfg(target_os = "linux")]
use futures_util::StreamExt;
use pechu_capture::Pipewire;
use pechu_server::{serve_with, Running, ServeConfig, TestOverrides};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// A fake `pw-record`: answers --version, otherwise writes silence in 20 ms chunks forever and remembers its own process id.
fn write_fake_tools(dir: &Path) -> Pipewire {
    let pid_file = dir.join("recorder.pid");
    let record = dir.join("fake-pw-record");
    std::fs::write(
        &record,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'pw-record fake'; exit 0; fi\necho $$ > '{}'\nwhile true; do head -c 3840 /dev/zero; sleep 0.02; done\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let dump = dir.join("fake-pw-dump");
    std::fs::write(&dump, "#!/bin/sh\necho '[{\"info\":{\"props\":{\"media.class\":\"Audio/Sink\"}}}]'\n").unwrap();
    for file in [&record, &dump] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Pipewire { record_program: record.into(), dump_program: dump.into(), tool_timeout: Duration::from_secs(5) }
}

struct Harness {
    _running: Running,
    port: u16,
    cookie: String,
    dir: tempfile::TempDir,
}

async fn start(backend: impl FnOnce(&Path) -> Pipewire) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(dir.path());
    let running = serve_with(
        ServeConfig {
            data_dir: dir.path().join("meetings"),
            config_dir: dir.path().join("config"),
            preferred_port: 0,
            static_dir: None,
            test_overrides: Some(TestOverrides { memory_keys: true, ..Default::default() }),
        },
        Arc::new(backend),
    )
    .await
    .expect("the server starts");
    // the window's first request: it trades the one-time token for the session cookie
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
    let res = http.get(format!("http://127.0.0.1:{}/?k={}", running.port, running.launch_token)).send().await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    Harness { port: running.port, _running: running, cookie, dir }
}

impl Harness {
    async fn status(&self, with_cookie: bool) -> (u16, Value) {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut req = http.get(format!("http://127.0.0.1:{}/api/native/system-audio/status", self.port));
        if with_cookie {
            req = req.header("cookie", &self.cookie);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    async fn socket(
        &self,
        with_cookie: bool,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, u16> {
        let mut request = format!("ws://127.0.0.1:{}/api/native/system-audio", self.port).into_client_request().unwrap();
        if with_cookie {
            request.headers_mut().insert("cookie", self.cookie.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(res)) => Err(res.status().as_u16()),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    fn recorder_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.path().join("recorder.pid")).ok()?.trim().parse().ok()
    }
}

/// Alive means running: a process that was killed but not yet collected (a zombie) counts as gone.
fn is_running(pid: u32) -> bool {
    let stat = std::fs::read_to_string(PathBuf::from(format!("/proc/{pid}/stat"))).unwrap_or_default();
    // the state letter is the first field after the ")" that ends the program name
    matches!(stat.rsplit(')').next().and_then(|rest| rest.trim().chars().next()), Some(c) if c != 'Z' && c != 'X')
}

async fn next(ws: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> WsMessage {
    tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("a message in time").expect("the socket is open").unwrap()
}

#[tokio::test]
async fn the_routes_are_closed_without_the_session_cookie() {
    let h = start(write_fake_tools).await;
    assert_eq!(h.status(false).await.0, 401);
    assert_eq!(h.socket(false).await.err(), Some(401));
    assert_eq!(h.recorder_pid(), None, "nothing was recorded");
}

#[tokio::test]
async fn status_says_whether_the_computer_can_be_captured_and_why_not() {
    let h = start(write_fake_tools).await;
    assert_eq!(h.status(true).await, (200, serde_json::json!({ "available": true })));

    let missing = start(|dir| Pipewire { record_program: dir.join("no-such-program").into(), ..write_fake_tools(dir) }).await;
    let (code, body) = missing.status(true).await;
    assert_eq!(code, 200);
    assert_eq!(body["available"], false);
    assert!(body["reason"].as_str().unwrap().contains("PipeWire"), "{body}");
}

#[tokio::test]
async fn the_page_gets_the_format_then_20_ms_chunks_of_sound() {
    let h = start(write_fake_tools).await;
    let mut ws = h.socket(true).await.expect("connected");
    let WsMessage::Text(first) = next(&mut ws).await else { panic!("the first message is the format") };
    let format: Value = serde_json::from_str(first.as_str()).unwrap();
    assert_eq!(format, serde_json::json!({ "type": "format", "sampleRate": 48000, "channels": 2 }));
    for _ in 0..3 {
        let WsMessage::Binary(pcm) = next(&mut ws).await else { panic!("then sound") };
        assert_eq!(pcm.len(), 3840); // 960 frames x 2 channels x 2 bytes = 20 ms
    }
}

#[tokio::test]
async fn only_one_capture_runs_at_a_time_and_the_recorder_dies_when_the_page_leaves() {
    let h = start(write_fake_tools).await;
    let mut first = h.socket(true).await.expect("connected");
    next(&mut first).await; // the format message: the recorder is running
    next(&mut first).await; // some sound
    let pid = h.recorder_pid().expect("the recorder wrote its process id");
    assert!(is_running(pid));

    assert_eq!(h.socket(true).await.err(), Some(409), "a second capture is refused");
    assert!(is_running(pid), "and it does not disturb the first one");

    drop(first); // the page leaves without saying goodbye
    let mut waited = 0;
    while is_running(pid) {
        assert!(waited < 100, "the recorder program was still running 5 s after the page left");
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }

    // the slot is free again
    let again = h.socket(true).await;
    assert!(again.is_ok(), "a new capture can start after the first ended");
}

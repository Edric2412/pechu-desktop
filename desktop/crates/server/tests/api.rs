//! Starts the real server on a free port (keys in memory, fake Gemini on this computer) and talks to it over HTTP, the way
//! the web UI does. Expectations are ported from tests/server.e2e.test.ts, tests/second-pass.e2e.test.ts and tests/notes-v2.test.ts.
//! Some tests also need the other crates (store, jobs, gemini, live) to be finished.
use axum::http::HeaderMap;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use pechu_server::{serve, Running, ServeConfig, TestOverrides};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------- fakes ----------

/// A stand-in for Google's API: "good-key" is accepted, any other key is refused.
async fn fake_gemini() -> String {
    let app = axum::Router::new().route(
        "/models",
        get(|headers: HeaderMap| async move {
            let good = headers.get("x-goog-api-key").is_some_and(|k| k == "good-key");
            if good {
                (
                    StatusCode::OK,
                    axum::Json(json!({ "models": [{ "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"] }] })),
                )
            } else {
                (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": { "message": "API key not valid" } })))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

/// A stand-in for Gemini's live captions: says "ready" at once, answers the first audio with a partial and a final text, and
/// ends the call normally when the audio stream ends. Returns the ws:// address.
async fn fake_gemini_live() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                let mut answered = false;
                while let Some(Ok(msg)) = ws.next().await {
                    let WsMessage::Text(text) = msg else { continue };
                    let text = text.as_str();
                    if text.contains("\"setup\"") {
                        let _ = ws.send(WsMessage::text(json!({ "setupComplete": {} }).to_string())).await;
                    } else if text.contains("audioStreamEnd") {
                        let _ = ws.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }))).await;
                    } else if text.contains("realtimeInput") && !answered {
                        answered = true;
                        let interim = json!({ "serverContent": { "interimInputTranscription": { "text": "hel" } } });
                        let fin = json!({ "serverContent": { "inputTranscription": { "text": "hello there" } } });
                        let _ = ws.send(WsMessage::text(interim.to_string())).await;
                        let _ = ws.send(WsMessage::text(fin.to_string())).await;
                    }
                }
            });
        }
    });
    format!("ws://127.0.0.1:{port}")
}

// ---------- the server under test ----------

struct Harness {
    _running: Running,
    port: u16,
    token: String,
    http: reqwest::Client,
    cookie: Option<String>,
    _dirs: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(Some(fake_gemini().await), None).await
    }

    async fn start_with(gemini_base_url: Option<String>, gemini_live_url: Option<String>) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let running = serve(ServeConfig {
            data_dir: dirs.path().join("meetings"),
            config_dir: dirs.path().join("config"),
            preferred_port: 0,
            static_dir: None,
            test_overrides: Some(TestOverrides {
                gemini_base_url,
                // nothing may ever reach the real services from a test
                sarvam_base_url: Some("http://127.0.0.1:9".into()),
                sarvam_live_url: Some("ws://127.0.0.1:9".into()),
                gemini_live_url,
                memory_keys: true,
                gemini_key: None,
            }),
        })
        .await
        .expect("the server starts");
        Harness {
            port: running.port,
            token: running.launch_token.clone(),
            _running: running,
            http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap(),
            cookie: None,
            _dirs: dirs,
        }
    }

    /// Opens the app window the way the shell does: /?k=<token>, and keeps the cookie it gets.
    async fn signed_in(mut self) -> Self {
        let res = self.http.get(self.url(&format!("/?k={}", self.token))).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::FOUND);
        let set = res.headers().get("set-cookie").expect("a session cookie").to_str().unwrap().to_string();
        self.cookie = Some(set.split(';').next().unwrap().to_string());
        self
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        let r = self.http.request(method, self.url(path));
        match &self.cookie {
            Some(c) => r.header("cookie", c),
            None => r,
        }
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.req(Method::GET, path)
    }

    fn send_json(&self, method: Method, path: &str, body: Value) -> RequestBuilder {
        self.req(method, path).json(&body)
    }

    async fn add_gemini_key(&self) {
        let res = self.send_json(Method::POST, "/api/key", json!({ "key": "good-key" })).send().await.unwrap();
        assert_eq!(res.status(), 200, "the fake Google accepts good-key");
    }

    async fn new_meeting(&self) -> String {
        let res = self.send_json(Method::POST, "/api/meetings", json!({ "transcript": "hello there", "title": "T" })).send().await.unwrap();
        body(res).await["id"].as_str().unwrap().to_string()
    }
}

async fn body(res: Response) -> Value {
    res.json().await.expect("a JSON body")
}

async fn error_of(res: Response, status: u16) -> String {
    assert_eq!(res.status().as_u16(), status);
    body(res).await["error"].as_str().expect("an error text").to_string()
}

fn bytes_pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

// ---------- sign-in, host and headers ----------

#[tokio::test]
async fn health_and_auth_need_no_cookie_but_the_api_does() {
    let h = Harness::start().await;
    assert_eq!(h.get("/healthz").send().await.unwrap().status(), 200);
    let auth = body(h.get("/api/auth").send().await.unwrap()).await;
    assert_eq!(auth, json!({ "authRequired": false, "authed": true }));
    assert_eq!(body(h.req(Method::POST, "/api/login").send().await.unwrap()).await, json!({ "ok": true }));
    assert_eq!(body(h.req(Method::POST, "/api/logout").send().await.unwrap()).await, json!({ "ok": true }));

    let res = h.get("/api/meetings").send().await.unwrap();
    assert_eq!(error_of(res, 401).await, "Please sign in.");
    // the socket needs the cookie too
    let res = h.get("/api/live").send().await.unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn the_launch_token_signs_in_once() {
    let h = Harness::start().await;
    // a wrong token is refused and does not use up the real one
    let wrong = h.get("/?k=not-the-token").send().await.unwrap();
    assert_eq!(wrong.status(), 403);
    assert!(wrong.headers().get("set-cookie").is_none());

    let first = h.get(&format!("/?k={}", h.token)).send().await.unwrap();
    assert_eq!(first.status(), 302);
    assert_eq!(first.headers()["location"], "/");
    let set = first.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly"), "{set}");
    assert!(set.contains("SameSite=Strict"), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();

    // the cookie opens the API
    let list = h.get("/api/status").header("cookie", &cookie).send().await.unwrap();
    assert_eq!(list.status(), 200);

    // the same address again, without the cookie: refused
    let replay = h.get(&format!("/?k={}", h.token)).send().await.unwrap();
    assert_eq!(replay.status(), 403);
    assert!(replay.headers().get("set-cookie").is_none());
    // ... and it did not sign anyone in
    assert_eq!(h.get("/api/meetings").send().await.unwrap().status(), 401);
    // a reload of that address from the signed-in window just goes to the page
    let reload = h.get(&format!("/?k={}", h.token)).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(reload.status(), 302);
    assert!(reload.headers().get("set-cookie").is_none());
    // a made-up cookie is no good
    let fake = h.get("/api/meetings").header("cookie", "pechu_session=abc123").send().await.unwrap();
    assert_eq!(fake.status(), 401);
}

#[tokio::test]
async fn wrong_host_and_wrong_origin_are_refused() {
    let h = Harness::start().await.signed_in().await;
    let res = h.get("/api/meetings").header("host", "evil.example.net").send().await.unwrap();
    assert_eq!(error_of(res, 403).await, "Not allowed.");
    let page = h.get("/").header("host", "evil.example.net").send().await.unwrap();
    assert_eq!(page.status(), 403);
    assert_eq!(page.text().await.unwrap(), "Not allowed.");
    let wrong_port = h.get("/api/meetings").header("host", "127.0.0.1:1").send().await.unwrap();
    assert_eq!(wrong_port.status(), 403);

    // another website, and another port of this same host, are other sites
    for origin in ["https://evil.example.net".to_string(), "http://127.0.0.1:3000".to_string(), "null".to_string()] {
        let res = h.get("/api/meetings").header("origin", origin).send().await.unwrap();
        assert_eq!(res.status(), 403);
    }
    let same = h.get("/api/meetings").header("origin", format!("http://127.0.0.1:{}", h.port)).send().await.unwrap();
    assert_eq!(same.status(), 200);
    // the browser says the request came from another site
    let cross = h.get("/api/meetings").header("sec-fetch-site", "cross-site").send().await.unwrap();
    assert_eq!(cross.status(), 403);
    let same_site = h.get("/api/meetings").header("sec-fetch-site", "same-origin").send().await.unwrap();
    assert_eq!(same_site.status(), 200);
    // a "localhost" Host works as well
    let localhost = h.get("/api/meetings").header("host", format!("localhost:{}", h.port)).send().await.unwrap();
    assert_eq!(localhost.status(), 200);
}

// Found by the contract tests (tests/contract): the web server answers these two requests this way.
#[tokio::test]
async fn readyz_needs_no_cookie_and_an_empty_meeting_id_is_a_meeting_not_found() {
    let h = Harness::start().await;
    let res = h.get("/readyz").send().await.unwrap(); // before signing in
    assert_eq!(res.status(), 200);
    assert_eq!(body(res).await, json!({ "ok": true }));
    let h = h.signed_in().await;
    for method in [Method::GET, Method::DELETE, Method::PATCH] {
        let res = h.req(method, "/api/meetings/").send().await.unwrap();
        assert_eq!(error_of(res, 404).await, "Meeting not found.");
    }
    let res = h.get("/api/nothing-here/").send().await.unwrap();
    assert_eq!(error_of(res, 404).await, "Not found.");
}

#[tokio::test]
async fn every_answer_carries_the_security_headers() {
    let h = Harness::start().await;
    for res in [h.get("/").send().await.unwrap(), h.get("/api/meetings").send().await.unwrap(), h.get("/healthz").send().await.unwrap()] {
        let csp = res.headers()["content-security-policy"].to_str().unwrap().to_string();
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains(&format!("ws://127.0.0.1:{}", h.port)));
        assert!(!csp.contains("google"));
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["x-frame-options"], "DENY");
        assert_eq!(res.headers()["referrer-policy"], "no-referrer");
    }
}

// ---------- the page ----------

#[tokio::test]
async fn the_page_and_its_files_are_served() {
    let h = Harness::start().await;
    let page = h.get("/").send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = page.text().await.unwrap();
    assert!(!html.contains("fonts.googleapis.com") && !html.contains("fonts.gstatic.com"));
    assert!(html.contains("<link rel=\"stylesheet\" href=\"/desktop-assets/fonts.css\">"));
    assert!(html.contains("id=\"app\""));

    let js = h.get("/js/app.js").send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    let mjs = h.get("/vendor/lenis.mjs").send().await.unwrap();
    assert!(mjs.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    let css = h.get("/styles.css").send().await.unwrap();
    assert!(css.headers()["content-type"].to_str().unwrap().starts_with("text/css"));

    // a path that is not a file is the page again (the page does its own routing)
    let spa = h.get("/meeting/abc123").send().await.unwrap();
    assert_eq!(spa.status(), 200);
    assert!(spa.text().await.unwrap().contains("id=\"app\""));
    // only GET and HEAD
    assert_eq!(h.req(Method::POST, "/").send().await.unwrap().status(), 404);
    // an unknown API path is a JSON 404, also for a wrong method
    let signed = Harness::start().await.signed_in().await;
    assert_eq!(error_of(signed.get("/api/nope").send().await.unwrap(), 404).await, "Not found.");
    assert_eq!(error_of(signed.req(Method::DELETE, "/api/status").send().await.unwrap(), 404).await, "Not found.");
}

#[tokio::test]
async fn bundled_fonts_are_served_under_desktop_assets() {
    let h = Harness::start().await;
    let css = h.get("/desktop-assets/fonts.css").send().await.unwrap();
    assert_eq!(css.status(), 200, "desktop/assets/fonts.css must exist");
    assert!(css.headers()["content-type"].to_str().unwrap().starts_with("text/css"));
    let text = css.text().await.unwrap();
    // every font the stylesheet names (the paths are relative to the stylesheet) is really served
    let fonts: Vec<&str> = text.split("url(").skip(1).filter_map(|s| s.split(')').next()).map(|u| u.trim_matches(['"', '\''])).collect();
    assert!(!fonts.is_empty(), "fonts.css names fonts");
    for font in fonts {
        let res = h.get(&format!("/desktop-assets/{font}")).send().await.unwrap();
        assert_eq!(res.status(), 200, "{font}");
        assert_eq!(res.headers()["content-type"], "font/woff2");
    }
    assert_eq!(h.get("/desktop-assets/missing.woff2").send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn a_path_that_climbs_out_of_the_ui_folder_gets_the_page_not_a_file() {
    let h = Harness::start().await;
    // reqwest tidies ".." away, so this request is written by hand
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", h.port)).await.unwrap();
    let request = format!("GET /..%2f..%2f..%2fCargo.toml HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", h.port);
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    tcp.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("id=\"app\""));
    assert!(!answer.contains("[package]") && !answer.contains("[workspace]"));
}

// ---------- key and status ----------

#[tokio::test]
async fn saving_a_key_checks_it_with_google() {
    let h = Harness::start().await.signed_in().await;
    let status = body(h.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status, json!({ "hasKey": false, "notesModel": null, "transcriber": "gemini", "desktop": true }));

    let wrong = h.send_json(Method::POST, "/api/key", json!({ "key": "bad-key" })).send().await.unwrap();
    assert_eq!(error_of(wrong, 400).await, "That key was not accepted by Google.");
    let spaces = h.send_json(Method::POST, "/api/key", json!({ "key": "has a space" })).send().await.unwrap();
    assert_eq!(error_of(spaces, 400).await, "That key was not accepted by Google.");
    let not_text = h.send_json(Method::POST, "/api/key", json!({ "key": 5 })).send().await.unwrap();
    assert_eq!(error_of(not_text, 400).await, "That key was not accepted by Google.");
    let plain = h.req(Method::POST, "/api/key").header("content-type", "text/plain").body("{\"key\":\"good-key\"}").send().await.unwrap();
    assert_eq!(error_of(plain, 415).await, "Send a JSON object.");
    assert!(!body(h.get("/api/status").send().await.unwrap()).await["hasKey"].as_bool().unwrap());

    // quotes pasted around the key are removed before it is checked
    let ok = h.send_json(Method::POST, "/api/key", json!({ "key": "  \"good-key\"  " })).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let saved = body(ok).await;
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["notesModel"], "gemini-2.5-flash");
    let status = body(h.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status["hasKey"], true);
    assert_eq!(status["notesModel"], "gemini-2.5-flash");
    assert_eq!(status["transcriber"], "gemini");
}

#[tokio::test]
async fn a_sarvam_key_can_be_saved_by_itself() {
    let h = Harness::start().await.signed_in().await;
    let ok = h.send_json(Method::POST, "/api/key", json!({ "sarvamKey": "sarvam-secret-1" })).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let status = body(h.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status["transcriber"], "sarvam");
    assert_eq!(status["hasKey"], false, "the Gemini key is still missing");
    let bad = h.send_json(Method::POST, "/api/key", json!({ "sarvamKey": "has a space" })).send().await.unwrap();
    assert_eq!(error_of(bad, 400).await, "That key was not accepted by Sarvam.");
}

#[tokio::test]
async fn when_google_cannot_be_reached_the_key_is_not_saved() {
    let h = Harness::start_with(Some("http://127.0.0.1:9".into()), None).await.signed_in().await;
    let res = h.send_json(Method::POST, "/api/key", json!({ "key": "good-key" })).send().await.unwrap();
    assert_eq!(
        error_of(res, 502).await,
        "Could not check the key with Google right now. Check your internet connection and try again."
    );
    assert!(!body(h.get("/api/status").send().await.unwrap()).await["hasKey"].as_bool().unwrap());
}

// ---------- meetings ----------

fn v1_notes() -> Value {
    json!({
        "title": "Release plan", "summary": "Ship on Monday.", "sections": [],
        "keyPoints": ["Build on Friday"], "decisions": ["Ship Monday"], "actions": [],
        "flow": { "label": "Release plan", "kind": "root", "children": [] }
    })
}

#[tokio::test]
async fn a_live_meeting_can_be_created_listed_read_and_deleted() {
    let h = Harness::start().await.signed_in().await;
    assert_eq!(body(h.get("/api/meetings").send().await.unwrap()).await, json!([]));

    let made = h
        .send_json(Method::POST, "/api/meetings", json!({ "source": "live", "title": "  Weekly sync  ", "transcript": "[00:01] Hello", "durationSec": 61.5, "transcriber": "Sarvam Saaras v4" }))
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), 200);
    let m = body(made).await;
    let id = m["id"].as_str().unwrap().to_string();
    assert_eq!(m["title"], "Weekly sync");
    assert_eq!(m["source"], "live");
    assert_eq!(m["status"], "ready");
    assert_eq!(m["durationSec"], 61.5);
    assert_eq!(m["transcript"], "[00:01] Hello");
    assert_eq!(m["transcriber"], "Sarvam Saaras v4 (live)");

    let list = body(h.get("/api/meetings").send().await.unwrap()).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["hasNotes"], false);
    assert!(list[0].get("transcript").is_none(), "the list holds only the short fields");

    let one = body(h.get(&format!("/api/meetings/{id}")).send().await.unwrap()).await;
    assert_eq!(one["transcript"], "[00:01] Hello");

    // no title: a default one
    let plain = body(h.send_json(Method::POST, "/api/meetings", json!({ "transcript": "x" })).send().await.unwrap()).await;
    assert_eq!(plain["title"], "Untitled meeting");
    assert_eq!(plain["durationSec"], Value::Null);

    let gone = h.req(Method::DELETE, &format!("/api/meetings/{id}")).send().await.unwrap();
    assert_eq!(body(gone).await, json!({ "ok": true }));
    assert_eq!(error_of(h.get(&format!("/api/meetings/{id}")).send().await.unwrap(), 404).await, "Meeting not found.");
    assert_eq!(error_of(h.req(Method::DELETE, &format!("/api/meetings/{id}")).send().await.unwrap(), 404).await, "Meeting not found.");
    // an id that is not an id never reaches a folder
    assert_eq!(h.get("/api/meetings/..%2f..%2fetc").send().await.unwrap().status(), 404);
    assert_eq!(h.get("/api/meetings/zz").send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn creating_a_meeting_validates_its_input() {
    let h = Harness::start().await.signed_in().await;
    let post = |b: Value| h.send_json(Method::POST, "/api/meetings", b);
    assert_eq!(error_of(post(json!({ "source": "upload", "transcript": "x" })).send().await.unwrap(), 400).await, "source must be \"live\".");
    assert_eq!(error_of(post(json!({ "title": "x" })).send().await.unwrap(), 400).await, "transcript must be text.");
    assert_eq!(error_of(post(json!({ "transcript": 5 })).send().await.unwrap(), 400).await, "transcript must be text.");
    assert_eq!(error_of(post(json!({ "transcript": "x", "title": 5 })).send().await.unwrap(), 400).await, "title must be text.");
    assert_eq!(error_of(post(json!({ "transcript": "x", "durationSec": "9" })).send().await.unwrap(), 400).await, "durationSec must be a number.");
    assert_eq!(error_of(post(json!({ "transcript": "x", "durationSec": -1 })).send().await.unwrap(), 400).await, "durationSec must be a number.");
    assert_eq!(error_of(post(json!([1])).send().await.unwrap(), 400).await, "Send a JSON object.");
    let not_json = h.req(Method::POST, "/api/meetings").header("content-type", "application/json").body("{nope").send().await.unwrap();
    assert_eq!(error_of(not_json, 400).await, "Send a JSON object.");
    let text = h.req(Method::POST, "/api/meetings").header("content-type", "text/plain").body("{}").send().await.unwrap();
    assert_eq!(error_of(text, 415).await, "Send a JSON object.");
    let long = "x".repeat(2_000_001);
    assert_eq!(error_of(post(json!({ "transcript": long })).send().await.unwrap(), 413).await, "That transcript is too long.");
    assert_eq!(body(h.get("/api/meetings").send().await.unwrap()).await, json!([]), "nothing was created");
}

#[tokio::test]
async fn patching_a_meeting_validates_each_field() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let path = format!("/api/meetings/{id}");
    let patch = |b: Value| h.send_json(Method::PATCH, &path, b);

    let renamed = body(patch(json!({ "title": "  New name  " })).send().await.unwrap()).await;
    assert_eq!(renamed["title"], "New name");
    let blank = body(patch(json!({ "title": "   " })).send().await.unwrap()).await;
    assert_eq!(blank["title"], "Untitled meeting");
    let cut = body(patch(json!({ "title": "t".repeat(300) })).send().await.unwrap()).await;
    assert_eq!(cut["title"].as_str().unwrap().len(), 200);
    let edited = body(patch(json!({ "transcript": "edited", "transcriptEnglish": "English" })).send().await.unwrap()).await;
    assert_eq!(edited["transcript"], "edited");
    assert_eq!(edited["transcriptEnglish"], "English");

    assert_eq!(error_of(patch(json!({ "title": 1 })).send().await.unwrap(), 400).await, "title must be text.");
    assert_eq!(error_of(patch(json!({ "transcript": 1 })).send().await.unwrap(), 400).await, "transcript must be text.");
    assert_eq!(error_of(patch(json!({ "transcriptEnglish": null })).send().await.unwrap(), 400).await, "transcriptEnglish must be text.");
    let long = "x".repeat(2_000_001);
    assert_eq!(error_of(patch(json!({ "transcript": long.clone() })).send().await.unwrap(), 413).await, "That transcript is too long.");
    assert_eq!(error_of(patch(json!({ "transcriptEnglish": long })).send().await.unwrap(), 413).await, "That transcript is too long.");
    assert_eq!(error_of(patch(json!({})).send().await.unwrap(), 400).await, "Send title, transcript, transcriptEnglish and/or notes.");
    assert_eq!(error_of(patch(json!({ "other": 1 })).send().await.unwrap(), 400).await, "Send title, transcript, transcriptEnglish and/or notes.");
    // unknown meeting first, then the body
    let missing = h.send_json(Method::PATCH, "/api/meetings/zzzzzz", json!({ "title": "x" })).send().await.unwrap();
    assert_eq!(error_of(missing, 404).await, "Meeting not found.");
    // the failed patches changed nothing
    let after = body(h.get(&path).send().await.unwrap()).await;
    assert_eq!(after["transcript"], "edited");
}

#[tokio::test]
async fn patched_notes_are_checked_and_stored_in_the_new_shape() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let path = format!("/api/meetings/{id}");
    let message = "Those notes could not be saved because they are not in the expected format. Reload the page and try again.";

    let bad = h.send_json(Method::PATCH, &path, json!({ "notes": { "title": "" } })).send().await.unwrap();
    assert_eq!(error_of(bad, 400).await, message);
    let text = h.send_json(Method::PATCH, &path, json!({ "notes": "x" })).send().await.unwrap();
    assert_eq!(error_of(text, 400).await, message);

    let ok = h.send_json(Method::PATCH, &path, json!({ "notes": v1_notes() })).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let saved = body(ok).await;
    assert_eq!(saved["notes"]["keyPoints"][0], json!({ "text": "Build on Friday" }));
    assert_eq!(saved["notes"]["sections"], json!([]));
    assert_eq!(saved["title"], "T");
    // the stored notes survive a second PATCH of themselves
    let again = body(h.send_json(Method::PATCH, &path, json!({ "notes": saved["notes"] })).send().await.unwrap()).await;
    assert_eq!(again["notes"], saved["notes"]);
    let listed = body(h.get("/api/meetings").send().await.unwrap()).await;
    assert_eq!(listed[0]["hasNotes"], true);
}

#[tokio::test]
async fn the_job_routes_check_their_conditions_before_any_work_starts() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let post = |path: String| h.req(Method::POST, &path);

    // no key yet
    assert_eq!(error_of(post(format!("/api/meetings/{id}/notes")).send().await.unwrap(), 401).await, "Add your Gemini API key first.");
    assert_eq!(error_of(post(format!("/api/meetings/{id}/notes?only=junk")).send().await.unwrap(), 400).await, "only must be \"notes\".");
    assert_eq!(
        error_of(post(format!("/api/meetings/{id}/notes?only=notes")).send().await.unwrap(), 400).await,
        "There is no English transcript yet. Write the full notes first."
    );
    assert_eq!(error_of(post(format!("/api/meetings/{id}/flow")).send().await.unwrap(), 400).await, "Write the notes first.");
    assert_eq!(error_of(post(format!("/api/meetings/{id}/transcribe")).send().await.unwrap(), 401).await, "Add your Gemini API key first.");
    assert_eq!(error_of(post("/api/meetings/zzzzzz/flow".into()).send().await.unwrap(), 404).await, "Meeting not found.");

    h.add_gemini_key().await;
    assert_eq!(
        error_of(post(format!("/api/meetings/{id}/transcribe")).send().await.unwrap(), 400).await,
        "This meeting has no saved audio to transcribe."
    );
    // notes exist, the English transcript does not
    h.send_json(Method::PATCH, &format!("/api/meetings/{id}"), json!({ "notes": v1_notes() })).send().await.unwrap();
    assert_eq!(
        error_of(post(format!("/api/meetings/{id}/flow")).send().await.unwrap(), 400).await,
        "There is no English transcript yet. Write the full notes first."
    );
}

// ---------- audio ----------

#[tokio::test]
async fn audio_is_saved_and_checked() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let path = format!("/api/meetings/{id}/audio");
    let put = |content_type: &'static str, bytes: Vec<u8>| h.req(Method::PUT, &path).header("content-type", content_type).body(bytes);

    assert_eq!(error_of(h.get(&path).send().await.unwrap(), 404).await, "This meeting has no audio.");
    assert_eq!(
        error_of(put("application/json", vec![1, 2, 3]).send().await.unwrap(), 415).await,
        "Send audio with an audio Content-Type, for example audio/webm."
    );
    assert_eq!(error_of(put("audio/webm", vec![]).send().await.unwrap(), 400).await, "The audio was empty.");
    let unknown = h.req(Method::PUT, "/api/meetings/zzzzzz/audio").header("content-type", "audio/webm").body(vec![1u8]).send().await.unwrap();
    assert_eq!(error_of(unknown, 404).await, "Meeting not found.");

    let big = put("audio/webm", vec![0u8; 100_000_001]).send().await.unwrap();
    assert!(error_of(big, 413).await.contains("100 MB"));
    assert_eq!(error_of(h.get(&path).send().await.unwrap(), 404).await, "This meeting has no audio.", "the too-big file was not kept");

    let ok = put("audio/webm;codecs=opus", bytes_pattern(1000)).send().await.unwrap();
    assert_eq!(body(ok).await, json!({ "ok": true }));
    let meeting = body(h.get(&format!("/api/meetings/{id}")).send().await.unwrap()).await;
    assert_eq!(meeting["audio"]["mime"], "audio/webm");
}

#[tokio::test]
async fn audio_plays_back_with_ranges() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let path = format!("/api/meetings/{id}/audio");
    let data = bytes_pattern(1000);
    h.req(Method::PUT, &path).header("content-type", "audio/webm").body(data.clone()).send().await.unwrap();
    let ranged = |range: &'static str| h.get(&path).header("range", range).send();

    let whole = h.get(&path).send().await.unwrap();
    assert_eq!(whole.status(), 200);
    assert_eq!(whole.headers()["accept-ranges"], "bytes");
    assert_eq!(whole.headers()["content-type"], "audio/webm");
    assert_eq!(whole.headers()["content-length"], "1000");
    assert_eq!(whole.bytes().await.unwrap().to_vec(), data);

    let part = ranged("bytes=10-19").await.unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 10-19/1000");
    assert_eq!(part.headers()["content-length"], "10");
    assert_eq!(part.bytes().await.unwrap().to_vec(), data[10..20]);

    let open = ranged("bytes=990-").await.unwrap();
    assert_eq!(open.status(), 206);
    assert_eq!(open.headers()["content-range"], "bytes 990-999/1000");
    assert_eq!(open.bytes().await.unwrap().to_vec(), data[990..]);

    let suffix = ranged("bytes=-5").await.unwrap();
    assert_eq!(suffix.status(), 206);
    assert_eq!(suffix.headers()["content-range"], "bytes 995-999/1000");
    assert_eq!(suffix.bytes().await.unwrap().to_vec(), data[995..]);

    let past_end = ranged("bytes=500-5000").await.unwrap();
    assert_eq!(past_end.headers()["content-range"], "bytes 500-999/1000");

    for bad in ["bytes=1000-", "bytes=5000-6000", "bytes=20-10", "bytes=-", "bytes=abc", "bytes=1-2,5-6"] {
        let res = h.get(&path).header("range", bad).send().await.unwrap();
        assert_eq!(res.status(), 416, "{bad}");
        assert_eq!(res.headers()["content-range"], "bytes */1000", "{bad}");
    }
}

#[tokio::test]
async fn the_hourly_audio_limit_is_30() {
    let h = Harness::start().await.signed_in().await;
    let id = h.new_meeting().await;
    let path = format!("/api/meetings/{id}/audio");
    for i in 0..30 {
        let res = h.req(Method::PUT, &path).header("content-type", "audio/webm").body(vec![1u8, 2, 3]).send().await.unwrap();
        assert_eq!(res.status(), 200, "upload {i}");
    }
    let res = h.req(Method::PUT, &path).header("content-type", "audio/webm").body(vec![1u8, 2, 3]).send().await.unwrap();
    assert_eq!(error_of(res, 429).await, "Upload limit reached for this hour.");
    // playback is not paid work
    assert_eq!(h.get(&path).send().await.unwrap().status(), 200);
}

// ---------- uploads ----------

fn form_with_file(name: &str, content_type: &str, bytes: Vec<u8>) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()).mime_str(content_type).unwrap();
    reqwest::multipart::Form::new().part("file", part)
}

#[tokio::test]
async fn an_upload_needs_the_key_a_real_file_and_an_audio_type() {
    let h = Harness::start().await.signed_in().await;
    let upload = || h.req(Method::POST, "/api/uploads");

    let form = form_with_file("talk.wav", "audio/wav", bytes_pattern(2000));
    assert_eq!(error_of(upload().multipart(form).send().await.unwrap(), 401).await, "Add your Gemini API key first.");

    h.add_gemini_key().await;
    let plain = upload().json(&json!({ "file": "x" })).send().await.unwrap();
    assert_eq!(error_of(plain, 400).await, "Send a multipart form with an audio file in the field named file.");
    let no_file = reqwest::multipart::Form::new().text("title", "hello");
    assert_eq!(error_of(upload().multipart(no_file).send().await.unwrap(), 400).await, "No audio file was received.");
    let empty = form_with_file("talk.wav", "audio/wav", vec![]);
    assert_eq!(error_of(upload().multipart(empty).send().await.unwrap(), 400).await, "No audio file was received.");
    let not_audio = form_with_file("notes.txt", "text/plain", b"just words".to_vec());
    assert_eq!(error_of(upload().multipart(not_audio).send().await.unwrap(), 415).await, "That does not look like an audio file.");
    let too_big = form_with_file("long.wav", "audio/wav", vec![0u8; 100_000_001]);
    assert_eq!(error_of(upload().multipart(too_big).send().await.unwrap(), 413).await, "That file is larger than 100 MB.");
    assert_eq!(body(h.get("/api/meetings").send().await.unwrap()).await, json!([]), "no refused upload left a meeting behind");
}

#[tokio::test]
async fn a_good_upload_becomes_a_transcribing_meeting_with_its_audio_saved() {
    let h = Harness::start().await.signed_in().await;
    h.add_gemini_key().await;
    let data = bytes_pattern(4000);

    // the title typed in the form wins
    let form = form_with_file("standup.final.wav", "audio/wav", data.clone()).text("title", "  Monday standup  ").text("mode", "verbatim").text("speakers", "0");
    let res = h.req(Method::POST, "/api/uploads").multipart(form).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let m = body(res).await;
    assert_eq!(m["title"], "Monday standup");
    assert_eq!(m["source"], "upload");
    assert!(m["id"].is_string());
    // (the background job may already have failed against the fake services; the first answer says "transcribing")
    assert_eq!(m["status"], "transcribing");

    // without a title: the file name without its extension
    let form = form_with_file("standup.final.wav", "audio/wav", data.clone()).text("speakers", "1");
    let m2 = body(h.req(Method::POST, "/api/uploads").multipart(form).send().await.unwrap()).await;
    assert_eq!(m2["title"], "standup.final");

    // the audio was kept and plays back
    let id = m["id"].as_str().unwrap();
    let audio = h.get(&format!("/api/meetings/{id}/audio")).send().await.unwrap();
    assert_eq!(audio.status(), 200);
    assert_eq!(audio.bytes().await.unwrap().to_vec(), data);
}

// ---------- live captions ----------

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open_socket(h: &Harness, query: &str, cookie: bool) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://127.0.0.1:{}/api/live{query}", h.port).into_client_request().unwrap();
    request.headers_mut().insert("origin", format!("http://127.0.0.1:{}", h.port).parse().unwrap());
    if let (true, Some(c)) = (cookie, &h.cookie) {
        request.headers_mut().insert("cookie", c.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.map(|(socket, _)| socket)
}

fn refused_with(result: Result<Socket, tokio_tungstenite::tungstenite::Error>) -> u16 {
    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status().as_u16(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => panic!("the socket should have been refused"),
    }
}

async fn next_json(socket: &mut Socket) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.expect("a message within 10 s");
        match msg {
            Some(Ok(WsMessage::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(WsMessage::Close(_))) | None => return json!({ "type": "(socket closed)" }),
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("socket error: {e}"),
        }
    }
}

#[tokio::test]
async fn the_socket_is_refused_without_the_cookie_a_key_or_a_valid_language() {
    let h = Harness::start().await.signed_in().await;
    assert_eq!(refused_with(open_socket(&h, "", false).await), 401, "no cookie");
    assert_eq!(refused_with(open_socket(&h, "", true).await), 401, "no key at all");
    h.add_gemini_key().await;
    assert_eq!(refused_with(open_socket(&h, "?lang=not%20a%20code", true).await), 400);
    // a plain GET that is not a WebSocket upgrade
    assert_eq!(error_of(h.get("/api/live").send().await.unwrap(), 400).await, "Expected a WebSocket connection.");
    assert_eq!(error_of(h.get("/api/live?lang=x_y").send().await.unwrap(), 400).await, "Unknown language code.");
    // another website cannot open it with the cookie either
    let mut request = format!("ws://127.0.0.1:{}/api/live", h.port).into_client_request().unwrap();
    request.headers_mut().insert("origin", "https://evil.example.net".parse().unwrap());
    request.headers_mut().insert("cookie", h.cookie.as_ref().unwrap().parse().unwrap());
    assert_eq!(refused_with(tokio_tungstenite::connect_async(request).await.map(|(s, _)| s)), 403);
}

#[tokio::test]
async fn audio_goes_in_and_captions_come_out() {
    let live_url = fake_gemini_live().await;
    let h = Harness::start_with(Some(fake_gemini().await), Some(live_url)).await.signed_in().await;
    h.add_gemini_key().await;

    let mut socket = open_socket(&h, "?mode=smart&lang=ta&rid=abcd1234-recording", true).await.expect("the socket opens");
    let ready = next_json(&mut socket).await;
    assert_eq!(ready["type"], "ready");
    assert!(ready["provider"].as_str().unwrap().contains("Gemini"));

    socket.send(WsMessage::binary(vec![0u8; 3200])).await.unwrap();
    assert_eq!(next_json(&mut socket).await, json!({ "type": "interim", "text": "hel" }));
    assert_eq!(next_json(&mut socket).await, json!({ "type": "final", "text": "hello there" }));

    // the speaker stops: the service ends the call and the page is told
    socket.send(WsMessage::text("end")).await.unwrap();
    assert_eq!(next_json(&mut socket).await, json!({ "type": "closed" }));

    // what the captions used is attached to the meeting made from this recording
    let made = h
        .send_json(Method::POST, "/api/meetings", json!({ "source": "live", "transcript": "hello there", "rid": "abcd1234-recording" }))
        .send()
        .await
        .unwrap();
    let m = body(made).await;
    assert_eq!(m["usage"]["geminiLiveSec"], 0.1, "3200 bytes of 16 kHz audio is 0.1 s");
}

#[tokio::test]
async fn only_three_live_sessions_run_at_once() {
    let live_url = fake_gemini_live().await;
    let h = Harness::start_with(Some(fake_gemini().await), Some(live_url)).await.signed_in().await;
    h.add_gemini_key().await;
    let mut open = Vec::new();
    for _ in 0..3 {
        open.push(open_socket(&h, "", true).await.expect("one of the first three"));
    }
    assert_eq!(refused_with(open_socket(&h, "", true).await), 503);
    // a closed session gives its place back
    let mut first = open.remove(0);
    first.send(WsMessage::Close(None)).await.unwrap();
    drop(first);
    let mut allowed = false;
    for _ in 0..50 {
        if open_socket(&h, "", true).await.is_ok() {
            allowed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(allowed, "the freed place can be used again");
}

// ---------- the page must not wait for Google ----------

/// A Google that takes 3 seconds to list its models (it does that when it is busy).
async fn slow_gemini() -> String {
    let app = axum::Router::new().route(
        "/models",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            axum::Json(json!({ "models": [{ "name": "models/gemini-2.5-flash", "supportedGenerationMethods": ["generateContent"] }] }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

/// The page asks /api/status before it shows anything, and a person who already saved a key has Pechu ask Google for the model
/// list on that first call. The page does not use the model name, so a slow Google must not make the app slow to open.
#[tokio::test]
async fn the_first_status_call_does_not_wait_for_a_slow_google() {
    let dirs = tempfile::tempdir().unwrap();
    let running = serve(ServeConfig {
        data_dir: dirs.path().join("meetings"),
        config_dir: dirs.path().join("config"),
        preferred_port: 0,
        static_dir: None,
        test_overrides: Some(TestOverrides {
            gemini_base_url: Some(slow_gemini().await),
            memory_keys: true,
            gemini_key: Some("good-key".into()),
            ..Default::default()
        }),
    })
    .await
    .unwrap();
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
    let res = http.get(format!("http://127.0.0.1:{}/?k={}", running.port, running.launch_token)).send().await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let status = || http.get(format!("http://127.0.0.1:{}/api/status", running.port)).header("cookie", &cookie).send();

    let began = std::time::Instant::now();
    let first: Value = status().await.unwrap().json().await.unwrap();
    assert!(began.elapsed() < Duration::from_millis(1500), "the status call waited {:?} for Google", began.elapsed());
    assert_eq!(first["hasKey"], true);

    // the lookup went on in the background, so a little later the model is known (and answered at once)
    tokio::time::sleep(Duration::from_secs(4)).await;
    let began = std::time::Instant::now();
    let later: Value = status().await.unwrap().json().await.unwrap();
    assert!(began.elapsed() < Duration::from_millis(500));
    assert!(later["notesModel"].is_string(), "the model was found in the background: {later}");
}

// ---------- start and stop ----------

#[tokio::test]
async fn shutdown_stops_the_server_and_a_busy_port_falls_back_to_a_free_one() {
    let dirs = tempfile::tempdir().unwrap();
    let config = |port: u16| ServeConfig {
        data_dir: dirs.path().join("meetings"),
        config_dir: dirs.path().join("config"),
        preferred_port: port,
        static_dir: None,
        test_overrides: Some(TestOverrides { memory_keys: true, ..Default::default() }),
    };
    let first = serve(config(0)).await.unwrap();
    assert_ne!(first.port, 0);
    let second = serve(config(first.port)).await.unwrap();
    assert_ne!(second.port, first.port, "the port was taken, so another was used");

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let port = first.port;
    assert_eq!(http.get(format!("http://127.0.0.1:{port}/healthz")).send().await.unwrap().status(), 200);
    tokio::time::timeout(Duration::from_secs(40), first.shutdown()).await.expect("shutdown finishes");
    assert!(http.get(format!("http://127.0.0.1:{port}/healthz")).send().await.is_err(), "nothing listens any more");
    second.shutdown().await;
}

#[tokio::test]
async fn the_ui_can_be_served_from_a_folder() {
    let dirs = tempfile::tempdir().unwrap();
    let ui = dirs.path().join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(ui.join("index.html"), "<html>\n<link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">\nfrom disk</html>\n").unwrap();
    std::fs::write(ui.join("hello.js"), "console.log(1)").unwrap();
    let running = serve(ServeConfig {
        data_dir: dirs.path().join("meetings"),
        config_dir: dirs.path().join("config"),
        preferred_port: 0,
        static_dir: Some(ui),
        test_overrides: Some(TestOverrides { memory_keys: true, ..Default::default() }),
    })
    .await
    .unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let page = http.get(format!("http://127.0.0.1:{}/", running.port)).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("from disk"));
    assert!(page.contains("/desktop-assets/fonts.css") && !page.contains("googleapis"));
    let js = http.get(format!("http://127.0.0.1:{}/hello.js", running.port)).send().await.unwrap();
    assert_eq!(js.text().await.unwrap(), "console.log(1)");
}

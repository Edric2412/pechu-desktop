//! The gate every request passes before a route sees it: host and origin checks, the launch token, the session cookie, rate
//! limits, and the security headers on the way out. (Port of the `fetch` function in server/index.ts.)
use crate::api::{json_response, ApiError};
use crate::security::{classify, cookie_value, host_allowed, security_headers, session_cookie, COOKIE};
use crate::App;
use axum::extract::{Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Only so many transcription / notes jobs run at once.
pub const MAX_JOBS: usize = 3;

pub async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let mut res = check(&app, req, next).await;
    for (name, value) in security_headers(&app.csp) {
        res.headers_mut().insert(HeaderName::from_static(name), HeaderValue::from_str(&value).expect("header text is plain ASCII"));
    }
    res
}

async fn check(app: &Arc<App>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_api = path == "/api" || path.starts_with("/api/");

    // Health probe: reachable without anything else.
    if path == "/healthz" {
        return json_response(StatusCode::OK, json!({ "ok": true, "uptimeSec": app.started.elapsed().as_secs() }));
    }

    // Readiness probe (as in the web version): the data folder can be written to and the app is not closing.
    if path == "/readyz" {
        let data_dir = app.store.tmp_dir();
        let writable = data_dir.parent().is_some_and(|root| tempfile::tempfile_in(root).is_ok());
        return if writable && !app.stopping.load(Ordering::SeqCst) {
            json_response(StatusCode::OK, json!({ "ok": true }))
        } else {
            ApiError::new(503, "Not ready.").into_response()
        };
    }

    // copied, so `req` can still be handed on to the route afterwards
    let headers = req.headers().clone();
    let header_text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    if !host_allowed(header_text("host").as_deref(), header_text("origin").as_deref(), header_text("sec-fetch-site").as_deref(), app.port, is_api) {
        return if is_api { ApiError::new(403, "Not allowed.").into_response() } else { (StatusCode::FORBIDDEN, "Not allowed.").into_response() };
    }

    if !is_api {
        // The app window opens "/?k=<launch token>": that signs it in.
        if req.method() == Method::GET && path == "/" {
            if let Some(token) = query_value(req.uri().query(), "k") {
                return launch(app, &token, header_text("cookie").as_deref());
            }
        }
        return next.run(req).await;
    }

    if !app.api_limit.take() {
        return ApiError::new(429, "Too many requests. Slow down a little.").into_response();
    }
    // These three answer the same thing to everyone.
    if matches!(path.as_str(), "/api/auth" | "/api/login" | "/api/logout") {
        return next.run(req).await;
    }
    let signed_in = header_text("cookie").and_then(|c| cookie_value(&c, COOKIE)).is_some_and(|id| app.sessions.is_valid(&id));
    if !signed_in {
        return ApiError::new(401, "Please sign in.").into_response();
    }
    if app.stopping.load(Ordering::SeqCst) {
        return ApiError::new(503, "The server is restarting. Try again in a moment.").into_response();
    }

    // Paid work: new audio and re-transcription share one hourly limit, and only so many jobs run at once.
    let (audio_work, starts_job) = classify(req.method(), &path);
    if audio_work && !app.upload_limit.take() {
        return ApiError::new(429, "Upload limit reached for this hour.").into_response();
    }
    if starts_job && app.jobs.active_jobs() >= MAX_JOBS {
        return ApiError::new(429, "Pechu is busy with other meetings. Try again in a few minutes.").into_response();
    }
    if path == "/api/live" && !app.live_limit.take() {
        return ApiError::new(429, "Too many live sessions started. Wait a minute.").into_response();
    }
    next.run(req).await
}

/// The value of one query parameter (the launch token is hex text, so nothing needs decoding).
fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    query?.split('&').find_map(|pair| pair.strip_prefix(name)?.strip_prefix('=')).map(str::to_string)
}

/// The first use of the launch token: set the session cookie and go to the page itself, so the token leaves the address bar.
/// A second use is refused, unless this window already has a working session (a reload of the same address).
fn launch(app: &App, token: &str, cookie_header: Option<&str>) -> Response {
    let redirect = |cookie: Option<String>| {
        let mut res = Response::builder().status(StatusCode::FOUND).header(header::LOCATION, "/").header(header::CACHE_CONTROL, "no-store");
        if let Some(cookie) = cookie {
            res = res.header(header::SET_COOKIE, cookie);
        }
        res.body(axum::body::Body::empty()).expect("redirect headers are valid")
    };
    if let Some(id) = app.sessions.redeem(token) {
        return redirect(Some(session_cookie(&id)));
    }
    let already_signed_in = cookie_header.and_then(|c| cookie_value(c, COOKIE)).is_some_and(|id| app.sessions.is_valid(&id));
    if already_signed_in {
        return redirect(None);
    }
    (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")], "This link was already used. Close Pechu and open it again.").into_response()
}

#[cfg(test)]
mod tests {
    use super::query_value;

    #[test]
    fn query_values() {
        assert_eq!(query_value(Some("k=abc"), "k").as_deref(), Some("abc"));
        assert_eq!(query_value(Some("x=1&k=abc&y=2"), "k").as_deref(), Some("abc"));
        assert_eq!(query_value(Some("key=abc"), "k"), None);
        assert_eq!(query_value(None, "k"), None);
    }
}

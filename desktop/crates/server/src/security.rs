//! What keeps other programs and other websites away from the local server: the launch token and the session cookie, the
//! Host / Origin checks, rate limits and the response headers. (Port of server/security.ts, with the password login replaced
//! by a launch token that only the app window knows.)
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

pub const COOKIE: &str = "pechu_session";

/// `n` random bytes as hex text. Used for the launch token and for session ids.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Compares two secrets without stopping at the first difference (hashing first makes the lengths equal).
pub fn secrets_equal(a: &str, b: &str) -> bool {
    sha(a).ct_eq(&sha(b)).into()
}

// ----- launch token and sessions -----

/// The one-time launch token and the sessions it creates. Everything is in memory, so closing the app signs everything out.
pub struct Sessions {
    launch_token: Mutex<Option<String>>,
    /// SHA-256 of each session id: looking a cookie up in this set then reveals nothing about the real ids.
    sessions: Mutex<HashSet<[u8; 32]>>,
}

impl Sessions {
    pub fn new(launch_token: String) -> Self {
        Sessions { launch_token: Mutex::new(Some(launch_token)), sessions: Mutex::new(HashSet::new()) }
    }

    /// Spends the launch token: true (and a new session id) the first time it is shown, never again.
    /// A wrong guess does not spend it, so a stranger cannot lock the real window out.
    pub fn redeem(&self, given: &str) -> Option<String> {
        let mut slot = self.launch_token.lock().unwrap();
        let matches = slot.as_deref().is_some_and(|t| secrets_equal(t, given));
        if !matches {
            return None;
        }
        *slot = None;
        let id = random_hex(32);
        self.sessions.lock().unwrap().insert(sha(&id));
        Some(id)
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains(&sha(id))
    }
}

/// The value of one cookie in a `Cookie:` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        if let Some((k, v)) = part.trim().split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The cookie that proves a request comes from the app window. No Max-Age: it ends when the window's session ends.
pub fn session_cookie(id: &str) -> String {
    format!("{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/")
}

// ----- rate limiting -----

/// "At most `max` in any `window`." One shared bucket: every caller is this computer.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    hits: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        RateLimiter { max, window, hits: Mutex::new(VecDeque::new()) }
    }

    /// true = allowed (and counted), false = over the limit.
    pub fn take(&self) -> bool {
        self.take_at(Instant::now())
    }

    pub fn take_at(&self, now: Instant) -> bool {
        let mut hits = self.hits.lock().unwrap();
        while hits.front().is_some_and(|t| now.duration_since(*t) >= self.window) {
            hits.pop_front();
        }
        if hits.len() >= self.max {
            return false;
        }
        hits.push_back(now);
        true
    }
}

// ----- Host / Origin checks (block other websites and DNS rebinding) -----

/// `host` is the Host header, `origin` the Origin header, `site` the browser's Sec-Fetch-Site hint.
/// - The Host must be exactly `127.0.0.1:<port>` or `localhost:<port>`.
/// - An Origin, when there is one, must be this very host and port (another port is another website).
/// - With `api` set, Sec-Fetch-Site must say same-origin or none (typed address, non-browser client), or be missing.
pub fn host_allowed(host: Option<&str>, origin: Option<&str>, site: Option<&str>, port: u16, api: bool) -> bool {
    let Some(host) = host.map(str::to_ascii_lowercase) else { return false };
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return false;
    }
    if api && !matches!(site, None | Some("same-origin") | Some("none")) {
        return false;
    }
    match origin {
        None => true,
        Some(o) => o.to_ascii_lowercase() == format!("http://{host}"),
    }
}

// ----- response headers -----

/// The Content-Security-Policy. Everything comes from this server itself; fonts are bundled, so no Google hosts.
/// Audio and images may also be blob: / data: URLs (recordings and the page icon). The live-caption socket is named by
/// port, because some web views do not treat 'self' as covering ws://.
pub fn csp(port: u16) -> String {
    [
        "default-src 'self'".to_string(),
        "script-src 'self'".to_string(),
        "style-src 'self' 'unsafe-inline'".to_string(),
        "font-src 'self'".to_string(),
        "img-src 'self' data: blob:".to_string(),
        "media-src 'self' blob:".to_string(),
        format!("connect-src 'self' ws://127.0.0.1:{port} ws://localhost:{port}"),
        "object-src 'none'".to_string(),
        "base-uri 'none'".to_string(),
        "form-action 'self'".to_string(),
        "frame-ancestors 'none'".to_string(),
    ]
    .join("; ")
}

/// The fixed headers every response carries (name, value).
pub fn security_headers(csp: &str) -> [(&'static str, String); 5] {
    [
        ("content-security-policy", csp.to_string()),
        ("x-content-type-options", "nosniff".to_string()),
        ("referrer-policy", "no-referrer".to_string()),
        ("x-frame-options", "DENY".to_string()),
        ("permissions-policy", "microphone=(self), display-capture=(self), camera=(), geolocation=()".to_string()),
    ]
}

// ----- which requests count as paid work -----

/// (audio_work, starts_job) for a request: audio work (new audio, re-transcription) shares one hourly limit, and anything
/// that starts a background job is limited by how many jobs run at once.
pub fn classify(method: &axum::http::Method, path: &str) -> (bool, bool) {
    use axum::http::Method;
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let post = method == Method::POST;
    match parts.as_slice() {
        ["api", "uploads"] => (post, post),
        ["api", "meetings", id, last] if !id.is_empty() => {
            let audio_work = matches!(*last, "audio" | "transcribe") && (method == Method::PUT || post);
            let starts_job = post && matches!(*last, "transcribe" | "notes" | "flow");
            (audio_work, starts_job)
        }
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn host_must_be_this_server() {
        assert!(host_allowed(Some("127.0.0.1:5000"), None, None, 5000, true));
        assert!(host_allowed(Some("localhost:5000"), None, None, 5000, true));
        assert!(host_allowed(Some("LocalHost:5000"), None, None, 5000, true));
        assert!(!host_allowed(Some("evil.example.net"), None, None, 5000, false)); // DNS rebinding
        assert!(!host_allowed(Some("evil.example.net:5000"), None, None, 5000, false));
        assert!(!host_allowed(Some("127.0.0.1:5001"), None, None, 5000, false)); // wrong port
        assert!(!host_allowed(Some("127.0.0.1"), None, None, 5000, false));
        assert!(!host_allowed(None, None, None, 5000, false));
    }

    #[test]
    fn origin_must_be_this_host_and_port() {
        let ok = |origin: &str| host_allowed(Some("localhost:5000"), Some(origin), None, 5000, true);
        assert!(ok("http://localhost:5000"));
        assert!(!ok("https://evil.example.net"));
        assert!(!ok("http://localhost:3000")); // another port is another site
        assert!(!ok("http://127.0.0.1:5000")); // same server, but not the host the page came from
        assert!(!ok("null"));
        assert!(!ok(""));
    }

    #[test]
    fn api_calls_must_not_be_cross_site() {
        let check = |site: Option<&str>, api| host_allowed(Some("localhost:5000"), None, site, 5000, api);
        assert!(check(Some("same-origin"), true));
        assert!(check(Some("none"), true));
        assert!(check(None, true)); // not a browser
        assert!(!check(Some("same-site"), true));
        assert!(!check(Some("cross-site"), true));
        assert!(check(Some("cross-site"), false)); // opening the page from a link elsewhere is fine
    }

    #[test]
    fn launch_token_works_once() {
        let s = Sessions::new("the-token".into());
        assert!(s.redeem("wrong").is_none());
        let id = s.redeem("the-token").expect("first use");
        assert!(s.is_valid(&id));
        assert!(s.redeem("the-token").is_none(), "a replay is refused");
        assert!(s.is_valid(&id), "the session made by the first use stays valid");
        assert!(!s.is_valid("something-else"));
    }

    #[test]
    fn a_wrong_guess_does_not_spend_the_token() {
        let s = Sessions::new("the-token".into());
        assert!(s.redeem("guess").is_none());
        assert!(s.redeem("the-token").is_some());
    }

    #[test]
    fn cookie_is_found_among_others() {
        assert_eq!(cookie_value("a=1; pechu_session=abc; b=2", "pechu_session").as_deref(), Some("abc"));
        assert_eq!(cookie_value("a=1", "pechu_session"), None);
        assert_eq!(cookie_value("x_pechu_session=1", "pechu_session"), None);
    }

    #[test]
    fn rate_limiter_allows_n_per_window_then_recovers() {
        let r = RateLimiter::new(3, Duration::from_secs(1));
        let t0 = Instant::now();
        assert!([0, 1, 2].iter().all(|ms| r.take_at(t0 + Duration::from_millis(*ms))));
        assert!(!r.take_at(t0 + Duration::from_millis(3)));
        assert!(r.take_at(t0 + Duration::from_millis(1500)));
    }

    #[test]
    fn paid_work_is_recognised() {
        assert_eq!(classify(&Method::POST, "/api/uploads"), (true, true));
        assert_eq!(classify(&Method::PUT, "/api/meetings/abc/audio"), (true, false));
        assert_eq!(classify(&Method::GET, "/api/meetings/abc/audio"), (false, false));
        assert_eq!(classify(&Method::POST, "/api/meetings/abc/transcribe"), (true, true));
        assert_eq!(classify(&Method::POST, "/api/meetings/abc/notes"), (false, true));
        assert_eq!(classify(&Method::POST, "/api/meetings/abc/flow"), (false, true));
        assert_eq!(classify(&Method::GET, "/api/meetings"), (false, false));
    }

    #[test]
    fn csp_has_no_google_hosts_and_names_the_socket() {
        let c = csp(4000);
        assert!(!c.contains("google"));
        assert!(c.contains("ws://127.0.0.1:4000"));
        assert!(c.contains("script-src 'self'"));
        assert!(c.contains("media-src 'self' blob:"));
    }
}

//! The UI files. They are built into the program (the web UI in `public/`, plus the bundled fonts in `desktop/assets/`),
//! or read from a folder on disk when `ServeConfig.static_dir` is set (development: edit a file, reload the window).
//! (Port of `serveStatic`, server/index.ts.)
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use rust_embed::RustEmbed;
use std::path::{Path, PathBuf};

/// The web UI, exactly as the web version serves it.
#[derive(RustEmbed)]
#[folder = "../../../public"]
struct Public;

/// Fonts, scripts and the pre-drawn backdrop pictures for the desktop app (served under /desktop-assets/). `allow_missing`: the build must not break while it is empty.
#[derive(RustEmbed)]
#[folder = "../../assets"]
#[allow_missing = true]
struct DesktopAssets;

const DESKTOP_PREFIX: &str = "/desktop-assets/";

/// The three lines in index.html that load fonts from Google become one link to the bundled fonts.
/// Together with `add_native_audio` below, this is the only change the desktop app makes to this page.
pub fn use_bundled_fonts(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut replaced = false;
    for line in html.split_inclusive('\n') {
        let is_google_link = line.trim_start().starts_with("<link") && (line.contains("fonts.googleapis.com") || line.contains("fonts.gstatic.com"));
        if !is_google_link {
            out.push_str(line);
        } else if !replaced {
            replaced = true;
            out.push_str("  <link rel=\"stylesheet\" href=\"/desktop-assets/fonts.css\">\n");
        }
    }
    out
}

/// The page gets one extra script, loaded before all the others: `desktop-assets/native-audio.js`. It adds the computer's own
/// sound to the list of inputs, which the web view cannot list by itself. Without a `<script` line the page is left as it is.
pub fn add_native_audio(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 80);
    let mut added = false;
    for line in html.split_inclusive('\n') {
        if !added && line.trim_start().starts_with("<script") {
            added = true;
            out.push_str("<script src=\"/desktop-assets/native-audio.js\"></script>\n");
        }
        out.push_str(line);
    }
    out
}

/// One more script, right after the native-audio one: F11 toggles fullscreen (the window has no title bar, so no buttons).
pub fn add_window_keys(html: &str) -> String {
    const AUDIO: &str = "<script src=\"/desktop-assets/native-audio.js\"></script>\n";
    html.replacen(AUDIO, &format!("{AUDIO}<script src=\"/desktop-assets/window-keys.js\"></script>\n"), 1)
}

/// The window's own minimize / maximize / close buttons (the window has no title bar): `window-controls.js` builds them, and
/// `window-controls.css` styles them. The script goes right after the F11 one (so call `add_window_keys` first), the
/// stylesheet at the very end of `<head>`, after the static-backdrop one (so call `add_static_backdrop` first).
pub fn add_window_controls(html: &str) -> String {
    const KEYS: &str = "<script src=\"/desktop-assets/window-keys.js\"></script>\n";
    html.replacen(KEYS, &format!("{KEYS}<script src=\"/desktop-assets/window-controls.js\"></script>\n"), 1)
        .replacen("</head>", "<link rel=\"stylesheet\" href=\"/desktop-assets/window-controls.css\">\n</head>", 1)
}

/// The desktop app draws the page background once and keeps it still: one extra stylesheet, added at the end of the page's
/// `<head>` so it wins over the page's own styles. The picture is the same; only the slow drift is switched off.
pub fn add_static_backdrop(html: &str) -> String {
    html.replacen("</head>", "<link rel=\"stylesheet\" href=\"/desktop-assets/static-backdrop.css\">\n</head>", 1)
}

/// Turns a URL path into a relative file path, or None when it tries to leave the folder (`..`, backslashes, NUL, ...).
/// A path that does not decode is treated as "/" like the web version does.
fn clean_path(url_path: &str) -> Option<String> {
    let decoded = percent_decode(url_path).unwrap_or_else(|| "/".to_string());
    let rel = decoded.trim_start_matches('/');
    // On Windows, joining "c:/x" onto the UI folder throws the folder away and gives "c:/x", so a colon is refused there.
    if rel.contains('\\') || rel.contains('\0') || (cfg!(windows) && rel.contains(':')) || rel.split('/').any(|part| part == "..") {
        return None;
    }
    Some(rel.to_string())
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "png" => "image/png",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn respond(file: &str, bytes: Vec<u8>) -> Response {
    // Like the web version: nothing is cached, so an updated UI is picked up at once.
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(file))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .expect("static response headers are valid")
}

fn not_found(text: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(text))
        .expect("static response headers are valid")
}

/// Where the UI files come from.
#[derive(Clone)]
pub enum UiFiles {
    Embedded,
    Folder(PathBuf),
}

impl UiFiles {
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        match self {
            UiFiles::Embedded => Public::get(rel).map(|f| f.data.into_owned()),
            UiFiles::Folder(root) => read_inside(root, rel),
        }
    }

    /// The response for a GET of `url_path`: a file when there is one, otherwise the UI's index page (the page does its own
    /// routing). Anything under /desktop-assets/ is one of the desktop app's own files or a plain 404.
    pub fn serve(&self, url_path: &str) -> Response {
        if let Some(rest) = url_path.strip_prefix(DESKTOP_PREFIX) {
            let found = clean_path(rest).and_then(|rel| DesktopAssets::get(&rel).map(|f| (rel, f.data.into_owned())));
            return match found {
                Some((rel, bytes)) => respond(&rel, bytes),
                None => not_found("Not found."),
            };
        }
        if let Some(rel) = clean_path(url_path).filter(|rel| !rel.is_empty() && rel != "index.html") {
            if let Some(bytes) = self.read(&rel) {
                return respond(&rel, bytes); // every UI file is served exactly as it is in public/
            }
        }
        match self.read("index.html") {
            Some(bytes) => {
                let mut page = add_window_keys(&add_native_audio(&use_bundled_fonts(&String::from_utf8_lossy(&bytes))));
                if std::env::var_os("PECHU_KEEP_MOTION").is_none() {
                    page = add_static_backdrop(&page); // start the app with PECHU_KEEP_MOTION=1 to get the moving background back
                }
                page = add_window_controls(&page);
                if std::env::var_os("PECHU_PROBE").is_some() {
                    page = page.replacen("<script", "<script src=\"/desktop-assets/probe.js\"></script>\n<script", 1); // timing probe, see probe.js
                }
                respond("index.html", page.into_bytes())
            }
            None => not_found("Frontend not found (public/index.html is missing)."),
        }
    }
}

/// Reads `root/rel` only when it is a regular file that really lives inside `root` (a link pointing outside is refused).
fn read_inside(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let root = root.canonicalize().ok()?;
    let target = root.join(rel).canonicalize().ok()?;
    if !target.starts_with(&root) || !target.is_file() {
        return None;
    }
    std::fs::read(target).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "<head>\n  <title>Pechu</title>\n  <link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">\n  <link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>\n  <link href=\"https://fonts.googleapis.com/css2?family=X\" rel=\"stylesheet\">\n  <link rel=\"stylesheet\" href=\"/styles.css\">\n</head>\n";

    #[test]
    fn google_font_links_become_one_bundled_link() {
        let out = use_bundled_fonts(HEAD);
        assert!(!out.contains("google"));
        assert!(!out.contains("gstatic"));
        assert_eq!(out.matches("/desktop-assets/fonts.css").count(), 1);
        assert!(out.contains("<link rel=\"stylesheet\" href=\"/styles.css\">")); // the other lines stay
        assert!(out.contains("<title>Pechu</title>"));
    }

    #[test]
    fn the_real_index_page_loses_its_google_links() {
        let page = Public::get("index.html").expect("public/index.html is embedded");
        let out = use_bundled_fonts(&String::from_utf8_lossy(&page.data));
        assert!(!out.contains("fonts.googleapis.com") && !out.contains("fonts.gstatic.com"));
        assert!(out.contains("/desktop-assets/fonts.css"));
    }

    #[test]
    fn the_native_audio_script_comes_before_every_other_script() {
        let html = "<head>\n<script src=\"/js/theme-init.js\"></script>\n</head>\n<body>\n  <script type=\"module\" src=\"/js/app.js\"></script>\n</body>\n";
        let out = add_native_audio(html);
        let ours = out.find("/desktop-assets/native-audio.js").expect("added");
        assert_eq!(out.matches("native-audio.js").count(), 1);
        assert!(ours < out.find("theme-init.js").unwrap() && ours < out.find("app.js").unwrap());
        let our_tag = out.find("<script src=\"/desktop-assets/native-audio.js\"").expect("our tag");
        assert!(!out[..our_tag].contains("<script")); // nothing runs before it
        assert_eq!(out.replace("<script src=\"/desktop-assets/native-audio.js\"></script>\n", ""), html); // nothing else changed
        assert_eq!(add_native_audio("<p>no scripts</p>"), "<p>no scripts</p>");
        let page = Public::get("index.html").expect("public/index.html is embedded");
        assert!(add_native_audio(&String::from_utf8_lossy(&page.data)).contains("/desktop-assets/native-audio.js"));
    }

    #[test]
    fn the_f11_script_is_added_once_after_the_audio_script_and_exists() {
        let page = Public::get("index.html").expect("public/index.html is embedded");
        let out = add_window_keys(&add_native_audio(&String::from_utf8_lossy(&page.data)));
        assert_eq!(out.matches("window-keys.js").count(), 1);
        assert!(out.find("native-audio.js").unwrap() < out.find("window-keys.js").unwrap());
        assert_eq!(add_window_keys("<p>no scripts</p>"), "<p>no scripts</p>");
        assert!(DesktopAssets::get("window-keys.js").is_some(), "the script is embedded");
    }

    #[test]
    fn the_window_buttons_come_after_the_f11_script_and_the_still_background_and_nothing_else_changes() {
        let html = "<html><head>\n<link rel=\"stylesheet\" href=\"/css/backdrop.css\">\n</head>\n<body>x</body></html>";
        let out = add_window_controls(&add_static_backdrop(html));
        assert!(out.find("static-backdrop.css").unwrap() < out.find("window-controls.css").unwrap(), "the stylesheet comes last");
        assert!(out.find("window-controls.css").unwrap() < out.find("</head>").unwrap());
        assert_eq!(out.matches("window-controls.css").count(), 1);
        assert_eq!(out.replace("<link rel=\"stylesheet\" href=\"/desktop-assets/window-controls.css\">\n", ""), add_static_backdrop(html));
        // the real page: the script lands right after the F11 script, once, and both files are embedded
        let page = Public::get("index.html").expect("public/index.html is embedded");
        let out = add_window_controls(&add_window_keys(&add_native_audio(&String::from_utf8_lossy(&page.data))));
        assert_eq!(out.matches("window-controls.js").count(), 1);
        assert!(out.find("window-keys.js").unwrap() < out.find("window-controls.js").unwrap());
        assert!(out.find("window-controls.js").unwrap() < out.find("/js/app.js").unwrap());
        assert_eq!(add_window_controls("<p>no head</p>"), "<p>no head</p>");
        assert!(DesktopAssets::get("window-controls.js").is_some(), "the script is embedded");
        let css = DesktopAssets::get("window-controls.css").expect("the stylesheet is embedded");
        assert!(String::from_utf8_lossy(&css.data).contains(".pw-controls"));
        assert_eq!(UiFiles::Embedded.serve("/desktop-assets/window-controls.css").headers()[header::CONTENT_TYPE], "text/css; charset=utf-8");
    }

    #[test]
    fn the_desktop_page_gets_a_still_background_and_nothing_else_changes() {
        let html = "<html><head>\n<link rel=\"stylesheet\" href=\"/css/backdrop.css\">\n</head>\n<body>x</body></html>";
        let out = add_static_backdrop(html);
        let ours = out.find("static-backdrop.css").expect("added");
        assert!(out.find("/css/backdrop.css").unwrap() < ours, "it comes after the page's own styles, so it wins");
        assert!(ours < out.find("</head>").unwrap());
        assert_eq!(out.replace("<link rel=\"stylesheet\" href=\"/desktop-assets/static-backdrop.css\">\n", ""), html);
        // the real page, and the stylesheet really switches off every animated layer
        let page = Public::get("index.html").expect("public/index.html is embedded");
        assert!(add_static_backdrop(&String::from_utf8_lossy(&page.data)).contains("/desktop-assets/static-backdrop.css"));
        let css = DesktopAssets::get("static-backdrop.css").expect("the stylesheet is embedded");
        assert!(String::from_utf8_lossy(&css.data).contains("animation: none"));
    }

    #[test]
    fn every_backdrop_picture_the_stylesheet_names_is_embedded_and_served_as_webp() {
        let css = DesktopAssets::get("static-backdrop.css").expect("the stylesheet is embedded");
        let css = String::from_utf8_lossy(&css.data).into_owned();
        let mut seen = 0;
        for theme in ["light", "dark"] {
            for layer in ["a", "b", "c"] {
                let name = format!("backdrop/{theme}-{layer}.webp");
                assert!(css.contains(&format!("url(/desktop-assets/{name})")), "{name} is not used by the stylesheet");
                let file = DesktopAssets::get(&name).unwrap_or_else(|| panic!("{name} is not embedded"));
                assert!(file.data.starts_with(b"RIFF") && file.data[8..12] == *b"WEBP", "{name} is not a WebP file");
                seen += 1;
            }
        }
        assert_eq!(seen, 6);
        let ui = UiFiles::Embedded;
        let response = ui.serve("/desktop-assets/backdrop/light-b.webp");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
    }

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        assert_eq!(clean_path("/js/app.js").as_deref(), Some("js/app.js"));
        assert_eq!(clean_path("/").as_deref(), Some(""));
        assert_eq!(clean_path("/../secret"), None);
        assert_eq!(clean_path("/js/../../secret"), None);
        assert_eq!(clean_path("/%2e%2e/secret"), None);
        assert_eq!(clean_path("/a%5cb"), None); // a backslash
        assert_eq!(clean_path("/bad%zz").as_deref(), Some("")); // does not decode: the index, like the web version
        #[cfg(windows)]
        assert_eq!(clean_path("/c:/windows/win.ini"), None); // a drive letter
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("js/app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("vendor/lenis.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("fonts/a.woff2"), "font/woff2");
        assert_eq!(content_type("a.svg"), "image/svg+xml");
        assert_eq!(content_type("backdrop/dark-a.webp"), "image/webp");
        assert_eq!(content_type("styles.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("mystery"), "application/octet-stream");
    }

    #[test]
    fn a_folder_serves_files_but_not_links_out_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ui");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "<html>home</html>").unwrap();
        std::fs::write(root.join("a.js"), "1").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "private").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), root.join("link.txt")).unwrap();
        let ui = UiFiles::Folder(root);
        assert_eq!(ui.read("a.js").as_deref(), Some(&b"1"[..]));
        assert_eq!(ui.read("missing.js"), None);
        #[cfg(unix)]
        assert_eq!(ui.read("link.txt"), None);
    }
}

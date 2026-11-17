//! Throwaway test mode (`PECHU_SPIKE=1`): shows a page that tests what the webview can do with audio
//! (microphone, devices, AudioWorklet, MediaRecorder) and prints the answers to stdout.
//! It is a plain TCP responder, not the real server, and is only a developer tool.
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PAGE: &str = include_str!("spike_page.html");

/// Starts the responder on a free port of 127.0.0.1 and returns that port.
pub async fn start() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                if let Err(err) = handle(stream).await {
                    eprintln!("spike server error: {err}");
                }
            });
        }
    });
    Ok(port)
}

/// Answers one request: `GET /` gives the test page, `POST /spike-result` prints the body.
async fn handle(mut stream: TcpStream) -> anyhow::Result<()> {
    // Read until the end of the headers, then as many body bytes as Content-Length says.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 || buf.len() > 64 * 1024 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0)
        .min(64 * 1024);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();

    let first_line = head.lines().next().unwrap_or("");
    let (status, content_type, payload) = if first_line.starts_with("GET / ") {
        ("200 OK", "text/html; charset=utf-8", PAGE.to_string())
    } else if first_line.starts_with("POST /spike-result") {
        println!("SPIKE {body}");
        ("200 OK", "text/plain", String::new())
    } else {
        ("404 Not Found", "text/plain", "not found".to_string())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

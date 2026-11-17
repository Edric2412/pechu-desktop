//! Starts the Pechu server on its own, for the contract tests in tests/contract (they talk to it over HTTP, exactly as
//! they talk to the web version). Settings come from environment variables:
//!   PECHU_DATA_DIR, PECHU_CONFIG_DIR   folders (both required)
//!   PORT                               preferred port (default: any free port)
//!   PECHU_TEST_GEMINI_URL, PECHU_TEST_SARVAM_URL, PECHU_TEST_GEMINI_LIVE_URL, PECHU_TEST_SARVAM_LIVE_URL
//!                                      point the paid services at fake servers
//!   PECHU_MEMORY_KEYS=1                keep API keys in memory only (never touch the real keychain)
//! It prints one line, `READY port=<n> token=<launch token>`, and then runs until it is killed.
use pechu_server::{serve, ServeConfig, TestOverrides};
use std::io::Write;
use std::path::PathBuf;

/// An environment variable that must be set.
fn need(name: &str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is not set"))
}

/// An optional variable: unset or empty means "not given".
fn opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let overrides = TestOverrides {
        gemini_base_url: opt("PECHU_TEST_GEMINI_URL"),
        sarvam_base_url: opt("PECHU_TEST_SARVAM_URL"),
        gemini_live_url: opt("PECHU_TEST_GEMINI_LIVE_URL"),
        sarvam_live_url: opt("PECHU_TEST_SARVAM_LIVE_URL"),
        memory_keys: opt("PECHU_MEMORY_KEYS").as_deref() == Some("1"),
        gemini_key: None,
    };
    let running = serve(ServeConfig {
        data_dir: PathBuf::from(need("PECHU_DATA_DIR")?),
        config_dir: PathBuf::from(need("PECHU_CONFIG_DIR")?),
        preferred_port: opt("PORT").and_then(|p| p.parse().ok()).unwrap_or(0),
        static_dir: None,
        test_overrides: Some(overrides),
    })
    .await?;

    // The test harness reads this line to learn where the server is and how to sign in.
    println!("READY port={} token={}", running.port, running.launch_token);
    std::io::stdout().flush()?;

    // Runs until the process is killed (the harness sends SIGTERM / SIGKILL).
    std::future::pending::<()>().await;
    Ok(())
}

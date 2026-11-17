# Pechu Desktop

**A meeting recorder and note-taker as a native desktop app, written in Rust.**
Record or import audio, transcribe it (Sarvam, or Gemini as the alternative), and get a summary, decisions, actions and flow charts.
One small program, no server to run, no Docker, no Bun, no GStreamer: everything the web version needed a stack for is inside the app.

This is the Rust rewrite of the back end of [Pechu](https://github.com/Edric2412/Pechu). The window shows the same UI as the web
version (it is embedded unchanged in `public/`); everything behind it is new.

## What makes it different

- **Rust all the way down.** Ten small crates plus a Tauri 2 window shell. The local server (axum), storage, the Gemini and Sarvam clients,
  live captions over WebSockets, audio decoding and system-sound capture are all Rust.
- **Fast to open.** The server answers 0.06 s after launch and the app shows its content about 0.9 s after launch in a release
  build (measured on Ubuntu with WebKitGTK; the first version took about 10 s because the web engine drew the page background on the CPU, so the
  background is now shown as pictures drawn once from the same drawing code).
- **Audio without external tools.** Any common format (mp3, wav, flac, ogg, m4a/aac, aiff, webm/Opus) is decoded in pure Rust
  (symphonia, an Opus decoder, a resampler) and cut into parts at the quietest moments, so a recording never needs ffmpeg or GStreamer.
- **Records "what the meeting app plays".** A "Monitor of this computer" input is added to the existing Device-audio switch. On Linux it
  captures the default output through PipeWire; a Windows backend (WASAPI loopback) is written but untested.
- **Careful with keys and billing.** Your own Gemini and Sarvam keys live in the OS keychain (a private file if there is none) and are never
  printed or logged. Live captions send audio only while the service is ready and never after you press stop or pause, because Sarvam bills by the
  audio it receives.
- **Safe by default.** The server listens on `127.0.0.1` only and the window gets in with a one-time token and an HttpOnly, SameSite=Strict cookie;
  Host, Origin and cross-site checks are ported from the web version; the page has no native powers (the Tauri permission list is empty), so
  even window buttons and F11 go through the local server.
- **Same data as the web version.** Meetings are folders with a `meeting.json` and the audio, in the same format, so unknown fields survive.

## How it is put together

```
src-tauri        the window (Tauri 2): starts the server, locks navigation, allows the microphone for its own page only
crates/server    axum: the same /api as the web version, the UI, security, live-captions socket, system-audio socket
crates/jobs      background jobs on a meeting (transcribe, notes, flow chart), one at a time per meeting
crates/store     meetings as files (atomic writes, nothing lost between threads)
crates/gemini    Google Gemini over HTTPS: models, translation, notes, flow charts, file transcription, retries
crates/sarvam    Sarvam batch transcription, with automatic cutting into short pieces when a recording is refused whole
crates/live      live captions over WebSockets (Sarvam, with Gemini taking over if Sarvam fails)
crates/audio     decoding, resampling and cutting audio
crates/capture   system sound: PipeWire on Linux, WASAPI loopback on Windows
crates/text      transcript helpers      crates/common   shared error and progress types
public/          the web UI, embedded in the program        assets/  fonts, background pictures, small desktop-only scripts
```

## Run it

You need the Rust toolchain and, on Ubuntu, the Tauri libraries (`libwebkit2gtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev libxdo-dev libssl-dev pkg-config`).

```sh
cd desktop
cargo run -p pechu-desktop        # opens the window; on first run it asks for a Gemini key (Sarvam: SARVAM_API_KEY)
cargo test --workspace --exclude pechu-desktop
```

Installers (Ubuntu): `cd desktop/src-tauri && cargo tauri build --bundles deb,appimage` (needs `cargo install tauri-cli --version "^2"`).
More detail, measurements and the list of switches are in [desktop/README.md](desktop/README.md).

## What has really been checked

| | |
| --- | --- |
| Rust tests (unit and integration, with fake services) | 283 pass, 0 lint warnings; 18 small Bun tests of the page scripts pass |
| Against the real Sarvam service | a 5 s clip transcribed in 4.2 s; live captions ready in 0.3 s, with exactly the audio that was sent counted |
| Against the real Gemini service | key check, model lookup, translation and notes |
| System-audio capture on Linux | a real tone was heard through PipeWire |
| Ubuntu installers | `.deb` and AppImage build and start |
| **Not checked yet** | Windows (it compiles; it has never run), macOS (not started), Gemini file transcription without a Sarvam key, Gemini live captions, very long recordings end to end |

## Status

Built and tested on Ubuntu first. It is a working app but not a finished product: no code signing, no auto-update, and the first run on Windows will
probably find problems. No licence has been chosen yet, so all rights are reserved for now.

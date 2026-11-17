# Pechu desktop

A Rust/Tauri 2 desktop version of Pechu. The app shell is in `src-tauri/`; it starts the local
server (`crates/server`) and opens a window that shows the same web UI as the browser version.

## Run

```sh
cargo run -p pechu-desktop                          # normal run
PECHU_STATIC_DIR=../public cargo run -p pechu-desktop   # use the UI files in ../public (no rebuild when you edit them)
```

Needs the Tauri system packages on Linux (libwebkit2gtk-4.1-dev, libayatana-appindicator3-dev, librsvg2-dev, libxdo-dev, libssl-dev).
Run a second copy and it only brings the first window to the front (single-instance).

## Where data lives

| What | Default | Override with |
| --- | --- | --- |
| Meetings (one folder each) | `<data dir>/Pechu/meetings` (Linux: `~/.local/share/Pechu/meetings`) | `PECHU_DATA_DIR` |
| `settings.json` (preferred port) and the key fallback file | `<config dir>/Pechu` (Linux: `~/.config/Pechu`) | `PECHU_CONFIG_DIR` |

The server listens on `127.0.0.1` only, first on port 47317; if that is taken it picks a free
port and the new port is saved in `settings.json`.
The window opens `http://127.0.0.1:<port>/?k=<one-time token>`; the token is how the window (and nothing else) gets in.

## How the shell is built

- `src-tauri/src/main.rs`: starts the server, then the window; stops the server cleanly on exit (waits for running jobs, up to 30 s).
- `src-tauri/src/webview.rs`: the window rules. Navigation is locked to our own origin; links to other sites
  (for example Google AI Studio) open in the system browser; the microphone is allowed only for our own page.
- `src-tauri/src/settings.rs`: reads and writes `settings.json`.
- `src-tauri/src/spike.rs` + `spike_page.html`: the audio test below (`PECHU_SPIKE=1`), a developer tool.
- `src-tauri/capabilities/default.json` has no permissions on purpose: the page cannot call any Tauri command.

## Spike results (Linux/WebKitGTK)

Run on 2026-11-16: Ubuntu, GNOME on Wayland, WebKitGTK 2.52.6 (Tauri 2.12, wry 0.57), page served from `http://127.0.0.1:<port>`.
Reproduce with `PECHU_SPIKE=1 cargo run -p pechu-desktop`; results print to stdout as `SPIKE ...` lines.

- **getUserMedia({audio:true}) works** without any prompt, once the app allows UserMedia for our own origin (see `webview.rs`). Track was the built-in microphone, `echoCancellation: true`.
- **Devices listed** (labels only): `Raptor Lake-P/U/H cAVS Digital Microphone`, `Raptor Lake-P/U/H cAVS Stereo Microphone`, `Pechu loopback`, plus one empty-label videoinput. One earlier run also listed three `HP-HPProBook4G1iR14inchNotebookPC-...` audio inputs. It was after a console message `Unable to open pipewire remote. Error: Timeout was reached`, so the list can vary.
- **Monitor/loopback inputs:** no input named `Monitor of ...` is listed. The only loopback-like input is the `Pechu loopback` device, which exists on this machine. Capturing what the speakers play (system audio) through the web API is therefore not available by default on Linux: it needs a loopback device or native capture.
- **AudioContext + AudioWorklet: available.** A worklet module loaded from a Blob URL and a node was created. The context starts `suspended` (needs `resume()` after a user action). Sample rate 44100.
- **getDisplayMedia exists** (`true`) but is denied by our permission rule (screen capture is never allowed).
- **MediaRecorder.isTypeSupported:** `audio/webm;codecs=opus` true, `audio/webm` true, `audio/ogg;codecs=opus` true, `audio/mp4` true.
- **Real 2-second recordings (what actually works):**
  - `audio/webm;codecs=opus`: works, about 49 KB, 4 chunks, reported type `audio/webm; codecs=opus`.
  - `audio/webm`: works, same result.
  - `audio/ogg;codecs=opus`: says supported, but the recording comes back typed `audio/mp4; codecs=opus` (so it is NOT Ogg). Do not trust it.
  - `audio/mp4`: says supported, but produced 0 bytes (broken).
  Use `audio/webm;codecs=opus`, which the web UI already prefers.

Conclusion: the browser-based recorder (microphone, AudioWorklet, MediaRecorder webm/opus) works in the Tauri window on Linux without native audio code. Only system-audio (speaker) capture was missing; see "System audio" below.

## System audio ("Device audio")

The web view does not list "Monitor of ..." inputs. So the server adds one script to the page (`assets/native-audio.js`, injected in `crates/server/src/assets.rs`) that adds a single input, "Monitor of this computer (system audio)", to the device list the UI already reads. When the UI opens it, the script plays the sound captured by the app (`crates/capture`, which runs PipeWire's `pw-record` on the default output) into a normal audio track. The UI files in `public/` are not changed at all. On Windows the same input is backed by WASAPI loopback (`crates/capture/src/windows.rs`): it is written and compiles for Windows, but it has never run on Windows. macOS (ScreenCaptureKit) is not written; there it says "not available on this system yet".

## The window (no title bar)

The window has no title bar, so the page fills it. That also means no minimize, maximize or close buttons: close with **Alt+F4**, move with **Super+drag** (GNOME), and **F11** toggles fullscreen. F11 works through `assets/window-keys.js` (added to the page by the server): the page cannot control the window, so it asks the local server (`POST /api/native/window/fullscreen`), which passes the request to the window shell (`WindowCommand` in `crates/server`, `run_window_command` in `src-tauri/src/webview.rs`). Checked in the real window with `PECHU_PROBE=1` (the page presses F11 itself and reports its size: 1469x928 before, 1536x960 = the whole screen after). To get the title bar back, remove `.decorations(false)` in `src-tauri/src/webview.rs`.

## Start-up speed (why the background is a set of pictures)

Measured on 2026-11-18 on this Ubuntu machine with `PECHU_PROBE=1` (a timing probe that is only added to the page when that variable is set): the Rust server answers 0.06 s after launch, but the window needed about 5 s to paint and 10 s to show the app, because the web engine (WebKitGTK) draws the page background's big blurred SVG layers on the CPU. Forcing the GPU, removing the frosted-glass blur or the slow drift did not remove the stall.

What the desktop app does now (the web version is unchanged): the background is shown as 6 pictures (3 layers x light/dark, `assets/backdrop/*.webp`, 370 KB in total) drawn once from the very same drawing code (`tools/bake-backdrop`), laid out exactly as the SVG was, and the slow drift is off. Result: the app shows its content about 0.9 s after launch (was about 10 s). Pixel difference to the live background: about 0.25 %, not visible. Re-run `cd desktop/tools/bake-backdrop && bun install && bun run bake` whenever `public/js/backdrop.js` or the `--bd-*` colours change.

Switches (environment variables): `PECHU_KEEP_MOTION=1` brings back the live moving SVG background (slow to open); `PECHU_PROBE=1` prints the page timings. The first `/api/status` call no longer waits for Google (it used to wait up to 10 s for the model list when a key is saved).

## What was built and how it is tested

| Crate | What it is | Port of (files in the original Bun/TypeScript Pechu repo) |
| --- | --- | --- |
| `text`, `audio` | transcript helpers; decoding any audio file in Rust (symphonia, incl. Opus) and cutting it at quiet moments | `server/text.ts`, `server/audio.ts` (GStreamer replaced) |
| `gemini`, `sarvam`, `live` | Google and Sarvam over HTTPS and WebSocket | `server/gemini.ts`, `sarvam.ts`, `live.ts` |
| `store`, `jobs` | meetings as files (same format as the web version); background jobs | `server/store.ts`, `jobs.ts` |
| `server` | the local web server, the same `/api` contract | `server/index.ts`, `security.ts`, `env.ts` |
| `capture` | system sound (Linux) | new |
| `src-tauri` | the window | new |

```sh
cd desktop && cargo test --workspace --exclude pechu-desktop   # all Rust tests (the window shell needs a screen, so it is only compiled in CI)
cd .. && bun test tests                                         # the small Bun tests of the page scripts (native-audio, window keys, window controls)
```

The Rust server was checked against the original TypeScript server with 111 shared HTTP tests (the same requests, the same expected
answers) in the original repo, [Edric2412/Pechu](https://github.com/Edric2412/Pechu). Those tests need that web server, so they are not part of this repository.

## Building installable packages (Ubuntu)

```sh
cd desktop/src-tauri && cargo tauri build --bundles deb,appimage   # needs `cargo install tauri-cli --version "^2"`
```

Output: `target/release/bundle/deb/Pechu_<version>_amd64.deb` and `.../appimage/Pechu_<version>_amd64.AppImage`. The `.deb` uses the system's GStreamer (it depends on `gstreamer1.0-plugins-base` and `-good`), which is what recording and playback need. The AppImage bundles the media framework too (`bundleMediaFramework` in `tauri.conf.json`); without it an AppImage prints "GStreamer element appsink not found" and may not record or play audio on machines that lack GStreamer. Building it needs the `patchelf` tool: either `sudo apt install patchelf`, or without root `apt download patchelf && dpkg-deb -x patchelf_*.deb root` and then put `root/usr/bin` on `PATH` for the build. Built and smoke-tested on 2026-11-18 (195 MB; it starts, serves the page, and the GStreamer warning is gone). Not tested: recording and playback from inside the AppImage on a machine without GStreamer.

## Not done yet (and why)

- **Checked for real on 2026-11-17** (examples called `real_check` in the `gemini`, `sarvam` and `live` crates, plus `tone_check` in `capture`; they print only the shape of the answers, never keys or what was said):
  - Gemini: key check, model discovery, a wrong key reported as invalid, translating a short Tamil/English text, and writing notes (title, sections, key points, decision, action, flow chart; all accepted by our validator). Done with `gemini-3.1-flash-lite`.
  - Sarvam batch: a 5 s clip came back as a timed Tamil transcript in 4.2 s.
  - Sarvam live captions: ready in 0.3 s, live updates and one final caption; the audio sent was exactly the clip (160,000 bytes = 5.0 s), which is what Sarvam bills. Sarvam never closes the connection after "end", so the app now closes it after 8 s (test added).
  - System audio: a real beep through PipeWire was captured (silence before = 0, during the tone = 4915 of 32767).
- **NOT checked for real:** transcription with Gemini (the path used when there is no Sarvam key), Gemini live captions, the "cut into pieces" path for long recordings, and a 4-hour meeting end to end. The newest Gemini models (`gemini-3.8-flash`, `gemini-3.6-flash`) answered "high demand" (503) for long stretches during testing, which the app retries and then reports plainly; one notes request on `gemini-3.6-flash` ended with a "could not reach the service" message whose technical cause was not captured. If it keeps happening, set `NOTES_MODEL` to another model.
- **No Sarvam key field in the UI.** The first-run page only asks for the Gemini key (the UI is unchanged on purpose). A Sarvam key can be given with the `SARVAM_API_KEY` environment variable when starting the app, or saved by sending `{"sarvamKey": "..."}` to `/api/key` (the server already accepts it; a settings screen is the missing piece).
- **Windows:** the whole workspace type-checks for Windows (`cargo check --target x86_64-pc-windows-gnu`; checked with placeholder C compilers, so nothing was linked or run), and a Windows job in `.github/workflows/desktop.yml` will build and test it for real on its first push. The first real Windows run will probably find problems. **macOS:** not started. **Both:** no code signing, no auto-update, no installer built yet.
- **The "share a browser tab" option** of the Record page is refused by the window on purpose; Device audio replaces it.
- A recording made in the app gets its length written into the WebM file, but not a seek index (the web version adds both with GStreamer).

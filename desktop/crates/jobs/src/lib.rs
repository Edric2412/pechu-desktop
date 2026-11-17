//! Background jobs on a meeting: transcribe, write notes, redraw the flow chart. Port of server/jobs.ts. A meeting never has two
//! jobs at once; progress and errors are written to the meeting (`status`, `progress`, `error`, `refineError`) and the page polls.
//! CONTRACT: the signatures below are used by other crates. You may add functions; do not change these.
//!
//! Shape of this file: `Jobs` decides WHAT happens to a meeting (statuses, messages, what is kept or cleared). The slow, paid
//! calls (Sarvam, Gemini) sit behind the small `Services` trait, so the tests can swap in a fake and check every transition
//! without a network.
use anyhow::anyhow;
use pechu_common::{describe, friendly, progress, user_err, Progress, TranscribeMode};
use pechu_store::Store;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

/// Where the jobs get API keys and settings from (read at the moment a job needs them, so a key saved later is picked up).
pub trait Keys: Send + Sync {
    fn gemini_key(&self) -> Option<String>;
    fn sarvam_key(&self) -> Option<String>;
    /// NOTES_MODEL override.
    fn notes_model(&self) -> Option<String> {
        None
    }
}

/// Test hooks: point the services at local fakes.
#[derive(Clone, Debug, Default)]
pub struct Endpoints {
    pub gemini_base_url: Option<String>,
    pub sarvam_base_url: Option<String>,
}

// ----- the services a job needs -----

/// A boxed future. A trait that has to be used as `dyn Services` cannot have `async fn`, so each method returns one of these.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Called with the English text as soon as it exists (so the page can show it before the notes are done).
pub type OnEnglish = Arc<dyn Fn(String) + Send + Sync>;

/// A finished transcript and the name of the service that wrote it (shown on the meeting).
pub struct Transcript {
    pub text: String,
    pub by: String,
}

/// The paid services, as the jobs see them. Hidden from the docs: only this crate and its tests use it.
#[doc(hidden)]
pub trait Services: Send + Sync {
    /// Audio file -> text. Checks the length limit before sending anything anywhere.
    fn transcribe(&self, path: PathBuf, mode: TranscribeMode, progress: Progress) -> BoxFuture<'_, anyhow::Result<Transcript>>;
    /// Translate to English, then write the notes. Returns (notes, english).
    fn generate_notes(&self, transcript: String, progress: Progress, on_english: OnEnglish) -> BoxFuture<'_, anyhow::Result<(Value, String)>>;
    /// Notes only, from an English transcript.
    fn notes_from_english(&self, english: String, progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>>;
    /// The meeting's notes with a new flow chart (and process chart) drawn from the English transcript; the rest stays.
    fn redraw_flow(&self, english: String, notes: Value, progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>>;
    /// Keys that must never appear in the log.
    fn secrets(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The real thing: Sarvam when a Sarvam key exists, otherwise Gemini.
struct RealServices {
    keys: Arc<dyn Keys>,
    endpoints: Endpoints,
}

fn non_empty(key: Option<String>) -> Option<String> {
    key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

impl RealServices {
    /// A Gemini client made fresh each time, so a key saved after startup is used by the next job.
    fn gemini(&self) -> anyhow::Result<pechu_gemini::Gemini> {
        let key = non_empty(self.keys.gemini_key()).ok_or_else(|| user_err("Add a Gemini API key in Settings first."))?;
        let mut gemini = pechu_gemini::Gemini::new(key).with_notes_model(self.keys.notes_model());
        if let Some(url) = &self.endpoints.gemini_base_url {
            gemini = gemini.with_base_url(url);
        }
        Ok(gemini)
    }
}

/// Runs blocking audio work (decoding) off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(work).await?
}

impl Services for RealServices {
    fn transcribe(&self, path: PathBuf, mode: TranscribeMode, progress: Progress) -> BoxFuture<'_, anyhow::Result<Transcript>> {
        Box::pin(async move {
            if let Some(key) = non_empty(self.keys.sarvam_key()) {
                // Over the limit: stop here with a clear message, nothing is sent to Sarvam.
                let check = path.clone();
                blocking(move || pechu_audio::audio_seconds(&check)).await?;
                let opts = pechu_sarvam::SarvamOpts {
                    mode: if mode == TranscribeMode::Verbatim { pechu_sarvam::SarvamMode::Verbatim } else { pechu_sarvam::SarvamMode::Codemix },
                    speakers: mode == TranscribeMode::Speakers,
                };
                let endpoint = pechu_sarvam::SarvamEndpoint { base_url: self.endpoints.sarvam_base_url.clone() };
                let text = pechu_sarvam::transcribe_with_sarvam(&key, &path, opts, progress, endpoint).await?;
                return Ok(Transcript { text, by: "Sarvam Saaras v4".into() });
            }
            // No Sarvam key: cut the audio into parts and let Gemini listen to each one.
            let gemini = self.gemini()?;
            let part_seconds = if mode == TranscribeMode::Speakers { pechu_audio::SPEAKER_PART_SECONDS } else { pechu_audio::PART_SECONDS };
            // `parts` must stay alive until Gemini is done: dropping it deletes the part files.
            let parts = blocking(move || pechu_audio::prepare_parts(&path, part_seconds)).await?;
            let text = gemini.transcribe_parts(&parts.files, mode, progress).await?;
            Ok(Transcript { text, by: "Gemini 3.5 Transcribe".into() })
        })
    }

    fn generate_notes(&self, transcript: String, progress: Progress, on_english: OnEnglish) -> BoxFuture<'_, anyhow::Result<(Value, String)>> {
        Box::pin(async move {
            let gemini = self.gemini()?;
            gemini.generate_notes(&transcript, progress, &*on_english).await
        })
    }

    fn notes_from_english(&self, english: String, progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>> {
        Box::pin(async move {
            let gemini = self.gemini()?;
            gemini.notes_from_english(&english, progress).await
        })
    }

    fn redraw_flow(&self, english: String, notes: Value, progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>> {
        Box::pin(async move {
            let title = notes["title"].as_str().unwrap_or("").to_string();
            let gemini = self.gemini()?;
            let drawn = gemini.flow_from_english(&english, &title, progress).await?;
            // Keep every note except the two charts, then put the new ones in and check the whole thing again.
            let mut next = notes;
            if let Some(map) = next.as_object_mut() {
                map.shift_remove("process");
                map.insert("flow".into(), drawn["flow"].clone());
                if !drawn["process"].is_null() {
                    map.insert("process".into(), drawn["process"].clone());
                }
            }
            pechu_gemini::validate_notes(&next)
        })
    }

    fn secrets(&self) -> Vec<String> {
        [self.keys.gemini_key(), self.keys.sarvam_key()].into_iter().filter_map(non_empty).collect()
    }
}

// ----- the jobs -----

/// The three short jobs that start from a transcript that already exists.
#[derive(Clone, Copy)]
enum Writing {
    Notes,
    NotesOnly,
    Flow,
}

struct Inner {
    store: Store,
    services: Arc<dyn Services>,
    /// Ids of meetings that have a job right now. Inserting into it is how a job is "claimed".
    running: Mutex<HashSet<String>>,
    /// Always holds the number of running jobs; `wait_idle` waits for it to reach 0.
    count: watch::Sender<usize>,
}

#[derive(Clone)]
pub struct Jobs {
    inner: Arc<Inner>,
}

impl Jobs {
    pub fn new(store: Store, keys: Arc<dyn Keys>, endpoints: Endpoints) -> Self {
        Self::with_services(store, Arc::new(RealServices { keys, endpoints }))
    }

    /// Tests give the jobs fake services here.
    #[doc(hidden)]
    pub fn with_services(store: Store, services: Arc<dyn Services>) -> Self {
        let (count, _) = watch::channel(0);
        Jobs { inner: Arc::new(Inner { store, services, running: Mutex::new(HashSet::new()), count }) }
    }

    /// How many jobs are running (the app waits for these when it closes, and refuses new ones past a limit).
    pub fn active_jobs(&self) -> usize {
        self.running().len()
    }

    /// Resolves when no job is running.
    pub async fn wait_idle(&self) {
        let mut seen = self.inner.count.subscribe();
        // wait_for checks the current value first, so a call made while idle returns at once.
        let _ = seen.wait_for(|n| *n == 0).await;
    }

    /// Translate the transcript to English and write the notes. Sets status "writing-notes" and returns the meeting at once;
    /// the work continues in the background. If a job is already running for this meeting nothing starts and the meeting is returned.
    pub fn start_notes(&self, id: &str) -> Option<Value> {
        // A live recording is written up from its live captions. Listening to the saved audio again (Sarvam, which costs
        // money) only happens when the person asks for it: see retranscribe().
        self.start_writing(id, Writing::Notes)
    }

    /// Notes only, from the stored English transcript (no translation).
    pub fn start_notes_only(&self, id: &str) -> Option<Value> {
        self.start_writing(id, Writing::NotesOnly)
    }

    /// Only the flow chart and the step-by-step chart; the rest of the notes stays.
    pub fn start_flow(&self, id: &str) -> Option<Value> {
        self.start_writing(id, Writing::Flow)
    }

    /// The meeting already exists with status "transcribing" and its audio saved: transcribe it, then write the notes.
    pub fn start_upload(&self, id: &str, mode: TranscribeMode) {
        if self.claim(id) {
            self.launch_transcribe(id, mode);
        }
    }

    /// Transcribe the saved audio again (for example to add speaker labels). Works for uploads and live recordings; sets status
    /// "transcribing", returns the meeting at once. Keeps the old transcript and notes until the new transcript is ready
    /// (a failure then keeps them and sets `refineError`).
    pub fn retranscribe(&self, id: &str, mode: TranscribeMode) -> Option<Value> {
        let meeting = self.inner.store.get_meeting(id)?;
        if meeting.get("audio").is_none_or(|a| a.is_null()) || !self.claim(id) {
            return Some(meeting);
        }
        // The transcript, the notes and the English version stay until a new transcript is ready to replace them.
        let current = self.inner.store.update_meeting(
            id,
            json!({ "status": "transcribing", "error": null, "progress": null, "refineError": null }),
        );
        if current.is_none() {
            self.release(id); // the meeting was deleted a moment ago
            return None;
        }
        self.launch_transcribe(id, mode);
        current
    }

    // ----- starting -----

    fn start_writing(&self, id: &str, kind: Writing) -> Option<Value> {
        let meeting = self.inner.store.get_meeting(id)?;
        if !self.claim(id) {
            return Some(meeting); // a job is already running: leave it alone
        }
        let current = self.inner.store.update_meeting(id, json!({ "status": "writing-notes", "error": null, "progress": null }));
        if current.is_none() {
            self.release(id);
            return None;
        }
        let (this, job_id) = (self.clone(), id.to_string());
        self.spawn_job(id, async move {
            match kind {
                Writing::Notes => this.write_notes(&job_id).await,
                Writing::NotesOnly => this.rewrite_notes(&job_id).await,
                Writing::Flow => this.redraw_flow(&job_id).await,
            }
        });
        current
    }

    /// The caller has already claimed `id`.
    fn launch_transcribe(&self, id: &str, mode: TranscribeMode) {
        let (this, job_id) = (self.clone(), id.to_string());
        self.spawn_job(id, async move { this.transcribe_then_notes(&job_id, mode).await });
    }

    /// Runs `job` on its own tokio task. Whatever happens (error, even a panic) the meeting ends up in a clear state and the
    /// claim on the meeting is given back.
    fn spawn_job(&self, id: &str, job: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let (this, id) = (self.clone(), id.to_string());
        tokio::spawn(async move {
            // The job runs in a second task so that a panic comes back here as an Err instead of killing this one.
            match tokio::spawn(job).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => this.fail(&id, &e),
                Err(crashed) => this.fail(&id, &anyhow!("the job crashed: {crashed}")),
            }
            this.release(&id);
        });
    }

    // ----- the claim on a meeting -----

    fn running(&self) -> MutexGuard<'_, HashSet<String>> {
        self.inner.running.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// true when the meeting had no job and now has one (check and insert happen under one lock, so two callers cannot both win).
    fn claim(&self, id: &str) -> bool {
        let mut running = self.running();
        let won = running.insert(id.to_string());
        if won {
            self.inner.count.send_replace(running.len());
        }
        won
    }

    fn release(&self, id: &str) {
        let mut running = self.running();
        running.remove(id);
        self.inner.count.send_replace(running.len());
    }

    // ----- what a job writes to the meeting -----

    /// The error is shown to the person (friendly text only); the full text goes to the log with keys hidden.
    fn fail(&self, id: &str, e: &anyhow::Error) {
        let mut log = describe(e);
        for secret in self.inner.services.secrets() {
            log = log.replace(&secret, &pechu_gemini::mask_key(&secret));
        }
        tracing::error!("[job {id}] {log}");
        self.inner.store.update_meeting(id, json!({ "status": "error", "error": friendly(e), "progress": null }));
    }

    /// A callback that stores "Part 2 of 3" on the meeting. Repeating the same note is not written again.
    fn progress_for(&self, id: &str) -> Progress {
        let (store, id) = (self.inner.store.clone(), id.to_string());
        let last = Mutex::new(String::new());
        progress(move |note| {
            let mut last = last.lock().unwrap_or_else(|p| p.into_inner());
            if *last != note {
                store.update_meeting(&id, json!({ "progress": note }));
                *last = note;
            }
        })
    }

    async fn write_notes(&self, id: &str) -> anyhow::Result<()> {
        let Some(meeting) = self.inner.store.get_meeting(id) else { return Ok(()) };
        let transcript = text_of(&meeting, "transcript");
        let (store, english_id) = (self.inner.store.clone(), id.to_string());
        let on_english: OnEnglish = Arc::new(move |english| {
            store.update_meeting(&english_id, json!({ "transcriptEnglish": english }));
        });
        let (notes, english) = self.inner.services.generate_notes(transcript, self.progress_for(id), on_english).await?;
        self.inner.store.update_meeting(
            id,
            json!({ "notes": notes, "transcriptEnglish": english, "status": "ready", "error": null, "progress": null }),
        );
        Ok(())
    }

    async fn rewrite_notes(&self, id: &str) -> anyhow::Result<()> {
        let Some(meeting) = self.inner.store.get_meeting(id) else { return Ok(()) };
        let english = text_of(&meeting, "transcriptEnglish");
        let notes = self.inner.services.notes_from_english(english, self.progress_for(id)).await?;
        self.inner.store.update_meeting(id, json!({ "notes": notes, "status": "ready", "error": null, "progress": null }));
        Ok(())
    }

    async fn redraw_flow(&self, id: &str) -> anyhow::Result<()> {
        let Some(meeting) = self.inner.store.get_meeting(id) else { return Ok(()) };
        let notes = meeting.get("notes").filter(|n| !n.is_null()).cloned();
        let Some(notes) = notes else {
            // Nothing to redraw. (The web version leaves the meeting stuck on "writing-notes" here; this one puts it back.)
            self.inner.store.update_meeting(id, json!({ "status": "ready", "progress": null }));
            return Ok(());
        };
        let english = text_of(&meeting, "transcriptEnglish");
        let notes = self.inner.services.redraw_flow(english, notes, self.progress_for(id)).await?;
        self.inner.store.update_meeting(id, json!({ "notes": notes, "status": "ready", "error": null, "progress": null }));
        Ok(())
    }

    async fn transcribe_then_notes(&self, id: &str, mode: TranscribeMode) -> anyhow::Result<()> {
        let store = &self.inner.store;
        let Some(meeting) = store.get_meeting(id) else { return Ok(()) };
        let had_transcript = !text_of(&meeting, "transcript").trim().is_empty();

        let transcribed = match store.audio_path(&meeting) {
            Some(path) => self.inner.services.transcribe(path, mode, self.progress_for(id)).await,
            None => Err(user_err("Pechu could not find the saved audio for this meeting.")),
        };
        let Transcript { text, by } = match transcribed {
            Ok(done) => done,
            Err(e) if !had_transcript => return Err(e), // nothing to fall back on: the meeting shows the error
            Err(e) => {
                // The meeting already has a transcript (and maybe notes): keep them, and tell the person why the new one was not made.
                tracing::error!("[job {id}] new transcript failed: {}", describe(&e));
                store.update_meeting(id, json!({ "status": "ready", "error": null, "progress": null, "refineError": friendly(&e) }));
                return Ok(());
            }
        };

        // A live recording keeps its live captions so they can still be read next to the new transcript.
        let mut patch = json!({
            "transcript": text,
            "transcriber": by,
            // The old notes and English version describe the old transcript, so they go now that it is replaced.
            "notes": null,
            "transcriptEnglish": null,
            "status": "writing-notes",
            "progress": null,
            "refineError": null,
        });
        let is_live = meeting["source"] == "live";
        if is_live && text_of(&meeting, "transcriptLive").is_empty() && had_transcript {
            patch["transcriptLive"] = meeting["transcript"].clone();
        }
        store.update_meeting(id, patch);
        self.write_notes(id).await
    }
}

/// A text field of the meeting, "" when it is missing.
fn text_of(meeting: &Value, key: &str) -> String {
    meeting.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// Stand-in for Sarvam and Gemini. Counts calls, can be told to fail, and can hold a job open until the test lets it go.
    #[derive(Default)]
    struct Fake {
        transcribe_calls: AtomicUsize,
        notes_calls: AtomicUsize,
        fail_transcribe: AtomicBool,
        fail_notes: AtomicBool,
        crash_notes: AtomicBool,
        /// when set, `transcribe` and `generate_notes` wait here after reporting progress
        gate: Option<Arc<Notify>>,
    }

    impl Fake {
        fn gated() -> (Arc<Fake>, Arc<Notify>) {
            let gate = Arc::new(Notify::new());
            (Arc::new(Fake { gate: Some(gate.clone()), ..Fake::default() }), gate)
        }
        async fn hold(&self) {
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
        }
    }

    impl Services for Fake {
        fn transcribe(&self, _path: PathBuf, mode: TranscribeMode, progress: Progress) -> BoxFuture<'_, anyhow::Result<Transcript>> {
            Box::pin(async move {
                self.transcribe_calls.fetch_add(1, Ordering::SeqCst);
                progress("Part 1 of 1".into());
                self.hold().await;
                if self.fail_transcribe.load(Ordering::SeqCst) {
                    return Err(user_err("could not reach Sarvam"));
                }
                Ok(Transcript { text: format!("new transcript ({mode:?})"), by: "Fake Transcribe".into() })
            })
        }
        fn generate_notes(&self, transcript: String, progress: Progress, on_english: OnEnglish) -> BoxFuture<'_, anyhow::Result<(Value, String)>> {
            Box::pin(async move {
                self.notes_calls.fetch_add(1, Ordering::SeqCst);
                progress("Writing notes".into());
                on_english(format!("english of: {transcript}"));
                self.hold().await;
                if self.crash_notes.load(Ordering::SeqCst) {
                    panic!("the fake blew up");
                }
                if self.fail_notes.load(Ordering::SeqCst) {
                    return Err(anyhow!("raw secret details"));
                }
                Ok((json!({ "title": format!("notes of: {transcript}"), "flow": { "label": "old flow" } }), format!("english of: {transcript}")))
            })
        }
        fn notes_from_english(&self, english: String, _progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>> {
            Box::pin(async move {
                self.notes_calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "title": format!("rewritten from: {english}") }))
            })
        }
        fn redraw_flow(&self, english: String, mut notes: Value, _progress: Progress) -> BoxFuture<'_, anyhow::Result<Value>> {
            Box::pin(async move {
                notes["flow"] = json!({ "label": format!("flow from: {english}") });
                Ok(notes)
            })
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        store: Store,
        jobs: Jobs,
        fake: Arc<Fake>,
    }

    fn setup_with(fake: Arc<Fake>) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("meetings"));
        let jobs = Jobs::with_services(store.clone(), fake.clone());
        Setup { _dir: dir, store, jobs, fake }
    }

    fn setup() -> Setup {
        setup_with(Arc::new(Fake::default()))
    }

    impl Setup {
        fn meeting(&self, init: Value) -> String {
            let mut base = json!({ "source": "upload", "status": "ready", "transcript": "" });
            for (k, v) in init.as_object().unwrap() {
                base[k] = v.clone();
            }
            self.store.create_meeting(base).unwrap()["id"].as_str().unwrap().to_string()
        }
        /// A meeting with a saved audio file, like one that was uploaded or recorded.
        fn meeting_with_audio(&self, init: Value) -> String {
            let id = self.meeting(init);
            let src = self.store.tmp_dir().join(format!("src-{id}"));
            std::fs::write(&src, b"not really audio").unwrap();
            self.store.save_audio_file(&id, &src, "wav").unwrap().unwrap();
            id
        }
        fn get(&self, id: &str) -> Value {
            self.store.get_meeting(id).unwrap()
        }
        async fn finish(&self) {
            tokio::time::timeout(Duration::from_secs(5), self.jobs.wait_idle()).await.expect("jobs did not finish");
        }
    }

    // ----- notes, notes only, flow -----

    #[tokio::test]
    async fn start_notes_writes_notes_and_clears_the_old_error() {
        let s = setup();
        let id = s.meeting(json!({ "transcript": "hello", "status": "error", "error": "old", "progress": "old" }));
        let at_once = s.jobs.start_notes(&id).unwrap();
        assert_eq!(at_once["status"], "writing-notes");
        assert!(at_once.get("error").is_none());
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["notes"]["title"], "notes of: hello");
        assert_eq!(m["transcriptEnglish"], "english of: hello");
        assert!(m.get("error").is_none() && m.get("progress").is_none());
    }

    #[tokio::test]
    async fn start_notes_failure_shows_a_friendly_error_never_raw_text() {
        let s = setup();
        s.fake.fail_notes.store(true, Ordering::SeqCst);
        let id = s.meeting(json!({ "transcript": "hello" }));
        s.jobs.start_notes(&id);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "error");
        let shown = m["error"].as_str().unwrap();
        assert!(shown.starts_with("Something went wrong"), "{shown}");
        assert!(!shown.contains("secret"));
        assert!(m.get("progress").is_none());
    }

    #[tokio::test]
    async fn a_crashing_job_ends_as_an_error_and_gives_the_meeting_back() {
        let s = setup();
        s.fake.crash_notes.store(true, Ordering::SeqCst);
        let id = s.meeting(json!({ "transcript": "hello" }));
        s.jobs.start_notes(&id);
        s.finish().await;
        assert_eq!(s.get(&id)["status"], "error");
        assert_eq!(s.jobs.active_jobs(), 0);
        s.fake.crash_notes.store(false, Ordering::SeqCst);
        s.jobs.start_notes(&id); // the meeting can be tried again
        s.finish().await;
        assert_eq!(s.get(&id)["status"], "ready");
    }

    #[tokio::test]
    async fn start_notes_only_uses_the_stored_english() {
        let s = setup();
        let id = s.meeting(json!({ "transcript": "tamil text", "transcriptEnglish": "stored english" }));
        assert_eq!(s.jobs.start_notes_only(&id).unwrap()["status"], "writing-notes");
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["notes"]["title"], "rewritten from: stored english");
        assert_eq!(m["transcriptEnglish"], "stored english");
    }

    #[tokio::test]
    async fn start_flow_replaces_only_the_flow() {
        let s = setup();
        let id = s.meeting(json!({ "transcriptEnglish": "the english", "notes": { "title": "Keep me", "summary": "kept", "flow": { "label": "old" } } }));
        s.jobs.start_flow(&id);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["notes"]["title"], "Keep me");
        assert_eq!(m["notes"]["summary"], "kept");
        assert_eq!(m["notes"]["flow"]["label"], "flow from: the english");
    }

    #[tokio::test]
    async fn start_flow_without_notes_does_not_leave_the_meeting_stuck() {
        let s = setup();
        let id = s.meeting(json!({ "transcript": "x" }));
        s.jobs.start_flow(&id);
        s.finish().await;
        assert_eq!(s.get(&id)["status"], "ready");
    }

    #[tokio::test]
    async fn unknown_meetings_return_none() {
        let s = setup();
        assert!(s.jobs.start_notes("nope-nope").is_none());
        assert!(s.jobs.start_notes_only("nope-nope").is_none());
        assert!(s.jobs.start_flow("nope-nope").is_none());
        assert!(s.jobs.retranscribe("nope-nope", TranscribeMode::Smart).is_none());
        assert_eq!(s.jobs.active_jobs(), 0);
    }

    // ----- upload and retranscribe -----

    #[tokio::test]
    async fn start_upload_transcribes_then_writes_notes() {
        let s = setup();
        let id = s.meeting_with_audio(json!({ "status": "transcribing" }));
        s.jobs.start_upload(&id, TranscribeMode::Speakers);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["transcript"], "new transcript (Speakers)");
        assert_eq!(m["transcriber"], "Fake Transcribe");
        assert_eq!(m["notes"]["title"], "notes of: new transcript (Speakers)");
        assert!(m.get("transcriptLive").is_none() && m.get("refineError").is_none() && m.get("progress").is_none());
    }

    #[tokio::test]
    async fn a_first_transcription_that_fails_shows_an_error() {
        let s = setup();
        s.fake.fail_transcribe.store(true, Ordering::SeqCst);
        let id = s.meeting_with_audio(json!({ "status": "transcribing" }));
        s.jobs.start_upload(&id, TranscribeMode::Smart);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "error");
        assert_eq!(m["error"], "could not reach Sarvam");
        assert_eq!(m["transcript"], "");
        assert!(m.get("refineError").is_none());
    }

    #[tokio::test]
    async fn a_first_transcription_with_missing_audio_shows_an_error() {
        let s = setup();
        let id = s.meeting(json!({ "status": "transcribing" })); // no audio saved
        s.jobs.start_upload(&id, TranscribeMode::Smart);
        s.finish().await;
        assert_eq!(s.get(&id)["status"], "error");
        assert_eq!(s.fake.transcribe_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retranscribing_a_live_recording_keeps_the_captions_and_replaces_the_notes() {
        let s = setup();
        let id = s.meeting_with_audio(json!({
            "source": "live", "transcript": "live captions", "transcriptEnglish": "old english", "notes": { "title": "old notes" }, "error": "x",
        }));
        let at_once = s.jobs.retranscribe(&id, TranscribeMode::Smart).unwrap();
        // the old transcript, notes and English stay while the new one is being made
        assert_eq!(at_once["status"], "transcribing");
        assert_eq!(at_once["transcript"], "live captions");
        assert_eq!(at_once["notes"]["title"], "old notes");
        assert!(at_once.get("error").is_none());
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["transcriptLive"], "live captions");
        assert_eq!(m["transcript"], "new transcript (Smart)");
        assert_eq!(m["notes"]["title"], "notes of: new transcript (Smart)");
        assert_eq!(m["transcriptEnglish"], "english of: new transcript (Smart)");
    }

    #[tokio::test]
    async fn the_live_captions_are_kept_only_the_first_time() {
        let s = setup();
        let id = s.meeting_with_audio(json!({ "source": "live", "transcript": "live captions" }));
        s.jobs.retranscribe(&id, TranscribeMode::Smart);
        s.finish().await;
        s.jobs.retranscribe(&id, TranscribeMode::Speakers);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["transcriptLive"], "live captions"); // not overwritten by the first re-transcription
        assert_eq!(m["transcript"], "new transcript (Speakers)");
    }

    #[tokio::test]
    async fn retranscribing_an_upload_does_not_make_live_captions() {
        let s = setup();
        let id = s.meeting_with_audio(json!({ "source": "upload", "transcript": "first transcript" }));
        s.jobs.retranscribe(&id, TranscribeMode::Verbatim);
        s.finish().await;
        let m = s.get(&id);
        assert!(m.get("transcriptLive").is_none());
        assert_eq!(m["transcript"], "new transcript (Verbatim)");
    }

    #[tokio::test]
    async fn a_failed_improvement_keeps_the_transcript_and_notes_and_says_why() {
        let s = setup();
        s.fake.fail_transcribe.store(true, Ordering::SeqCst);
        let id = s.meeting_with_audio(json!({
            "source": "live", "transcript": "live captions", "transcriptEnglish": "english", "notes": { "title": "Kept notes" },
        }));
        s.jobs.retranscribe(&id, TranscribeMode::Smart);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert_eq!(m["transcript"], "live captions");
        assert_eq!(m["notes"]["title"], "Kept notes");
        assert_eq!(m["transcriptEnglish"], "english");
        assert_eq!(m["refineError"], "could not reach Sarvam");
        assert!(m.get("transcriptLive").is_none() && m.get("error").is_none() && m.get("progress").is_none());
        assert_eq!(s.fake.notes_calls.load(Ordering::SeqCst), 0);

        // trying again clears the old refineError at once
        s.fake.fail_transcribe.store(false, Ordering::SeqCst);
        let again = s.jobs.retranscribe(&id, TranscribeMode::Smart).unwrap();
        assert!(again.get("refineError").is_none());
        s.finish().await;
        assert_eq!(s.get(&id)["transcript"], "new transcript (Smart)");
    }

    #[tokio::test]
    async fn notes_failing_after_a_new_transcript_leaves_an_error_with_the_new_transcript() {
        let s = setup();
        s.fake.fail_notes.store(true, Ordering::SeqCst);
        let id = s.meeting_with_audio(json!({ "transcript": "old", "notes": { "title": "old notes" } }));
        s.jobs.retranscribe(&id, TranscribeMode::Smart);
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "error");
        assert_eq!(m["transcript"], "new transcript (Smart)");
        assert!(m.get("notes").is_none()); // the old notes described the old transcript
    }

    #[tokio::test]
    async fn retranscribe_without_audio_changes_nothing() {
        let s = setup();
        let id = s.meeting(json!({ "transcript": "text" }));
        let m = s.jobs.retranscribe(&id, TranscribeMode::Smart).unwrap();
        assert_eq!(m["status"], "ready");
        assert_eq!(s.jobs.active_jobs(), 0);
        assert_eq!(s.fake.transcribe_calls.load(Ordering::SeqCst), 0);
    }

    // ----- one job at a time, progress, wait_idle -----

    #[tokio::test]
    async fn a_second_start_on_the_same_meeting_does_not_start_a_second_job() {
        let (fake, gate) = Fake::gated();
        let s = setup_with(fake);
        let id = s.meeting_with_audio(json!({ "transcript": "hello" }));

        assert_eq!(s.jobs.start_notes(&id).unwrap()["status"], "writing-notes");
        assert_eq!(s.jobs.active_jobs(), 1);
        // every other way of starting is refused while the first job runs, and each returns the meeting
        assert_eq!(s.jobs.start_notes(&id).unwrap()["id"], id.as_str());
        assert!(s.jobs.start_notes_only(&id).is_some());
        assert!(s.jobs.start_flow(&id).is_some());
        assert!(s.jobs.retranscribe(&id, TranscribeMode::Smart).is_some());
        s.jobs.start_upload(&id, TranscribeMode::Smart);
        assert_eq!(s.jobs.active_jobs(), 1);

        gate.notify_one();
        s.finish().await;
        assert_eq!(s.fake.notes_calls.load(Ordering::SeqCst), 1);
        assert_eq!(s.fake.transcribe_calls.load(Ordering::SeqCst), 0);
        assert_eq!(s.jobs.active_jobs(), 0);
        assert_eq!(s.get(&id)["status"], "ready");
        // once the job is over a new one can start
        gate.notify_one(); // let the second job through at once
        s.jobs.start_notes(&id);
        s.finish().await;
        assert_eq!(s.fake.notes_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn progress_notes_are_written_to_the_meeting_while_the_job_runs() {
        let (fake, gate) = Fake::gated();
        let s = setup_with(fake);
        let id = s.meeting_with_audio(json!({ "status": "transcribing" }));
        s.jobs.start_upload(&id, TranscribeMode::Smart);
        let mut seen = false;
        for _ in 0..100 {
            if s.get(&id)["progress"] == "Part 1 of 1" {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen, "the progress note never appeared");
        assert_eq!(s.get(&id)["status"], "transcribing");
        gate.notify_one(); // transcription ends...
        gate.notify_one(); // ...and so does the notes step
        s.finish().await;
        let m = s.get(&id);
        assert_eq!(m["status"], "ready");
        assert!(m.get("progress").is_none());
    }

    #[tokio::test]
    async fn the_english_text_is_stored_as_soon_as_it_exists() {
        let (fake, gate) = Fake::gated();
        let s = setup_with(fake);
        let id = s.meeting(json!({ "transcript": "hello" }));
        s.jobs.start_notes(&id);
        let mut seen = false;
        for _ in 0..100 {
            if s.get(&id)["transcriptEnglish"] == "english of: hello" {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen);
        assert_eq!(s.get(&id)["status"], "writing-notes"); // not finished yet
        gate.notify_one();
        s.finish().await;
    }

    #[tokio::test]
    async fn wait_idle_returns_at_once_when_nothing_runs_and_waits_for_every_job() {
        let (fake, gate) = Fake::gated();
        let s = setup_with(fake);
        tokio::time::timeout(Duration::from_secs(1), s.jobs.wait_idle()).await.expect("idle at the start");

        let a = s.meeting(json!({ "transcript": "a" }));
        let b = s.meeting(json!({ "transcript": "b" }));
        s.jobs.start_notes(&a);
        s.jobs.start_notes(&b);
        assert_eq!(s.jobs.active_jobs(), 2);
        let waiting = tokio::spawn({
            let jobs = s.jobs.clone();
            async move { jobs.wait_idle().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "wait_idle returned while jobs were running");
        gate.notify_one();
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("wait_idle never returned").unwrap();
        assert_eq!(s.jobs.active_jobs(), 0);
        assert_eq!(s.get(&a)["status"], "ready");
        assert_eq!(s.get(&b)["status"], "ready");
    }
}

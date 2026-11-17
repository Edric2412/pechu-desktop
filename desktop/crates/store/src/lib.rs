//! Meetings stored as files: `<data dir>/<id>/meeting.json` plus `audio.<ext>`. Same on-disk format as the web version
//! (server/store.ts) so a meeting can be copied between the two. A meeting is kept as a `serde_json::Value` (an object) so
//! fields this code does not know about (for example `usage`, `transcriptLive`) survive every read and write.
//! CONTRACT: the signatures below are used by other crates. You may add functions; do not change these.
use anyhow::{anyhow, Context};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cheap to clone (shared inside). All methods are synchronous and a read-modify-write never interleaves with another.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    /// Every read and write of a meeting takes this lock first. The lock holds no data; it only makes the threads
    /// take turns, so two `update_meeting` calls can never both read the old file and then overwrite each other.
    lock: Arc<Mutex<()>>,
}

/// Extension -> MIME type stored on the meeting (the same table as `EXT_TO_MIME` in store.ts).
const EXT_TO_MIME: [(&str, &str); 9] = [
    ("webm", "audio/webm"),
    ("ogg", "audio/ogg"),
    ("mp4", "audio/mp4"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("flac", "audio/flac"),
    ("aiff", "audio/aiff"),
];

/// Content-Type -> extension (`MIME_TO_EXT` in store.ts).
const MIME_TO_EXT: [(&str, &str); 22] = [
    ("audio/webm", "webm"),
    ("video/webm", "webm"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "ogg"),
    ("audio/mp4", "mp4"),
    ("video/mp4", "mp4"),
    ("audio/mpeg", "mp3"),
    ("audio/mp3", "mp3"),
    ("audio/wav", "wav"),
    ("audio/x-wav", "wav"),
    ("audio/wave", "wav"),
    ("audio/m4a", "m4a"),
    ("audio/x-m4a", "m4a"),
    ("audio/aac", "aac"),
    ("audio/x-aac", "aac"),
    ("audio/aacp", "aac"),
    ("audio/x-hx-aac-adts", "aac"),
    ("audio/vnd.dlna.adts", "aac"),
    ("audio/flac", "flac"),
    ("audio/x-flac", "flac"),
    ("audio/aiff", "aiff"),
    ("audio/x-aiff", "aiff"),
];

/// Extensions that are stored under another name (Ogg containers, AIFF spelling).
const EXT_ALIAS: [(&str, &str); 4] = [("oga", "ogg"), ("opus", "ogg"), ("aif", "aiff"), ("mpga", "mp3")];

/// Looks a key up in one of the small tables above.
fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

impl Store {
    /// `root` is the folder that holds one folder per meeting. Created if missing.
    pub fn new(root: PathBuf) -> Self {
        if let Err(e) = fs::create_dir_all(&root) {
            // Not fatal here: the first write will report the real problem to the caller.
            tracing::warn!("could not create data folder {}: {e}", root.display());
        }
        Store { root, lock: Arc::new(Mutex::new(())) }
    }

    /// `^[a-z0-9-]{4,36}$` (this is what keeps ids from reaching other folders).
    pub fn id_ok(id: &str) -> bool {
        // len() counts bytes, which is right here: every allowed character is one byte.
        (4..=36).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// `init` is an object (needs source/status/transcript; may carry more). Adds id (first 8 chars of a v4 UUID), title
    /// "Untitled meeting", createdAt (ISO 8601 UTC with milliseconds and "Z"), durationSec null where missing. Written atomically
    /// (meeting.json.tmp then rename), pretty-printed with 2 spaces like JSON.stringify(m, null, 2). Returns the stored meeting.
    pub fn create_meeting(&self, init: Value) -> anyhow::Result<Value> {
        let Value::Object(init) = init else {
            return Err(anyhow!("a meeting must be a JSON object"));
        };
        let _guard = self.guard();

        // Defaults first, then the caller's fields on top (a field that exists keeps its place and takes the caller's value).
        let mut meeting = Map::new();
        meeting.insert("id".into(), json!(self.fresh_id()));
        meeting.insert("title".into(), json!("Untitled meeting"));
        meeting.insert("createdAt".into(), json!(now_iso()));
        meeting.insert("durationSec".into(), Value::Null);
        for (key, value) in init {
            meeting.insert(key, value);
        }
        let meeting = Value::Object(meeting);

        self.write(&meeting)?;
        Ok(meeting)
    }

    pub fn get_meeting(&self, id: &str) -> Option<Value> {
        let _guard = self.guard();
        self.read(id)
    }

    /// Merges `patch` (an object) into the meeting; `id` is never changed; a patch value of `null` REMOVES the key (this is what
    /// `updateMeeting(id, {error: undefined})` does in the web version). Returns the new meeting, or None when it does not exist.
    pub fn update_meeting(&self, id: &str, patch: Value) -> Option<Value> {
        let _guard = self.guard();
        self.update_locked(id, patch)
    }

    /// Removes the meeting folder. false when it did not exist.
    pub fn delete_meeting(&self, id: &str) -> bool {
        let _guard = self.guard();
        if self.read(id).is_none() {
            return false;
        }
        match fs::remove_dir_all(self.root.join(id)) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!("could not delete meeting {id}: {e}");
                false
            }
        }
    }

    /// Newest first (createdAt). Each item has ONLY: id, title, source, createdAt, durationSec, status, hasNotes (bool).
    pub fn list_meetings(&self) -> Vec<Value> {
        let _guard = self.guard();
        self.list_locked()
    }

    /// After a restart anything still "transcribing" or "writing-notes" is dead: set status "error" and
    /// error "Stopped before it finished. Try again.". Returns how many.
    pub fn fail_interrupted(&self) -> usize {
        // One lock for the whole sweep, so a job that starts meanwhile cannot be marked dead by mistake.
        let _guard = self.guard();
        let mut count = 0;
        for summary in self.list_locked() {
            let status = summary["status"].as_str().unwrap_or("");
            if status == "transcribing" || status == "writing-notes" {
                let Some(id) = summary["id"].as_str() else { continue };
                let patch = json!({ "status": "error", "error": "Stopped before it finished. Try again." });
                if self.update_locked(id, patch).is_some() {
                    count += 1;
                }
            }
        }
        count
    }

    /// `audioExt` from store.ts: the file extension for a Content-Type (parameters like ;codecs=opus ignored), trusting the file
    /// name's extension for empty/generic/unknown audio types. None = not an accepted audio type.
    pub fn audio_ext(content_type: &str, file_name: &str) -> Option<&'static str> {
        let kind = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
        if let Some(ext) = lookup(&MIME_TO_EXT, &kind) {
            return Some(ext);
        }
        // Browsers label some files oddly (AAC is often "audio/x-hx-aac-adts") or not at all, so for an empty, generic or
        // unrecognised audio type we trust the file name's extension instead.
        if kind.is_empty() || kind == "application/octet-stream" || kind.starts_with("audio/") {
            // Like the web version, a name without a dot counts as its own extension ("mp3" alone is accepted).
            let raw = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
            let ext = lookup(&EXT_ALIAS, &raw).unwrap_or(raw.as_str());
            // Return the table's own &'static str, not the borrowed text.
            return EXT_TO_MIME.iter().find(|(e, _)| *e == ext).map(|(e, _)| *e);
        }
        None
    }

    /// The MIME type stored for an extension (webm -> audio/webm, m4a -> audio/mp4, ...).
    pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
        lookup(&EXT_TO_MIME, ext)
    }

    /// Moves `src` (a file already written, for example a streamed upload in a temp file on the same disk or copied across) to
    /// `<id>/audio.<ext>`, removing any other `audio.*` first, and sets `audio: {file, mime}` on the meeting. Returns the
    /// meeting, or None when the meeting does not exist or `ext` is not accepted.
    pub fn save_audio_file(&self, id: &str, src: &Path, ext: &str) -> anyhow::Result<Option<Value>> {
        let Some(mime) = Self::mime_for_ext(ext) else { return Ok(None) };
        let _guard = self.guard();
        if self.read(id).is_none() {
            return Ok(None);
        }
        let dir = self.root.join(id);

        // Only one audio file per meeting: a new recording or format replaces the old one.
        for entry in fs::read_dir(&dir).context("could not read the meeting folder")?.flatten() {
            if entry.file_name().to_string_lossy().starts_with("audio.") {
                // remove_file on a symlink removes the link itself, never what it points to.
                fs::remove_file(entry.path()).context("could not remove the old audio file")?;
            }
        }

        let file = format!("audio.{ext}");
        let dest = dir.join(&file);
        // (the retry matters on Windows: the old audio.<ext> we just removed lingers while a player still streams it,
        // and the same name cannot be taken again until that player lets go)
        if rename_with_retry(src, &dest).is_err() {
            // rename cannot cross disks; copy then delete the source instead.
            fs::copy(src, &dest).context("could not save the audio file")?;
            let _ = fs::remove_file(src);
        }
        Ok(self.update_locked(id, json!({ "audio": { "file": file, "mime": mime } })))
    }

    /// Where the meeting's audio file is, if it has one: only a regular file named `audio.<alnum>` inside the meeting's folder
    /// (a symlink or special file is never returned).
    pub fn audio_path(&self, meeting: &Value) -> Option<PathBuf> {
        let id = meeting["id"].as_str().filter(|id| Self::id_ok(id))?;
        let file = meeting["audio"]["file"].as_str()?;
        // `audio.` followed by one or more of a-z0-9: no slashes, no "..", nothing that leaves the folder.
        let name_ok = file.strip_prefix("audio.").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
        if !name_ok {
            return None;
        }
        let dir = self.root.join(id);
        // symlink_metadata does NOT follow links, so a link in place of the folder or file is seen as a link.
        if !fs::symlink_metadata(&dir).ok()?.is_dir() {
            return None;
        }
        let path = dir.join(file);
        fs::symlink_metadata(&path).ok()?.is_file().then_some(path)
    }

    /// The folder for temporary files that must be on the same disk as the data (uploads are streamed here first).
    pub fn tmp_dir(&self) -> PathBuf {
        // The leading dot means it is never a valid meeting id, so list_meetings ignores it.
        let dir = self.root.join(".tmp");
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!("could not create {}: {e}", dir.display());
        }
        dir
    }

    // ----- private helpers (the caller already holds the lock) -----

    /// Waits for the lock. If a thread panicked while holding it the data on disk is still fine, so carry on.
    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An id not used yet (8 hex characters from a v4 UUID: collisions are very unlikely, but cheap to rule out).
    fn fresh_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
            if !self.root.join(&id).exists() {
                return id;
            }
        }
    }

    fn read(&self, id: &str) -> Option<Value> {
        if !Self::id_ok(id) {
            return None;
        }
        let text = fs::read_to_string(self.root.join(id).join("meeting.json")).ok()?;
        let meeting: Value = serde_json::from_str(&text).ok()?;
        meeting.is_object().then_some(meeting)
    }

    /// Writes meeting.json.tmp, then renames it over meeting.json, so a crash never leaves half a file.
    fn write(&self, meeting: &Value) -> anyhow::Result<()> {
        let id = meeting["id"].as_str().filter(|id| Self::id_ok(id)).ok_or_else(|| anyhow!("invalid meeting id"))?;
        let dir = self.root.join(id);
        fs::create_dir_all(&dir).context("could not create the meeting folder")?;
        let tmp = dir.join("meeting.json.tmp");
        // to_string_pretty indents with 2 spaces, the same as JSON.stringify(m, null, 2).
        fs::write(&tmp, serde_json::to_string_pretty(meeting)?).context("could not write meeting.json.tmp")?;
        rename_with_retry(&tmp, &dir.join("meeting.json")).context("could not replace meeting.json")
    }

    fn update_locked(&self, id: &str, patch: Value) -> Option<Value> {
        let mut meeting = self.read(id)?;
        if let (Some(fields), Value::Object(patch)) = (meeting.as_object_mut(), patch) {
            for (key, value) in patch {
                if key == "id" {
                    continue; // the id never changes
                }
                if value.is_null() {
                    fields.shift_remove(&key); // keeps the order of the other keys
                } else {
                    fields.insert(key, value);
                }
            }
        }
        match self.write(&meeting) {
            Ok(()) => Some(meeting),
            Err(e) => {
                tracing::error!("could not save meeting {id}: {e:#}");
                None
            }
        }
    }

    fn list_locked(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(&self.root) else { return Vec::new() };
        let mut meetings: Vec<Value> = entries
            .flatten()
            .filter_map(|entry| self.read(&entry.file_name().to_string_lossy()))
            .collect();
        // createdAt is ISO text, so sorting it as text sorts by time. Newest first.
        meetings.sort_by(|a, b| b["createdAt"].as_str().unwrap_or("").cmp(a["createdAt"].as_str().unwrap_or("")));
        meetings.iter().map(summary).collect()
    }
}

/// The seven fields the meeting list shows. A field the meeting lacks is left out (like JSON.stringify drops `undefined`).
fn summary(meeting: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "title", "source", "createdAt", "durationSec", "status"] {
        if let Some(value) = meeting.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    let has_notes = meeting.get("notes").is_some_and(|n| !n.is_null());
    out.insert("hasNotes".into(), Value::Bool(has_notes));
    Value::Object(out)
}

/// Windows says "something has this file open" in three ways: PermissionDenied (error 5, also what a file that is
/// waiting to be deleted gives), a sharing violation (32) or a lock violation (33). Only the first exists on Linux.
fn is_busy(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// On Windows a rename over a file that something (an antivirus, a reader) has open fails for a
/// moment. Try a few times before giving up.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt: u32 = 0;
    loop {
        match fs::rename(from, to) {
            Err(e) if is_busy(&e) && attempt < 5 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20 * u64::from(attempt)));
            }
            other => return other,
        }
    }
}

/// Now as `2026-11-16T09:30:00.123Z` (what JavaScript's `toISOString()` gives).
fn now_iso() -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    iso_from_millis(millis)
}

/// Milliseconds since 1970 -> ISO 8601 UTC. Written by hand to avoid a date crate for ten lines.
fn iso_from_millis(millis: u64) -> String {
    let ms = millis % 1000;
    let secs = millis / 1000;
    let days = (secs / 86_400) as i64;
    let in_day = secs % 86_400;

    // Days since 1970-01-01 -> calendar date (Howard Hinnant's "civil from days" algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        in_day / 3600,
        (in_day % 3600) / 60,
        in_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn new_store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("meetings"));
        (dir, store)
    }

    fn make(store: &Store, extra: Value) -> Value {
        let mut init = json!({ "source": "live", "status": "ready", "transcript": "" });
        for (k, v) in extra.as_object().unwrap() {
            init[k] = v.clone();
        }
        store.create_meeting(init).unwrap()
    }

    #[test]
    fn create_get_patch_list_delete() {
        let (dir, store) = new_store();
        let m = make(&store, json!({ "transcript": "hello", "title": "First" }));
        let id = m["id"].as_str().unwrap().to_string();
        assert!(Store::id_ok(&id));
        assert_eq!(store.get_meeting(&id).unwrap()["transcript"], "hello");

        let patched = store.update_meeting(&id, json!({ "title": "Renamed", "transcript": "hello world" })).unwrap();
        assert_eq!(patched["title"], "Renamed");
        assert_eq!(store.get_meeting(&id).unwrap()["transcript"], "hello world");
        assert_eq!(store.get_meeting(&id).unwrap()["id"], id.as_str());

        let list = store.list_meetings();
        let item = list.iter().find(|x| x["id"] == id.as_str()).unwrap();
        assert_eq!(item["hasNotes"], false);

        assert!(store.delete_meeting(&id));
        assert!(store.get_meeting(&id).is_none());
        assert!(!dir.path().join("meetings").join(&id).exists());
        assert!(!store.delete_meeting(&id));
        assert!(store.update_meeting(&id, json!({ "title": "x" })).is_none());
    }

    #[test]
    fn create_fills_in_defaults() {
        let (_dir, store) = new_store();
        let m = store.create_meeting(json!({ "source": "upload", "status": "transcribing", "transcript": "" })).unwrap();
        assert_eq!(m["title"], "Untitled meeting");
        assert!(m["durationSec"].is_null());
        let created = m["createdAt"].as_str().unwrap();
        assert_eq!(created.len(), 24); // 2026-11-16T09:30:00.123Z
        assert!(created.ends_with('Z') && created.as_bytes()[10] == b'T');
        // an explicit value from the caller wins over the default
        let m2 = make(&store, json!({ "title": "Mine", "durationSec": 12.5 }));
        assert_eq!(m2["title"], "Mine");
        assert_eq!(m2["durationSec"], 12.5);
        assert!(store.create_meeting(json!("not an object")).is_err());
    }

    #[test]
    fn iso_dates_are_right() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_millis(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso_from_millis(1_709_164_800_000), "2024-02-29T00:00:00.000Z"); // leap day
    }

    #[test]
    fn file_is_two_space_pretty_json_with_keys_in_order() {
        let (dir, store) = new_store();
        let m = make(&store, json!({ "title": "T", "transcript": "hi" }));
        let id = m["id"].as_str().unwrap();
        let text = fs::read_to_string(dir.path().join("meetings").join(id).join("meeting.json")).unwrap();
        assert!(text.starts_with("{\n  \"id\": \""), "got: {text}");
        // keys stay in insertion order, like JSON.stringify
        let pos = |k: &str| text.find(&format!("\"{k}\"")).unwrap();
        assert!(pos("id") < pos("title") && pos("title") < pos("createdAt") && pos("createdAt") < pos("durationSec"));
        assert!(pos("durationSec") < pos("source") && pos("source") < pos("status") && pos("status") < pos("transcript"));
        assert!(!text.ends_with('\n')); // JSON.stringify adds no trailing newline
        assert!(!dir.path().join("meetings").join(id).join("meeting.json.tmp").exists());
    }

    #[test]
    fn ids_that_could_escape_the_data_folder_are_rejected() {
        let (_dir, store) = new_store();
        let long = "x".repeat(37);
        for bad in ["../x", "..", "a/b", "ab", "UPPER12", long.as_str(), "", "a b c d", "abc\n", "ünïcode"] {
            assert!(!Store::id_ok(bad), "{bad:?}");
            assert!(store.get_meeting(bad).is_none());
            assert!(!store.delete_meeting(bad));
            assert!(store.update_meeting(bad, json!({ "title": "x" })).is_none());
        }
        for good in ["abcd", "a1b2c3d4", "my-meeting-1", &"x".repeat(36)] {
            assert!(Store::id_ok(good), "{good:?}");
        }
    }

    #[test]
    fn newest_first_in_the_list() {
        let (_dir, store) = new_store();
        let a = make(&store, json!({ "createdAt": "2026-01-01T00:00:00.000Z" }));
        let b = make(&store, json!({ "createdAt": "2026-02-01T00:00:00.000Z" }));
        let c = make(&store, json!({ "createdAt": "2025-12-01T00:00:00.000Z" }));
        let ids: Vec<Value> = store.list_meetings().iter().map(|x| x["id"].clone()).collect();
        assert_eq!(ids, vec![b["id"].clone(), a["id"].clone(), c["id"].clone()]);
    }

    #[test]
    fn list_has_only_the_seven_summary_fields() {
        let (_dir, store) = new_store();
        let m = make(&store, json!({ "title": "T", "durationSec": 9, "transcript": "long text", "usage": { "x": 1 } }));
        store.update_meeting(m["id"].as_str().unwrap(), json!({ "notes": { "title": "n" } }));
        let list = store.list_meetings();
        assert_eq!(list.len(), 1);
        let mut keys: Vec<&str> = list[0].as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["createdAt", "durationSec", "hasNotes", "id", "source", "status", "title"]);
        assert_eq!(list[0]["hasNotes"], true);
        assert_eq!(list[0]["durationSec"], 9);
    }

    #[test]
    fn list_skips_folders_that_are_not_meetings() {
        let (dir, store) = new_store();
        make(&store, json!({}));
        let root = dir.path().join("meetings");
        fs::create_dir_all(root.join("empty-folder")).unwrap(); // no meeting.json
        fs::create_dir_all(root.join("broken-json")).unwrap();
        fs::write(root.join("broken-json").join("meeting.json"), "{ not json").unwrap();
        store.tmp_dir();
        assert_eq!(store.list_meetings().len(), 1);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let (_dir, store) = new_store();
        let m = make(&store, json!({ "usage": { "gemini": { "calls": 3 } }, "futureField": [1, "two", { "three": 3.5 }] }));
        let id = m["id"].as_str().unwrap();
        store.update_meeting(id, json!({ "title": "changed" }));
        let back = store.get_meeting(id).unwrap();
        assert_eq!(back["usage"], json!({ "gemini": { "calls": 3 } }));
        assert_eq!(back["futureField"], json!([1, "two", { "three": 3.5 }]));
        assert_eq!(back["title"], "changed");
    }

    #[test]
    fn null_in_a_patch_removes_the_key_and_id_is_never_changed() {
        let (_dir, store) = new_store();
        let m = make(&store, json!({ "error": "boom", "progress": "Part 1 of 2" }));
        let id = m["id"].as_str().unwrap();
        let next = store.update_meeting(id, json!({ "error": null, "id": "hacked-id", "progress": "Part 2 of 2" })).unwrap();
        assert!(next.get("error").is_none());
        assert_eq!(next["progress"], "Part 2 of 2");
        assert_eq!(next["id"], id);
        assert!(store.get_meeting(id).unwrap().get("error").is_none());
        // removing a key that is not there is fine
        assert!(store.update_meeting(id, json!({ "never-existed": null })).is_some());
    }

    #[test]
    fn concurrent_updates_from_several_threads_lose_nothing() {
        let (_dir, store) = new_store();
        let id = make(&store, json!({})).get("id").unwrap().as_str().unwrap().to_string();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let store = store.clone();
                let id = id.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        store.update_meeting(&id, json!({ format!("t{t}_{i}"): i })).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let m = store.get_meeting(&id).unwrap();
        for t in 0..8 {
            for i in 0..25 {
                assert_eq!(m[format!("t{t}_{i}")], i, "update t{t}_{i} was lost");
            }
        }
    }

    #[test]
    fn audio_is_saved_next_to_the_meeting_and_a_different_format_replaces_the_old_file() {
        let (dir, store) = new_store();
        let m = make(&store, json!({}));
        let id = m["id"].as_str().unwrap();
        let src = dir.path().join("upload-1");
        fs::write(&src, [1u8, 2, 3, 4]).unwrap();

        let saved = store.save_audio_file(id, &src, "webm").unwrap().unwrap();
        assert_eq!(saved["audio"], json!({ "file": "audio.webm", "mime": "audio/webm" }));
        assert!(!src.exists(), "the source file is moved, not copied");
        let path = store.audio_path(&store.get_meeting(id).unwrap()).unwrap();
        assert!(path.ends_with("audio.webm"));
        assert_eq!(fs::read(&path).unwrap(), [1u8, 2, 3, 4]);

        let src2 = dir.path().join("upload-2");
        fs::write(&src2, [9u8]).unwrap();
        store.save_audio_file(id, &src2, "wav").unwrap().unwrap();
        assert!(!path.exists(), "the old audio.webm is gone");
        assert_eq!(store.get_meeting(id).unwrap()["audio"]["file"], "audio.wav");
    }

    #[test]
    fn save_audio_refuses_unknown_meetings_and_extensions() {
        let (dir, store) = new_store();
        let m = make(&store, json!({}));
        let src = dir.path().join("u");
        fs::write(&src, "x").unwrap();
        assert!(store.save_audio_file(m["id"].as_str().unwrap(), &src, "exe").unwrap().is_none());
        assert!(store.save_audio_file("nope-nope", &src, "wav").unwrap().is_none());
        assert!(src.exists(), "a refused file is left alone");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_the_audio_file_is_never_returned() {
        let (dir, store) = new_store();
        let m = make(&store, json!({}));
        let id = m["id"].as_str().unwrap();
        let mdir = dir.path().join("meetings").join(id);
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, "private").unwrap();
        std::os::unix::fs::symlink(&secret, mdir.join("audio.webm")).unwrap();
        let with_audio = json!({ "id": id, "audio": { "file": "audio.webm", "mime": "audio/webm" } });
        assert!(store.audio_path(&with_audio).is_none());
        fs::remove_file(mdir.join("audio.webm")).unwrap();
        fs::write(mdir.join("audio.webm"), "x").unwrap();
        assert_eq!(store.audio_path(&with_audio), Some(mdir.join("audio.webm")));
        // a symlinked meeting folder is refused too
        let other = dir.path().join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("audio.webm"), "x").unwrap();
        fs::remove_dir_all(&mdir).unwrap();
        std::os::unix::fs::symlink(&other, &mdir).unwrap();
        assert!(store.audio_path(&with_audio).is_none());
    }

    #[test]
    fn audio_path_only_accepts_plain_audio_names() {
        let (dir, store) = new_store();
        let m = make(&store, json!({}));
        let id = m["id"].as_str().unwrap();
        let mdir = dir.path().join("meetings").join(id);
        fs::write(mdir.join("meeting.json.bak"), "x").unwrap();
        fs::write(mdir.join("audio."), "x").unwrap();
        fs::create_dir_all(mdir.join("audio.dir")).unwrap();
        for file in ["../meeting.json", "meeting.json", "audio.", "audio.WAV", "audio.a/b", "audio.dir", "audio.missing", "/etc/passwd"] {
            let meeting = json!({ "id": id, "audio": { "file": file, "mime": "audio/wav" } });
            assert!(store.audio_path(&meeting).is_none(), "{file}");
        }
        assert!(store.audio_path(&json!({ "id": id })).is_none()); // no audio field
        assert!(store.audio_path(&json!({ "id": "../x", "audio": { "file": "audio.wav" } })).is_none());
    }

    #[test]
    fn audio_ext_maps_content_types_and_refuses_non_audio() {
        assert_eq!(Store::audio_ext("audio/webm;codecs=opus", ""), Some("webm"));
        assert_eq!(Store::audio_ext("audio/mpeg", ""), Some("mp3"));
        assert_eq!(Store::audio_ext("audio/x-m4a", ""), Some("m4a"));
        assert_eq!(Store::audio_ext("application/octet-stream", "talk.MP3"), Some("mp3"));
        assert_eq!(Store::audio_ext("", "notes.txt"), None);
        assert_eq!(Store::audio_ext("text/plain", "a.mp3"), None);
        assert_eq!(Store::audio_ext("application/pdf", ""), None);
    }

    #[test]
    fn audio_ext_accepts_aac_and_other_formats_however_the_browser_labels_them() {
        for kind in ["audio/x-hx-aac-adts", "audio/aacp", "audio/x-aac", "audio/aac", "audio/vnd.dlna.adts"] {
            assert_eq!(Store::audio_ext(kind, "call.aac"), Some("aac"), "{kind}");
        }
        assert_eq!(Store::audio_ext("", "call.AAC"), Some("aac"));
        assert_eq!(Store::audio_ext("audio/x-something", "voice.opus"), Some("ogg"));
        assert_eq!(Store::audio_ext("audio/x-something", "voice.oga"), Some("ogg"));
        assert_eq!(Store::audio_ext("audio/x-something", "voice.aif"), Some("aiff"));
        assert_eq!(Store::audio_ext("audio/x-something", "voice.mpga"), Some("mp3"));
        assert_eq!(Store::audio_ext("audio/x-something", "voice.xyz"), None);
    }

    #[test]
    fn every_mime_in_the_table_maps_to_an_extension_with_a_mime() {
        for (mime, ext) in MIME_TO_EXT {
            assert_eq!(Store::audio_ext(mime, ""), Some(ext));
            assert!(Store::mime_for_ext(ext).is_some(), "{ext}");
        }
        assert_eq!(Store::mime_for_ext("m4a"), Some("audio/mp4"));
        assert_eq!(Store::mime_for_ext("mp3"), Some("audio/mpeg"));
        assert_eq!(Store::mime_for_ext("exe"), None);
    }

    #[test]
    fn unfinished_jobs_are_marked_as_errors_on_start() {
        let (_dir, store) = new_store();
        let a = make(&store, json!({ "source": "upload", "status": "transcribing" }));
        let b = make(&store, json!({ "status": "writing-notes", "transcript": "x" }));
        let c = make(&store, json!({ "transcript": "x" }));
        assert_eq!(store.fail_interrupted(), 2);
        let a = store.get_meeting(a["id"].as_str().unwrap()).unwrap();
        assert_eq!(a["status"], "error");
        assert_eq!(a["error"], "Stopped before it finished. Try again.");
        assert_eq!(store.get_meeting(b["id"].as_str().unwrap()).unwrap()["status"], "error");
        assert_eq!(store.get_meeting(c["id"].as_str().unwrap()).unwrap()["status"], "ready");
        assert_eq!(store.fail_interrupted(), 0);
    }

    #[test]
    fn tmp_dir_is_inside_the_data_folder_and_not_listed() {
        let (dir, store) = new_store();
        let tmp = store.tmp_dir();
        assert!(tmp.is_dir());
        assert!(tmp.starts_with(dir.path().join("meetings")));
        assert!(store.list_meetings().is_empty());
        let ids: HashSet<String> = (0..20).map(|_| make(&store, json!({}))["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids.len(), 20);
    }
}

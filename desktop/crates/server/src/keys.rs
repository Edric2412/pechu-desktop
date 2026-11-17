//! Where the API keys live. Looked up in this order:
//!   1. the environment (GEMINI_API_KEY / SARVAM_API_KEY): a developer override, never written anywhere;
//!   2. the operating system's keychain (service "pechu");
//!   3. a file `keys.json` in the config folder, only when there is no keychain (for example Linux without Secret Service).
//!
//! Keys are read into memory once, because asking the keychain can be slow and `Keys::gemini_key` is called often.
use pechu_jobs::Keys;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

const SERVICE: &str = "pechu";
const GEMINI: &str = "gemini_api_key";
const SARVAM: &str = "sarvam_api_key";

/// Removes the whitespace and the quotes people often paste around a key. (Port of `normalizeKey`.)
pub fn normalize_key(raw: &str) -> String {
    raw.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string()
}

/// A key is accepted when it is 1 to 300 printable ASCII characters with no spaces. (Same rule as the web version.)
pub fn key_shape_ok(key: &str) -> bool {
    (1..=300).contains(&key.len()) && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

#[derive(Default)]
struct Saved {
    gemini: Option<String>,
    sarvam: Option<String>,
}

pub struct KeyStore {
    /// true = tests: keys live only in `saved`, and the environment is ignored.
    memory_only: bool,
    config_dir: PathBuf,
    saved: Mutex<Saved>,
}

impl KeyStore {
    /// Reads the saved keys. This may talk to the keychain, so call it from a blocking context (`spawn_blocking`).
    pub fn open(config_dir: PathBuf, memory_only: bool) -> Self {
        let store = KeyStore { memory_only, config_dir, saved: Mutex::new(Saved::default()) };
        if !memory_only {
            let loaded = Saved { gemini: store.load(GEMINI), sarvam: store.load(SARVAM) };
            *store.saved.lock().unwrap() = loaded;
        }
        store
    }

    /// Keeps a Gemini key (in the keychain, or in the file when there is no keychain) and uses it from now on.
    /// Blocking: call from `spawn_blocking`.
    pub fn save_gemini(&self, key: &str) -> anyhow::Result<()> {
        self.persist(GEMINI, key)?;
        self.saved.lock().unwrap().gemini = Some(key.to_string());
        Ok(())
    }

    pub fn save_sarvam(&self, key: &str) -> anyhow::Result<()> {
        self.persist(SARVAM, key)?;
        self.saved.lock().unwrap().sarvam = Some(key.to_string());
        Ok(())
    }

    fn env(&self, name: &str) -> Option<String> {
        if self.memory_only {
            return None;
        }
        std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    fn keys_file(&self) -> PathBuf {
        self.config_dir.join("keys.json")
    }

    fn load(&self, name: &str) -> Option<String> {
        if let Ok(entry) = keyring::Entry::new(SERVICE, name) {
            if let Ok(value) = entry.get_password() {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        let text = std::fs::read_to_string(self.keys_file()).ok()?;
        let file: Value = serde_json::from_str(&text).ok()?;
        file.get(name)?.as_str().filter(|v| !v.is_empty()).map(str::to_string)
    }

    fn persist(&self, name: &str, key: &str) -> anyhow::Result<()> {
        if self.memory_only {
            return Ok(());
        }
        if self.save_in_keychain(name, key) {
            return Ok(());
        }
        // No usable keychain: fall back to a private file.
        self.save_in_file(name, key)
    }

    /// true only when the key went in AND reads back: some keychain set-ups accept a write and then forget it.
    fn save_in_keychain(&self, name: &str, key: &str) -> bool {
        let Ok(entry) = keyring::Entry::new(SERVICE, name) else { return false };
        entry.set_password(key).is_ok() && entry.get_password().ok().as_deref() == Some(key)
    }

    fn save_in_file(&self, name: &str, key: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let path = self.keys_file();
        // keep the other key that is already in the file
        let mut file: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        file[name] = json!(key);
        // write next to the target and rename, so a crash cannot leave a half-written key file
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, serde_json::to_string_pretty(&file)?.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Writes a file only the owner can read (0600 on Unix). On Windows there is no mode: the file keeps the access list of
/// its folder (%APPDATA%\Pechu), which by default is the current user plus administrators.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

impl Keys for KeyStore {
    fn gemini_key(&self) -> Option<String> {
        self.env("GEMINI_API_KEY").or_else(|| self.saved.lock().unwrap().gemini.clone())
    }

    fn sarvam_key(&self) -> Option<String> {
        self.env("SARVAM_API_KEY").or_else(|| self.saved.lock().unwrap().sarvam.clone())
    }

    fn notes_model(&self) -> Option<String> {
        self.env("NOTES_MODEL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_spaces_are_removed() {
        assert_eq!(normalize_key("  \"abc123\"  "), "abc123");
        assert_eq!(normalize_key("'abc'"), "abc");
        assert_eq!(normalize_key(" ' abc ' "), "abc");
    }

    #[test]
    fn key_shape() {
        assert!(key_shape_ok("AIzaSy-abc_123"));
        assert!(!key_shape_ok(""));
        assert!(!key_shape_ok("has space"));
        assert!(!key_shape_ok("tamil-தமிழ்"));
        assert!(!key_shape_ok(&"a".repeat(301)));
    }

    #[test]
    fn the_key_file_is_private_and_keeps_both_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyStore { memory_only: false, config_dir: dir.path().to_path_buf(), saved: Mutex::new(Saved::default()) };
        store.save_in_file(GEMINI, "g-key").unwrap();
        store.save_in_file(SARVAM, "s-key").unwrap();
        assert_eq!(store.load_from_file_only(GEMINI).as_deref(), Some("g-key"));
        assert_eq!(store.load_from_file_only(SARVAM).as_deref(), Some("s-key"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("keys.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn memory_keys_are_used_and_the_environment_is_ignored() {
        let store = KeyStore::open(PathBuf::from("/nonexistent"), true);
        assert_eq!(store.gemini_key(), None);
        store.save_gemini("abc").unwrap();
        assert_eq!(store.gemini_key().as_deref(), Some("abc"));
        assert_eq!(store.sarvam_key(), None);
    }

    impl KeyStore {
        /// Test helper: read only the file, never the machine's real keychain.
        fn load_from_file_only(&self, name: &str) -> Option<String> {
            let text = std::fs::read_to_string(self.keys_file()).ok()?;
            let file: Value = serde_json::from_str(&text).ok()?;
            file.get(name)?.as_str().map(str::to_string)
        }
    }
}

//! Choosing which Gemini model writes the notes. Port of `pickNotesModel` in server/gemini.ts (see tests/pick-model.test.ts).
use regex::Regex;
use serde::Deserialize;
use std::cmp::Ordering;
use std::sync::LazyLock;

/// One entry of Google's models list (only the two fields we need).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub name: String,
    #[serde(default)]
    pub supported_generation_methods: Vec<String>,
}

static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)gemini-(\d+(?:\.\d+)?)").unwrap());

// "gemini-3.8-flash" -> 3.8. Compared as numbers, so 10 beats 3.8.
fn version(name: &str) -> f64 {
    VERSION.captures(name).and_then(|m| m[1].parse().ok()).unwrap_or(0.0)
}

fn mentions(name: &str, words: &[&str]) -> bool {
    let lower = name.to_lowercase();
    words.iter().any(|w| lower.contains(w))
}

// Newest version first; on a tie, a non-preview model wins.
fn newest_first(a: &str, b: &str) -> Ordering {
    version(b)
        .partial_cmp(&version(a))
        .unwrap_or(Ordering::Equal)
        .then(mentions(a, &["preview"]).cmp(&mentions(b, &["preview"])))
}

/// Returns the bare model id (no "models/" prefix) or None.
pub fn pick_notes_model(models: &[ModelInfo]) -> Option<String> {
    let usable: Vec<String> = models
        .iter()
        .filter(|m| m.supported_generation_methods.iter().any(|x| x == "generateContent"))
        .map(|m| m.name.strip_prefix("models/").unwrap_or(&m.name).to_string())
        .collect();
    let best: Vec<&String> = usable
        .iter()
        .filter(|n| {
            mentions(n, &["flash"])
                && !mentions(n, &["lite", "live", "tts", "image", "audio", "transcribe", "embed", "thinking", "exp"])
        })
        .collect();
    let fallback: Vec<&String> = usable
        .iter()
        .filter(|n| mentions(n, &["pro", "flash"]) && !mentions(n, &["live", "tts", "image", "audio", "transcribe", "embed"]))
        .collect();
    let mut pool = if best.is_empty() { fallback } else { best };
    // In the fallback pool a lite model is the last resort. (`sort_by` is stable, like JS sort.)
    pool.sort_by(|a, b| mentions(a, &["lite"]).cmp(&mentions(b, &["lite"])).then_with(|| newest_first(a, b)));
    pool.first().map(|n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str) -> ModelInfo {
        mm(name, &["generateContent"])
    }
    fn mm(name: &str, methods: &[&str]) -> ModelInfo {
        ModelInfo { name: format!("models/{name}"), supported_generation_methods: methods.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn picks_the_highest_version_flash_and_prefers_non_preview_on_a_tie() {
        let models = [m("gemini-2.5-flash"), m("gemini-3-flash"), m("gemini-3.8-flash-preview"), m("gemini-3.8-flash"), m("gemini-3.5-flash")];
        assert_eq!(pick_notes_model(&models).as_deref(), Some("gemini-3.8-flash"));
    }

    #[test]
    fn a_preview_wins_only_when_nothing_else_has_that_version() {
        assert_eq!(pick_notes_model(&[m("gemini-3.9-flash-preview"), m("gemini-3.8-flash")]).as_deref(), Some("gemini-3.9-flash-preview"));
    }

    #[test]
    fn version_numbers_compare_numerically_not_as_text() {
        assert_eq!(pick_notes_model(&[m("gemini-2.5-flash"), m("gemini-10-flash"), m("gemini-3.8-flash")]).as_deref(), Some("gemini-10-flash"));
    }

    #[test]
    fn excludes_special_purpose_models_and_ones_that_cannot_generate() {
        let mut models: Vec<ModelInfo> = ["lite", "live", "tts", "image", "audio", "transcribe", "embed", "thinking", "exp"]
            .iter()
            .map(|w| m(&format!("gemini-9-flash-{w}")))
            .collect();
        models.push(mm("gemini-9-flash", &["embedContent"]));
        models.push(m("gemini-2.5-flash"));
        assert_eq!(pick_notes_model(&models).as_deref(), Some("gemini-2.5-flash"));
    }

    #[test]
    fn falls_back_to_pro_when_no_plain_flash_model_is_usable() {
        let models = [m("gemini-2.5-pro"), m("gemini-3.1-pro"), m("gemini-9-flash-lite"), m("gemini-9-flash-live")];
        assert_eq!(pick_notes_model(&models).as_deref(), Some("gemini-3.1-pro"));
    }

    #[test]
    fn returns_none_when_nothing_fits() {
        assert_eq!(pick_notes_model(&[]), None);
        assert_eq!(pick_notes_model(&[mm("text-embedding-004", &["embedContent"]), m("gemma-3-27b-it")]), None);
    }
}

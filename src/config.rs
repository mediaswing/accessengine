//! Non-secret settings, persisted as JSON next to the app's support files.
//!
//! The ElevenLabs API key deliberately lives elsewhere; see [`crate::keychain`].

use crate::audio::AudioFormat;
use crate::dictionary::Replacement;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which synthesiser the user picked. There is no "automatic" setting: the app
/// asks the question in one dropdown and the answer stays answered, which is far
/// easier to operate — and to hear read out — than a mode that silently changes
/// under you depending on whether a key happens to be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnginePreference {
    #[default]
    System,
    ElevenLabs,
}

impl EnginePreference {
    pub const ALL: [Self; 2] = [Self::System, Self::ElevenLabs];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System voices",
            Self::ElevenLabs => "ElevenLabs",
        }
    }

    /// Spoken to the user in tooltips and read out by a screen reader, so it
    /// says what the choice actually means rather than repeating the name.
    pub fn description(self) -> &'static str {
        match self {
            Self::System => "The voices built into this computer. No account or internet needed.",
            Self::ElevenLabs => "Higher-quality online voices. Needs an ElevenLabs API key.",
        }
    }
}

/// Accepts the `"auto"` written by earlier versions. Without this the whole
/// config fails to parse and every other saved setting is lost with it.
fn deserialize_engine<'de, D>(deserializer: D) -> Result<EnginePreference, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        "eleven_labs" | "elevenlabs" => EnginePreference::ElevenLabs,
        _ => EnginePreference::System,
    })
}

/// The vision models offered in Settings, with the download each one commits
/// the user to.
///
/// Ollama runs vision models on its own engine now and has dropped the old
/// `mllama` runner, so a model that worked a year ago may simply refuse to
/// load — see [`is_retired`]. Everything listed here loads on current Ollama;
/// the descriptions say what each is actually good at, since "which vision
/// model" is not a question most people should have to research.
pub const VISION_MODELS: &[(&str, &str)] = &[
    (
        "qwen2.5vl:3b",
        "Qwen2.5-VL 3B — about 3 GB. Reads text well and is the quickest to download.",
    ),
    (
        "qwen2.5vl:7b",
        "Qwen2.5-VL 7B — about 5.7 GB. Better on dense or handwritten pages, slower to answer.",
    ),
    (
        "granite3.2-vision:2b",
        "Granite Vision 2B — about 2.3 GB. The smallest; tuned for documents and tables.",
    ),
    (
        "gemma3:4b",
        "Gemma 3 4B — about 3.2 GB. Describes photographs and scenes well.",
    ),
    (
        "minicpm-v:8b",
        "MiniCPM-V 8B — about 5.2 GB. A strong all-rounder on both text and scenes.",
    ),
];

pub const DEFAULT_VISION_MODEL: &str = "qwen2.5vl:3b";

/// True for a model this app used to default to that current Ollama can no
/// longer load.
///
/// `llama3.2-vision` needs the `mllama` architecture, which Ollama removed
/// along with the llama.cpp runner that implemented it; loading one now fails
/// with `unknown model architecture: 'mllama'` and no amount of retrying helps.
/// Anyone upgrading from an older release has that name saved in their config,
/// so it is swapped for a working default on load rather than left to fail at
/// the first image.
pub fn is_retired(model: &str) -> bool {
    let name = model
        .trim()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == "llama3.2-vision"
}

/// Replaces a retired model with the current default, so an upgrade fixes
/// itself instead of failing at the first image.
fn deserialize_vision_model<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(if raw.trim().is_empty() || is_retired(&raw) {
        DEFAULT_VISION_MODEL.to_string()
    } else {
        raw
    })
}

/// What the Apply button does. The two things the app can do with a document
/// are one choice, not two buttons that look alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    ReadAloud,
    SaveAudio,
}

impl Action {
    pub const ALL: [Self; 2] = [Self::ReadAloud, Self::SaveAudio];

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadAloud => "Read Text Aloud",
            Self::SaveAudio => "Save Audio File",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ReadAloud => "Speak the document through this computer's audio output.",
            Self::SaveAudio => "Write the spoken document to an audio file on disk.",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(deserialize_with = "deserialize_engine")]
    pub engine: EnginePreference,

    /// What the Apply button does.
    pub action: Action,
    /// Audio format used when the action is [`Action::SaveAudio`].
    pub save_format: AudioFormat,

    /// ElevenLabs voice id (not the display name) and model id.
    pub elevenlabs_voice_id: String,
    pub elevenlabs_voice_name: String,
    pub elevenlabs_model_id: String,

    /// System voice identifier, empty meaning "whatever this computer is set
    /// to". On macOS that is a `say` voice name; on Windows, a SAPI voice name.
    pub system_voice: String,
    /// Words per minute. 175 is the macOS default and roughly the Windows one.
    pub system_rate: u32,

    /// Words to swap out before the document is spoken. See [`crate::dictionary`].
    pub dictionary: Vec<Replacement>,

    /// Ollama vision model used to turn an image into readable text.
    #[serde(deserialize_with = "deserialize_vision_model")]
    pub ollama_model: String,
    pub ollama_prompt: String,

    /// Directory the last save went to, so the dialog reopens somewhere useful.
    pub last_save_dir: Option<PathBuf>,
    /// Directory the Audio Player last opened a file from.
    pub last_audio_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EnginePreference::System,
            action: Action::ReadAloud,
            save_format: AudioFormat::Wav,
            elevenlabs_voice_id: String::new(),
            elevenlabs_voice_name: String::new(),
            elevenlabs_model_id: "eleven_multilingual_v2".to_string(),
            system_voice: String::new(),
            system_rate: 175,
            dictionary: Vec::new(),
            ollama_model: DEFAULT_VISION_MODEL.to_string(),
            ollama_prompt: DEFAULT_VISION_PROMPT.to_string(),
            last_save_dir: None,
            last_audio_dir: None,
        }
    }
}

/// Used when the configured prompt comes back empty. Small vision models often
/// go quiet on a long conditional instruction but answer a plain question, so
/// this is deliberately as simple as it can be.
pub const FALLBACK_VISION_PROMPT: &str = "Describe this image, including any text it contains.";

pub const DEFAULT_VISION_PROMPT: &str = "\
Transcribe every word of text visible in this image, exactly as written, preserving \
the reading order and line breaks. If the image contains no text, instead describe \
what it shows in two or three plain sentences. Reply with the transcription or \
description only, with no preamble, headings, or commentary.";

impl Config {
    pub fn path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("io", "accessengine", "accessengine")?;
        Some(dirs.config_dir().join("config.json"))
    }

    /// Loads the saved config, falling back to defaults if it is missing or
    /// unreadable. A corrupt config should never stop the app from starting.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("could not locate a config directory")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_vision_model_is_one_of_the_offered_ones() {
        assert!(
            VISION_MODELS
                .iter()
                .any(|(id, _)| *id == DEFAULT_VISION_MODEL)
        );
        // A default that is itself retired would migrate in a loop.
        assert!(!is_retired(DEFAULT_VISION_MODEL));
        for (id, _) in VISION_MODELS {
            assert!(!is_retired(id), "{id} can no longer be loaded by Ollama");
        }
    }

    #[test]
    fn retired_models_are_recognised_with_or_without_a_tag() {
        assert!(is_retired("llama3.2-vision"));
        assert!(is_retired("llama3.2-vision:latest"));
        assert!(is_retired("llama3.2-vision:90b"));
        assert!(is_retired("  Llama3.2-Vision  "));
        assert!(!is_retired("qwen2.5vl:3b"));
        assert!(!is_retired("llava:13b"));
    }

    #[test]
    fn loading_an_old_config_swaps_the_retired_model_and_keeps_everything_else() {
        let saved = r#"{
            "engine": "elevenlabs",
            "system_rate": 210,
            "ollama_model": "llama3.2-vision",
            "elevenlabs_voice_name": "Rachel"
        }"#;
        let config: Config = serde_json::from_str(saved).expect("an old config should still load");

        assert_eq!(config.ollama_model, DEFAULT_VISION_MODEL);
        assert_eq!(config.engine, EnginePreference::ElevenLabs);
        assert_eq!(config.system_rate, 210);
        assert_eq!(config.elevenlabs_voice_name, "Rachel");
    }

    #[test]
    fn a_deliberately_chosen_model_is_left_alone() {
        let saved = r#"{ "ollama_model": "minicpm-v:8b" }"#;
        let config: Config = serde_json::from_str(saved).unwrap();
        assert_eq!(config.ollama_model, "minicpm-v:8b");
    }
}

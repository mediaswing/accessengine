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
    pub ollama_model: String,
    pub ollama_prompt: String,

    /// Directory the last save went to, so the dialog reopens somewhere useful.
    pub last_save_dir: Option<PathBuf>,
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
            ollama_model: "llama3.2-vision".to_string(),
            ollama_prompt: DEFAULT_VISION_PROMPT.to_string(),
            last_save_dir: None,
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

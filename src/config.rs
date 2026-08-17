//! Non-secret settings, persisted as JSON next to the app's support files.
//!
//! The ElevenLabs API key lives in a file of its own alongside this one, so
//! that a secret is not rewritten every time a setting changes; see
//! [`crate::apikey`].

use crate::audio::AudioFormat;
use crate::dictionary::Replacement;
use crate::t;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The saved value meaning "whatever language this computer is set to".
pub const AUTO_LANGUAGE: &str = "auto";

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

    pub fn label(self) -> String {
        match self {
            Self::System => t!("engine.system.label"),
            Self::ElevenLabs => t!("engine.elevenlabs.label"),
        }
    }

    /// Spoken to the user in tooltips and read out by a screen reader, so it
    /// says what the choice actually means rather than repeating the name.
    pub fn description(self) -> String {
        match self {
            Self::System => t!("engine.system.description"),
            Self::ElevenLabs => t!("engine.elevenlabs.description"),
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

/// The vision models offered in Settings, as `(model id, description key)`.
///
/// Ollama runs vision models on its own engine now and has dropped the old
/// `mllama` runner, so a model that worked a year ago may simply refuse to
/// load — see [`is_retired`]. Everything listed here loads on current Ollama;
/// the descriptions say what each is actually good at, since "which vision
/// model" is not a question most people should have to research. They are keys
/// rather than sentences because the size and the trade-off have to be readable
/// in whatever language the app is running in — see [`vision_model_description`].
pub const VISION_MODELS: &[(&str, &str)] = &[
    ("qwen2.5vl:3b", "model.qwen3b"),
    ("qwen2.5vl:7b", "model.qwen7b"),
    ("granite3.2-vision:2b", "model.granite"),
    ("gemma3:4b", "model.gemma"),
    ("minicpm-v:8b", "model.minicpm"),
];

/// What a model in [`VISION_MODELS`] is good at, in the language in use.
pub fn vision_model_description(key: &str) -> String {
    crate::i18n::text(key, &[])
}

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

/// Whether the formatting in a Word document is read out along with its words.
///
/// Off by default, and deliberately: most documents are formatted for the eye
/// and announcing all of it turns a letter into a stream of interruptions. It
/// earns its place on the documents where the formatting *is* the meaning — a
/// contract with one clause in red, a form whose deadline is the only bold
/// thing on the page — which is exactly the case someone who cannot see the
/// page has no other way to learn about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Formatting {
    #[default]
    Ignore,
    Announce,
}

impl Formatting {
    pub const ALL: [Self; 2] = [Self::Ignore, Self::Announce];

    pub fn label(self) -> String {
        match self {
            Self::Ignore => t!("formatting.ignore.label"),
            Self::Announce => t!("formatting.announce.label"),
        }
    }

    pub fn description(self) -> String {
        match self {
            Self::Ignore => t!("formatting.ignore.description"),
            Self::Announce => t!("formatting.announce.description"),
        }
    }
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

    pub fn label(self) -> String {
        match self {
            Self::ReadAloud => t!("action.read.label"),
            Self::SaveAudio => t!("action.save.label"),
        }
    }

    pub fn description(self) -> String {
        match self {
            Self::ReadAloud => t!("action.read.description"),
            Self::SaveAudio => t!("action.save.description"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The language the interface is written in: a code like `fr`, or `auto`
    /// to follow the operating system. See [`crate::i18n`].
    ///
    /// Deliberately not reset by [`Config::reset_to_defaults`]. Everything else
    /// on that page is a preference; this one is the difference between an app
    /// somebody can read and an app they cannot, and putting it back to
    /// "whatever this computer says" would be a reset that hides its own
    /// confirmation dialog behind a language they did not choose.
    pub language: String,

    #[serde(deserialize_with = "deserialize_engine")]
    pub engine: EnginePreference,

    /// What the Apply button does.
    pub action: Action,
    /// Whether a Word document's formatting is read out with its words.
    pub formatting: Formatting,
    /// Whether finishing or failing plays a sound. On by default: the app is
    /// built for people who cannot see the status line change colour, and a
    /// cue is the fastest way to know an action landed. Off is here because a
    /// sound nobody can silence is its own accessibility problem — plenty of
    /// people run this alongside a screen reader that is already talking.
    pub sound_effects: bool,
    /// Whether a running job keeps making a quiet sound while it runs. Its own
    /// setting rather than part of `sound_effects`: it is the one cue that
    /// reports nothing new, so it is the one somebody might want gone while
    /// keeping the rest. Ignored entirely when `sound_effects` is off.
    pub progress_tick: bool,
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
    #[serde(deserialize_with = "deserialize_vision_prompt")]
    pub ollama_prompt: String,

    /// What each still taken from a video is asked about. Shorter than the
    /// image prompt on purpose: this answer is one of dozens that will be
    /// joined together, so a frame that opens with its own preamble makes a
    /// narration that says "this image shows" forty times.
    pub video_frame_prompt: String,
    /// Whether the frame descriptions are rewritten as continuous narration
    /// before they are spoken. Off leaves the labelled list, which is the
    /// honest raw material — nothing in it came from anywhere but a frame.
    pub video_narrate: bool,
    /// The instruction for that rewrite.
    pub video_narration_prompt: String,
    /// Text model used for the rewrite. Empty means "use the vision model",
    /// which is the default because it is already downloaded and vision models
    /// answer text-only prompts perfectly well. Naming a dedicated text model
    /// here buys better prose at the cost of another multi-gigabyte download.
    pub narration_model: String,
    /// How different a frame must be from the one before it to count as a new
    /// shot, 0.0 to 1.0. See [`crate::ffmpeg::Sampling`].
    pub video_scene_threshold: f32,
    /// Take a frame anyway if this many seconds have passed without one.
    pub video_interval_secs: u32,
    /// The most frames one video may be described from.
    pub video_max_frames: usize,

    /// Directory the last save went to, so the dialog reopens somewhere useful.
    pub last_save_dir: Option<PathBuf>,
    /// Directory the audio player last opened a file from.
    pub last_audio_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: AUTO_LANGUAGE.to_string(),
            engine: EnginePreference::System,
            action: Action::ReadAloud,
            formatting: Formatting::Ignore,
            sound_effects: true,
            progress_tick: true,
            save_format: AudioFormat::Wav,
            elevenlabs_voice_id: String::new(),
            elevenlabs_voice_name: String::new(),
            elevenlabs_model_id: "eleven_multilingual_v2".to_string(),
            system_voice: String::new(),
            system_rate: 175,
            dictionary: Vec::new(),
            ollama_model: DEFAULT_VISION_MODEL.to_string(),
            ollama_prompt: default_vision_prompt(),
            video_frame_prompt: default_frame_prompt(),
            video_narrate: true,
            video_narration_prompt: default_narration_prompt(),
            narration_model: String::new(),
            video_scene_threshold: DEFAULT_SCENE_THRESHOLD,
            video_interval_secs: DEFAULT_INTERVAL_SECS,
            video_max_frames: DEFAULT_MAX_FRAMES,
            last_save_dir: None,
            last_audio_dir: None,
        }
    }
}

/// Used when the configured prompt comes back empty. Small vision models often
/// go quiet on a long conditional instruction but answer a plain question, so
/// this is deliberately as simple as it can be.
pub fn fallback_vision_prompt() -> String {
    t!("prompt.fallback")
}

/// What the vision model is asked for.
///
/// Comes from the language file rather than from here, so that a French
/// interface asks the model in French and a French voice reads back a French
/// description. The three prompts are the one place where the interface
/// language decides what the app *says to something else* rather than to the
/// user, and getting them from the same file is what keeps the whole chain in
/// one language. A prompt the user has since edited is never replaced — see
/// [`crate::i18n::is_untouched_prompt`].
///
/// The description comes first and unconditionally, which is the whole point of
/// the wording. The prompt this replaced asked for a transcription and said to
/// describe the picture *only* if it contained no text — and a photograph of a
/// street corner contains text, because there is a road sign in it. A model
/// following that instruction faithfully answered a photo of a city square with
/// the words "ONE WAY" and nothing else, which is both correct and useless. Any
/// text at all, however incidental, suppressed the description entirely.
///
/// So: always describe, then transcribe whatever text is there, and shorten the
/// description to one sentence when the image really is a page — which keeps
/// document reading, the other thing this app is for, from growing a preamble.
///
/// The last sentence earns its place too. Asked for a transcription without it,
/// the model returns Markdown — `**Transcription:**` and a fenced block — and a
/// speech synthesiser reads the asterisks out.
pub fn default_vision_prompt() -> String {
    t!("prompt.vision")
}

/// The prompt shipped up to version 1.2.2, kept only to recognise it.
///
/// Anyone who has run the app before has this saved in their config, and a
/// default is only a default for someone opening the app for the first time.
/// Left alone, the people most affected by the bug above — everyone already
/// using the app — would be the only ones never to get the fix.
const SUPERSEDED_VISION_PROMPT: &str = "\
Transcribe every word of text visible in this image, exactly as written, preserving \
the reading order and line breaks. If the image contains no text, instead describe \
what it shows in two or three plain sentences. Reply with the transcription or \
description only, with no preamble, headings, or commentary.";

/// Replaces the superseded prompt with the current one, leaving a prompt the
/// user has written themselves exactly as they wrote it.
fn deserialize_vision_prompt<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(
        if raw.trim().is_empty() || raw.trim() == SUPERSEDED_VISION_PROMPT.trim() {
            default_vision_prompt()
        } else {
            raw
        },
    )
}

/// What each still taken from a video is asked.
///
/// "One or two sentences" is the load-bearing part. The image prompt asks for
/// two or three plus a full transcription of any text, which is right for a
/// photograph the user chose to open and wrong forty times over for frames that
/// will be joined together — a video of a conference talk would otherwise
/// return the same slide transcribed in full at every cut.
pub fn default_frame_prompt() -> String {
    t!("prompt.frame")
}

/// The instruction for turning those frames into narration.
///
/// The middle sentence is the one that matters. A model given a list of stills
/// will happily write the story that connects them — a person who appears in
/// frame one and frame nine is described as having "waited there all afternoon"
/// — and a description track that invents what happened between the frames is
/// worse than useless to someone who cannot check it against the picture.
pub fn default_narration_prompt() -> String {
    t!("prompt.narration")
}

/// ffmpeg's scene score for an ordinary cut. Lower catches camera movement and
/// costs vision calls for it; higher misses everything but hard cuts.
pub const DEFAULT_SCENE_THRESHOLD: f32 = 0.4;
/// Seconds without a frame before one is taken regardless.
pub const DEFAULT_INTERVAL_SECS: u32 = 30;
/// Frames per video by default.
pub const DEFAULT_MAX_FRAMES: usize = 40;
/// The most a user may raise [`Config::video_max_frames`] to.
///
/// A cap on the cap, because this number is minutes of the user's life: at a
/// minute a frame on a machine with no GPU, 200 is already most of an
/// afternoon, and a config file with a stray zero in it should not be able to
/// commit them to a week.
pub const MAX_FRAMES_LIMIT: usize = 200;

impl Config {
    /// Which frames to take out of a video, with the saved numbers held to
    /// ranges ffmpeg and the user's patience can both survive.
    pub fn video_sampling(&self) -> crate::ffmpeg::Sampling {
        crate::ffmpeg::Sampling {
            scene_threshold: self.video_scene_threshold.clamp(0.05, 1.0),
            floor: std::time::Duration::from_secs(self.video_interval_secs.clamp(1, 3600) as u64),
            max_frames: self.video_max_frames.clamp(1, MAX_FRAMES_LIMIT),
        }
    }

    /// The model that rewrites the frames as narration: whichever text model
    /// was named, or the vision model that described them.
    pub fn narration_model(&self) -> &str {
        let named = self.narration_model.trim();
        if named.is_empty() {
            self.ollama_model.trim()
        } else {
            named
        }
    }

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

    /// Puts every setting back to its default, keeping what the user wrote.
    ///
    /// The dictionary is one exception: every other field here is a preference
    /// that takes seconds to set again, whereas a list of replacements is built
    /// up a word at a time over months of use. A "reset settings" button that
    /// quietly threw it away would be a far worse bug than any setting it put
    /// right.
    ///
    /// The language is the other, and for a starker reason — a reset that also
    /// changed the language would leave the user unable to read the message
    /// telling them what had just happened.
    pub fn reset_to_defaults(&mut self) {
        let dictionary = std::mem::take(&mut self.dictionary);
        let language = std::mem::take(&mut self.language);
        *self = Self {
            dictionary,
            language,
            ..Self::default()
        };
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

    /// A reset gives back the defaults — and must not take the one thing on
    /// the config that took the user any effort to make.
    #[test]
    fn resetting_restores_defaults_but_keeps_the_dictionary() {
        let mut config = Config {
            system_rate: 300,
            ollama_model: "minicpm-v:8b".to_string(),
            ollama_prompt: "Just read the numbers out.".to_string(),
            engine: EnginePreference::ElevenLabs,
            dictionary: vec![Replacement {
                from: "Dr".to_string(),
                to: "Doctor".to_string(),
                whole_word: true,
            }],
            ..Config::default()
        };
        config.reset_to_defaults();

        assert_eq!(config.system_rate, 175);
        assert_eq!(config.ollama_model, DEFAULT_VISION_MODEL);
        assert_eq!(config.ollama_prompt, default_vision_prompt());
        assert_eq!(config.engine, EnginePreference::System);
        assert_eq!(config.dictionary.len(), 1, "the dictionary must survive");
        assert_eq!(config.dictionary[0].from, "Dr");
    }

    /// The prompt that answered a photograph of a city square with the words
    /// "ONE WAY" is saved in the config of everyone who has used the app, so
    /// loading it has to be what replaces it.
    #[test]
    fn the_superseded_prompt_is_replaced_on_load() {
        let saved = serde_json::json!({ "ollama_prompt": SUPERSEDED_VISION_PROMPT }).to_string();
        let config: Config = serde_json::from_str(&saved).unwrap();
        assert_eq!(config.ollama_prompt, default_vision_prompt());
    }

    /// The description has to be unconditional. A prompt that asks for one only
    /// when there is no text is the bug, whatever else it says.
    #[test]
    fn the_default_prompt_asks_for_a_description_before_any_condition() {
        // Pinned to English: the wording being checked is English wording, and
        // holding every translation to it would be checking the wrong thing.
        let prompt = crate::i18n::with_language("en", default_vision_prompt);
        assert!(prompt.starts_with("Describe what this image shows"));
        assert!(
            !prompt.contains("contains no text"),
            "the description must not be conditional on the image having no text"
        );
        // Spoken output, so the model has to be told not to write Markdown.
        assert!(prompt.contains("read aloud"));
    }

    /// A prompt somebody has written for themselves is theirs.
    #[test]
    fn a_hand_written_prompt_survives_loading() {
        let saved = r#"{ "ollama_prompt": "Just read the numbers out." }"#;
        let config: Config = serde_json::from_str(saved).unwrap();
        assert_eq!(config.ollama_prompt, "Just read the numbers out.");
    }

    #[test]
    fn a_deliberately_chosen_model_is_left_alone() {
        let saved = r#"{ "ollama_model": "minicpm-v:8b" }"#;
        let config: Config = serde_json::from_str(saved).unwrap();
        assert_eq!(config.ollama_model, "minicpm-v:8b");
    }

    /// A config written by an older version has none of the video settings in
    /// it, and must come back with working ones rather than zeroes — a scene
    /// threshold of 0 would take every frame in the video.
    #[test]
    fn a_config_from_before_video_gets_working_video_settings() {
        let saved = r#"{ "ollama_model": "qwen2.5vl:3b" }"#;
        let config: Config = serde_json::from_str(saved).unwrap();

        assert_eq!(config.video_scene_threshold, DEFAULT_SCENE_THRESHOLD);
        assert_eq!(config.video_interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(config.video_max_frames, DEFAULT_MAX_FRAMES);
        assert!(config.video_narrate);
        assert!(!config.video_frame_prompt.is_empty());
        assert!(!config.video_narration_prompt.is_empty());
    }

    /// The sampling is what a video costs in time, so nonsense in the file must
    /// not become nonsense in the job.
    #[test]
    fn saved_sampling_numbers_are_held_to_survivable_ranges() {
        let absurd: Config = serde_json::from_str(
            r#"{ "video_scene_threshold": 0.0, "video_interval_secs": 0, "video_max_frames": 100000 }"#,
        )
        .unwrap();
        let sampling = absurd.video_sampling();

        // A threshold of zero selects every frame; a floor of zero does too.
        assert!(sampling.scene_threshold >= 0.05);
        assert!(sampling.floor >= std::time::Duration::from_secs(1));
        assert_eq!(sampling.max_frames, MAX_FRAMES_LIMIT);

        // And the ordinary case passes through untouched.
        let sane = Config::default().video_sampling();
        assert_eq!(sane.scene_threshold, DEFAULT_SCENE_THRESHOLD);
        assert_eq!(sane.max_frames, DEFAULT_MAX_FRAMES);
    }

    /// Left empty, the narration is written by the model that is already
    /// downloaded — which is the only reason this feature needs no second
    /// multi-gigabyte download to work at all.
    #[test]
    fn the_vision_model_writes_the_narration_unless_another_is_named() {
        let mut config = Config {
            ollama_model: "qwen2.5vl:3b".to_string(),
            ..Default::default()
        };
        assert_eq!(config.narration_model(), "qwen2.5vl:3b");

        // Whitespace is not a model name.
        config.narration_model = "   ".to_string();
        assert_eq!(config.narration_model(), "qwen2.5vl:3b");

        config.narration_model = "llama3.2".to_string();
        assert_eq!(config.narration_model(), "llama3.2");
    }

    /// Both video prompts are read aloud, so both carry the instruction that
    /// keeps Markdown out of a synthesiser's mouth.
    #[test]
    fn the_video_prompts_ask_for_speakable_text() {
        let (frame, narration) = crate::i18n::with_language("en", || {
            (default_frame_prompt(), default_narration_prompt())
        });
        for prompt in [&frame, &narration] {
            assert!(prompt.contains("read aloud"), "{prompt}");
            assert!(prompt.contains("markdown"), "{prompt}");
        }
        // The frame prompt has to keep each answer short: it is one of dozens.
        assert!(frame.contains("one or two"));
        // And the narration prompt has to forbid the invented connective
        // tissue that makes a description track untrustworthy.
        assert!(narration.contains("do not invent"));
    }
}

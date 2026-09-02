//! Persisted settings and the on-disk layout.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::document::ChunkMode;
use crate::speech::EngineKind;
use crate::t;
use crate::wordlist::BlockPolicy;

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("org", "AccessEngine", "AccessEngine")
}

/// Where settings and bundled wordlists live.
pub fn config_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().to_path_buf())
}

/// Where logs live.
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

/// Directory holding the user's wordlists.
pub fn wordlist_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("wordlists"))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.json"))
}

/// The log folder chosen in Settings, read straight out of the file.
///
/// Logging starts before the rest of the app does, so this picks out the one
/// field it needs rather than calling [`Config::load`] — which logs about what
/// it found, at a point where there is nowhere yet to write that.
pub fn configured_log_dir() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let dir = value.get("log_dir")?.as_str()?.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// Light, dark, or whatever this computer is set to.
///
/// Following the system is the right default and the wrong rule: someone who
/// wants a dark window on a machine set to light, or a light window because
/// low-contrast dark themes are harder to read, is an ordinary case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> String {
        match self {
            Self::System => t!("appearance.system"),
            Self::Light => t!("appearance.light"),
            Self::Dark => t!("appearance.dark"),
        }
    }
}

/// What pressing Apply on the General tab does with what is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    #[default]
    ReadAloud,
    SaveAudio,
}

impl Output {
    pub const ALL: [Self; 2] = [Self::ReadAloud, Self::SaveAudio];

    pub fn label(self) -> String {
        match self {
            Self::ReadAloud => t!("output.read_aloud"),
            Self::SaveAudio => t!("output.save_audio"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: EngineKind,

    // System voice.
    // Rate and pitch are 0..=1 slider positions mapped onto whatever range the
    // platform back end reports. `None` means "leave the back end's default
    // alone", which is not the same as any particular position.
    pub system_voice_id: Option<String>,
    pub rate: Option<f32>,
    pub pitch: Option<f32>,
    /// 0..=1, applied to both engines.
    pub volume: f32,

    // ElevenLabs
    /// Only written to disk when `save_api_key` is true. The environment
    /// variable `ELEVENLABS_API_KEY` takes precedence over both.
    pub elevenlabs_api_key: String,
    pub save_api_key: bool,
    pub elevenlabs_voice_id: String,
    pub elevenlabs_voice_name: String,
    pub elevenlabs_model: String,
    pub elevenlabs_stability: f32,
    pub elevenlabs_similarity: f32,

    // Ollama
    pub ollama_url: String,
    pub ollama_model: String,
    pub ollama_prompt: String,
    /// Add "Taken in …" to the description of a geotagged photo. This is the
    /// one part of describing an image that leaves the machine, so it is a
    /// setting rather than a given.
    pub geotag_images: bool,

    // Appearance
    /// The language the interface is written in: a code like `fr`, or
    /// [`crate::i18n::AUTO`] for whatever this computer is set to.
    pub language: String,
    /// Light, dark, or the system's choice. See [`Appearance`].
    pub appearance: Appearance,
    /// Whether the document and image preview takes the right of the window.
    /// With it off the tabs have the whole window, which is what someone
    /// listening rather than following along wants.
    pub show_preview: bool,
    /// How large everything is drawn, as a multiplier. This is egui's zoom
    /// factor, so it scales the document text along with the controls — which
    /// is the point: the reader is for people who may need it bigger, and
    /// Ctrl/Cmd + plus is not much use if you cannot read the menu to find it.
    pub text_scale: f32,

    // Reading
    pub chunk_mode: ChunkMode,
    /// What Apply on the General tab does with the open file.
    pub output: Output,
    /// Short cues when reading finishes or something fails.
    pub sounds_enabled: bool,

    // Wordlists
    pub block_policy: BlockPolicy,
    pub bleep_text: String,
    /// Lists explicitly switched off; anything else found on disk is enabled.
    pub disabled_wordlists: Vec<String>,
    pub wordlists_enabled: bool,

    // Updates
    /// Check GitHub for a newer release on startup.
    pub check_for_updates: bool,
    /// The version last dismissed with "Skip this version", so the dialog
    /// does not keep offering a release the user has already declined.
    pub skipped_update_version: String,

    // Logging
    /// Where the daily log files are written, when the user has chosen
    /// somewhere other than the platform's own data directory.
    pub log_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EngineKind::System,
            system_voice_id: None,
            rate: None,
            pitch: None,
            volume: 1.0,
            language: crate::i18n::AUTO.to_string(),
            appearance: Appearance::default(),
            show_preview: true,
            text_scale: 1.0,
            elevenlabs_api_key: String::new(),
            save_api_key: false,
            elevenlabs_voice_id: String::new(),
            elevenlabs_voice_name: String::new(),
            elevenlabs_model: "eleven_multilingual_v2".to_string(),
            elevenlabs_stability: 0.5,
            elevenlabs_similarity: 0.75,
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: String::new(),
            ollama_prompt: DEFAULT_VISION_PROMPT.to_string(),
            geotag_images: true,
            chunk_mode: ChunkMode::Sentence,
            output: Output::default(),
            sounds_enabled: true,
            block_policy: BlockPolicy::Bleep,
            bleep_text: "beep".to_string(),
            disabled_wordlists: Vec::new(),
            wordlists_enabled: true,
            check_for_updates: true,
            skipped_update_version: String::new(),
            log_dir: None,
        }
    }
}

/// Hand-written so a `{cfg:?}` can never put a live credential in the log.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("engine", &self.engine)
            .field("system_voice_id", &self.system_voice_id)
            .field(
                "elevenlabs_api_key",
                &crate::speech::elevenlabs::redacted(&self.elevenlabs_api_key),
            )
            .field("save_api_key", &self.save_api_key)
            .field("elevenlabs_voice_id", &self.elevenlabs_voice_id)
            .field("elevenlabs_model", &self.elevenlabs_model)
            .field("ollama_url", &self.ollama_url)
            .field("ollama_model", &self.ollama_model)
            .field("language", &self.language)
            .field("chunk_mode", &self.chunk_mode)
            .field("geotag_images", &self.geotag_images)
            .field("log_dir", &self.log_dir)
            .field("block_policy", &self.block_policy)
            .field("wordlists_enabled", &self.wordlists_enabled)
            .finish_non_exhaustive()
    }
}

/// Smallest and largest interface scale offered, and accepted from the file.
pub const TEXT_SCALE_RANGE: (f32, f32) = (0.8, 2.5);

pub const DEFAULT_VISION_PROMPT: &str =
    "Describe this image for someone who cannot see it. Lead with the single most \
     important thing in the image, then add detail in descending order of importance. \
     Read out any visible text verbatim. Use plain prose with no markdown, no bullet \
     points and no preamble such as 'This image shows'. Keep it under 150 words.";

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            log::warn!("no platform config directory; using defaults");
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    log::info!("loaded settings from {}", path.display());
                    if !cfg.save_api_key {
                        cfg.elevenlabs_api_key.clear();
                    }
                    // A hand-edited or corrupted scale must not leave someone
                    // with a window they cannot read well enough to fix it.
                    if !cfg.text_scale.is_finite() {
                        cfg.text_scale = 1.0;
                    }
                    cfg.text_scale = cfg.text_scale.clamp(TEXT_SCALE_RANGE.0, TEXT_SCALE_RANGE.1);
                    cfg
                }
                Err(e) => {
                    log::error!("settings at {} are unreadable ({e}); using defaults", path.display());
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no settings file yet; starting from defaults");
                Self::default()
            }
            Err(e) => {
                log::error!("could not read settings: {e}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context("no platform config directory available")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // Never write the key out unless the user asked for it.
        let mut to_write = self.clone();
        if !to_write.save_api_key {
            to_write.elevenlabs_api_key.clear();
        }

        let json = serde_json::to_string_pretty(&to_write)?;

        // Write to a temporary file and rename over the target. Two reasons:
        // an interrupted save cannot leave a half-written settings file, and
        // the temporary is created owner-only from the outset — `fs::write`
        // would create it 0644 and leave the key world-readable until the
        // chmod landed.
        let temporary = path.with_extension("json.tmp");
        write_private(&temporary, json.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &path).with_context(|| {
            format!("replacing {} with {}", path.display(), temporary.display())
        })?;
        // A rename preserves the source's mode, but an existing target that
        // predates this code may still be too open; make sure either way.
        restrict_permissions(&path);
        log::debug!("saved settings to {}", path.display());
        Ok(())
    }

    /// The key actually used for requests: the environment wins, so a shared
    /// machine can supply a key without it ever reaching the config file.
    pub fn effective_api_key(&self) -> String {
        match std::env::var("ELEVENLABS_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => self.elevenlabs_api_key.trim().to_string(),
        }
    }

    pub fn api_key_from_env(&self) -> bool {
        std::env::var("ELEVENLABS_API_KEY").is_ok_and(|k| !k.trim().is_empty())
    }
}

/// Create a file that is owner-only from the moment it exists, and write to it.
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
    file.write_all(bytes)?;
    // Settings that survive a power cut are worth the one fsync per save.
    file.sync_all()
}

/// The settings file may hold an API key, so keep it owner-only on Unix.
/// Windows inherits the user profile ACL, which is already user-scoped.
fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            log::warn!("could not restrict permissions on {}: {e}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings file can hold an API key, so its `Debug` must not.
    /// A nonsense scale in the settings file must not produce an unreadable
    /// or invisible interface.
    #[test]
    fn absurd_text_scales_are_brought_back_into_range() {
        for (written, expected) in [
            ("0.0", TEXT_SCALE_RANGE.0),
            ("99.0", TEXT_SCALE_RANGE.1),
            ("-3.0", TEXT_SCALE_RANGE.0),
            ("1.25", 1.25),
        ] {
            let json = format!(r#"{{"text_scale": {written}}}"#);
            let mut cfg: Config = serde_json::from_str(&json).expect("parses");
            if !cfg.text_scale.is_finite() {
                cfg.text_scale = 1.0;
            }
            cfg.text_scale = cfg.text_scale.clamp(TEXT_SCALE_RANGE.0, TEXT_SCALE_RANGE.1);
            assert_eq!(cfg.text_scale, expected, "{written}");
        }
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let cfg = Config {
            elevenlabs_api_key: "sk_super_secret_value".to_string(),
            save_api_key: true,
            ..Config::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("sk_super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    /// Serialising for disk must drop the key unless it was asked for.
    #[test]
    fn the_key_is_not_serialised_unless_requested() {
        let mut cfg = Config {
            elevenlabs_api_key: "sk_super_secret_value".to_string(),
            save_api_key: false,
            ..Config::default()
        };
        // `save` applies this rule; replicate it here without touching disk.
        let mut to_write = cfg.clone();
        if !to_write.save_api_key {
            to_write.elevenlabs_api_key.clear();
        }
        let json = serde_json::to_string(&to_write).expect("serialises");
        assert!(!json.contains("sk_super_secret_value"), "{json}");

        cfg.save_api_key = true;
        let json = serde_json::to_string(&cfg).expect("serialises");
        assert!(json.contains("sk_super_secret_value"));
    }

    #[test]
    fn a_private_file_is_owner_only_from_the_start() {
        let dir = std::env::temp_dir().join(format!("accessengine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("secret.json");
        write_private(&path, b"{}").expect("writes");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

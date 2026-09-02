//! Persisted settings and the on-disk layout.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::document::ChunkMode;
use crate::speech::cloud::VoiceRequest;
use crate::speech::{cloud, deepgram, elevenlabs, google, openai, polly, EngineKind};
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

    // ------------------------------------------------------ cloud providers
    //
    // Every provider's credential follows the same rule, and it is the rule
    // this app has always had for the ElevenLabs one: it is only written to
    // disk when `save_api_key` is true, the matching environment variable
    // takes precedence over whatever is in the file, and the file itself is
    // owner-only on Unix. `save_api_key` governs all of them together —
    // it is one question ("remember my keys on this computer"), and asking it
    // once per provider would be five ways to get the same answer wrong.
    //
    // Storage is plain text in `config.json`. That is a deliberate carry-over
    // rather than an oversight: it is what this app already did with the
    // ElevenLabs key, the UI says so where the key is entered, and moving five
    // credentials into a platform keychain is a piece of work in its own right
    // rather than a side effect of adding providers.
    pub save_api_key: bool,

    // ElevenLabs. The environment variable is `ELEVENLABS_API_KEY`.
    pub elevenlabs_api_key: String,
    pub elevenlabs_voice_id: String,
    pub elevenlabs_voice_name: String,
    pub elevenlabs_model: String,
    pub elevenlabs_stability: f32,
    pub elevenlabs_similarity: f32,

    // OpenAI. The environment variable is `OPENAI_API_KEY`.
    pub openai_api_key: String,
    pub openai_voice_id: String,
    pub openai_voice_name: String,
    pub openai_model: String,
    /// 0.25 to 4.0, where 1.0 is the voice as recorded.
    pub openai_speed: f32,
    /// How to read, in words. Only `gpt-4o-mini-tts` listens to it.
    pub openai_instructions: String,

    // Deepgram. The environment variable is `DEEPGRAM_API_KEY`. Deepgram has
    // no separate model setting: the Aura model *is* the voice.
    pub deepgram_api_key: String,
    pub deepgram_voice_id: String,
    pub deepgram_voice_name: String,

    // Google Cloud. The environment variable is `GOOGLE_API_KEY`.
    pub google_api_key: String,
    pub google_voice_id: String,
    pub google_voice_name: String,
    /// The BCP-47 code of the chosen voice, saved because the API asks for it
    /// alongside the voice name and will not infer it.
    pub google_language: String,
    pub google_speaking_rate: f32,
    pub google_pitch: f32,

    // Amazon Polly. Credentials come from the standard AWS chain first — see
    // `speech::polly` — and only fall back to the two fields here.
    pub polly_access_key_id: String,
    pub polly_secret_access_key: String,
    pub polly_region: String,
    /// Which `~/.aws/credentials` profile to read, when that is where the
    /// credentials are coming from.
    pub polly_profile: String,
    pub polly_voice_id: String,
    pub polly_voice_name: String,
    /// neural, generative, long-form or standard.
    pub polly_engine: String,

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
            save_api_key: false,
            elevenlabs_api_key: String::new(),
            elevenlabs_voice_id: String::new(),
            elevenlabs_voice_name: String::new(),
            elevenlabs_model: elevenlabs::DEFAULT_MODEL.to_string(),
            elevenlabs_stability: 0.5,
            elevenlabs_similarity: 0.75,
            openai_api_key: String::new(),
            // OpenAI publishes no endpoint that lists its voices, so unlike
            // the others this one can start out chosen rather than empty.
            openai_voice_id: openai::DEFAULT_VOICE.to_string(),
            openai_voice_name: "Alloy".to_string(),
            openai_model: openai::DEFAULT_MODEL.to_string(),
            openai_speed: 1.0,
            openai_instructions: String::new(),
            deepgram_api_key: String::new(),
            deepgram_voice_id: deepgram::DEFAULT_VOICE.to_string(),
            deepgram_voice_name: "Thalia".to_string(),
            google_api_key: String::new(),
            google_voice_id: String::new(),
            google_voice_name: String::new(),
            google_language: String::new(),
            google_speaking_rate: 1.0,
            google_pitch: 0.0,
            polly_access_key_id: String::new(),
            polly_secret_access_key: String::new(),
            polly_region: polly::DEFAULT_REGION.to_string(),
            polly_profile: polly::DEFAULT_PROFILE.to_string(),
            polly_voice_id: String::new(),
            polly_voice_name: String::new(),
            polly_engine: polly::DEFAULT_ENGINE.to_string(),
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
            .field("save_api_key", &self.save_api_key)
            // Every credential goes through `redacted`, so adding a provider
            // cannot add a way for one to reach the log.
            .field(
                "elevenlabs_api_key",
                &cloud::redacted(&self.elevenlabs_api_key),
            )
            .field("elevenlabs_voice_id", &self.elevenlabs_voice_id)
            .field("elevenlabs_model", &self.elevenlabs_model)
            .field("openai_api_key", &cloud::redacted(&self.openai_api_key))
            .field("openai_voice_id", &self.openai_voice_id)
            .field("openai_model", &self.openai_model)
            .field("deepgram_api_key", &cloud::redacted(&self.deepgram_api_key))
            .field("deepgram_voice_id", &self.deepgram_voice_id)
            .field("google_api_key", &cloud::redacted(&self.google_api_key))
            .field("google_voice_id", &self.google_voice_id)
            .field("polly_access_key_id", &self.polly_access_key_id)
            .field(
                "polly_secret_access_key",
                &cloud::redacted(&self.polly_secret_access_key),
            )
            .field("polly_region", &self.polly_region)
            .field("polly_voice_id", &self.polly_voice_id)
            .field("polly_engine", &self.polly_engine)
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
                        cfg.forget_credentials();
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
                    log::error!(
                        "settings at {} are unreadable ({e}); using defaults",
                        path.display()
                    );
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
            to_write.forget_credentials();
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

    /// Empty every stored credential, in place.
    ///
    /// Used both when the settings file is read with "remember" switched off
    /// and when it is written, so a key that was only ever meant for this
    /// session cannot survive into the next one by either route.
    fn forget_credentials(&mut self) {
        self.elevenlabs_api_key.clear();
        self.openai_api_key.clear();
        self.deepgram_api_key.clear();
        self.google_api_key.clear();
        self.polly_access_key_id.clear();
        self.polly_secret_access_key.clear();
    }

    /// The environment variable a provider's key may arrive in, if it has one.
    ///
    /// Polly has none here because AWS credentials are a set rather than a
    /// single string; [`crate::speech::polly::environment_credentials`] reads
    /// the three standard variables instead.
    fn key_variable(engine: EngineKind) -> Option<&'static str> {
        match engine {
            EngineKind::ElevenLabs => Some("ELEVENLABS_API_KEY"),
            EngineKind::OpenAi => Some("OPENAI_API_KEY"),
            EngineKind::Deepgram => Some("DEEPGRAM_API_KEY"),
            EngineKind::Google => Some("GOOGLE_API_KEY"),
            EngineKind::System | EngineKind::Polly => None,
        }
    }

    /// The key this provider stores in the settings file.
    fn stored_api_key(&self, engine: EngineKind) -> &str {
        match engine {
            EngineKind::ElevenLabs => &self.elevenlabs_api_key,
            EngineKind::OpenAi => &self.openai_api_key,
            EngineKind::Deepgram => &self.deepgram_api_key,
            EngineKind::Google => &self.google_api_key,
            EngineKind::System | EngineKind::Polly => "",
        }
    }

    pub fn set_api_key(&mut self, engine: EngineKind, key: &str) {
        let key = key.trim().to_string();
        match engine {
            EngineKind::ElevenLabs => self.elevenlabs_api_key = key,
            EngineKind::OpenAi => self.openai_api_key = key,
            EngineKind::Deepgram => self.deepgram_api_key = key,
            EngineKind::Google => self.google_api_key = key,
            EngineKind::System | EngineKind::Polly => {}
        }
    }

    /// The key actually used for requests: the environment wins, so a shared
    /// machine can supply a key without it ever reaching the config file.
    pub fn effective_api_key(&self, engine: EngineKind) -> String {
        if let Some(variable) = Self::key_variable(engine) {
            if let Ok(key) = std::env::var(variable) {
                if !key.trim().is_empty() {
                    return key.trim().to_string();
                }
            }
        }
        self.stored_api_key(engine).trim().to_string()
    }

    /// Whether the key in use came from the environment, which the UI says out
    /// loud: a key it cannot change is not one to offer a dialog for.
    pub fn api_key_from_env(&self, engine: EngineKind) -> bool {
        match engine {
            EngineKind::Polly => polly::environment_credentials().is_some(),
            _ => Self::key_variable(engine)
                .and_then(|variable| std::env::var(variable).ok())
                .is_some_and(|key| !key.trim().is_empty()),
        }
    }

    /// The AWS credentials Polly will be asked with, from the standard chain —
    /// environment, then this app's own fields, then `~/.aws/credentials`.
    /// See [`crate::speech::polly`] for why that order.
    pub fn aws_credentials(&self) -> Option<polly::Credentials> {
        if let Some(from_env) = polly::environment_credentials() {
            return Some(from_env);
        }
        let typed = polly::Credentials {
            access_key_id: self.polly_access_key_id.trim().to_string(),
            secret_access_key: self.polly_secret_access_key.trim().to_string(),
            session_token: String::new(),
        };
        if !typed.access_key_id.is_empty() && !typed.secret_access_key.is_empty() {
            return Some(typed);
        }
        polly::shared(&self.polly_profile).credentials
    }

    /// The region Polly is asked in: the setting, then the environment, then
    /// whatever `~/.aws/config` says.
    pub fn aws_region(&self) -> String {
        let configured = self.polly_region.trim();
        if !configured.is_empty() {
            return configured.to_string();
        }
        polly::environment_region()
            .or_else(|| polly::shared(&self.polly_profile).region)
            .unwrap_or_else(|| polly::DEFAULT_REGION.to_string())
    }

    /// Whether this engine has everything it needs to make a request.
    ///
    /// The one question the UI asks before deciding whether to put the button
    /// for entering a credential on screen.
    pub fn has_credentials(&self, engine: EngineKind) -> bool {
        match engine {
            EngineKind::System => true,
            EngineKind::Polly => self.aws_credentials().is_some(),
            _ => !self.effective_api_key(engine).is_empty(),
        }
    }

    /// The voice chosen for an engine: what the provider is sent, and what the
    /// user sees.
    ///
    /// Kept per provider rather than in one field, so switching engine to try
    /// another voice and switching back does not lose the first choice.
    pub fn cloud_voice(&self, engine: EngineKind) -> (&str, &str) {
        match engine {
            EngineKind::ElevenLabs => (&self.elevenlabs_voice_id, &self.elevenlabs_voice_name),
            EngineKind::OpenAi => (&self.openai_voice_id, &self.openai_voice_name),
            EngineKind::Deepgram => (&self.deepgram_voice_id, &self.deepgram_voice_name),
            EngineKind::Google => (&self.google_voice_id, &self.google_voice_name),
            EngineKind::Polly => (&self.polly_voice_id, &self.polly_voice_name),
            EngineKind::System => ("", ""),
        }
    }

    pub fn set_cloud_voice(&mut self, engine: EngineKind, voice: &cloud::RemoteVoice) {
        let (id, name) = (voice.id.clone(), voice.name.clone());
        match engine {
            EngineKind::ElevenLabs => {
                self.elevenlabs_voice_id = id;
                self.elevenlabs_voice_name = name;
            }
            EngineKind::OpenAi => {
                self.openai_voice_id = id;
                self.openai_voice_name = name;
            }
            EngineKind::Deepgram => {
                self.deepgram_voice_id = id;
                self.deepgram_voice_name = name;
            }
            EngineKind::Google => {
                // Google asks for the language alongside the voice, so it is
                // saved at the moment the voice is picked — the only moment
                // the two are known to belong together.
                self.google_language = if voice.language.is_empty() {
                    google::language_of(&voice.id)
                } else {
                    voice.language.clone()
                };
                self.google_voice_id = id;
                self.google_voice_name = name;
            }
            EngineKind::Polly => {
                // A voice that cannot be spoken by the engine currently chosen
                // would fail on the first sentence, so the engine moves to one
                // this voice does support rather than leaving the pair broken.
                if !voice.engines.is_empty() && !voice.engines.contains(&self.polly_engine) {
                    if let Some(best) = polly::ENGINES
                        .iter()
                        .map(|(id, _)| *id)
                        .find(|id| voice.engines.iter().any(|e| e == id))
                    {
                        log::info!(
                            "{name} cannot speak with the {} engine; using {best}",
                            self.polly_engine
                        );
                        self.polly_engine = best.to_string();
                    }
                }
                self.polly_voice_id = id;
                self.polly_voice_name = name;
            }
            EngineKind::System => {}
        }
    }

    /// Everything the worker needs to speak, for the engine currently chosen.
    ///
    /// `None` for the system voices, which are not a request at all. Built here
    /// rather than in the UI so that there is one place where a credential is
    /// read out of the settings, and so the export thread and the playback
    /// worker are asking with exactly the same thing.
    pub fn voice_request(&self) -> Option<VoiceRequest> {
        Some(match self.engine {
            EngineKind::System => return None,
            EngineKind::ElevenLabs => VoiceRequest::ElevenLabs(elevenlabs::Request {
                api_key: self.effective_api_key(EngineKind::ElevenLabs),
                voice_id: self.elevenlabs_voice_id.clone(),
                model: self.elevenlabs_model.clone(),
                stability: self.elevenlabs_stability,
                similarity: self.elevenlabs_similarity,
            }),
            EngineKind::OpenAi => VoiceRequest::OpenAi(openai::Request {
                api_key: self.effective_api_key(EngineKind::OpenAi),
                voice_id: self.openai_voice_id.clone(),
                model: self.openai_model.clone(),
                speed: self.openai_speed,
                instructions: self.openai_instructions.clone(),
            }),
            EngineKind::Deepgram => VoiceRequest::Deepgram(deepgram::Request {
                api_key: self.effective_api_key(EngineKind::Deepgram),
                voice_id: self.deepgram_voice_id.clone(),
            }),
            EngineKind::Google => VoiceRequest::Google(google::Request {
                api_key: self.effective_api_key(EngineKind::Google),
                voice_id: self.google_voice_id.clone(),
                language: self.google_language.clone(),
                speaking_rate: self.google_speaking_rate,
                pitch: self.google_pitch,
            }),
            EngineKind::Polly => VoiceRequest::Polly(polly::Request {
                credentials: self
                    .aws_credentials()
                    .unwrap_or_else(|| polly::Credentials {
                        access_key_id: String::new(),
                        secret_access_key: String::new(),
                        session_token: String::new(),
                    }),
                region: self.aws_region(),
                voice_id: self.polly_voice_id.clone(),
                engine: self.polly_engine.clone(),
            }),
        })
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

    /// Serialising for disk must drop every key unless it was asked for — not
    /// only the one provider that had this rule before the others existed.
    #[test]
    fn no_key_is_serialised_unless_requested() {
        let mut cfg = Config {
            elevenlabs_api_key: "el_secret".to_string(),
            openai_api_key: "sk-openai_secret".to_string(),
            deepgram_api_key: "dg_secret".to_string(),
            google_api_key: "AIza_secret".to_string(),
            polly_secret_access_key: "aws_secret".to_string(),
            save_api_key: false,
            ..Config::default()
        };
        // `save` applies this rule; replicate it here without touching disk.
        let mut to_write = cfg.clone();
        if !to_write.save_api_key {
            to_write.forget_credentials();
        }
        let json = serde_json::to_string(&to_write).expect("serialises");
        for secret in [
            "el_secret",
            "sk-openai_secret",
            "dg_secret",
            "AIza_secret",
            "aws_secret",
        ] {
            assert!(!json.contains(secret), "{secret} reached the file: {json}");
        }

        cfg.save_api_key = true;
        let json = serde_json::to_string(&cfg).expect("serialises");
        assert!(json.contains("el_secret"));
        assert!(json.contains("aws_secret"));
    }

    /// A settings file written before the other providers existed must load
    /// exactly as it did, with every field it does carry preserved and every
    /// field it does not filled in from the defaults.
    #[test]
    fn a_settings_file_from_before_the_other_providers_still_loads() {
        let existing = r#"{
          "engine": "ElevenLabs",
          "system_voice_id": "com.apple.voice.compact.en-GB.Daniel",
          "rate": 0.6,
          "pitch": null,
          "volume": 0.8,
          "elevenlabs_api_key": "sk_kept",
          "save_api_key": true,
          "elevenlabs_voice_id": "21m00Tcm4TlvDq8ikWAM",
          "elevenlabs_voice_name": "Rachel",
          "elevenlabs_model": "eleven_turbo_v2_5",
          "elevenlabs_stability": 0.3,
          "elevenlabs_similarity": 0.9,
          "ollama_url": "http://localhost:11434",
          "chunk_mode": "Sentence",
          "wordlists_enabled": false,
          "disabled_wordlists": ["classroom-safe"]
        }"#;
        let cfg: Config =
            serde_json::from_str(existing).expect("an old settings file still parses");

        assert_eq!(cfg.engine, EngineKind::ElevenLabs);
        assert_eq!(cfg.elevenlabs_api_key, "sk_kept");
        assert_eq!(cfg.elevenlabs_voice_id, "21m00Tcm4TlvDq8ikWAM");
        assert_eq!(cfg.elevenlabs_voice_name, "Rachel");
        assert_eq!(cfg.elevenlabs_model, "eleven_turbo_v2_5");
        assert_eq!(cfg.elevenlabs_stability, 0.3);
        assert_eq!(cfg.elevenlabs_similarity, 0.9);
        assert_eq!(cfg.volume, 0.8);
        assert_eq!(cfg.rate, Some(0.6));
        assert!(!cfg.wordlists_enabled);
        assert_eq!(cfg.disabled_wordlists, ["classroom-safe"]);

        // And the providers it had never heard of arrive at their defaults
        // rather than as empty strings that would fail on first use.
        assert_eq!(cfg.openai_model, openai::DEFAULT_MODEL);
        assert_eq!(cfg.openai_speed, 1.0);
        assert_eq!(cfg.deepgram_voice_id, deepgram::DEFAULT_VOICE);
        assert_eq!(cfg.polly_region, polly::DEFAULT_REGION);
        assert_eq!(cfg.polly_engine, polly::DEFAULT_ENGINE);
        assert_eq!(cfg.google_speaking_rate, 1.0);
    }

    /// The settings file is hand-editable and written by other versions of
    /// this app, so nothing in it should be able to stop the app starting.
    #[test]
    fn an_unreadable_engine_costs_one_setting_rather_than_the_file() {
        let cfg: Config =
            serde_json::from_str(r#"{"engine":"Vogon","volume":0.4}"#).expect("still parses");
        assert_eq!(cfg.engine, EngineKind::System);
        assert_eq!(cfg.volume, 0.4, "the rest of the file has to survive");
    }

    /// Each provider keeps its own voice, so trying another engine and coming
    /// back does not lose the choice already made.
    #[test]
    fn every_provider_remembers_its_own_voice() {
        let mut cfg = Config::default();
        for (engine, voice) in [
            (EngineKind::ElevenLabs, "21m00Tcm4TlvDq8ikWAM"),
            (EngineKind::OpenAi, "nova"),
            (EngineKind::Deepgram, "aura-2-thalia-en"),
            (EngineKind::Google, "en-GB-Neural2-A"),
            (EngineKind::Polly, "Amy"),
        ] {
            cfg.set_cloud_voice(
                engine,
                &cloud::RemoteVoice {
                    id: voice.to_string(),
                    name: format!("{voice} by name"),
                    description: String::new(),
                    language: String::new(),
                    engines: Vec::new(),
                },
            );
        }
        for (engine, voice) in [
            (EngineKind::ElevenLabs, "21m00Tcm4TlvDq8ikWAM"),
            (EngineKind::OpenAi, "nova"),
            (EngineKind::Deepgram, "aura-2-thalia-en"),
            (EngineKind::Google, "en-GB-Neural2-A"),
            (EngineKind::Polly, "Amy"),
        ] {
            let (id, name) = cfg.cloud_voice(engine);
            assert_eq!(id, voice, "{engine:?}");
            assert_eq!(name, format!("{voice} by name"), "{engine:?}");
        }
        // Google needs the language too, and reads it off the voice name when
        // the provider did not say.
        assert_eq!(cfg.google_language, "en-GB");
    }

    /// Choosing a voice Polly cannot speak with the current engine would fail
    /// on the first sentence, so the engine follows the voice.
    #[test]
    fn a_polly_voice_brings_a_supported_engine_with_it() {
        let mut cfg = Config {
            polly_engine: "generative".to_string(),
            ..Config::default()
        };
        cfg.set_cloud_voice(
            EngineKind::Polly,
            &cloud::RemoteVoice {
                id: "Brian".to_string(),
                name: "Brian".to_string(),
                description: String::new(),
                language: "en-GB".to_string(),
                engines: vec!["neural".to_string(), "standard".to_string()],
            },
        );
        assert_eq!(cfg.polly_engine, "neural");

        // A voice that does support the chosen engine leaves it alone.
        cfg.polly_engine = "standard".to_string();
        cfg.set_cloud_voice(
            EngineKind::Polly,
            &cloud::RemoteVoice {
                id: "Amy".to_string(),
                name: "Amy".to_string(),
                description: String::new(),
                language: "en-GB".to_string(),
                engines: vec!["neural".to_string(), "standard".to_string()],
            },
        );
        assert_eq!(cfg.polly_engine, "standard");
    }

    /// The request handed to the worker has to be the one the chosen engine
    /// needs, with the voice and settings that were actually saved.
    #[test]
    fn the_request_matches_the_engine_in_use() {
        let mut cfg = Config {
            save_api_key: true,
            elevenlabs_voice_id: "abc".to_string(),
            ..Config::default()
        };
        assert!(
            cfg.voice_request().is_none(),
            "the system voices are not a request"
        );

        for engine in EngineKind::ALL.iter().filter(|e| e.is_cloud()) {
            cfg.engine = *engine;
            let request = cfg.voice_request().expect("a cloud engine has a request");
            assert_eq!(request.engine(), *engine);
        }
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

//! Background work and the messages it sends back to the UI.
//!
//! Every slow thing — network calls, model downloads, `brew install`, file
//! rendering — runs as a [`Job`] on its own thread and reports progress through
//! an [`Update`] channel that the UI drains each frame. Nothing here touches
//! egui, and nothing in the UI blocks on any of it.

use crate::audio::{self, AudioFormat};
use crate::config::{Config, Formatting};
use crate::extract::{self, FileKind};
use crate::geocode;
use crate::ollama;
use crate::tts::{self, Voice};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

/// Which synthesiser a job should use. Resolved from the user's preference and
/// whether a key is present before the job is queued, so jobs never have to ask.
#[derive(Debug, Clone)]
pub enum Engine {
    ElevenLabs {
        api_key: String,
        voice_id: String,
        model_id: String,
    },
    System {
        voice: String,
        rate: u32,
    },
}

#[derive(Debug, Clone)]
pub enum Job {
    /// Read a .txt or .docx straight off disk.
    ReadDocument {
        path: PathBuf,
        /// Only Word documents carry any; see [`crate::extract::docx`].
        formatting: Formatting,
    },
    /// Read an image, arranging Ollama first if it needs arranging.
    ReadImage { path: PathBuf, config: Box<Config> },
    /// Describe a video, arranging ffmpeg and Ollama first if they need it.
    ReadVideo { path: PathBuf, config: Box<Config> },
    /// Runs Homebrew's own installer, answering the password it asks for with
    /// a macOS dialog.
    InstallHomebrew,
    /// `brew install ollama`.
    InstallOllama,
    /// `brew install ffmpeg`, or winget's equivalent.
    InstallFfmpeg,
    /// Download a vision model.
    PullModel(String),
    /// Fetch the voice list for a key.
    LoadElevenLabsVoices(String),
    /// Produce audio for playback. Only ElevenLabs needs this; system voices
    /// are spoken directly by `say`.
    Synthesize { engine: Engine, text: String },
    /// Render to a file.
    Save {
        engine: Engine,
        text: String,
        path: PathBuf,
        format: AudioFormat,
        /// Audio already synthesised for this exact text and voice, so saving
        /// after a listen doesn't pay for the same ElevenLabs call twice.
        cached_mp3: Option<Arc<Vec<u8>>>,
    },
}

impl Job {
    /// Text shown in the progress bar while this job runs.
    pub fn status_label(&self) -> String {
        match self {
            Self::ReadDocument { path, .. } => format!(
                "Reading {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::ReadImage { path, .. } => format!(
                "Looking at {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::ReadVideo { path, .. } => format!(
                "Watching {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::InstallHomebrew => "Installing Homebrew…".to_string(),
            Self::InstallOllama => "Installing Ollama…".to_string(),
            Self::InstallFfmpeg => "Installing ffmpeg…".to_string(),
            Self::PullModel(model) => format!("Downloading {model}…"),
            Self::LoadElevenLabsVoices(_) => "Loading ElevenLabs voices…".to_string(),
            Self::Synthesize { .. } => "Synthesising speech…".to_string(),
            Self::Save { path, .. } => format!(
                "Saving {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }

    /// Whether the Cancel button should appear. Interrupting an install
    /// halfway would leave a mess, so none of them is cancellable.
    pub fn is_cancellable(&self) -> bool {
        !matches!(
            self,
            Self::InstallHomebrew | Self::InstallOllama | Self::InstallFfmpeg
        )
    }
}

/// Messages from a running job.
pub enum Update {
    /// Replace the status line.
    Status(String),
    /// Fraction complete, 0.0 to 1.0.
    Progress(f32),
    /// A line of output from a subprocess or download, for the log pane.
    Log(String),
    /// Extracted text is ready to be spoken.
    TextReady {
        text: String,
        note: String,
    },
    ElevenLabsVoices(Vec<Voice>),
    /// The voice list could not be fetched; the UI shows this next to the
    /// picker rather than retrying forever.
    ElevenLabsVoicesFailed(String),
    /// ElevenLabs turned the key down. Its own message rather than an
    /// [`Update::Error`] because it is the one failure the UI does something
    /// about — see [`tts::elevenlabs::KeyRejected`] — instead of only saying.
    ApiKeyRejected,
    /// MP3 for playback.
    Mp3Ready(Arc<Vec<u8>>),
    Saved(PathBuf),
    /// The image needs Ollama and it isn't installed.
    NeedsOllamaInstall,
    /// The video needs ffmpeg and it isn't installed.
    NeedsFfmpegInstall,
    /// Ollama is there but the vision model isn't.
    NeedsModel(String),
    /// A setup step finished; the UI can retry whatever was blocked. Carries
    /// its own message since "Ollama is ready" would be wrong to say after,
    /// say, only Homebrew just finished installing.
    SetupComplete(String),
    Error(String),
    /// Always sent last, whatever the outcome.
    Finished,
}

/// A cancellation flag shared with the UI thread.
pub type Cancel = Arc<AtomicBool>;

fn cancelled(cancel: &Cancel) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// True for the failure that means the saved key is no good.
///
/// The whole chain is searched, not just the outermost error: a rejection that
/// happened while synthesising part 3 of 5 arrives wrapped in the context that
/// says so, and it is still a rejection.
fn is_key_rejection(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| e.is::<tts::elevenlabs::KeyRejected>())
}

/// Runs `job` on a new thread. The thread always sends [`Update::Finished`],
/// so the UI can clear its busy state in exactly one place.
pub fn spawn(job: Job, tx: Sender<Update>, cancel: Cancel, repaint: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(error) = run(job, &tx, &cancel) {
            // A cancellation is the user's own doing, not something to report.
            if !cancelled(&cancel) {
                let _ = tx.send(if is_key_rejection(&error) {
                    Update::ApiKeyRejected
                } else {
                    Update::Error(format!("{error:#}"))
                });
            }
        }
        let _ = tx.send(Update::Finished);
        repaint();
    });
}

fn run(job: Job, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    match job {
        Job::ReadDocument { path, formatting } => read_document(path, formatting, tx),
        Job::ReadImage { path, config } => read_image(path, &config, tx, cancel),
        Job::ReadVideo { path, config } => read_video(path, &config, tx, cancel),
        Job::InstallHomebrew => install_homebrew(tx),
        Job::InstallOllama => install_ollama(tx),
        Job::InstallFfmpeg => install_ffmpeg(tx),
        Job::PullModel(model) => pull_model(model, tx, cancel),
        Job::LoadElevenLabsVoices(key) => {
            // Reported on its own channel so a failure is attached to the
            // voice picker instead of becoming a generic error.
            match tts::elevenlabs::list_voices(&key) {
                Ok(voices) => {
                    let _ = tx.send(Update::ElevenLabsVoices(voices));
                }
                // A key the account no longer honours is not a voice-picker
                // problem, so it does not go in the voice picker.
                Err(error) if is_key_rejection(&error) => {
                    let _ = tx.send(Update::ApiKeyRejected);
                }
                Err(error) => {
                    let _ = tx.send(Update::ElevenLabsVoicesFailed(format!("{error:#}")));
                }
            }
            Ok(())
        }
        Job::Synthesize { engine, text } => synthesize(engine, text, tx, cancel),
        Job::Save {
            engine,
            text,
            path,
            format,
            cached_mp3,
        } => save(engine, text, path, format, cached_mp3, tx, cancel),
    }
}

fn read_document(path: PathBuf, formatting: Formatting, tx: &Sender<Update>) -> Result<()> {
    let extracted = extract::extract_document(&path, formatting)?;
    let text = extracted.text;
    if text.trim().is_empty() {
        bail!(
            "{} contains no readable text",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    let kind = FileKind::from_path(&path)
        .map(FileKind::label)
        .unwrap_or("file");
    let mut note = format!("{} · {} characters", kind, text.chars().count());
    // The status line is the only part a screen reader announces by itself, so
    // a warning that lives only in the log would not reach the person it is
    // for. The headline goes here; the reader has already logged the detail.
    if let Some(caveat) = extracted.caveat {
        note.push_str(" · ");
        note.push_str(&caveat);
    }
    let _ = tx.send(Update::TextReady { text, note });
    Ok(())
}

/// Walks the image path: make sure Ollama exists, is running, and has the
/// model, then ask it what the picture says. Each missing prerequisite stops
/// the job and asks the UI to prompt, rather than installing things silently.
fn read_image(path: PathBuf, config: &Config, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    let _ = tx.send(Update::Status("Checking for Ollama…".into()));
    match ollama::status() {
        ollama::Status::NotInstalled => {
            let _ = tx.send(Update::NeedsOllamaInstall);
            return Ok(());
        }
        ollama::Status::NotRunning => {
            let _ = tx.send(Update::Status("Starting Ollama…".into()));
            ollama::ensure_running()?;
        }
        ollama::Status::Running => {}
    }

    let model = config.ollama_model.trim();
    if model.is_empty() {
        bail!("no Ollama vision model is configured");
    }
    let installed = ollama::installed_models()?;
    if !ollama::has_model(&installed, model) {
        let _ = tx.send(Update::NeedsModel(model.to_string()));
        return Ok(());
    }

    if cancelled(cancel) {
        return Ok(());
    }
    let _ = tx.send(Update::Status("Encoding the image…".into()));
    let encoded = extract::image::encode_for_ollama(&path)?;

    let _ = tx.send(Update::Status(format!("Asking {model} to read the image…")));
    let mut described = ollama::describe_image(model, &config.ollama_prompt, &encoded.base64)?;

    // Smaller vision models sometimes answer a long conditional prompt with
    // nothing at all. One retry with a plain question usually gets a real
    // answer, which is a better outcome than reporting failure.
    if described.text.is_empty() && !cancelled(cancel) {
        let _ = tx.send(Update::Log(format!(
            "{model} returned nothing; retrying with a simpler prompt"
        )));
        let _ = tx.send(Update::Status(format!("Asking {model} again…")));
        described = ollama::describe_image(
            model,
            crate::config::FALLBACK_VISION_PROMPT,
            &encoded.base64,
        )?;
    }

    // A model cut off by its context limit is not worth retrying — a simpler
    // prompt leaves the image exactly as large — so a fragment is reported
    // rather than spoken, and a long answer that merely lost its ending is
    // kept with a note in the log.
    if described.truncated {
        if described.text.chars().count() < ollama::FRAGMENT_CHARS {
            bail!("{}", ollama::explain_truncation(model));
        }
        let _ = tx.send(Update::Log(format!(
            "{model} ran out of context and stopped early; the end of this description is missing"
        )));
    }

    let mut text = extract::tidy(&described.text);
    if text.is_empty() {
        bail!("{model} could not read anything from this image");
    }

    // A bonus, not a requirement: a photo with no location tag, or a lookup
    // that fails, still leaves the description the model already gave.
    if let Some(location) = encoded.location
        && !cancelled(cancel)
    {
        let _ = tx.send(Update::Status(
            "Looking up where the photo was taken…".into(),
        ));
        match geocode::place_name(location.latitude, location.longitude) {
            Ok(place) => {
                text.push_str("\n\nTaken in ");
                text.push_str(&place);
                text.push('.');
            }
            Err(error) => {
                let _ = tx.send(Update::Log(format!(
                    "could not look up where the photo was taken: {error:#}"
                )));
            }
        }
    }

    let note = format!(
        "image read by {model} · {} characters",
        text.chars().count()
    );
    let _ = tx.send(Update::TextReady { text, note });
    Ok(())
}

/// How the progress bar is divided between the three stages of reading a video.
///
/// The middle one is nearly all of it because it is nearly all of the time:
/// pulling the frames out takes seconds and describing them takes a minute
/// each. A bar that gave the stages equal thirds would sit at 33% for an hour.
const EXTRACT_SHARE: f32 = 0.10;
const DESCRIBE_SHARE: f32 = 0.80;

/// A directory of extracted frames, removed when the job leaves this function
/// by any route — including a failure partway through, which would otherwise
/// leave several hundred megabytes of stills in the temporary directory.
struct FrameDir(PathBuf);

impl Drop for FrameDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            crate::log::line(format!(
                "video: could not clean up {} — {error}",
                self.0.display()
            ));
        }
    }
}

/// Reading a video: ffmpeg for the frames, a vision model for each of them, and
/// then a model to join what came back into something worth listening to.
///
/// The expensive part is the loop, and it is expensive per frame rather than
/// per video — which is why [`crate::ffmpeg::Sampling`] exists, why the status
/// line counts frames rather than saying "working…", and why cancelling is
/// checked between every one.
fn read_video(path: PathBuf, config: &Config, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let _ = tx.send(Update::Status("Checking for ffmpeg…".into()));
    if crate::ffmpeg::status() == crate::ffmpeg::Status::NotInstalled {
        let _ = tx.send(Update::NeedsFfmpegInstall);
        return Ok(());
    }

    let _ = tx.send(Update::Status("Checking for Ollama…".into()));
    match ollama::status() {
        ollama::Status::NotInstalled => {
            let _ = tx.send(Update::NeedsOllamaInstall);
            return Ok(());
        }
        ollama::Status::NotRunning => {
            let _ = tx.send(Update::Status("Starting Ollama…".into()));
            ollama::ensure_running()?;
        }
        ollama::Status::Running => {}
    }

    let model = config.ollama_model.trim();
    if model.is_empty() {
        bail!("no Ollama vision model is configured");
    }
    let installed = ollama::installed_models()?;
    if !ollama::has_model(&installed, model) {
        let _ = tx.send(Update::NeedsModel(model.to_string()));
        return Ok(());
    }
    // Asked for before any frames are described, not after: a missing narration
    // model discovered at the end would throw away an hour of inference.
    let narrator = config.narration_model().to_string();
    if config.video_narrate && !ollama::has_model(&installed, &narrator) {
        let _ = tx.send(Update::NeedsModel(narrator));
        return Ok(());
    }

    if cancelled(cancel) {
        return Ok(());
    }

    let length = crate::ffmpeg::duration(&path);
    let _ = tx.send(Update::Status(format!("Taking frames out of {name}…")));
    let _ = tx.send(Update::Progress(0.0));

    let dir = FrameDir(std::env::temp_dir().join(format!(
        "accessengine-frames-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )));
    std::fs::create_dir_all(&dir.0)
        .with_context(|| format!("could not create {}", dir.0.display()))?;

    let log = tx.clone();
    let frames = crate::ffmpeg::extract_frames(
        &path,
        config.video_sampling(),
        &dir.0,
        cancel,
        move |line| {
            let _ = log.send(Update::Log(line));
        },
    )?;
    if cancelled(cancel) {
        return Ok(());
    }
    let _ = tx.send(Update::Progress(EXTRACT_SHARE));

    let total = frames.len();
    let _ = tx.send(Update::Log(match length {
        Some(length) => format!(
            "{name} runs for {} — describing {total} frames from it",
            audio::spoken_time(length)
        ),
        None => format!("describing {total} frames from {name}"),
    }));

    let mut described: Vec<(std::time::Duration, String)> = Vec::with_capacity(total);
    for (index, frame) in frames.iter().enumerate() {
        if cancelled(cancel) {
            return Ok(());
        }
        let _ = tx.send(Update::Status(format!(
            "Describing frame {} of {total} — {}…",
            index + 1,
            extract::video::moment(frame.at).to_lowercase()
        )));

        let encoded = extract::image::encode_for_ollama(&frame.path)?;
        let answer = ollama::describe_image(model, &config.video_frame_prompt, &encoded.base64)?;
        // A frame cut off before the model produced a real answer — the same
        // failure `read_image` rejects outright, see `ollama::FRAGMENT_CHARS`
        // — is worth exactly as little here as an empty one, so it is dropped
        // the same way rather than entering the transcript as a garbled word.
        let fragment = answer.truncated && answer.text.chars().count() < ollama::FRAGMENT_CHARS;
        if answer.truncated && !fragment {
            let _ = tx.send(Update::Log(format!(
                "frame {} was cut off before {model} finished describing it",
                index + 1
            )));
        }
        // A frame the model had nothing to say about is one frame lost, not a
        // failed video, so it is dropped rather than retried — a retry here
        // costs another minute and buys one sentence out of dozens.
        let text = extract::tidy(&answer.text);
        if fragment {
            let _ = tx.send(Update::Log(format!(
                "{model} was cut off with too little of frame {} to keep",
                index + 1
            )));
        } else if text.is_empty() {
            let _ = tx.send(Update::Log(format!(
                "{model} had nothing to say about frame {}",
                index + 1
            )));
        } else {
            described.push((frame.at, text));
        }

        let done = (index + 1) as f32 / total as f32;
        let _ = tx.send(Update::Progress(EXTRACT_SHARE + DESCRIBE_SHARE * done));
    }

    if cancelled(cancel) {
        return Ok(());
    }
    let transcript = extract::video::transcript(&described);
    if transcript.is_empty() {
        bail!("{model} could not describe any of the {total} frames taken from {name}");
    }

    let mut note = format!(
        "video read by {model} · {} of {total} frames described",
        described.len()
    );
    let mut text = transcript.clone();

    if config.video_narrate {
        let _ = tx.send(Update::Status(format!(
            "Asking {narrator} to write the description…"
        )));
        let request =
            extract::video::narration_request(&config.video_narration_prompt, &transcript);
        // The narration is a rewrite of text the app already has. If it fails,
        // or comes back as a stub, the frame-by-frame account is still a full
        // description of the video — so this never costs the user the job.
        match ollama::narrate(&narrator, &request) {
            Ok(narration) if extract::video::narration_is_usable(&narration.text, &transcript) => {
                text = extract::tidy(&narration.text);
                note = format!(
                    "video read by {model} from {} of {total} frames, described by {narrator}",
                    described.len()
                );
            }
            Ok(_) => {
                let _ = tx.send(Update::Log(format!(
                    "{narrator} did not return a usable description; \
                     falling back to the frame-by-frame account"
                )));
                note.push_str(" · frame by frame");
            }
            Err(error) => {
                let _ = tx.send(Update::Log(format!(
                    "{narrator} could not write the description ({error:#}); \
                     falling back to the frame-by-frame account"
                )));
                note.push_str(" · frame by frame");
            }
        }
    }

    let _ = tx.send(Update::Progress(1.0));
    let _ = tx.send(Update::TextReady { text, note });
    Ok(())
}

fn install_homebrew(tx: &Sender<Update>) -> Result<()> {
    let log = tx.clone();
    crate::homebrew::install(move |line| {
        let _ = log.send(Update::Log(line));
    })?;
    let _ = tx.send(Update::SetupComplete("Homebrew is installed.".into()));
    Ok(())
}

fn install_ollama(tx: &Sender<Update>) -> Result<()> {
    if ollama::install_command().is_none() {
        bail!(
            "Ollama cannot be installed automatically on this computer. \
             Download it from https://ollama.com/download instead."
        );
    }
    let log = tx.clone();
    ollama::install(move |line| {
        let _ = log.send(Update::Log(line));
    })?;

    let _ = tx.send(Update::Status("Starting Ollama…".into()));
    ollama::ensure_running()?;
    let _ = tx.send(Update::SetupComplete("Ollama is ready.".into()));
    Ok(())
}

fn install_ffmpeg(tx: &Sender<Update>) -> Result<()> {
    if crate::ffmpeg::install_command().is_none() {
        bail!(
            "ffmpeg cannot be installed automatically on this computer. \
             Download it from {} instead.",
            crate::ffmpeg::DOWNLOAD_URL
        );
    }
    let log = tx.clone();
    crate::ffmpeg::install(move |line| {
        let _ = log.send(Update::Log(line));
    })?;
    let _ = tx.send(Update::SetupComplete("ffmpeg is ready.".into()));
    Ok(())
}

fn pull_model(model: String, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    ollama::ensure_running()?;
    let progress = tx.clone();
    let mut last_status = String::new();
    ollama::pull_model(&model, cancel, move |status, fraction| {
        if status != last_status {
            let _ = progress.send(Update::Log(status.clone()));
            last_status = status.clone();
        }
        let _ = progress.send(Update::Status(status));
        if let Some(fraction) = fraction {
            let _ = progress.send(Update::Progress(fraction));
        }
    })?;
    let _ = tx.send(Update::SetupComplete(format!("{model} is ready.")));
    Ok(())
}

/// Turns ElevenLabs' per-part progress into a bar and a status line.
///
/// A document over the API's per-request limit is sent as several requests and
/// the audio joined back together. That is invisible in the finished file, but
/// it is very visible in how long the job takes, so the status says which part
/// it is waiting on rather than sitting on "Synthesising speech…" for minutes.
/// `scale` is how much of the bar the synthesis owns, leaving the rest for
/// whatever the caller does afterwards.
fn report_parts(tx: &Sender<Update>, done: usize, total: usize, scale: f32) {
    if total > 1 {
        // `done` counts finished parts, so the one being waited on is the next.
        let current = (done + 1).min(total);
        let _ = tx.send(Update::Status(format!(
            "Synthesising speech — part {current} of {total}…"
        )));
    }
    let fraction = if total == 0 {
        0.0
    } else {
        done as f32 / total as f32
    };
    let _ = tx.send(Update::Progress(fraction * scale));
}

fn synthesize(engine: Engine, text: String, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    match engine {
        Engine::ElevenLabs {
            api_key,
            voice_id,
            model_id,
        } => {
            let progress = tx.clone();
            let mp3 = tts::elevenlabs::synthesize(
                &api_key,
                &voice_id,
                &model_id,
                &text,
                cancel,
                move |done, total| report_parts(&progress, done, total, 1.0),
            )?;
            let _ = tx.send(Update::Mp3Ready(Arc::new(mp3)));
            Ok(())
        }
        // The UI speaks system voices directly, so this should never be queued.
        Engine::System { .. } => bail!("system voices do not need a synthesis step"),
    }
}

fn save(
    engine: Engine,
    text: String,
    path: PathBuf,
    format: AudioFormat,
    cached_mp3: Option<Arc<Vec<u8>>>,
    tx: &Sender<Update>,
    cancel: &Cancel,
) -> Result<()> {
    match engine {
        Engine::ElevenLabs {
            api_key,
            voice_id,
            model_id,
        } => {
            let mp3 = match cached_mp3 {
                Some(cached) => {
                    let _ = tx.send(Update::Status("Reusing the audio just played…".into()));
                    cached
                }
                None => {
                    let progress = tx.clone();
                    Arc::new(tts::elevenlabs::synthesize(
                        &api_key,
                        &voice_id,
                        &model_id,
                        &text,
                        cancel,
                        // Leave the last slice of the bar for the file write.
                        move |done, total| report_parts(&progress, done, total, 0.9),
                    )?)
                }
            };
            if cancelled(cancel) {
                return Ok(());
            }

            let _ = tx.send(Update::Status("Writing the file…".into()));
            match format {
                // Already MP3; no point decoding and re-encoding it.
                AudioFormat::Mp3 => std::fs::write(&path, mp3.as_slice())
                    .with_context(|| format!("could not write {}", path.display()))?,
                AudioFormat::Wav => {
                    let pcm = audio::decode_mp3(&mp3)?;
                    audio::write_wav(&path, &pcm)?;
                }
            }
        }
        Engine::System { voice, rate } => {
            let _ = tx.send(Update::Progress(0.1));
            match format {
                AudioFormat::Wav => tts::system::write_wav(&text, &voice, rate, &path)?,
                AudioFormat::Mp3 => {
                    // `say` cannot write MP3, so render WAV alongside the
                    // destination and encode from that.
                    let temp = path.with_extension("soe-tmp.wav");
                    let result = (|| -> Result<()> {
                        tts::system::write_wav(&text, &voice, rate, &temp)?;
                        let _ = tx.send(Update::Progress(0.7));
                        let pcm = audio::read_wav(&temp)?;
                        audio::save(&path, &pcm, AudioFormat::Mp3)
                    })();
                    let _ = std::fs::remove_file(&temp);
                    result?;
                }
            }
        }
    }

    let _ = tx.send(Update::Progress(1.0));
    let _ = tx.send(Update::Saved(path));
    Ok(())
}

/// Resolves which engine to speak with from settings alone.
///
/// The free-standing version of what the GUI works out interactively — see
/// `SpeechApp::active_engine`/`job_engine` in `app.rs`, which also has to
/// decide *what to do about it* when the choice can't be honoured (open the
/// API key dialog, grey out a button). This just needs a reason, for
/// [`speak_to_file`] and anything else with no UI to fall back on.
pub fn resolve_engine(config: &Config, api_key: Option<&str>) -> Result<Engine> {
    match config.engine {
        crate::config::EnginePreference::ElevenLabs => {
            let api_key = api_key
                .map(str::to_string)
                .context("ElevenLabs is selected but no API key is saved")?;
            Ok(Engine::ElevenLabs {
                api_key,
                voice_id: config.elevenlabs_voice_id.clone(),
                model_id: config.elevenlabs_model_id.clone(),
            })
        }
        crate::config::EnginePreference::System => {
            if !tts::system::SUPPORTED {
                bail!("{}", tts::system::UNSUPPORTED_MESSAGE);
            }
            Ok(Engine::System {
                voice: config.system_voice.clone(),
                rate: config.system_rate,
            })
        }
    }
}

/// `path`, or the first `path (2)`, `path (3)`, … that doesn't already exist.
///
/// So running [`speak_to_file`] twice on the same source never overwrites the
/// audio from the first run.
fn unique_path(path: &std::path::Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path.extension().map(|e| e.to_string_lossy().to_string());
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new(""));

    (2..)
        .map(|n| {
            let name = match &extension {
                Some(extension) => format!("{stem} ({n}).{extension}"),
                None => format!("{stem} ({n})"),
            };
            parent.join(name)
        })
        .find(|candidate| !candidate.exists())
        .expect("an unbounded count of names always finds one that doesn't exist")
}

/// Reads `path` and writes the speech next to it, using whatever settings are
/// already saved. The pipeline behind the Windows right-click "Speak to file"
/// entry — see [`crate::context_menu`] — which has no window to drive the
/// interactive Read pane through, so this drives the same pieces directly.
///
/// Limited to the file kinds that need no setup of their own. `Image` and
/// `Video` go through Ollama/ffmpeg, can run for minutes, and may need to
/// prompt about installing something first — none of which a headless run
/// can do, so those are refused here with a message pointing back at the GUI.
pub fn speak_to_file(
    path: &std::path::Path,
    config: &Config,
    api_key: Option<&str>,
) -> Result<PathBuf> {
    let name = || path.file_name().unwrap_or_default().to_string_lossy();
    match FileKind::from_path(path) {
        Some(FileKind::Text) | Some(FileKind::Docx) | Some(FileKind::Pdf) | Some(FileKind::Csv) => {
        }
        Some(FileKind::Image) | Some(FileKind::Video) => bail!(
            "{} needs Ollama to read, which can take a while and may need setup — open it in \
             accessengine instead of using the right-click menu",
            name()
        ),
        None => bail!("{} is not a file type accessengine can read", name()),
    }

    let extracted = extract::extract_document(path, config.formatting)?;
    if extracted.text.trim().is_empty() {
        bail!("{} contains no readable text", name());
    }
    // Nothing here has a window to report into, so the log is the whole of it —
    // which the reader has already written to. Saving still goes ahead: a
    // mostly-good recording is what was asked for, and refusing it would be a
    // worse answer than an imperfect one.
    let (text, _) = crate::dictionary::apply(&extracted.text, &config.dictionary);

    let engine = resolve_engine(config, api_key)?;
    let format = config.save_format;
    let destination = unique_path(&path.with_extension(format.extension()));

    let (tx, rx) = std::sync::mpsc::channel();
    let cancel: Cancel = Arc::new(AtomicBool::new(false));
    save(
        engine,
        text,
        destination.clone(),
        format,
        None,
        &tx,
        &cancel,
    )?;
    drop(tx);
    for _ in rx {}

    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn system_engine() -> Engine {
        Engine::System {
            // Empty voice means the machine's own default, so this works on any
            // Mac regardless of which voices are installed.
            voice: String::new(),
            rate: 250,
        }
    }

    /// Runs a save job to completion and returns the updates it emitted.
    ///
    /// Carries the same `cfg` as the only tests that call it: both drive the
    /// system voice through a real save, which this suite does on macOS only.
    /// Without it the helper is dead code on Windows, which `-D warnings`
    /// rightly refuses to build.
    #[cfg(target_os = "macos")]
    fn run_save(path: &std::path::Path, format: AudioFormat) -> Vec<Update> {
        let (tx, rx) = channel();
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        save(
            system_engine(),
            "Saving a short line of speech.".to_string(),
            path.to_path_buf(),
            format,
            None,
            &tx,
            &cancel,
        )
        .expect("the save job should succeed");
        drop(tx);
        rx.into_iter().collect()
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn saving_with_a_system_voice_writes_a_playable_wav() {
        let path = std::env::temp_dir().join("soe-job-save-test.wav");
        let updates = run_save(&path, AudioFormat::Wav);

        let pcm = audio::read_wav(&path).expect("the saved WAV should be readable");
        std::fs::remove_file(&path).ok();

        assert!(!pcm.samples.is_empty());
        assert!(matches!(updates.last(), Some(Update::Saved(_))));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn saving_as_mp3_encodes_and_removes_its_temporary_wav() {
        let path = std::env::temp_dir().join("soe-job-save-test.mp3");
        let temp = path.with_extension("soe-tmp.wav");
        run_save(&path, AudioFormat::Mp3);

        let bytes = std::fs::read(&path).expect("the saved MP3 should exist");
        std::fs::remove_file(&path).ok();

        assert!(
            bytes.len() > 1_000,
            "suspiciously small MP3: {}",
            bytes.len()
        );
        // The MP3 must decode, and the scratch WAV must not be left behind.
        assert!(audio::decode_mp3(&bytes).is_ok());
        assert!(!temp.exists(), "the temporary WAV was not cleaned up");
    }

    /// A rejected key is thrown away and asked for again, so this test is the
    /// difference between "your key is wrong" and "your wifi is off" — and it
    /// has to survive the context a failure picks up on its way up, or a key
    /// turned down mid-document would be kept.
    #[test]
    fn only_a_rejected_key_reads_as_a_rejected_key() {
        use anyhow::Context as _;

        let rejected: anyhow::Error = tts::elevenlabs::KeyRejected.into();
        assert!(is_key_rejection(&rejected));

        let wrapped = Err::<(), _>(rejected)
            .context("reading part 3 of 5")
            .unwrap_err();
        assert!(is_key_rejection(&wrapped));

        // Every other failure leaves the key alone, however it is worded.
        let offline = anyhow::anyhow!("could not reach ElevenLabs");
        assert!(!is_key_rejection(&offline));
        let lookalike = anyhow::anyhow!("ElevenLabs rejected that API key");
        assert!(!is_key_rejection(&lookalike));
    }

    #[test]
    fn a_synthesis_job_rejects_the_system_engine() {
        let (tx, _rx) = channel();
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        // System voices are spoken directly by the UI, so queueing them here is
        // a programming error rather than something to silently ignore.
        assert!(synthesize(system_engine(), "hello".into(), &tx, &cancel).is_err());
    }

    #[test]
    fn unique_path_leaves_a_free_name_alone() {
        let path = std::env::temp_dir().join("soe-unique-path-test-does-not-exist.wav");
        assert_eq!(unique_path(&path), path);
    }

    /// The bug this exists to prevent: a second "Speak to file" on the same
    /// source overwriting the first result instead of sitting beside it.
    #[test]
    fn unique_path_numbers_around_existing_files() {
        let dir = std::env::temp_dir().join("soe-unique-path-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.wav");
        std::fs::write(&path, b"one").unwrap();
        std::fs::write(dir.join("clip (2).wav"), b"two").unwrap();

        let next = unique_path(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(next, dir.join("clip (3).wav"));
    }

    #[test]
    fn unique_path_numbers_a_file_with_no_extension() {
        let dir = std::env::temp_dir().join("soe-unique-path-test-no-ext");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip");
        std::fs::write(&path, b"one").unwrap();

        let next = unique_path(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(next, dir.join("clip (2)"));
    }

    #[test]
    fn resolve_engine_picks_system_voices_when_that_is_what_is_saved() {
        let config = Config {
            engine: crate::config::EnginePreference::System,
            system_voice: "Some Voice".to_string(),
            system_rate: 200,
            ..Default::default()
        };
        match resolve_engine(&config, None) {
            Ok(Engine::System { voice, rate }) => {
                assert_eq!(voice, "Some Voice");
                assert_eq!(rate, 200);
            }
            other => panic!("expected a system engine, got {}", describe(&other)),
        }
    }

    #[test]
    fn resolve_engine_needs_a_key_for_elevenlabs() {
        let config = Config {
            engine: crate::config::EnginePreference::ElevenLabs,
            ..Default::default()
        };
        // No key on hand, same as a headless run with none saved — this must
        // fail with a reason rather than build a request with an empty key.
        assert!(resolve_engine(&config, None).is_err());

        match resolve_engine(&config, Some("sk-test")) {
            Ok(Engine::ElevenLabs { api_key, .. }) => assert_eq!(api_key, "sk-test"),
            other => panic!("expected an ElevenLabs engine, got {}", describe(&other)),
        }
    }

    fn describe(result: &Result<Engine>) -> String {
        match result {
            Ok(Engine::System { .. }) => "a system engine".to_string(),
            Ok(Engine::ElevenLabs { .. }) => "an ElevenLabs engine".to_string(),
            Err(error) => format!("an error ({error:#})"),
        }
    }

    /// The headless pipeline end to end: a real text file, read, spoken with a
    /// system voice, and written next to the source.
    ///
    /// Ignored by default — it speaks for real, which is slow and, on a CI
    /// runner with no audio stack configured, not guaranteed to work. Run it
    /// with:
    ///     cargo test speak_to_file_writes -- --ignored --nocapture
    #[test]
    #[ignore = "needs a working system voice"]
    fn speak_to_file_writes_audio_next_to_the_source() {
        if !tts::system::SUPPORTED {
            eprintln!("no system voice on this platform; skipping");
            return;
        }
        let dir = std::env::temp_dir().join("soe-speak-to-file-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("note.txt");
        std::fs::write(&source, "A short note, read aloud and saved.").unwrap();

        let config = Config {
            engine: crate::config::EnginePreference::System,
            save_format: AudioFormat::Wav,
            ..Default::default()
        };
        let result = speak_to_file(&source, &config, None);

        let saved = result.expect("the headless pipeline should succeed");
        assert_eq!(saved, dir.join("note.wav"));
        let pcm = audio::read_wav(&saved).expect("the saved WAV should be readable");
        assert!(!pcm.samples.is_empty());

        // Run again on the same source: the first result must survive.
        let second = speak_to_file(&source, &config, None).expect("a second run should succeed");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(second, dir.join("note (2).wav"));
    }

    /// The whole video path, for real: ffmpeg takes the frames, the vision
    /// model describes each one, and a model writes them up.
    ///
    /// Ignored by default, and not because it is unimportant — it is the only
    /// test that runs the feature as the user does. It is ignored because it
    /// needs Ollama with a vision model pulled, and because vision inference on
    /// a laptop takes minutes, which is not a cost to put on every `cargo test`.
    ///
    /// Run it with:
    ///     cargo test read_a_real_video -- --ignored --nocapture
    #[test]
    #[ignore = "needs ffmpeg, a running Ollama and minutes of inference"]
    fn read_a_real_video_end_to_end() {
        let Some(binary) = crate::ffmpeg::binary_path() else {
            eprintln!("no ffmpeg; skipping");
            return;
        };
        // `main` does this before the first HTTPS request; a test has no main.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = std::env::temp_dir().join("soe-video-job-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");

        // Two distinguishable shots, so the transcript should have two entries
        // that do not read identically.
        let made = std::process::Command::new(&binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x480:rate=10:duration=2",
                "-f",
                "lavfi",
                "-i",
                "smptebars=size=640x480:rate=10:duration=2",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0[out]",
                "-map",
                "[out]",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg should run");
        assert!(made.success());

        // Both ways out: the narration, and the frame-by-frame account it
        // falls back to. The second is not a lesser path — it is what every
        // user with a model too small to narrate will actually hear.
        for narrate in [true, false] {
            let (tx, rx) = channel();
            let cancel: Cancel = Arc::new(AtomicBool::new(false));
            let config = Config {
                video_max_frames: 2,
                video_narrate: narrate,
                ..Default::default()
            };
            let result = read_video(video.clone(), &config, &tx, &cancel);
            drop(tx);

            let updates: Vec<Update> = rx.into_iter().collect();
            result.expect("the video should be readable");

            for update in &updates {
                if let Update::Log(line) = update {
                    eprintln!("log: {line}");
                }
            }
            let progress: Vec<f32> = updates
                .iter()
                .filter_map(|u| match u {
                    Update::Progress(fraction) => Some(*fraction),
                    _ => None,
                })
                .collect();
            assert!(
                progress.windows(2).all(|pair| pair[1] >= pair[0]),
                "the progress bar went backwards: {progress:?}"
            );
            assert_eq!(progress.last(), Some(&1.0), "the bar never reached the end");

            let ready = updates.iter().find_map(|u| match u {
                Update::TextReady { text, note } => Some((text.clone(), note.clone())),
                _ => None,
            });
            let (text, note) = ready.expect("the job should have produced text");
            eprintln!("\n=== narrate: {narrate} ===\nnote: {note}\n\n{text}\n");
            assert!(
                text.chars().count() > 80,
                "the description is too short to be one: {text}"
            );
            // Spoken output: the prompts forbid Markdown, and this is where
            // that is actually put to the test rather than asserted about the
            // prompt.
            assert!(!text.contains("**"), "markdown reached the speech: {text}");
            assert!(
                !text.contains("```"),
                "a code fence reached the speech: {text}"
            );

            // The unnarrated form is the transcript, which must carry both
            // frames and say when each of them happens.
            if !narrate {
                assert!(text.contains("At the start"), "no opening frame: {text}");
                assert!(
                    text.contains(" in: "),
                    "nothing labelled with a time: {text}"
                );
            }
        }

        // The frames are temporary, and nothing may outlive the job that made
        // them — including when it fails partway through.
        let leftovers: Vec<PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temporary directory should be readable")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with("accessengine-frames-"))
            })
            .collect();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            leftovers.is_empty(),
            "extracted frames were left behind: {leftovers:?}"
        );
    }
}

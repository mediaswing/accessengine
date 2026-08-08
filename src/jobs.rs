//! Background work and the messages it sends back to the UI.
//!
//! Every slow thing — network calls, model downloads, `brew install`, file
//! rendering — runs as a [`Job`] on its own thread and reports progress through
//! an [`Update`] channel that the UI drains each frame. Nothing here touches
//! egui, and nothing in the UI blocks on any of it.

use crate::audio::{self, AudioFormat};
use crate::config::Config;
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
    ReadDocument(PathBuf),
    /// Read an image, arranging Ollama first if it needs arranging.
    ReadImage { path: PathBuf, config: Box<Config> },
    /// Runs Homebrew's own installer, answering the password it asks for with
    /// a macOS dialog.
    InstallHomebrew,
    /// `brew install ollama`.
    InstallOllama,
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
            Self::ReadDocument(path) => format!(
                "Reading {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::ReadImage { path, .. } => format!(
                "Looking at {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::InstallHomebrew => "Installing Homebrew…".to_string(),
            Self::InstallOllama => "Installing Ollama…".to_string(),
            Self::PullModel(model) => format!("Downloading {model}…"),
            Self::LoadElevenLabsVoices(_) => "Loading ElevenLabs voices…".to_string(),
            Self::Synthesize { .. } => "Synthesising speech…".to_string(),
            Self::Save { path, .. } => format!(
                "Saving {}…",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }

    /// Whether the Cancel button should appear. Interrupting a Homebrew or
    /// Ollama install halfway would leave a mess, so neither is cancellable.
    pub fn is_cancellable(&self) -> bool {
        !matches!(self, Self::InstallHomebrew | Self::InstallOllama)
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
    /// MP3 for playback.
    Mp3Ready(Arc<Vec<u8>>),
    Saved(PathBuf),
    /// The image needs Ollama and it isn't installed.
    NeedsOllamaInstall,
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

/// Runs `job` on a new thread. The thread always sends [`Update::Finished`],
/// so the UI can clear its busy state in exactly one place.
pub fn spawn(job: Job, tx: Sender<Update>, cancel: Cancel, repaint: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(error) = run(job, &tx, &cancel) {
            // A cancellation is the user's own doing, not something to report.
            if !cancelled(&cancel) {
                let _ = tx.send(Update::Error(format!("{error:#}")));
            }
        }
        let _ = tx.send(Update::Finished);
        repaint();
    });
}

fn run(job: Job, tx: &Sender<Update>, cancel: &Cancel) -> Result<()> {
    match job {
        Job::ReadDocument(path) => read_document(path, tx),
        Job::ReadImage { path, config } => read_image(path, &config, tx, cancel),
        Job::InstallHomebrew => install_homebrew(tx),
        Job::InstallOllama => install_ollama(tx),
        Job::PullModel(model) => pull_model(model, tx, cancel),
        Job::LoadElevenLabsVoices(key) => {
            // Reported on its own channel so a failure is attached to the
            // voice picker instead of becoming a generic error.
            match tts::elevenlabs::list_voices(&key) {
                Ok(voices) => {
                    let _ = tx.send(Update::ElevenLabsVoices(voices));
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

fn read_document(path: PathBuf, tx: &Sender<Update>) -> Result<()> {
    let text = extract::extract_document(&path)?;
    if text.trim().is_empty() {
        bail!(
            "{} contains no readable text",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    let kind = FileKind::from_path(&path)
        .map(FileKind::label)
        .unwrap_or("file");
    let note = format!("{} · {} characters", kind, text.chars().count());
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

    #[test]
    fn a_synthesis_job_rejects_the_system_engine() {
        let (tx, _rx) = channel();
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        // System voices are spoken directly by the UI, so queueing them here is
        // a programming error rather than something to silently ignore.
        assert!(synthesize(system_engine(), "hello".into(), &tx, &cancel).is_err());
    }
}

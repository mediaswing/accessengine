//! ElevenLabs cloud voices.
//!
//! Runs on a worker thread: every sentence is an HTTP round trip, which must
//! never touch the UI thread. The worker synthesises one sentence ahead of the
//! one it is playing, so the gap between sentences is playback-bound rather
//! than network-bound.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::Cursor;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const API_ROOT: &str = "https://api.elevenlabs.io/v1";
const OUTPUT_FORMAT: &str = "mp3_44100_128";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// Models worth offering in the UI. The field is free text, so a new model id
/// can be typed in without waiting for an update.
pub const MODELS: &[(&str, &str)] = &[
    ("eleven_multilingual_v2", "Multilingual v2 — best quality"),
    ("eleven_turbo_v2_5", "Turbo v2.5 — faster, cheaper"),
    ("eleven_flash_v2_5", "Flash v2.5 — lowest latency"),
    ("eleven_v3", "v3 — most expressive"),
];

#[derive(Clone, Debug)]
pub struct RemoteVoice {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Everything the worker needs to synthesise; gathered on the UI thread so the
/// worker never reads shared state.
#[derive(Clone)]
pub struct VoiceRequest {
    pub api_key: String,
    pub voice_id: String,
    pub model: String,
    pub stability: f32,
    pub similarity: f32,
}

/// Hand-written so the key cannot reach the log file. The log is the first
/// thing a user attaches to a bug report, and a derived `Debug` would put a
/// live credential in it the moment anyone adds a `{request:?}`.
impl std::fmt::Debug for VoiceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceRequest")
            .field("api_key", &redacted(&self.api_key))
            .field("voice_id", &self.voice_id)
            .field("model", &self.model)
            .field("stability", &self.stability)
            .field("similarity", &self.similarity)
            .finish()
    }
}

/// Describe a secret without revealing it.
pub(crate) fn redacted(secret: &str) -> String {
    if secret.is_empty() {
        "<unset>".to_string()
    } else {
        format!("<{} chars redacted>", secret.len())
    }
}

pub enum Command {
    Play {
        texts: Vec<String>,
        start: usize,
        request: VoiceRequest,
        gain: f32,
        /// A voice sample rather than the document. Progress events are
        /// suppressed for these, so testing a voice cannot be mistaken for
        /// the document playing.
        preview: bool,
    },
    Pause,
    Resume,
    Stop,
    SetGain(f32),
    FetchVoices {
        api_key: String,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    /// Index into the plan that just started playing.
    Started(usize),
    /// Waiting on the network for the given plan index.
    Synthesising(usize),
    /// The plan ran to the end.
    Finished,
    /// Playback was cancelled.
    Stopped,
    Error(String),
    Voices(Vec<RemoteVoice>),
}

pub struct ElevenLabs {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
}

impl ElevenLabs {
    /// Spawn the worker. `repaint` wakes the UI when an event is posted.
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let worker = std::thread::Builder::new()
            .name("elevenlabs".to_string())
            .spawn(move || worker_main(cmd_rx, evt_tx, repaint))
            .ok();
        if worker.is_none() {
            log::error!("could not spawn the ElevenLabs worker thread");
        }
        Self {
            cmd_tx,
            evt_rx,
            worker,
        }
    }

    pub fn send(&self, cmd: Command) {
        if self.cmd_tx.send(cmd).is_err() {
            log::error!("ElevenLabs worker is gone; command dropped");
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }
}

impl Drop for ElevenLabs {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("AccessEngine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

fn worker_main(cmd_rx: Receiver<Command>, evt_tx: Sender<Event>, repaint: impl Fn()) {
    let http = match client() {
        Ok(c) => c,
        Err(e) => {
            let _ = evt_tx.send(Event::Error(format!("{e:#}")));
            repaint();
            return;
        }
    };

    // The audio device is opened on first use, not at startup: a user who
    // never touches ElevenLabs should not have this app holding their sound
    // card open.
    let mut audio: Option<AudioOut> = None;

    let emit = |e: Event| {
        let _ = evt_tx.send(e);
        repaint();
    };

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Shutdown => break,
            Command::FetchVoices { api_key } => match fetch_voices(&http, &api_key) {
                Ok(voices) => {
                    log::info!("fetched {} ElevenLabs voices", voices.len());
                    emit(Event::Voices(voices));
                }
                Err(e) => {
                    log::error!("fetching ElevenLabs voices: {e:#}");
                    emit(Event::Error(format!("{e:#}")));
                }
            },
            Command::Play {
                texts,
                start,
                request,
                gain,
                preview,
            } => {
                if audio.is_none() {
                    match AudioOut::open() {
                        Ok(a) => audio = Some(a),
                        Err(e) => {
                            log::error!("opening audio output: {e:#}");
                            emit(Event::Error(format!("{e:#}")));
                            continue;
                        }
                    }
                }
                let Some(out) = audio.as_ref() else {
                    continue;
                };
                // Each superseding Play is handled by looping here, so a held
                // arrow key cannot nest playback inside itself.
                let mut pending = (texts, start, request, gain, preview);
                let mut stop_worker = false;
                loop {
                    let (texts, start, request, gain, preview) = pending;
                    let outcome =
                        play_plan(&http, out, &cmd_rx, &emit, texts, start, request, gain, preview);
                    match outcome {
                        Outcome::Completed => {
                            if !preview {
                                emit(Event::Finished);
                            }
                        }
                        Outcome::Cancelled => emit(Event::Stopped),
                        Outcome::Shutdown => stop_worker = true,
                        Outcome::Restart(next) => {
                            if let Command::Play {
                                texts,
                                start,
                                request,
                                gain,
                                preview,
                            } = *next
                            {
                                pending = (texts, start, request, gain, preview);
                                continue;
                            }
                        }
                        Outcome::Failed(e) => {
                            log::error!("ElevenLabs playback: {e:#}");
                            emit(Event::Error(format!("{e:#}")));
                        }
                    }
                    break;
                }
                if stop_worker {
                    break;
                }
            }
            // Only meaningful while a plan is running, where they are handled
            // inside the playback loop.
            Command::Pause | Command::Resume | Command::Stop | Command::SetGain(_) => {}
        }
    }
    log::info!("ElevenLabs worker stopped");
}

/// Owns the output device for as long as the worker lives.
struct AudioOut {
    _device: rodio::MixerDeviceSink,
}

impl AudioOut {
    fn open() -> Result<Self> {
        let device = rodio::DeviceSinkBuilder::open_default_sink()
            .context("opening the default audio output device")?;
        Ok(Self { _device: device })
    }

    fn player(&self) -> rodio::Player {
        rodio::Player::connect_new(self._device.mixer())
    }
}

enum Outcome {
    Completed,
    Cancelled,
    Shutdown,
    /// A fresh `Play` arrived mid-playback. Handed back to the worker loop
    /// rather than recursed into: skipping repeats it once per key press, and
    /// recursion would grow the stack, and retain every superseded document,
    /// until playback finally ended.
    Restart(Box<Command>),
    Failed(anyhow::Error),
}

#[allow(clippy::too_many_arguments)]
fn play_plan(
    http: &reqwest::blocking::Client,
    audio: &AudioOut,
    cmd_rx: &Receiver<Command>,
    emit: &impl Fn(Event),
    texts: Vec<String>,
    start: usize,
    request: VoiceRequest,
    mut gain: f32,
    preview: bool,
) -> Outcome {
    // Audio for the next sentence, fetched while the current one plays.
    let mut prefetch: Option<(usize, JoinHandle<Result<Vec<u8>>>)> = None;
    let mut index = start;

    while index < texts.len() {
        // Take the prefetched bytes if they are for this sentence, else fetch.
        let audio_bytes = match prefetch.take() {
            Some((i, handle)) if i == index => match handle.join() {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(e)) => return Outcome::Failed(e),
                Err(_) => return Outcome::Failed(anyhow!("synthesis thread panicked")),
            },
            other => {
                // A stale prefetch (after a skip) is simply abandoned.
                drop(other);
                if !preview {
                    emit(Event::Synthesising(index));
                }
                match synthesise(http, &request, &texts[index]) {
                    Ok(bytes) => bytes,
                    Err(e) => return Outcome::Failed(e),
                }
            }
        };

        // Start the next request before playing this one.
        if index + 1 < texts.len() {
            let http = http.clone();
            let request = request.clone();
            let text = texts[index + 1].clone();
            let next = index + 1;
            prefetch = std::thread::Builder::new()
                .name("elevenlabs-prefetch".to_string())
                .spawn(move || synthesise(&http, &request, &text))
                .ok()
                .map(|h| (next, h));
        }

        let decoder = match rodio::Decoder::try_from(Cursor::new(audio_bytes)) {
            Ok(d) => d,
            Err(e) => return Outcome::Failed(anyhow!("decoding returned audio: {e}")),
        };

        let player = audio.player();
        player.set_volume(gain.clamp(0.0, 1.0));
        player.append(decoder);
        player.play();
        if !preview {
            emit(Event::Started(index));
        }

        // Wait for this sentence to finish, staying responsive to commands.
        let began = Instant::now();
        let mut heard_audio = false;
        loop {
            if !player.empty() {
                heard_audio = true;
            } else if heard_audio || began.elapsed() > Duration::from_millis(750) {
                // Either it played out, or it never started and we should not
                // hang the whole document waiting for it.
                break;
            }

            match cmd_rx.try_recv() {
                Ok(Command::Stop) => {
                    player.stop();
                    return Outcome::Cancelled;
                }
                Ok(Command::Shutdown) => {
                    player.stop();
                    return Outcome::Shutdown;
                }
                Ok(Command::Pause) => player.pause(),
                Ok(Command::Resume) => player.play(),
                Ok(Command::SetGain(g)) => {
                    gain = g;
                    player.set_volume(g.clamp(0.0, 1.0));
                }
                // A fresh Play supersedes this one; requeue it for the outer
                // loop by returning, so settings and plan are picked up whole.
                Ok(cmd @ Command::Play { .. }) => {
                    player.stop();
                    return Outcome::Restart(Box::new(cmd));
                }
                Ok(Command::FetchVoices { .. }) => {}
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    player.stop();
                    return Outcome::Shutdown;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        index += 1;
    }
    Outcome::Completed
}

/// One sentence of MP3. Also used by [`crate::export`], which writes the same
/// bytes to a file instead of playing them.
pub(crate) fn synthesise(
    http: &reqwest::blocking::Client,
    request: &VoiceRequest,
    text: &str,
) -> Result<Vec<u8>> {
    if request.api_key.is_empty() {
        bail!("no ElevenLabs API key set");
    }
    if request.voice_id.is_empty() {
        bail!("no ElevenLabs voice selected");
    }
    // The voice id is interpolated into a URL path. It normally arrives from
    // the API, but the config file is hand-editable, so do not let a stray `?`
    // or `/` reshape the request.
    if !is_safe_voice_id(&request.voice_id) {
        bail!("the selected ElevenLabs voice id is not a valid id");
    }

    let url = format!(
        "{API_ROOT}/text-to-speech/{}?output_format={OUTPUT_FORMAT}",
        request.voice_id
    );
    let body = serde_json::json!({
        "text": text,
        "model_id": request.model,
        "voice_settings": {
            "stability": request.stability,
            "similarity_boost": request.similarity,
        }
    });

    let response = http
        .post(&url)
        .header("xi-api-key", &request.api_key)
        .header("accept", "audio/mpeg")
        .json(&body)
        .send()
        .context("contacting ElevenLabs")?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        bail!("{}", describe_api_error(status, &detail));
    }

    let bytes = response.bytes().context("reading audio from ElevenLabs")?;
    if bytes.is_empty() {
        bail!("ElevenLabs returned no audio");
    }
    Ok(bytes.to_vec())
}

/// ElevenLabs voice ids are opaque alphanumeric strings; anything else has no
/// business being pasted into a URL path.
fn is_safe_voice_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn fetch_voices(http: &reqwest::blocking::Client, api_key: &str) -> Result<Vec<RemoteVoice>> {
    if api_key.is_empty() {
        bail!("enter an API key first");
    }

    #[derive(Deserialize)]
    struct VoicesResponse {
        voices: Vec<ApiVoice>,
    }
    #[derive(Deserialize)]
    struct ApiVoice {
        voice_id: String,
        name: Option<String>,
        category: Option<String>,
        #[serde(default)]
        labels: std::collections::HashMap<String, String>,
    }

    let response = http
        .get(format!("{API_ROOT}/voices"))
        .header("xi-api-key", api_key)
        .send()
        .context("contacting ElevenLabs")?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        bail!("{}", describe_api_error(status, &detail));
    }

    let parsed: VoicesResponse = response.json().context("reading the voice list")?;
    Ok(parsed
        .voices
        .into_iter()
        .map(|v| {
            // Labels vary by voice; accent and description are the two that
            // actually help someone choose.
            let mut parts: Vec<String> = Vec::new();
            for key in ["accent", "description", "age", "gender", "use_case"] {
                if let Some(value) = v.labels.get(key) {
                    if !value.is_empty() {
                        parts.push(value.clone());
                    }
                }
            }
            if parts.is_empty() {
                if let Some(c) = v.category {
                    parts.push(c);
                }
            }
            RemoteVoice {
                name: v.name.unwrap_or_else(|| v.voice_id.clone()),
                id: v.voice_id,
                description: parts.join(", "),
            }
        })
        .collect())
}

/// The opening words of the message for a key ElevenLabs will not accept.
/// The UI matches on it to bring back the button for entering a new one, so
/// the two have to agree; keeping it here is what makes that so.
pub const KEY_REJECTED: &str = "ElevenLabs rejected the API key.";

/// Whether a message reaching the UI means the key is no good.
pub fn is_key_rejection(message: &str) -> bool {
    message.contains(KEY_REJECTED)
}

/// Turn an API failure into something a user can act on.
fn describe_api_error(status: reqwest::StatusCode, body: &str) -> String {
    let message = extract_detail(body);
    match status.as_u16() {
        401 => format!("{KEY_REJECTED} Enter another on the General tab."),
        402 => format!(
            "ElevenLabs quota exhausted{}",
            suffix(&message)
        ),
        404 => "That ElevenLabs voice no longer exists. Pick another.".to_string(),
        422 => format!("ElevenLabs rejected the request{}", suffix(&message)),
        429 => "ElevenLabs rate limit hit. Wait a moment and try again.".to_string(),
        500..=599 => format!("ElevenLabs is having trouble (HTTP {status}){}", suffix(&message)),
        _ => format!("ElevenLabs returned HTTP {status}{}", suffix(&message)),
    }
}

fn suffix(message: &Option<String>) -> String {
    match message {
        Some(m) => format!(": {m}"),
        None => String::new(),
    }
}

/// ElevenLabs puts the useful text in `detail`, which is sometimes a string and
/// sometimes an object with a `message`.
fn extract_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let detail = value.get("detail")?;
    if let Some(s) = detail.as_str() {
        return Some(s.to_string());
    }
    detail
        .get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_is_read_from_both_shapes() {
        assert_eq!(
            extract_detail(r#"{"detail":"plain string"}"#).as_deref(),
            Some("plain string")
        );
        assert_eq!(
            extract_detail(r#"{"detail":{"status":"x","message":"nested"}}"#).as_deref(),
            Some("nested")
        );
        assert_eq!(extract_detail("not json"), None);
    }

    #[test]
    fn voice_ids_that_could_reshape_a_url_are_rejected() {
        assert!(is_safe_voice_id("21m00Tcm4TlvDq8ikWAM"));
        assert!(is_safe_voice_id("my-voice_1"));
        assert!(!is_safe_voice_id(""));
        assert!(!is_safe_voice_id("../../v1/history"));
        assert!(!is_safe_voice_id("abc?output_format=evil"));
        assert!(!is_safe_voice_id("abc/def"));
        assert!(!is_safe_voice_id(&"a".repeat(65)));
    }

    /// The log is the first thing attached to a bug report; a derived `Debug`
    /// here would put a live credential in it.
    #[test]
    fn debug_output_never_contains_the_key() {
        let request = VoiceRequest {
            api_key: "sk_super_secret_value".to_string(),
            voice_id: "abc123".to_string(),
            model: "eleven_multilingual_v2".to_string(),
            stability: 0.5,
            similarity: 0.75,
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("sk_super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        // The harmless fields should still be there, or the impl is useless.
        assert!(rendered.contains("abc123"), "{rendered}");
    }

    #[test]
    fn auth_failure_names_the_fix() {
        let msg = describe_api_error(reqwest::StatusCode::UNAUTHORIZED, "");
        assert!(msg.contains("API key"), "{msg}");
    }

    #[test]
    fn unknown_status_still_carries_the_detail() {
        let msg = describe_api_error(
            reqwest::StatusCode::IM_A_TEAPOT,
            r#"{"detail":"short and stout"}"#,
        );
        assert!(msg.contains("short and stout"), "{msg}");
    }
}

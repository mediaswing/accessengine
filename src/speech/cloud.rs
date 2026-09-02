//! The hosted voices, and the one worker thread they all run on.
//!
//! Every sentence is an HTTP round trip, which must never touch the UI thread.
//! The worker synthesises one sentence ahead of the one it is playing, so the
//! gap between sentences is playback-bound rather than network-bound.
//!
//! This module is everything that is the same whichever provider is chosen:
//! the command and event channels, the audio device, the prefetch, and what
//! happens when a fresh `Play` arrives mid-sentence. What is not the same —
//! the URL, the body, the credential, the shape of the voice list — lives in
//! the provider's own module behind [`VoiceRequest`].
//!
//! It began as the ElevenLabs engine and still behaves exactly as it did: the
//! playback loop below is that code, moved rather than rewritten.
//!
//! ## Credentials
//!
//! Nothing in here reads a credential from anywhere. A [`VoiceRequest`] is
//! assembled on the UI thread from [`crate::config::Config`], which is the one
//! place that knows where a key comes from, and every provider's request type
//! has a hand-written `Debug` that redacts it — because the log is the first
//! thing attached to a bug report, and a derived one would put a live
//! credential in it the moment anybody added a `{request:?}`.

use anyhow::{anyhow, Context, Result};
use std::io::Cursor;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{deepgram, elevenlabs, google, openai, polly, EngineKind};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// One voice offered by a provider.
///
/// `id` is whatever that provider needs back — an opaque ElevenLabs id, a
/// Google voice name, a Deepgram model — and `name` is what a person reads.
/// The two are kept apart so a provider renaming its voices cannot change what
/// gets requested, and so a saved selection survives a voice list that has not
/// been fetched yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteVoice {
    pub id: String,
    pub name: String,
    pub description: String,
    /// BCP-47 where the provider says so, otherwise empty. Google needs it
    /// back at synthesis time; everyone else uses it only to be searched.
    pub language: String,
    /// Which synthesis engines this voice can be spoken by. Polly's, and empty
    /// for every other provider — none of the rest offer the choice.
    pub engines: Vec<String>,
}

impl RemoteVoice {
    /// The name with whatever is known about the voice after it, for a list
    /// somebody is choosing from.
    pub fn label(&self) -> String {
        if self.description.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, self.description)
        }
    }

    /// Whether a search box's words appear anywhere in this voice.
    pub fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || self.name.to_lowercase().contains(needle)
            || self.language.to_lowercase().contains(needle)
            || self.description.to_lowercase().contains(needle)
    }
}

/// Everything the worker needs to synthesise, gathered on the UI thread so the
/// worker never reads shared state.
///
/// One variant per provider rather than one struct with every provider's
/// fields in it: the settings genuinely differ — Polly wants a region and an
/// engine, Google a language code, ElevenLabs two sliders — and a struct
/// holding all of them at once would be mostly empty whichever engine was
/// chosen, with nothing to say which parts meant anything.
#[derive(Clone, Debug)]
pub enum VoiceRequest {
    ElevenLabs(elevenlabs::Request),
    OpenAi(openai::Request),
    Deepgram(deepgram::Request),
    Google(google::Request),
    Polly(polly::Request),
}

impl VoiceRequest {
    pub fn engine(&self) -> EngineKind {
        match self {
            Self::ElevenLabs(_) => EngineKind::ElevenLabs,
            Self::OpenAi(_) => EngineKind::OpenAi,
            Self::Deepgram(_) => EngineKind::Deepgram,
            Self::Google(_) => EngineKind::Google,
            Self::Polly(_) => EngineKind::Polly,
        }
    }

    /// One chunk of MP3. Also used by [`crate::export`], which writes the same
    /// bytes to a file instead of playing them.
    ///
    /// MP3 from every provider, deliberately: it is what the app's decoder
    /// already reads, and asking for anything else would mean a media
    /// dependency for no audible gain.
    pub fn synthesise(&self, http: &reqwest::blocking::Client, text: &str) -> Result<Vec<u8>> {
        let bytes = match self {
            Self::ElevenLabs(r) => elevenlabs::synthesise(http, r, text),
            Self::OpenAi(r) => openai::synthesise(http, r, text),
            Self::Deepgram(r) => deepgram::synthesise(http, r, text),
            Self::Google(r) => google::synthesise(http, r, text),
            Self::Polly(r) => polly::synthesise(http, r, text),
        }?;
        if bytes.is_empty() {
            anyhow::bail!("{} returned no audio", self.engine().provider_name());
        }
        Ok(bytes)
    }

    /// The voices this account can use.
    pub fn fetch_voices(&self, http: &reqwest::blocking::Client) -> Result<Vec<RemoteVoice>> {
        match self {
            Self::ElevenLabs(r) => elevenlabs::fetch_voices(http, r),
            Self::OpenAi(r) => openai::fetch_voices(http, r),
            Self::Deepgram(r) => deepgram::fetch_voices(http, r),
            Self::Google(r) => google::fetch_voices(http, r),
            Self::Polly(r) => polly::fetch_voices(http, r),
        }
    }
}

/// Describe a secret without revealing it.
pub fn redacted(secret: &str) -> String {
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
        request: Box<VoiceRequest>,
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
    /// A voice list, and which provider answered with it — the app may have
    /// been switched to another engine while the request was in flight, and a
    /// list of the wrong provider's voices is worse than none.
    Voices {
        engine: EngineKind,
        voices: Vec<RemoteVoice>,
    },
}

pub struct CloudEngine {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
}

impl CloudEngine {
    /// Spawn the worker. `repaint` wakes the UI when an event is posted.
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let worker = std::thread::Builder::new()
            .name("cloud-speech".to_string())
            .spawn(move || worker_main(cmd_rx, evt_tx, repaint))
            .ok();
        if worker.is_none() {
            log::error!("could not spawn the cloud speech worker thread");
        }
        Self {
            cmd_tx,
            evt_rx,
            worker,
        }
    }

    pub fn send(&self, cmd: Command) {
        if self.cmd_tx.send(cmd).is_err() {
            log::error!("cloud speech worker is gone; command dropped");
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }
}

impl Drop for CloudEngine {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn client() -> Result<reqwest::blocking::Client> {
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
    // never touches a cloud voice should not have this app holding their sound
    // card open.
    let mut audio: Option<AudioOut> = None;

    let emit = |e: Event| {
        let _ = evt_tx.send(e);
        repaint();
    };

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Shutdown => break,
            Command::FetchVoices { request } => {
                let engine = request.engine();
                match request.fetch_voices(&http) {
                    Ok(voices) => {
                        log::info!("fetched {} {} voices", voices.len(), engine.provider_name());
                        emit(Event::Voices { engine, voices });
                    }
                    Err(e) => {
                        log::error!("fetching {} voices: {e:#}", engine.provider_name());
                        emit(Event::Error(format!("{e:#}")));
                    }
                }
            }
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
                    let outcome = play_plan(
                        &http, out, &cmd_rx, &emit, texts, start, request, gain, preview,
                    );
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
                            log::error!("cloud playback: {e:#}");
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
    log::info!("cloud speech worker stopped");
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
                match request.synthesise(http, &texts[index]) {
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
                .name("cloud-speech-prefetch".to_string())
                .spawn(move || request.synthesise(&http, &text))
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

/// The words every provider's "that key is no good" message has in common.
///
/// The UI matches on it to bring back the button for entering a new one, so
/// the two have to agree; keeping the phrase here, and building every such
/// message from [`describe_api_error`], is what makes that so.
pub const KEY_REJECTED: &str = "rejected the API key.";

/// Whether a message reaching the UI means the credential is no good.
pub fn is_key_rejection(message: &str) -> bool {
    message.contains(KEY_REJECTED)
}

/// Turn an API failure into something a user can act on.
///
/// Deliberately not a bare status code. "HTTP 429" tells somebody trying to
/// read a document nothing they can do; "wait a moment and try again" does.
///
/// `body` is the provider's own response text. Only the human-readable part of
/// it is quoted back — never headers, and never the request — so an error on
/// screen cannot carry a credential with it.
pub fn describe_api_error(provider: &str, status: reqwest::StatusCode, body: &str) -> String {
    let message = extract_detail(body);
    match status.as_u16() {
        401 => format!("{provider} {KEY_REJECTED} Enter another on the General tab."),
        400 | 403 if is_credential_complaint(body) => {
            format!("{provider} {KEY_REJECTED} Enter another on the General tab.")
        }
        403 => format!(
            "{provider} refused the request. The credentials may be right but not \
             permitted to use text to speech{}",
            suffix(&message)
        ),
        402 => format!("{provider} quota exhausted{}", suffix(&message)),
        404 => format!("That {provider} voice no longer exists. Pick another."),
        422 => format!("{provider} rejected the request{}", suffix(&message)),
        429 => format!("{provider} rate limit hit. Wait a moment and try again."),
        500..=599 => format!(
            "{provider} is having trouble (HTTP {status}){}",
            suffix(&message)
        ),
        _ => format!("{provider} returned HTTP {status}{}", suffix(&message)),
    }
}

/// Whether an error body is the provider objecting to the credential itself,
/// rather than to what that credential is allowed to do.
///
/// Only ElevenLabs, OpenAI and Deepgram answer a bad key with a plain 401. AWS
/// signs every request, so a wrong key is a *signature* failure — a 403 naming
/// `UnrecognizedClientException` or `InvalidSignatureException` — and Google
/// answers `400 API key not valid`. Without this the UI reads both as "the
/// credential is fine but not permitted", never offers the button for entering
/// another, and one mistyped character can only be undone by hand-editing
/// `config.json`.
///
/// Matched against the whole body rather than the extracted sentence because
/// AWS puts the useful part in `__type`, which is not the human-readable
/// `Message` that [`extract_detail`] picks out.
fn is_credential_complaint(body: &str) -> bool {
    const MARKERS: &[&str] = &[
        // AWS, from the SigV4 error table.
        "UnrecognizedClientException",
        "InvalidSignatureException",
        "InvalidClientTokenId",
        "SignatureDoesNotMatch",
        "IncompleteSignature",
        "MissingAuthenticationToken",
        "InvalidAccessKeyId",
        "ExpiredToken",
        // Google.
        "API_KEY_INVALID",
        "API key not valid",
    ];
    MARKERS.iter().any(|marker| body.contains(marker))
}

fn suffix(message: &Option<String>) -> String {
    match message {
        Some(m) => format!(": {m}"),
        None => String::new(),
    }
}

/// The useful sentence out of an error body, wherever this provider puts it.
///
/// Five services, five spellings of the same idea: ElevenLabs uses `detail`
/// (sometimes a string, sometimes an object with a `message`), OpenAI and
/// Google use `error.message`, Deepgram `err_msg`, and AWS `Message`. Trying
/// each in turn is shorter than five parsers and means a provider that changes
/// its mind still has somewhere to land.
fn extract_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;

    let candidates: [Option<&serde_json::Value>; 6] = [
        value.get("detail"),
        value.get("error").and_then(|e| e.get("message")),
        value.get("err_msg"),
        value.get("message"),
        value.get("Message"),
        value.get("error"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(text) = candidate.as_str() {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
        if let Some(text) = candidate.get("message").and_then(|m| m.as_str()) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// Read an error response's body without letting a huge or binary one through.
///
/// A provider having a bad day can answer a synthesis request with a whole
/// HTML error page, and that would otherwise be pasted into the status line.
pub fn error_body(response: reqwest::blocking::Response) -> String {
    let text = response.text().unwrap_or_default();
    if text.len() > 2000 {
        text.chars().take(2000).collect()
    } else {
        text
    }
}

/// Whether an id is safe to interpolate into a URL path or query string.
///
/// Voice ids and model names normally arrive from the provider, but the config
/// file is hand-editable, so do not let a stray `?` or `/` reshape a request.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_useful_sentence_is_found_wherever_a_provider_puts_it() {
        // ElevenLabs, both shapes.
        assert_eq!(
            extract_detail(r#"{"detail":"plain string"}"#).as_deref(),
            Some("plain string")
        );
        assert_eq!(
            extract_detail(r#"{"detail":{"status":"x","message":"nested"}}"#).as_deref(),
            Some("nested")
        );
        // OpenAI and Google.
        assert_eq!(
            extract_detail(r#"{"error":{"message":"Incorrect API key provided"}}"#).as_deref(),
            Some("Incorrect API key provided")
        );
        // Deepgram.
        assert_eq!(
            extract_detail(r#"{"err_code":"X","err_msg":"model not found"}"#).as_deref(),
            Some("model not found")
        );
        // AWS.
        assert_eq!(
            extract_detail(r#"{"Message":"The voice is not available"}"#).as_deref(),
            Some("The voice is not available")
        );
        assert_eq!(extract_detail("not json"), None);
        assert_eq!(extract_detail(r#"{"detail":""}"#), None);
    }

    /// The exact wording ElevenLabs produced before the other providers were
    /// added. Existing users read these messages; they should not change.
    #[test]
    fn the_elevenlabs_messages_are_word_for_word_what_they_were() {
        let describe = |code: u16, body: &str| {
            describe_api_error(
                "ElevenLabs",
                reqwest::StatusCode::from_u16(code).unwrap(),
                body,
            )
        };
        assert_eq!(
            describe(401, ""),
            "ElevenLabs rejected the API key. Enter another on the General tab."
        );
        assert_eq!(describe(402, ""), "ElevenLabs quota exhausted");
        assert_eq!(
            describe(404, ""),
            "That ElevenLabs voice no longer exists. Pick another."
        );
        assert_eq!(
            describe(422, r#"{"detail":"too long"}"#),
            "ElevenLabs rejected the request: too long"
        );
        assert_eq!(
            describe(429, ""),
            "ElevenLabs rate limit hit. Wait a moment and try again."
        );
    }

    /// The UI brings back the "enter a key" button by matching on the message,
    /// so every provider's version of it has to be recognisable.
    #[test]
    fn a_refused_key_is_recognised_whichever_provider_said_so() {
        for engine in EngineKind::ALL {
            let message = describe_api_error(
                engine.provider_name(),
                reqwest::StatusCode::UNAUTHORIZED,
                "",
            );
            assert!(is_key_rejection(&message), "{message}");
            assert!(message.contains("API key"), "{message}");
        }
        assert!(!is_key_rejection("ElevenLabs rate limit hit."));
    }

    /// The two providers that never send a 401 for a bad credential. Without
    /// this, saving one wrong character leaves the UI with no way back to the
    /// dialog and `config.json` the only way out.
    #[test]
    fn a_signature_or_key_complaint_counts_however_it_arrives() {
        let aws = describe_api_error(
            "Amazon Polly",
            reqwest::StatusCode::FORBIDDEN,
            r#"{"__type":"UnrecognizedClientException","Message":"The security token included in the request is invalid."}"#,
        );
        assert!(is_key_rejection(&aws), "{aws}");

        let signature = describe_api_error(
            "Amazon Polly",
            reqwest::StatusCode::FORBIDDEN,
            r#"{"__type":"InvalidSignatureException","Message":"The request signature we calculated does not match."}"#,
        );
        assert!(is_key_rejection(&signature), "{signature}");

        let google = describe_api_error(
            "Google Cloud",
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"status":"INVALID_ARGUMENT","message":"API key not valid. Please pass a valid API key."}}"#,
        );
        assert!(is_key_rejection(&google), "{google}");
    }

    /// A credential that is real but not allowed to do this is a different
    /// problem with a different answer, and must not be read as a bad key.
    #[test]
    fn a_permission_refusal_is_not_a_refused_key() {
        let denied = describe_api_error(
            "Amazon Polly",
            reqwest::StatusCode::FORBIDDEN,
            r#"{"__type":"AccessDeniedException","Message":"User is not authorized to perform polly:SynthesizeSpeech"}"#,
        );
        assert!(!is_key_rejection(&denied), "{denied}");
        assert!(denied.contains("not permitted"), "{denied}");
    }

    #[test]
    fn unknown_status_still_carries_the_detail() {
        let msg = describe_api_error(
            "ElevenLabs",
            reqwest::StatusCode::IM_A_TEAPOT,
            r#"{"detail":"short and stout"}"#,
        );
        assert!(msg.contains("short and stout"), "{msg}");
    }

    #[test]
    fn ids_that_could_reshape_a_url_are_rejected() {
        assert!(is_safe_id("21m00Tcm4TlvDq8ikWAM"));
        assert!(is_safe_id("aura-2-thalia-en"));
        assert!(is_safe_id("en-GB-Neural2-A"));
        assert!(is_safe_id("gpt-4o-mini-tts"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../../v1/history"));
        assert!(!is_safe_id("abc?output_format=evil"));
        assert!(!is_safe_id("abc/def"));
        assert!(!is_safe_id(&"a".repeat(97)));
    }

    #[test]
    fn a_voice_is_searched_by_everything_shown_about_it() {
        let voice = RemoteVoice {
            id: "en-GB-Neural2-A".to_string(),
            name: "Amelia".to_string(),
            description: "British English, female".to_string(),
            language: "en-GB".to_string(),
            engines: Vec::new(),
        };
        assert!(voice.matches(""));
        assert!(voice.matches("amelia"));
        assert!(voice.matches("en-gb"));
        assert!(voice.matches("female"));
        assert!(!voice.matches("welsh"));
        assert_eq!(voice.label(), "Amelia — British English, female");
    }
}

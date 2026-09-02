//! OpenAI text to speech.
//!
//! One POST to `/v1/audio/speech` per chunk, asking for MP3 — the same shape
//! as every other provider here, so the worker in [`super::cloud`] needs to
//! know nothing about it.
//!
//! ## Why the voices are a list in this file
//!
//! OpenAI publishes no endpoint that lists them: the voices are a fixed,
//! documented set and the API rejects anything outside it. So "fetch my
//! voices" hands back the list below — but only after asking `/v1/models`,
//! which is what actually answers the question the user is really pressing
//! that button to ask, namely whether the key works. The alternative, a menu
//! that fills in without a single request having been made, would leave a bad
//! key to be discovered halfway through a document.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::cloud::{describe_api_error, error_body, redacted, RemoteVoice};
use super::EngineKind;

const API_ROOT: &str = "https://api.openai.com/v1";

/// The models that can speak, newest first. Free text in the UI, as
/// ElevenLabs's is, so a model announced tomorrow can be typed in today.
pub const MODELS: &[(&str, &str)] = &[
    ("gpt-4o-mini-tts", "GPT-4o mini TTS — steerable, newest"),
    ("tts-1", "TTS-1 — lowest latency"),
    ("tts-1-hd", "TTS-1 HD — higher quality, slower"),
];

pub const DEFAULT_MODEL: &str = "gpt-4o-mini-tts";

/// The published voices, with the words OpenAI's own documentation uses about
/// them. Names only — there is nothing else to say about a voice you cannot
/// listen to first, which is what the Test button on Settings is for.
pub const VOICES: &[(&str, &str)] = &[
    ("alloy", "Neutral, even"),
    ("ash", "Warm, conversational"),
    ("ballad", "Gentle, expressive"),
    ("coral", "Bright, friendly"),
    ("echo", "Calm, measured"),
    ("fable", "Storytelling, British"),
    ("nova", "Light, energetic"),
    ("onyx", "Deep, authoritative"),
    ("sage", "Soft, thoughtful"),
    ("shimmer", "Clear, upbeat"),
    ("verse", "Varied, dramatic"),
];

pub const DEFAULT_VOICE: &str = "alloy";

/// The slowest and fastest the API will accept. 1.0 is the voice as recorded.
pub const SPEED_RANGE: (f32, f32) = (0.25, 4.0);

pub const SIGN_UP_URL: &str = "https://platform.openai.com/signup";
pub const KEYS_URL: &str = "https://platform.openai.com/api-keys";

#[derive(Clone)]
pub struct Request {
    pub api_key: String,
    pub voice_id: String,
    pub model: String,
    /// 0.25 to 4.0, where 1.0 is the voice unaltered.
    pub speed: f32,
    /// Free text describing how to read — "speak slowly and clearly, as if
    /// reading to a child". Honoured by `gpt-4o-mini-tts` and quietly ignored
    /// by the older models, which is why it is sent only when it says
    /// something.
    pub instructions: String,
}

/// Hand-written so the key cannot reach the log file — see the note in
/// [`super::cloud`].
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("openai::Request")
            .field("api_key", &redacted(&self.api_key))
            .field("voice_id", &self.voice_id)
            .field("model", &self.model)
            .field("speed", &self.speed)
            .finish()
    }
}

fn provider() -> &'static str {
    EngineKind::OpenAi.provider_name()
}

/// The JSON body for one chunk.
///
/// Split from the request that carries it so the shape can be checked without
/// a key or a network: the parts worth getting wrong are which fields are sent
/// at all, and whether the speed has been brought into the accepted range.
fn body(request: &Request, text: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": request.model,
        "input": text,
        "voice": request.voice_id,
        "response_format": "mp3",
        "speed": request.speed.clamp(SPEED_RANGE.0, SPEED_RANGE.1),
    });
    let instructions = request.instructions.trim();
    if !instructions.is_empty() {
        body["instructions"] = serde_json::Value::String(instructions.to_string());
    }
    body
}

pub fn synthesise(
    http: &reqwest::blocking::Client,
    request: &Request,
    text: &str,
) -> Result<Vec<u8>> {
    if request.api_key.is_empty() {
        bail!("no OpenAI API key set");
    }
    if request.voice_id.is_empty() {
        bail!("no OpenAI voice selected");
    }

    let response = http
        .post(format!("{API_ROOT}/audio/speech"))
        .bearer_auth(&request.api_key)
        .json(&body(request, text))
        .send()
        .context("contacting OpenAI")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let bytes = response.bytes().context("reading audio from OpenAI")?;
    Ok(bytes.to_vec())
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

/// The built-in voice list, after checking the key is one OpenAI accepts.
pub fn fetch_voices(
    http: &reqwest::blocking::Client,
    request: &Request,
) -> Result<Vec<RemoteVoice>> {
    if request.api_key.is_empty() {
        bail!("enter an API key first");
    }

    let response = http
        .get(format!("{API_ROOT}/models"))
        .bearer_auth(&request.api_key)
        .send()
        .context("contacting OpenAI")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    // Only for the log: the answer to "why does this key not work" is often
    // that the account has no access to the speech models at all.
    match response.json::<ModelsResponse>() {
        Ok(models) => {
            let speech = speech_models(&models.data);
            log::info!("OpenAI key accepted; speech models available: {speech:?}");
        }
        Err(e) => log::warn!("OpenAI model list could not be read: {e}"),
    }

    Ok(built_in_voices())
}

fn speech_models(models: &[Model]) -> Vec<&str> {
    models
        .iter()
        .map(|m| m.id.as_str())
        .filter(|id| id.contains("tts") || id.contains("audio"))
        .collect()
}

pub fn built_in_voices() -> Vec<RemoteVoice> {
    VOICES
        .iter()
        .map(|(id, description)| RemoteVoice {
            id: (*id).to_string(),
            // The API takes the lower-case id; a person reading a menu should
            // see a name.
            name: capitalise(id),
            description: (*description).to_string(),
            language: String::new(),
            engines: Vec::new(),
        })
        .collect()
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            api_key: "sk-super_secret_value".to_string(),
            voice_id: DEFAULT_VOICE.to_string(),
            model: DEFAULT_MODEL.to_string(),
            speed: 1.0,
            instructions: String::new(),
        }
    }

    #[test]
    fn the_body_asks_for_mp3_so_the_existing_decoder_can_read_it() {
        let body = body(&request(), "Hello.");
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["input"], "Hello.");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        // Nothing to say about delivery, so the field is left out rather than
        // sent empty — the older models reject fields they do not know.
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn instructions_are_sent_only_when_there_are_some() {
        let mut request = request();
        request.instructions = "  Read slowly.  ".to_string();
        assert_eq!(body(&request, "x")["instructions"], "Read slowly.");
    }

    /// A hand-edited settings file must not be able to send a speed the API
    /// will refuse — which would fail every sentence of the document rather
    /// than reading it slightly wrong.
    #[test]
    fn an_impossible_speed_is_brought_into_range() {
        let mut request = request();
        request.speed = 99.0;
        assert_eq!(body(&request, "x")["speed"], 4.0);
        request.speed = -1.0;
        assert_eq!(body(&request, "x")["speed"], 0.25);
    }

    #[test]
    fn the_built_in_voices_read_as_names_rather_than_ids() {
        let voices = built_in_voices();
        assert_eq!(voices.len(), VOICES.len());
        assert_eq!(voices[0].id, "alloy");
        assert_eq!(voices[0].name, "Alloy");
        assert!(!voices[0].description.is_empty());
    }

    #[test]
    fn the_model_list_is_narrowed_to_the_ones_that_speak() {
        let models: Vec<Model> = ["gpt-4o", "tts-1", "gpt-4o-mini-tts", "dall-e-3"]
            .iter()
            .map(|id| Model { id: id.to_string() })
            .collect();
        assert_eq!(speech_models(&models), vec!["tts-1", "gpt-4o-mini-tts"]);
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let rendered = format!("{:?}", request());
        assert!(!rendered.contains("sk-super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains("alloy"), "{rendered}");
    }
}

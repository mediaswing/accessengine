//! Deepgram Aura.
//!
//! Deepgram makes no distinction between a voice and a model: `aura-2-thalia-en`
//! *is* the voice, and it is passed as the `model` query parameter. So the
//! voice picker on the General tab is the whole of the choice, and there is no
//! separate model dropdown on Settings — the one thing this app must not do is
//! invent a second control that means the same as the first.
//!
//! The voice list is fetched from `/v1/models`, which reports what the account
//! can actually use, rather than being a list baked in here that would go
//! stale the week Deepgram adds a voice.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::cloud::{describe_api_error, error_body, is_safe_id, redacted, RemoteVoice};
use super::EngineKind;

const API_ROOT: &str = "https://api.deepgram.com/v1";

/// Asked for by name, because Deepgram's default is linear16 in a raw
/// container. MP3 is what the rest of this app already decodes.
const ENCODING: &str = "mp3";

/// The voice used when nothing has been chosen and nothing fetched: Deepgram's
/// own default, so a first play works before the voice list has been asked for.
pub const DEFAULT_VOICE: &str = "aura-2-thalia-en";

pub const SIGN_UP_URL: &str = "https://console.deepgram.com/signup";
pub const KEYS_URL: &str = "https://console.deepgram.com/project/_/settings/api-keys";

#[derive(Clone)]
pub struct Request {
    pub api_key: String,
    /// The Aura model, which is also the voice.
    pub voice_id: String,
}

/// Hand-written so the key cannot reach the log file — see the note in
/// [`super::cloud`].
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("deepgram::Request")
            .field("api_key", &redacted(&self.api_key))
            .field("voice_id", &self.voice_id)
            .finish()
    }
}

fn provider() -> &'static str {
    EngineKind::Deepgram.provider_name()
}

/// Deepgram authenticates with `Token <key>` rather than a bearer token.
fn authorisation(api_key: &str) -> String {
    format!("Token {api_key}")
}

pub fn synthesise(
    http: &reqwest::blocking::Client,
    request: &Request,
    text: &str,
) -> Result<Vec<u8>> {
    if request.api_key.is_empty() {
        bail!("no Deepgram API key set");
    }
    if request.voice_id.is_empty() {
        bail!("no Deepgram voice selected");
    }
    // The model goes into the query string, and the settings file is
    // hand-editable: do not let a stray `&` add a parameter of its own.
    if !is_safe_id(&request.voice_id) {
        bail!("the selected Deepgram voice is not a valid model name");
    }

    let url = format!(
        "{API_ROOT}/speak?model={}&encoding={ENCODING}",
        request.voice_id
    );
    let response = http
        .post(&url)
        .header("Authorization", authorisation(&request.api_key))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .context("contacting Deepgram")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let bytes = response.bytes().context("reading audio from Deepgram")?;
    Ok(bytes.to_vec())
}

/// The shape of `/v1/models`, cut down to what this app reads. `stt` is
/// deliberately not deserialised: this is a reader, and a speech-to-text model
/// in the voice menu would be a bug wearing a name.
#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    tts: Vec<ApiVoice>,
}

#[derive(Deserialize)]
struct ApiVoice {
    /// What the API wants back — `aura-2-thalia-en`.
    canonical_name: Option<String>,
    /// What a person calls it — "Thalia".
    name: Option<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Deserialize, Default)]
struct Metadata {
    accent: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

pub fn fetch_voices(
    http: &reqwest::blocking::Client,
    request: &Request,
) -> Result<Vec<RemoteVoice>> {
    if request.api_key.is_empty() {
        bail!("enter an API key first");
    }

    let response = http
        .get(format!("{API_ROOT}/models"))
        .header("Authorization", authorisation(&request.api_key))
        .send()
        .context("contacting Deepgram")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let parsed: ModelsResponse = response.json().context("reading the voice list")?;
    Ok(voices_from(parsed.tts))
}

fn voices_from(list: Vec<ApiVoice>) -> Vec<RemoteVoice> {
    let mut voices: Vec<RemoteVoice> = list
        .into_iter()
        // A voice with no canonical name cannot be asked for, so it would be a
        // menu entry that fails when picked.
        .filter_map(|v| {
            let id = v.canonical_name?;
            let mut parts: Vec<String> = Vec::new();
            if let Some(accent) = v.metadata.accent.filter(|a| !a.is_empty()) {
                parts.push(accent);
            }
            parts.extend(v.metadata.tags.into_iter().filter(|t| !t.is_empty()));
            Some(RemoteVoice {
                name: v.name.clone().unwrap_or_else(|| id.clone()),
                language: v.languages.first().cloned().unwrap_or_default(),
                description: parts.join(", "),
                id,
                engines: Vec::new(),
            })
        })
        .collect();
    // Deepgram answers in no particular order, and the menu is long enough
    // that alphabetical is the difference between finding a voice and
    // scrolling for it.
    voices.sort_by_key(|v| v.name.to_lowercase());
    voices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cut-down recording of a real `/v1/models` reply. The `stt` half is
    /// left in deliberately: it must not appear in the voice menu.
    const MODELS_JSON: &str = r#"{
      "stt": [{ "name": "Nova-3", "canonical_name": "nova-3" }],
      "tts": [
        {
          "name": "Thalia",
          "canonical_name": "aura-2-thalia-en",
          "languages": ["en"],
          "metadata": { "accent": "American", "tags": ["clear", "confident"] }
        },
        {
          "name": "Angus",
          "canonical_name": "aura-angus-en",
          "languages": ["en-IE"],
          "metadata": { "accent": "Irish", "tags": [] }
        },
        { "name": "Nameless" }
      ]
    }"#;

    fn parse(json: &str) -> Vec<RemoteVoice> {
        let parsed: ModelsResponse = serde_json::from_str(json).expect("parses");
        voices_from(parsed.tts)
    }

    #[test]
    fn only_the_speaking_models_become_voices() {
        let voices = parse(MODELS_JSON);
        // Three entries in, one of them unusable, and no speech-to-text model.
        assert_eq!(voices.len(), 2);
        assert!(!voices.iter().any(|v| v.id.starts_with("nova")));
    }

    #[test]
    fn a_voice_carries_the_model_name_the_api_wants_back() {
        let voices = parse(MODELS_JSON);
        // Sorted, so Angus comes first.
        assert_eq!(voices[0].name, "Angus");
        assert_eq!(voices[0].id, "aura-angus-en");
        assert_eq!(voices[0].language, "en-IE");
        assert_eq!(voices[0].description, "Irish");

        assert_eq!(voices[1].name, "Thalia");
        assert_eq!(voices[1].id, "aura-2-thalia-en");
        assert_eq!(voices[1].description, "American, clear, confident");
    }

    /// Malformed or unexpected JSON should cost the voice list, not the app.
    #[test]
    fn a_reply_with_nothing_in_it_is_no_voices_rather_than_an_error() {
        assert!(parse("{}").is_empty());
        assert!(parse(r#"{"tts":[]}"#).is_empty());
    }

    #[test]
    fn the_header_is_deepgrams_own_scheme_not_a_bearer_token() {
        assert_eq!(authorisation("abc123"), "Token abc123");
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let request = Request {
            api_key: "super_secret_value".to_string(),
            voice_id: DEFAULT_VOICE.to_string(),
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains(DEFAULT_VOICE), "{rendered}");
    }
}

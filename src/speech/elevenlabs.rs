//! ElevenLabs cloud voices.
//!
//! The worker, the queue and the audio device all live in [`super::cloud`];
//! this is only what is particular to ElevenLabs — the two requests it makes,
//! and the settings they carry.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::cloud::{describe_api_error, error_body, is_safe_id, redacted, RemoteVoice};
use super::EngineKind;

const API_ROOT: &str = "https://api.elevenlabs.io/v1";
const OUTPUT_FORMAT: &str = "mp3_44100_128";

/// Models worth offering in the UI. The field is free text, so a new model id
/// can be typed in without waiting for an update.
pub const MODELS: &[(&str, &str)] = &[
    ("eleven_multilingual_v2", "Multilingual v2 — best quality"),
    ("eleven_turbo_v2_5", "Turbo v2.5 — faster, cheaper"),
    ("eleven_flash_v2_5", "Flash v2.5 — lowest latency"),
    ("eleven_v3", "v3 — most expressive"),
];

pub const DEFAULT_MODEL: &str = "eleven_multilingual_v2";

/// Where to sign up, and where the key is once you have.
pub const SIGN_UP_URL: &str = "https://elevenlabs.io/sign-up";
pub const KEYS_URL: &str = "https://elevenlabs.io/app/settings/api-keys";

#[derive(Clone)]
pub struct Request {
    pub api_key: String,
    pub voice_id: String,
    pub model: String,
    pub stability: f32,
    pub similarity: f32,
}

/// Hand-written so the key cannot reach the log file. The log is the first
/// thing a user attaches to a bug report, and a derived `Debug` would put a
/// live credential in it the moment anyone adds a `{request:?}`.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("elevenlabs::Request")
            .field("api_key", &redacted(&self.api_key))
            .field("voice_id", &self.voice_id)
            .field("model", &self.model)
            .field("stability", &self.stability)
            .field("similarity", &self.similarity)
            .finish()
    }
}

fn provider() -> &'static str {
    EngineKind::ElevenLabs.provider_name()
}

/// One chunk of MP3.
pub fn synthesise(
    http: &reqwest::blocking::Client,
    request: &Request,
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
    if !is_safe_id(&request.voice_id) {
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
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let bytes = response.bytes().context("reading audio from ElevenLabs")?;
    Ok(bytes.to_vec())
}

/// The fields of a voice this app actually reads. At module scope rather than
/// inside the request, so the mapping below can be tested against a recorded
/// response without a network call or anybody's key.
#[derive(Deserialize)]
struct ApiVoice {
    voice_id: String,
    name: Option<String>,
    category: Option<String>,
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct VoicesResponse {
    voices: Vec<ApiVoice>,
}

pub fn fetch_voices(
    http: &reqwest::blocking::Client,
    request: &Request,
) -> Result<Vec<RemoteVoice>> {
    if request.api_key.is_empty() {
        bail!("enter an API key first");
    }

    let response = http
        .get(format!("{API_ROOT}/voices"))
        .header("xi-api-key", &request.api_key)
        .send()
        .context("contacting ElevenLabs")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let parsed: VoicesResponse = response.json().context("reading the voice list")?;
    Ok(parsed.voices.into_iter().map(voice_from).collect())
}

fn voice_from(v: ApiVoice) -> RemoteVoice {
    // Labels vary by voice; accent and description are the two that actually
    // help someone choose.
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
        language: v.labels.get("language").cloned().unwrap_or_default(),
        id: v.voice_id,
        description: parts.join(", "),
        engines: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorded shape of a `/v1/voices` reply, cut down to the fields this
    /// app reads. A fixture rather than a live call: tests must never need
    /// somebody's key.
    const VOICES_JSON: &str = r#"{
      "voices": [
        {
          "voice_id": "21m00Tcm4TlvDq8ikWAM",
          "name": "Rachel",
          "category": "premade",
          "labels": {"accent": "American", "description": "calm", "gender": "female"}
        },
        {
          "voice_id": "AZnzlk1XvdvUeBnXmlld",
          "name": "Domi",
          "category": "premade",
          "labels": {}
        },
        { "voice_id": "bare", "category": "cloned", "labels": {} }
      ]
    }"#;

    fn parse(json: &str) -> Vec<RemoteVoice> {
        let parsed: VoicesResponse = serde_json::from_str(json).expect("parses");
        parsed.voices.into_iter().map(voice_from).collect()
    }

    #[test]
    fn the_voice_list_becomes_names_a_person_can_choose_between() {
        let voices = parse(VOICES_JSON);
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].id, "21m00Tcm4TlvDq8ikWAM");
        assert_eq!(voices[0].name, "Rachel");
        assert_eq!(voices[0].description, "American, calm, female");
        // No labels at all: the category is better than an empty line.
        assert_eq!(voices[1].description, "premade");
        // No name either: the id is at least something to point at, rather
        // than a blank row in the menu.
        assert_eq!(voices[2].name, "bare");
    }

    /// The log is the first thing attached to a bug report; a derived `Debug`
    /// here would put a live credential in it.
    #[test]
    fn debug_output_never_contains_the_key() {
        let request = Request {
            api_key: "sk_super_secret_value".to_string(),
            voice_id: "abc123".to_string(),
            model: DEFAULT_MODEL.to_string(),
            stability: 0.5,
            similarity: 0.75,
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("sk_super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        // The harmless fields should still be there, or the impl is useless.
        assert!(rendered.contains("abc123"), "{rendered}");
    }
}

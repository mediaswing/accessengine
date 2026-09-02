//! Google Cloud Text-to-Speech.
//!
//! ## Authentication, and why it is an API key
//!
//! Google's own preferred credential for this API is a *service account*: a
//! JSON file holding an RSA private key, which the client signs a JWT with and
//! exchanges for an access token that expires every hour. That is three things
//! this app does not otherwise have — an RSA signer, a token cache, and a
//! secret on disk that is materially worse to lose than an API key — and it
//! would be the largest dependency in the tree for one of five providers.
//!
//! So this uses the other mechanism Google supports on the same REST endpoints:
//! an **API key**, passed as `?key=`. It is a single string, like every other
//! provider here, which means the key dialog, the environment variable and the
//! "remember this on this computer" rule all work exactly as they do for the
//! others rather than needing a Google-shaped exception.
//!
//! What the user has to supply is therefore one thing, and the README says so:
//! a Cloud project with the Text-to-Speech API enabled, and an API key from it.
//! Restricting that key to the Text-to-Speech API alone is worth doing and is
//! also in the README — an unrestricted Cloud key is a key to the whole
//! project.
//!
//! Nothing is embedded in the binary. There is no client secret here, no
//! bundled project, and no credential of any kind in the source: everything
//! comes from the user's own settings or environment.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Deserialize;

use super::cloud::{describe_api_error, error_body, redacted, RemoteVoice};
use super::EngineKind;

const API_ROOT: &str = "https://texttospeech.googleapis.com/v1";

pub const SIGN_UP_URL: &str = "https://console.cloud.google.com/projectcreate";
pub const KEYS_URL: &str = "https://console.cloud.google.com/apis/credentials";

/// What the API accepts. 1.0 is the voice as recorded.
pub const RATE_RANGE: (f32, f32) = (0.25, 4.0);
/// Semitones either side of the voice's own pitch.
pub const PITCH_RANGE: (f32, f32) = (-20.0, 20.0);

#[derive(Clone)]
pub struct Request {
    pub api_key: String,
    /// A full Google voice name, such as `en-GB-Neural2-A`.
    pub voice_id: String,
    /// The BCP-47 code the voice belongs to. Saved alongside the voice when
    /// one is picked, because the API asks for both and will not infer it.
    pub language: String,
    pub speaking_rate: f32,
    pub pitch: f32,
}

/// Hand-written so the key cannot reach the log file — see the note in
/// [`super::cloud`].
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("google::Request")
            .field("api_key", &redacted(&self.api_key))
            .field("voice_id", &self.voice_id)
            .field("language", &self.language)
            .field("speaking_rate", &self.speaking_rate)
            .field("pitch", &self.pitch)
            .finish()
    }
}

fn provider() -> &'static str {
    EngineKind::Google.provider_name()
}

/// The language a voice belongs to, read off its own name.
///
/// Google names every voice `<language>-<variant>`: `en-GB-Neural2-A`,
/// `cmn-CN-Wavenet-A`. The saved `language` is what the picker stored when the
/// voice was chosen, but a settings file written by hand — or by a build
/// before this field existed — may not have one, and a request without a
/// language code is refused outright. Deriving it costs nothing and turns that
/// into a reading rather than an error.
pub fn language_of(voice_id: &str) -> String {
    let mut parts = voice_id.split('-');
    match (parts.next(), parts.next()) {
        (Some(language), Some(region))
            if !language.is_empty() && region.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            format!("{language}-{region}")
        }
        _ => String::new(),
    }
}

/// The JSON body for one chunk, split out so it can be checked without a key.
fn body(request: &Request, text: &str) -> serde_json::Value {
    let language = if request.language.trim().is_empty() {
        language_of(&request.voice_id)
    } else {
        request.language.trim().to_string()
    };
    serde_json::json!({
        "input": { "text": text },
        "voice": { "languageCode": language, "name": request.voice_id },
        "audioConfig": {
            "audioEncoding": "MP3",
            "speakingRate": request.speaking_rate.clamp(RATE_RANGE.0, RATE_RANGE.1),
            "pitch": request.pitch.clamp(PITCH_RANGE.0, PITCH_RANGE.1),
        }
    })
}

/// Google returns the audio as base64 inside a JSON object rather than as
/// bytes, so there is one decode between the response and the decoder.
#[derive(Deserialize)]
struct SynthesisResponse {
    #[serde(rename = "audioContent")]
    audio_content: Option<String>,
}

pub fn synthesise(
    http: &reqwest::blocking::Client,
    request: &Request,
    text: &str,
) -> Result<Vec<u8>> {
    if request.api_key.is_empty() {
        bail!("no Google Cloud API key set");
    }
    if request.voice_id.is_empty() {
        bail!("no Google Cloud voice selected");
    }

    let response = http
        .post(format!("{API_ROOT}/text:synthesize"))
        // As a header rather than in the query string: a URL is the part of a
        // request that ends up in logs and proxies, and this one is a
        // credential.
        .header("X-Goog-Api-Key", &request.api_key)
        .json(&body(request, text))
        .send()
        .context("contacting Google Cloud Text-to-Speech")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let parsed: SynthesisResponse = response
        .json()
        .context("reading audio from Google Cloud Text-to-Speech")?;
    let Some(encoded) = parsed.audio_content else {
        bail!("Google Cloud Text-to-Speech returned a reply with no audio in it");
    };
    decode_audio(&encoded)
}

fn decode_audio(encoded: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("the audio Google Cloud sent back could not be decoded")
}

#[derive(Deserialize)]
struct VoicesResponse {
    #[serde(default)]
    voices: Vec<ApiVoice>,
}

#[derive(Deserialize)]
struct ApiVoice {
    name: Option<String>,
    #[serde(rename = "languageCodes", default)]
    language_codes: Vec<String>,
    #[serde(rename = "ssmlGender")]
    ssml_gender: Option<String>,
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
        .header("X-Goog-Api-Key", &request.api_key)
        .send()
        .context("contacting Google Cloud Text-to-Speech")?;

    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let parsed: VoicesResponse = response.json().context("reading the voice list")?;
    Ok(voices_from(parsed.voices))
}

fn voices_from(list: Vec<ApiVoice>) -> Vec<RemoteVoice> {
    let mut voices: Vec<RemoteVoice> = list
        .into_iter()
        .filter_map(|v| {
            let id = v.name?;
            let language = v.language_codes.first().cloned().unwrap_or_default();
            // Google offers well over a thousand voices, so what is said about
            // one has to be the two things a person actually chooses on: which
            // language it speaks, and how it sounds.
            let mut parts = vec![];
            if !language.is_empty() {
                parts.push(language.clone());
            }
            if let Some(gender) = v
                .ssml_gender
                .filter(|g| g != "SSML_VOICE_GENDER_UNSPECIFIED")
            {
                parts.push(gender.to_lowercase());
            }
            if let Some(family) = voice_family(&id) {
                parts.push(family);
            }
            Some(RemoteVoice {
                name: id.clone(),
                id,
                description: parts.join(", "),
                language,
                engines: Vec::new(),
            })
        })
        .collect();
    voices.sort_by(|a, b| a.name.cmp(&b.name));
    voices
}

/// Which family a voice belongs to — Standard, WaveNet, Neural2, Chirp — read
/// off its name, since the API does not say. It is the single biggest
/// difference in how one sounds, and in what it costs.
fn voice_family(voice_id: &str) -> Option<String> {
    for family in [
        "Chirp3-HD",
        "Chirp-HD",
        "Chirp",
        "Journey",
        "Studio",
        "Polyglot",
        "Neural2",
        "News",
        "Wavenet",
        "Standard",
    ] {
        if voice_id.contains(family) {
            return Some(family.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOICES_JSON: &str = r#"{
      "voices": [
        {
          "languageCodes": ["en-GB"],
          "name": "en-GB-Neural2-A",
          "ssmlGender": "FEMALE",
          "naturalSampleRateHertz": 24000
        },
        {
          "languageCodes": ["cmn-CN"],
          "name": "cmn-CN-Wavenet-A",
          "ssmlGender": "SSML_VOICE_GENDER_UNSPECIFIED"
        },
        { "languageCodes": ["fr-FR"] }
      ]
    }"#;

    fn request() -> Request {
        Request {
            api_key: "AIza_super_secret_value".to_string(),
            voice_id: "en-GB-Neural2-A".to_string(),
            language: "en-GB".to_string(),
            speaking_rate: 1.0,
            pitch: 0.0,
        }
    }

    #[test]
    fn the_body_asks_for_mp3_and_names_both_voice_and_language() {
        let body = body(&request(), "Hello.");
        assert_eq!(body["audioConfig"]["audioEncoding"], "MP3");
        assert_eq!(body["voice"]["name"], "en-GB-Neural2-A");
        assert_eq!(body["voice"]["languageCode"], "en-GB");
        assert_eq!(body["input"]["text"], "Hello.");
    }

    /// A settings file with no saved language — hand-written, or written by a
    /// build before that field existed — must still produce a valid request.
    #[test]
    fn a_missing_language_is_read_off_the_voice_name() {
        let mut request = request();
        request.language = String::new();
        assert_eq!(body(&request, "x")["voice"]["languageCode"], "en-GB");

        assert_eq!(language_of("cmn-CN-Wavenet-A"), "cmn-CN");
        assert_eq!(language_of("en-GB-Chirp3-HD-Achernar"), "en-GB");
        assert_eq!(language_of("nonsense"), "");
        assert_eq!(language_of(""), "");
    }

    #[test]
    fn impossible_rates_and_pitches_are_brought_into_range() {
        let mut request = request();
        request.speaking_rate = 99.0;
        request.pitch = -400.0;
        let body = body(&request, "x");
        assert_eq!(body["audioConfig"]["speakingRate"], 4.0);
        assert_eq!(body["audioConfig"]["pitch"], -20.0);
    }

    #[test]
    fn the_voice_list_says_which_language_and_which_family() {
        let parsed: VoicesResponse = serde_json::from_str(VOICES_JSON).expect("parses");
        let voices = voices_from(parsed.voices);
        // The third entry has no name, so it cannot be asked for.
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].id, "cmn-CN-Wavenet-A");
        assert_eq!(voices[0].language, "cmn-CN");
        // Unspecified gender is left out rather than read aloud as a constant.
        assert_eq!(voices[0].description, "cmn-CN, Wavenet");
        assert_eq!(voices[1].id, "en-GB-Neural2-A");
        assert_eq!(voices[1].description, "en-GB, female, Neural2");
    }

    #[test]
    fn the_audio_is_base64_and_comes_back_as_bytes() {
        // "ID3" — the first three bytes of an MP3 with a tag on it.
        assert_eq!(decode_audio("SUQz").expect("decodes"), b"ID3");
        assert!(decode_audio("not base64 at all!!").is_err());
    }

    #[test]
    fn debug_output_never_contains_the_key() {
        let rendered = format!("{:?}", request());
        assert!(!rendered.contains("AIza_super_secret_value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains("en-GB-Neural2-A"), "{rendered}");
    }
}

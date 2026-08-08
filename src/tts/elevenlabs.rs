//! ElevenLabs text-to-speech.
//!
//! Requests return MP3, which is what gets played and what gets written for an
//! MP3 save; WAV saves decode it first. Long documents are sent as several
//! requests and the resulting MP3 frames are concatenated, which players handle
//! fine. Each request carries the neighbouring text as context so prosody
//! doesn't reset at every seam.

use super::Voice;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const API_BASE: &str = "https://api.elevenlabs.io/v1";

/// Well under the model limit, so a chunk is never rejected for length.
const MAX_CHARS_PER_REQUEST: usize = 4_500;

/// Characters of surrounding text sent as prosody context.
const CONTEXT_CHARS: usize = 300;

/// 128 kbps MP3 at 44.1 kHz: available on every account tier, and the only
/// format the app needs since WAV saves are decoded from it.
const OUTPUT_FORMAT: &str = "mp3_44100_128";

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("speech-output-engine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not create an HTTP client")
}

#[derive(Deserialize)]
struct VoicesResponse {
    #[serde(default)]
    voices: Vec<VoiceEntry>,
}

#[derive(Deserialize)]
struct VoiceEntry {
    voice_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

/// Fetches the voices available to this key. Doubles as the key check: an
/// invalid key fails here with a clear message.
pub fn list_voices(api_key: &str) -> Result<Vec<Voice>> {
    let response = client()?
        .get(format!("{API_BASE}/voices"))
        .header("xi-api-key", api_key)
        .send()
        .context("could not reach ElevenLabs")?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("ElevenLabs rejected that API key");
    }
    let response = check(response)?;

    let body: VoicesResponse = response
        .json()
        .context("ElevenLabs returned an unexpected voice list")?;

    let mut voices: Vec<Voice> = body
        .voices
        .into_iter()
        .map(|v| Voice {
            id: v.voice_id,
            detail: describe(&v.labels, &v.category),
            name: v.name,
        })
        .collect();
    voices.sort_by_key(|v| v.name.to_lowercase());

    if voices.is_empty() {
        bail!("that ElevenLabs account has no voices available");
    }
    Ok(voices)
}

/// Builds the short description shown after a voice's name.
///
/// The API returns a free-form `labels` map; `accent` and `descriptive` are the
/// two people actually pick a voice by. Values use snake_case, so they are
/// unpicked into words. Falls back to the category ("premade", "cloned").
fn describe(labels: &std::collections::BTreeMap<String, String>, category: &str) -> String {
    let detail = ["accent", "descriptive"]
        .iter()
        .filter_map(|key| labels.get(*key))
        .filter(|value| !value.is_empty())
        .map(|value| value.replace('_', " "))
        .collect::<Vec<_>>()
        .join(", ");
    if detail.is_empty() {
        category.to_string()
    } else {
        detail
    }
}

/// Turns an ElevenLabs error response into a message worth showing a user.
fn check(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/detail/message")
                .or_else(|| v.pointer("/detail"))
                .and_then(|d| d.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(200).collect());

    match status {
        reqwest::StatusCode::UNAUTHORIZED => bail!("ElevenLabs rejected that API key"),
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            bail!("ElevenLabs is rate limiting this key; try again shortly")
        }
        reqwest::StatusCode::PAYMENT_REQUIRED => {
            bail!("this ElevenLabs account is out of credits: {detail}")
        }
        _ => bail!("ElevenLabs returned HTTP {status}: {detail}"),
    }
}

/// Synthesises `text` and returns MP3 bytes.
///
/// `on_progress` is called with the fraction complete after each chunk so a
/// long document doesn't look frozen. Setting `cancel` stops between chunks.
pub fn synthesize(
    api_key: &str,
    voice_id: &str,
    model_id: &str,
    text: &str,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(f32),
) -> Result<Vec<u8>> {
    if voice_id.trim().is_empty() {
        bail!("choose an ElevenLabs voice first");
    }
    let chunks = super::chunk_text(text, MAX_CHARS_PER_REQUEST);
    if chunks.is_empty() {
        bail!("there is no text to read");
    }

    let client = client()?;
    let mut audio = Vec::new();

    for (index, chunk) in chunks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }

        let mut body = serde_json::json!({
            "text": chunk,
            "model_id": model_id,
        });
        // Context is what the API calls the surrounding text; it keeps the
        // voice from resetting its intonation at each chunk boundary.
        if index > 0 {
            body["previous_text"] = tail(&chunks[index - 1], CONTEXT_CHARS).into();
        }
        if let Some(next) = chunks.get(index + 1) {
            body["next_text"] = head(next, CONTEXT_CHARS).into();
        }

        let response = client
            .post(format!(
                "{API_BASE}/text-to-speech/{voice_id}?output_format={OUTPUT_FORMAT}"
            ))
            .header("xi-api-key", api_key)
            .header("accept", "audio/mpeg")
            .json(&body)
            .send()
            .context("could not reach ElevenLabs")?;

        let bytes = check(response)?
            .bytes()
            .context("the audio from ElevenLabs was cut short")?;
        audio.extend_from_slice(&bytes);

        on_progress((index + 1) as f32 / chunks.len() as f32);
    }

    if audio.is_empty() {
        bail!("ElevenLabs returned no audio");
    }
    Ok(audio)
}

/// Character-safe prefix/suffix helpers for building the context strings.
fn head(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

fn tail(text: &str, chars: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(chars)).collect()
}

#[cfg(test)]
mod tests {
    use super::{describe, head, tail};

    /// Mirrors a real `labels` map from `GET /v1/voices`.
    fn labels(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn voice_detail_uses_accent_and_descriptive() {
        let map = labels(&[
            ("gender", "male"),
            ("descriptive", "classy"),
            ("use_case", "conversational"),
            ("accent", "american"),
            ("age", "middle_aged"),
        ]);
        assert_eq!(describe(&map, "premade"), "american, classy");
    }

    #[test]
    fn voice_detail_unpacks_snake_case_and_falls_back_to_category() {
        assert_eq!(
            describe(&labels(&[("accent", "british_essex")]), "premade"),
            "british essex"
        );
        assert_eq!(describe(&labels(&[]), "cloned"), "cloned");
        assert_eq!(describe(&labels(&[("accent", "")]), "premade"), "premade");
    }

    #[test]
    fn context_helpers_respect_character_boundaries() {
        let text = "日本語abc";
        assert_eq!(head(text, 3), "日本語");
        assert_eq!(tail(text, 3), "abc");
        // Asking for more than exists returns the whole string.
        assert_eq!(head(text, 99), text);
        assert_eq!(tail(text, 99), text);
    }
}

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

/// The one ElevenLabs failure the app *acts* on rather than only reports: the
/// key is wrong, so it is thrown away and asked for again instead of being kept
/// to fail every later request in the same way.
///
/// A type rather than a matched-on message, because the message is written for
/// the person reading it and rewording it must never quietly change what the
/// app does with the key.
#[derive(Debug)]
pub struct KeyRejected;

impl std::fmt::Display for KeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ElevenLabs rejected that API key")
    }
}

impl std::error::Error for KeyRejected {}

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
        return Err(KeyRejected.into());
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

/// Whether a string is shaped like an ElevenLabs voice id, and so is safe to
/// put in a URL path unescaped.
///
/// ElevenLabs issues these as short alphanumeric strings — `21m00Tcm4TlvDq8ikWAM`
/// and the like. The set allowed here is deliberately a little wider than that
/// (underscore and hyphen too, in case the format ever grows one) and still
/// contains nothing with meaning in a URL: no `/` to climb to another endpoint,
/// no `?` or `#` to end the path early, no `%` to smuggle any of those in
/// encoded, and no space.
///
/// It matters because the id does not only come back from the API. It is saved
/// in `config.json`, which is a file the user can edit and a file this app will
/// happily load whatever it finds in.
fn is_voice_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        reqwest::StatusCode::UNAUTHORIZED => Err(KeyRejected.into()),
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
/// Text of any length works: it is split into requests of at most
/// [`MAX_CHARS_PER_REQUEST`] characters — comfortably inside the API's own
/// 10,000-character ceiling — and the returned MP3 frames are concatenated, so
/// a 50,000-word document comes back as one continuous recording. There is no
/// length at which the app has to refuse.
///
/// `on_progress` is called with `(parts finished, parts in total)` as each one
/// lands, so a long document can say which part it is on rather than looking
/// frozen. Setting `cancel` stops between parts.
pub fn synthesize(
    api_key: &str,
    voice_id: &str,
    model_id: &str,
    text: &str,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<Vec<u8>> {
    let voice_id = voice_id.trim();
    if voice_id.is_empty() {
        bail!("choose an ElevenLabs voice first");
    }
    // Checked rather than escaped, because a voice id with a `/`, a `?` or a `#`
    // in it is not a voice id somebody mistyped — it is the only value on this
    // request that gets pasted straight into the URL path, and those three
    // characters are what decides which endpoint the key is sent to. Refusing is
    // also the more useful answer: escaping would turn it into a puzzling 404,
    // and there is no legitimate id this rejects. See `is_voice_id`.
    if !is_voice_id(voice_id) {
        bail!(
            "\"{voice_id}\" is not a valid ElevenLabs voice id — choose a voice from the list \
             rather than typing one in"
        );
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
        // Reported before the request rather than after, so the status line
        // names the part currently being waited on.
        on_progress(index, chunks.len());

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
        if bytes.is_empty() {
            bail!(
                "ElevenLabs returned nothing for part {} of {}, so the recording would have a \
                 hole in it",
                index + 1,
                chunks.len()
            );
        }
        audio.extend_from_slice(&bytes);
    }
    on_progress(chunks.len(), chunks.len());

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
    use super::{describe, head, is_voice_id, tail};

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

    /// The ids ElevenLabs actually issues have to keep working, or the check is
    /// worse than the problem it fixes.
    #[test]
    fn real_voice_ids_are_accepted() {
        assert!(is_voice_id("21m00Tcm4TlvDq8ikWAM"));
        assert!(is_voice_id("EXAVITQu4vr4xnSDxMaL"));
        assert!(is_voice_id("voice-with-hyphens"));
        assert!(is_voice_id("voice_with_underscores"));
    }

    /// Everything that could make the request go somewhere other than this one
    /// voice's endpoint. The id reaches the URL path from `config.json`, which
    /// is an ordinary editable file.
    #[test]
    fn an_id_that_could_reshape_the_url_is_refused() {
        assert!(!is_voice_id(""));
        // Climbing out of the endpoint.
        assert!(!is_voice_id("../../v1/user"));
        assert!(!is_voice_id("abc/def"));
        // Ending the path early, so the rest becomes query or fragment.
        assert!(!is_voice_id("abc?output_format=mp3_22050_32"));
        assert!(!is_voice_id("abc#fragment"));
        // Smuggling any of the above in encoded.
        assert!(!is_voice_id("abc%2F..%2Fuser"));
        // Whitespace and control characters have no business in a URL either.
        assert!(!is_voice_id("abc def"));
        assert!(!is_voice_id("abc\ndef"));
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

//! Amazon Polly.
//!
//! ## Credentials
//!
//! Polly is the one provider here that does not take a single key, so this is
//! the one place the app has to meet somebody else's convention rather than
//! set its own. It uses the standard AWS chain, in this order:
//!
//! 1. the environment — `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
//!    optionally `AWS_SESSION_TOKEN`;
//! 2. what was typed into this app, if anything;
//! 3. `~/.aws/credentials`, under the chosen profile — the file the AWS CLI
//!    writes, so a machine already set up for `aws` needs nothing entered here
//!    at all.
//!
//! The environment first because that is what the rest of this app does with
//! an API key, and because it is how a shared machine supplies a credential
//! without it touching any file. What was typed comes before the shared file
//! so that entering credentials in the app visibly does something even on a
//! machine that has a stale `~/.aws` on it.
//!
//! The region comes from the setting, then `AWS_REGION`, then
//! `AWS_DEFAULT_REGION`, then `~/.aws/config`.
//!
//! Nothing invents a credential format: the file read here is the one AWS
//! already writes, and the only thing this app stores of its own is the pair
//! of strings a user may choose to type, under the same "remember this on this
//! computer" rule as every other key.
//!
//! ## Signing
//!
//! Requests are signed with Signature Version 4 by hand — see [`sign`]. It is
//! a chain of HMAC-SHA256 over strings assembled below, and it is here rather
//! than in a vendor SDK because the SDK would be the largest dependency in the
//! tree for one provider out of five.

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::cloud::{describe_api_error, error_body, redacted, RemoteVoice};
use super::EngineKind;

const SERVICE: &str = "polly";
/// Polly refuses a request longer than this, and says so in a way nobody can
/// act on. Reading in paragraphs can genuinely reach it, so it is checked here
/// where the message can name the setting that fixes it.
const MAX_CHARACTERS: usize = 3000;

/// The engines Polly can speak with, best first. Not every voice supports
/// every one, which is why the picker narrows this list to the voice in hand.
pub const ENGINES: &[(&str, &str)] = &[
    ("generative", "Generative — most expressive, dearest"),
    ("neural", "Neural — natural, widely supported"),
    ("long-form", "Long-form — for whole documents"),
    ("standard", "Standard — cheapest, most robotic"),
];

pub const DEFAULT_ENGINE: &str = "neural";
pub const DEFAULT_REGION: &str = "eu-west-2";
pub const DEFAULT_PROFILE: &str = "default";

pub const SIGN_UP_URL: &str = "https://portal.aws.amazon.com/billing/signup";
pub const KEYS_URL: &str = "https://console.aws.amazon.com/iam/home#/security_credentials";

/// Regions Polly is available in, for the picker. Free text as well, because
/// AWS adds regions faster than this list can be updated and a region that is
/// merely unknown to this app should not be unusable.
pub const REGIONS: &[&str] = &[
    "us-east-1",
    "us-west-2",
    "eu-west-1",
    "eu-west-2",
    "eu-central-1",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "ca-central-1",
];

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Present for temporary credentials — an SSO login, or an assumed role.
    pub session_token: String,
}

impl Credentials {
    fn is_complete(&self) -> bool {
        !self.access_key_id.trim().is_empty() && !self.secret_access_key.trim().is_empty()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The access key id is not a secret — it appears in the Authorization
        // header of every signed request, and in AWS's own console — but the
        // secret and the session token are, and neither ever reaches the log.
        f.debug_struct("polly::Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &redacted(&self.secret_access_key))
            .field("session_token", &redacted(&self.session_token))
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub credentials: Credentials,
    pub region: String,
    pub voice_id: String,
    pub engine: String,
}

fn provider() -> &'static str {
    EngineKind::Polly.provider_name()
}

/// A region is interpolated into the hostname, so anything that could reshape
/// it — or point the request at another machine altogether — is refused.
fn is_safe_region(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 32
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn host(region: &str) -> String {
    format!("polly.{region}.amazonaws.com")
}

/// The JSON body for one chunk, split out so it can be checked without keys.
fn body(request: &Request, text: &str) -> serde_json::Value {
    serde_json::json!({
        "Text": text,
        "TextType": "text",
        "VoiceId": request.voice_id,
        "Engine": request.engine,
        "OutputFormat": "mp3",
    })
}

pub fn synthesise(
    http: &reqwest::blocking::Client,
    request: &Request,
    text: &str,
) -> Result<Vec<u8>> {
    if !request.credentials.is_complete() {
        bail!("no AWS credentials for Amazon Polly");
    }
    if request.voice_id.is_empty() {
        bail!("no Amazon Polly voice selected");
    }
    if !is_safe_region(&request.region) {
        bail!("the Amazon Polly region is not a valid region name");
    }
    if text.chars().count() > MAX_CHARACTERS {
        bail!(
            "Amazon Polly will not read more than {MAX_CHARACTERS} characters at once, and this \
             passage is longer. Set Read in to Sentences on the Settings tab."
        );
    }

    let payload = serde_json::to_vec(&body(request, text)).context("building the request")?;
    let signed = sign(
        request,
        "POST",
        "/v1/speech",
        "",
        &[("content-type", "application/json")],
        &payload,
        SystemTime::now(),
    )?;

    let mut post = http
        .post(format!("https://{}/v1/speech", host(&request.region)))
        .header("content-type", "application/json")
        .header("x-amz-date", &signed.amz_date)
        .header("authorization", &signed.authorization)
        .body(payload);
    if !request.credentials.session_token.is_empty() {
        post = post.header("x-amz-security-token", &request.credentials.session_token);
    }

    let response = post.send().context("contacting Amazon Polly")?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "{}",
            describe_api_error(provider(), status, &error_body(response))
        );
    }

    let bytes = response
        .bytes()
        .context("reading audio from Amazon Polly")?;
    Ok(bytes.to_vec())
}

#[derive(Deserialize)]
struct VoicesResponse {
    #[serde(rename = "Voices", default)]
    voices: Vec<ApiVoice>,
}

#[derive(Deserialize)]
struct ApiVoice {
    #[serde(rename = "Id")]
    id: Option<String>,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "LanguageCode")]
    language_code: Option<String>,
    #[serde(rename = "LanguageName")]
    language_name: Option<String>,
    #[serde(rename = "Gender")]
    gender: Option<String>,
    #[serde(rename = "SupportedEngines", default)]
    supported_engines: Vec<String>,
}

pub fn fetch_voices(
    http: &reqwest::blocking::Client,
    request: &Request,
) -> Result<Vec<RemoteVoice>> {
    if !request.credentials.is_complete() {
        bail!("enter your AWS credentials first, or set them in the environment");
    }
    if !is_safe_region(&request.region) {
        bail!("the Amazon Polly region is not a valid region name");
    }

    let signed = sign(
        request,
        "GET",
        "/v1/voices",
        "",
        &[],
        b"",
        SystemTime::now(),
    )?;
    let mut get = http
        .get(format!("https://{}/v1/voices", host(&request.region)))
        .header("x-amz-date", &signed.amz_date)
        .header("authorization", &signed.authorization);
    if !request.credentials.session_token.is_empty() {
        get = get.header("x-amz-security-token", &request.credentials.session_token);
    }

    let response = get.send().context("contacting Amazon Polly")?;
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
            let id = v.id?;
            let mut parts: Vec<String> = Vec::new();
            if let Some(language) = v.language_name.filter(|l| !l.is_empty()) {
                parts.push(language);
            }
            if let Some(gender) = v.gender.filter(|g| !g.is_empty()) {
                parts.push(gender.to_lowercase());
            }
            // Which engines a voice supports is the difference between a
            // choice that works and one that fails on the first sentence, so
            // it is said in the menu rather than discovered later.
            if !v.supported_engines.is_empty() {
                parts.push(v.supported_engines.join("/"));
            }
            Some(RemoteVoice {
                name: v.name.unwrap_or_else(|| id.clone()),
                language: v.language_code.unwrap_or_default(),
                description: parts.join(", "),
                engines: v.supported_engines,
                id,
            })
        })
        .collect();
    voices.sort_by_key(|v| v.name.to_lowercase());
    voices
}

// ------------------------------------------------------- signature version 4

struct Signed {
    amz_date: String,
    authorization: String,
}

type HmacSha256 = Hmac<Sha256>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Sign one request, returning the two headers that make it authentic.
///
/// `extra_headers` are the headers besides `host` and `x-amz-date` that must
/// be covered by the signature; they have to be sent exactly as given here or
/// AWS will compute a different signature and refuse the request.
fn sign(
    request: &Request,
    method: &str,
    path: &str,
    query: &str,
    extra_headers: &[(&str, &str)],
    payload: &[u8],
    now: SystemTime,
) -> Result<Signed> {
    let (amz_date, date_stamp) = timestamps(now)?;
    let host = host(&request.region);

    // Every signed header, lower-cased and sorted, as SigV4 requires.
    let mut headers: Vec<(String, String)> = extra_headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), (*v).to_string()))
        .collect();
    headers.push(("host".to_string(), host.clone()));
    headers.push(("x-amz-date".to_string(), amz_date.clone()));
    if !request.credentials.session_token.is_empty() {
        headers.push((
            "x-amz-security-token".to_string(),
            request.credentials.session_token.clone(),
        ));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed_headers = signed_headers.join(";");
    let payload_hash = sha256_hex(payload);

    let canonical_request =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", request.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let key = signing_key(
        &request.credentials.secret_access_key,
        &date_stamp,
        &request.region,
        SERVICE,
    );
    let signature = hex(&hmac(&key, to_sign.as_bytes()));

    Ok(Signed {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
             Signature={signature}",
            request.credentials.access_key_id
        ),
        amz_date,
    })
}

/// The four-step key derivation: secret, then date, region, service, and the
/// terminator. Each step signs the previous result.
fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let start = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let by_region = hmac(&start, region.as_bytes());
    let by_service = hmac(&by_region, service.as_bytes());
    hmac(&by_service, b"aws4_request")
}

/// `20260902T134500Z` and `20260902`, which is the only date formatting this
/// app needs and so is not worth a calendar crate.
fn timestamps(now: SystemTime) -> Result<(String, String)> {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .context("this computer's clock is set before 1970, which AWS will not accept")?
        .as_secs();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let time = seconds % 86_400;
    let (hour, minute, second) = (time / 3600, (time % 3600) / 60, time % 60);
    Ok((
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    ))
}

/// Days since the epoch to a calendar date, by Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ------------------------------------------------ the standard AWS locations

/// What the shared AWS files have to say, for one profile.
#[derive(Clone, Default)]
pub struct Shared {
    pub credentials: Option<Credentials>,
    pub region: Option<String>,
}

/// Cached, because the UI asks "are there credentials?" once per frame while
/// deciding whether to show the button for entering some, and stat-ing two
/// files sixty times a second to answer it would be absurd. Short enough that
/// a user who has just run `aws configure` in another window sees it.
const SHARED_TTL: Duration = Duration::from_secs(3);

static SHARED_CACHE: Mutex<Option<(Instant, String, Shared)>> = Mutex::new(None);

/// `~/.aws/credentials` and `~/.aws/config`, for the named profile.
pub fn shared(profile: &str) -> Shared {
    let profile = if profile.trim().is_empty() {
        std::env::var("AWS_PROFILE").unwrap_or_else(|_| DEFAULT_PROFILE.to_string())
    } else {
        profile.trim().to_string()
    };

    if let Ok(cache) = SHARED_CACHE.lock() {
        if let Some((read_at, cached_profile, shared)) = cache.as_ref() {
            if *cached_profile == profile && read_at.elapsed() < SHARED_TTL {
                return shared.clone();
            }
        }
    }

    let found = read_shared(&profile);
    if let Ok(mut cache) = SHARED_CACHE.lock() {
        *cache = Some((Instant::now(), profile, found.clone()));
    }
    found
}

fn read_shared(profile: &str) -> Shared {
    let credentials = credentials_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| {
            let section = ini_section(&text, profile)?;
            let credentials = Credentials {
                access_key_id: section
                    .get("aws_access_key_id")
                    .cloned()
                    .unwrap_or_default(),
                secret_access_key: section
                    .get("aws_secret_access_key")
                    .cloned()
                    .unwrap_or_default(),
                session_token: section
                    .get("aws_session_token")
                    .cloned()
                    .unwrap_or_default(),
            };
            credentials.is_complete().then_some(credentials)
        });

    // `~/.aws/config` names the default profile `[default]` and every other
    // one `[profile name]`, which `~/.aws/credentials` does not.
    let region = config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| {
            let heading = if profile == DEFAULT_PROFILE {
                profile.to_string()
            } else {
                format!("profile {profile}")
            };
            ini_section(&text, &heading)?.get("region").cloned()
        })
        .filter(|region| !region.is_empty());

    Shared {
        credentials,
        region,
    }
}

fn aws_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".aws"))
}

fn credentials_path() -> Option<PathBuf> {
    match std::env::var("AWS_SHARED_CREDENTIALS_FILE") {
        Ok(path) if !path.trim().is_empty() => Some(PathBuf::from(path)),
        _ => aws_dir().map(|dir| dir.join("credentials")),
    }
}

fn config_path() -> Option<PathBuf> {
    match std::env::var("AWS_CONFIG_FILE") {
        Ok(path) if !path.trim().is_empty() => Some(PathBuf::from(path)),
        _ => aws_dir().map(|dir| dir.join("config")),
    }
}

/// The key-value pairs under one `[heading]` of an INI file.
///
/// Enough of the format for the two files AWS writes, and no more: comments,
/// headings, `key = value`. Nested sub-settings (`sso_session`, and the
/// indented blocks under it) are read as ordinary keys and ignored, which is
/// what should happen to a setting this app has no use for.
fn ini_section(text: &str, heading: &str) -> Option<std::collections::HashMap<String, String>> {
    let mut inside = false;
    let mut found = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if inside {
                break; // The next heading ends the one we wanted.
            }
            inside = name.trim() == heading;
            continue;
        }
        if inside {
            if let Some((key, value)) = line.split_once('=') {
                found.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }
    }
    (!found.is_empty()).then_some(found)
}

/// Credentials from the environment, if it has a complete set.
pub fn environment_credentials() -> Option<Credentials> {
    let read = |name: &str| std::env::var(name).unwrap_or_default().trim().to_string();
    let credentials = Credentials {
        access_key_id: read("AWS_ACCESS_KEY_ID"),
        secret_access_key: read("AWS_SECRET_ACCESS_KEY"),
        session_token: read("AWS_SESSION_TOKEN"),
    };
    credentials.is_complete().then_some(credentials)
}

/// The region the environment asks for, if it asks for one.
pub fn environment_region() -> Option<String> {
    for name in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(region) = std::env::var(name) {
            let region = region.trim();
            if !region.is_empty() {
                return Some(region.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            credentials: Credentials {
                access_key_id: "AKIDEXAMPLE".to_string(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                session_token: String::new(),
            },
            region: "eu-west-2".to_string(),
            voice_id: "Amy".to_string(),
            engine: DEFAULT_ENGINE.to_string(),
        }
    }

    #[test]
    fn the_body_asks_for_mp3_so_the_existing_decoder_can_read_it() {
        let body = body(&request(), "Hello.");
        assert_eq!(body["OutputFormat"], "mp3");
        assert_eq!(body["VoiceId"], "Amy");
        assert_eq!(body["Engine"], "neural");
        assert_eq!(body["TextType"], "text");
        assert_eq!(body["Text"], "Hello.");
    }

    /// The region becomes part of the hostname, so a settings file must not be
    /// able to send this app's requests — and the user's credentials — to a
    /// machine of somebody else's choosing.
    #[test]
    fn a_region_that_could_reshape_the_host_is_refused() {
        assert!(is_safe_region("eu-west-2"));
        assert!(is_safe_region("ap-northeast-1"));
        assert!(!is_safe_region(""));
        assert!(!is_safe_region("eu-west-2.evil.example.com"));
        assert!(!is_safe_region("eu-west-2/../.."));
        assert!(!is_safe_region("EU-WEST-2"));
        assert_eq!(host("eu-west-2"), "polly.eu-west-2.amazonaws.com");
    }

    /// AWS's own worked example from the SigV4 documentation, which is the
    /// only way to know the derivation below is right without a live account.
    #[test]
    fn the_signing_key_matches_amazons_worked_example() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn a_signature_covers_every_header_it_says_it_covers() {
        let signed = sign(
            &request(),
            "POST",
            "/v1/speech",
            "",
            &[("content-type", "application/json")],
            b"{}",
            UNIX_EPOCH + Duration::from_secs(1_756_800_000),
        )
        .expect("signs");

        assert!(signed
            .authorization
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=content-type;host;x-amz-date"),
            "{}",
            signed.authorization
        );
        assert!(signed
            .authorization
            .contains("/eu-west-2/polly/aws4_request"));
        // The secret must never appear in a header that gets logged.
        assert!(!signed.authorization.contains("wJalrXUtnFEMI"));
    }

    /// Temporary credentials must have their token signed as well, or AWS
    /// rejects the request with a signature mismatch that says nothing useful.
    #[test]
    fn a_session_token_is_part_of_the_signature() {
        let mut request = request();
        request.credentials.session_token = "IQoJb3JpZ2luX2VjE".to_string();
        let signed = sign(
            &request,
            "GET",
            "/v1/voices",
            "",
            &[],
            b"",
            SystemTime::now(),
        )
        .expect("signs");
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
            "{}",
            signed.authorization
        );
    }

    #[test]
    fn the_timestamp_is_the_two_formats_aws_asks_for() {
        // 2026-09-02T00:00:00Z
        let (amz, stamp) =
            timestamps(UNIX_EPOCH + Duration::from_secs(1_788_307_200)).expect("a date after 1970");
        assert_eq!(amz, "20260902T000000Z");
        assert_eq!(stamp, "20260902");

        // A leap day, and a time that is not midnight.
        let (amz, stamp) =
            timestamps(UNIX_EPOCH + Duration::from_secs(1_709_206_496)).expect("a date after 1970");
        assert_eq!(amz, "20240229T113456Z");
        assert_eq!(stamp, "20240229");

        assert_eq!(
            timestamps(UNIX_EPOCH).expect("the epoch itself").0,
            "19700101T000000Z"
        );
    }

    const VOICES_JSON: &str = r#"{
      "Voices": [
        {
          "Gender": "Female",
          "Id": "Amy",
          "LanguageCode": "en-GB",
          "LanguageName": "British English",
          "Name": "Amy",
          "SupportedEngines": ["generative", "neural", "standard"]
        },
        {
          "Gender": "Male",
          "Id": "Brian",
          "LanguageCode": "en-GB",
          "LanguageName": "British English",
          "Name": "Brian",
          "SupportedEngines": ["neural", "standard"]
        },
        { "Gender": "Female", "LanguageCode": "en-US" }
      ]
    }"#;

    #[test]
    fn the_voice_list_says_which_engines_each_voice_can_use() {
        let parsed: VoicesResponse = serde_json::from_str(VOICES_JSON).expect("parses");
        let voices = voices_from(parsed.voices);
        assert_eq!(voices.len(), 2, "a voice with no Id cannot be asked for");
        assert_eq!(voices[0].id, "Amy");
        assert_eq!(voices[0].language, "en-GB");
        assert_eq!(
            voices[0].description,
            "British English, female, generative/neural/standard"
        );
        assert_eq!(voices[0].engines, ["generative", "neural", "standard"]);
        assert_eq!(voices[1].engines, ["neural", "standard"]);
    }

    #[test]
    fn the_shared_credentials_file_is_read_the_way_the_aws_cli_writes_it() {
        let text = "\
# a comment
[default]
aws_access_key_id = AKIAIOSFODNN7EXAMPLE
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY

[work]
aws_access_key_id=AKIAI44QH8DHBEXAMPLE
aws_secret_access_key=je7MtGbClwBF/2Zp9Utk/h3yCo8nvbEXAMPLEKEY
aws_session_token = FQoGZXIvYXdzEBYaD
";
        let default = ini_section(text, "default").expect("a default profile");
        assert_eq!(
            default.get("aws_access_key_id").map(String::as_str),
            Some("AKIAIOSFODNN7EXAMPLE")
        );
        // The next heading ends the section: the default profile has no token.
        assert!(!default.contains_key("aws_session_token"));

        let work = ini_section(text, "work").expect("a named profile");
        assert_eq!(
            work.get("aws_session_token").map(String::as_str),
            Some("FQoGZXIvYXdzEBYaD")
        );
        assert!(ini_section(text, "nobody").is_none());
    }

    #[test]
    fn incomplete_credentials_are_treated_as_none_at_all() {
        let half = Credentials {
            access_key_id: "AKIA".to_string(),
            secret_access_key: String::new(),
            session_token: String::new(),
        };
        assert!(!half.is_complete());
        assert!(request().credentials.is_complete());
    }

    #[test]
    fn debug_output_never_contains_the_secret() {
        let rendered = format!("{:?}", request());
        assert!(!rendered.contains("wJalrXUtnFEMI"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        // The access key id is not a secret and is worth having in the log.
        assert!(rendered.contains("AKIDEXAMPLE"), "{rendered}");
    }
}

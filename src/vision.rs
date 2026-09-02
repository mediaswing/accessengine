//! Image description via a locally installed model (Ollama).
//!
//! Entirely optional and entirely local: if Ollama is not running, the feature
//! reports that and everything else in the app carries on working. Nothing is
//! sent to a third party.

use crate::t;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// Vision models are slow, especially the first run when the model is paged in.
const GENERATE_TIMEOUT: Duration = Duration::from_secs(300);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// A place-name lookup is a nicety; nobody should wait long for one.
const GEOCODE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ollama holds the whole request in memory; refuse silly inputs early.
const MAX_IMAGE_BYTES: u64 = 24 * 1024 * 1024;
/// How long to wait for a server we just started to accept connections.
const SERVER_WAIT: Duration = Duration::from_secs(20);
/// Gap between readiness probes while waiting for it.
const PROBE_INTERVAL: Duration = Duration::from_millis(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `heic`/`heif` are offered on every platform even though only macOS can
/// convert them, so that picking one fails with an explanation rather than
/// the file being invisible in the dialog.
pub const IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "webp", "gif", "bmp", "heic", "heif"];

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub name: String,
    /// Whether this looks like it can accept images at all.
    pub vision_capable: bool,
}

#[derive(Debug)]
pub enum VisionResult {
    Models(Vec<ModelInfo>),
    Description(String),
    Error(String),
}

/// A single outstanding request. One at a time is plenty: these are heavy, and
/// queueing several would just exhaust the user's GPU.
#[derive(Default)]
pub struct Vision {
    pending: Option<Receiver<VisionResult>>,
    pub busy_message: Option<String>,
}

impl Vision {
    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Ask Ollama which models are installed.
    pub fn list_models(&mut self, base_url: String, repaint: impl Fn() + Send + 'static) {
        self.start(&t!("vision.looking_for_models"), repaint, move || {
            match list_models(&base_url) {
                Ok(models) => VisionResult::Models(models),
                Err(e) => VisionResult::Error(format!("{e:#}")),
            }
        });
    }

    /// Describe an image. `geotag` adds where a geotagged photo was taken,
    /// which is the one part of this that leaves the machine.
    pub fn describe(
        &mut self,
        base_url: String,
        model: String,
        prompt: String,
        image: PathBuf,
        geotag: bool,
        repaint: impl Fn() + Send + 'static,
    ) {
        let message = t!("vision.describing", model = model);
        self.start(&message, repaint, move || {
            match describe_image(&base_url, &model, &prompt, &image, geotag) {
                Ok(text) => VisionResult::Description(text),
                Err(e) => VisionResult::Error(format!("{e:#}")),
            }
        });
    }

    fn start(
        &mut self,
        message: &str,
        repaint: impl Fn() + Send + 'static,
        work: impl FnOnce() -> VisionResult + Send + 'static,
    ) {
        if self.is_busy() {
            log::debug!("ignoring vision request: one is already running");
            return;
        }
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("ollama".to_string())
            .spawn(move || {
                let _ = tx.send(work());
                repaint();
            });
        match spawned {
            Ok(_) => {
                self.pending = Some(rx);
                self.busy_message = Some(message.to_string());
            }
            Err(e) => {
                log::error!("could not spawn the vision thread: {e}");
                self.busy_message = None;
            }
        }
    }

    /// Call each frame; returns a result once the worker has one.
    pub fn poll(&mut self) -> Option<VisionResult> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                self.busy_message = None;
                Some(result)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                self.busy_message = None;
                Some(VisionResult::Error(
                    "the local model request ended unexpectedly".to_string(),
                ))
            }
        }
    }
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("building HTTP client")
}

fn normalise(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// Names and model families that indicate image input is supported.
const VISION_HINTS: &[&str] = &[
    "llava", "bakllava", "moondream", "minicpm-v", "llama3.2-vision", "llama4", "vision", "-vl",
    "gemma3", "mistral-small3", "granite3.2-vision", "internvl", "qwen2.5vl", "qwen3-vl",
];
const VISION_FAMILIES: &[&str] = &["clip", "mllama", "qwen2vl", "gemma3", "siglip", "internvl"];

fn looks_like_vision(name: &str, families: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    if VISION_HINTS.iter().any(|h| lower.contains(h)) {
        return true;
    }
    families
        .iter()
        .any(|f| VISION_FAMILIES.contains(&f.to_ascii_lowercase().as_str()))
}

/// Whether this address is a server on this machine — one we may start, and
/// the only case in which nothing about an image leaves the computer. A remote
/// Ollama is somebody else's to run.
pub fn is_local(base_url: &str) -> bool {
    let rest = normalise(base_url);
    let rest = rest
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&rest);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    // `http://127.0.0.1@evil.example` has an authority of `evil.example`; the
    // part before the `@` is a username, however much it looks like a host.
    let host = host.rsplit_once('@').map_or(host, |(_, after)| after);
    // Strip the port, taking the bracketed form of an IPv6 literal into account.
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1"
    )
}

/// Where the `ollama` binary is, for a GUI app that cannot rely on the user's
/// shell `PATH`: an app launched from Finder or the Start menu inherits a bare
/// environment.
///
/// Absolute paths come first and the bare name is the last resort, deliberately.
/// A bare name is resolved by searching, and what gets searched is not ours to
/// choose: any writable directory earlier in `PATH` wins, and on Windows the
/// process-creation search order can include the current directory — which is
/// whatever folder the app happened to be launched from.
#[cfg(target_os = "macos")]
const SERVER_COMMANDS: &[&[&str]] = &[
    &["/usr/local/bin/ollama", "serve"],
    &["/opt/homebrew/bin/ollama", "serve"],
    &["/Applications/Ollama.app/Contents/Resources/ollama", "serve"],
    // The menu-bar app starts the server itself, and is what a Mac user who
    // installed the download rather than the CLI actually has.
    &["/usr/bin/open", "-a", "Ollama"],
    &["ollama", "serve"],
];
#[cfg(target_os = "windows")]
const SERVER_COMMANDS: &[&[&str]] = &[&["ollama.exe", "serve"]];
#[cfg(all(unix, not(target_os = "macos")))]
const SERVER_COMMANDS: &[&[&str]] = &[
    &["/usr/local/bin/ollama", "serve"],
    &["/usr/bin/ollama", "serve"],
    &["/opt/ollama/bin/ollama", "serve"],
    &["ollama", "serve"],
];

/// The per-user install location, which is where the Windows installer puts
/// `ollama.exe` and is an absolute path we can trust.
#[cfg(target_os = "windows")]
fn user_install_candidates() -> Vec<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|local| {
            vec![PathBuf::from(local)
                .join("Programs")
                .join("Ollama")
                .join("ollama.exe")]
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "windows"))]
fn user_install_candidates() -> Vec<PathBuf> {
    Vec::new()
}

/// Is anything answering there?
fn is_reachable(base_url: &str) -> bool {
    let Ok(client) = client(PROBE_TIMEOUT) else {
        return false;
    };
    client
        .get(format!("{}/api/tags", normalise(base_url)))
        .send()
        .is_ok()
}

/// Launch the server and wait for it to bind, returning whether it came up.
///
/// Starting it for the user is the point: the alternative is telling somebody
/// who just picked a photograph to go and run a terminal command. Nothing is
/// installed and nothing is downloaded — this only runs a program already on
/// the machine, and gives up quietly if there isn't one.
fn start_local_server(base_url: &str) -> bool {
    if !is_local(base_url) {
        return false;
    }
    log::info!("Ollama is not answering at {base_url}; trying to start it");

    let mut spawned = false;
    let installed = user_install_candidates();
    let user_installs: Vec<Vec<&str>> = installed
        .iter()
        .filter_map(|p| p.to_str().map(|p| vec![p, "serve"]))
        .collect();
    let candidates = user_installs
        .iter()
        .map(|c| c.as_slice())
        .chain(SERVER_COMMANDS.iter().copied());

    for command in candidates {
        let (program, args) = command.split_first().expect("each command names a program");
        // An absolute path that is not there is not worth a spawn attempt, and
        // the failure would only clutter the log.
        if program.starts_with('/') && !Path::new(program).exists() {
            continue;
        }
        match std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                log::info!("started Ollama with `{program}`");
                // Reap it if it exits first, so a failed attempt does not sit
                // around as a zombie for the life of the app.
                let _ = std::thread::Builder::new()
                    .name("ollama-serve".to_string())
                    .spawn(move || {
                        let _ = child.wait();
                    });
                spawned = true;
                break;
            }
            Err(e) => log::debug!("could not run `{program}`: {e}"),
        }
    }
    if !spawned {
        log::warn!("no `ollama` command found to start");
        return false;
    }

    let deadline = Instant::now() + SERVER_WAIT;
    while Instant::now() < deadline {
        std::thread::sleep(PROBE_INTERVAL);
        if is_reachable(base_url) {
            log::info!("Ollama is up at {base_url}");
            return true;
        }
    }
    log::warn!("Ollama did not start answering within {}s", SERVER_WAIT.as_secs());
    false
}

/// Send a request, starting a local Ollama and trying once more if nothing is
/// listening yet.
fn send_or_start(
    request: reqwest::blocking::RequestBuilder,
    base_url: &str,
) -> Result<reqwest::blocking::Response> {
    let retry = request.try_clone();
    match request.send() {
        Ok(response) => Ok(response),
        Err(e) if e.is_connect() => {
            let Some(retry) = retry.filter(|_| start_local_server(base_url)) else {
                return Err(connection_error(e, base_url));
            };
            retry.send().map_err(|e| connection_error(e, base_url))
        }
        Err(e) => Err(connection_error(e, base_url)),
    }
}

/// Ollama writes `null` where the API documents a list: `families` for a model
/// that carries no family metadata, and `models` itself on some builds. A plain
/// `#[serde(default)]` covers a missing key but not an explicit null, so one
/// such field would fail the whole list and leave the user with no models at
/// all rather than the one entry that lacks the detail.
fn null_is_empty<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

fn parse_models(body: &str) -> Result<Vec<ModelInfo>> {
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default, deserialize_with = "null_is_empty")]
        models: Vec<Tag>,
    }
    #[derive(Deserialize)]
    struct Tag {
        name: String,
        #[serde(default)]
        details: Details,
    }
    #[derive(Deserialize, Default)]
    struct Details {
        #[serde(default, deserialize_with = "null_is_empty")]
        families: Vec<String>,
    }

    let tags: Tags = serde_json::from_str(body).context("reading the model list")?;
    let mut models: Vec<ModelInfo> = tags
        .models
        .into_iter()
        .map(|t| ModelInfo {
            vision_capable: looks_like_vision(&t.name, &t.details.families),
            name: t.name,
        })
        .collect();

    // Vision models first, so the useful ones are at the top of the list.
    models.sort_by(|a, b| {
        b.vision_capable
            .cmp(&a.vision_capable)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(models)
}

fn list_models(base_url: &str) -> Result<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", normalise(base_url));
    let response = send_or_start(client(LIST_TIMEOUT)?.get(&url), base_url)?;

    if !response.status().is_success() {
        bail!("Ollama returned HTTP {} from {url}", response.status());
    }

    parse_models(&response.text().context("reading the model list")?)
}

fn describe_image(
    base_url: &str,
    model: &str,
    prompt: &str,
    image: &Path,
    geotag: bool,
) -> Result<String> {
    #[derive(Deserialize)]
    struct GenerateResponse {
        #[serde(default)]
        response: String,
        #[serde(default)]
        error: Option<String>,
    }

    if model.trim().is_empty() {
        bail!("choose a local model first");
    }

    let meta = std::fs::metadata(image)
        .with_context(|| format!("opening {}", image.display()))?;
    if meta.len() > MAX_IMAGE_BYTES {
        bail!(
            "{} is {:.1} MB; images are capped at 24 MB",
            image.display(),
            meta.len() as f64 / 1_048_576.0
        );
    }

    let mut bytes =
        std::fs::read(image).with_context(|| format!("reading {}", image.display()))?;
    if is_heif(&bytes) {
        // The cap was checked against the HEIC; JPEG of the same picture can be
        // larger, so check again rather than posting an oversized request.
        bytes = heif_to_jpeg(image)?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            bail!(
                "{} becomes {:.1} MB as JPEG; images are capped at 24 MB",
                image.display(),
                bytes.len() as f64 / 1_048_576.0
            );
        }
    }
    if !looks_like_image(&bytes) {
        bail!("{} does not look like an image file", image.display());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let url = format!("{}/api/generate", normalise(base_url));
    let body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "images": [encoded],
        "stream": false,
    });

    log::info!("asking {model} to describe {}", image.display());
    let response = send_or_start(client(GENERATE_TIMEOUT)?.post(&url).json(&body), base_url)?;

    let status = response.status();
    let text = response.text().context("reading the model's reply")?;
    if !status.is_success() {
        // Ollama puts a readable reason in `error` for 404s (model not pulled)
        // and 400s (model cannot take images).
        let detail = serde_json::from_str::<GenerateResponse>(&text)
            .ok()
            .and_then(|r| r.error)
            .unwrap_or_else(|| text.clone());
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!(t!("error.model_missing", model = model));
        }
        bail!("Ollama returned HTTP {status}: {}", detail.trim());
    }

    let parsed: GenerateResponse =
        serde_json::from_str(&text).context("parsing the model's reply")?;
    if let Some(error) = parsed.error {
        bail!("{error}");
    }
    let described = parsed.response.trim().to_string();
    if described.is_empty() {
        bail!("'{model}' returned an empty description; it may not accept images");
    }
    if !geotag {
        return Ok(described);
    }
    // Read the location from the file the user picked, not from any JPEG we
    // made from it: a conversion is free to drop the GPS tags.
    Ok(match location_sentence(image) {
        Some(sentence) => format!("{described}\n\n{sentence}"),
        None => described,
    })
}

/// Sniff the magic bytes so a mis-named file fails with a clear message rather
/// than as a confusing model error.
fn looks_like_image(bytes: &[u8]) -> bool {
    const SIGNATURES: &[&[u8]] = &[
        &[0x89, b'P', b'N', b'G'],       // PNG
        &[0xFF, 0xD8, 0xFF],             // JPEG
        b"GIF87a",
        b"GIF89a",
        b"BM",                           // BMP
    ];
    if SIGNATURES.iter().any(|sig| bytes.starts_with(sig)) {
        return true;
    }
    // WebP: "RIFF....WEBP"
    bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
}

/// HEIF is ISO base media format: a `ftyp` box at offset 4, then a brand that
/// says which flavour. iPhone photos are `heic`.
fn is_heif(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    const BRANDS: &[&[u8]] = &[
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    ];
    BRANDS.iter().any(|brand| &bytes[8..12] == *brand)
}

/// Ollama decodes images with stb_image, which has no HEIF support, so an
/// iPhone photo has to become a JPEG before it is worth sending. macOS ships
/// `sips`, which uses the system decoder; nothing equivalent is guaranteed
/// elsewhere, so those platforms say so plainly instead.
#[cfg(target_os = "macos")]
fn heif_to_jpeg(image: &Path) -> Result<Vec<u8>> {
    // Convert inside a directory created fresh for this one conversion.
    // `create_dir` fails if the path already exists, so nobody can have put a
    // symlink there first and had us write — or read back — somewhere else. A
    // fixed name in a shared temp directory is exactly that hazard.
    let dir = unique_temp_dir()?;
    let out = dir.join("converted.jpg");

    let result = std::process::Command::new("sips")
        .args(["-s", "format", "jpeg"])
        .arg(image)
        .arg("--out")
        .arg(&out)
        .output()
        .context("running sips to convert the HEIC image");

    let bytes = result.and_then(|result| {
        if !result.status.success() {
            bail!(
                "could not convert {} to JPEG: {}",
                image.display(),
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
        std::fs::read(&out).with_context(|| format!("reading {}", out.display()))
    });

    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// A directory that did not exist a moment ago, inside the temp directory.
#[cfg(target_os = "macos")]
fn unique_temp_dir() -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let base = std::env::temp_dir();
    for _ in 0..8 {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default()
            ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(2_654_435_761);
        let candidate = base.join(format!(
            "accessengine-{}-{nonce:08x}",
            std::process::id()
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("creating a temporary directory")),
        }
    }
    bail!("could not create a temporary directory for the conversion")
}

#[cfg(not(target_os = "macos"))]
fn heif_to_jpeg(image: &Path) -> Result<Vec<u8>> {
    bail!(
        "{} is a HEIC/HEIF image, which this platform cannot convert. \
         Export it as JPEG or PNG and try again.",
        image.display()
    )
}

/// Where the photo was taken, phrased to be spoken. `None` when the image
/// carries no GPS tags at all — the common case for anything but a phone, and
/// not worth a sentence of its own on every screenshot.
///
/// A place name, not coordinates: "Taken at 52.4755 degrees north, 1.9061
/// degrees west" is a correct answer to a question nobody asked. Turning one
/// into the other means sending the coordinates — not the image — to a lookup
/// service, which is the one thing about this feature that leaves the machine.
fn location_sentence(image: &Path) -> Option<String> {
    let (latitude, longitude) = coordinates(image)?;
    Some(match reverse_geocode(latitude, longitude) {
        Ok(Some(place)) => t!("vision.taken_in", place = place),
        Ok(None) => {
            log::info!("no place name for {latitude:.4}, {longitude:.4}");
            t!("vision.place_unknown")
        }
        Err(e) => {
            log::warn!("looking up the photo location: {e:#}");
            t!("vision.place_unknown")
        }
    })
}

/// The photo's own latitude and longitude, if it has them.
fn coordinates(image: &Path) -> Option<(f64, f64)> {
    let file = std::fs::File::open(image).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let latitude = coordinate(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef)?;
    let longitude = coordinate(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)?;
    Some((latitude, longitude))
}

/// Ask OpenStreetMap what is at these coordinates.
///
/// Nominatim is free and needs no account, and asks in return for a User-Agent
/// that identifies the caller and for no more than one request a second. One
/// lookup per described photograph is well inside that.
fn reverse_geocode(latitude: f64, longitude: f64) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Place {
        #[serde(default)]
        address: Address,
    }

    let url = format!(
        "https://nominatim.openstreetmap.org/reverse?format=jsonv2&zoom=14&lat={latitude}&lon={longitude}"
    );
    let response = reqwest::blocking::Client::builder()
        .timeout(GEOCODE_TIMEOUT)
        .user_agent(concat!(
            "AccessEngine/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/mediaswing/accessengine)"
        ))
        .build()
        .context("building HTTP client")?
        .get(&url)
        .send()
        .context("asking OpenStreetMap where that is")?;

    if !response.status().is_success() {
        bail!("OpenStreetMap returned HTTP {}", response.status());
    }
    let place: Place = response.json().context("reading the place")?;
    Ok(place_name(&place.address))
}

/// The parts of a Nominatim address this app has any use for.
#[derive(Deserialize, Default)]
struct Address {
    #[serde(default)]
    neighbourhood: Option<String>,
    #[serde(default)]
    suburb: Option<String>,
    #[serde(default)]
    hamlet: Option<String>,
    #[serde(default)]
    village: Option<String>,
    #[serde(default)]
    town: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    county: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

/// Somewhere between a postcode and a continent: the two or three names a
/// person would actually use.
///
/// Nominatim's own `display_name` is the whole hierarchy — house number, road,
/// suburb, city, county, postcode, country — which is a paragraph to listen to
/// and mostly noise. This takes the most specific locality, the settlement it
/// sits in, and the country, skipping any that repeat.
fn place_name(address: &Address) -> Option<String> {
    let locality = address
        .neighbourhood
        .as_deref()
        .or(address.suburb.as_deref())
        .or(address.hamlet.as_deref());
    let settlement = address
        .city
        .as_deref()
        .or(address.town.as_deref())
        .or(address.village.as_deref());
    // A county only earns its place when there is no settlement to name.
    let region = if settlement.is_some() {
        None
    } else {
        address.county.as_deref().or(address.state.as_deref())
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in [locality, settlement, region, address.country.as_deref()]
        .into_iter()
        .flatten()
    {
        let part = part.trim();
        if !part.is_empty() && !parts.iter().any(|p| p.eq_ignore_ascii_case(part)) {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// One signed decimal degree from the degrees/minutes/seconds triple and the
/// N/S/E/W that goes with it.
fn coordinate(exif: &exif::Exif, value: exif::Tag, reference: exif::Tag) -> Option<f64> {
    let field = exif.get_field(value, exif::In::PRIMARY)?;
    let exif::Value::Rational(parts) = &field.value else {
        return None;
    };
    let [degrees, minutes, seconds] = parts.get(..3)? else {
        return None;
    };
    let hemisphere = exif
        .get_field(reference, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .unwrap_or_default();
    Some(dms_to_decimal(
        degrees.to_f64(),
        minutes.to_f64(),
        seconds.to_f64(),
        &hemisphere,
    ))
}

fn dms_to_decimal(degrees: f64, minutes: f64, seconds: f64, hemisphere: &str) -> f64 {
    let magnitude = degrees + minutes / 60.0 + seconds / 3600.0;
    let south_or_west = hemisphere
        .trim()
        .starts_with(['S', 's', 'W', 'w']);
    if south_or_west {
        -magnitude
    } else {
        magnitude
    }
}

/// A refused connection is the overwhelmingly common failure here, and the fix
/// is specific enough to be worth saying out loud.
fn connection_error(e: reqwest::Error, base_url: &str) -> anyhow::Error {
    if e.is_connect() {
        anyhow::anyhow!(t!("error.ollama_unreachable", url = base_url))
    } else if e.is_timeout() {
        anyhow::anyhow!(t!("error.model_timeout"))
    } else {
        anyhow::Error::new(e).context(format!("talking to Ollama at {base_url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_models_are_recognised_by_name_or_family() {
        assert!(looks_like_vision("llava:13b", &[]));
        assert!(looks_like_vision("qwen2.5vl:7b", &[]));
        assert!(looks_like_vision("custom-model", &["clip".to_string()]));
        assert!(!looks_like_vision("llama3:8b", &["llama".to_string()]));
    }

    /// Ollama sends `"families": null` for models with no family metadata, and
    /// that used to fail the whole list.
    #[test]
    fn null_lists_in_the_model_list_are_read_as_empty() {
        let body = r#"{"models":[
            {"name":"llama3:8b","details":{"families":["llama"]}},
            {"name":"nomic-embed-text:latest","details":{"families":null}},
            {"name":"llava:13b","details":{}}
        ]}"#;
        let models = parse_models(body).expect("a null family is not a failure");
        let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
        // Vision first, then by name.
        assert_eq!(names, ["llava:13b", "llama3:8b", "nomic-embed-text:latest"]);
    }

    #[test]
    fn a_null_model_list_is_no_models() {
        assert!(parse_models(r#"{"models":null}"#).unwrap().is_empty());
        assert!(parse_models("{}").unwrap().is_empty());
    }

    #[test]
    fn image_signatures_are_detected() {
        assert!(looks_like_image(&[0x89, b'P', b'N', b'G', 0x0D]));
        assert!(looks_like_image(b"GIF89a...."));
        assert!(looks_like_image(b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!looks_like_image(b"just some text"));
    }

    #[test]
    fn heif_brands_are_detected() {
        // Byte-for-byte the header `sips` writes for a real HEIC.
        assert!(is_heif(b"\0\0\0\x24ftypheic\0\0\0\0"));
        assert!(is_heif(b"\0\0\0\x18ftypmif1\0\0\0\0"));
        assert!(!is_heif(b"\0\0\0\x18ftypqt  \0\0\0\0"));
        assert!(!is_heif(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!is_heif(b"short"));
    }

    /// HEIC has to reach Ollama as JPEG; `sips` is the macOS converter.
    #[cfg(target_os = "macos")]
    #[test]
    fn heic_converts_to_jpeg() {
        let dir = std::env::temp_dir().join("accessengine-heif-test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("seed.png");
        let heic = dir.join("seed.heic");

        // A 2x2 PNG is enough for sips to transcode.
        std::fs::write(
            &png,
            [
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00,
                0x00, 0xFD, 0xD4, 0x9A, 0x73, 0x00, 0x00, 0x00, 0x13, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x9F, 0x81, 0x81, 0x01, 0x00, 0x0E, 0xB4, 0x02,
                0xF9, 0x1D, 0x53, 0xF1, 0x8D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
                0x42, 0x60, 0x82,
            ],
        )
        .unwrap();

        let ok = std::process::Command::new("sips")
            .args(["-s", "format", "heic"])
            .arg(&png)
            .arg("--out")
            .arg(&heic)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return; // No HEIC encoder on this machine; nothing to assert.
        }

        assert!(is_heif(&std::fs::read(&heic).unwrap()));
        let jpeg = heif_to_jpeg(&heic).expect("sips should convert HEIC to JPEG");
        assert!(looks_like_image(&jpeg), "converted bytes should sniff as an image");
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "should be JPEG");

        let _ = std::fs::remove_dir_all(&dir);
    }



    #[test]
    fn hemisphere_decides_the_sign() {
        // 52d29'10.32" = 52.4862
        assert!((dms_to_decimal(52.0, 29.0, 10.32, "N") - 52.4862).abs() < 1e-6);
        assert!((dms_to_decimal(52.0, 29.0, 10.32, "S") + 52.4862).abs() < 1e-6);
        assert!((dms_to_decimal(1.0, 53.0, 25.44, "W") + 1.8904).abs() < 1e-6);
        assert!((dms_to_decimal(1.0, 53.0, 25.44, "E") - 1.8904).abs() < 1e-6);
        // An absent or odd reference must not silently flip the sign.
        assert!(dms_to_decimal(10.0, 0.0, 0.0, "") > 0.0);
    }

    fn address(json: &str) -> Address {
        serde_json::from_str(json).expect("parses")
    }

    /// The name a person would give, not the whole hierarchy Nominatim returns.
    #[test]
    fn a_place_is_named_the_way_someone_would_say_it() {
        let birmingham = address(
            r#"{"suburb":"Moseley","city":"Birmingham","county":"West Midlands",
                "state":"England","country":"United Kingdom","postcode":"B13"}"#,
        );
        assert_eq!(
            place_name(&birmingham).as_deref(),
            Some("Moseley, Birmingham, United Kingdom")
        );

        // No settlement: the county stands in for one rather than being dropped.
        let moor = address(r#"{"county":"Devon","country":"United Kingdom"}"#);
        assert_eq!(
            place_name(&moor).as_deref(),
            Some("Devon, United Kingdom")
        );

        // A village is a settlement in its own right.
        let village = address(r#"{"village":"Clun","county":"Shropshire","country":"England"}"#);
        assert_eq!(place_name(&village).as_deref(), Some("Clun, England"));
    }

    /// The shape Nominatim actually returns for a Birmingham photograph, kept
    /// verbatim from a live reply so a change in what this app reads out of it
    /// shows up here rather than in someone's ears.
    #[test]
    fn a_real_reply_reads_as_a_place() {
        let birmingham = address(
            r#"{"hamlet":"Calthorpe Fields","village":"Park Central","city":"Birmingham",
                "state_district":"West Midlands","state":"England","postcode":"B15 1JB",
                "country":"United Kingdom","country_code":"gb"}"#,
        );
        assert_eq!(
            place_name(&birmingham).as_deref(),
            Some("Calthorpe Fields, Birmingham, United Kingdom")
        );
    }

    /// A city whose suburb shares its name must not be said twice.
    #[test]
    fn repeated_names_are_said_once() {
        let repeated = address(
            r#"{"suburb":"Luxembourg","city":"luxembourg","country":"Luxembourg"}"#,
        );
        assert_eq!(place_name(&repeated).as_deref(), Some("Luxembourg"));
    }

    /// Nothing usable in the reply is not an error, but it is not a place
    /// either — the caller says so rather than inventing one.
    #[test]
    fn an_empty_address_names_nowhere() {
        assert_eq!(place_name(&address("{}")), None);
        assert_eq!(place_name(&address(r#"{"country":"   "}"#)), None);
    }

    #[test]
    fn only_a_server_on_this_machine_may_be_started() {
        for url in [
            "http://localhost:11434",
            "http://127.0.0.1:11434/",
            "http://[::1]:11434",
            "localhost:11434",
        ] {
            assert!(is_local(url), "{url}");
        }
        for url in [
            "http://ollama.example.com:11434",
            "http://192.168.1.10:11434",
            "https://my-box.local",
            // The authority here is evil.example: everything before the `@` is
            // a username, however much it reads like a loopback address.
            "http://127.0.0.1:11434@evil.example/",
            "http://localhost@evil.example",
            "",
        ] {
            assert!(!is_local(url), "{url}");
        }
    }

    #[test]
    fn urls_are_normalised() {
        assert_eq!(normalise("http://localhost:11434/"), "http://localhost:11434");
        assert_eq!(normalise("  http://x:1  "), "http://x:1");
    }
}

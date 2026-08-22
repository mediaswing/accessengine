//! Talking to a local Ollama, and getting one installed if there isn't.
//!
//! Ollama is only needed for images, so nothing here runs until the user opens
//! a picture. Everything is blocking and expects to be called from a worker
//! thread, never from the UI thread.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const HOST: &str = "http://127.0.0.1:11434";

/// How far along the "can we actually read an image?" chain we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No `ollama` binary anywhere on PATH or in the usual install locations.
    NotInstalled,
    /// Installed, but the server isn't answering yet.
    NotRunning,
    Running,
}

/// Finds the `ollama` binary.
///
/// The usual install locations are checked explicitly as well as `PATH`,
/// because a GUI app launched from Finder or the Start menu inherits a minimal
/// environment that often doesn't include them. The lookup itself is
/// [`crate::sysexec::locate`], which refuses an answer nominated by the folder
/// this app happens to be sitting in.
pub fn binary_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let fallbacks = vec![
        std::env::var("LOCALAPPDATA")
            .map(|dir| format!("{dir}\\Programs\\Ollama\\ollama.exe"))
            .unwrap_or_default(),
        std::env::var("ProgramFiles")
            .map(|dir| format!("{dir}\\Ollama\\ollama.exe"))
            .unwrap_or_default(),
    ];
    #[cfg(not(target_os = "windows"))]
    let fallbacks = vec![
        "/opt/homebrew/bin/ollama".to_string(),
        "/usr/local/bin/ollama".to_string(),
        "/Applications/Ollama.app/Contents/Resources/ollama".to_string(),
    ];

    crate::sysexec::locate("ollama").or_else(|| {
        fallbacks
            .into_iter()
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from)
            .find(|path| path.exists())
    })
}

/// Builds a client for the local server. A zero `timeout` means "no limit",
/// which model downloads need.
fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        // The server is on loopback; a proxy would only get in the way.
        .no_proxy();
    if !timeout.is_zero() {
        builder = builder.timeout(timeout);
    }
    builder.build().context("could not create an HTTP client")
}

pub fn status() -> Status {
    if server_responds() {
        return Status::Running;
    }
    if binary_path().is_some() {
        Status::NotRunning
    } else {
        Status::NotInstalled
    }
}

fn server_responds() -> bool {
    let Ok(client) = client(Duration::from_millis(1500)) else {
        return false;
    };
    client
        .get(format!("{HOST}/api/version"))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Starts `ollama serve` in the background and waits for it to accept requests.
/// Doing nothing if it is already up makes this safe to call on every image.
pub fn ensure_running() -> Result<()> {
    if server_responds() {
        return Ok(());
    }
    let binary = binary_path().ok_or_else(|| anyhow!("Ollama is not installed"))?;
    let mut command = Command::new(&binary);
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Without this the server flashes up a console window in front of the app.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
        .spawn()
        .with_context(|| format!("could not start {}", binary.display()))?;

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if server_responds() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    bail!("Ollama was started but did not become ready within 30 seconds")
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
}

/// Names of the models already pulled, e.g. `llama3.2-vision:latest`.
pub fn installed_models() -> Result<Vec<String>> {
    let body: TagsResponse = client(Duration::from_secs(10))?
        .get(format!("{HOST}/api/tags"))
        .send()
        .context("could not reach the Ollama server")?
        .error_for_status()?
        .json()
        .context("the Ollama server returned an unexpected model list")?;
    Ok(body.models.into_iter().map(|m| m.name).collect())
}

/// True if `model` is present. Ollama reports `name:tag`, so a bare name given
/// by the user matches the `:latest` tag it would resolve to.
pub fn has_model(installed: &[String], model: &str) -> bool {
    let wanted = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    installed.iter().any(|m| m == &wanted)
}

#[derive(Deserialize)]
struct PullProgress {
    #[serde(default)]
    status: String,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Downloads a model, reporting progress lines as they stream in. Returns early
/// with an error if `cancel` is set, which leaves the partial download in place
/// for Ollama to resume next time.
pub fn pull_model(
    model: &str,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(String, Option<f32>),
) -> Result<()> {
    // No overall timeout: a vision model is several gigabytes.
    let response = client(Duration::from_secs(0))?
        .post(format!("{HOST}/api/pull"))
        .json(&serde_json::json!({ "model": model, "stream": true }))
        .send()
        .context("could not reach the Ollama server")?
        .error_for_status()?;

    for line in BufReader::new(response).lines() {
        if cancel.load(Ordering::Relaxed) {
            bail!("download cancelled");
        }
        let line = line.context("the download stream ended unexpectedly")?;
        if line.trim().is_empty() {
            continue;
        }
        let progress: PullProgress = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(error) = progress.error {
            bail!("Ollama could not download {model}: {error}");
        }
        let fraction = match (progress.completed, progress.total) {
            (Some(done), Some(total)) if total > 0 => Some(done as f32 / total as f32),
            _ => None,
        };
        on_progress(progress.status, fraction);
    }
    Ok(())
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    response: String,
    #[serde(default)]
    error: Option<String>,
    /// Why generation stopped. `"length"` means the context ran out — see
    /// [`VISION_CONTEXT_TOKENS`] for why that is worth knowing about.
    #[serde(default)]
    done_reason: Option<String>,
    /// Tokens the image and prompt cost, and tokens the answer took. Not used
    /// to decide anything — they go in the log, where they are the numbers that
    /// distinguish "the model read a clear image" from "the model was handed
    /// something it had to squeeze, and invented an answer".
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

/// How much context the vision call asks for.
///
/// Ollama's own default is 4096 tokens, and an image is not free: a 12-megapixel
/// photo becomes about 4095 of them all by itself. That leaves room for exactly
/// one token of answer, which the model duly produces — a description one
/// character long, spoken aloud as a syllable. Every photo a phone takes is over
/// that line, so the default is no default at all here.
///
/// Context costs memory in proportion, so this is not set higher than the job
/// needs: 8192 leaves a full-resolution photo about four thousand tokens to
/// answer in, which is a page of transcription.
const VISION_CONTEXT_TOKENS: u32 = 8192;

/// A vision model's answer.
pub struct Description {
    pub text: String,
    /// True if the model was cut off mid-answer by the context limit rather
    /// than finishing. What is here is then a fragment, not an answer.
    pub truncated: bool,
}

/// How much context the narration pass asks for.
///
/// Larger than the vision call because its input is larger: forty frame
/// descriptions is a few thousand tokens of prompt before the model writes a
/// word, and a narration cut off halfway is the failure this guards against.
const NARRATION_CONTEXT_TOKENS: u32 = 16384;

/// Sends one base64-encoded image to a vision model and returns its answer.
pub fn describe_image(model: &str, prompt: &str, image_base64: &str) -> Result<Description> {
    crate::log::line(format!(
        "ollama: asking {model} to read an image ({} KB encoded, prompt {} characters, num_ctx {VISION_CONTEXT_TOKENS})",
        image_base64.len() / 1024,
        prompt.chars().count()
    ));
    generate(
        model,
        serde_json::json!({
            "model": model,
            "prompt": prompt,
            "images": [image_base64],
            "stream": false,
            "options": { "num_ctx": VISION_CONTEXT_TOKENS },
        }),
        "reading the image",
    )
}

/// What a narration pass is working from, which is the only thing that differs
/// between the two callers of [`narrate`].
///
/// It exists because both of them end up describing themselves to somebody: in
/// the log, and — when Ollama fails — in the sentence the user is shown. A
/// capture summary that failed while "narrating the video" is a worse message
/// than no message, since it names a file the user did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Narration {
    /// Descriptions of the frames taken out of a video.
    Video,
    /// Figures counted out of a network capture.
    Capture,
}

impl Narration {
    /// What the model is being handed, for the log.
    fn material(self) -> &'static str {
        match self {
            Self::Video => "frame descriptions",
            Self::Capture => "counted capture figures",
        }
    }

    /// What it is doing, as it appears in a failure the user reads.
    fn doing(self) -> &'static str {
        match self {
            Self::Video => "narrating the video",
            Self::Capture => "summarising the capture",
        }
    }
}

/// Asks a model to rewrite what it is given as continuous prose.
///
/// No image goes with this one, which is why it can be answered by a text model
/// as easily as by the vision model that produced the descriptions — see
/// [`crate::config::Config::narration_model`].
pub fn narrate(model: &str, request: &str, about: Narration) -> Result<Description> {
    crate::log::line(format!(
        "ollama: asking {model} to narrate {} characters of {} (num_ctx {NARRATION_CONTEXT_TOKENS})",
        request.chars().count(),
        about.material()
    ));
    generate(
        model,
        serde_json::json!({
            "model": model,
            "prompt": request,
            "stream": false,
            "options": { "num_ctx": NARRATION_CONTEXT_TOKENS },
        }),
        about.doing(),
    )
}

/// The one call both of the above are: post to `/api/generate`, then read the
/// answer and the reasons it might not be one.
fn generate(model: &str, body: serde_json::Value, doing: &str) -> Result<Description> {
    // Models on CPU are slow; ten minutes is generous but finite.
    let response = client(Duration::from_secs(600))?
        .post(format!("{HOST}/api/generate"))
        .json(&body)
        .send()
        .context("could not reach the Ollama server")?;

    let status = response.status();
    let body: GenerateResponse = response
        .json()
        .context("the Ollama server returned an unexpected response")?;
    if let Some(error) = body.error {
        crate::log::line(format!("ollama: failed — {error}"));
        bail!("{}", explain_failure(model, &error, doing));
    }
    if !status.is_success() {
        crate::log::line(format!("ollama: HTTP {status}"));
        bail!("Ollama returned HTTP {status} while {doing}");
    }

    let answer = body.response.trim();
    crate::log::line(format!(
        "ollama: answered in {} characters — done_reason {}, prompt cost {} tokens, answer took {} tokens",
        answer.chars().count(),
        body.done_reason.as_deref().unwrap_or("none given"),
        describe_count(body.prompt_eval_count),
        describe_count(body.eval_count),
    ));
    // An empty answer is returned as-is rather than as an error: small models
    // sometimes go quiet on an elaborate prompt, and the caller retries with a
    // simpler one before giving up.
    Ok(Description {
        text: answer.to_string(),
        truncated: body.done_reason.as_deref() == Some("length"),
    })
}

fn describe_count(count: Option<u32>) -> String {
    count.map_or_else(|| "an unreported number of".to_string(), |n| n.to_string())
}

/// Below this, a cut-off answer is a fragment rather than a short description —
/// the failure this app actually saw was a single character. Above it there is
/// real content to read, even if the end is missing.
pub const FRAGMENT_CHARS: usize = 40;

/// What to say when a model runs out of room mid-answer. Only reachable now
/// with an image dense enough to fill 8192 tokens on its own, so the advice is
/// about the image rather than the setting.
pub fn explain_truncation(model: &str) -> String {
    format!(
        "“{model}” ran out of room to answer: this image alone fills the context it was \
         given, leaving nothing to describe it with. Try a smaller copy of the image, or \
         a model that takes a larger context."
    )
}

/// Turns an Ollama error into something a user can act on.
///
/// The one worth catching is a model that downloaded perfectly and then cannot
/// be loaded, because Ollama dropped the runner it was built for. Reported raw
/// it reads as a crash — `llama-server process has terminated: exit status 1` —
/// when in fact the fix is one dropdown away, so it is spelled out instead.
fn explain_failure(model: &str, error: &str, doing: &str) -> String {
    if error.contains("unknown model architecture") {
        return format!(
            "“{model}” cannot be run by the version of Ollama on this computer. It was \
             built for a model runner that Ollama has since removed, so no amount of \
             re-downloading will help. Choose a different vision model in Settings — the \
             suggested ones all work with current Ollama. To reclaim the disk space it \
             is using, run: ollama rm {model}"
        );
    }
    if error.contains("requires more system memory") || error.contains("available memory") {
        return format!(
            "“{model}” needs more memory than this computer has free. Choose a smaller \
             vision model in Settings, or close some other applications and try again. \
             Ollama said: {error}"
        );
    }
    format!("Ollama failed while {doing}: {error}")
}

/// What the app would run to install Ollama, or `None` if this machine has no
/// package manager it can drive. Shown to the user verbatim before anything
/// happens, because installing software is not something to do by surprise.
pub fn install_command() -> Option<String> {
    let installer = package_manager()?;
    Some(
        std::iter::once(installer.program.to_string_lossy().to_string())
            .chain(installer.args.iter().map(|arg| (*arg).to_string()))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Advice for when there is no package manager to drive.
#[cfg(target_os = "macos")]
pub fn manual_install_advice() -> String {
    crate::t!("install.advice.homebrew_missing.ollama")
}
#[cfg(not(target_os = "macos"))]
pub fn manual_install_advice() -> String {
    crate::t!("install.advice.no_manager.ollama")
}

/// A package manager and the arguments that install Ollama with it.
struct Installer {
    program: std::path::PathBuf,
    args: &'static [&'static str],
}

/// Homebrew on macOS, winget on Windows — the two that can install Ollama
/// without the user leaving the app.
fn package_manager() -> Option<Installer> {
    #[cfg(target_os = "windows")]
    {
        // winget ships with Windows 11 and recent Windows 10, but not with
        // every install, so its presence is checked rather than assumed. The
        // agreement flags are what stop it stopping for a prompt nobody can see.
        //
        // Found the same way `ollama` is, and it matters more here: this one is
        // handed a package name and told to install it.
        let path = crate::sysexec::locate("winget")?;
        Some(Installer {
            program: path,
            args: &[
                "install",
                "--id",
                "Ollama.Ollama",
                "--exact",
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(Installer {
            program: crate::homebrew::path()?,
            args: &["install", "ollama"],
        })
    }
}

/// Installs Ollama with whichever package manager this machine has, streaming
/// the output back a line at a time so the UI can show what a multi-minute
/// install is doing.
pub fn install(mut on_line: impl FnMut(String)) -> Result<()> {
    let installer = package_manager()
        .ok_or_else(|| anyhow!("there is no package manager here that can install Ollama"))?;

    let mut child = Command::new(&installer.program)
        .args(installer.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", installer.program.display()))?;

    // These tools write progress to stderr and results to stdout; both are
    // useful, and neither is worth losing.
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            BufReader::new(pipe)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            on_line(line);
        }
    }
    let status = child
        .wait()
        .context("the installer did not finish cleanly")?;
    if let Some(handle) = stderr
        && let Ok(lines) = handle.join()
    {
        for line in lines {
            on_line(line);
        }
    }

    if !status.success() {
        bail!(
            "installing Ollama failed (exit status {status}). Try installing it by hand \
             from https://ollama.com/download."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Narration, explain_failure, has_model};

    /// A video and a capture are narrated by the same call, so the only thing
    /// that can name the file the user actually opened is what the caller
    /// passed — and "Ollama failed while narrating the video" is a worse
    /// message than none at all on a `.pcap`.
    #[test]
    fn a_failure_names_the_work_that_was_actually_being_done() {
        let capture = explain_failure("llama3.2", "context canceled", Narration::Capture.doing());
        assert!(capture.contains("summarising the capture"), "{capture}");
        assert!(!capture.contains("video"), "{capture}");

        let video = explain_failure("llama3.2", "context canceled", Narration::Video.doing());
        assert!(video.contains("narrating the video"), "{video}");
        assert_ne!(capture, video);
    }

    #[test]
    fn a_dropped_architecture_is_explained_rather_than_repeated() {
        // Verbatim from Ollama 0.32 asked to load llama3.2-vision.
        let raw = "llama-server process has terminated: exit status 1: error loading model: \
                   unknown model architecture: 'mllama'";
        let message = explain_failure("llama3.2-vision", raw, "reading the image");

        assert!(message.contains("llama3.2-vision"));
        assert!(message.contains("Settings"), "no way out is offered");
        assert!(message.contains("ollama rm llama3.2-vision"));
        // The raw text is a dead end for a user, so it should not be the message.
        assert!(!message.contains("exit status 1"));
    }

    #[test]
    fn an_unrecognised_error_is_passed_through_intact() {
        let message = explain_failure(
            "qwen2.5vl:3b",
            "the image could not be decoded",
            "reading the image",
        );
        assert!(message.contains("the image could not be decoded"));
    }

    /// The same failures reach the narration pass, where a message about
    /// reading an image would be describing something that never happened.
    #[test]
    fn a_failure_says_which_step_it_happened_in() {
        let message = explain_failure("llama3.2", "context canceled", "narrating the video");
        assert!(message.contains("narrating the video"), "{message}");
        assert!(!message.contains("image"), "{message}");
    }

    #[test]
    fn bare_names_match_the_latest_tag() {
        let installed = vec![
            "llama3.2-vision:latest".to_string(),
            "llava:13b".to_string(),
        ];
        assert!(has_model(&installed, "llama3.2-vision"));
        assert!(has_model(&installed, "llama3.2-vision:latest"));
        assert!(has_model(&installed, "llava:13b"));
        assert!(!has_model(&installed, "llava"));
        assert!(!has_model(&installed, "qwen2.5vl"));
    }
}

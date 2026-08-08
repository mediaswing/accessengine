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
/// environment that often doesn't include them.
pub fn binary_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let (locator, fallbacks) = (
        "where",
        vec![
            std::env::var("LOCALAPPDATA")
                .map(|dir| format!("{dir}\\Programs\\Ollama\\ollama.exe"))
                .unwrap_or_default(),
            std::env::var("ProgramFiles")
                .map(|dir| format!("{dir}\\Ollama\\ollama.exe"))
                .unwrap_or_default(),
        ],
    );
    #[cfg(not(target_os = "windows"))]
    let (locator, fallbacks) = (
        "/usr/bin/which",
        vec![
            "/opt/homebrew/bin/ollama".to_string(),
            "/usr/local/bin/ollama".to_string(),
            "/Applications/Ollama.app/Contents/Resources/ollama".to_string(),
        ],
    );

    if let Ok(out) = Command::new(locator).arg("ollama").output()
        && out.status.success()
    {
        // `where` can report several matches, one per line; take the first.
        let found = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);
        if let Some(path) = found {
            return Some(path.into());
        }
    }
    fallbacks
        .into_iter()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .find(|path| path.exists())
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
}

/// Sends one base64-encoded image to a vision model and returns its answer.
pub fn describe_image(model: &str, prompt: &str, image_base64: &str) -> Result<String> {
    // Vision models on CPU are slow; ten minutes is generous but finite.
    let response = client(Duration::from_secs(600))?
        .post(format!("{HOST}/api/generate"))
        .json(&serde_json::json!({
            "model": model,
            "prompt": prompt,
            "images": [image_base64],
            "stream": false,
        }))
        .send()
        .context("could not reach the Ollama server")?;

    let status = response.status();
    let body: GenerateResponse = response
        .json()
        .context("the Ollama server returned an unexpected response")?;
    if let Some(error) = body.error {
        bail!("Ollama could not read the image: {error}");
    }
    if !status.is_success() {
        bail!("Ollama returned HTTP {status} while reading the image");
    }

    // An empty answer is returned as-is rather than as an error: small models
    // sometimes go quiet on an elaborate prompt, and the caller retries with a
    // simpler one before giving up.
    Ok(body.response.trim().to_string())
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
pub const MANUAL_INSTALL_ADVICE: &str = "Homebrew isn't installed either, so this can't be automated. Ollama can be \
     downloaded and installed by hand instead.";
#[cfg(not(target_os = "macos"))]
pub const MANUAL_INSTALL_ADVICE: &str = "There is no package manager here that the app can drive, so this can't be \
     automated. Ollama can be downloaded and installed by hand instead.";

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
        let found = Command::new("where")
            .arg("winget")
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let path = String::from_utf8_lossy(&found.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?
            .to_string();
        Some(Installer {
            program: path.into(),
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
        let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
            .iter()
            .map(std::path::PathBuf::from)
            .find(|path| path.exists())?;
        Some(Installer {
            program: brew,
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
    use super::has_model;

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

//! Startup check for a newer release on GitHub.
//!
//! Runs once in the background after launch. If a newer tagged release is
//! found, the UI shows a dialog pointing at the release page. This never
//! downloads or replaces the running binary: the Windows and Linux builds
//! are not code-signed, and self-replacing a binary that is currently
//! executing is its own source of bugs, so the only safe "auto update" here
//! is doing the checking automatically and leaving the download to the user.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::Duration;

const REPO: &str = "mediaswing/accessengine";
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
}

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
}

/// A single in-flight (or finished) check. `poll` is cheap to call every
/// frame, matching the pattern the vision worker uses.
pub struct UpdateChecker {
    pending: Option<Receiver<Option<UpdateInfo>>>,
}

impl UpdateChecker {
    /// Kick off the check in the background. `repaint` wakes the UI once the
    /// result is in, so the dialog does not wait for the next unrelated frame.
    pub fn start(repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("update-check".to_string())
            .spawn(move || {
                let result = match check() {
                    Ok(info) => info,
                    Err(e) => {
                        log::debug!("update check failed: {e:#}");
                        None
                    }
                };
                let _ = tx.send(result);
                repaint();
            });

        Self {
            pending: match spawned {
                Ok(_) => Some(rx),
                Err(e) => {
                    log::warn!("could not spawn the update-check thread: {e}");
                    None
                }
            },
        }
    }

    /// Non-blocking; returns the result once, the frame it arrives.
    pub fn poll(&mut self) -> Option<Option<UpdateInfo>> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                Some(None)
            }
        }
    }
}

fn check() -> Result<Option<UpdateInfo>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(CHECK_TIMEOUT)
        // GitHub's API rejects requests with no User-Agent header.
        .user_agent(concat!("AccessEngine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("requesting the latest release")?;

    if !response.status().is_success() {
        bail!("GitHub returned {}", response.status());
    }

    let release: Release = response.json().context("parsing the release response")?;
    let latest = release.tag_name.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");

    if is_newer(latest, current) {
        Ok(Some(UpdateInfo {
            version: latest.to_string(),
            url: release.html_url,
            notes: release.body,
        }))
    } else {
        Ok(None)
    }
}

/// Compare two `major.minor.patch` version strings. Anything that fails to
/// parse is treated as "not newer" rather than erroring, so a hand-edited or
/// pre-release tag on GitHub can never make this nag on every launch.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<(u64, u64, u64)> {
        let mut it = v.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        Some((major, minor, patch))
    }
    match (parts(latest), parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_compare_correctly() {
        assert!(is_newer("1.2.0", "1.1.0"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.0.9", "1.1.0"));
    }

    #[test]
    fn unparseable_versions_are_never_newer() {
        assert!(!is_newer("not-a-version", "1.1.0"));
        assert!(!is_newer("1.2.0-beta", "1.1.0"));
        assert!(!is_newer("1.2.0", "also-not-a-version"));
    }
}

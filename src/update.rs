//! Checking GitHub for a release newer than the one currently running.
//!
//! This only ever looks — nothing is downloaded or installed. Neither this
//! app's Windows build nor its macOS build is code-signed (see
//! `release.yml`), so there is no way to verify a downloaded binary before
//! running it; replacing the running app with one is a different, much
//! larger feature than "let the user know a newer one exists".

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

const RELEASES_LATEST: &str =
    "https://api.github.com/repos/mediaswing/accessengine/releases/latest";

/// A release newer than the one currently running.
pub struct Available {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
}

/// Checks GitHub's latest published release against the version this binary
/// was built as. Drafts and pre-releases are never returned by this endpoint,
/// so there is nothing here to filter out.
///
/// An `Err` covers only a genuine failure to check — no network, GitHub
/// unreachable, an unexpected response — and the caller treats it exactly
/// like "nothing newer" rather than showing it, since this is a nicety and
/// not something the rest of the app depends on.
pub fn check() -> Result<Option<Available>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        // The GitHub API rejects requests with no User-Agent at all.
        .user_agent(concat!("speech-output-engine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not create an HTTP client")?;

    let response = client
        .get(RELEASES_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("could not reach GitHub")?;

    // No release has been published yet; not an error, just nothing to
    // report.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release: Release = response
        .error_for_status()
        .context("GitHub returned an error")?
        .json()
        .context("GitHub returned an unexpected response")?;

    let latest = release.tag_name.trim_start_matches('v');
    Ok(
        is_newer(latest, env!("CARGO_PKG_VERSION")).then(|| Available {
            version: latest.to_string(),
            url: release.html_url,
        }),
    )
}

/// Compares two version strings, tolerantly: a tag that doesn't parse as
/// semver — on either side — means "cannot compare", not "must be newer", so
/// a malformed tag can never nag the user.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn a_higher_version_is_newer() {
        assert!(is_newer("1.2.0", "1.1.0"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(is_newer("1.1.1", "1.1.0"));
    }

    #[test]
    fn the_same_or_an_older_version_is_not_newer() {
        assert!(!is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.0.0", "1.1.0"));
    }

    #[test]
    fn digit_counts_are_compared_numerically_not_lexically() {
        // A naive string comparison would put "1.9.0" after "1.10.0".
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.9.0", "1.10.0"));
    }

    #[test]
    fn an_unparseable_tag_is_never_treated_as_newer() {
        assert!(!is_newer("not-a-version", "1.1.0"));
        assert!(!is_newer("1.2.0", "not-a-version"));
    }
}

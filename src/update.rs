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

/// Where "Download Release" goes when nothing usable came back — the only URL
/// here that is not out of a JSON response.
const RELEASES_PAGE: &str = "https://github.com/mediaswing/accessengine/releases";

/// A release newer than the one currently running.
pub struct Available {
    pub version: String,
    /// The release's notes, as written to the GitHub release body — shown to
    /// the user as the changelog.
    pub notes: String,
    /// Where "Download Release" goes: the zip built for this platform, or
    /// the release page itself if no matching asset was found there.
    pub download_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
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
            notes: release.body.unwrap_or_default(),
            download_url: pick_download_url(
                &release.assets,
                platform_asset_prefix(),
                &release.html_url,
            ),
        }),
    )
}

/// The prefix `release.yml` gives the zip it builds for this platform, or
/// `None` on a platform the workflow does not build for (there is no
/// download to point at, so the release page is as close as it gets).
fn platform_asset_prefix() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("accessengine-macos")
    } else if cfg!(target_os = "windows") {
        Some("accessengine-windows")
    } else {
        None
    }
}

/// The direct download for this platform's build, falling back to the
/// release page itself when there is no asset to match against — an
/// unrecognized platform, or a release published without the usual zips.
///
/// Both candidates come out of a JSON response and the winner is handed to
/// `ctx.open_url`, which is the operating system's opener and will follow
/// whatever scheme it is given. Every other URL this app opens is a constant
/// compiled into the binary; this is the one that arrives over the wire, so it
/// is held to `https` and falls through to [`RELEASES_PAGE`] — which is not —
/// if neither candidate is.
fn pick_download_url(assets: &[Asset], platform_prefix: Option<&str>, html_url: &str) -> String {
    let asset = platform_prefix
        .and_then(|prefix| assets.iter().find(|asset| asset.name.starts_with(prefix)))
        .map(|asset| asset.browser_download_url.as_str());

    [asset, Some(html_url)]
        .into_iter()
        .flatten()
        .find(|url| url.starts_with("https://"))
        .unwrap_or(RELEASES_PAGE)
        .to_string()
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
    use super::{Asset, is_newer, pick_download_url};

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn the_asset_matching_this_platform_is_preferred() {
        let assets = vec![
            asset("accessengine-macos-aarch64.zip"),
            asset("accessengine-windows-x86_64.zip"),
        ];
        assert_eq!(
            pick_download_url(
                &assets,
                Some("accessengine-windows"),
                "https://example.com/release"
            ),
            "https://example.com/accessengine-windows-x86_64.zip"
        );
    }

    #[test]
    fn the_release_page_is_used_when_no_asset_matches() {
        let assets = vec![asset("accessengine-macos-aarch64.zip")];
        assert_eq!(
            pick_download_url(
                &assets,
                Some("accessengine-windows"),
                "https://example.com/release"
            ),
            "https://example.com/release"
        );
    }

    #[test]
    fn the_release_page_is_used_on_an_unrecognized_platform() {
        let assets = vec![asset("accessengine-macos-aarch64.zip")];
        assert_eq!(
            pick_download_url(&assets, None, "https://example.com/release"),
            "https://example.com/release"
        );
    }

    /// The URL goes to the system opener, so a scheme other than `https` is
    /// refused rather than followed — whichever of the two candidates carries
    /// it, and even when that leaves nothing from the response to use.
    #[test]
    fn a_url_that_is_not_https_is_never_opened() {
        let hostile = |url: &str| Asset {
            name: "accessengine-macos-aarch64.zip".to_string(),
            browser_download_url: url.to_string(),
        };
        for scheme in [
            "javascript:alert(1)",
            "file:///Applications/Calculator.app",
            "http://example.com/accessengine.zip",
        ] {
            assert_eq!(
                pick_download_url(
                    &[hostile(scheme)],
                    Some("accessengine-macos"),
                    "https://example.com/release"
                ),
                "https://example.com/release",
                "{scheme} was passed through"
            );
            // And with the release page itself no better, nothing from the
            // response is used at all.
            assert_eq!(
                pick_download_url(&[hostile(scheme)], Some("accessengine-macos"), scheme),
                super::RELEASES_PAGE,
                "{scheme} was passed through as the fallback"
            );
        }
    }

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

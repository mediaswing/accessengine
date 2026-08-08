//! Reading images by way of a local vision model.
//!
//! Ollama's API takes base64 image data. JPEG and PNG go straight through;
//! HEIF/HEIC — what an iPhone photo actually is — is converted to JPEG first
//! using macOS's own `sips`, since vision models generally can't decode it.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::path::Path;

/// Refuse anything implausibly large before spending time encoding it. Vision
/// models downscale aggressively anyway, so a huge file buys nothing.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Loads an image as base64, converting to JPEG if the format needs it.
pub fn encode_for_ollama(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?
        .len();
    if size > MAX_IMAGE_BYTES {
        bail!(
            "that image is {:.0} MB, which is too large to send to a vision model",
            size as f64 / (1024.0 * 1024.0)
        );
    }

    let bytes = if needs_conversion(path) {
        convert_to_jpeg(path)?
    } else {
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?
    };
    Ok(BASE64.encode(bytes))
}

fn needs_conversion(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif"))
        .unwrap_or(false)
}

/// Converts to JPEG with `sips`, which ships with macOS and handles HEIF via
/// the same system codecs Preview uses.
///
/// Windows has no equivalent that is guaranteed to be present — its own HEIF
/// support is an optional Store extension — so rather than half-work, that path
/// says plainly what to do instead.
fn convert_to_jpeg(path: &Path) -> Result<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        bail!(
            "HEIC and HEIF photos can only be converted on macOS. Save this one as a \
             JPEG or PNG first, then open it again."
        );
    }

    let mut destination = std::env::temp_dir();
    destination.push(format!(
        "speech-output-engine-{}-{}.jpg",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));

    let output = std::process::Command::new("/usr/bin/sips")
        .arg("--setProperty")
        .arg("format")
        .arg("jpeg")
        .arg(path)
        .arg("--out")
        .arg(&destination)
        .output()
        .context("could not run sips to convert the image")?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&destination);
        bail!(
            "could not convert {} to JPEG: {}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let bytes =
        std::fs::read(&destination).context("the converted image could not be read back")?;
    let _ = std::fs::remove_file(&destination);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::needs_conversion;
    use std::path::PathBuf;

    #[test]
    fn only_heif_family_is_converted() {
        assert!(needs_conversion(&PathBuf::from("IMG_0001.HEIC")));
        assert!(needs_conversion(&PathBuf::from("photo.heif")));
        assert!(!needs_conversion(&PathBuf::from("photo.jpg")));
        assert!(!needs_conversion(&PathBuf::from("scan.png")));
    }
}

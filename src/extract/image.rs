//! Reading images by way of a local vision model.
//!
//! Ollama's API takes base64 image data. JPEG and PNG go straight through;
//! HEIF/HEIC — what an iPhone photo actually is — is converted to JPEG first
//! using macOS's own `sips`, since vision models generally can't decode it.
//!
//! Along the way, the same bytes are checked for an embedded GPS position —
//! most cameras and phones write one to EXIF unless location tagging was
//! turned off. See [`crate::geocode`] for what becomes of it.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::path::Path;

/// Refuse anything implausibly large before spending time encoding it. Vision
/// models downscale aggressively anyway, so a huge file buys nothing.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// A coordinate read out of a photo's EXIF data, in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsLocation {
    pub latitude: f64,
    pub longitude: f64,
}

/// An image ready to send to Ollama, and wherever EXIF says it was taken.
pub struct EncodedImage {
    pub base64: String,
    pub location: Option<GpsLocation>,
}

/// Loads an image as base64, converting to JPEG if the format needs it, and
/// pulls out its GPS position if it has one.
pub fn encode_for_ollama(path: &Path) -> Result<EncodedImage> {
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
    let location = gps_location(&bytes);
    Ok(EncodedImage {
        base64: BASE64.encode(bytes),
        location,
    })
}

/// Reads a GPS position out of EXIF, if the image has one. Absent EXIF, an
/// unrecognised container, or a photo with location tagging turned off are
/// all just `None` — this is a bonus, not something the read should fail
/// over.
fn gps_location(bytes: &[u8]) -> Option<GpsLocation> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    Some(GpsLocation {
        latitude: coordinate(
            &exif,
            exif::Tag::GPSLatitude,
            exif::Tag::GPSLatitudeRef,
            b'S',
        )?,
        longitude: coordinate(
            &exif,
            exif::Tag::GPSLongitude,
            exif::Tag::GPSLongitudeRef,
            b'W',
        )?,
    })
}

/// One half of a coordinate: the `value_tag` gives degrees/minutes/seconds,
/// the `ref_tag` gives the hemisphere. `negative` is the reference letter
/// ('S' or 'W') that flips the sign — EXIF stores both always positive.
fn coordinate(
    exif: &exif::Exif,
    value_tag: exif::Tag,
    ref_tag: exif::Tag,
    negative: u8,
) -> Option<f64> {
    let exif::Value::Rational(parts) = &exif.get_field(value_tag, exif::In::PRIMARY)?.value else {
        return None;
    };
    let degrees = dms_to_decimal(parts)?;

    let exif::Value::Ascii(refs) = &exif.get_field(ref_tag, exif::In::PRIMARY)?.value else {
        return None;
    };
    let is_negative = refs.first()?.first()?.to_ascii_uppercase() == negative;
    Some(if is_negative { -degrees } else { degrees })
}

/// EXIF gives degrees/minutes/seconds as up to three rationals; some cameras
/// omit the seconds, or even the minutes, so all three lengths are accepted.
fn dms_to_decimal(parts: &[exif::Rational]) -> Option<f64> {
    match parts {
        [deg] => Some(deg.to_f64()),
        [deg, min] => Some(deg.to_f64() + min.to_f64() / 60.0),
        [deg, min, sec] => Some(deg.to_f64() + min.to_f64() / 60.0 + sec.to_f64() / 3600.0),
        _ => None,
    }
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

    // An absolute path, for two reasons. `sips` takes the input as a positional
    // argument, so a file actually named `-h` — or anything else beginning with
    // a dash — would be read as an option instead of a filename; a path from
    // `canonicalize` always starts with `/` and can never be mistaken for one.
    // It also pins down exactly which file is converted, rather than leaving it
    // to whatever the working directory happens to be by then.
    let source = std::fs::canonicalize(path)
        .with_context(|| format!("could not locate {}", path.display()))?;

    // Claimed rather than merely named: creating it exclusively means the path
    // is not already a symlink pointing somewhere sips would then write. sips
    // replaces the file rather than writing into the handle, so this is about
    // owning the name, not the descriptor. See `crate::sysexec`.
    let (_claim, destination) = crate::sysexec::create_scratch_file("accessengine-image", "jpg")?;

    let output = std::process::Command::new("/usr/bin/sips")
        .arg("--setProperty")
        .arg("format")
        .arg("jpeg")
        .arg(&source)
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

    // Conversion only happens on macOS, so the fixture and the tests that use
    // it are compiled there and nowhere else.
    #[cfg(target_os = "macos")]
    use super::encode_for_ollama;

    /// A 2×2 greyscale PNG, small enough to write inline.
    #[cfg(target_os = "macos")]
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x57,
        0xDD, 0x52, 0xF8, 0x00, 0x00, 0x00, 0x0F, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0x60,
        0x60, 0x60, 0xF8, 0x0F, 0x00, 0x01, 0x04, 0x01, 0x00, 0x2B, 0xB3, 0x0A, 0x1B, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// The real HEIC path, end to end: canonicalise, claim a scratch file, run
    /// sips, read the JPEG back. Skipped if this machine's sips cannot write
    /// HEIC, since that would be testing the runner rather than the code.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_heic_photo_is_converted_and_encoded() {
        use base64::Engine as _;

        let dir = std::env::temp_dir();
        let png = dir.join("soe-image-test.png");
        let heic = dir.join("soe-image-test.heic");
        std::fs::write(&png, TINY_PNG).unwrap();

        let made = std::process::Command::new("/usr/bin/sips")
            .args(["--setProperty", "format", "heic"])
            .arg(&png)
            .arg("--out")
            .arg(&heic)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        std::fs::remove_file(&png).ok();
        if !made || !heic.exists() {
            eprintln!("sips cannot write HEIC here; skipping");
            return;
        }

        let encoded = encode_for_ollama(&heic).expect("a HEIC photo should convert");
        std::fs::remove_file(&heic).ok();

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded.base64)
            .expect("the result should be base64");
        // JPEG's magic number: what reaches Ollama is no longer HEIC.
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "not a JPEG");
    }

    /// A leading dash is a filename, not an option — `sips` takes the input
    /// positionally, so a path that reaches it unqualified would be parsed as
    /// a flag. `canonicalize` is what keeps that from being possible.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_filename_that_looks_like_an_option_is_still_treated_as_a_file() {
        let path = std::env::temp_dir().join("--setProperty.png");
        std::fs::write(&path, TINY_PNG).unwrap();

        let absolute = std::fs::canonicalize(&path).unwrap();
        assert!(absolute.is_absolute());
        assert!(
            !absolute.to_string_lossy().starts_with('-'),
            "the path sips receives must not begin with a dash"
        );

        // PNG needs no conversion, but it must still be read rather than refused.
        assert!(encode_for_ollama(&path).is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn only_heif_family_is_converted() {
        assert!(needs_conversion(&PathBuf::from("IMG_0001.HEIC")));
        assert!(needs_conversion(&PathBuf::from("photo.heif")));
        assert!(!needs_conversion(&PathBuf::from("photo.jpg")));
        assert!(!needs_conversion(&PathBuf::from("scan.png")));
    }
}

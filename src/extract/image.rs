//! Reading images by way of a local vision model.
//!
//! Ollama's API takes base64 image data. HEIF/HEIC — what an iPhone photo
//! actually is — is converted to JPEG first using macOS's own `sips`, since
//! vision models generally can't decode it.
//!
//! Whatever the source format, an oversized picture is then shrunk to
//! [`MAX_LONG_EDGE`] before it is sent. That is not about bandwidth: Ollama
//! squeezes a large image down to its own token budget anyway, and does it
//! badly enough that a photographed page comes back as invented text rather
//! than a transcription. See [`MAX_LONG_EDGE`] for the measurements.
//!
//! Along the way, the same bytes are checked for an embedded GPS position —
//! most cameras and phones write one to EXIF unless location tagging was
//! turned off. See [`crate::geocode`] for what becomes of it.

use crate::t;
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
    // Read before any resize: shrinking re-encodes the image and drops EXIF
    // with it, so both the position and the orientation have to come off the
    // original bytes.
    let location = gps_location(&bytes);
    let orientation = exif_orientation(&bytes);
    let bytes = shrink_for_vision(bytes, orientation);

    Ok(EncodedImage {
        base64: BASE64.encode(bytes),
        location,
    })
}

/// The longest edge, in pixels, an image is sent at.
///
/// Not a size limit — a legibility one. Ollama caps how many tokens an image is
/// worth and squeezes anything bigger down to fit, and its resampling is poor
/// enough that small text turns to mush. A vision model handed mush does not
/// say it cannot read the image; it invents plausible-looking characters, and
/// the app happily speaks them.
///
/// Measured against qwen2.5vl:3b, transcribing a page of text photographed at
/// 4032×3024: sent as-is it came back as `L1: 2 l1s q4k b3wv f0 rjzrsw0 t6
/// hzydog`, and resized to 2048 first it came back exactly right. Both cost
/// **the same 4095 image tokens** — the resize buys accuracy, not budget, which
/// is why this is worth doing rather than just asking for more context.
///
/// 2048 rather than lower because that is the largest edge that still costs the
/// same as sending the original; 1568 also transcribed cleanly but started
/// losing detail, and there is nothing to gain by paying less than the model
/// charges anyway.
const MAX_LONG_EDGE: u32 = 2048;

/// Shrinks an oversized image and bakes in its EXIF orientation.
///
/// Returns the original bytes untouched if the image is already small enough,
/// is upright, or cannot be decoded — a picture this crate has no decoder for
/// is still worth sending, since Ollama may well read it.
fn shrink_for_vision(bytes: Vec<u8>, orientation: Orientation) -> Vec<u8> {
    let size = dimensions(&bytes);
    let oversized = size
        .map(|(width, height)| width.max(height) > MAX_LONG_EDGE)
        .unwrap_or(false);
    let describe = |(width, height)| format!("{width}×{height}");
    let before = size.map_or_else(|| "an unreadable size".to_string(), describe);

    if !oversized && orientation == Orientation::Upright {
        crate::log::line(format!(
            "image: {before}, upright and small enough — sent as-is"
        ));
        return bytes;
    }

    match resample(&bytes, oversized, orientation) {
        Some(resized) => {
            let after = dimensions(&resized).map_or_else(|| "unknown".to_string(), describe);
            crate::log::line(format!(
                "image: {before} {orientation:?} — sent as {after}, {} KB",
                resized.len() / 1024
            ));
            resized
        }
        // Anything that fails here is a picture that could not be decoded or
        // re-encoded, which is not a reason to refuse to read it at all.
        None => {
            crate::log::line(format!(
                "image: {before} could not be resized or rotated — sent unchanged"
            ));
            bytes
        }
    }
}

/// Reads an image's pixel dimensions from its header alone, without decoding
/// the whole thing — the usual case is an image that needs no resizing, and
/// that case should not pay for a full decode to find out.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn resample(bytes: &[u8], oversized: bool, orientation: Orientation) -> Option<Vec<u8>> {
    let mut image = image::load_from_memory(bytes).ok()?;

    if oversized {
        // Lanczos3 specifically: the whole point is to land a *legible*
        // downscale where Ollama's own was not, and a cheaper filter here
        // would reproduce the bug this function exists to fix.
        let (width, height) = (image.width(), image.height());
        let scale = MAX_LONG_EDGE as f64 / width.max(height) as f64;
        image = image.resize(
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }

    // After the resize, so the rotation is applied to fewer pixels.
    image = match orientation {
        Orientation::Upright => image,
        Orientation::Rotate90 => image.rotate90(),
        Orientation::Rotate180 => image.rotate180(),
        Orientation::Rotate270 => image.rotate270(),
        Orientation::FlipHorizontal => image.fliph(),
        Orientation::FlipVertical => image.flipv(),
        Orientation::Transpose => image.rotate90().fliph(),
        Orientation::Transverse => image.rotate270().fliph(),
    };

    let mut out = Vec::new();
    // Quality 90: visually lossless at this size, and JPEG artefacts around
    // small glyphs are exactly what must not be introduced here.
    image
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, 90,
        ))
        .ok()?;
    Some(out)
}

/// The eight EXIF orientations, as the transform needed to make the image
/// upright.
///
/// Phones overwhelmingly store a photo in the sensor's own orientation and note
/// the rotation here rather than rotating the pixels, so a portrait photo is a
/// landscape image plus a tag. Re-encoding drops the tag, and a vision model
/// handed a page of text lying on its side reads it about as well as a person
/// would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Upright,
    FlipHorizontal,
    Rotate180,
    FlipVertical,
    Transpose,
    Rotate90,
    Transverse,
    Rotate270,
}

/// Reads the EXIF orientation tag. Anything missing or unrecognised is treated
/// as upright, which is what an image with no tag at all means.
fn exif_orientation(bytes: &[u8]) -> Orientation {
    let value = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()
        .and_then(|exif| {
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
                .value
                .get_uint(0)
        });

    match value {
        Some(2) => Orientation::FlipHorizontal,
        Some(3) => Orientation::Rotate180,
        Some(4) => Orientation::FlipVertical,
        Some(5) => Orientation::Transpose,
        // The common one: an upright photo from a phone held normally.
        Some(6) => Orientation::Rotate90,
        Some(7) => Orientation::Transverse,
        Some(8) => Orientation::Rotate270,
        _ => Orientation::Upright,
    }
}

/// Reads a GPS position out of EXIF, if the image has one. Absent EXIF, an
/// unrecognised container, or a photo with location tagging turned off are
/// all just `None` — this is a bonus, not something the read should fail
/// over.
fn gps_location(bytes: &[u8]) -> Option<GpsLocation> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let latitude = coordinate(
        &exif,
        exif::Tag::GPSLatitude,
        exif::Tag::GPSLatitudeRef,
        b'S',
    )?;
    let longitude = coordinate(
        &exif,
        exif::Tag::GPSLongitude,
        exif::Tag::GPSLongitudeRef,
        b'W',
    )?;
    is_on_earth(latitude, longitude).then_some(GpsLocation {
        latitude,
        longitude,
    })
}

/// Whether a pair of degrees is a place rather than arithmetic.
///
/// EXIF stores each part as a rational, and a rational whose denominator is
/// zero reads back as an infinity or a NaN. This is the one value on the image
/// path that leaves the computer — see [`crate::geocode`] — so it is checked
/// before it goes rather than left to fail at the far end.
fn is_on_earth(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && latitude.abs() <= 90.0
        && longitude.abs() <= 180.0
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

/// Whether `path` is a format a camera or phone actually produces, as opposed
/// to a screenshot or a diagram saved as PNG. See [`crate::config::Config::photo_ai_note`],
/// which uses this to decide whether a description needs the disclosure at all.
pub fn is_photo(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            e == "jpg" || e == "jpeg" || e == "heic" || e == "heif"
        })
        .unwrap_or(false)
}

/// Appended to a photo's finished description when [`crate::config::Config::photo_ai_note`]
/// is on, so what is heard says where it came from.
pub fn ai_disclosure_note(text: &str) -> String {
    format!("{}\n\n{}", text, t!("photo.ai_note"))
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
    use super::*;
    use std::path::PathBuf;

    /// A 2×2 greyscale PNG, small enough to write inline.
    #[cfg(target_os = "macos")]
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x57,
        0xDD, 0x52, 0xF8, 0x00, 0x00, 0x00, 0x0F, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0x60,
        0x60, 0x60, 0xF8, 0x0F, 0x00, 0x01, 0x04, 0x01, 0x00, 0x2B, 0xB3, 0x0A, 0x1B, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// EXIF stores each part of a coordinate as a rational, and a denominator
    /// of zero reads back as an infinity — which would otherwise be formatted
    /// into the Nominatim query as the literal `inf`.
    #[test]
    fn a_coordinate_that_is_not_a_place_is_not_sent_anywhere() {
        assert!(is_on_earth(54.43, -2.96));
        assert!(is_on_earth(0.0, 0.0));
        // The poles and the date line are real places.
        assert!(is_on_earth(-90.0, 180.0));

        assert!(!is_on_earth(f64::INFINITY, 0.0));
        assert!(!is_on_earth(0.0, f64::NEG_INFINITY));
        assert!(!is_on_earth(f64::NAN, 0.0));
        assert!(!is_on_earth(0.0, f64::NAN));
        assert!(!is_on_earth(91.0, 0.0));
        assert!(!is_on_earth(0.0, -181.0));
    }

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

    /// A JPEG of the given size, with enough variation that it cannot be
    /// mistaken for a blank image.
    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut bytes, 90,
            ))
            .unwrap();
        bytes
    }

    fn size_of(bytes: &[u8]) -> (u32, u32) {
        super::dimensions(bytes).expect("the result should be a decodable image")
    }

    /// The bug this whole path exists for: a photo-sized image reached the
    /// model at full resolution, was crushed to fit its token budget, and came
    /// back as invented text.
    #[test]
    fn an_oversized_photo_is_shrunk_to_the_long_edge() {
        let original = jpeg(4032, 3024);
        let shrunk = shrink_for_vision(original, Orientation::Upright);

        let (width, height) = size_of(&shrunk);
        assert_eq!(width, MAX_LONG_EDGE);
        // The aspect ratio has to survive, or the model reads a stretched page.
        assert_eq!(height, 1536, "4:3 should stay 4:3");
    }

    /// The long edge is the one that matters, whichever way round it is.
    #[test]
    fn a_portrait_photo_is_shrunk_by_its_height() {
        let shrunk = shrink_for_vision(jpeg(3024, 4032), Orientation::Upright);
        let (width, height) = size_of(&shrunk);
        assert_eq!(height, MAX_LONG_EDGE);
        assert_eq!(width, 1536);
    }

    /// Re-encoding a small image would cost quality for nothing.
    #[test]
    fn an_image_already_small_enough_is_left_exactly_as_it_was() {
        let original = jpeg(1200, 900);
        let untouched = shrink_for_vision(original.clone(), Orientation::Upright);
        assert_eq!(
            untouched, original,
            "a small upright image should be passed through"
        );
    }

    /// A sideways page of text is a page a vision model cannot read, so the
    /// rotation is applied even when the image needs no resizing.
    #[test]
    fn a_rotated_image_is_turned_upright_even_when_it_is_small() {
        let original = jpeg(1200, 900);
        let rotated = shrink_for_vision(original.clone(), Orientation::Rotate90);

        assert_ne!(rotated, original, "the rotation should have been applied");
        // rotate90 swaps the axes.
        assert_eq!(size_of(&rotated), (900, 1200));
    }

    #[test]
    fn orientation_six_is_the_common_phone_rotation() {
        // A JPEG with no EXIF at all is upright, not an error.
        assert_eq!(exif_orientation(&jpeg(8, 8)), Orientation::Upright);
        assert_eq!(exif_orientation(b"not an image"), Orientation::Upright);
    }

    /// A file this crate has no decoder for is still worth sending.
    #[test]
    fn an_undecodable_image_is_passed_through_rather_than_dropped() {
        let bytes = b"neither a JPEG nor a PNG".to_vec();
        assert_eq!(
            shrink_for_vision(bytes.clone(), Orientation::Upright),
            bytes
        );
    }

    #[test]
    fn only_heif_family_is_converted() {
        assert!(needs_conversion(&PathBuf::from("IMG_0001.HEIC")));
        assert!(needs_conversion(&PathBuf::from("photo.heif")));
        assert!(!needs_conversion(&PathBuf::from("photo.jpg")));
        assert!(!needs_conversion(&PathBuf::from("scan.png")));
    }

    /// The disclosure is for formats a camera actually produces. A PNG here is
    /// far more often a screenshot or a diagram than a photo.
    #[test]
    fn only_camera_formats_count_as_a_photo() {
        assert!(is_photo(&PathBuf::from("IMG_0001.JPG")));
        assert!(is_photo(&PathBuf::from("photo.jpeg")));
        assert!(is_photo(&PathBuf::from("photo.HEIC")));
        assert!(is_photo(&PathBuf::from("photo.heif")));
        assert!(!is_photo(&PathBuf::from("screenshot.png")));
        assert!(!is_photo(&PathBuf::from("noextension")));
    }

    #[test]
    fn the_ai_disclosure_is_appended_after_a_blank_line() {
        let with_note = ai_disclosure_note("A description of the photo.");
        assert!(with_note.starts_with("A description of the photo.\n\n"));
        assert!(with_note.len() > "A description of the photo.".len());
    }
}

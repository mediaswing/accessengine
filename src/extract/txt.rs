//! Plain-text reading, tolerant of the encodings text files turn up in.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// The most text the app will take in one file. A file is read whole into
/// memory — decoding UTF-16 and normalising blank runs both need it — so
/// without a ceiling a mistyped path to a disk image is an allocation failure
/// rather than a message. Far past any document a person would sit and listen
/// to: this is roughly two hundred novels.
const MAX_TEXT_BYTES: u64 = 64 * 1024 * 1024;

pub fn extract(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?
        .len();
    if size > MAX_TEXT_BYTES {
        bail!(
            "{} is {:.0} MB, which is more text than this app will read at once",
            path.file_name().unwrap_or_default().to_string_lossy(),
            size as f64 / (1024.0 * 1024.0)
        );
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(super::tidy(&decode(&bytes)))
}

/// Decodes a byte buffer to text. UTF-16 files (common when Windows tools write
/// "Unicode" text) are detected by their byte-order mark; anything else that
/// is valid UTF-8 is taken as UTF-8. Failing that, the bytes are read as
/// Windows-1252 rather than lossy UTF-8 — see [`decode_windows_1252`] for why.
pub(super) fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => decode_windows_1252(bytes),
        },
    }
}

fn decode_utf16(bytes: &[u8], to_unit: fn([u8; 2]) -> u16) -> String {
    // A trailing odd byte is not half a character, and is dropped — which is
    // what the remainder of `as_chunks` is and why it is discarded here.
    let (pairs, _odd_byte) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().copied().map(to_unit).collect();
    String::from_utf16_lossy(&units)
}

/// Decodes as Windows-1252, the fallback for a file that isn't valid UTF-8 and
/// carries no BOM.
///
/// This is what Windows still calls "ANSI": Excel's plain "CSV (Comma
/// delimited)" export writes it rather than UTF-8 unless "CSV UTF-8" is
/// chosen instead, and Notepad defaulted to it until Windows 10's 1809
/// update. Read as UTF-8, a file like that isn't rejected — invalid byte
/// sequences never are — it comes back with every accented letter turned into
/// one or more replacement characters instead, silently, since bytes above
/// 0x7F almost never happen to also form valid UTF-8 by chance.
///
/// Windows-1252 agrees with ASCII below 0x80 and with Latin-1 from 0xA0 up;
/// only 0x80–0x9F differ, mostly the typographic punctuation — curly quotes,
/// an em dash — that this fallback exists to stop losing.
fn decode_windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => HIGH[(byte - 0x80) as usize],
            _ => byte as char,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn strips_utf8_bom() {
        assert_eq!(decode(b"\xEF\xBB\xBFhello"), "hello");
    }

    #[test]
    fn reads_both_utf16_byte_orders() {
        assert_eq!(decode(b"\xFF\xFEh\0i\0"), "hi");
        assert_eq!(decode(b"\xFE\xFF\0h\0i"), "hi");
    }

    #[test]
    fn valid_utf8_is_read_as_utf8_even_with_high_bytes() {
        // "café" — 0xC3 0xA9 is "é" in UTF-8, and must not be reinterpreted as
        // two separate Windows-1252 characters.
        assert_eq!(decode("café".as_bytes()), "café");
    }

    /// The bug this exists for: a file that isn't valid UTF-8 used to come
    /// back with every non-ASCII byte turned into a replacement character —
    /// silently, since decoding never fails. Excel's plain "CSV (Comma
    /// delimited)" export and pre-2019 Notepad both write Windows-1252, not
    /// UTF-8, so this is the ordinary case for a name or address with an
    /// accent in it, not an edge case.
    #[test]
    fn invalid_utf8_falls_back_to_windows_1252_instead_of_replacing() {
        // "café" as Windows-1252: é is the single byte 0xE9.
        assert_eq!(decode(b"caf\xE9"), "café");
        // "José" the same way.
        assert_eq!(decode(b"Jos\xE9"), "José");
    }

    #[test]
    fn windows_1252_typographic_punctuation_is_recognised() {
        // A right single quotation mark and an em dash — the two characters
        // "smart quotes" pasted from a word processor most often turn into.
        assert_eq!(decode(b"it\x92s \x97 fine"), "it\u{2019}s \u{2014} fine");
    }
}

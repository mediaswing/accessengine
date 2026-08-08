//! Plain-text reading, tolerant of the encodings text files turn up in.

use anyhow::{Context, Result};
use std::path::Path;

pub fn extract(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(super::tidy(&decode(&bytes)))
}

/// Decodes a byte buffer to text. UTF-16 files (common when Windows tools write
/// "Unicode" text) are detected by their byte-order mark; everything else is
/// treated as UTF-8, with invalid sequences replaced rather than rejected.
fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], to_unit: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| to_unit([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
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
    fn replaces_invalid_utf8_instead_of_failing() {
        assert_eq!(decode(b"ok\xFF"), "ok\u{FFFD}");
    }
}

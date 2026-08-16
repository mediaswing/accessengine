//! Undoing the encodings a PDF stream can be stored in.
//!
//! Only the filters that carry *text* are implemented: the page descriptions,
//! the object streams and the embedded character maps. The image filters —
//! `DCTDecode` and friends — are recognised and refused, since a picture is
//! not something this module can turn into words.
//!
//! Everything here is defensive about damaged data. A stream that stops early
//! is decoded as far as it goes rather than thrown away: half a page read
//! aloud is worth more than a file that refuses to open, and truncated streams
//! are common in files that have been through a bad editor.

use super::object::{Dict, Object};
use anyhow::{Result, bail};
use std::io::Read;

/// The most any single stream may expand to.
///
/// A page description is measured in tens of kilobytes; the largest legitimate
/// stream in a normal file is an object stream, and those stay well inside a
/// megabyte. The ceiling is here because deflate is a compression format and
/// a few hundred kilobytes of it can name gigabytes of output — the same
/// reasoning as the one in [`crate::extract::docx`].
const MAX_DECODED_BYTES: usize = 128 * 1024 * 1024;

/// Decodes one stream's data through whatever chain of filters its dictionary
/// names. `resolve` supplies the value of any indirect reference, since both
/// `/Filter` and `/DecodeParms` are allowed to be one.
pub fn decode(
    dict: &Dict,
    raw: &[u8],
    resolve: &dyn Fn(&Object) -> Object,
) -> Result<Vec<u8>> {
    let filters = names(dict.get("Filter"), resolve);
    let parms = parameter_dicts(dict.get("DecodeParms"), resolve, filters.len());

    let mut data = raw.to_vec();
    for (index, filter) in filters.iter().enumerate() {
        let parms = parms.get(index).and_then(Option::as_ref);
        data = match filter.as_str() {
            // The abbreviated names are what a stream inside an inline image
            // uses, and some writers use them everywhere.
            "FlateDecode" | "Fl" => flate(&data),
            "LZWDecode" | "LZW" => lzw(&data, early_change(parms, resolve)),
            "ASCIIHexDecode" | "AHx" => ascii_hex(&data),
            "ASCII85Decode" | "A85" => ascii_85(&data),
            "RunLengthDecode" | "RL" => run_length(&data),
            // Not an error worth a message of its own: the caller asked for
            // text and this stream is a picture.
            "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" | "CCITTFaxDecode" | "CCF" => {
                bail!("stream holds an image, not text")
            }
            "Crypt" => data,
            other => bail!("unsupported stream filter {other}"),
        };
        if let Some(parms) = parms {
            data = undo_predictor(data, parms, resolve);
        }
        if data.len() > MAX_DECODED_BYTES {
            bail!("a stream in this file expands to more than this app will read");
        }
    }
    Ok(data)
}

/// A filter entry is either one name or an array of them.
fn names(entry: Option<&Object>, resolve: &dyn Fn(&Object) -> Object) -> Vec<String> {
    match entry.map(resolve) {
        Some(Object::Name(name)) => vec![name],
        Some(Object::Array(items)) => items
            .iter()
            .map(resolve)
            .filter_map(|item| item.as_name().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// `/DecodeParms` mirrors `/Filter`: one dictionary, or one per filter with
/// nulls for the filters that take none.
fn parameter_dicts(
    entry: Option<&Object>,
    resolve: &dyn Fn(&Object) -> Object,
    count: usize,
) -> Vec<Option<Dict>> {
    let mut out = match entry.map(resolve) {
        Some(Object::Dict(dict)) => vec![Some(dict)],
        Some(Object::Array(items)) => items
            .iter()
            .map(|item| resolve(item).as_dict().cloned())
            .collect(),
        _ => Vec::new(),
    };
    out.resize(count, None);
    out
}

fn early_change(parms: Option<&Dict>, resolve: &dyn Fn(&Object) -> Object) -> bool {
    parms
        .and_then(|dict| dict.get("EarlyChange"))
        .map(resolve)
        .and_then(|value| value.as_i64())
        .is_none_or(|value| value != 0)
}

/// Inflates a zlib stream, keeping whatever came out if it ends badly.
///
/// The two-byte zlib header is missing often enough — writers that emit raw
/// deflate, and files whose first bytes have been eaten by a broken edit — to
/// be worth a second attempt without it.
fn flate(data: &[u8]) -> Vec<u8> {
    let trimmed = data
        .iter()
        .position(|byte| !super::object::is_white(*byte))
        .map_or(&data[..0], |start| &data[start..]);

    let zlib = inflate(flate2::read::ZlibDecoder::new(trimmed));
    if !zlib.is_empty() {
        return zlib;
    }
    let raw = inflate(flate2::read::DeflateDecoder::new(trimmed));
    if !raw.is_empty() {
        return raw;
    }
    // One more try a byte in: a stray leading byte before the zlib header is a
    // known symptom of a stream whose `/Length` counted the EOL after
    // `stream`.
    if trimmed.len() > 1 {
        return inflate(flate2::read::ZlibDecoder::new(&trimmed[1..]));
    }
    Vec::new()
}

/// Reads a decompressor to exhaustion, keeping the bytes produced before any
/// error. `read_to_end` discards its buffer on failure, so this reads in
/// chunks and holds on to them itself.
fn inflate<R: Read>(mut reader: R) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                out.extend_from_slice(&chunk[..read]);
                if out.len() > MAX_DECODED_BYTES {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut high: Option<u8> = None;
    for &byte in data {
        if byte == b'>' {
            break;
        }
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => continue,
        };
        match high.take() {
            Some(first) => out.push(first * 16 + value),
            None => high = Some(value),
        }
    }
    if let Some(first) = high {
        out.push(first * 16);
    }
    out
}

fn ascii_85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut filled = 0usize;
    let mut index = 0usize;
    // Some writers keep the `<~` opener Adobe's own encoder writes.
    if data.starts_with(b"<~") {
        index = 2;
    }
    while index < data.len() {
        let byte = data[index];
        index += 1;
        match byte {
            b'~' => break,
            b'z' if filled == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b'!'..=b'u' => {
                group[filled] = byte - b'!';
                filled += 1;
                if filled == 5 {
                    push_base85(&mut out, &group, 5);
                    filled = 0;
                }
            }
            _ => {}
        }
    }
    if filled > 1 {
        // A short final group is padded with the highest digit and yields one
        // byte fewer than it holds.
        for slot in group.iter_mut().skip(filled) {
            *slot = 84;
        }
        push_base85(&mut out, &group, filled);
    }
    out
}

fn push_base85(out: &mut Vec<u8>, group: &[u8; 5], filled: usize) {
    let mut value = 0u32;
    for &digit in group {
        value = value.wrapping_mul(85).wrapping_add(u32::from(digit));
    }
    out.extend_from_slice(&value.to_be_bytes()[..filled - 1]);
}

fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < data.len() {
        let length = data[index];
        index += 1;
        match length {
            128 => break,
            0..=127 => {
                let count = usize::from(length) + 1;
                let end = (index + count).min(data.len());
                out.extend_from_slice(&data[index..end]);
                index = end;
            }
            _ => {
                let Some(&byte) = data.get(index) else { break };
                index += 1;
                out.extend(std::iter::repeat_n(byte, 257 - usize::from(length)));
            }
        }
    }
    out
}

/// LZW as PDF uses it: nine-bit codes growing to twelve, MSB first, with the
/// off-by-one `EarlyChange` behaviour Adobe's own encoder has.
fn lzw(data: &[u8], early_change: bool) -> Vec<u8> {
    const CLEAR: u16 = 256;
    const EOD: u16 = 257;

    let mut out = Vec::new();
    let mut table: Vec<Vec<u8>> = Vec::new();
    let reset = |table: &mut Vec<Vec<u8>>| {
        table.clear();
        table.extend((0..=255u16).map(|byte| vec![byte as u8]));
        table.push(Vec::new()); // 256, the clear code
        table.push(Vec::new()); // 257, end of data
    };
    reset(&mut table);

    let mut width = 9u32;
    let mut previous: Option<u16> = None;
    let mut bits = 0u32;
    let mut buffer = 0u32;

    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= width {
            let code = ((buffer >> (bits - width)) & ((1 << width) - 1)) as u16;
            bits -= width;

            if code == EOD {
                return out;
            }
            if code == CLEAR {
                reset(&mut table);
                width = 9;
                previous = None;
                continue;
            }

            let entry = match table.get(usize::from(code)) {
                Some(entry) => entry.clone(),
                // The one legal forward reference: a code for the string that
                // is about to be added.
                None => match previous.and_then(|code| table.get(usize::from(code))) {
                    Some(earlier) => {
                        let mut entry = earlier.clone();
                        entry.push(earlier[0]);
                        entry
                    }
                    None => return out,
                },
            };
            out.extend_from_slice(&entry);
            if out.len() > MAX_DECODED_BYTES {
                return out;
            }

            if let Some(code) = previous
                && let Some(earlier) = table.get(usize::from(code))
            {
                let mut added = earlier.clone();
                added.push(entry[0]);
                table.push(added);
            }
            previous = Some(code);

            let limit = table.len() + usize::from(early_change);
            width = match limit {
                0..=511 => 9,
                512..=1023 => 10,
                1024..=2047 => 11,
                _ => 12,
            };
        }
    }
    out
}

/// Undoes the row-to-row prediction some streams are filtered through before
/// compression. Cross reference streams always use one; object streams
/// sometimes do.
fn undo_predictor(data: Vec<u8>, parms: &Dict, resolve: &dyn Fn(&Object) -> Object) -> Vec<u8> {
    let number = |key: &str, fallback: i64| {
        parms
            .get(key)
            .map(resolve)
            .and_then(|value| value.as_i64())
            .unwrap_or(fallback)
    };
    let predictor = number("Predictor", 1);
    if predictor < 2 {
        return data;
    }
    let colours = number("Colors", 1).clamp(1, 32) as usize;
    let bits = number("BitsPerComponent", 8).clamp(1, 16) as usize;
    let columns = number("Columns", 1).max(1) as usize;
    let sample = (colours * bits).div_ceil(8).max(1);
    let row_length = (columns * colours * bits).div_ceil(8);

    if predictor == 2 {
        return tiff_predictor(data, bits, colours, row_length);
    }

    // PNG predictors: every row is prefixed with the filter type used for it.
    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0u8; row_length];
    for chunk in data.chunks(row_length + 1) {
        let Some((&kind, row)) = chunk.split_first() else {
            break;
        };
        let mut row = row.to_vec();
        row.resize(row_length, 0);
        for index in 0..row_length {
            let left = if index >= sample {
                row[index - sample]
            } else {
                0
            };
            let up = previous[index];
            let up_left = if index >= sample {
                previous[index - sample]
            } else {
                0
            };
            row[index] = match kind {
                1 => row[index].wrapping_add(left),
                2 => row[index].wrapping_add(up),
                3 => row[index].wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8),
                4 => row[index].wrapping_add(paeth(left, up, up_left)),
                _ => row[index],
            };
        }
        out.extend_from_slice(&row);
        previous = row;
    }
    out
}

fn tiff_predictor(mut data: Vec<u8>, bits: usize, colours: usize, row_length: usize) -> Vec<u8> {
    // Only the eight-bit case is worth handling: sub-byte components appear on
    // images, and images do not come through this module.
    if bits != 8 || row_length == 0 {
        return data;
    }
    for row in data.chunks_mut(row_length) {
        for index in colours..row.len() {
            row[index] = row[index].wrapping_add(row[index - colours]);
        }
    }
    data
}

fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(up_left);
    let distance_left = (estimate - i16::from(left)).abs();
    let distance_up = (estimate - i16::from(up)).abs();
    let distance_up_left = (estimate - i16::from(up_left)).abs();
    if distance_left <= distance_up && distance_left <= distance_up_left {
        left
    } else if distance_up <= distance_up_left {
        up
    } else {
        up_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn no_references(object: &Object) -> Object {
        object.clone()
    }

    fn dict(pairs: &[(&str, Object)]) -> Dict {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn deflate(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn inflates_a_flate_stream() {
        let compressed = deflate(b"BT /F1 12 Tf (hello) Tj ET");
        let decoded = decode(
            &dict(&[("Filter", Object::Name("FlateDecode".into()))]),
            &compressed,
            &no_references,
        )
        .unwrap();
        assert_eq!(decoded, b"BT /F1 12 Tf (hello) Tj ET");
    }

    /// A stream cut short mid-way still has to give up what it holds: half a
    /// page spoken beats a file that will not open.
    #[test]
    fn a_truncated_flate_stream_keeps_what_decoded() {
        let compressed = deflate(&b"The quick brown fox. ".repeat(200));
        let cut = &compressed[..compressed.len() - 8];
        let decoded = decode(
            &dict(&[("Filter", Object::Name("FlateDecode".into()))]),
            cut,
            &no_references,
        )
        .unwrap();
        assert!(decoded.starts_with(b"The quick brown fox."));
    }

    #[test]
    fn applies_a_chain_of_filters_in_order() {
        // Hex on the outside, flate underneath — the order `/Filter` lists.
        let compressed = deflate(b"nested");
        let hex: String = compressed.iter().map(|byte| format!("{byte:02X}")).collect();
        let decoded = decode(
            &dict(&[(
                "Filter",
                Object::Array(vec![
                    Object::Name("ASCIIHexDecode".into()),
                    Object::Name("FlateDecode".into()),
                ]),
            )]),
            hex.as_bytes(),
            &no_references,
        )
        .unwrap();
        assert_eq!(decoded, b"nested");
    }

    #[test]
    fn decodes_ascii85_including_the_z_shorthand_and_short_group() {
        assert_eq!(ascii_85(b"87cURD_*#4DfTZ)~>"), b"Hello, World");
        assert_eq!(ascii_85(b"z~>"), &[0, 0, 0, 0]);
        // A final group of three digits stands for two bytes, not four.
        assert_eq!(ascii_85(b"87_~>"), b"He");
    }

    #[test]
    fn decodes_run_length_runs_and_literals() {
        // 2 → three literal bytes; 254 → three copies of the next byte.
        assert_eq!(run_length(&[2, b'a', b'b', b'c', 254, b'z', 128]), b"abczzz");
    }

    #[test]
    fn decodes_lzw_as_the_specification_writes_it() {
        // The worked example from the PDF specification, which encodes the
        // ten bytes below as these nine.
        let encoded = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        assert_eq!(
            lzw(&encoded, true),
            [45, 45, 45, 45, 45, 65, 45, 45, 45, 66]
        );
    }

    #[test]
    fn undoes_a_png_up_predictor() {
        // Two rows of three columns; the second is stored as deltas from the
        // first with filter type 2 ("up").
        let parms = dict(&[
            ("Predictor", Object::Int(12)),
            ("Columns", Object::Int(3)),
        ]);
        let data = vec![0, 10, 20, 30, 2, 1, 1, 1];
        assert_eq!(
            undo_predictor(data, &parms, &no_references),
            [10, 20, 30, 11, 21, 31]
        );
    }

    #[test]
    fn an_image_stream_is_refused_rather_than_returned_as_noise() {
        let error = decode(
            &dict(&[("Filter", Object::Name("DCTDecode".into()))]),
            b"\xFF\xD8\xFF",
            &no_references,
        )
        .unwrap_err();
        assert!(error.to_string().contains("image"));
    }
}

//! Turning the bytes a page shows into the characters they stand for.
//!
//! A PDF string is not text. `(Hello)` is five numbers that mean "the glyphs
//! at these slots of the font in force", and what those glyphs *are* is a
//! separate question the font has to answer. Three answers turn up, in
//! descending order of how much they can be trusted:
//!
//! 1. A `/ToUnicode` map, which states outright what each code means. Anything
//!    written to be searchable or read aloud carries one, and it is the only
//!    answer that works for a subset font whose codes were assigned
//!    arbitrarily when the file was written.
//! 2. A named encoding, optionally with `/Differences` patching individual
//!    slots. This is a table lookup — see [`super::encodings`].
//! 3. Nothing usable, which is a font whose codes cannot be turned back into
//!    text by any means. Those codes are dropped rather than guessed at: a
//!    voice reading confident nonsense is worse than one that stays quiet, and
//!    the caller counts what was lost so it can say so.

use super::doc::Document;
use super::encodings;
use super::object::{Dict, Lexer, Object, Token};
use std::collections::HashMap;

pub struct Font {
    /// Composite fonts address their glyphs with two bytes rather than one.
    /// Nearly all of them use `Identity-H`, where a code is simply a glyph
    /// number, which is exactly the case that needs `/ToUnicode` to say
    /// anything at all.
    two_byte: bool,
    to_unicode: HashMap<u32, String>,
    /// The one-byte fallback table, absent for a font whose codes stand for
    /// pictures rather than letters.
    simple: Option<[char; 256]>,
    widths: Widths,
}

/// How wide each glyph is, in fractions of the font size.
///
/// This is what makes it possible to say where a string *ended*, and so
/// whether the gap before the next one is a space. Without it the reader is
/// reduced to assuming that anything drawn at a fresh position is a new word,
/// which turns a line positioned glyph by glyph — as a typesetter that has
/// tuned its spacing does — into "M a c B o o k".
enum Widths {
    /// One entry per code from `first`, which is how a simple font states it.
    Simple {
        first: u32,
        widths: Vec<f64>,
        missing: f64,
    },
    /// A composite font's sparse table, keyed by glyph.
    Composite {
        default: f64,
        widths: HashMap<u32, f64>,
    },
    /// The font stated none, so every glyph is assumed to be one typical
    /// letter wide. Distances measured this way drift, and the caller is told
    /// as much by [`Font::has_widths`].
    Assumed(f64),
}

/// How a string came out.
pub struct Decoded {
    pub text: String,
    /// Codes that no map or table could account for. Counted rather than
    /// guessed at, so a document that comes back suspiciously short can say
    /// why.
    pub dropped: usize,
    /// How far the pen moved, as a multiple of the font size, before any
    /// character or word spacing is added.
    pub advance: f64,
    /// How many codes were shown, and how many of those were spaces — the two
    /// counts character and word spacing are charged against.
    pub codes: usize,
    pub spaces: usize,
}

impl Font {
    pub fn load(doc: &Document, dict: &Dict) -> Self {
        let subtype = doc.get_name(dict, "Subtype").unwrap_or_default();
        let base_font = doc.get_name(dict, "BaseFont").unwrap_or_default();
        let two_byte = subtype == "Type0";

        let to_unicode = doc
            .get(dict, "ToUnicode")
            .and_then(|object| doc.stream_data(object).ok())
            .map(|data| parse_cmap(&data))
            .unwrap_or_default();

        Self {
            two_byte,
            to_unicode,
            simple: (!two_byte && !is_pictorial(base_font))
                .then(|| simple_table(doc, dict, base_font)),
            widths: Widths::load(doc, dict, two_byte, base_font),
        }
    }

    /// A font referred to by a page that does not declare it. Reading its
    /// bytes as WinAnsi is a guess, but it is the guess that makes an
    /// otherwise unreadable page come out as its own text.
    pub fn fallback() -> Self {
        Self {
            two_byte: false,
            to_unicode: HashMap::new(),
            simple: Some(encodings::base_table(None)),
            widths: Widths::Assumed(ASSUMED_WIDTH),
        }
    }

    /// Whether the font stated its own metrics. Distances worked out from
    /// assumed ones are too rough to judge a space by.
    pub fn has_widths(&self) -> bool {
        !matches!(self.widths, Widths::Assumed(_))
    }

    pub fn decode(&self, bytes: &[u8]) -> Decoded {
        let mut text = String::with_capacity(bytes.len());
        let mut dropped = 0usize;
        let mut advance = 0.0;
        let mut codes = 0usize;
        let mut spaces = 0usize;

        for code in self.codes(bytes) {
            codes += 1;
            advance += self.widths.of(code);
            // Word spacing is charged against the single byte 32 and nothing
            // else — in a two-byte font it applies to no code at all.
            if code == 32 && !self.two_byte {
                spaces += 1;
            }
            if let Some(mapped) = self.to_unicode.get(&code) {
                text.push_str(mapped);
                continue;
            }
            match self
                .simple
                .as_ref()
                .and_then(|table| usize::try_from(code).ok().and_then(|code| table.get(code)))
            {
                Some(&character) if character != '\0' => text.push(character),
                _ => dropped += 1,
            }
        }

        Decoded {
            text,
            dropped,
            advance,
            codes,
            spaces,
        }
    }

    /// Splits a string into character codes. Two-byte codes are big-endian; a
    /// trailing odd byte in a two-byte font is a damaged string, and is padded
    /// rather than dropped so the glyph before it is still read.
    fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        if self.two_byte {
            bytes
                .chunks(2)
                .map(|pair| match pair {
                    [high, low] => u32::from(u16::from_be_bytes([*high, *low])),
                    [high] => u32::from(*high) << 8,
                    _ => 0,
                })
                .collect()
        } else {
            bytes.iter().map(|&byte| u32::from(byte)).collect()
        }
    }
}

/// What one glyph is assumed to be worth when a font says nothing: a little
/// over half the font size, which is about the average for the proportional
/// Latin faces this arises for — the built-in Helvetica and Times, which carry
/// their metrics outside the file where this cannot see them.
const ASSUMED_WIDTH: f64 = 0.5;

/// Courier and its relatives are the one family whose missing metrics are not
/// a guess: every glyph in a monospaced font is exactly this wide.
const COURIER_WIDTH: f64 = 0.6;

impl Widths {
    fn load(doc: &Document, dict: &Dict, two_byte: bool, base_font: &str) -> Self {
        if two_byte {
            return Self::composite(doc, dict);
        }
        let widths: Vec<f64> = doc
            .get(dict, "Widths")
            .and_then(Object::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| doc.resolve(entry).as_f64().unwrap_or(0.0) / 1000.0)
                    .collect()
            })
            .unwrap_or_default();
        if widths.is_empty() {
            let family = base_font.rsplit('+').next().unwrap_or(base_font);
            return Self::Assumed(if family.starts_with("Courier") {
                COURIER_WIDTH
            } else {
                ASSUMED_WIDTH
            });
        }

        let missing = doc
            .get_dict(dict, "FontDescriptor")
            .and_then(|descriptor| doc.get(descriptor, "MissingWidth"))
            .and_then(Object::as_f64)
            .map(|width| width / 1000.0)
            // A code outside the range the font declares is one it never
            // meant to draw. Charging the average for it keeps the pen
            // roughly right rather than stopping it dead.
            .filter(|width| *width > 0.0)
            .unwrap_or(ASSUMED_WIDTH);

        Self::Simple {
            first: doc
                .get(dict, "FirstChar")
                .and_then(Object::as_i64)
                .and_then(|first| u32::try_from(first).ok())
                .unwrap_or(0),
            widths,
            missing,
        }
    }

    /// A composite font keeps its metrics on the descendant font that actually
    /// holds the glyphs, in a `/W` array that mixes two spellings: a starting
    /// glyph with a list of widths, or a range of glyphs sharing one.
    fn composite(doc: &Document, dict: &Dict) -> Self {
        let Some(descendant) = doc
            .get(dict, "DescendantFonts")
            .and_then(Object::as_array)
            .and_then(|fonts| fonts.first())
            .and_then(|font| doc.resolve(font).as_dict())
        else {
            return Self::Assumed(ASSUMED_WIDTH);
        };

        let default = doc
            .get(descendant, "DW")
            .and_then(Object::as_f64)
            .map_or(1.0, |width| width / 1000.0);
        let mut widths = HashMap::new();

        if let Some(entries) = doc.get(descendant, "W").and_then(Object::as_array) {
            /// A single `first last width` run is capped, so a malformed pair
            /// of huge numbers cannot fill memory one entry at a time.
            const MAX_RUN: u32 = 65_536;

            let mut index = 0usize;
            while index < entries.len() {
                let Some(first) = doc.resolve(&entries[index]).as_f64() else {
                    break;
                };
                let first = first.max(0.0) as u32;
                index += 1;
                match entries.get(index).map(|entry| doc.resolve(entry)) {
                    Some(Object::Array(list)) => {
                        for (offset, width) in list.iter().enumerate() {
                            let width = doc.resolve(width).as_f64().unwrap_or(0.0);
                            widths.insert(first + offset as u32, width / 1000.0);
                        }
                        index += 1;
                    }
                    Some(other) => {
                        let last = other.as_f64().unwrap_or(0.0).max(0.0) as u32;
                        let width = entries
                            .get(index + 1)
                            .and_then(|entry| doc.resolve(entry).as_f64())
                            .unwrap_or(0.0);
                        for glyph in first..=last.min(first.saturating_add(MAX_RUN)) {
                            widths.insert(glyph, width / 1000.0);
                        }
                        index += 2;
                    }
                    None => break,
                }
            }
        }

        Self::Composite { default, widths }
    }

    /// How wide one code's glyph is, as a fraction of the font size.
    fn of(&self, code: u32) -> f64 {
        match self {
            Self::Simple {
                first,
                widths,
                missing,
            } => code
                .checked_sub(*first)
                .and_then(|index| widths.get(index as usize))
                .copied()
                .unwrap_or(*missing),
            // A composite font's widths are keyed by glyph number, which is
            // what the code already is under `Identity-H` — the encoding all
            // but a handful of these files use.
            Self::Composite { default, widths } => widths.get(&code).copied().unwrap_or(*default),
            Self::Assumed(width) => *width,
        }
    }
}

/// Whether a font's glyphs are pictures rather than letters.
///
/// Symbol and ZapfDingbats are the two standard fonts whose codes mean
/// mathematics and ornaments; read through a Latin table, a bulleted list
/// comes out as a column of stray letters for the voice to spell. Only these
/// two are treated this way, and only when the font supplied no `/ToUnicode`:
/// the "symbolic" flag in a font descriptor is set on so many ordinary subset
/// text fonts that it says nothing useful.
fn is_pictorial(base_font: &str) -> bool {
    // Subset fonts are named `ABCDEF+Symbol`.
    let name = base_font.rsplit('+').next().unwrap_or(base_font);
    matches!(name, "Symbol" | "ZapfDingbats")
}

/// The 256-entry table for a simple font: a named base encoding, with any
/// `/Differences` applied over the top.
fn simple_table(doc: &Document, dict: &Dict, base_font: &str) -> [char; 256] {
    let encoding = doc.get(dict, "Encoding");
    let (base_name, differences) = match encoding {
        Some(Object::Name(name)) => (Some(name.as_str()), None),
        Some(Object::Dict(encoding)) => (
            doc.get_name(encoding, "BaseEncoding"),
            doc.get(encoding, "Differences").and_then(Object::as_array),
        ),
        _ => (None, None),
    };
    // The Courier/Helvetica/Times built-ins predate WinAnsi and use the
    // Standard set unless told otherwise.
    let base_name =
        base_name.or_else(|| is_standard_base_font(base_font).then_some("StandardEncoding"));

    let mut table = encodings::base_table(base_name);
    let Some(differences) = differences else {
        return table;
    };

    // `[ 1 /a /b 5 /c ]`: a number sets the next code, and every name after it
    // fills one slot and moves on.
    let mut code = 0usize;
    for item in differences {
        match doc.resolve(item) {
            Object::Int(_) | Object::Real(_) => {
                code = doc
                    .resolve(item)
                    .as_i64()
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(code);
            }
            Object::Name(name) => {
                if code < table.len() {
                    table[code] = encodings::glyph_to_char(name).unwrap_or('\0');
                }
                code += 1;
            }
            _ => {}
        }
    }
    table
}

fn is_standard_base_font(base_font: &str) -> bool {
    let name = base_font.rsplit('+').next().unwrap_or(base_font);
    let family = name.split(['-', ',']).next().unwrap_or(name);
    matches!(family, "Courier" | "Helvetica" | "Times" | "Arial")
}

/// Reads a `/ToUnicode` character map.
///
/// A CMap is a PostScript program, but the part that matters is entirely
/// declarative: runs of `beginbfchar`/`endbfchar` pairing one code with its
/// text, and `beginbfrange`/`endbfrange` doing the same for a span of codes at
/// once. Everything else in the file is skipped.
fn parse_cmap(data: &[u8]) -> HashMap<u32, String> {
    /// A single range is capped: `<0000> <FFFF>` is legal and would otherwise
    /// be sixty-five thousand entries, and a damaged one could name far more.
    const MAX_RANGE: u32 = 65_536;

    let mut map = HashMap::new();
    let mut lexer = Lexer::new(data);
    let mut operands: Vec<Object> = Vec::new();

    loop {
        match lexer.next_token() {
            Token::Object(object) => {
                operands.push(object);
                // Nothing here needs more than three operands in hand; an
                // unbounded array of them would be a damaged file eating
                // memory.
                if operands.len() > 3 {
                    operands.remove(0);
                }
            }
            Token::Keyword(keyword) => {
                match keyword.as_str() {
                    "beginbfchar" => {
                        while let (Some(from), Some(to)) =
                            (lexer.next_object(), lexer.next_object())
                        {
                            let (Some(from), Some(to)) = (from.as_bytes(), destination(&to)) else {
                                break;
                            };
                            if !to.is_empty() {
                                map.insert(code_of(from), to);
                            }
                        }
                    }
                    "beginbfrange" => {
                        while let (Some(low), Some(high), Some(to)) = (
                            lexer.next_object(),
                            lexer.next_object(),
                            lexer.next_object(),
                        ) {
                            let (Some(low), Some(high)) = (low.as_bytes(), high.as_bytes()) else {
                                break;
                            };
                            let (low, high) = (code_of(low), code_of(high));
                            if high < low || high - low > MAX_RANGE {
                                continue;
                            }
                            match to {
                                // `[ <0041> <0042> … ]`: one destination each.
                                Object::Array(items) => {
                                    for (offset, item) in items.iter().enumerate() {
                                        let Some(text) = destination(item) else {
                                            continue;
                                        };
                                        if !text.is_empty() {
                                            map.insert(low + offset as u32, text);
                                        }
                                    }
                                }
                                // One destination for the whole span, counting
                                // up from it a code at a time.
                                other => {
                                    let Some(bytes) = other.as_bytes() else {
                                        break;
                                    };
                                    for code in low..=high {
                                        let text = utf16be(&stepped(bytes, code - low));
                                        if !text.is_empty() {
                                            map.insert(code, text);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
            Token::Close => {}
            Token::Eof => break,
        }
    }
    map
}

/// What a `bfchar` or `bfrange` maps to: a string of UTF-16 code units, or
/// occasionally a glyph name.
fn destination(object: &Object) -> Option<String> {
    match object {
        Object::Str(bytes) => Some(utf16be(bytes)),
        Object::Name(name) => Some(
            encodings::glyph_to_char(name)
                .map(String::from)
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

/// Adds an offset to the last code unit of a destination, which is how a
/// `bfrange` names one mapping for a whole span of codes.
fn stepped(bytes: &[u8], offset: u32) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.len() >= 2 {
        let last = out.len() - 2;
        let value = u16::from_be_bytes([out[last], out[last + 1]]).wrapping_add(offset as u16);
        out[last..].copy_from_slice(&value.to_be_bytes());
    } else if let Some(last) = out.last_mut() {
        *last = last.wrapping_add(offset as u8);
    }
    out
}

/// A code as a number: one byte or two, big-endian, as the map wrote it.
fn code_of(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .take(4)
        .fold(0u32, |code, &byte| (code << 8) | u32::from(byte))
}

/// Decodes the UTF-16BE that a CMap states its destinations in, dropping the
/// nulls that stand for "no mapping" — a font that says a code means U+0000 is
/// saying it means nothing, and pushing one into the text would put a stray
/// control character into the speech.
fn utf16be(bytes: &[u8]) -> String {
    let (pairs, _odd_byte) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_be_bytes).collect();
    String::from_utf16_lossy(&units)
        .chars()
        .filter(|character| *character != '\0' && *character != '\u{FFFD}')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmap(source: &str) -> HashMap<u32, String> {
        parse_cmap(source.as_bytes())
    }

    #[test]
    fn reads_bfchar_pairs() {
        let map = cmap(
            "/CIDInit /ProcSet findresource begin
             1 begincodespacerange <00> <FF> endcodespacerange
             2 beginbfchar <01> <0048> <02> <0069> endbfchar
             endcmap",
        );
        assert_eq!(map.get(&1).map(String::as_str), Some("H"));
        assert_eq!(map.get(&2).map(String::as_str), Some("i"));
    }

    #[test]
    fn reads_bfranges_that_count_up_from_one_destination() {
        let map = cmap("1 beginbfrange <0003> <0005> <0041> endbfrange");
        assert_eq!(map.get(&3).map(String::as_str), Some("A"));
        assert_eq!(map.get(&4).map(String::as_str), Some("B"));
        assert_eq!(map.get(&5).map(String::as_str), Some("C"));
    }

    #[test]
    fn reads_bfranges_that_list_their_destinations() {
        let map = cmap("1 beginbfrange <10> <12> [<0058> <0059> <005A>] endbfrange");
        assert_eq!(map.get(&0x10).map(String::as_str), Some("X"));
        assert_eq!(map.get(&0x12).map(String::as_str), Some("Z"));
    }

    /// A ligature is one code standing for two letters, and dropping the
    /// second turns "difficult" into "dicult".
    #[test]
    fn a_code_may_map_to_more_than_one_character() {
        let map = cmap("1 beginbfchar <07> <00660066> endbfchar");
        assert_eq!(map.get(&7).map(String::as_str), Some("ff"));
    }

    #[test]
    fn a_mapping_to_nothing_is_left_out() {
        let map = cmap("1 beginbfchar <01> <0000> endbfchar");
        assert!(map.is_empty());
    }

    #[test]
    fn an_enormous_range_is_refused_rather_than_expanded() {
        let map = cmap("1 beginbfrange <0000> <FFFFFF> <0041> endbfrange");
        assert!(map.is_empty());
    }

    #[test]
    fn a_truncated_cmap_keeps_what_it_had() {
        let map = cmap("1 beginbfchar <01> <0048> <02>");
        assert_eq!(map.get(&1).map(String::as_str), Some("H"));
    }

    #[test]
    fn a_simple_font_falls_back_to_a_table() {
        let font = Font::fallback();
        let decoded = font.decode(b"Caf\xE9");
        assert_eq!(decoded.text, "Café");
        assert_eq!(decoded.dropped, 0);
    }

    #[test]
    fn two_byte_codes_without_a_map_are_dropped_rather_than_invented() {
        let font = Font {
            two_byte: true,
            to_unicode: HashMap::new(),
            simple: None,
            widths: Widths::Assumed(ASSUMED_WIDTH),
        };
        let decoded = font.decode(&[0x00, 0x24, 0x00, 0x25]);
        assert!(decoded.text.is_empty());
        assert_eq!(decoded.dropped, 2);
    }

    #[test]
    fn a_tounicode_map_beats_the_table() {
        let font = Font {
            two_byte: false,
            to_unicode: HashMap::from([(0x41, "the".to_string())]),
            simple: Some(encodings::base_table(None)),
            widths: Widths::Assumed(ASSUMED_WIDTH),
        };
        assert_eq!(font.decode(b"AB").text, "theB");
    }

    #[test]
    fn symbol_and_dingbat_fonts_stay_quiet() {
        assert!(is_pictorial("ZapfDingbats"));
        assert!(is_pictorial("ABCDEF+Symbol"));
        assert!(!is_pictorial("ABCDEF+Symbola-Regular"));
        assert!(!is_pictorial("Helvetica"));
    }

    #[test]
    fn recognises_the_built_in_fonts_that_predate_winansi() {
        assert!(is_standard_base_font("Helvetica-Bold"));
        assert!(is_standard_base_font("ABCDEF+Times-Roman"));
        assert!(!is_standard_base_font("Calibri"));
    }
}

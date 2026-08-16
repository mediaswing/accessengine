//! The tables that turn a font's byte codes into characters.
//!
//! A simple font shows text one byte at a time, and what each byte means comes
//! from an encoding: usually one of the three named here, sometimes one of
//! them with a list of differences applied on top. A font that carries a
//! `/ToUnicode` map needs none of this — see [`super::font`] — but plenty of
//! files, especially older ones, do not carry one.
//!
//! Only the Latin encodings are here. A symbolic font with no `/ToUnicode` is
//! not decodable by any table: its bytes mean whatever its own glyphs say they
//! mean, and reading them as letters would produce confident nonsense for a
//! voice to say out loud.

/// The 256 characters a named base encoding maps its codes to, with `'\0'`
/// where the encoding defines nothing.
pub fn base_table(name: Option<&str>) -> [char; 256] {
    let mut table = ['\0'; 256];
    // The printable ASCII range is common to all of them, and is its own code
    // point throughout.
    for code in 0x20u8..=0x7E {
        table[usize::from(code)] = code as char;
    }

    let high = match name {
        Some("MacRomanEncoding") => &MAC_ROMAN_HIGH,
        Some("StandardEncoding") | Some("MacExpertEncoding") => {
            // The two places the Standard set disagrees with ASCII: an
            // apostrophe is a right quote and a backquote is a left one.
            table[0x27] = '\u{2019}';
            table[0x60] = '\u{2018}';
            &STANDARD_HIGH
        }
        // WinAnsi is both the commonest encoding and the most forgiving guess
        // for a font that names none: it agrees with Latin-1 over the whole
        // upper half, which is where the letters people actually read are.
        _ => &WIN_ANSI_HIGH,
    };
    table[0x80..].copy_from_slice(high);
    table
}

/// A glyph name as `/Differences` writes it, resolved to a character.
///
/// Handles the three forms that turn up: an ordinary name from the Adobe list,
/// the `uni0041` and `u1F600` spellings that state a code point outright, and
/// a name with a variant suffix such as `a.sc` for a small capital, which is
/// still the letter `a` as far as a voice is concerned.
pub fn glyph_to_char(name: &str) -> Option<char> {
    let base = name.split('.').next().unwrap_or(name);
    if base.is_empty() {
        return None;
    }

    if let Some(digits) = base.strip_prefix("uni")
        && digits.len() >= 4
        && let Ok(code) = u32::from_str_radix(&digits[..4], 16)
    {
        return char::from_u32(code);
    }
    if let Some(digits) = base.strip_prefix('u')
        && (4..=6).contains(&digits.len())
        && let Ok(code) = u32::from_str_radix(digits, 16)
    {
        return char::from_u32(code);
    }

    // A one-character name is that character: `A`, `a`, `1`.
    let mut characters = base.chars();
    if let (Some(single), None) = (characters.next(), characters.next())
        && single.is_ascii_alphanumeric()
    {
        return Some(single);
    }

    GLYPH_NAMES
        .iter()
        .find(|(glyph, _)| *glyph == base)
        .map(|(_, character)| *character)
}

/// Codes 0x80–0xFF of WinAnsiEncoding, which is Windows code page 1252: the
/// Latin-1 upper half with typographic punctuation filling the control range.
/// The same table as the one [`crate::extract::txt`] falls back on, for the
/// same reason — it is where the curly quotes and dashes live.
const WIN_ANSI_HIGH: [char; 128] = [
    '\u{20AC}', '\0', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\0', '\u{017D}', '\0', '\0',
    '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}',
    '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\0', '\u{017E}', '\u{0178}', ' ', '¡', '¢',
    '£', '¤', '¥', '¦', '§', '¨', '©', 'ª', '«', '¬', '\u{00AD}', '®', '¯', '°', '±', '²', '³',
    '´', 'µ', '¶', '·', '¸', '¹', 'º', '»', '¼', '½', '¾', '¿', 'À', 'Á', 'Â', 'Ã', 'Ä', 'Å', 'Æ',
    'Ç', 'È', 'É', 'Ê', 'Ë', 'Ì', 'Í', 'Î', 'Ï', 'Ð', 'Ñ', 'Ò', 'Ó', 'Ô', 'Õ', 'Ö', '×', 'Ø', 'Ù',
    'Ú', 'Û', 'Ü', 'Ý', 'Þ', 'ß', 'à', 'á', 'â', 'ã', 'ä', 'å', 'æ', 'ç', 'è', 'é', 'ê', 'ë', 'ì',
    'í', 'î', 'ï', 'ð', 'ñ', 'ò', 'ó', 'ô', 'õ', 'ö', '÷', 'ø', 'ù', 'ú', 'û', 'ü', 'ý', 'þ', 'ÿ',
];

/// Codes 0x80–0xFF of MacRomanEncoding. The Apple logo at 0xF0 is left
/// undefined rather than mapped to the private-use character it really is.
const MAC_ROMAN_HIGH: [char; 128] = [
    'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á', 'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è', 'ê', 'ë', 'í',
    'ì', 'î', 'ï', 'ñ', 'ó', 'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü', '\u{2020}', '°', '¢', '£',
    '§', '\u{2022}', '¶', 'ß', '®', '©', '\u{2122}', '´', '¨', '\u{2260}', 'Æ', 'Ø', '\u{221E}',
    '±', '\u{2264}', '\u{2265}', '¥', 'µ', '\u{2202}', '\u{2211}', '\u{220F}', '\u{03C0}',
    '\u{222B}', 'ª', 'º', '\u{03A9}', 'æ', 'ø', '¿', '¡', '¬', '\u{221A}', '\u{0192}', '\u{2248}',
    '\u{2206}', '«', '»', '\u{2026}', ' ', 'À', 'Ã', 'Õ', '\u{0152}', '\u{0153}', '\u{2013}',
    '\u{2014}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '÷', '\u{25CA}', 'ÿ', '\u{0178}',
    '\u{2044}', '\u{20AC}', '\u{2039}', '\u{203A}', '\u{FB01}', '\u{FB02}', '\u{2021}', '·',
    '\u{201A}', '\u{201E}', '\u{2030}', 'Â', 'Ê', 'Á', 'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô',
    '\0', 'Ò', 'Ú', 'Û', 'Ù', '\u{0131}', '\u{02C6}', '\u{02DC}', '¯', '\u{02D8}', '\u{02D9}',
    '\u{02DA}', '¸', '\u{02DD}', '\u{02DB}', '\u{02C7}',
];

/// Codes 0x80–0xFF of StandardEncoding, the built-in encoding of the original
/// Type 1 text fonts. Mostly empty: it has no accented letters at all, which
/// is why `/Differences` exists.
const STANDARD_HIGH: [char; 128] = [
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', '¡', '¢', '£', '\u{2044}', '¥', '\u{0192}', '§', '¤', '\u{2019}', '\u{201C}', '«',
    '\u{2039}', '\u{203A}', '\u{FB01}', '\u{FB02}', '\0', '\u{2013}', '\u{2020}', '\u{2021}', '·',
    '\0', '¶', '\u{2022}', '\u{201A}', '\u{201E}', '\u{201D}', '»', '\u{2026}', '\u{2030}', '\0',
    '¿', '\0', '`', '´', '\u{02C6}', '\u{02DC}', '¯', '\u{02D8}', '\u{02D9}', '¨', '\0',
    '\u{02DA}', '¸', '\0', '\u{02DD}', '\u{02DB}', '\u{02C7}', '\u{2014}', '\0', '\0', '\0', '\0',
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', 'Æ', '\0', 'ª', '\0',
    '\0', '\0', '\0', '\u{0141}', 'Ø', '\u{0152}', 'º', '\0', '\0', '\0', '\0', '\0', 'æ', '\0',
    '\0', '\0', '\u{0131}', '\0', '\0', '\u{0142}', 'ø', '\u{0153}', 'ß', '\0', '\0', '\0', '\0',
];

/// Glyph names, for resolving a font's `/Differences`.
///
/// The Adobe glyph list runs to some four thousand names; this is the Latin
/// part of it, which is what a `/Differences` array in a document written in a
/// European language actually contains. Letters, digits and anything spelled
/// `uniXXXX` are handled in [`glyph_to_char`] without needing an entry here.
const GLYPH_NAMES: &[(&str, char)] = &[
    ("space", ' '),
    ("exclam", '!'),
    ("quotedbl", '"'),
    ("numbersign", '#'),
    ("dollar", '$'),
    ("percent", '%'),
    ("ampersand", '&'),
    ("quotesingle", '\''),
    ("quoteright", '\u{2019}'),
    ("quoteleft", '\u{2018}'),
    ("quotedblleft", '\u{201C}'),
    ("quotedblright", '\u{201D}'),
    ("quotesinglbase", '\u{201A}'),
    ("quotedblbase", '\u{201E}'),
    ("guilsinglleft", '\u{2039}'),
    ("guilsinglright", '\u{203A}'),
    ("guillemotleft", '«'),
    ("guillemotright", '»'),
    ("parenleft", '('),
    ("parenright", ')'),
    ("asterisk", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("hyphen", '-'),
    ("period", '.'),
    ("slash", '/'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less", '<'),
    ("equal", '='),
    ("greater", '>'),
    ("question", '?'),
    ("at", '@'),
    ("bracketleft", '['),
    ("backslash", '\\'),
    ("bracketright", ']'),
    ("asciicircum", '^'),
    ("underscore", '_'),
    ("grave", '`'),
    ("braceleft", '{'),
    ("bar", '|'),
    ("braceright", '}'),
    ("asciitilde", '~'),
    ("zero", '0'),
    ("one", '1'),
    ("two", '2'),
    ("three", '3'),
    ("four", '4'),
    ("five", '5'),
    ("six", '6'),
    ("seven", '7'),
    ("eight", '8'),
    ("nine", '9'),
    ("endash", '\u{2013}'),
    ("emdash", '\u{2014}'),
    ("bullet", '\u{2022}'),
    ("ellipsis", '\u{2026}'),
    ("dagger", '\u{2020}'),
    ("daggerdbl", '\u{2021}'),
    ("perthousand", '\u{2030}'),
    ("fraction", '\u{2044}'),
    ("fi", '\u{FB01}'),
    ("fl", '\u{FB02}'),
    ("ff", '\u{FB00}'),
    ("ffi", '\u{FB03}'),
    ("ffl", '\u{FB04}'),
    ("exclamdown", '¡'),
    ("questiondown", '¿'),
    ("cent", '¢'),
    ("sterling", '£'),
    ("currency", '¤'),
    ("yen", '¥'),
    ("brokenbar", '¦'),
    ("section", '§'),
    ("dieresis", '¨'),
    ("copyright", '©'),
    ("ordfeminine", 'ª'),
    ("logicalnot", '¬'),
    ("registered", '®'),
    ("macron", '¯'),
    ("degree", '°'),
    ("plusminus", '±'),
    ("acute", '´'),
    ("mu", 'µ'),
    ("paragraph", '¶'),
    ("periodcentered", '·'),
    ("cedilla", '¸'),
    ("ordmasculine", 'º'),
    ("onequarter", '¼'),
    ("onehalf", '½'),
    ("threequarters", '¾'),
    ("multiply", '×'),
    ("divide", '÷'),
    ("trademark", '\u{2122}'),
    ("Euro", '\u{20AC}'),
    ("florin", '\u{0192}'),
    ("circumflex", '\u{02C6}'),
    ("tilde", '\u{02DC}'),
    ("breve", '\u{02D8}'),
    ("dotaccent", '\u{02D9}'),
    ("ring", '\u{02DA}'),
    ("hungarumlaut", '\u{02DD}'),
    ("ogonek", '\u{02DB}'),
    ("caron", '\u{02C7}'),
    ("dotlessi", '\u{0131}'),
    ("Agrave", 'À'),
    ("Aacute", 'Á'),
    ("Acircumflex", 'Â'),
    ("Atilde", 'Ã'),
    ("Adieresis", 'Ä'),
    ("Aring", 'Å'),
    ("AE", 'Æ'),
    ("Ccedilla", 'Ç'),
    ("Egrave", 'È'),
    ("Eacute", 'É'),
    ("Ecircumflex", 'Ê'),
    ("Edieresis", 'Ë'),
    ("Igrave", 'Ì'),
    ("Iacute", 'Í'),
    ("Icircumflex", 'Î'),
    ("Idieresis", 'Ï'),
    ("Eth", 'Ð'),
    ("Ntilde", 'Ñ'),
    ("Ograve", 'Ò'),
    ("Oacute", 'Ó'),
    ("Ocircumflex", 'Ô'),
    ("Otilde", 'Õ'),
    ("Odieresis", 'Ö'),
    ("Oslash", 'Ø'),
    ("OE", '\u{0152}'),
    ("Ugrave", 'Ù'),
    ("Uacute", 'Ú'),
    ("Ucircumflex", 'Û'),
    ("Udieresis", 'Ü'),
    ("Yacute", 'Ý'),
    ("Ydieresis", '\u{0178}'),
    ("Thorn", 'Þ'),
    ("Scaron", '\u{0160}'),
    ("Zcaron", '\u{017D}'),
    ("Lslash", '\u{0141}'),
    ("germandbls", 'ß'),
    ("agrave", 'à'),
    ("aacute", 'á'),
    ("acircumflex", 'â'),
    ("atilde", 'ã'),
    ("adieresis", 'ä'),
    ("aring", 'å'),
    ("ae", 'æ'),
    ("ccedilla", 'ç'),
    ("egrave", 'è'),
    ("eacute", 'é'),
    ("ecircumflex", 'ê'),
    ("edieresis", 'ë'),
    ("igrave", 'ì'),
    ("iacute", 'í'),
    ("icircumflex", 'î'),
    ("idieresis", 'ï'),
    ("eth", 'ð'),
    ("ntilde", 'ñ'),
    ("ograve", 'ò'),
    ("oacute", 'ó'),
    ("ocircumflex", 'ô'),
    ("otilde", 'õ'),
    ("odieresis", 'ö'),
    ("oslash", 'ø'),
    ("oe", '\u{0153}'),
    ("ugrave", 'ù'),
    ("uacute", 'ú'),
    ("ucircumflex", 'û'),
    ("udieresis", 'ü'),
    ("yacute", 'ý'),
    ("ydieresis", 'ÿ'),
    ("thorn", 'þ'),
    ("scaron", '\u{0161}'),
    ("zcaron", '\u{017E}'),
    ("lslash", '\u{0142}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_itself_in_every_encoding() {
        for name in [None, Some("WinAnsiEncoding"), Some("MacRomanEncoding")] {
            let table = base_table(name);
            assert_eq!(table[usize::from(b'A')], 'A');
            assert_eq!(table[usize::from(b'~')], '~');
        }
    }

    #[test]
    fn the_upper_halves_differ_where_they_should() {
        assert_eq!(base_table(Some("WinAnsiEncoding"))[0xE9], 'é');
        assert_eq!(base_table(Some("MacRomanEncoding"))[0x8E], 'é');
        assert_eq!(base_table(Some("WinAnsiEncoding"))[0x93], '\u{201C}');
        assert_eq!(base_table(Some("MacRomanEncoding"))[0xD2], '\u{201C}');
        assert_eq!(base_table(Some("StandardEncoding"))[0xAA], '\u{201C}');
    }

    /// In the Standard set an apostrophe is really a right quotation mark.
    #[test]
    fn standard_encoding_rewrites_the_two_ascii_quotes() {
        let table = base_table(Some("StandardEncoding"));
        assert_eq!(table[0x27], '\u{2019}');
        assert_eq!(table[0x60], '\u{2018}');
    }

    #[test]
    fn resolves_glyph_names() {
        assert_eq!(glyph_to_char("eacute"), Some('é'));
        assert_eq!(glyph_to_char("space"), Some(' '));
        assert_eq!(glyph_to_char("A"), Some('A'));
        assert_eq!(glyph_to_char("seven"), Some('7'));
        assert_eq!(glyph_to_char("uni20AC"), Some('€'));
        assert_eq!(glyph_to_char("u1F600"), Some('\u{1F600}'));
        // A small-capital variant is still the letter it is a variant of.
        assert_eq!(glyph_to_char("a.sc"), Some('a'));
        assert_eq!(glyph_to_char("g123"), None);
    }

    /// The ligature names matter more than they look: a font that uses them
    /// turns "find" into "nd" if they are dropped.
    #[test]
    fn ligatures_resolve_to_their_own_characters() {
        assert_eq!(glyph_to_char("fi"), Some('\u{FB01}'));
        assert_eq!(glyph_to_char("ffl"), Some('\u{FB04}'));
    }
}

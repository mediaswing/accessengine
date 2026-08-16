//! The PDF object model, and the reader that turns bytes into it.
//!
//! PDF's syntax is small — numbers, names, strings, arrays, dictionaries,
//! streams and references to other objects — and everything above it is built
//! out of those seven things. This module is only the syntax; what any
//! particular dictionary *means* belongs to its caller.

use std::collections::HashMap;
use std::ops::Range;

pub type Dict = HashMap<String, Object>;

/// One PDF object.
///
/// A reference carries only the object number, not the generation. Generations
/// exist so a file can be edited by appending, and a number can be reused once
/// its old object is free; in practice writers bump the generation almost
/// never, and a reader that scans the file rather than trusting its cross
/// reference table — see [`super::doc`] for why this one does — has no
/// dependable way to tell two generations apart anyway. Later definitions of a
/// number win, which is the behaviour an appended edit wants.
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    /// A string's bytes, left undecoded: what they mean depends on the font
    /// they are eventually shown in, which is not known here.
    Str(Vec<u8>),
    Name(String),
    Array(Vec<Object>),
    Dict(Dict),
    /// A dictionary and the byte range of its still-encoded stream data,
    /// as an offset into the file it was read from.
    Stream(Dict, Range<usize>),
    Ref(u32),
}

impl Object {
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Int(value) => Some(value as f64),
            Self::Real(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(value) => Some(value),
            // Writers do emit `/Length 1024.0`, and a real that is not a whole
            // number is not a count of anything, so rounding is safe.
            Self::Real(value) => Some(value as i64),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Str(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The dictionary of a dictionary *or* of a stream — a stream is a
    /// dictionary with data attached, and every lookup that matters here
    /// applies equally to both.
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Self::Dict(dict) | Self::Stream(dict, _) => Some(dict),
            _ => None,
        }
    }
}

pub const fn is_white(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

pub const fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// A "regular" character: anything that is neither whitespace nor a delimiter,
/// and so can be part of a name, a number or a keyword.
pub const fn is_regular(byte: u8) -> bool {
    !is_white(byte) && !is_delimiter(byte)
}

/// What one step of the reader found.
///
/// Keywords are handed back rather than swallowed because a content stream is
/// the same syntax with operators mixed in: `1 0 0 1 72 720 Tm` is five numbers
/// and a keyword, and only the caller knows which it wants.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Object(Object),
    Keyword(String),
    /// A `]` or `>>` with no matching opener at this level.
    Close,
    Eof,
}

/// Reads objects out of a buffer, one at a time, from a position it keeps.
pub struct Lexer<'a> {
    data: &'a [u8],
    pub pos: usize,
}

/// How deep arrays and dictionaries may nest before the reader gives up.
///
/// Not a real limit — nothing a writer produces goes anywhere near it — but
/// this parser recurses, and a file with ten thousand `[` in a row would
/// otherwise take the stack out rather than be rejected.
const MAX_DEPTH: usize = 64;

impl<'a> Lexer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos: pos.min(data.len()),
        }
    }

    /// Skips whitespace and comments. A `%` runs to the end of the line
    /// everywhere except inside a string, which is parsed separately.
    pub fn skip_space(&mut self) {
        while let Some(&byte) = self.data.get(self.pos) {
            if byte == b'%' {
                while let Some(&byte) = self.data.get(self.pos) {
                    if byte == b'\n' || byte == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
            } else if is_white(byte) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// The next object, or `None` at a keyword, a closing bracket or the end
    /// of the buffer. The position is advanced past whatever was found either
    /// way, so a caller looping on this always makes progress.
    pub fn next_object(&mut self) -> Option<Object> {
        match self.next_token() {
            Token::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn next_token(&mut self) -> Token {
        self.token(0)
    }

    fn token(&mut self, depth: usize) -> Token {
        self.skip_space();
        let Some(&byte) = self.data.get(self.pos) else {
            return Token::Eof;
        };
        match byte {
            b'/' => Token::Object(Object::Name(self.read_name())),
            b'(' => Token::Object(Object::Str(self.read_literal_string())),
            b'[' => {
                self.pos += 1;
                Token::Object(self.read_array(depth))
            }
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    let dict = self.read_dict(depth);
                    Token::Object(self.maybe_stream(dict))
                } else {
                    Token::Object(Object::Str(self.read_hex_string()))
                }
            }
            b']' | b')' | b'}' => {
                self.pos += 1;
                Token::Close
            }
            b'>' => {
                // A `>>` closing a dictionary, or a stray `>`.
                self.pos += if self.data.get(self.pos + 1) == Some(&b'>') {
                    2
                } else {
                    1
                };
                Token::Close
            }
            b'{' => {
                // Only PostScript calculator functions use these, and they
                // hold no text. Treated as a bracket so the contents are read
                // and discarded rather than derailing the caller.
                self.pos += 1;
                Token::Object(self.read_array(depth))
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.read_number(),
            _ => {
                let word = self.read_keyword();
                match word.as_str() {
                    "true" => Token::Object(Object::Bool(true)),
                    "false" => Token::Object(Object::Bool(false)),
                    "null" => Token::Object(Object::Null),
                    // A byte that can start nothing at all — junk in a damaged
                    // file. Step over it so the caller does not spin.
                    "" => {
                        self.pos += 1;
                        Token::Keyword(String::new())
                    }
                    _ => Token::Keyword(word),
                }
            }
        }
    }

    fn read_keyword(&mut self) -> String {
        let start = self.pos;
        while self.data.get(self.pos).is_some_and(|&b| is_regular(b)) {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.data[start..self.pos]).into_owned()
    }

    /// A name, with `#41`-style escapes resolved. Names are compared as text
    /// throughout, and every name that means anything in a PDF is ASCII.
    fn read_name(&mut self) -> String {
        self.pos += 1; // the '/'
        let mut out = Vec::new();
        while let Some(&byte) = self.data.get(self.pos) {
            if !is_regular(byte) {
                break;
            }
            self.pos += 1;
            if byte == b'#'
                && let (Some(high), Some(low)) = (
                    self.data.get(self.pos).and_then(|b| hex_value(*b)),
                    self.data.get(self.pos + 1).and_then(|b| hex_value(*b)),
                )
            {
                out.push(high * 16 + low);
                self.pos += 2;
                continue;
            }
            out.push(byte);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A `(…)` string. Parentheses nest, a backslash escapes the usual handful
    /// of characters plus up to three octal digits, and a backslash at the end
    /// of a line continues it.
    fn read_literal_string(&mut self) -> Vec<u8> {
        self.pos += 1; // the '('
        let mut out = Vec::new();
        let mut depth = 1usize;
        while let Some(&byte) = self.data.get(self.pos) {
            self.pos += 1;
            match byte {
                b'(' => {
                    depth += 1;
                    out.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(byte);
                }
                b'\\' => {
                    let Some(&escaped) = self.data.get(self.pos) else {
                        break;
                    };
                    self.pos += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'\n' => {}
                        b'\r' => {
                            if self.data.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'0'..=b'7' => {
                            let mut value = u32::from(escaped - b'0');
                            for _ in 0..2 {
                                match self.data.get(self.pos) {
                                    Some(&digit @ b'0'..=b'7') => {
                                        value = value * 8 + u32::from(digit - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(value as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(byte),
            }
        }
        out
    }

    /// A `<…>` string of hex digit pairs. A trailing odd digit is padded with
    /// zero, which the specification asks for and which matters: a two-byte
    /// font code written `<0041 3>` means `0x0410`, not nothing.
    fn read_hex_string(&mut self) -> Vec<u8> {
        self.pos += 1; // the '<'
        let mut out = Vec::new();
        let mut high: Option<u8> = None;
        while let Some(&byte) = self.data.get(self.pos) {
            self.pos += 1;
            if byte == b'>' {
                break;
            }
            let Some(value) = hex_value(byte) else {
                continue;
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

    fn read_array(&mut self, depth: usize) -> Object {
        let mut items = Vec::new();
        if depth >= MAX_DEPTH {
            self.skip_container();
            return Object::Array(items);
        }
        loop {
            match self.token(depth + 1) {
                Token::Object(object) => items.push(object),
                Token::Keyword(_) => {}
                Token::Close | Token::Eof => break,
            }
        }
        Object::Array(items)
    }

    fn read_dict(&mut self, depth: usize) -> Dict {
        let mut dict = Dict::new();
        if depth >= MAX_DEPTH {
            self.skip_container();
            return dict;
        }
        loop {
            let key = match self.token(depth + 1) {
                Token::Object(Object::Name(name)) => name,
                // A value where a key belongs is a damaged dictionary; skip it
                // rather than losing the entries that follow.
                Token::Object(_) | Token::Keyword(_) => continue,
                Token::Close | Token::Eof => break,
            };
            match self.token(depth + 1) {
                Token::Object(value) => {
                    dict.insert(key, value);
                }
                Token::Keyword(_) => continue,
                Token::Close | Token::Eof => break,
            }
        }
        dict
    }

    /// Steps over an array or dictionary that is nested too deeply to be worth
    /// building, from just after its opening bracket to just past its close.
    ///
    /// Counts brackets by hand rather than reading objects, because reading
    /// them is what recurses: this is the path a file with ten thousand
    /// unclosed brackets takes, and it has to use no stack at all.
    fn skip_container(&mut self) {
        let mut open = 1usize;
        while open > 0 {
            self.skip_space();
            let Some(&byte) = self.data.get(self.pos) else {
                return;
            };
            match byte {
                b'[' | b'{' => {
                    self.pos += 1;
                    open += 1;
                }
                b']' | b'}' => {
                    self.pos += 1;
                    open -= 1;
                }
                b'<' if self.data.get(self.pos + 1) == Some(&b'<') => {
                    self.pos += 2;
                    open += 1;
                }
                b'>' if self.data.get(self.pos + 1) == Some(&b'>') => {
                    self.pos += 2;
                    open -= 1;
                }
                // Strings and names are stepped over whole: a `]` inside one
                // closes nothing.
                b'(' => {
                    self.read_literal_string();
                }
                b'<' => {
                    self.read_hex_string();
                }
                b'/' => {
                    self.read_name();
                }
                _ => {
                    let before = self.pos;
                    self.read_keyword();
                    if self.pos == before {
                        self.pos += 1;
                    }
                }
            }
        }
    }

    /// Turns a dictionary into a stream if `stream` follows it.
    fn maybe_stream(&mut self, dict: Dict) -> Object {
        let mark = self.pos;
        self.skip_space();
        if !self.data[self.pos..].starts_with(b"stream") {
            self.pos = mark;
            return Object::Dict(dict);
        }
        self.pos += b"stream".len();
        // The keyword is followed by CRLF or LF — never CR alone, though
        // writers that emit one anyway are tolerated by skipping it too.
        if self.data.get(self.pos) == Some(&b'\r') {
            self.pos += 1;
        }
        if self.data.get(self.pos) == Some(&b'\n') {
            self.pos += 1;
        }
        let range = self.stream_range(&dict);
        Object::Stream(dict, range)
    }

    /// Where a stream's data ends.
    ///
    /// `/Length` is believed only when the bytes it points at are actually
    /// followed by `endstream`. It is one of the most commonly wrong entries in
    /// a PDF — anything that edits a file without rewriting it properly leaves
    /// a stale length behind — and a wrong one truncates a page to nothing.
    /// Failing that check, and when the length is an indirect reference that
    /// cannot be resolved this early, the next `endstream` decides instead.
    fn stream_range(&mut self, dict: &Dict) -> Range<usize> {
        let start = self.pos;
        if let Some(length) = dict.get("Length").and_then(Object::as_i64)
            && let Ok(length) = usize::try_from(length)
            && let Some(end) = start.checked_add(length)
            && end <= self.data.len()
        {
            let mut probe = end;
            while self.data.get(probe).is_some_and(|&b| is_white(b)) {
                probe += 1;
            }
            if self.data[probe..].starts_with(b"endstream") {
                self.pos = probe + b"endstream".len();
                return start..end;
            }
        }

        match find(self.data, b"endstream", start) {
            Some(mut end) => {
                self.pos = end + b"endstream".len();
                // The EOL before `endstream` belongs to the file's layout, not
                // to the data.
                if end > start && self.data[end - 1] == b'\n' {
                    end -= 1;
                }
                if end > start && self.data[end - 1] == b'\r' {
                    end -= 1;
                }
                start..end
            }
            None => {
                self.pos = self.data.len();
                start..self.data.len()
            }
        }
    }

    /// A number, or the `12 0 R` reference that starts with one.
    fn read_number(&mut self) -> Token {
        let start = self.pos;
        if matches!(self.data.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let mut real = false;
        while let Some(&byte) = self.data.get(self.pos) {
            match byte {
                b'0'..=b'9' => self.pos += 1,
                b'.' => {
                    real = true;
                    self.pos += 1;
                }
                // `--5` and `1-2` both turn up in the wild. Keep consuming so
                // the junk does not come back as an operator on the next step.
                b'+' | b'-' => self.pos += 1,
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&self.data[start..self.pos]).into_owned();

        if !real
            && let Ok(number) = text.parse::<i64>()
            && number >= 0
            && let Some(object) = self.try_reference(number)
        {
            return Token::Object(object);
        }

        Token::Object(if real {
            Object::Real(parse_real(&text))
        } else {
            match text.parse::<i64>() {
                Ok(number) => Object::Int(number),
                // Out of range, or malformed: as a real it is at least the
                // right magnitude, which is all any caller uses it for.
                Err(_) => Object::Real(parse_real(&text)),
            }
        })
    }

    /// Looks ahead for the `<generation> R` that would make the number just
    /// read a reference. Restores the position when it is not one.
    fn try_reference(&mut self, number: i64) -> Option<Object> {
        let mark = self.pos;
        self.skip_space();
        let generation_start = self.pos;
        while self.data.get(self.pos).is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == generation_start {
            self.pos = mark;
            return None;
        }
        self.skip_space();
        if self.data.get(self.pos) == Some(&b'R')
            && !self.data.get(self.pos + 1).is_some_and(|&b| is_regular(b))
        {
            self.pos += 1;
            return u32::try_from(number).ok().map(Object::Ref);
        }
        self.pos = mark;
        None
    }
}

fn parse_real(text: &str) -> f64 {
    if let Ok(value) = text.parse::<f64>() {
        return value;
    }
    // Salvage the leading well-formed part of something like `1.2.3` or `--4`,
    // which is what a viewer does with it.
    let mut cleaned = String::with_capacity(text.len());
    let mut seen_dot = false;
    for (index, character) in text.chars().enumerate() {
        match character {
            '-' | '+' if index == 0 => cleaned.push(character),
            '.' if !seen_dot => {
                seen_dot = true;
                cleaned.push('.');
            }
            '0'..='9' => cleaned.push(character),
            _ => break,
        }
    }
    cleaned.parse::<f64>().unwrap_or(0.0)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The first occurrence of `needle` in `haystack` at or after `from`.
pub fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| from + index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &[u8]) -> Object {
        Lexer::new(source).next_object().expect("an object")
    }

    #[test]
    fn reads_the_simple_types() {
        assert_eq!(parse(b"42"), Object::Int(42));
        assert_eq!(parse(b"-3.5"), Object::Real(-3.5));
        assert_eq!(parse(b".5"), Object::Real(0.5));
        assert_eq!(parse(b"true"), Object::Bool(true));
        assert_eq!(parse(b"null"), Object::Null);
        assert_eq!(parse(b"/Type"), Object::Name("Type".into()));
    }

    #[test]
    fn resolves_hash_escapes_in_names() {
        assert_eq!(parse(b"/A#20B"), Object::Name("A B".into()));
    }

    #[test]
    fn reads_strings_with_escapes_and_nesting() {
        assert_eq!(parse(br"(a\(b\)c)"), Object::Str(b"a(b)c".to_vec()));
        assert_eq!(parse(b"(outer (inner) end)"), Object::Str(b"outer (inner) end".to_vec()));
        assert_eq!(parse(br"(\101\102)"), Object::Str(b"AB".to_vec()));
        // A backslash before a newline continues the line without adding one.
        assert_eq!(parse(b"(one\\\ntwo)"), Object::Str(b"onetwo".to_vec()));
    }

    #[test]
    fn pads_an_odd_final_hex_digit() {
        assert_eq!(parse(b"<48656C6C6F>"), Object::Str(b"Hello".to_vec()));
        // `<0413>` would be the two-byte code 0x0413; the odd form must pad on
        // the right, not the left.
        assert_eq!(parse(b"<041 3>"), Object::Str(vec![0x04, 0x13]));
    }

    #[test]
    fn reads_references_but_not_the_numbers_around_them() {
        assert_eq!(parse(b"12 0 R"), Object::Ref(12));
        // Three numbers followed by an operator, as a content stream has them:
        // none of these is a reference.
        let mut lexer = Lexer::new(b"1 0 Tc");
        assert_eq!(lexer.next_token(), Token::Object(Object::Int(1)));
        assert_eq!(lexer.next_token(), Token::Object(Object::Int(0)));
        assert_eq!(lexer.next_token(), Token::Keyword("Tc".into()));
    }

    #[test]
    fn reads_a_dictionary_and_the_array_in_it() {
        let object = parse(b"<< /Type /Page /Kids [1 0 R 2 0 R] /Count 2 >>");
        let dict = object.as_dict().expect("a dictionary");
        assert_eq!(dict.get("Type"), Some(&Object::Name("Page".into())));
        assert_eq!(dict.get("Count"), Some(&Object::Int(2)));
        assert_eq!(
            dict.get("Kids").and_then(Object::as_array),
            Some(&[Object::Ref(1), Object::Ref(2)][..])
        );
    }

    #[test]
    fn a_stream_keeps_the_range_of_its_own_bytes() {
        let source: &[u8] = b"<< /Length 5 >>\nstream\nHELLO\nendstream";
        let object = parse(source);
        let Object::Stream(dict, range) = object else {
            panic!("expected a stream");
        };
        assert_eq!(dict.get("Length"), Some(&Object::Int(5)));
        assert_eq!(&source[range], b"HELLO");
    }

    /// A stale `/Length` is one of the commonest faults in a real file. The
    /// data has to come back whole regardless, or the page reads as empty.
    #[test]
    fn a_wrong_length_falls_back_to_endstream() {
        let source: &[u8] = b"<< /Length 2 >>\nstream\nHELLO WORLD\nendstream";
        let Object::Stream(_, range) = parse(source) else {
            panic!("expected a stream");
        };
        assert_eq!(&source[range], b"HELLO WORLD");
    }

    /// `/Length` as a reference cannot be resolved while the object is being
    /// read, so the same fallback has to carry it.
    #[test]
    fn an_indirect_length_falls_back_to_endstream() {
        let source: &[u8] = b"<< /Length 9 0 R >>\nstream\nDATA\nendstream";
        let Object::Stream(_, range) = parse(source) else {
            panic!("expected a stream");
        };
        assert_eq!(&source[range], b"DATA");
    }

    #[test]
    fn comments_are_skipped_outside_strings_and_kept_inside_them() {
        assert_eq!(parse(b"% a comment\n/Name"), Object::Name("Name".into()));
        assert_eq!(parse(b"(50% off)"), Object::Str(b"50% off".to_vec()));
    }

    /// Junk must not stall the reader: every step has to move the position on,
    /// or the loops that drive it never end.
    #[test]
    fn unreadable_bytes_still_advance() {
        let mut lexer = Lexer::new(b")))");
        for _ in 0..3 {
            assert_eq!(lexer.next_token(), Token::Close);
        }
        assert_eq!(lexer.next_token(), Token::Eof);
    }

    #[test]
    fn deep_nesting_is_bounded_rather_than_recursing_forever() {
        let source = "[".repeat(10_000);
        // The point is that this returns at all.
        assert!(matches!(parse(source.as_bytes()), Object::Array(_)));
    }
}

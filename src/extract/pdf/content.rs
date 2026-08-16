//! Reading a page description back into readable text.
//!
//! A page is a little program: it sets a font, moves to a position, shows a
//! string, moves again. Nothing in it says where a word ends or a line begins
//! — those are things a reader *sees*, and a file that has been through a
//! typesetter has already thrown them away. Reconstructing them is the whole
//! job here, and it is guesswork with the odds stacked in favour of being
//! readable aloud.
//!
//! The rules, in the order they apply:
//!
//! - **Two strings shown with no move between them run together.** Whatever
//!   the second one is, it carries on exactly where the first stopped, so
//!   inserting anything would break a word in half.
//! - **A move to a different height is a new line**, and a move of more than
//!   about two lines is a paragraph. Both are measured against the size the
//!   text is actually being drawn at, so a footnote and a heading are each
//!   judged by their own scale.
//! - **A move along the same line is a space.** Typesetters reposition
//!   mid-line at word gaps — justified text, a tab, a table cell — and
//!   kerning, which is the exception, is not done this way. See
//!   [`SPACE_THRESHOLD`] for the case that is.
//!
//! Text comes out in the order the page draws it, which is the order it was
//! written in and very nearly always the order it should be read in. Sorting
//! by position instead would fix the rare file that draws its footer first and
//! ruin every file with a sidebar.

use super::doc::{Document, Page};
use super::font::Font;
use super::object::{Dict, Lexer, Object, Token, find, is_white};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// How wide a gap between two pieces of text on the same line has to be
/// before it is read as a space, as a fraction of the font size.
///
/// Files that leave the spaces out of their strings altogether are common —
/// it is what a typesetter does when it justifies a line by widening the gaps,
/// and what a program that positions every glyph does all the time. The number
/// has to sit above kerning, which rarely moves a glyph more than a twentieth
/// of an em, and below a real word gap, which is a quarter of an em or more.
/// A fifth of an em splits the difference; erring low is deliberate, since a
/// missing space welds two words into one that no voice can pronounce, while a
/// spare one is a pause nobody notices.
const MIN_SPACE_GAP: f64 = 0.2;

/// How far back text may start, relative to where the last string ended,
/// before it is treated as a separate thing rather than a continuation.
const MAX_BACKWARD_GAP: f64 = -1.0;

/// The same judgement for a font that never said how wide its glyphs are,
/// where the only thing known for certain is the size of the `TJ` adjustment
/// itself, in thousandths of the font size.
const UNMEASURED_SPACE_SHIFT: f64 = -170.0;

/// A `[a b c d e f]` transformation, as PDF writes them.
type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `first` applied, then `second`.
fn multiply(first: Matrix, second: Matrix) -> Matrix {
    [
        first[0] * second[0] + first[1] * second[2],
        first[0] * second[1] + first[1] * second[3],
        first[2] * second[0] + first[3] * second[2],
        first[2] * second[1] + first[3] * second[3],
        first[4] * second[0] + first[5] * second[2] + second[4],
        first[4] * second[1] + first[5] * second[3] + second[5],
    ]
}

/// Pulls the text out of pages, keeping the fonts it has already read.
pub struct Extractor<'a> {
    doc: &'a Document<'a>,
    fonts: HashMap<u32, Rc<Font>>,
    /// Character codes no font could account for, over the whole document.
    pub dropped: usize,
}

impl<'a> Extractor<'a> {
    pub fn new(doc: &'a Document<'a>) -> Self {
        Self {
            doc,
            fonts: HashMap::new(),
            dropped: 0,
        }
    }

    pub fn page_text(&mut self, page: &Page<'a>) -> String {
        let doc = self.doc;
        let Some(contents) = page.dict.get("Contents") else {
            return String::new();
        };

        // `/Contents` is one stream, or an array of them that are joined end
        // to end — and a single operator is allowed to be split across that
        // join, so they have to be concatenated before being read, not read
        // one at a time.
        let mut data = Vec::new();
        match doc.resolve(contents) {
            Object::Array(parts) => {
                for part in parts {
                    if let Ok(part) = doc.stream_data(part) {
                        data.extend_from_slice(&part);
                        data.push(b'\n');
                    }
                }
            }
            stream => match doc.stream_data(stream) {
                Ok(part) => data = part,
                Err(_) => return String::new(),
            },
        }

        let mut writer = Writer::default();
        let mut seen = HashSet::new();
        self.run(&data, page.resources, IDENTITY, &mut writer, &mut seen, 0);
        writer.text
    }

    /// Walks one content stream, keeping the text state as it goes.
    fn run(
        &mut self,
        content: &[u8],
        resources: Option<&'a Dict>,
        initial: Matrix,
        writer: &mut Writer,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) {
        let mut lexer = Lexer::new(content);
        let mut operands: Vec<Object> = Vec::new();
        let mut ctm = initial;
        let mut stack: Vec<Matrix> = Vec::new();
        let mut text = TextState::default();
        let mut font: Option<Rc<Font>> = None;

        loop {
            let keyword = match lexer.next_token() {
                Token::Object(object) => {
                    operands.push(object);
                    // No operator here takes more than six, and an array of
                    // operands with no operator to consume them is a damaged
                    // stream that must not be allowed to grow without limit.
                    if operands.len() > 8 {
                        operands.remove(0);
                    }
                    continue;
                }
                Token::Keyword(keyword) => keyword,
                Token::Close => continue,
                Token::Eof => break,
            };

            let number = |index: usize| -> f64 {
                operands
                    .len()
                    .checked_sub(index)
                    .and_then(|position| operands.get(position))
                    .and_then(Object::as_f64)
                    .unwrap_or(0.0)
            };

            match keyword.as_str() {
                "q" => stack.push(ctm),
                "Q" => ctm = stack.pop().unwrap_or(IDENTITY),
                "cm" => {
                    let matrix = [
                        number(6),
                        number(5),
                        number(4),
                        number(3),
                        number(2),
                        number(1),
                    ];
                    ctm = multiply(matrix, ctm);
                }

                "BT" => {
                    text.matrix = IDENTITY;
                    text.line = IDENTITY;
                    // A new text object starts somewhere unrelated to wherever
                    // the last one ended, so nothing may run on across it.
                    writer.end_run();
                }
                "ET" => writer.end_run(),

                "Tf" => {
                    text.size = number(1);
                    font = operands
                        .iter()
                        .rev()
                        .find_map(Object::as_name)
                        .map(|name| self.font_for(resources, name));
                }
                "TL" => text.leading = number(1),
                "Tc" => text.char_spacing = number(1),
                "Tw" => text.word_spacing = number(1),
                // Given as a percentage, and used as a multiplier.
                "Tz" => text.horizontal = number(1) / 100.0,
                "Ts" => text.rise = number(1),
                "Td" => text.offset(number(2), number(1), writer),
                "TD" => {
                    text.leading = -number(1);
                    text.offset(number(2), number(1), writer);
                }
                "Tm" => {
                    text.line = [
                        number(6),
                        number(5),
                        number(4),
                        number(3),
                        number(2),
                        number(1),
                    ];
                    text.matrix = text.line;
                    writer.moved = true;
                }
                "T*" => text.next_line(writer),

                "Tj" => {
                    if let Some(bytes) = operands.last().and_then(Object::as_bytes) {
                        self.show(bytes, &font, &mut text, ctm, writer);
                    }
                }
                "'" => {
                    text.next_line(writer);
                    if let Some(bytes) = operands.last().and_then(Object::as_bytes) {
                        self.show(bytes, &font, &mut text, ctm, writer);
                    }
                }
                "\"" => {
                    text.word_spacing = number(3);
                    text.char_spacing = number(2);
                    text.next_line(writer);
                    if let Some(bytes) = operands.last().and_then(Object::as_bytes) {
                        self.show(bytes, &font, &mut text, ctm, writer);
                    }
                }
                "TJ" => {
                    let Some(items) = operands.last().and_then(Object::as_array) else {
                        operands.clear();
                        continue;
                    };
                    let measured = font.as_ref().is_some_and(|font| font.has_widths());
                    for item in items.iter().cloned() {
                        match item {
                            Object::Str(bytes) => self.show(&bytes, &font, &mut text, ctm, writer),
                            other => {
                                let Some(shift) = other.as_f64() else {
                                    continue;
                                };
                                // The number is a distance to pull the next
                                // glyph back by, in thousandths of the font
                                // size — negative numbers push it forward,
                                // which is how a gap is written.
                                text.advance(-shift / 1000.0 * text.size * text.horizontal);
                                // With no glyph widths there is no pen to move
                                // meaningfully, and the shift itself is the
                                // only evidence of a gap there is.
                                if !measured && shift < UNMEASURED_SPACE_SHIFT {
                                    writer.space();
                                }
                            }
                        }
                    }
                }

                "Do" => {
                    if let Some(name) = operands.iter().rev().find_map(Object::as_name) {
                        let name = name.to_string();
                        self.run_xobject(&name, resources, ctm, writer, seen, depth);
                    }
                }
                // Inline image data is raw bytes in the middle of the stream,
                // and reading it as operators would produce nonsense until it
                // happened to resynchronise. Skip to the end of the image.
                "BI" => lexer.pos = end_of_inline_image(content, lexer.pos),

                _ => {}
            }
            operands.clear();
        }
    }

    /// Draws a form XObject, which is a content stream of its own with its own
    /// resources — headers, footers and anything placed by a layout program
    /// routinely live in one.
    fn run_xobject(
        &mut self,
        name: &str,
        resources: Option<&'a Dict>,
        ctm: Matrix,
        writer: &mut Writer,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) {
        /// Forms nest, legitimately, a few levels deep.
        const MAX_DEPTH: usize = 8;
        if depth >= MAX_DEPTH {
            return;
        }
        let doc = self.doc;
        let Some(entry) = resources
            .and_then(|resources| doc.get_dict(resources, "XObject"))
            .and_then(|xobjects| xobjects.get(name))
        else {
            return;
        };
        // A form that draws itself, directly or through another, would
        // otherwise never finish.
        let number = match entry {
            Object::Ref(number) => {
                if !seen.insert(*number) {
                    return;
                }
                Some(*number)
            }
            _ => None,
        };

        if let Some(dict) = doc.resolve(entry).as_dict()
            && doc.get_name(dict, "Subtype") == Some("Form")
            && let Ok(data) = doc.stream_data(entry)
        {
            let matrix = doc
                .get(dict, "Matrix")
                .and_then(Object::as_array)
                .filter(|values| values.len() == 6)
                .map_or(IDENTITY, |values| {
                    let mut matrix = IDENTITY;
                    for (slot, value) in matrix.iter_mut().zip(values) {
                        *slot = doc.resolve(value).as_f64().unwrap_or(0.0);
                    }
                    matrix
                });
            // A form's own resources, or the page's where it declares none.
            let inner = doc.get_dict(dict, "Resources").or(resources);
            writer.end_run();
            self.run(&data, inner, multiply(matrix, ctm), writer, seen, depth + 1);
            writer.end_run();
        }

        if let Some(number) = number {
            seen.remove(&number);
        }
    }

    /// Shows one string: decodes it, works out where it starts and where it
    /// leaves the pen, and moves the pen there.
    fn show(
        &mut self,
        bytes: &[u8],
        font: &Option<Rc<Font>>,
        text: &mut TextState,
        ctm: Matrix,
        writer: &mut Writer,
    ) {
        // A page that shows text without ever setting a font is broken, but
        // its bytes are usually plain WinAnsi and worth reading.
        let fallback;
        let font = match font {
            Some(font) => font.as_ref(),
            None => {
                fallback = Font::fallback();
                &fallback
            }
        };
        let decoded = font.decode(bytes);
        self.dropped += decoded.dropped;

        let (x, y, size) = text.placement(ctm);
        // Everything the pen moves for: the glyphs themselves, then the two
        // spacing settings, all stretched by the horizontal scale.
        let distance = (decoded.advance * text.size
            + text.char_spacing * decoded.codes as f64
            + text.word_spacing * decoded.spaces as f64)
            * text.horizontal;
        text.advance(distance);
        let (end_x, ..) = text.placement(ctm);

        writer.show(&decoded.text, Shown {
            x,
            y,
            size,
            end_x,
            measured: font.has_widths(),
        });
    }

    /// The font a resource name refers to, read once and then remembered: the
    /// same font is named on every page, and its `/ToUnicode` map can run to
    /// thousands of entries.
    fn font_for(&mut self, resources: Option<&'a Dict>, name: &str) -> Rc<Font> {
        let doc = self.doc;
        let Some(entry) = resources
            .and_then(|resources| doc.get_dict(resources, "Font"))
            .and_then(|fonts| fonts.get(name))
        else {
            return Rc::new(Font::fallback());
        };
        if let Object::Ref(number) = entry
            && let Some(font) = self.fonts.get(number)
        {
            return Rc::clone(font);
        }
        let font = Rc::new(match doc.resolve(entry).as_dict() {
            Some(dict) => Font::load(doc, dict),
            None => Font::fallback(),
        });
        if let Object::Ref(number) = entry {
            self.fonts.insert(*number, Rc::clone(&font));
        }
        font
    }
}

/// Where the next string will be drawn, and how wide it will be drawn.
struct TextState {
    /// The text matrix, which moves along as each glyph is placed.
    matrix: Matrix,
    /// The start of the current line, which is what `Td` and `T*` move from.
    line: Matrix,
    leading: f64,
    size: f64,
    /// `Tc` and `Tw`: extra space added after every glyph, and after every
    /// space. A typesetter justifies a line by raising the second of these, so
    /// leaving them out would put the pen in the wrong place on exactly the
    /// text that is hardest to read.
    char_spacing: f64,
    word_spacing: f64,
    /// `Tz`, as a multiplier rather than the percentage the operator takes.
    horizontal: f64,
    /// `Ts`: how far the text sits off its own baseline, which is what makes a
    /// superscript a superscript.
    rise: f64,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            matrix: IDENTITY,
            line: IDENTITY,
            leading: 0.0,
            size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal: 1.0,
            rise: 0.0,
        }
    }
}

impl TextState {
    fn offset(&mut self, x: f64, y: f64, writer: &mut Writer) {
        self.line = multiply([1.0, 0.0, 0.0, 1.0, x, y], self.line);
        self.matrix = self.line;
        writer.moved = true;
    }

    fn next_line(&mut self, writer: &mut Writer) {
        let leading = self.leading;
        self.offset(0.0, -leading, writer);
    }

    /// Moves the pen along the line by a distance already in text space.
    fn advance(&mut self, distance: f64) {
        self.matrix = multiply([1.0, 0.0, 0.0, 1.0, distance, 0.0], self.matrix);
    }

    /// Where the pen is, in the page's own coordinates, and how large the text
    /// there is.
    fn placement(&self, ctm: Matrix) -> (f64, f64, f64) {
        let placement = multiply(
            multiply([1.0, 0.0, 0.0, 1.0, 0.0, self.rise], self.matrix),
            ctm,
        );
        // The size the glyphs are actually drawn at, which is the font size
        // through both matrices — text set with `Tf 1` and scaled up by the
        // matrix is common enough that taking `Tf` at face value would put
        // every line break in the wrong place.
        let scale = (placement[0] * placement[3] - placement[1] * placement[2])
            .abs()
            .sqrt();
        (placement[4], placement[5], (self.size.abs() * scale).max(0.01))
    }
}

/// One string, placed on the page.
#[derive(Clone, Copy)]
struct Shown {
    x: f64,
    y: f64,
    size: f64,
    /// Where the pen finished, which is where the next string would start if
    /// nothing moved it.
    end_x: f64,
    /// Whether that end position was worked out from real glyph widths. When
    /// it was not it is an estimate, and too rough to measure a gap against.
    measured: bool,
}

/// Collects the text of one page, deciding what belongs between the pieces.
#[derive(Default)]
struct Writer {
    text: String,
    /// Where the last string was drawn.
    last: Option<Shown>,
    /// Whether anything has moved since that string. Until something does, the
    /// next string continues the same word.
    moved: bool,
    /// Whether the run of text was ended outright — by a new text object, or
    /// by a form drawn in the middle of the page. Unlike a move, this
    /// separates even two strings drawn at the same point, which is what a
    /// page that resets its matrix for every line does.
    broken: bool,
}

impl Writer {
    fn show(&mut self, chunk: &str, shown: Shown) {
        if chunk.is_empty() {
            // Nothing to write, but the position still counts: the next string
            // is the one that has to be separated from whatever came before.
            self.last = Some(shown);
            return;
        }
        if (self.moved || self.broken)
            && let Some(separator) = self.separator(&shown)
        {
            self.push_separator(separator);
        }
        self.text.push_str(chunk);
        self.last = Some(shown);
        self.moved = false;
        self.broken = false;
    }

    /// Ends the current run of text, so that whatever comes next is separated
    /// from it however the positions say it should be.
    fn end_run(&mut self) {
        self.moved = true;
        self.broken = true;
    }

    /// A gap wide enough to be a space, from a `TJ` adjustment.
    fn space(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with(char::is_whitespace) {
            self.text.push(' ');
        }
    }

    fn separator(&self, shown: &Shown) -> Option<&'static str> {
        let last = self.last?;
        // Judged against the larger of the two sizes: text dropping from a
        // heading to body text has moved a heading's worth of distance.
        let size = shown.size.max(last.size).max(1.0);
        let dropped = last.y - shown.y;

        if dropped.abs() < 0.4 * size {
            // The same line, so the question is how far this string starts
            // from where the last one ended.
            if shown.measured && last.measured {
                let gap = (shown.x - last.end_x) / size;
                let word_gap = gap > MIN_SPACE_GAP;
                // Text far enough back to be struck over what came before is
                // not continuing it — a column drawn out of order, or a
                // correction — and is still a break between two things.
                let overstruck = gap < MAX_BACKWARD_GAP;
                return (word_gap || overstruck).then_some(" ");
            }
            // Without widths, all that can be said is whether anything moved
            // at all. Within one run a move of nothing is a writer resetting
            // the matrix to where it already was, which is not a word gap;
            // across two runs it is a new piece of text regardless.
            return ((shown.x - last.x).abs() > 0.01 * size || self.broken).then_some(" ");
        }
        // Down by about one line is the next line; anything more, or a move
        // upwards, has left the run of text entirely — a new column, a new
        // block, or the header the page drew last.
        if dropped > 0.0 && dropped < 1.9 * size {
            Some("\n")
        } else {
            Some("\n\n")
        }
    }

    fn push_separator(&mut self, separator: &str) {
        if self.text.is_empty() {
            return;
        }
        if separator == " " {
            if !self.text.ends_with(char::is_whitespace) {
                self.text.push(' ');
            }
            return;
        }
        // A line break after a trailing space would leave the space dangling
        // at the end of the line.
        while self.text.ends_with(' ') || self.text.ends_with('\t') {
            self.text.pop();
        }
        let existing = self
            .text
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        for _ in existing..separator.len() {
            self.text.push('\n');
        }
    }
}

/// Finds the `EI` that ends an inline image, given the position just after
/// `BI`.
///
/// The data between `ID` and `EI` is raw bytes, which can perfectly well
/// contain those two letters, so the keyword only counts when it stands alone
/// between whitespace and is followed by something that could start an
/// operator.
fn end_of_inline_image(content: &[u8], from: usize) -> usize {
    let mut pos = from;
    while let Some(index) = find(content, b"EI", pos) {
        let before_is_space = index > 0 && is_white(content[index - 1]);
        let after_is_space = content
            .get(index + 2)
            .is_none_or(|&byte| is_white(byte) || byte == b'/' || byte == b'[');
        if before_is_space && after_is_space {
            return index + 2;
        }
        pos = index + 2;
    }
    content.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a content stream with no document behind it, which is enough for
    /// everything about spacing and line breaks.
    fn text_of(content: &str) -> String {
        let data = b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj";
        let doc = Document::parse(data).unwrap();
        let mut extractor = Extractor::new(&doc);
        let mut writer = Writer::default();
        let mut seen = HashSet::new();
        extractor.run(
            content.as_bytes(),
            None,
            IDENTITY,
            &mut writer,
            &mut seen,
            0,
        );
        writer.text
    }

    #[test]
    fn shows_a_simple_string() {
        assert_eq!(text_of("BT /F1 12 Tf (Hello) Tj ET"), "Hello");
    }

    /// The rule everything else is built on: with no move in between, two
    /// strings are two halves of the same word.
    #[test]
    fn strings_shown_without_moving_run_together() {
        assert_eq!(text_of("BT /F1 12 Tf (Hel) Tj (lo) Tj ET"), "Hello");
        assert_eq!(text_of("BT /F1 12 Tf [(Hel) (lo)] TJ ET"), "Hello");
    }

    #[test]
    fn a_wide_tj_adjustment_is_a_space_and_a_narrow_one_is_not() {
        assert_eq!(
            text_of("BT /F1 12 Tf [(Hello) -400 (world)] TJ ET"),
            "Hello world"
        );
        // Kerning between two letters, which must not become a space.
        assert_eq!(text_of("BT /F1 12 Tf [(A) -80 (V)] TJ ET"), "AV");
    }

    #[test]
    fn dropping_to_the_next_line_is_a_line_break() {
        assert_eq!(
            text_of("BT /F1 12 Tf 100 700 Td (first) Tj 0 -14 Td (second) Tj ET"),
            "first\nsecond"
        );
    }

    #[test]
    fn a_wider_gap_is_a_paragraph() {
        assert_eq!(
            text_of("BT /F1 12 Tf 100 700 Td (first) Tj 0 -40 Td (second) Tj ET"),
            "first\n\nsecond"
        );
    }

    #[test]
    fn moving_along_the_same_line_is_a_space() {
        assert_eq!(
            text_of("BT /F1 12 Tf 1 0 0 1 100 700 Tm (left) Tj 1 0 0 1 200 700 Tm (right) Tj ET"),
            "left right"
        );
    }

    /// A superscript sits slightly above the line it belongs to and is part of
    /// the same sentence.
    #[test]
    fn a_small_rise_stays_on_the_line() {
        assert_eq!(
            text_of("BT /F1 12 Tf 1 0 0 1 100 700 Tm (word) Tj 1 0 0 1 130 703 Tm (1) Tj ET"),
            "word 1"
        );
    }

    #[test]
    fn t_star_uses_the_leading() {
        assert_eq!(
            text_of("BT /F1 10 Tf 12 TL 72 720 Td (one) Tj T* (two) Tj T* (three) Tj ET"),
            "one\ntwo\nthree"
        );
    }

    /// `Tf 1` with the size in the matrix is how several writers do it, and
    /// judging the line spacing by the `Tf` value alone would call every line
    /// break a paragraph.
    #[test]
    fn the_size_comes_from_the_matrix_as_well_as_the_font() {
        assert_eq!(
            text_of("BT /F1 1 Tf 24 0 0 24 72 700 Tm (first) Tj 24 0 0 24 72 672 Tm (second) Tj ET"),
            "first\nsecond"
        );
    }

    /// The whole page can be drawn under a transformation, and the line
    /// spacing has to be measured after it, not before.
    #[test]
    fn the_current_transformation_matrix_scales_the_page() {
        assert_eq!(
            text_of("q 3 0 0 3 0 0 cm BT /F1 4 Tf 10 200 Td (first) Tj 0 -5 Td (second) Tj ET Q"),
            "first\nsecond"
        );
    }

    #[test]
    fn inline_image_data_is_skipped_rather_than_read_as_operators() {
        let content = "BT /F1 12 Tf 72 700 Td (before) Tj ET\n\
                       BI /W 2 /H 2 /F /AHx ID \x00(junk) Tj EI\n\
                       BT /F1 12 Tf 72 680 Td (after) Tj ET";
        assert_eq!(text_of(content), "before\nafter");
    }

    #[test]
    fn a_stream_that_stops_mid_operator_keeps_what_it_had() {
        assert_eq!(text_of("BT /F1 12 Tf (kept) Tj 0 -14"), "kept");
    }

    #[test]
    fn finds_the_end_of_an_inline_image() {
        // The image data itself contains the bytes `EI`, which must not end it.
        let content = b"BI /W 1 ID \xFF\xFFEI\xFF EI Q";
        let end = end_of_inline_image(content, 2);
        assert_eq!(&content[end..], b" Q");
    }
}

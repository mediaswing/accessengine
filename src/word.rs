//! Reading the words out of a Word document.
//!
//! The same split as PowerPoint's, and for the same reason — the format
//! changed in 2007 and the one before it is still in everybody's mailbox:
//!
//! - **`.docx`**, since 2007 — a zip archive of XML, the prose of the document
//!   in a single part.
//! - **`.doc`**, before that — a Compound File Binary container in which the
//!   text is not stored in reading order at all. It is stored in pieces, and a
//!   table elsewhere in the file says what order to put them back in.
//!
//! Only the body is read. Headers, footers, footnotes and comments each live
//! in a part or a stream of their own and are left there: a header repeats on
//! every page, and reading "Confidential — page 3 of 40" between paragraphs
//! would be the master-slide problem again. See [`crate::powerpoint`], which
//! leaves the master slides alone for the same reason.
//!
//! The extension is not trusted. A file is identified by what is inside it, so
//! a `.docx` that somebody renamed to `.doc` on the way out of a mail client —
//! which happens — still opens.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

use crate::cfb;
use crate::document::{flush, separate};
use crate::t;
use crate::xml::{decode_entities, Table};

/// A zip archive: every `.docx` is one.
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 0x03, 0x04];

/// The most XML this reader will pull out of a `.docx`, and the most text out
/// of a `.doc`. A zip says how large its contents are and is believed by
/// nobody; a piece table is a list of ranges and nothing stops a crafted one
/// from naming the same range ten thousand times. Both are how a few kilobytes
/// of input turn into an out-of-memory kill without a ceiling here.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_PIECES: usize = 100_000;

/// Whether this reader handles the given extension.
///
/// The macro-enabled and template variants are the same containers holding the
/// same parts, and a template is a document somebody may well want read to
/// them.
pub fn handles(extension: &str) -> bool {
    matches!(extension, "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm")
}

/// The text of a document, paragraph by paragraph.
pub fn text_from_file(path: &Path) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    text_from_bytes(&raw).with_context(|| format!("reading {}", path.display()))
}

fn text_from_bytes(raw: &[u8]) -> Result<String> {
    let paragraphs = if raw.starts_with(&ZIP_MAGIC) {
        modern_paragraphs(raw)?
    } else if raw.starts_with(&cfb::MAGIC) {
        legacy_paragraphs(raw)?
    } else {
        bail!(t!("error.not_a_document"));
    };
    if paragraphs.is_empty() {
        bail!(t!("error.document_empty"));
    }
    Ok(paragraphs.join("\n\n"))
}

// ------------------------------------------------------------------- .docx

/// The part every producer of a `.docx` writes the body into.
///
/// Strictly this should be found by following the package relationships to
/// whichever part is the main document. In practice that indirection has one
/// answer in every file anyone has ever been sent, and the reader says plainly
/// when it is not there rather than guessing at a different one.
const BODY_PART: &str = "word/document.xml";

fn modern_paragraphs(raw: &[u8]) -> Result<Vec<String>> {
    let cursor = std::io::Cursor::new(raw);
    let mut archive = zip::ZipArchive::new(cursor).with_context(|| t!("error.unreadable_zip"))?;
    let mut body = archive
        .by_name(BODY_PART)
        .map_err(|_| anyhow::anyhow!(t!("error.no_document_part")))?;

    let mut xml = String::new();
    // Capped as it is read, not after: the size a zip entry claims is a claim,
    // and reading to the end of a lie is the whole attack.
    body.by_ref()
        .take(MAX_DOCUMENT_BYTES)
        .read_to_string(&mut xml)
        .with_context(|| t!("error.document_not_text"))?;
    Ok(document_paragraphs(&xml))
}

/// The paragraphs of a document part's XML.
///
/// The same deliberately small scanner [`crate::powerpoint`] uses on a slide,
/// pointed at the other namespace: `<w:p>` is a paragraph and `<w:t>` a run of
/// text inside one, and a heading, a list item and a table cell all put their
/// words in exactly those.
///
/// Two elements are conspicuously absent, and their absence is the feature.
/// `<w:delText>` is text somebody deleted with track changes on, and
/// `<w:instrText>` is the machinery behind a field — `HYPERLINK "http://…"`,
/// `PAGE`, `REF _Toc41`. Neither was on the page the author saw, and matching
/// `<w:t>` by name rather than scooping up every leaf leaves both where they
/// are.
///
/// A table is read as a table, through the same prose builder a `.csv` goes
/// through, because the shape is the meaning — see
/// [`crate::document::records_to_prose`].
fn document_paragraphs(xml: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut table = Table::default();
    let mut rest = xml;

    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];

        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '/'])
            .next()
            .unwrap_or("");
        match name {
            // Nesting is counted rather than assumed away: a table inside a
            // table cell is legal here, unlike on a slide.
            "w:tbl" if !closing => {
                if table.depth == 0 {
                    flush(&mut current, &mut paragraphs);
                    table = Table::default();
                }
                table.depth += 1;
            }
            "w:tbl" => {
                table.depth = table.depth.saturating_sub(1);
                if table.depth == 0 {
                    paragraphs.extend(table.finish());
                    current.clear();
                }
            }
            "w:tr" if closing && table.depth > 0 => {
                table.rows.push(std::mem::take(&mut table.row));
            }
            "w:tc" if closing && table.depth > 0 => {
                table.row.push(current.trim().to_string());
                current.clear();
            }
            // A paragraph ends where the next begins, so both edges act: an
            // unclosed one still reaches the listener. Inside a cell there is
            // nowhere for a paragraph to go on its own, so the several a cell
            // may hold are run together instead.
            "w:p" if table.depth > 0 => separate(&mut current),
            "w:p" => flush(&mut current, &mut paragraphs),
            // A line break, a carriage return and a tab all stay inside their
            // paragraph as a space: a newline mid-paragraph makes several
            // speech back ends pause as though at a full stop.
            "w:br" | "w:cr" | "w:tab" => separate(&mut current),
            "w:t" if !closing => {
                let Some(end) = rest.find('<') else { break };
                current.push_str(&decode_entities(&rest[..end]));
                rest = &rest[end..];
            }
            _ => {}
        }
    }
    // A table left unclosed by a truncated file still says what it had.
    if table.depth > 0 {
        table.row.push(current.trim().to_string());
        table.rows.push(std::mem::take(&mut table.row));
        paragraphs.extend(table.finish());
    } else {
        flush(&mut current, &mut paragraphs);
    }
    paragraphs
}

// -------------------------------------------------------------------- .doc

/// The two bytes every File Information Block begins with.
const FIB_IDENT: u16 = 0xA5EC;

/// The first version of the FIB this reader understands. Word 97 wrote 193;
/// Word 6 and Word 95 wrote 101 through 104 and laid the block out differently
/// enough that reading one as the other yields noise rather than an error.
const FIB_FIRST_KNOWN: u16 = 193;

/// Flags in the FIB base, at offset 10.
const FIB_ENCRYPTED: u16 = 0x0100;
const FIB_TABLE_STREAM_1: u16 = 0x0200;

/// `fcClx` is the thirty-fourth of the FIB's file-offset/length pairs, and
/// `lcbClx` is the length beside it.
const FC_CLX: usize = 33 * 8;

/// The two markers a `Clx` is built from: a run of properties to be stepped
/// over, and the piece table itself.
const CLX_PROPERTIES: u8 = 0x01;
const CLX_PIECE_TABLE: u8 = 0x02;

/// Set in a piece's file offset when its text is eight-bit rather than UTF-16.
const PIECE_COMPRESSED: u32 = 0x4000_0000;
const PIECE_OFFSET: u32 = 0x3FFF_FFFF;

fn legacy_paragraphs(raw: &[u8]) -> Result<Vec<String>> {
    let file = cfb::CompoundFile::open(raw)?;
    let document = file
        .stream("WordDocument")?
        .with_context(|| t!("error.no_document_stream"))?;

    let word = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            document.get(at..at + 2)?.try_into().ok()?,
        ))
    };
    if word(0) != Some(FIB_IDENT) {
        bail!(t!("error.not_a_document"));
    }
    if word(2).unwrap_or(0) < FIB_FIRST_KNOWN {
        bail!(t!("error.document_too_old"));
    }
    let flags = word(10).unwrap_or(0);
    // Every piece of text in the file is ciphertext, and reading it would put
    // noise through a speech engine.
    if flags & FIB_ENCRYPTED != 0 {
        bail!(t!("error.document_locked"));
    }

    // The FIB is a fixed 32-byte base followed by four counted arrays, each
    // introduced by its own count. That is why the position of `fcClx` cannot
    // simply be written down: every count has to be read to know where the
    // next array starts.
    let malformed = || t!("error.document_malformed");
    let mut at = 32usize;
    for width in [2usize, 4] {
        let count = word(at).with_context(malformed)? as usize;
        at = at
            .checked_add(2)
            .and_then(|at| count.checked_mul(width).and_then(|len| at.checked_add(len)))
            .with_context(malformed)?;
    }
    let pairs = word(at).with_context(malformed)? as usize;
    let table_of_offsets = at.checked_add(2).with_context(malformed)?;
    if pairs * 8 < FC_CLX + 8 {
        bail!(t!("error.no_piece_table"));
    }

    let long = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            document.get(at..at + 4)?.try_into().ok()?,
        ))
    };
    let clx_at = long(table_of_offsets + FC_CLX).with_context(malformed)? as usize;
    let clx_len = long(table_of_offsets + FC_CLX + 4).with_context(malformed)? as usize;

    // Which of the two table streams is the live one flips every time the
    // document is saved, and the stale one is still sitting there beside it.
    let name = if flags & FIB_TABLE_STREAM_1 != 0 {
        "1Table"
    } else {
        "0Table"
    };
    let table = file
        .stream(name)?
        .with_context(|| t!("error.no_table_stream"))?;
    let clx = clx_at
        .checked_add(clx_len)
        .and_then(|end| table.get(clx_at..end))
        .with_context(|| t!("error.no_piece_table"))?;

    Ok(paragraphs_of(&reassemble(piece_table(clx)?, &document)?))
}

/// The `PlcPcd` at the end of a `Clx`.
///
/// A `Clx` is any number of property runs followed by exactly one piece table.
/// The runs are not wanted, but they are variable-length, so each has to be
/// measured in order to be stepped over.
fn piece_table(clx: &[u8]) -> Result<&[u8]> {
    let missing = || t!("error.no_piece_table");
    let mut at = 0usize;
    loop {
        match clx.get(at) {
            Some(&CLX_PROPERTIES) => {
                let size = clx
                    .get(at + 1..at + 3)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_le_bytes)
                    .with_context(missing)? as usize;
                at = at
                    .checked_add(3)
                    .and_then(|at| at.checked_add(size))
                    .with_context(missing)?;
            }
            Some(&CLX_PIECE_TABLE) => {
                let size = clx
                    .get(at + 1..at + 5)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                    .with_context(missing)? as usize;
                let start = at + 5;
                return start
                    .checked_add(size)
                    .and_then(|end| clx.get(start..end))
                    .with_context(missing);
            }
            _ => bail!(missing()),
        }
    }
}

/// Put the pieces back into reading order.
///
/// A `PlcPcd` is a list of character positions — one more than there are
/// pieces, so that every piece has a start and an end — followed by one
/// eight-byte descriptor per piece saying where in the `WordDocument` stream
/// that piece's bytes actually live, and in which of two encodings.
///
/// This indirection is why a `.doc` cannot be read by looking for runs of text
/// in it. Word wrote edits by appending them and adjusting this table, so the
/// order the bytes sit in is the order they were typed in, not the order they
/// are read in, and a heavily edited document is thoroughly out of sequence.
fn reassemble(plc: &[u8], document: &[u8]) -> Result<String> {
    let long = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(plc.get(at..at + 4)?.try_into().ok()?))
    };
    // n positions plus n descriptors, with one position left over.
    let pieces = plc.len().saturating_sub(4) / 12;
    if pieces == 0 {
        bail!(t!("error.no_piece_table"));
    }
    let descriptors = 4 * (pieces + 1);

    let mut out = String::new();
    for piece in 0..pieces.min(MAX_PIECES) {
        let from = long(piece * 4).unwrap_or(0);
        let to = long((piece + 1) * 4).unwrap_or(0);
        let characters = to.saturating_sub(from) as usize;
        if characters == 0 {
            continue;
        }

        let Some(raw) = long(descriptors + piece * 8 + 2) else {
            continue;
        };
        let compressed = raw & PIECE_COMPRESSED != 0;
        let (at, wanted) = if compressed {
            ((raw & PIECE_OFFSET) as usize / 2, characters)
        } else {
            ((raw & PIECE_OFFSET) as usize, characters * 2)
        };

        // A piece pointing past the end of a truncated file gives up what is
        // there rather than the whole document: half a letter read aloud beats
        // an error message about a byte offset.
        let Some(available) = document.get(at..) else {
            continue;
        };
        let bytes = &available[..wanted.min(available.len())];
        if compressed {
            out.push_str(&cp1252(bytes));
        } else {
            out.push_str(&cfb::utf16_le(bytes));
        }
        if out.len() > MAX_TEXT_BYTES {
            bail!(t!("error.document_too_large"));
        }
    }
    Ok(out)
}

/// The eight-bit pieces of a `.doc` are Windows-1252, not Latin-1.
///
/// The two agree everywhere except `0x80`–`0x9F`, which is exactly where the
/// characters Word's autocorrect produces live: the curly quotes, the en and
/// em dash, the ellipsis. Reading those as Latin-1 hands the speech engine an
/// unpronounceable control character in the middle of every quoted sentence.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

fn cp1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

/// Cut the reassembled text into paragraphs.
///
/// What comes back from the piece table is one long string in which the
/// structure is control characters: `\r` ends a paragraph, `\x07` ends a table
/// cell, and a handful of others stand in for things that are not text at all.
/// Every one of them would otherwise be handed to a speech engine as it
/// stands.
fn paragraphs_of(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    // Between a field's start and its separator sits the instruction that
    // produced it, and after the separator sits what the reader actually saw
    // on the page. The first is machinery and is dropped; the second is prose
    // and is kept.
    let mut instruction = false;

    for c in text.chars() {
        match c {
            '\u{13}' => instruction = true,
            '\u{14}' | '\u{15}' => instruction = false,
            _ if instruction => {}
            // The paragraph mark, the mark ending a table cell, and the breaks
            // that start a new page or column. A cell becomes a paragraph of
            // its own: which cells made up a row is held in properties this
            // reader does not parse, and inventing rows would be worse than
            // reading the cells in order.
            '\r' | '\n' | '\u{7}' | '\u{c}' | '\u{e}' => flush(&mut current, &mut paragraphs),
            // A soft line break, a tab and a non-breaking space all stay
            // inside their paragraph, as a space.
            '\u{b}' | '\t' | '\u{a0}' => separate(&mut current),
            '\u{1e}' => current.push('-'),
            // An optional hyphen says where a word may be broken across lines.
            // It is not part of the word.
            '\u{1f}' => {}
            // What is left below `0x20` anchors a picture, a footnote or a
            // drawn object. The thing itself is not text and its anchor should
            // not be read as though it were.
            c if (c as u32) < 0x20 => {}
            c => current.push(c),
        }
    }
    flush(&mut current, &mut paragraphs);
    paragraphs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document read in English.
    ///
    /// Pinned, because the way a table in one is read out comes from the
    /// language file, and a test switching the language beside this one would
    /// otherwise fail it.
    fn read(raw: &[u8]) -> String {
        crate::i18n::with_language("en", || text_from_bytes(raw)).expect("reads")
    }

    /// The same, for the files this reader turns away.
    fn refuse(raw: &[u8]) -> String {
        crate::i18n::with_language("en", || text_from_bytes(raw))
            .expect_err("this file should not be read")
            .to_string()
    }

    #[test]
    fn the_extensions_it_claims_are_the_ones_it_reads() {
        assert!(handles("docx"));
        assert!(handles("doc"));
        assert!(handles("dotx"));
        assert!(!handles("txt"));
        assert!(!handles("pptx"));
        assert!(!handles("DOCX"), "the caller lowercases first");
    }

    // ---------------------------------------------------------------- .docx

    /// A `.docx` is a zip of XML, so one can be built here rather than
    /// committed as a binary fixture nobody can review in a diff.
    fn docx(body: &str) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(BODY_PART, options).unwrap();
        zip.write_all(format!("<w:document><w:body>{body}</w:body></w:document>").as_bytes())
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// One paragraph of one run.
    fn para(text: &str) -> String {
        format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    #[test]
    fn runs_within_a_paragraph_are_joined_and_paragraphs_are_not() {
        let xml = "<w:p><w:r><w:t>Half a </w:t></w:r><w:r><w:t>sentence.</w:t></w:r></w:p>\
                   <w:p><w:r><w:t>Another.</w:t></w:r></w:p>";
        assert_eq!(document_paragraphs(xml), ["Half a sentence.", "Another."]);
    }

    #[test]
    fn a_line_break_within_a_paragraph_stays_within_it() {
        let xml = "<w:p><w:r><w:t>First line</w:t><w:br/><w:t>second line</w:t></w:r></w:p>";
        assert_eq!(document_paragraphs(xml), ["First line second line"]);
    }

    #[test]
    fn a_tab_between_words_does_not_run_them_together() {
        let xml = "<w:p><w:r><w:t>Name</w:t><w:tab/><w:t>Value</w:t></w:r></w:p>";
        assert_eq!(document_paragraphs(xml), ["Name Value"]);
    }

    #[test]
    fn entities_are_decoded_rather_than_spelled_out() {
        let xml = para("Fish &amp; chips &#8212; twice");
        assert_eq!(document_paragraphs(&xml), ["Fish & chips — twice"]);
    }

    #[test]
    fn attributes_and_other_elements_are_not_read_aloud() {
        let xml = "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>\
                   <w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Just this.</w:t></w:r></w:p>";
        assert_eq!(document_paragraphs(xml), ["Just this."]);
    }

    /// Text somebody deleted with track changes on is not text in the
    /// document, and neither is the instruction that produced a field. Both
    /// sit in elements beside `<w:t>` and neither is read.
    #[test]
    fn deleted_text_and_field_instructions_are_left_out() {
        let xml = "<w:p><w:r><w:t>Kept.</w:t></w:r>\
                   <w:del><w:r><w:delText>Struck out.</w:delText></w:r></w:del></w:p>\
                   <w:p><w:r><w:instrText>HYPERLINK \"http://example.com\"</w:instrText></w:r>\
                   <w:r><w:t>See the site.</w:t></w:r></w:p>";
        assert_eq!(document_paragraphs(xml), ["Kept.", "See the site."]);
    }

    #[test]
    fn a_table_in_a_document_is_read_under_its_headings() {
        let xml = format!(
            "{}<w:tbl>{}{}</w:tbl>{}",
            para("Before."),
            row(&["Region", "Sales"]),
            row(&["North", "1,200"]),
            para("After.")
        );
        let out = read(&docx(&xml));

        assert!(out.starts_with("Before."), "{out:?}");
        assert!(out.contains("A table of 1 row and 2 columns"), "{out:?}");
        assert!(out.contains("Region: North"), "{out:?}");
        assert!(out.contains("Sales: 1,200"), "{out:?}");
        assert!(out.trim_end().ends_with("After."), "{out:?}");
    }

    fn row(cells: &[&str]) -> String {
        let cells: String = cells
            .iter()
            .map(|cell| format!("<w:tc>{}</w:tc>", para(cell)))
            .collect();
        format!("<w:tr>{cells}</w:tr>")
    }

    #[test]
    fn a_cell_holding_several_paragraphs_is_still_one_cell() {
        let xml = format!(
            "<w:tbl>{}<w:tr><w:tc>{}{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
            row(&["Region", "Note"]),
            para("North"),
            para("and east"),
            para("Growing")
        );
        let out = read(&docx(&xml));
        assert!(out.contains("Region: North and east"), "{out:?}");
    }

    #[test]
    fn a_zip_with_no_document_part_is_not_a_word_document() {
        let empty = {
            use std::io::Write as _;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("word/settings.xml", options).unwrap();
            zip.write_all(b"<w:settings/>").unwrap();
            zip.finish().unwrap().into_inner()
        };
        assert!(
            refuse(&empty).contains("word/document.xml"),
            "{}",
            refuse(&empty)
        );
    }

    #[test]
    fn a_document_with_no_words_in_it_says_so() {
        let refusal = refuse(&docx("<w:p><w:r></w:r></w:p>"));
        assert!(refusal.contains("no text in this document"), "{refusal}");
    }

    #[test]
    fn something_that_is_not_a_document_says_so() {
        let refusal = refuse(b"Just a text file, really");
        assert!(refusal.contains("not a Word document"), "{refusal}");
    }

    // ----------------------------------------------------------------- .doc

    /// Builds a compound file holding several named streams, which is what a
    /// `.doc` is: the document beside the table that says how to read it.
    ///
    /// Every stream is put in the mini stream, which is where Word puts
    /// anything under 4 KB and where everything these tests build belongs.
    /// [`crate::powerpoint`] has the sector-by-sector variant for the case
    /// this one does not cover.
    fn compound_file(streams: &[(&str, &[u8])]) -> Vec<u8> {
        const SECTOR: usize = 512;
        const MINI: usize = 64;
        const END: u32 = 0xFFFF_FFFE;

        // Where each stream starts inside the mini stream, in mini sectors.
        let mut starts = Vec::new();
        let mut used = 0usize;
        for (_, contents) in streams {
            starts.push(used);
            used += contents.len().div_ceil(MINI).max(1);
        }
        let mini_bytes = used * MINI;
        let mini_sectors = mini_bytes.div_ceil(SECTOR).max(1);

        // Sector 0 is the FAT, 1 the directory, 2 the mini FAT, and the mini
        // stream itself follows.
        let sectors = 3 + mini_sectors;
        let mut file = vec![0u8; SECTOR * (1 + sectors)];
        file[..8].copy_from_slice(&cfb::MAGIC);
        file[26..28].copy_from_slice(&3u16.to_le_bytes()); // minor version
        file[28..30].copy_from_slice(&[0xFE, 0xFF]); // little-endian
        file[30..32].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
        file[32..34].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors
        file[44..48].copy_from_slice(&1u32.to_le_bytes()); // one FAT sector
        file[48..52].copy_from_slice(&1u32.to_le_bytes()); // directory at sector 1
        file[56..60].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
        file[60..64].copy_from_slice(&2u32.to_le_bytes()); // mini FAT at sector 2
        file[64..68].copy_from_slice(&1u32.to_le_bytes()); // one mini FAT sector
        file[68..72].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes()); // no extra DIFAT
        file[76..80].copy_from_slice(&0u32.to_le_bytes()); // the FAT is sector 0

        let put = |file: &mut Vec<u8>, sector: usize, index: usize, value: u32| {
            let base = SECTOR * (1 + sector) + index * 4;
            file[base..base + 4].copy_from_slice(&value.to_le_bytes());
        };
        for index in 0..SECTOR / 4 {
            put(&mut file, 0, index, 0xFFFF_FFFF); // free
            put(&mut file, 2, index, 0xFFFF_FFFF);
        }
        put(&mut file, 0, 0, 0xFFFF_FFFD); // sector 0 is the FAT
        put(&mut file, 0, 1, END); // the directory ends here
        put(&mut file, 0, 2, END); // so does the mini FAT
        for sector in 3..sectors {
            let last = sector + 1 == sectors;
            put(
                &mut file,
                0,
                sector,
                if last { END } else { sector as u32 + 1 },
            );
        }

        // The mini FAT chains each stream's own 64-byte sectors, one chain per
        // stream, each ending where the stream does.
        for (index, (_, contents)) in streams.iter().enumerate() {
            let start = starts[index];
            let length = contents.len().div_ceil(MINI).max(1);
            for step in 0..length {
                let last = step + 1 == length;
                let at = start + step;
                put(&mut file, 2, at, if last { END } else { at as u32 + 1 });
            }
        }

        let entry =
            |file: &mut Vec<u8>, index: usize, name: &str, kind: u8, start: u32, size: u64| {
                let base = SECTOR * 2 + index * cfb::DIRECTORY_ENTRY_BYTES;
                let units: Vec<u8> = name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                file[base..base + units.len()].copy_from_slice(&units);
                file[base + 64..base + 66].copy_from_slice(&(units.len() as u16).to_le_bytes());
                file[base + 66] = kind;
                file[base + 68..base + 80].copy_from_slice(&[0xFF; 12]); // no siblings, no child
                file[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
                file[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
            };
        entry(&mut file, 0, "Root Entry", 5, 3, mini_bytes as u64);
        for (index, (name, contents)) in streams.iter().enumerate() {
            entry(
                &mut file,
                index + 1,
                name,
                2,
                starts[index] as u32,
                contents.len() as u64,
            );
        }

        // The mini stream itself, laid out at the offsets the entries name.
        for (index, (_, contents)) in streams.iter().enumerate() {
            let at = SECTOR * 4 + starts[index] * MINI;
            file[at..at + contents.len()].copy_from_slice(contents);
        }
        file
    }

    /// Where this fixture puts the text, well past the header it writes.
    const TEXT_AT: usize = 2048;

    /// A `WordDocument` stream: a File Information Block with `fcClx` pointing
    /// into the table stream, and the text sitting at [`TEXT_AT`].
    fn word_document(flags: u16, clx_at: u32, clx_len: u32, text: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; TEXT_AT];
        out[0..2].copy_from_slice(&FIB_IDENT.to_le_bytes());
        out[2..4].copy_from_slice(&FIB_FIRST_KNOWN.to_le_bytes());
        out[10..12].copy_from_slice(&flags.to_le_bytes());

        // The counted arrays, at the sizes Word 97 wrote them.
        let mut at = 32;
        for (count, width) in [(14usize, 2usize), (22, 4)] {
            out[at..at + 2].copy_from_slice(&(count as u16).to_le_bytes());
            at += 2 + count * width;
        }
        out[at..at + 2].copy_from_slice(&93u16.to_le_bytes());
        at += 2;
        out[at + FC_CLX..at + FC_CLX + 4].copy_from_slice(&clx_at.to_le_bytes());
        out[at + FC_CLX + 4..at + FC_CLX + 8].copy_from_slice(&clx_len.to_le_bytes());

        out.extend_from_slice(text);
        out
    }

    /// A piece: where its bytes are in the `WordDocument` stream, how many
    /// characters they make, and which of the two encodings they are in.
    struct Piece {
        at: usize,
        characters: usize,
        compressed: bool,
    }

    /// A table stream holding nothing but a piece table.
    fn table_stream(pieces: &[Piece]) -> Vec<u8> {
        let mut positions = Vec::new();
        let mut descriptors = Vec::new();
        let mut cp = 0u32;
        for piece in pieces {
            positions.extend_from_slice(&cp.to_le_bytes());
            cp += piece.characters as u32;

            let raw = if piece.compressed {
                (piece.at as u32 * 2) | PIECE_COMPRESSED
            } else {
                piece.at as u32
            };
            descriptors.extend_from_slice(&[0, 0]); // the properties, unused here
            descriptors.extend_from_slice(&raw.to_le_bytes());
            descriptors.extend_from_slice(&[0, 0]);
        }
        positions.extend_from_slice(&cp.to_le_bytes());

        let plc: Vec<u8> = positions.into_iter().chain(descriptors).collect();
        let mut out = vec![CLX_PIECE_TABLE];
        out.extend_from_slice(&(plc.len() as u32).to_le_bytes());
        out.extend_from_slice(&plc);
        out
    }

    fn doc(flags: u16, text: &[u8], pieces: &[Piece]) -> Vec<u8> {
        let table = table_stream(pieces);
        let name = if flags & FIB_TABLE_STREAM_1 != 0 {
            "1Table"
        } else {
            "0Table"
        };
        let document = word_document(flags, 0, table.len() as u32, text);
        compound_file(&[("WordDocument", &document), (name, &table)])
    }

    /// The point of the piece table: the bytes in the file are in the order
    /// they were typed, and only the table says what order they are read in.
    #[test]
    fn a_doc_reads_its_pieces_in_the_order_the_table_gives_them() {
        // "world." is stored before "Hello, " in the stream.
        let text = b"world.Hello, ";
        let out = read(&doc(
            0,
            text,
            &[
                Piece {
                    at: TEXT_AT + 6,
                    characters: 7,
                    compressed: true,
                },
                Piece {
                    at: TEXT_AT,
                    characters: 6,
                    compressed: true,
                },
            ],
        ));
        assert_eq!(out, "Hello, world.");
    }

    #[test]
    fn a_paragraph_mark_ends_a_paragraph_and_a_line_break_does_not() {
        let text = b"First para.\rSecond\x0bpara.\r";
        let out = read(&doc(
            0,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert_eq!(out, "First para.\n\nSecond para.");
    }

    /// The eight-bit pieces are Windows-1252, where Word's autocorrect puts
    /// its quotation marks and dashes. Read as Latin-1 these are control
    /// characters.
    #[test]
    fn the_curly_quotes_word_inserts_survive_the_trip() {
        let text = b"He said \x93yes\x94 \x97 twice.";
        let out = read(&doc(
            0,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert_eq!(out, "He said \u{201C}yes\u{201D} \u{2014} twice.");
    }

    #[test]
    fn a_utf16_piece_reads_as_the_letters_it_holds() {
        let utf16: Vec<u8> = "Café\rDeuxième"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let out = read(&doc(
            0,
            &utf16,
            &[Piece {
                at: TEXT_AT,
                characters: 13,
                compressed: false,
            }],
        ));
        assert_eq!(out, "Café\n\nDeuxième");
    }

    /// A field is an instruction and a result. The instruction is machinery
    /// and the result is what was on the page.
    #[test]
    fn a_fields_instruction_is_dropped_and_its_result_is_kept() {
        let text = b"See \x13HYPERLINK \"http://example.com\"\x14our site\x15 for more.";
        let out = read(&doc(
            0,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert_eq!(out, "See our site for more.");
    }

    #[test]
    fn a_table_cell_mark_ends_a_paragraph() {
        let text = b"Region\x07North\x07\rRest of it.\r";
        let out = read(&doc(
            0,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert_eq!(out, "Region\n\nNorth\n\nRest of it.");
    }

    /// Which of the two table streams is live flips on every save, and the
    /// stale one is still sitting beside it.
    #[test]
    fn the_flag_decides_which_of_the_two_table_streams_is_read() {
        let text = b"From the second table stream.";
        let out = read(&doc(
            FIB_TABLE_STREAM_1,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert_eq!(out, "From the second table stream.");
    }

    #[test]
    fn a_password_protected_document_says_so_rather_than_reading_noise() {
        let text = b"ciphertext";
        let refusal = refuse(&doc(
            FIB_ENCRYPTED,
            text,
            &[Piece {
                at: TEXT_AT,
                characters: text.len(),
                compressed: true,
            }],
        ));
        assert!(refusal.contains("password-protected"), "{refusal}");
    }

    #[test]
    fn a_document_from_word_95_says_so_rather_than_reading_noise() {
        let mut document = word_document(0, 0, 0, b"anything");
        document[2..4].copy_from_slice(&104u16.to_le_bytes());
        let refusal = refuse(&compound_file(&[
            ("WordDocument", &document),
            ("0Table", &[]),
        ]));
        assert!(refusal.contains("Word 95 or earlier"), "{refusal}");
    }

    #[test]
    fn a_compound_file_that_is_not_a_document_says_so() {
        let refusal = refuse(&compound_file(&[("Workbook", b"not a document")]));
        assert!(refusal.contains("no Word Document stream"), "{refusal}");
    }

    /// A piece pointing past the end of a truncated file gives up what is
    /// there rather than the whole document.
    #[test]
    fn a_piece_running_past_the_end_of_the_file_does_not_panic() {
        let text = b"All that survived.";
        let out = read(&doc(
            0,
            text,
            &[
                Piece {
                    at: TEXT_AT,
                    characters: text.len(),
                    compressed: true,
                },
                Piece {
                    at: 900_000,
                    characters: 40,
                    compressed: true,
                },
            ],
        ));
        assert_eq!(out, "All that survived.");
    }

    #[test]
    fn a_truncated_file_does_not_panic() {
        let whole = doc(
            0,
            b"Something to cut short.",
            &[Piece {
                at: TEXT_AT,
                characters: 23,
                compressed: true,
            }],
        );
        for length in (0..whole.len()).step_by(97) {
            let _ = crate::i18n::with_language("en", || text_from_bytes(&whole[..length]));
        }
    }
}

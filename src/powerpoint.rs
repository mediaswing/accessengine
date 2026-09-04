//! Reading the words out of a PowerPoint presentation.
//!
//! Two formats wear the same icon, and both still turn up in a mailbox:
//!
//! - **`.pptx`**, since 2007 — a zip archive of XML, one file per slide.
//! - **`.ppt`**, before that — a Compound File Binary container holding one
//!   long stream of nested binary records.
//!
//! Neither is read in full. What this module wants is the text on the slides,
//! in the order the slides come in, and nothing else: no themes, no layouts,
//! no shape geometry. The master slides are deliberately left out, because
//! their placeholder text is "Click to edit Master title style" and reading
//! that between every slide would be worse than reading nothing at all.
//!
//! The extension is not trusted. A file is identified by what is inside it, so
//! a `.pptx` that somebody renamed to `.ppt` on the way out of a mail client —
//! which happens — still opens.

use anyhow::{bail, Context, Result};
use std::io::Read;

use crate::cfb;
use crate::document::{flush, separate};
use crate::t;
use crate::xml::{decode_entities, Table};
use std::path::Path;

/// A zip archive: every `.pptx` is one.
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 0x03, 0x04];
/// Ceilings on what a presentation may expand to. A zip archive says how large
/// its contents are and is believed by nobody: these are what stops a few
/// kilobytes of cleverly compressed nothing from filling this process's memory.
const MAX_SLIDES: usize = 5_000;
const MAX_SLIDE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// How deep the record tree in a `.ppt` may nest before the file is called
/// malformed. Real presentations use about six levels; the limit is here
/// because the walk is recursive and the file comes from outside.
const MAX_RECORD_DEPTH: usize = 32;

/// Whether this reader handles the given extension.
pub fn handles(extension: &str) -> bool {
    matches!(extension, "ppt" | "pptx" | "pptm" | "pps" | "ppsx")
}

/// The text of a presentation, slide by slide.
pub fn text_from_file(path: &Path) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    text_from_bytes(&raw).with_context(|| format!("reading {}", path.display()))
}

fn text_from_bytes(raw: &[u8]) -> Result<String> {
    let slides = if raw.starts_with(&ZIP_MAGIC) {
        modern_slides(raw)?
    } else if raw.starts_with(&cfb::MAGIC) {
        legacy_slides(raw)?
    } else {
        bail!(t!("error.not_a_presentation"));
    };
    Ok(lay_out(&slides))
}

/// Slides as prose: each one announced by number, its paragraphs beneath.
///
/// A slide with nothing on it says so rather than being skipped. Someone
/// following along is counting, and a deck whose fourth slide is one
/// photograph should not renumber the fifth.
fn lay_out(slides: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (index, paragraphs) in slides.iter().enumerate() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let number = index + 1;
        if paragraphs.is_empty() {
            out.push_str(&t!("slide.empty", number = number));
            continue;
        }
        out.push_str(&t!("slide.heading", number = number));
        for paragraph in paragraphs {
            out.push_str("\n\n");
            out.push_str(paragraph);
        }
    }
    out
}

// ------------------------------------------------------------------- .pptx

/// The slides of a `.pptx`, in the order they are shown.
fn modern_slides(raw: &[u8]) -> Result<Vec<Vec<String>>> {
    let cursor = std::io::Cursor::new(raw);
    let mut archive = zip::ZipArchive::new(cursor).with_context(|| t!("error.unreadable_zip"))?;

    // `ppt/slides/slide12.xml`. Sorted by the number rather than by the name,
    // or slide 10 would be read second — the archive's own order is whatever
    // the program that wrote it happened to use.
    let mut names: Vec<(u32, String)> = (0..archive.len())
        .filter_map(|index| archive.name_for_index(index).map(str::to_string))
        .filter_map(|name| slide_number(&name).map(|number| (number, name)))
        .collect();
    if names.is_empty() {
        bail!(t!("error.no_slides"));
    }
    names.sort();
    names.truncate(MAX_SLIDES);

    let mut total = 0u64;
    let mut slides = Vec::with_capacity(names.len());
    for (_, name) in names {
        let mut entry = archive
            .by_name(&name)
            .with_context(|| format!("reading {name}"))?;
        let mut xml = String::new();
        // Capped as it is read, not after: the size a zip entry claims is a
        // claim, and reading to the end of a lie is the whole attack.
        entry
            .by_ref()
            .take(MAX_SLIDE_BYTES)
            .read_to_string(&mut xml)
            .with_context(|| format!("{name} is not readable as text"))?;
        // Counted from what was actually read, for the same reason: an archive
        // that declares every slide as empty and then hands over eight
        // megabytes of each would otherwise never reach this budget at all,
        // and five thousand slides of it would still be held in memory.
        total += xml.len() as u64;
        if total > MAX_TOTAL_BYTES {
            bail!(t!("error.slides_too_large"));
        }
        slides.push(slide_paragraphs(&xml));
    }
    Ok(slides)
}

/// The number in `ppt/slides/slide12.xml`, for the entries that are slides.
///
/// `ppt/slides/_rels/slide12.xml.rels` sits alongside them and is not one, so
/// the path is matched whole rather than by its ends.
fn slide_number(name: &str) -> Option<u32> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

/// The paragraphs of one slide's XML.
///
/// A deliberately small scanner rather than an XML parser. Almost everything
/// wanted here is in two elements — `<a:p>` is a paragraph and `<a:t>` is a run
/// of text inside one — and a text box, a title and a table cell all put their
/// words in exactly those, so the shape they sit in never has to be understood.
///
/// A table is the exception, and it has to be understood, because the shape is
/// the meaning. `Region · Sales · North · 1,200` read as four paragraphs is
/// four unrelated words; the same four cells read as a table say which figure
/// belongs to which column. So `<a:tbl>` and the rows and cells inside it are
/// tracked, and what comes out goes through the same prose builder a `.csv`
/// does — see [`crate::document::records_to_prose`].
fn slide_paragraphs(xml: &str) -> Vec<String> {
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
            // Nesting is counted rather than assumed away: DrawingML has no
            // table inside a table, but this reads files written by anything.
            "a:tbl" if !closing => {
                if table.depth == 0 {
                    flush(&mut current, &mut paragraphs);
                    table = Table::default();
                }
                table.depth += 1;
            }
            "a:tbl" => {
                table.depth = table.depth.saturating_sub(1);
                if table.depth == 0 {
                    paragraphs.extend(table.finish());
                    current.clear();
                }
            }
            "a:tr" if closing && table.depth > 0 => {
                table.rows.push(std::mem::take(&mut table.row));
            }
            "a:tc" if closing && table.depth > 0 => {
                table.row.push(current.trim().to_string());
                current.clear();
            }
            // A paragraph ends where the next begins, so both edges act: an
            // unclosed one still reaches the listener. Inside a cell there is
            // nowhere for a paragraph to go on its own, so the several a cell
            // may hold are run together instead.
            "a:p" if table.depth > 0 => separate(&mut current),
            "a:p" => flush(&mut current, &mut paragraphs),
            // A soft line break inside a paragraph. Kept as a space, because a
            // newline mid-paragraph makes several speech back ends pause as
            // though at a full stop.
            "a:br" => separate(&mut current),
            "a:t" if !closing => {
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

// -------------------------------------------------------------------- .ppt

/// Record types from [MS-PPT]. Only the four that say where the text is.
const SLIDE_CONTAINER: u16 = 0x03EE;
const TEXT_CHARS_ATOM: u16 = 0x0FA0;
const TEXT_BYTES_ATOM: u16 = 0x0FA8;
/// Present when the presentation was saved with a password, in which case
/// every text atom in the file is ciphertext.
const CRYPT_SESSION_CONTAINER: u16 = 0x2F14;

/// The slides of a `.ppt`.
///
/// Slides come back in the order they sit in the stream, which is the order
/// they are shown in every file this has been tried on but is not guaranteed
/// to be: the authoritative order is a chain of persist directories, and
/// following it would be several hundred lines to correct the running order of
/// a deck that has been heavily re-edited.
fn legacy_slides(raw: &[u8]) -> Result<Vec<Vec<String>>> {
    let file = cfb::CompoundFile::open(raw)?;
    let stream = file
        .stream("PowerPoint Document")?
        .with_context(|| t!("error.no_powerpoint_stream"))?;

    let mut slides: Vec<Vec<String>> = Vec::new();
    walk_records(&stream, 0, None, &mut slides)?;
    Ok(slides)
}

/// Walk the record tree, collecting the text under each slide.
///
/// `slide` is the index of the slide being walked through, or `None` above
/// them all — which is where the masters and the note pages live, and why
/// their placeholder text never reaches the listener.
fn walk_records(
    buffer: &[u8],
    depth: usize,
    slide: Option<usize>,
    slides: &mut Vec<Vec<String>>,
) -> Result<()> {
    if depth > MAX_RECORD_DEPTH {
        bail!(t!("error.records_too_deep"));
    }
    let mut at = 0usize;
    while at + 8 <= buffer.len() {
        let version_instance = u16::from_le_bytes([buffer[at], buffer[at + 1]]);
        let record_type = u16::from_le_bytes([buffer[at + 2], buffer[at + 3]]);
        let length = u32::from_le_bytes([
            buffer[at + 4],
            buffer[at + 5],
            buffer[at + 6],
            buffer[at + 7],
        ]) as usize;
        at += 8;
        // A length running past the end is a truncated file, not a reason to
        // abandon everything already read.
        let end = at.saturating_add(length).min(buffer.len());
        let body = &buffer[at..end];
        at = end;

        if record_type == CRYPT_SESSION_CONTAINER {
            bail!(t!("error.presentation_locked"));
        }
        // A `recVer` of 0xF marks a container; anything else is a leaf.
        if version_instance & 0x000F == 0x000F {
            let slide = if record_type == SLIDE_CONTAINER {
                if slides.len() >= MAX_SLIDES {
                    return Ok(());
                }
                slides.push(Vec::new());
                Some(slides.len() - 1)
            } else {
                slide
            };
            walk_records(body, depth + 1, slide, slides)?;
            continue;
        }

        let Some(index) = slide else { continue };
        let text = match record_type {
            TEXT_CHARS_ATOM => Some(cfb::utf16_le(body)),
            // "ANSI", which in practice means the code page the deck was
            // written on. Latin-1 is the one that leaves western European text
            // intact and turns nothing into an error.
            TEXT_BYTES_ATOM => Some(body.iter().map(|&b| b as char).collect()),
            _ => None,
        };
        if let Some(text) = text {
            slides[index].extend(paragraphs_of(&text));
        }
    }
    Ok(())
}

/// Split a text atom into paragraphs.
///
/// PowerPoint separates paragraphs within one atom with a carriage return, and
/// a soft line break within a paragraph with a vertical tab. Both are control
/// characters that would otherwise be handed to a speech engine as they stand.
fn paragraphs_of(text: &str) -> Vec<String> {
    text.split(['\r', '\n'])
        .map(|paragraph| paragraph.replace(['\u{b}', '\u{c}'], " ").trim().to_string())
        // A run of empty paragraphs is a deck's spacing, not its words.
        .filter(|paragraph| !paragraph.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A presentation read in English.
    ///
    /// Pinned, because the words around the slides — their numbering, and the
    /// way a table on one is read out — come from the language file now, and a
    /// test switching the language beside this one would otherwise fail it.
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
        assert!(handles("pptx"));
        assert!(handles("ppt"));
        assert!(!handles("txt"));
        assert!(!handles("PPTX"), "the caller lowercases first");
    }

    #[test]
    fn a_slide_number_is_taken_from_the_whole_path() {
        assert_eq!(slide_number("ppt/slides/slide12.xml"), Some(12));
        // The relationship file that sits beside every slide is not one.
        assert_eq!(slide_number("ppt/slides/_rels/slide12.xml.rels"), None);
        assert_eq!(slide_number("ppt/slideLayouts/slideLayout1.xml"), None);
        assert_eq!(slide_number("ppt/notesSlides/notesSlide1.xml"), None);
    }

    #[test]
    fn runs_within_a_paragraph_are_joined_and_paragraphs_are_not() {
        let xml = "<a:p><a:r><a:t>Half a </a:t></a:r><a:r><a:t>sentence.</a:t></a:r></a:p>\
                   <a:p><a:r><a:t>Another.</a:t></a:r></a:p>";
        assert_eq!(slide_paragraphs(xml), ["Half a sentence.", "Another."]);
    }

    /// A soft line break is a space, not a newline: mid-paragraph, several
    /// speech back ends hear a newline as a full stop.
    #[test]
    fn a_line_break_within_a_paragraph_stays_within_it() {
        let xml = "<a:p><a:r><a:t>One</a:t></a:r><a:br/><a:r><a:t>two</a:t></a:r></a:p>";
        assert_eq!(slide_paragraphs(xml), ["One two"]);
    }

    #[test]
    fn entities_are_decoded_rather_than_spelled_out() {
        let xml = "<a:p><a:t>Ben &amp; Jerry&apos;s &lt;3 &#8212; &#x2019;99</a:t></a:p>";
        assert_eq!(slide_paragraphs(xml), ["Ben & Jerry's <3 — ’99"]);
        // An ampersand that begins nothing is left where it is.
        assert_eq!(decode_entities("Fish & chips"), "Fish & chips");
        assert_eq!(decode_entities("R&D; still"), "R&D; still");
    }

    /// Nothing outside `<a:t>` may reach the listener, markup least of all.
    #[test]
    fn attributes_and_other_elements_are_not_read_aloud() {
        let xml = "<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Title 1\"/></p:nvSpPr>\
                   <a:p><a:pPr lvl=\"0\"/><a:r><a:rPr lang=\"en-GB\" dirty=\"0\"/>\
                   <a:t>The title</a:t></a:r></a:p></p:sp>";
        assert_eq!(slide_paragraphs(xml), ["The title"]);
    }

    /// The whole point of the change: a table on a slide is read the way a
    /// spreadsheet is, every figure under the name of its column, rather than
    /// as a handful of unrelated words.
    #[test]
    fn a_table_on_a_slide_is_read_as_a_table() {
        let xml = table_xml(&[&["Region", "Sales"], &["North", "1,200"], &["South", "980"]]);
        let read = |xml: &str| crate::i18n::with_language("en", || slide_paragraphs(xml));
        assert_eq!(
            read(&xml),
            [
                "A table of 2 rows and 2 columns: Region, Sales.",
                "Row 1. Region: North. Sales: 1,200.",
                "Row 2. Region: South. Sales: 980.",
            ]
        );
    }

    /// A two-row table — a header and one figure — is the ordinary shape on a
    /// slide, and the one a bare cell-by-cell reading served worst.
    #[test]
    fn a_single_figure_still_arrives_under_its_heading() {
        let xml = table_xml(&[&["Region", "Sales"], &["North", "1,200"]]);
        let read = crate::i18n::with_language("en", || slide_paragraphs(&xml));
        assert_eq!(
            read,
            [
                "A table of 1 row and 2 columns: Region, Sales.",
                "Row 1. Region: North. Sales: 1,200.",
            ]
        );
    }

    /// Text either side of a table on the same slide stays where it was, and
    /// the table does not swallow it.
    #[test]
    fn a_table_does_not_disturb_the_words_around_it() {
        let xml = format!(
            "<a:p><a:r><a:t>Before the table</a:t></a:r></a:p>{}\
             <a:p><a:r><a:t>After it</a:t></a:r></a:p>",
            table_xml(&[&["Region", "Sales"], &["North", "1,200"]])
        );
        let read = crate::i18n::with_language("en", || slide_paragraphs(&xml));
        assert_eq!(read.first().map(String::as_str), Some("Before the table"));
        assert_eq!(read.last().map(String::as_str), Some("After it"));
        assert!(read.iter().any(|p| p.contains("Sales: 1,200")), "{read:?}");
    }

    /// One cell may hold several paragraphs, and a cell may hold nothing. The
    /// first must not become several cells, and the second must not shift every
    /// value after it into the wrong column.
    #[test]
    fn a_cell_may_hold_several_paragraphs_or_none() {
        let xml = "<a:tbl>\
             <a:tr><a:tc><a:txBody><a:p><a:r><a:t>Region</a:t></a:r></a:p></a:txBody></a:tc>\
                   <a:tc><a:txBody><a:p><a:r><a:t>Note</a:t></a:r></a:p></a:txBody></a:tc>\
                   <a:tc><a:txBody><a:p><a:r><a:t>Sales</a:t></a:r></a:p></a:txBody></a:tc></a:tr>\
             <a:tr><a:tc><a:txBody><a:p><a:r><a:t>North</a:t></a:r></a:p></a:txBody></a:tc>\
                   <a:tc><a:txBody></a:txBody></a:tc>\
                   <a:tc><a:txBody><a:p><a:r><a:t>1,200</a:t></a:r></a:p></a:txBody></a:tc></a:tr>\
             <a:tr><a:tc><a:txBody><a:p><a:r><a:t>South</a:t></a:r></a:p></a:txBody></a:tc>\
                   <a:tc><a:txBody><a:p><a:r><a:t>Up on</a:t></a:r></a:p>\
                                   <a:p><a:r><a:t>last year</a:t></a:r></a:p></a:txBody></a:tc>\
                   <a:tc><a:txBody><a:p><a:r><a:t>980</a:t></a:r></a:p></a:txBody></a:tc></a:tr>\
             </a:tbl>";
        let read = crate::i18n::with_language("en", || slide_paragraphs(xml));
        // The empty note is left out, and Sales is still Sales.
        assert_eq!(read[1], "Row 1. Region: North. Sales: 1,200.");
        assert_eq!(read[2], "Row 2. Region: South. Note: Up on last year. Sales: 980.");
    }

    /// A whole deck, end to end, with a table on one of its slides.
    #[test]
    fn a_pptx_with_a_table_reads_it_under_its_headings() {
        let out = read(&pptx_with_table());
        assert!(out.contains("Slide 1.\n\nQuarterly review"), "{out:?}");
        assert!(
            out.contains("Slide 2.\n\nA table of 2 rows and 2 columns: Region, Sales."),
            "{out:?}"
        );
        assert!(out.contains("Row 2. Region: South. Sales: 980."), "{out:?}");
    }

    /// `<a:tbl>` wrapped in the shapes a real slide puts around it.
    fn table_xml(rows: &[&[&str]]) -> String {
        let mut xml = String::from("<a:graphicFrame><a:graphic><a:graphicData><a:tbl>");
        xml.push_str("<a:tblGrid><a:gridCol w=\"3000\"/><a:gridCol w=\"3000\"/></a:tblGrid>");
        for row in rows {
            xml.push_str("<a:tr h=\"370\">");
            for cell in *row {
                xml.push_str(&format!(
                    "<a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:rPr lang=\"en-GB\"/>\
                     <a:t>{cell}</a:t></a:r></a:p></a:txBody></a:tc>"
                ));
            }
            xml.push_str("</a:tr>");
        }
        xml.push_str("</a:tbl></a:graphicData></a:graphic></a:graphicFrame>");
        xml
    }

    /// A two-slide deck: a title, then a table.
    fn pptx_with_table() -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("ppt/slides/slide1.xml", options).unwrap();
        zip.write_all(b"<p:sld><a:p><a:r><a:t>Quarterly review</a:t></a:r></a:p></p:sld>")
            .unwrap();
        zip.start_file("ppt/slides/slide2.xml", options).unwrap();
        let table = table_xml(&[&["Region", "Sales"], &["North", "1,200"], &["South", "980"]]);
        zip.write_all(format!("<p:sld><p:cSld><p:spTree>{table}</p:spTree></p:cSld></p:sld>").as_bytes())
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn an_empty_slide_is_counted_rather_than_skipped() {
        let out = crate::i18n::with_language("en", || lay_out(&[
            vec!["First.".to_string()],
            Vec::new(),
            vec!["Third.".to_string()],
        ]));
        assert_eq!(
            out,
            "Slide 1.\n\nFirst.\n\nSlide 2. No text on this slide.\n\nSlide 3.\n\nThird."
        );
    }

    #[test]
    fn a_text_atom_splits_into_paragraphs_on_its_control_characters() {
        assert_eq!(
            paragraphs_of("Title\rFirst point\rSecond\u{b}point\r\r"),
            ["Title", "First point", "Second point"]
        );
    }

    #[test]
    fn something_that_is_not_a_presentation_says_so() {
        let refusal = refuse(b"Just some text");
        assert!(refusal.contains("not a PowerPoint"), "{refusal}");
    }

    // ------------------------------------------------- the formats themselves

    /// A `.pptx` is a zip of XML, so one can be built here rather than
    /// committed as a binary fixture nobody can review in a diff.
    fn pptx(slides: &[&str]) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        // Written in reverse, so a reader that trusts the archive's order rather
        // than the slide number fails this test.
        for number in (1..=slides.len()).rev() {
            let body: String = slides[number - 1]
                .split('|')
                .map(|paragraph| format!("<a:p><a:r><a:t>{paragraph}</a:t></a:r></a:p>"))
                .collect();
            zip.start_file(format!("ppt/slides/slide{number}.xml"), options)
                .unwrap();
            zip.write_all(format!("<p:sld><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>").as_bytes())
                .unwrap();
        }
        zip.start_file("ppt/slideLayouts/slideLayout1.xml", options)
            .unwrap();
        zip.write_all(b"<a:p><a:r><a:t>Click to edit Master title style</a:t></a:r></a:p>")
            .unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn a_pptx_reads_its_slides_in_order() {
        // Ten slides, so that slide 10 sorting before slide 2 as text would
        // show up here.
        let slides: Vec<String> = (1..=10).map(|n| format!("Slide {n} title|Its body")).collect();
        let names: Vec<&str> = slides.iter().map(String::as_str).collect();
        let out = read(&pptx(&names));

        for n in 1..=10 {
            assert!(
                out.contains(&format!("Slide {n}.\n\nSlide {n} title\n\nIts body")),
                "slide {n} missing or out of order in {out:?}"
            );
        }
        // The layout's placeholder text is not part of the presentation.
        assert!(!out.contains("Master title style"), "{out:?}");
    }

    #[test]
    fn a_zip_with_no_slides_in_it_is_not_a_presentation() {
        let refusal = refuse(&pptx(&[]));
        assert!(refusal.contains("no slides"), "{refusal}");
    }

    /// Builds a compound file holding one stream, which is all this reader
    /// asks of the format. Version 3, 512-byte sectors, exactly as PowerPoint
    /// wrote them.
    ///
    /// A stream shorter than the 4 KB cutoff goes into the mini stream, as the
    /// format requires — so which of the reader's two paths a test exercises
    /// follows from how much text the test puts in the deck, the same way it
    /// does for a real file.
    fn compound_file(stream_name: &str, contents: &[u8]) -> Vec<u8> {
        const SECTOR: usize = 512;
        const MINI: usize = 64;
        const END: u32 = 0xFFFF_FFFE;
        let mini = contents.len() < 4096;

        // Sector 0 is the FAT and sector 1 the directory. What follows is
        // either the stream itself, or the mini FAT and the mini stream that
        // carries it.
        let payload_sectors = contents.len().div_ceil(SECTOR).max(1);
        let mini_sectors = contents.len().div_ceil(MINI).max(1);
        let (first_payload, sectors) = if mini {
            (3, 3 + (mini_sectors * MINI).div_ceil(SECTOR))
        } else {
            (2, 2 + payload_sectors)
        };

        let mut file = vec![0u8; SECTOR * (1 + sectors)];
        file[..8].copy_from_slice(&cfb::MAGIC);
        file[26..28].copy_from_slice(&3u16.to_le_bytes()); // minor version
        file[28..30].copy_from_slice(&[0xFE, 0xFF]); // little-endian
        file[30..32].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors
        file[32..34].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors
        file[44..48].copy_from_slice(&1u32.to_le_bytes()); // one FAT sector
        file[48..52].copy_from_slice(&1u32.to_le_bytes()); // directory at sector 1
        file[56..60].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
        file[60..64].copy_from_slice(&if mini { 2 } else { END }.to_le_bytes());
        file[64..68].copy_from_slice(&u32::from(mini).to_le_bytes()); // mini FAT sectors
        file[68..72].copy_from_slice(&END.to_le_bytes()); // no extra DIFAT
        file[76..80].copy_from_slice(&0u32.to_le_bytes()); // the FAT is sector 0

        let put = |file: &mut Vec<u8>, sector: usize, index: usize, value: u32| {
            let base = SECTOR * (1 + sector) + index * 4;
            file[base..base + 4].copy_from_slice(&value.to_le_bytes());
        };
        // The FAT: itself, the directory, then a chain through what follows.
        for index in 0..SECTOR / 4 {
            put(&mut file, 0, index, 0xFFFF_FFFF); // free
        }
        put(&mut file, 0, 0, 0xFFFF_FFFD); // sector 0 is the FAT
        put(&mut file, 0, 1, END); // the directory ends here
        if mini {
            put(&mut file, 0, 2, END); // the mini FAT is one sector
        }
        for sector in first_payload..sectors {
            let last = sector + 1 == sectors;
            put(&mut file, 0, sector, if last { END } else { sector as u32 + 1 });
        }

        if mini {
            // The mini FAT chains the mini stream's own 64-byte sectors.
            for index in 0..mini_sectors {
                let last = index + 1 == mini_sectors;
                put(&mut file, 2, index, if last { END } else { index as u32 + 1 });
            }
            for index in mini_sectors..SECTOR / 4 {
                put(&mut file, 2, index, 0xFFFF_FFFF);
            }
        }

        let entry = |file: &mut Vec<u8>, index: usize, name: &str, kind: u8, start: u32, size: u64| {
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
        // The root entry holds the mini stream, when there is one.
        let (root_start, root_size) = if mini {
            (first_payload as u32, (mini_sectors * MINI) as u64)
        } else {
            (END, 0)
        };
        entry(&mut file, 0, "Root Entry", 5, root_start, root_size);
        // Inside the mini stream a stream starts at mini sector 0; outside it,
        // at the first sector after the directory.
        let stream_start = if mini { 0 } else { first_payload as u32 };
        entry(&mut file, 1, stream_name, 2, stream_start, contents.len() as u64);

        let at = SECTOR * (1 + first_payload);
        file[at..at + contents.len()].copy_from_slice(contents);
        file
    }

    /// One record: an eight-byte header and its body.
    fn record(version_instance: u16, record_type: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&version_instance.to_le_bytes());
        out.extend_from_slice(&record_type.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn container(record_type: u16, body: &[u8]) -> Vec<u8> {
        record(0x000F, record_type, body)
    }

    #[test]
    fn a_ppt_reads_the_slides_and_leaves_the_masters_alone() {
        const MAIN_MASTER: u16 = 0x03F8;
        let mut stream = Vec::new();
        // A master, whose placeholder text must not be read.
        stream.extend(container(
            MAIN_MASTER,
            &record(0, TEXT_BYTES_ATOM, b"Click to edit Master title style"),
        ));
        // Two slides, one of each text encoding.
        stream.extend(container(
            SLIDE_CONTAINER,
            &record(0, TEXT_BYTES_ATOM, b"First slide\rIts body"),
        ));
        let utf16: Vec<u8> = "Deuxi\u{e8}me\rCaf\u{e9}"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        stream.extend(container(
            SLIDE_CONTAINER,
            &record(0, TEXT_CHARS_ATOM, &utf16),
        ));

        let file = compound_file("PowerPoint Document", &stream);
        let out = read(&file);
        assert_eq!(
            out,
            "Slide 1.\n\nFirst slide\n\nIts body\n\nSlide 2.\n\nDeuxi\u{e8}me\n\nCaf\u{e9}"
        );
    }

    /// A stream at or over the 4 KB cutoff lives in sectors of its own rather
    /// than in the mini stream, and every real presentation is one: this is
    /// the path that matters, and it is not the path the test above takes.
    #[test]
    fn a_deck_too_large_for_the_mini_stream_reads_the_same_way() {
        let filler: Vec<u8> = "Padding. ".repeat(600).into_bytes();
        let mut stream = container(
            SLIDE_CONTAINER,
            &record(0, TEXT_BYTES_ATOM, b"The only slide"),
        );
        // A record type nothing looks at, carrying enough bytes to push the
        // stream past the cutoff.
        stream.extend(record(0, 0x0FBA, &filler));
        assert!(stream.len() > 4096);

        let file = compound_file("PowerPoint Document", &stream);
        assert_eq!(read(&file), "Slide 1.\n\nThe only slide");
    }

    #[test]
    fn a_password_protected_presentation_says_so_rather_than_reading_noise() {
        let stream = container(CRYPT_SESSION_CONTAINER, &[0u8; 16]);
        let file = compound_file("PowerPoint Document", &stream);
        let refusal = refuse(&file);
        assert!(refusal.contains("password-protected"), "{refusal}");
    }

    #[test]
    fn a_compound_file_that_is_not_a_presentation_says_so() {
        let file = compound_file("WordDocument", b"not a deck");
        let refusal = refuse(&file);
        assert!(refusal.contains("PowerPoint Document stream"), "{refusal}");
    }

    /// Half a file, at every boundary that matters, must come back as an error
    /// rather than as a panic: this one arrives from outside.
    /// A zip may declare every slide empty and then hand over megabytes of
    /// each. The budget counts what was read, not what was claimed, or five
    /// thousand slides of that would be held in memory at once.
    #[test]
    fn an_archive_that_lies_about_its_sizes_still_meets_the_budget() {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        // Highly compressible, so the archive stays small while what it expands
        // to does not.
        let padding = " ".repeat(4 * 1024 * 1024);
        for number in 1..=40 {
            zip.start_file(format!("ppt/slides/slide{number}.xml"), options)
                .unwrap();
            zip.write_all(format!("<a:p><a:t>Slide {number}</a:t></a:p><!--{padding}-->").as_bytes())
                .unwrap();
        }
        let archive = zip.finish().unwrap().into_inner();
        assert!(archive.len() < 1024 * 1024, "the archive itself is small");

        let refusal = refuse(&archive);
        assert!(refusal.contains("64 MB"), "{refusal}");
    }

    /// An allocation table pointing in a circle must stop at the size of the
    /// file, not at a constant: two sectors followed a quarter of a million
    /// times is a hundred megabytes out of a kilobyte in.
    #[test]
    fn a_chain_that_loops_stops_at_the_size_of_the_file() {
        const SECTOR: usize = 512;
        let mut file = compound_file("PowerPoint Document", &vec![0u8; 8 * 1024]);
        // Point the stream's first sector back at itself.
        let fat = SECTOR + 3 * 4;
        file[fat..fat + 4].copy_from_slice(&3u32.to_le_bytes());

        let started = std::time::Instant::now();
        let refusal = refuse(&file);
        assert!(refusal.contains("never ends"), "{refusal}");
        // The loop is cut at the file's own length, so this is a few dozen
        // sectors rather than the quarter of a million the constant allows.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_truncated_file_does_not_panic() {
        let file = compound_file("PowerPoint Document", &container(SLIDE_CONTAINER, b""));
        for cut in [0, 100, 512, 700, 1024, 1500] {
            let _ = text_from_bytes(&file[..cut.min(file.len())]);
        }
    }
}

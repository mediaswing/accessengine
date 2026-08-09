//! Text extraction from `.docx`.
//!
//! A `.docx` is a zip archive whose main part, `word/document.xml`, holds the
//! body as WordprocessingML. We only need the readable text, so rather than
//! model the schema we stream the XML and keep the parts that a person would
//! actually hear: the contents of `<w:t>` runs, with paragraph and line breaks
//! turned into newlines and `<w:tab/>` into a tab.
//!
//! # Formatting
//!
//! Bold, italic, underline, strikethrough, colour and highlight are announced
//! aloud when [`Formatting::Announce`] is chosen — "bold, 30 June, end bold" —
//! and are otherwise thrown away with the rest of the styling. Someone who
//! cannot see the page has no other way to know a date was emphasised, and a
//! contract that puts its one important clause in red says nothing at all
//! read flat.
//!
//! Both ends of a run are marked. The alternative — announcing only where
//! formatting starts — is shorter but leaves no way to tell which words it
//! covered, which for the documents this matters in is the whole question.
//!
//! **Only direct formatting is read.** A run is bold here because it carries
//! `<w:b/>`, which is what Word writes when someone presses the bold button;
//! bold inherited from a paragraph or character *style* is not resolved, as
//! that means reading `styles.xml` and walking its inheritance. This is also
//! the more useful line to draw: it reports what the author deliberately
//! emphasised, rather than announcing every heading in the document as bold.

use crate::config::Formatting;
use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::io::{BufReader, Read};
use std::path::Path;

/// The most decompressed XML the body is allowed to be. Comfortably past any
/// real document — a 500-page book is a few megabytes of `word/document.xml` —
/// and far short of what a zip bomb wants to hand over.
const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;

pub fn extract(path: &Path, formatting: Formatting) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file)).with_context(|| {
        format!(
            "{} is not a readable .docx file",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;

    let part = main_document_name(&mut archive)?;
    let mut xml = String::new();
    archive
        .by_name(&part)
        .with_context(|| format!("could not read {part} from the document"))?
        // Bounded by what comes *out* of the decompressor, not by the size of
        // the file on disk. A .docx is a zip, and a few hundred kilobytes of it
        // can expand to gigabytes of XML — enough to take the app down with an
        // allocation failure, which for someone who depends on this to read
        // their post is a worse outcome than a refusal.
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut xml)
        .context("the document body was not valid UTF-8")?;

    if xml.len() as u64 > MAX_BODY_BYTES {
        bail!(
            "the text inside {} expands to more than {} MB, which is more than this app will \
             read — it may be a damaged or deliberately malformed file",
            path.file_name().unwrap_or_default().to_string_lossy(),
            MAX_BODY_BYTES / (1024 * 1024)
        );
    }

    Ok(super::tidy(&parse_body(&xml, formatting)?))
}

/// Finds the main document part. It is `word/document.xml` in every file Word
/// itself writes, but some generators use a different name under `word/`.
fn main_document_name<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Result<String> {
    const CONVENTIONAL: &str = "word/document.xml";
    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    if names.iter().any(|n| n == CONVENTIONAL) {
        return Ok(CONVENTIONAL.to_string());
    }
    names
        .into_iter()
        .find(|n| n.starts_with("word/document") && n.ends_with(".xml"))
        .ok_or_else(|| anyhow!("the file has no Word document body inside it"))
}

/// Elements whose children are formatting instructions rather than readable
/// text. Skipping them wholesale keeps stray tab stops and style names out.
fn is_properties_element(local: &[u8]) -> bool {
    matches!(
        local,
        b"pPr" | b"rPr" | b"sectPr" | b"tblPr" | b"tcPr" | b"trPr" | b"numPr"
    )
}

/// The formatting carried by one run, as far as it is worth saying out loud.
///
/// Compared as a whole rather than a flag at a time: Word splits a sentence
/// into a fresh run at every change of language, spell-check state or revision
/// mark, so identical formatting arrives over and over and must not be
/// re-announced each time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Format {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    /// Already resolved to a word — see [`colour_name`].
    colour: Option<&'static str>,
    highlight: Option<&'static str>,
}

impl Format {
    /// What is in force, in the order it is announced. Fixed so that a run
    /// which is bold *and* red always says them the same way round.
    fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.bold {
            names.push("bold".to_string());
        }
        if self.italic {
            names.push("italic".to_string());
        }
        if self.underline {
            names.push("underlined".to_string());
        }
        if self.strikethrough {
            names.push("struck through".to_string());
        }
        if let Some(colour) = self.colour {
            names.push(format!("{colour} text"));
        }
        if let Some(highlight) = self.highlight {
            names.push(format!("{highlight} highlight"));
        }
        names
    }
}

/// Word stores a text colour as a hex triplet, which is no use spoken aloud.
/// Each is announced as the nearest of these by straight-line distance in RGB.
/// Crude as colour science, but the question being answered is "which word
/// describes this", and for the handful of colours anyone actually applies to
/// text it lands on the right one.
const COLOURS: &[(&str, [u8; 3])] = &[
    ("black", [0x00, 0x00, 0x00]),
    ("white", [0xFF, 0xFF, 0xFF]),
    ("grey", [0x80, 0x80, 0x80]),
    ("red", [0xFF, 0x00, 0x00]),
    ("dark red", [0x8B, 0x00, 0x00]),
    ("orange", [0xFF, 0xA5, 0x00]),
    ("yellow", [0xFF, 0xFF, 0x00]),
    ("green", [0x00, 0x80, 0x00]),
    ("light green", [0x90, 0xEE, 0x90]),
    ("blue", [0x00, 0x00, 0xFF]),
    ("dark blue", [0x00, 0x00, 0x8B]),
    ("light blue", [0xAD, 0xD8, 0xE6]),
    ("purple", [0x80, 0x00, 0x80]),
    ("pink", [0xFF, 0xC0, 0xCB]),
    ("brown", [0xA5, 0x2A, 0x2A]),
    ("teal", [0x00, 0x80, 0x80]),
];

/// The nearest colour word to a `w:color` value, or `None` when there is
/// nothing worth saying.
///
/// Black is deliberately nothing worth saying. It is the colour text already
/// is, and Word writes it explicitly all over a document that has never been
/// recoloured — announcing it would bury every real one. `auto` and
/// `windowtext` mean "whatever the theme says", which is the same case.
fn colour_name(value: &str) -> Option<&'static str> {
    let value = value.trim().trim_start_matches('#');
    if value.len() != 6 || value.eq_ignore_ascii_case("auto") {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&value[at..at + 2], 16).ok();
    let rgb = [channel(0)?, channel(2)?, channel(4)?];

    let nearest = COLOURS.iter().min_by_key(|(_, candidate)| {
        candidate
            .iter()
            .zip(rgb)
            .map(|(&a, b)| {
                let difference = a as i32 - b as i32;
                difference * difference
            })
            .sum::<i32>()
    })?;
    (nearest.0 != "black").then_some(nearest.0)
}

/// Word's highlighter pen, whose values are a fixed list of names rather than
/// hex. Spoken as the colours the Word menu calls them, which is what someone
/// who has had the document described to them will have been told.
fn highlight_name(value: &str) -> Option<&'static str> {
    Some(match value.trim() {
        "yellow" => "yellow",
        "green" => "bright green",
        "cyan" => "turquoise",
        "magenta" => "pink",
        "blue" => "blue",
        "red" => "red",
        "darkBlue" => "dark blue",
        "darkCyan" => "teal",
        "darkGreen" => "green",
        "darkMagenta" => "violet",
        "darkRed" => "dark red",
        "darkYellow" => "dark yellow",
        "darkGray" => "grey",
        "lightGray" => "light grey",
        "black" => "black",
        "white" => "white",
        // "none", and anything a future Word invents.
        _ => return None,
    })
}

/// An attribute by local name, so a `w:val` is found whatever prefix the
/// generator bound the WordprocessingML namespace to.
fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name.as_bytes())
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Whether a toggle property is switching its formatting on.
///
/// Present means on — `<w:b/>` is how Word writes bold. The explicit off value
/// exists because a run inside a bold style turns bold *off* with
/// `<w:b w:val="0"/>`, and reading that as "bold" would announce the emphasis
/// exactly backwards.
fn is_on(element: &BytesStart) -> bool {
    !matches!(
        attribute(element, "val").as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Folds one element of a `<w:rPr>` into the run's formatting.
fn read_property(element: &BytesStart, format: &mut Format) {
    match element.local_name().as_ref() {
        b"b" => format.bold = is_on(element),
        b"i" => format.italic = is_on(element),
        // `w:val="none"` is an underline switched off, and is the only value
        // that means that — the rest name a style of line.
        b"u" => {
            format.underline =
                is_on(element) && attribute(element, "val").as_deref() != Some("none");
        }
        b"strike" | b"dstrike" => format.strikethrough = is_on(element),
        b"color" => format.colour = attribute(element, "val").as_deref().and_then(colour_name),
        b"highlight" => {
            format.highlight = attribute(element, "val")
                .as_deref()
                .and_then(highlight_name);
        }
        _ => {}
    }
}

/// Announces the change from one run's formatting to the next's.
///
/// Only the difference is spoken. Bold text that becomes bold *and* italic
/// opens the italic and leaves the bold alone, rather than closing and
/// reopening something that never stopped.
fn transition(speech: &mut Speech, active: &mut Format, next: &Format) {
    if active == next {
        return;
    }
    let (before, after) = (active.names(), next.names());
    // Closed in reverse, so nesting comes apart in the order it went together.
    for name in before.iter().rev().filter(|name| !after.contains(name)) {
        speech.marker(&format!("end {name}"));
    }
    for name in after.iter().filter(|name| !before.contains(name)) {
        speech.marker(name);
    }
    *active = next.clone();
}

/// The text being built, with the punctuation around announcements kept sane.
///
/// An announcement is a clause of its own and needs commas around it or the
/// synthesiser runs it into the sentence. Getting those commas right is fiddly
/// enough — the document brings its own punctuation, and its own spaces, to
/// exactly the places an announcement lands — that it lives here rather than
/// being repeated at each of the half-dozen call sites.
#[derive(Default)]
struct Speech {
    out: String,
    /// Set immediately after an announcement, so the document's own full stop
    /// can take the place of the comma just written instead of following it:
    /// "end bold, ." is a pause, a second pause, and a full stop attached to
    /// neither.
    after_marker: bool,
}

impl Speech {
    fn marker(&mut self, phrase: &str) {
        while self.out.ends_with([' ', '\t']) {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            if !self.out.ends_with(',') {
                self.out.push(',');
            }
            self.out.push(' ');
        }
        self.out.push_str(phrase);
        self.out.push(',');
        self.after_marker = true;
    }

    fn text(&mut self, text: &str) {
        if std::mem::take(&mut self.after_marker) {
            if text
                .trim_start()
                .starts_with(['.', ',', ';', ':', '!', '?'])
            {
                // The document's punctuation, not one of ours as well.
                while self.out.ends_with(',') {
                    self.out.pop();
                }
                self.out.push_str(text.trim_start());
                return;
            }
            if !text.starts_with([' ', '\t', '\n']) {
                self.out.push(' ');
            }
        }
        self.out.push_str(text);
    }

    /// A break the document asked for: a newline or a tab, never announced.
    fn raw(&mut self, c: char) {
        if std::mem::take(&mut self.after_marker) && self.out.ends_with(',') {
            self.out.pop();
        }
        self.out.push(c);
    }

    fn finish(mut self) -> String {
        if self.after_marker && self.out.ends_with(',') {
            self.out.pop();
        }
        self.out
    }
}

fn parse_body(xml: &str, formatting: Formatting) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let announce = formatting == Formatting::Announce;
    let mut speech = Speech::default();
    let mut in_text_run = false;
    // Depth of nesting inside a properties element; non-zero means "ignore".
    let mut skipping = 0usize;
    // Inside a `<w:rPr>` that is being read rather than skipped.
    let mut reading_properties = false;
    // What the current run carries, and what has been announced so far. They
    // are separate because an announcement is only worth making once text
    // actually arrives — a run holding nothing but a bookmark would otherwise
    // open and close a formatting that was never heard.
    let mut pending = Format::default();
    let mut active = Format::default();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let local = e.local_name();
                let local = local.as_ref();
                if skipping > 0 {
                    if is_properties_element(local) {
                        skipping += 1;
                    }
                    continue;
                }
                if reading_properties {
                    read_property(&e, &mut pending);
                } else if announce && local == b"rPr" {
                    reading_properties = true;
                } else if is_properties_element(local) {
                    skipping = 1;
                } else if local == b"r" {
                    // A run inherits nothing from the one before it.
                    pending = Format::default();
                } else if local == b"t" {
                    in_text_run = true;
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                let local = local.as_ref();
                if skipping > 0 {
                    if is_properties_element(local) {
                        skipping -= 1;
                    }
                    continue;
                }
                if reading_properties {
                    if local == b"rPr" {
                        reading_properties = false;
                    }
                    continue;
                }
                match local {
                    b"t" => in_text_run = false,
                    b"r" => pending = Format::default(),
                    // A paragraph and a table row each end a line of speech.
                    // Formatting is closed off first: a document that carries
                    // bold across a paragraph break is far rarer than one where
                    // hearing "end bold" after the next paragraph has started
                    // would be baffling.
                    b"p" | b"tr" => {
                        transition(&mut speech, &mut active, &Format::default());
                        speech.raw('\n');
                    }
                    // Cells within a row read better separated by a pause.
                    b"tc" => {
                        transition(&mut speech, &mut active, &Format::default());
                        speech.raw('\t');
                    }
                    _ => {}
                }
            }
            Event::Empty(e) if skipping == 0 => {
                if reading_properties {
                    read_property(&e, &mut pending);
                } else {
                    match e.local_name().as_ref() {
                        b"br" | b"cr" => speech.raw('\n'),
                        b"tab" => speech.raw('\t'),
                        _ => {}
                    }
                }
            }
            Event::Text(e) if in_text_run && skipping == 0 && !reading_properties => {
                transition(&mut speech, &mut active, &pending);
                speech.text(&e.xml10_content()?);
            }
            // The reader reports `&amp;`, `&#233;` and friends as their own
            // events rather than inlining them into the surrounding text.
            Event::GeneralRef(e) if in_text_run && skipping == 0 && !reading_properties => {
                let name = e.xml10_content()?;
                transition(&mut speech, &mut active, &pending);
                speech.text(&quick_xml::escape::unescape(&format!("&{name};"))?);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // A document that ends mid-emphasis still has to close it.
    transition(&mut speech, &mut active, &Format::default());
    Ok(speech.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but structurally real .docx on disk.
    fn write_fixture(name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types/>"#).unwrap();
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn extracts_from_a_real_zip_container() {
        let path = write_fixture(
            "soe-docx-test.docx",
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:body>
              <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Quarterly Report</w:t></w:r></w:p>
              <w:p><w:r><w:t xml:space="preserve">Revenue rose 12% </w:t></w:r>
                   <w:r><w:t>year over year &amp; margins held.</w:t></w:r></w:p>
            </w:body></w:document>"#,
        );
        let text = extract(&path, Formatting::Ignore).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(
            text,
            "Quarterly Report\nRevenue rose 12% year over year & margins held."
        );
    }

    /// The whole path off disk on a body shaped the way Word really writes
    /// one, rather than the tidy XML the unit tests above use: namespaced
    /// throughout, `rsid` revision attributes on everything, `<w:rPr>` holding
    /// fonts and sizes alongside the formatting that matters, a sentence split
    /// into three runs for no reason a reader would notice, and — the one that
    /// ruins the output if it is taken at face value — an explicit
    /// `<w:color w:val="000000"/>` on ordinary black text.
    #[test]
    fn a_word_document_reads_with_its_formatting_announced() {
        let path = write_fixture(
            "soe-docx-formatting.docx",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:body>
              <w:p w:rsidR="00A21F3C" w:rsidRDefault="00A21F3C">
                <w:pPr><w:rPr><w:b/><w:sz w:val="24"/></w:rPr></w:pPr>
                <w:r w:rsidRPr="00A21F3C">
                  <w:rPr><w:rFonts w:ascii="Calibri"/><w:color w:val="000000"/><w:sz w:val="22"/></w:rPr>
                  <w:t xml:space="preserve">Your payment of </w:t>
                </w:r>
                <w:r w:rsidRPr="00A21F3C">
                  <w:rPr><w:b/><w:color w:val="C00000"/></w:rPr>
                  <w:t>£82.50</w:t>
                </w:r>
                <w:r>
                  <w:rPr><w:color w:val="000000"/></w:rPr>
                  <w:t xml:space="preserve"> is due on </w:t>
                </w:r>
                <w:r><w:rPr><w:b/></w:rPr><w:t>30</w:t></w:r>
                <w:r><w:rPr><w:b/><w:lang w:val="en-GB"/></w:rPr><w:t xml:space="preserve"> June</w:t></w:r>
                <w:r><w:t>.</w:t></w:r>
              </w:p>
              <w:p>
                <w:r><w:rPr><w:u w:val="single"/></w:rPr><w:t>Late fees apply</w:t></w:r>
                <w:r><w:t xml:space="preserve"> after that date.</w:t></w:r>
              </w:p>
            </w:body></w:document>"#,
        );
        let announced = extract(&path, Formatting::Announce).unwrap();
        let plain = extract(&path, Formatting::Ignore).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(
            announced,
            "Your payment of, bold, dark red text, £82.50, end dark red text, end bold, \
             is due on, bold, 30 June, end bold.\n\
             underlined, Late fees apply, end underlined, after that date."
        );
        // The same file with the setting off is what it has always been.
        assert_eq!(
            plain,
            "Your payment of £82.50 is due on 30 June.\nLate fees apply after that date."
        );
    }

    /// A .docx is a zip, so the body can be enormously larger than the file.
    /// This builds a real one — a few hundred kilobytes on disk that inflates
    /// past the cap — and checks it is refused rather than allocated.
    #[test]
    fn refuses_a_body_that_expands_far_beyond_its_file_size() {
        use std::io::Write as _;

        let path = std::env::temp_dir().join("soe-docx-bomb.docx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("word/document.xml", options).unwrap();
        // Highly compressible: a run of one byte deflates to almost nothing.
        let block = vec![b' '; 1024 * 1024];
        for _ in 0..(super::MAX_BODY_BYTES / block.len() as u64 + 2) {
            zip.write_all(&block).unwrap();
        }
        zip.finish().unwrap();

        let on_disk = std::fs::metadata(&path).unwrap().len();
        let error = extract(&path, Formatting::Ignore).unwrap_err().to_string();
        std::fs::remove_file(&path).ok();

        assert!(
            on_disk < super::MAX_BODY_BYTES / 10,
            "the fixture should be far smaller than what it expands to"
        );
        assert!(error.contains("expands to more than"), "got: {error}");
    }

    #[test]
    fn rejects_a_file_that_is_not_a_zip() {
        let path = std::env::temp_dir().join("soe-not-a-docx.docx");
        std::fs::write(&path, b"this is plain text, not a zip").unwrap();
        let error = extract(&path, Formatting::Ignore).unwrap_err().to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("not a readable .docx"), "got: {error}");
    }

    #[test]
    fn joins_runs_and_breaks_paragraphs() {
        let xml = r#"<w:document><w:body>
            <w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second</w:t><w:br/><w:t>line</w:t></w:r></w:p>
        </w:body></w:document>"#;
        assert_eq!(
            parse_body(xml, Formatting::Ignore).unwrap(),
            "Hello world\nSecond\nline\n"
        );
    }

    #[test]
    fn ignores_formatting_properties() {
        // The tab stop inside pPr must not become a tab in the output, and the
        // style name inside rPr must not be spoken.
        let xml = r#"<w:p>
            <w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr>
            <w:r><w:rPr><w:rStyle w:val="Strong"/></w:rPr><w:t>Only this</w:t></w:r>
        </w:p>"#;
        assert_eq!(parse_body(xml, Formatting::Ignore).unwrap(), "Only this\n");
    }

    #[test]
    fn preserves_significant_whitespace_and_entities() {
        let xml = r#"<w:p><w:r><w:t xml:space="preserve">a &amp; b </w:t></w:r>
            <w:r><w:t>c</w:t></w:r></w:p>"#;
        assert_eq!(parse_body(xml, Formatting::Ignore).unwrap(), "a & b c\n");
    }

    /// Announcing a run wraps it, and the document's own full stop stays a
    /// full stop rather than trailing after a stray comma.
    #[test]
    fn bold_is_announced_at_both_ends() {
        let xml = r#"<w:p>
            <w:r><w:t xml:space="preserve">Payment is due on </w:t></w:r>
            <w:r><w:rPr><w:b/></w:rPr><w:t>30 June</w:t></w:r>
            <w:r><w:t>.</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "Payment is due on, bold, 30 June, end bold.\n"
        );
    }

    /// The same document with the setting off must be exactly what it always
    /// was — this is the promise the dropdown makes.
    #[test]
    fn nothing_is_announced_when_the_setting_is_off() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:b/><w:i/></w:rPr><w:t>Urgent</w:t></w:r>
        </w:p>"#;
        assert_eq!(parse_body(xml, Formatting::Ignore).unwrap(), "Urgent\n");
    }

    /// Word splits a sentence into a new run at every change of language or
    /// spell-check state, so the same formatting arrives again and again. Each
    /// one re-announced would make an emphasised sentence unlistenable.
    #[test]
    fn formatting_carried_across_consecutive_runs_is_announced_once() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">One </w:t></w:r>
            <w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">two </w:t></w:r>
            <w:r><w:rPr><w:b/></w:rPr><w:t>three</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "bold, One two three, end bold\n"
        );
    }

    /// Adding italic to text that is already bold opens the italic and leaves
    /// the bold alone; it never stopped.
    #[test]
    fn only_the_change_in_formatting_is_announced() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Bold </w:t></w:r>
            <w:r><w:rPr><w:b/><w:i/></w:rPr><w:t>and italic</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "bold, Bold, italic, and italic, end italic, end bold\n"
        );
    }

    /// `<w:b w:val="0"/>` is how a run inside a bold style switches bold back
    /// *off*. Read as "present means on", it would announce the emphasis
    /// exactly backwards.
    #[test]
    fn a_toggle_switched_off_is_not_announced() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:b w:val="0"/><w:u w:val="none"/></w:rPr><w:t>Plain</w:t></w:r>
        </w:p>"#;
        assert_eq!(parse_body(xml, Formatting::Announce).unwrap(), "Plain\n");
    }

    /// Colour is stored as hex and has to arrive as a word.
    #[test]
    fn a_colour_is_announced_by_the_nearest_colour_name() {
        assert_eq!(colour_name("FF0000"), Some("red"));
        assert_eq!(colour_name("#C00000"), Some("dark red"));
        assert_eq!(colour_name("0000CD"), Some("blue"));
        // Black is the colour text already is, and Word writes it explicitly
        // all over a document nobody has recoloured.
        assert_eq!(colour_name("000000"), None);
        assert_eq!(colour_name("auto"), None);
        assert_eq!(colour_name("not a colour"), None);
    }

    #[test]
    fn colour_and_highlight_are_announced_with_what_they_are() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:color w:val="FF0000"/></w:rPr><w:t>Overdue</w:t></w:r>
            <w:r><w:rPr><w:highlight w:val="yellow"/></w:rPr><w:t>Check this</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "red text, Overdue, end red text, yellow highlight, Check this, end yellow highlight\n"
        );
    }

    #[test]
    fn strikethrough_is_announced() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:strike/></w:rPr><w:t>Cancelled</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "struck through, Cancelled, end struck through\n"
        );
    }

    /// The `<w:rPr>` inside a `<w:pPr>` describes the paragraph mark, not the
    /// text of the paragraph. Applying it would announce bold on a paragraph
    /// where nothing the reader hears is bold at all.
    #[test]
    fn the_paragraph_marks_own_properties_do_not_format_the_paragraph() {
        let xml = r#"<w:p>
            <w:pPr><w:rPr><w:b/><w:i/></w:rPr></w:pPr>
            <w:r><w:t>Ordinary text</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "Ordinary text\n"
        );
    }

    /// Formatting is closed at the end of a paragraph. Left open, the "end
    /// bold" would arrive after the next paragraph had already started.
    #[test]
    fn formatting_is_closed_off_at_the_end_of_a_paragraph() {
        let xml = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Heading</w:t></w:r></w:p>
            <w:p><w:r><w:t>Body text</w:t></w:r></w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "bold, Heading, end bold\nBody text\n"
        );
    }

    /// A run holding no text at all — a bookmark, a comment anchor — must not
    /// open and close a formatting nobody ever hears.
    #[test]
    fn a_run_with_no_text_announces_nothing() {
        let xml = r#"<w:p>
            <w:r><w:rPr><w:b/></w:rPr></w:r>
            <w:r><w:t>Just words</w:t></w:r>
        </w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Announce).unwrap(),
            "Just words\n"
        );
    }

    #[test]
    fn resolves_named_and_numeric_character_references() {
        let xml = r#"<w:p><w:r><w:t>caf&#233; &amp; cr&#xE8;me &lt;3</w:t></w:r></w:p>"#;
        assert_eq!(
            parse_body(xml, Formatting::Ignore).unwrap(),
            "café & crème <3\n"
        );
    }
}

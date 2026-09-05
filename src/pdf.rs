//! Reading the words out of a PDF.
//!
//! This is the one format in the app that is not read by this app. Every other
//! reader here is a few hundred lines against a published specification,
//! because the specifications are small enough to hold: a `.pptx` is a zip of
//! XML, a `.doc` is a container and a piece table.
//!
//! A PDF is not that. Getting the words out means an object parser, cross
//! reference tables and the streams that replaced them, several compression
//! filters, and then — the part that actually decides whether the output is
//! prose or gibberish — resolving each font's own private mapping from byte to
//! character through its `ToUnicode` map, its `Differences` array, or the
//! standard encoding it declines to name. Files that decrypt with an empty
//! password are common enough that AES belongs on the list too. A partial
//! implementation of that does not produce partial output; it produces
//! confident nonsense, and confident nonsense read aloud is worse for the
//! person listening than a message saying the file could not be opened.
//!
//! So `pdf_extract` does the reading, and what is left here is the part that
//! is this app's own: deciding what is a paragraph, and making sure a file
//! from outside cannot take the process down with it.

use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use crate::document::flush;
use crate::t;

/// Every PDF has this in it, followed by its version.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// How far into the file the header is still allowed to sit.
///
/// A PDF is not obliged to begin with its header, and plenty do not: anything
/// that has been concatenated, or passed through something that prepended a
/// line of its own, carries its header a little way in, and every reader that
/// matters — `pdf_extract` here among them — goes and finds it. A kilobyte is
/// the distance Acrobat looks through, and is short enough that a file which
/// is simply not a PDF is still turned away.
const MAGIC_WITHIN: usize = 1024;

/// What is written between two pages, and what `paragraphs_of` parts them on.
const PAGE_BREAK: char = '\u{c}';

/// Whether this reader handles the given extension.
pub fn handles(extension: &str) -> bool {
    extension == "pdf"
}

/// Whether the header is anywhere it is allowed to be.
fn looks_like_pdf(raw: &[u8]) -> bool {
    let window = &raw[..raw.len().min(MAGIC_WITHIN + PDF_MAGIC.len())];
    window
        .windows(PDF_MAGIC.len())
        .any(|bytes| bytes == PDF_MAGIC)
}

/// The text of a PDF, paragraph by paragraph.
pub fn text_from_file(path: &Path) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    text_from_bytes(&raw).with_context(|| format!("reading {}", path.display()))
}

fn text_from_bytes(raw: &[u8]) -> Result<String> {
    if !looks_like_pdf(raw) {
        bail!(t!("error.not_a_pdf"));
    }
    let paragraphs = paragraphs_of(&extract(raw)?);
    if paragraphs.is_empty() {
        // Overwhelmingly this is a scan: a page of photographs of text, with
        // no text layer behind them. The message says so, because "this PDF is
        // empty" would send someone looking for a fault in a file that is
        // exactly as its author left it.
        bail!(t!("error.pdf_has_no_text"));
    }
    Ok(paragraphs.join("\n\n"))
}

/// Hand the file to `pdf_extract`, and survive whatever it makes of it.
///
/// The crate reaches for `unwrap` in a hundred places against data that came
/// from outside this machine, so a malformed PDF is as likely to panic as to
/// return an error. Unwinding it here turns what would be a dead application —
/// with whatever the user had queued up in it — into one refused file. The
/// panic still reaches the log through the hook `main` installs, which is
/// where a report of a file that will not open should be read from.
fn extract(raw: &[u8]) -> Result<String> {
    // `AssertUnwindSafe` because the borrow is only read from, and nothing
    // this closure touches outlives the call.
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pages_of(raw)));
    match attempt {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(why)) => Err(anyhow::anyhow!(t!("error.pdf_unreadable", why = why))),
        Err(_) => bail!(t!("error.pdf_malformed")),
    }
}

/// The document's text, with [`PAGE_BREAK`] between its pages.
///
/// This is `pdf_extract::extract_text_from_mem` opened up: the same three
/// steps it takes, with [`Paged`] in place of the plain output device so that
/// the page endings survive. The crate's own `extract_text_from_mem_by_pages`
/// would give page endings too, and is not used because of how it ends: it
/// stops at the first page that fails and returns the pages before it as a
/// success, which is a document quietly missing its second half.
fn pages_of(raw: &[u8]) -> std::result::Result<String, pdf_extract::OutputError> {
    let mut doc = pdf_extract::Document::load_mem(raw)?;
    // A file encrypted under the empty password is the "protected" PDF half of
    // officialdom sends out; it opens without anybody being asked anything.
    // One with a real password fails here, and the failure is reported.
    if doc.is_encrypted() {
        doc.decrypt("")?;
    }
    let text = Shared::default();
    let mut output = Paged {
        inner: pdf_extract::PlainTextOutput::new(text.clone()),
        text: text.clone(),
    };
    pdf_extract::output_doc(&doc, &mut output)?;
    let text = text.0.borrow().clone();
    Ok(text)
}

/// A string that both the extractor and the code around it write into.
///
/// `PlainTextOutput` takes its writer by value and never hands it back, so
/// putting anything of our own between two pages means keeping a second handle
/// on what it is writing to.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<String>>);

impl std::fmt::Write for Shared {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.borrow_mut().push_str(text);
        Ok(())
    }
}

impl pdf_extract::ConvertToFmt for Shared {
    type Writer = Shared;
    fn convert(self) -> Self::Writer {
        self
    }
}

/// `PlainTextOutput`, with the page endings put back into it.
///
/// The crate's own `end_page` writes nothing whatever. Where one page stops
/// and the next starts is left to be inferred from where the characters land,
/// and when the last character drawn on a page sits at about the height of the
/// first drawn on the next, nothing at all is written between them — not even
/// a space, so "the agreement." and "The parties" arrive as one word. Which
/// character is drawn last is a matter of the order the content stream draws
/// in rather than of how the page looks, so this is not a rare shape for a
/// document to have. Writing the break here is what keeps a page ending from
/// becoming a word, and it is what puts the paragraph break between two pages
/// that [`paragraphs_of`] then reads.
struct Paged {
    inner: pdf_extract::PlainTextOutput<Shared>,
    text: Shared,
}

impl pdf_extract::OutputDev for Paged {
    fn begin_page(
        &mut self,
        page_num: u32,
        media_box: &pdf_extract::MediaBox,
        art_box: Option<(f64, f64, f64, f64)>,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner.begin_page(page_num, media_box, art_box)
    }

    fn end_page(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner.end_page()?;
        self.text.0.borrow_mut().push(PAGE_BREAK);
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        spacing: f64,
        font_size: f64,
        char: &str,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner
            .output_character(trm, width, spacing, font_size, char)
    }

    fn begin_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner.begin_word()
    }

    fn end_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner.end_word()
    }

    fn end_line(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        self.inner.end_line()
    }
}

/// Cut the extracted text into paragraphs.
///
/// What comes back is laid out the way the page was: one line per line of
/// type, padded with the spaces that put the words where they sat. A line of
/// a page is not a paragraph — a paragraph is the run of them between blank
/// lines — and handing the speech engine one line at a time would put a pause
/// at the end of every line of type rather than at the end of every sentence.
fn paragraphs_of(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();

    // A page break ends a paragraph. It is the one piece of the page's
    // structure worth keeping, and `lines` does not treat it as a break.
    for line in text.replace(PAGE_BREAK, "\n\n").lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(&mut current, &mut paragraphs);
            continue;
        }

        // A word broken across two lines is put back together. Justified text
        // hyphenates heavily, and "informa- tion" spoken aloud is a worse
        // failure than the one this risks: a real hyphen that happens to fall
        // at a line ending, joined into one word. The next line beginning
        // lower case is what keeps that rare.
        let hyphenated = current.ends_with('-')
            && current[..current.len() - 1].ends_with(char::is_alphabetic)
            && line.starts_with(char::is_lowercase);
        if hyphenated {
            current.pop();
        } else if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(line);
    }

    flush(&mut current, &mut paragraphs);
    paragraphs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8]) -> String {
        crate::i18n::with_language("en", || text_from_bytes(raw)).expect("reads")
    }

    fn refuse(raw: &[u8]) -> String {
        crate::i18n::with_language("en", || text_from_bytes(raw))
            .expect_err("this file should not be read")
            .to_string()
    }

    #[test]
    fn the_extensions_it_claims_are_the_ones_it_reads() {
        assert!(handles("pdf"));
        assert!(!handles("txt"));
        assert!(!handles("docx"));
        assert!(!handles("PDF"), "the caller lowercases first");
    }

    /// A PDF built here rather than committed as a binary fixture nobody can
    /// review in a diff. Five objects and a cross reference table, which is
    /// the smallest thing that is honestly a PDF.
    fn pdf(pages: &[&[&str]]) -> Vec<u8> {
        let mut objects: Vec<String> = vec![String::new(), String::new()];
        let mut kids = String::new();
        for lines in pages {
            let mut content = String::from("BT /F1 12 Tf 72 720 Td 14 TL\n");
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    content.push_str("T*\n");
                }
                content.push_str(&format!("({line}) Tj\n"));
            }
            content.push_str("ET\n");

            let page = objects.len() + 1;
            kids.push_str(&format!("{page} 0 R "));
            objects.push(format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents {} 0 R\
                 /Resources<</Font<</F1 {} 0 R>>>>>>",
                page + 1,
                page + 2
            ));
            objects.push(format!(
                "<</Length {}>>stream\n{content}endstream",
                content.len()
            ));
            objects.push("<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string());
        }
        objects[0] = "<</Type/Catalog/Pages 2 0 R>>".to_string();
        objects[1] = format!(
            "<</Type/Pages/Kids[{}]/Count {}>>",
            kids.trim(),
            pages.len()
        );

        let mut out = Vec::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj{body}endobj\n", index + 1).as_bytes());
        }
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for at in &offsets {
            out.extend_from_slice(format!("{at:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn a_pdf_reads_the_words_on_its_pages() {
        let out = read(&pdf(&[&["Quarterly report.", "Revenue rose."]]));
        assert!(out.contains("Quarterly report."), "{out:?}");
        assert!(out.contains("Revenue rose."), "{out:?}");
    }

    /// Both pages, and — the part `contains` cannot see — the break between
    /// them. Both pages of this file put their line at the same height, which
    /// is the case where the extractor writes nothing at all between them and
    /// "First page.Second page." arrives as one word.
    #[test]
    fn every_page_is_read_and_the_page_ending_parts_them() {
        let out = read(&pdf(&[&["First page."], &["Second page."]]));
        assert_eq!(out, "First page.\n\nSecond page.");
    }

    #[test]
    fn something_that_is_not_a_pdf_says_so() {
        let refusal = refuse(b"Just a text file, really");
        assert!(refusal.contains("not a PDF"), "{refusal}");
    }

    /// A header a little way in is where a good many real files keep theirs,
    /// and the reader below finds it there, so this reader does not refuse the
    /// file before it gets the chance.
    #[test]
    fn a_pdf_that_does_not_begin_at_its_first_byte_is_still_read() {
        let mut file = Vec::from("a line something else put in front\n");
        file.extend_from_slice(&pdf(&[&["Readable all the same."]]));
        assert!(read(&file).contains("Readable all the same."));
    }

    /// The search stops after a kilobyte. Otherwise the header is not a header
    /// but a mention of one, and a file that merely talks about PDFs would be
    /// handed to the extractor.
    #[test]
    fn a_header_further_in_than_the_search_goes_is_not_a_pdf() {
        let mut file = vec![b' '; MAGIC_WITHIN + 1];
        file.extend_from_slice(PDF_MAGIC);
        let refusal = refuse(&file);
        assert!(refusal.contains("not a PDF"), "{refusal}");
    }

    /// The common case worth its own message: a scan has pages but no text
    /// behind them.
    #[test]
    fn a_pdf_with_no_text_layer_says_it_is_probably_a_scan() {
        let refusal = refuse(&pdf(&[&[]]));
        assert!(refusal.contains("most likely a scan"), "{refusal}");
    }

    #[test]
    fn a_damaged_pdf_is_refused_rather_than_taking_the_app_down() {
        let mut damaged = pdf(&[&["Something."]]);
        damaged.truncate(damaged.len() / 2);
        // Whether this returns an error or unwinds a panic from inside the
        // extractor is not the point; that the process is still here is.
        let _ = crate::i18n::with_language("en", || text_from_bytes(&damaged));
    }

    // The paragraph building is this app's own, and is tested on its own
    // rather than through a PDF: what arrives from the extractor is lines.

    #[test]
    fn the_lines_of_a_paragraph_are_joined_and_a_blank_line_parts_them() {
        let text = "The first line of it   \nand the second.   \n\nA new paragraph.\n";
        assert_eq!(
            paragraphs_of(text),
            ["The first line of it and the second.", "A new paragraph."]
        );
    }

    #[test]
    fn a_word_broken_across_two_lines_is_put_back_together() {
        assert_eq!(
            paragraphs_of("more informa-\ntion here"),
            ["more information here"]
        );
    }

    /// The join only happens where a broken word is plausible. A dash between
    /// two words, or before a capital, is left where the author put it.
    #[test]
    fn a_hyphen_that_is_not_a_broken_word_is_left_alone() {
        assert_eq!(
            paragraphs_of("the North-\nSouth line"),
            ["the North- South line"]
        );
        assert_eq!(paragraphs_of("a dash -\nthen more"), ["a dash - then more"]);
    }

    #[test]
    fn a_page_break_ends_a_paragraph() {
        assert_eq!(
            paragraphs_of("End of one.\u{c}Start of two."),
            ["End of one.", "Start of two."]
        );
    }
}

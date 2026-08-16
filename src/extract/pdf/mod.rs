//! Text extraction from `.pdf`.
//!
//! A PDF is not a document in the sense the other readers here deal with. A
//! `.docx` says "this is a paragraph, and these are its words"; a PDF says
//! "put this glyph at this point on the page". Everything a reader needs —
//! where the words are, where a line ends, which order to read the columns in
//! — was thrown away when the file was made, and getting it back is
//! reconstruction rather than parsing. The pieces of that job are split up:
//!
//! - [`object`] reads the syntax: numbers, names, strings, dictionaries,
//!   streams.
//! - [`filters`] undoes the compression each stream is stored under.
//! - [`doc`] finds the objects in the file and builds the tree of pages, and
//!   explains why it scans rather than trusting the cross reference table.
//! - [`font`] turns the bytes a page shows into the characters they stand for.
//! - [`content`] runs the page description and works out where the words and
//!   lines are.
//!
//! # What it will not read
//!
//! **Scanned pages.** A PDF from a scanner or a phone is a photograph of a
//! page with no text in it at all, and no amount of parsing produces words
//! from one. Rather than report such a file as empty, this says what it is —
//! and points at the image reader, which is the part of this app that *can*
//! read a picture of a page.
//!
//! **Encrypted files.** Including the very common kind with no password on
//! them, only a restriction on printing or copying: the text is still
//! scrambled, and unscrambling it needs cryptography this app does not carry.

pub mod content;
pub mod doc;
pub mod encodings;
pub mod filters;
pub mod font;
pub mod object;

use anyhow::{Context, Result, bail};
use std::path::Path;

/// The largest file this will open.
///
/// A PDF holds its text compressed, so this is a great deal more document than
/// the plain-text ceiling of the same size suggests — several tens of
/// thousands of pages. The point is the same one [`crate::extract::txt`] makes:
/// the whole file is read into memory, so a mistyped path to a disk image
/// should be a message rather than an allocation failure.
const MAX_PDF_BYTES: u64 = 128 * 1024 * 1024;

/// How much text will be taken out of one file, in characters. Past any
/// document a person listens to, and a guard against a malformed file that
/// describes an endless page.
const MAX_TEXT_CHARS: usize = 16 * 1024 * 1024;

/// Below this many characters a page is treated as having no text on it at
/// all. A scanned page is not always completely empty: the scanner's own
/// software often stamps a page number or its name on it, and a document whose
/// every page holds four characters is still a document nobody can read.
const MIN_CHARS_PER_PAGE: usize = 8;

pub fn extract(path: &Path) -> Result<String> {
    let name = || path.file_name().unwrap_or_default().to_string_lossy();

    let size = std::fs::metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?
        .len();
    if size > MAX_PDF_BYTES {
        bail!(
            "{} is {:.0} MB, which is a larger PDF than this app will read at once",
            name(),
            size as f64 / (1024.0 * 1024.0)
        );
    }

    let data = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    // The header is allowed to sit a little way in — a file that has been
    // through a download or a mail server often has a few stray bytes in front
    // of it, and every reader tolerates that.
    if object::find(&data[..data.len().min(1024)], b"%PDF", 0).is_none() {
        bail!("{} does not look like a PDF file inside", name());
    }

    let document = doc::Document::parse(&data)
        .with_context(|| format!("{} could not be read as a PDF", name()))?;
    if document.is_encrypted() {
        bail!(
            "{} is encrypted, so its text cannot be read. Files locked against printing or \
             copying are encrypted too, even when they open without a password. Saving a fresh \
             copy from a PDF viewer usually removes it.",
            name()
        );
    }

    let pages = document.pages();
    if pages.is_empty() {
        bail!("{} has no pages in it", name());
    }

    let mut extractor = content::Extractor::new(&document);
    let mut text = String::new();
    let mut with_text = 0usize;
    for page in &pages {
        let page_text = extractor.page_text(page);
        if page_text.trim().chars().count() >= MIN_CHARS_PER_PAGE {
            with_text += 1;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&page_text);
        if text.len() > MAX_TEXT_CHARS {
            crate::log::line(format!(
                "pdf: stopped after {MAX_TEXT_CHARS} characters, which is all this app will read"
            ));
            break;
        }
    }

    crate::log::line(format!(
        "pdf: {} pages, {} with text, {} characters{}",
        pages.len(),
        with_text,
        text.chars().count(),
        match extractor.dropped {
            0 => String::new(),
            dropped => format!(", {dropped} glyphs no font could account for"),
        }
    ));

    if with_text == 0 {
        bail!("{}", nothing_to_read(&name(), pages.len(), extractor.dropped));
    }

    let text = super::tidy(&text);
    if extractor.dropped > text.chars().count() {
        // More was lost than kept: the file's fonts do not say what their
        // glyphs mean, so what came back is not the document.
        crate::log::line("pdf: most of the text could not be decoded");
    }
    Ok(text)
}

/// What to say about a PDF that gave up no text.
///
/// There are two quite different reasons for it, and telling someone the wrong
/// one sends them off to fix the wrong thing. A file with no text at all is a
/// scan. A file that *showed* text which no font could account for is a
/// document whose glyphs are numbered rather than named — which is what a
/// Chinese, Japanese or Korean document typeset before `/ToUnicode` became
/// usual looks like, and reading one needs character tables that ship with a
/// PDF viewer rather than with the file.
fn nothing_to_read(name: &str, pages: usize, dropped: usize) -> String {
    /// Enough shown-but-undecodable characters to be text rather than a stray
    /// glyph in the corner of a picture.
    const ENOUGH_TO_BE_TEXT: usize = 32;

    if dropped >= ENOUGH_TO_BE_TEXT {
        return format!(
            "{name} has text in it, but its fonts do not say what their letters are, so there is \
             nothing to read out. This happens with documents typeset in Chinese, Japanese or \
             Korean, where the letters are stored as numbers into tables the file does not carry. \
             Opening it in a PDF viewer and copying the text into a plain text file will work."
        );
    }
    format!(
        "{name} has no text in it — its {} a picture of the page rather than the words on it, \
         which is what a scan or a photographed document is. Reading one needs the image reader: \
         save the page as a JPEG or PNG and open that instead.",
        if pages == 1 {
            "one page is".to_string()
        } else {
            format!("{pages} pages are")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Assembles a PDF around a page whose content stream is `content`,
    /// working out the cross reference offsets the way a writer does — none of
    /// which this reader looks at, which is exactly what the last test here
    /// checks.
    pub(super) fn build(objects: &[&str], content: &str) -> Vec<u8> {
        let mut out = Vec::from(&b"%PDF-1.4\n"[..]);
        let push = |out: &mut Vec<u8>, number: usize, body: &str| {
            out.extend_from_slice(format!("{number} 0 obj {body} endobj\n").as_bytes());
        };
        push(&mut out, 1, "<< /Type /Catalog /Pages 2 0 R >>");
        push(&mut out, 2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        push(
            &mut out,
            3,
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>",
        );
        out.extend_from_slice(
            format!(
                "4 0 obj << /Length {} >> stream\n{content}\nendstream endobj\n",
                content.len()
            )
            .as_bytes(),
        );
        push(
            &mut out,
            5,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        );
        for (index, body) in objects.iter().enumerate() {
            push(&mut out, 6 + index, body);
        }
        out.extend_from_slice(b"trailer << /Root 1 0 R /Size 9 >>\n%%EOF\n");
        out
    }

    fn write(name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(data).unwrap();
        path
    }

    fn extract_bytes(name: &str, data: &[u8]) -> Result<String> {
        let path = write(name, data);
        let result = extract(&path);
        std::fs::remove_file(&path).ok();
        result
    }

    #[test]
    fn reads_a_page_of_text_off_disk() {
        let pdf = build(
            &[],
            "BT /F1 12 Tf 72 720 Td (Quarterly Report) Tj 0 -18 Td (Revenue rose 12%.) Tj ET",
        );
        let text = extract_bytes("soe-pdf-simple.pdf", &pdf).unwrap();
        assert_eq!(text, "Quarterly Report\nRevenue rose 12%.");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_pdf() {
        let error = extract_bytes("soe-pdf-notapdf.pdf", b"just some text").unwrap_err();
        assert!(error.to_string().contains("does not look like a PDF"));
    }

    /// The message for a scan is the one a user of this app is most likely to
    /// meet, so it has to say what the file is and what to do about it.
    #[test]
    fn a_page_with_no_text_says_it_is_a_picture() {
        let pdf = build(&[], "q 200 0 0 100 72 600 cm /Im0 Do Q");
        let error = extract_bytes("soe-pdf-scan.pdf", &pdf).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("picture of the page"), "{message}");
        assert!(message.contains("image reader"), "{message}");
    }

    #[test]
    fn an_encrypted_file_says_so_rather_than_reading_as_gibberish() {
        let mut pdf = build(&[], "BT /F1 12 Tf (unreadable) Tj ET");
        let trailer = pdf.len() - b"trailer << /Root 1 0 R /Size 9 >>\n%%EOF\n".len();
        pdf.truncate(trailer);
        pdf.extend_from_slice(b"trailer << /Root 1 0 R /Encrypt 7 0 R >>\n%%EOF\n");
        let error = extract_bytes("soe-pdf-encrypted.pdf", &pdf).unwrap_err();
        assert!(error.to_string().contains("encrypted"));
    }

    /// The whole reason this reader scans for objects instead of seeking to
    /// them: a file whose cross reference table is wrong still has to open,
    /// because that is the ordinary state of a PDF that has been edited.
    #[test]
    fn a_file_with_a_useless_cross_reference_table_still_reads() {
        let mut pdf = build(&[], "BT /F1 12 Tf 72 720 Td (Still readable) Tj ET");
        pdf.extend_from_slice(
            b"xref\n0 6\n0000000000 65535 f \n0000009999 00000 n \n\
              trailer << /Root 1 0 R >>\nstartxref\n999999\n%%EOF\n",
        );
        assert_eq!(
            extract_bytes("soe-pdf-badxref.pdf", &pdf).unwrap(),
            "Still readable"
        );
    }
}


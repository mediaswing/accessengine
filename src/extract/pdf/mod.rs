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
//!
//! # What it reads only in part
//!
//! A font is free to number its glyphs rather than name them, and to leave out
//! the `/ToUnicode` table that says which character each number is. Nothing can
//! be recovered from such a page — the file simply does not record what it
//! says. Whole documents like this are caught above and refused. The harder
//! case is the mixed one, a document in English with a section in Chinese,
//! Japanese or Korean: it opens, nearly all of it is right, and the affected
//! pages come back with a third or more of their characters quietly missing.
//! Those pages are counted as they are read and reported through
//! [`Extracted::caveat`], because they are indistinguishable from good text
//! until they are read aloud. Where such a page gave back nothing at all, a
//! marker stands in its place, so a section that is missing sounds missing
//! rather than sounding like the end of the paragraph before it.

pub mod content;
pub mod doc;
pub mod encodings;
pub mod filters;
pub mod font;
pub mod object;

use super::Extracted;
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

/// The share of a page's glyphs that has to be undecodable before the page is
/// called garbled.
///
/// This is judged per page, not over the file, and the difference matters. A
/// long English document with a Japanese appendix loses a rounding error's
/// worth of its total characters, so a whole-document ratio stays quiet while
/// the appendix comes out as nonsense. Measured per page the two are not close:
/// a page whose fonts are readable drops nothing worth counting, and a page
/// whose fonts are not drops a third of itself or more.
const GARBLED_PERCENT: usize = 25;

/// Enough undecodable glyphs on one page to be text rather than a stray symbol
/// in a logo or a bullet from a font with no encoding.
const ENOUGH_TO_BE_TEXT: usize = 32;

pub fn extract(path: &Path) -> Result<Extracted> {
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
    let mut read: Vec<ReadPage> = Vec::with_capacity(pages.len());
    let mut with_text = 0usize;
    let mut budget = 0usize;
    for page in &pages {
        let before = extractor.dropped;
        let text = extractor.page_text(page);
        let kept = text.trim().chars().count();
        if kept >= MIN_CHARS_PER_PAGE {
            with_text += 1;
        }
        let garbled = is_garbled(kept, extractor.dropped - before);
        budget += text.len();
        read.push(ReadPage { text, garbled });
        if budget > MAX_TEXT_CHARS {
            crate::log::line(format!(
                "pdf: stopped after {MAX_TEXT_CHARS} characters, which is all this app will read"
            ));
            break;
        }
    }

    let garbled: Vec<usize> = read
        .iter()
        .enumerate()
        .filter(|(_, page)| page.garbled)
        .map(|(index, _)| index + 1)
        .collect();
    let text = super::tidy(&assemble(&read));

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

    // Some pages read and some did not, which is the case that needs saying out
    // loud: the document opens, most of it is right, and the bad pages look
    // exactly like the good ones until you listen to them.
    let caveat = (!garbled.is_empty()).then(|| {
        crate::log::line(garbled_explanation(&garbled, pages.len()));
        match garbled.len() {
            1 => "1 page did not decode".to_string(),
            count => format!("{count} pages did not decode"),
        }
    });
    Ok(Extracted { text, caveat })
}

/// One page's worth of the document, kept until every page has been read
/// because a marker has to name the whole run of pages it stands for, and the
/// end of a run is not known until the run is over.
struct ReadPage {
    text: String,
    garbled: bool,
}

/// Joins the pages up, announcing the ones whose fonts could not be decoded.
///
/// A page that lost everything contributes nothing, and a gap in a spoken
/// document is indistinguishable from the document simply not saying anything —
/// so the gap is given a voice. The announcement covers a whole run of
/// consecutive pages rather than appearing on each: the fifteen-page stretch
/// that prompted this would otherwise interrupt fifteen times to say the same
/// thing, which is worse than the silence it replaces.
fn assemble(pages: &[ReadPage]) -> String {
    let mut out = String::new();
    let mut push = |part: &str| {
        if part.trim().is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(part);
    };
    for (index, page) in pages.iter().enumerate() {
        let starts_run = page.garbled && (index == 0 || !pages[index - 1].garbled);
        if starts_run {
            let mut last = index;
            while last + 1 < pages.len() && pages[last + 1].garbled {
                last += 1;
            }
            push(&marker(index + 1, last + 1));
        }
        push(&page.text);
    }
    out
}

/// What stands in for a run of pages that could not be decoded.
///
/// This one is spoken, unlike the log's version, so it says "27 to 41" rather
/// than "27–41" — a dash is anybody's guess as to how a voice reads it — and it
/// explains itself in a single sentence, since the listener has no log in front
/// of them.
fn marker(first: usize, last: usize) -> String {
    let pages = if first == last {
        format!("Page {first}")
    } else {
        format!("Pages {first} to {last}")
    };
    format!("[{pages} could not be read: the fonts used there do not say what their letters are.]")
}

/// Whether a page gave back so little of what it showed that what did come back
/// cannot be trusted. Pages that show almost nothing — a chapter opener, a
/// figure with a caption — are left alone, since a handful of undecodable
/// glyphs there is a symbol rather than prose.
fn is_garbled(kept: usize, dropped: usize) -> bool {
    let shown = kept + dropped;
    dropped >= ENOUGH_TO_BE_TEXT && dropped * 100 >= shown * GARBLED_PERCENT
}

/// The long form, for the log: which pages, why, and what to do about it.
fn garbled_explanation(garbled: &[usize], pages: usize) -> String {
    format!(
        "pdf: {} of {pages} pages are set in fonts that do not say what their letters are, so the \
         text from {} is missing characters and will not read aloud correctly — {}. This is usual \
         in Chinese, Japanese and Korean documents. Opening the file in a PDF viewer and copying \
         those pages into a plain text file will give them in full.",
        garbled.len(),
        if garbled.len() == 1 { "it" } else { "them" },
        ranges(garbled),
    )
}

/// Page numbers as runs — "pages 27–41, 59–64" rather than twenty-one numbers
/// in a row, which is unreadable on a status line and worse read aloud.
fn ranges(numbers: &[usize]) -> String {
    let mut runs: Vec<String> = Vec::new();
    let mut index = 0;
    while index < numbers.len() {
        let start = index;
        while index + 1 < numbers.len() && numbers[index + 1] == numbers[index] + 1 {
            index += 1;
        }
        runs.push(match numbers[index] - numbers[start] {
            0 => numbers[start].to_string(),
            // Two in a row read better as "8 and 9" than as a range of two.
            1 => format!("{} and {}", numbers[start], numbers[index]),
            _ => format!("{}–{}", numbers[start], numbers[index]),
        });
        index += 1;
    }
    format!(
        "page{} {}",
        if numbers.len() == 1 { "" } else { "s" },
        runs.join(", ")
    )
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

    fn extract_all(name: &str, data: &[u8]) -> Result<Extracted> {
        let path = write(name, data);
        let result = extract(&path);
        std::fs::remove_file(&path).ok();
        result
    }

    fn extract_bytes(name: &str, data: &[u8]) -> Result<String> {
        extract_all(name, data).map(|extracted| extracted.text)
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

    #[test]
    fn page_runs_are_written_as_ranges() {
        assert_eq!(ranges(&[7]), "page 7");
        assert_eq!(ranges(&[8, 9]), "pages 8 and 9");
        assert_eq!(ranges(&[27, 28, 29, 30]), "pages 27–30");
        assert_eq!(
            ranges(&[3, 27, 28, 29, 59, 60, 74]),
            "pages 3, 27–29, 59 and 60, 74"
        );
    }

    /// The threshold has to separate the two cases actually seen in the wild: a
    /// page set in a font that decodes drops a glyph or two out of thousands,
    /// and one set in a font that does not drops a third of itself or more.
    #[test]
    fn a_page_is_garbled_only_when_a_real_share_of_it_is_lost() {
        assert!(!is_garbled(4381, 3), "a clean page is not garbled");
        assert!(!is_garbled(3661, 39), "1% lost is not garbled");
        assert!(is_garbled(1225, 518), "30% lost is garbled");
        assert!(is_garbled(0, 1593), "a page that lost everything is garbled");
        // A handful of undecodable glyphs on a nearly empty page is a symbol in
        // a logo, not a sentence, and saying otherwise would cry wolf.
        assert!(!is_garbled(0, 4), "a stray glyph is not garbled");
    }

    /// Builds a page that uses two fonts: one that decodes and one that numbers
    /// its glyphs without saying what they are, which is the mixed-language
    /// document this whole path exists for.
    fn half_readable_page() -> Vec<u8> {
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
             /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> >>",
        );
        // Forty undecodable two-byte codes against a short readable line, which
        // is the proportion a page of Chinese under an English heading has.
        let codes = "00240025".repeat(20);
        let content = format!(
            "BT /F1 12 Tf 72 720 Td (Appendix A) Tj ET \
             BT /F2 12 Tf 72 700 Td <{codes}> Tj ET"
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
        push(
            &mut out,
            6,
            "<< /Type /Font /Subtype /Type0 /BaseFont /AAAAAA+PingFangSC-Regular \
             /Encoding /Identity-H /DescendantFonts [7 0 R] >>",
        );
        push(
            &mut out,
            7,
            "<< /Type /Font /Subtype /CIDFontType0 /BaseFont /AAAAAA+PingFangSC-Regular \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 1000 >>",
        );
        out.extend_from_slice(b"trailer << /Root 1 0 R /Size 9 >>\n%%EOF\n");
        out
    }

    /// The case the whole-document ratio used to miss. The readable half is
    /// still returned — it is a perfectly good heading — but the file no longer
    /// passes off a page with most of its characters missing as complete.
    #[test]
    fn a_page_whose_font_cannot_be_decoded_is_reported_but_still_read() {
        let extracted = extract_all("soe-pdf-halfreadable.pdf", &half_readable_page()).unwrap();
        assert_eq!(
            extracted.text,
            "[Page 1 could not be read: the fonts used there do not say what their letters are.]\n\n\
             Appendix A"
        );
        assert_eq!(extracted.caveat.as_deref(), Some("1 page did not decode"));
    }

    fn page(text: &str, garbled: bool) -> ReadPage {
        ReadPage {
            text: text.to_string(),
            garbled,
        }
    }

    /// The point of collapsing runs: the stretch that prompted all this is
    /// fifteen pages long, and a marker on each would interrupt fifteen times
    /// over to say the same sentence.
    #[test]
    fn a_run_of_undecodable_pages_is_announced_once_for_the_whole_run() {
        let pages = [
            page("Chapter One", false),
            page("", true),
            page("", true),
            page("", true),
            page("Chapter Two", false),
        ];
        assert_eq!(
            assemble(&pages),
            "Chapter One\n\n[Pages 2 to 4 could not be read: the fonts used there do not say what \
             their letters are.]\n\nChapter Two"
        );
    }

    /// Two separate runs are two separate announcements, each naming its own
    /// pages — otherwise the numbers would be wrong, which is worse than none.
    #[test]
    fn separate_runs_are_announced_separately() {
        let pages = [
            page("", true),
            page("Readable", false),
            page("", true),
            page("", true),
        ];
        let assembled = assemble(&pages);
        assert!(assembled.starts_with("[Page 1 could not be read"), "{assembled}");
        assert!(assembled.contains("[Pages 3 to 4 could not be read"), "{assembled}");
    }

    /// A garbled page that still has some text keeps it, with the warning ahead
    /// of it rather than instead of it — the listener is told before they hear
    /// the part that cannot be trusted.
    #[test]
    fn a_partly_readable_page_keeps_its_text_under_the_marker() {
        let pages = [page("Appendix A", true), page("Clean page", false)];
        assert_eq!(
            assemble(&pages),
            "[Page 1 could not be read: the fonts used there do not say what their letters are.]\
             \n\nAppendix A\n\nClean page"
        );
    }

    /// A document with nothing wrong with it must come out exactly as it did
    /// before any of this existed.
    #[test]
    fn a_clean_document_gains_no_markers() {
        let pages = [page("One", false), page("", false), page("Two", false)];
        assert_eq!(assemble(&pages), "One\n\nTwo");
    }

    /// A document that decodes cleanly must stay silent, or the warning means
    /// nothing on the documents that need it.
    #[test]
    fn a_readable_document_carries_no_caveat() {
        let pdf = build(&[], "BT /F1 12 Tf 72 720 Td (Quarterly Report) Tj ET");
        let extracted = extract_all("soe-pdf-nocaveat.pdf", &pdf).unwrap();
        assert_eq!(extracted.caveat, None);
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


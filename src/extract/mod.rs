//! Turning an input file into the plain text that will be spoken.

pub mod csv;
pub mod docx;
pub mod image;
pub mod pcap;
pub mod pdf;
pub mod pptx;
pub mod txt;
pub mod video;

use anyhow::{Result, bail};
use std::path::Path;

/// The kinds of file the app knows how to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Plain text, read straight off disk.
    Text,
    /// Word document; text is pulled out of the OOXML body.
    Docx,
    /// A PDF, whose text has to be reassembled from the instructions that draw
    /// each page.
    Pdf,
    /// A spreadsheet export, read as a table rather than as lines of values.
    Csv,
    /// A PowerPoint deck; text is pulled out of one OOXML part per slide.
    Pptx,
    /// A network capture, whose packets are counted up and then written out
    /// as prose by a text model — see [`pcap`].
    Pcap,
    /// A picture, which has to go through Ollama before there is any text.
    Image,
    /// A video, which is taken apart into stills by ffmpeg and then read the
    /// same way a picture is — many times over.
    Video,
}

impl FileKind {
    /// Classifies by extension. Returns `None` for anything unsupported.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "txt" | "text" | "md" | "markdown" | "log" => Self::Text,
            "docx" => Self::Docx,
            "pdf" => Self::Pdf,
            "csv" | "tsv" => Self::Csv,
            "pptx" => Self::Pptx,
            "pcap" | "pcapng" | "cap" => Self::Pcap,
            "jpg" | "jpeg" | "png" | "heic" | "heif" => Self::Image,
            "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" => Self::Video,
            _ => return None,
        })
    }

    pub fn label(self) -> String {
        match self {
            Self::Text => crate::t!("filekind.text"),
            Self::Docx => crate::t!("filekind.docx"),
            Self::Pdf => crate::t!("filekind.pdf"),
            Self::Csv => crate::t!("filekind.csv"),
            Self::Pptx => crate::t!("filekind.pptx"),
            Self::Pcap => crate::t!("filekind.pcap"),
            Self::Image => crate::t!("filekind.image"),
            Self::Video => crate::t!("filekind.video"),
        }
    }
}

/// Extensions offered in the open dialog, grouped the way the dialog shows them.
pub const TEXT_EXTENSIONS: &[&str] = &["txt", "text", "md", "markdown", "log"];
pub const DOC_EXTENSIONS: &[&str] = &["docx"];
pub const PDF_EXTENSIONS: &[&str] = &["pdf"];
pub const TABLE_EXTENSIONS: &[&str] = &["csv", "tsv"];
pub const PRESENTATION_EXTENSIONS: &[&str] = &["pptx"];
pub const CAPTURE_EXTENSIONS: &[&str] = &["pcap", "pcapng", "cap"];
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "heic", "heif"];
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "avi", "mkv", "webm"];

/// Text taken out of a file, and anything the reader needs to say about how it
/// came out.
///
/// Most readers either produce the document or fail. A PDF has a third outcome:
/// text that arrived, but incomplete, because the file's fonts do not say what
/// their glyphs mean — see [`pdf`]. That is not an error, since the rest of the
/// document is perfectly good, but it is not something to keep quiet about
/// either when the result is going to be read aloud.
pub struct Extracted {
    pub text: String,
    /// A short headline for the status line, if there is anything to warn
    /// about. The long version goes to the log, where there is room for it.
    pub caveat: Option<String>,
}

impl Extracted {
    /// Text with nothing to report, which is what every reader but [`pdf`] returns.
    fn plain(text: String) -> Self {
        Self { text, caveat: None }
    }
}

/// Reads a text or Word file. Images are not handled here because they need the
/// Ollama plumbing in [`image`], which the caller drives separately so it can
/// prompt about installing Ollama first.
/// `formatting` applies to Word documents, the only kind that carries any.
pub fn extract_document(path: &Path, formatting: crate::config::Formatting) -> Result<Extracted> {
    Ok(match FileKind::from_path(path) {
        Some(FileKind::Text) => Extracted::plain(txt::extract(path)?),
        Some(FileKind::Docx) => Extracted::plain(docx::extract(path, formatting)?),
        Some(FileKind::Pdf) => pdf::extract(path)?,
        Some(FileKind::Csv) => Extracted::plain(csv::extract(path)?),
        Some(FileKind::Pptx) => Extracted::plain(pptx::extract(path)?),
        Some(FileKind::Pcap) => bail!("captures are summarised through Ollama, not this path"),
        Some(FileKind::Image) => bail!("images are read through Ollama, not this path"),
        Some(FileKind::Video) => bail!("videos are read through ffmpeg and Ollama, not this path"),
        None => match unsupported_advice(path) {
            Some(advice) => bail!("{}", advice),
            None => bail!(
                "{} is not a file type this app can read",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        },
    })
}

/// The "written by AI" line a piece of extracted text needs, if any.
///
/// Carried alongside the text rather than baked into it, and turned into a
/// sentence only at the moment the text is about to be spoken — because half
/// of what it discloses is *which engine is about to speak it*, and that is
/// the user's to change right up until they press Apply. Appended at
/// extraction time instead, it went stale the moment somebody switched from a
/// system voice to ElevenLabs: the description would be uploaded to a cloud
/// service while the sentence riding along with it said the reading was
/// happening on this computer, which is the one claim it exists to get right.
///
/// Which line to use is the reader's decision, not the caller's — a PNG
/// screenshot needs none where a JPEG photo does, and a capture needs one only
/// when a model actually wrote the summary rather than the app counting it —
/// so the reader names the kind here and the wording is settled later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disclosure {
    /// A photograph described by a vision model. See [`image::is_photo`].
    Photo,
    Video,
    /// A network capture whose counted figures a text model wrote up.
    Capture,
}

impl Disclosure {
    /// `text` with the disclosure on the end, worded for the engine that is
    /// about to read it aloud.
    pub fn note(self, text: &str, engine: crate::config::EnginePreference) -> String {
        match self {
            Self::Photo => image::ai_disclosure_note(text, engine),
            Self::Video => video::ai_disclosure_note(text, engine),
            Self::Capture => pcap::ai_disclosure_note(text, engine),
        }
    }
}

/// File types this app cannot read but can at least explain itself about.
///
/// Refusing a `.ppt` with "not a file type this app can read" is true and
/// useless: the app plainly does read PowerPoint, and the difference between
/// the two extensions is not something anyone should have to know. Naming the
/// fix instead — re-save it — turns a dead end into one step.
///
/// Kept apart from [`FileKind`] on purpose. Everything in that enum is
/// something the app will actually read, and a variant that exists only to be
/// refused would have to be excluded by hand at every match on it.
pub fn unsupported_advice(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    match ext.as_str() {
        // The 1997 binary format, which shares nothing with .pptx but a
        // syllable. PowerPoint still opens one and still offers to save it as
        // the modern format.
        "ppt" => Some(crate::t!("advice.legacy_ppt", name = name)),
        _ => None,
    }
}

/// Collapses the runs of blank space that documents are full of, so the voice
/// doesn't sit in silence and the character count sent to ElevenLabs is honest.
pub fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(
            FileKind::from_path(&PathBuf::from("a/b/Notes.TXT")),
            Some(FileKind::Text)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("report.docx")),
            Some(FileKind::Docx)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("Statement.PDF")),
            Some(FileKind::Pdf)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("photo.HEIC")),
            Some(FileKind::Image)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("holiday.MOV")),
            Some(FileKind::Video)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("Deck.PPTX")),
            Some(FileKind::Pptx)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("office.PCAPNG")),
            Some(FileKind::Pcap)
        );
        assert_eq!(FileKind::from_path(&PathBuf::from("archive.zip")), None);
        assert_eq!(FileKind::from_path(&PathBuf::from("noextension")), None);
    }

    /// Every extension the open dialog offers has to classify as the kind that
    /// dialog filed it under, or the app offers a file it then refuses.
    #[test]
    fn every_offered_extension_classifies_as_its_own_kind() {
        for (extensions, kind) in [
            (TEXT_EXTENSIONS, FileKind::Text),
            (DOC_EXTENSIONS, FileKind::Docx),
            (PDF_EXTENSIONS, FileKind::Pdf),
            (TABLE_EXTENSIONS, FileKind::Csv),
            (PRESENTATION_EXTENSIONS, FileKind::Pptx),
            (CAPTURE_EXTENSIONS, FileKind::Pcap),
            (IMAGE_EXTENSIONS, FileKind::Image),
            (VIDEO_EXTENSIONS, FileKind::Video),
        ] {
            for extension in extensions {
                assert_eq!(
                    FileKind::from_path(&PathBuf::from(format!("file.{extension}"))),
                    Some(kind),
                    "{extension} is offered but does not classify as {kind:?}"
                );
            }
        }
    }

    /// The older PowerPoint format is not readable here, but "not a file type
    /// this app can read" would be a baffling thing to hear about a
    /// presentation from an app that reads presentations.
    #[test]
    fn a_legacy_ppt_is_refused_with_advice_rather_than_a_shrug() {
        let advice = unsupported_advice(&PathBuf::from("Q3 Review.ppt"))
            .expect("a .ppt should have something said about it");
        assert!(advice.contains("Q3 Review.ppt"), "{advice}");
        // The way out has to be in the message; that is the whole point of it.
        assert!(advice.contains(".pptx"), "{advice}");
        // The extension is matched however it is capitalised, the same way
        // every readable kind is.
        assert!(
            unsupported_advice(&PathBuf::from("Deck.PPT"))
                .is_some_and(|advice| advice.contains("Deck.PPT"))
        );
        // Everything the app does read, and everything it has nothing to say
        // about, goes through the ordinary path.
        assert_eq!(unsupported_advice(&PathBuf::from("deck.pptx")), None);
        assert_eq!(unsupported_advice(&PathBuf::from("archive.zip")), None);
        assert_eq!(unsupported_advice(&PathBuf::from("noextension")), None);
    }

    #[test]
    fn tidy_collapses_blank_runs_and_trims() {
        assert_eq!(tidy("\n\nfirst\n\n\n\nsecond   \n\n"), "first\n\nsecond");
    }
}

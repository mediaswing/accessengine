//! Turning an input file into the plain text that will be spoken.

pub mod csv;
pub mod docx;
pub mod image;
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
    /// A spreadsheet export, read as a table rather than as lines of values.
    Csv,
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
            "csv" | "tsv" => Self::Csv,
            "jpg" | "jpeg" | "png" | "heic" | "heif" => Self::Image,
            "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" => Self::Video,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text file",
            Self::Docx => "Word document",
            Self::Csv => "table",
            Self::Image => "image",
            Self::Video => "video",
        }
    }
}

/// Extensions offered in the open dialog, grouped the way the dialog shows them.
pub const TEXT_EXTENSIONS: &[&str] = &["txt", "text", "md", "markdown", "log"];
pub const DOC_EXTENSIONS: &[&str] = &["docx"];
pub const TABLE_EXTENSIONS: &[&str] = &["csv", "tsv"];
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "heic", "heif"];
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "avi", "mkv", "webm"];

/// Reads a text or Word file. Images are not handled here because they need the
/// Ollama plumbing in [`image`], which the caller drives separately so it can
/// prompt about installing Ollama first.
/// `formatting` applies to Word documents, the only kind that carries any.
pub fn extract_document(path: &Path, formatting: crate::config::Formatting) -> Result<String> {
    match FileKind::from_path(path) {
        Some(FileKind::Text) => txt::extract(path),
        Some(FileKind::Docx) => docx::extract(path, formatting),
        Some(FileKind::Csv) => csv::extract(path),
        Some(FileKind::Image) => bail!("images are read through Ollama, not this path"),
        Some(FileKind::Video) => bail!("videos are read through ffmpeg and Ollama, not this path"),
        None => bail!(
            "{} is not a file type this app can read",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
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
            FileKind::from_path(&PathBuf::from("photo.HEIC")),
            Some(FileKind::Image)
        );
        assert_eq!(
            FileKind::from_path(&PathBuf::from("holiday.MOV")),
            Some(FileKind::Video)
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
            (TABLE_EXTENSIONS, FileKind::Csv),
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

    #[test]
    fn tidy_collapses_blank_runs_and_trims() {
        assert_eq!(tidy("\n\nfirst\n\n\n\nsecond   \n\n"), "first\n\nsecond");
    }
}

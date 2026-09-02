//! Loading a file and cutting it into speakable chunks.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How finely to cut the document up. Smaller chunks give tighter progress
/// tracking and faster response to skip/stop; larger chunks sound more natural
/// and, on ElevenLabs, cost fewer requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkMode {
    Sentence,
    Paragraph,
}

impl ChunkMode {
    pub const ALL: [ChunkMode; 2] = [ChunkMode::Sentence, ChunkMode::Paragraph];
    pub fn label(&self) -> &'static str {
        match self {
            ChunkMode::Sentence => "Sentence",
            ChunkMode::Paragraph => "Paragraph",
        }
    }
}

/// Hard cap on chunk size. Keeps individual ElevenLabs requests small and
/// stops one runaway paragraph from blocking stop/skip for a minute.
const MAX_CHUNK_CHARS: usize = 600;

/// A span of the document, as it appears on screen.
#[derive(Clone, Debug)]
pub struct Chunk {
    /// Text exactly as it appears in the source, for display.
    pub display: String,
    /// Byte range within [`Document::text`].
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Document {
    pub path: Option<PathBuf>,
    pub title: String,
    pub text: String,
    pub chunks: Vec<Chunk>,
}

pub const SUPPORTED_EXTENSIONS: &[&str] =
    &["txt", "text", "md", "markdown", "csv", "log", "json", "rst", "org"];

/// Markup this reader used to strip and no longer does.
///
/// Turned away by name rather than left to the plain-text path: a page opened
/// from the command line, or through the dialog's "All files", would otherwise
/// be read out tag by tag — `less than div class equals` — which is a worse
/// answer than saying the file is not one this app opens.
const UNSUPPORTED_MARKUP: &[&str] = &["html", "htm", "xhtml", "xml"];

impl Document {
    pub fn from_path(path: &Path, mode: ChunkMode) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // Refuse absurd files rather than freezing the UI thread on read.
        const MAX_BYTES: u64 = 64 * 1024 * 1024;
        if meta.len() > MAX_BYTES {
            bail!(
                "{} is {:.1} MB; the reader caps files at 64 MB",
                path.display(),
                meta.len() as f64 / 1_048_576.0
            );
        }

        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let text = normalise_line_endings(&decode_text(&raw)?);

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if UNSUPPORTED_MARKUP.contains(&ext.as_str()) {
            bail!(
                "{} is {ext}, which this reader no longer opens. Save it as plain text or \
                 markdown first.",
                path.display()
            );
        }
        let text = match ext.as_str() {
            "md" | "markdown" | "rst" => strip_markdown(&text),
            _ => text,
        };

        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "document".to_string());

        let mut doc = Document {
            path: Some(path.to_path_buf()),
            title,
            text,
            chunks: Vec::new(),
        };
        doc.rechunk(mode);
        Ok(doc)
    }

    /// Build a document from text already in memory (e.g. an image description
    /// or something pasted into the editor).
    pub fn from_text(title: impl Into<String>, text: String, mode: ChunkMode) -> Self {
        let mut doc = Document {
            path: None,
            title: title.into(),
            text,
            chunks: Vec::new(),
        };
        doc.rechunk(mode);
        doc
    }

    pub fn rechunk(&mut self, mode: ChunkMode) {
        self.chunks = split_chunks(&self.text, mode);
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn word_count(&self) -> usize {
        self.text.split_whitespace().count()
    }
}

/// Decode as UTF-8, tolerating a BOM and falling back to latin-1 for the
/// plain-text files that are still, in practice, not UTF-8.
fn decode_text(raw: &[u8]) -> Result<String> {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    if raw.starts_with(&[0xFF, 0xFE]) || raw.starts_with(&[0xFE, 0xFF]) {
        bail!("this looks like a UTF-16 file; please re-save it as UTF-8");
    }
    match std::str::from_utf8(raw) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Ok(raw.iter().map(|&b| b as char).collect()),
    }
}

/// Collapse `\r\n` and lone `\r` to `\n`.
///
/// Windows text files are typically CRLF. The paragraph splitter below only
/// looks for `\n` and slices right up to it, so a CRLF file leaves every
/// line's trailing `\r` sitting in the text — inside a wrapped paragraph,
/// that is a stray control character embedded mid-sentence, both shown in
/// the document view and sent to the speech engines.
fn normalise_line_endings(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            chars.next_if_eq(&'\n');
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Words that end in a full stop without ending a sentence.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "rev", "hon", "st", "ave", "rd", "blvd", "jr", "sr", "vs",
    "etc", "eg", "ie", "cf", "al", "fig", "no", "vol", "pp", "ed", "est", "approx", "inc", "ltd",
    "co", "corp", "dept", "univ", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept",
    "oct", "nov", "dec", "mon", "tue", "tues", "wed", "thu", "thur", "thurs", "fri", "sat", "sun",
    "am", "pm", "min", "max", "sec", "hr", "kg", "km", "cm", "mm", "ft", "in", "lb", "oz",
];

fn is_abbreviation(word: &str) -> bool {
    let w = word.trim_end_matches('.').to_ascii_lowercase();
    if w.is_empty() {
        return false;
    }
    // A single letter before a stop is almost always an initial ("J. R. R.").
    if w.chars().count() == 1 && w.chars().all(|c| c.is_alphabetic()) {
        return true;
    }
    ABBREVIATIONS.contains(&w.as_str())
}

fn split_chunks(text: &str, mode: ChunkMode) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for (para_start, para) in paragraphs(text) {
        match mode {
            ChunkMode::Paragraph => {
                push_chunk(&mut chunks, text, para_start, para_start + para.len());
            }
            ChunkMode::Sentence => {
                for (s, e) in sentence_bounds(para) {
                    push_chunk(&mut chunks, text, para_start + s, para_start + e);
                }
            }
        }
    }
    chunks
}

/// Split on blank lines, yielding (byte offset, slice) pairs.
fn paragraphs(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // Look ahead for a second newline with only whitespace between.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                if start < i {
                    out.push((start, &text[start..i]));
                }
                // Skip the whole run of blank lines.
                let mut k = j;
                while k < bytes.len()
                    && (bytes[k] == b'\n' || bytes[k] == b' ' || bytes[k] == b'\t' || bytes[k] == b'\r')
                {
                    k += 1;
                }
                start = k;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    if start < text.len() {
        out.push((start, &text[start..]));
    }
    out
}

/// Byte ranges of sentences within a single paragraph.
fn sentence_bounds(para: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = para.char_indices().collect();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < chars.len() {
        let (idx, ch) = chars[i];
        if matches!(ch, '.' | '!' | '?') {
            // Absorb trailing quotes/brackets that belong to this sentence.
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '"' | '\'' | ')' | ']' | '}' | '\u{201d}' | '\u{2019}')
            {
                j += 1;
            }
            // Must be followed by whitespace (or end of paragraph).
            let followed_by_space = j >= chars.len() || chars[j].1.is_whitespace();
            if followed_by_space {
                let end = if j < chars.len() { chars[j].0 } else { para.len() };
                let ends_sentence = if ch == '.' {
                    // "Dr. Smith" and "e.g. this" are not sentence ends.
                    let word = last_word(&para[start..idx]);
                    !is_abbreviation(&word) && next_starts_sentence(para, end)
                } else {
                    true
                };
                if ends_sentence {
                    out.push((start, end));
                    // Skip the whitespace before the next sentence.
                    let mut k = j;
                    while k < chars.len() && chars[k].1.is_whitespace() {
                        k += 1;
                    }
                    start = if k < chars.len() { chars[k].0 } else { para.len() };
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }
    if start < para.len() {
        out.push((start, para.len()));
    }
    out
}

fn last_word(s: &str) -> String {
    s.chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// After a full stop, a new sentence normally starts with a capital, a digit or
/// an opening quote. Lowercase means we probably hit an abbreviation.
fn next_starts_sentence(para: &str, from: usize) -> bool {
    match para[from..].chars().find(|c| !c.is_whitespace()) {
        None => true,
        Some(c) => c.is_uppercase() || c.is_numeric() || matches!(c, '"' | '\'' | '(' | '\u{201c}'),
    }
}

/// Whether a slice is short enough to be one chunk.
///
/// Stops at the cap rather than counting the whole slice: the loop below asks
/// this once per chunk about the *rest of the document*, so counting to the end
/// each time made splitting a long unbroken paragraph quadratic — a 64 MB file,
/// which the reader otherwise accepts, froze the window for minutes.
fn within_cap(slice: &str) -> bool {
    slice.chars().nth(MAX_CHUNK_CHARS).is_none()
}

/// Record a chunk, breaking it up further if it is unreasonably long.
fn push_chunk(chunks: &mut Vec<Chunk>, text: &str, start: usize, end: usize) {
    let slice = &text[start..end];
    if slice.trim().is_empty() {
        return;
    }
    if within_cap(slice) {
        chunks.push(Chunk {
            display: slice.to_string(),
            start,
            end,
        });
        return;
    }

    // Too long: break at the last sensible separator before the cap.
    let mut cursor = start;
    while cursor < end {
        let remaining = &text[cursor..end];
        if within_cap(remaining) {
            chunks.push(Chunk {
                display: remaining.to_string(),
                start: cursor,
                end,
            });
            break;
        }
        let limit = remaining
            .char_indices()
            .nth(MAX_CHUNK_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let head = &remaining[..limit];
        let split = head
            .rfind([';', ',', ':'])
            .map(|i| i + 1)
            .or_else(|| head.rfind(' '))
            .unwrap_or(limit);
        let split = split.max(1);
        let piece = &text[cursor..cursor + split];
        if !piece.trim().is_empty() {
            chunks.push(Chunk {
                display: piece.to_string(),
                start: cursor,
                end: cursor + split,
            });
        }
        cursor += split;
    }
}

/// Turn markdown into something worth listening to: drop the syntax, keep the
/// words, and announce images by their alt text.
fn strip_markdown(md: &str) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;

    // Markdown treats a single newline as a space, so lines belonging to one
    // paragraph are reflowed into one. That matters for more than looks: a
    // newline mid-sentence makes several back ends pause as if at a full stop.
    let flush = |current: &mut String, paragraphs: &mut Vec<String>| {
        let text = current.trim().to_string();
        if !text.is_empty() {
            paragraphs.push(text);
        }
        current.clear();
    };

    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            flush(&mut current, &mut paragraphs);
            continue;
        }
        if in_fence {
            continue; // Reading code aloud helps nobody.
        }
        if trimmed.is_empty() {
            flush(&mut current, &mut paragraphs);
            continue;
        }
        // Horizontal rules.
        if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') {
            flush(&mut current, &mut paragraphs);
            continue;
        }

        let is_heading = trimmed.starts_with('#');
        let mut line = trimmed.trim_start_matches('#').trim_start().to_string();
        line = line.trim_start_matches('>').trim_start().to_string();
        let is_item = matches!(
            line.get(..2),
            Some("- ") | Some("* ") | Some("+ ")
        ) || line
            .split_once(". ")
            .is_some_and(|(head, _)| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()));
        if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            line = rest.to_string();
        }

        // Headings and list items stand alone; body lines run together.
        if is_heading || is_item {
            flush(&mut current, &mut paragraphs);
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&strip_inline_markdown(&line));
        if is_heading || is_item {
            flush(&mut current, &mut paragraphs);
        }
    }
    flush(&mut current, &mut paragraphs);

    paragraphs.join("\n\n")
}

fn strip_inline_markdown(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;

    while i < chars.len() {
        // ![alt](src) -> "Image: alt"; [text](href) -> "text"
        if chars[i] == '!' && i + 1 < chars.len() && chars[i + 1] == '[' {
            if let Some((label, next)) = parse_link(&chars, i + 1) {
                if !label.trim().is_empty() {
                    out.push_str("Image: ");
                    out.push_str(&label);
                }
                i = next;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some((label, next)) = parse_link(&chars, i) {
                out.push_str(&label);
                i = next;
                continue;
            }
        }
        // Emphasis and inline code markers carry no sound.
        if matches!(chars[i], '*' | '_' | '`' | '~') {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Parse `[label](target)` starting at `open`. Returns the label and the index
/// just past the closing paren.
fn parse_link(chars: &[char], open: usize) -> Option<(String, usize)> {
    if chars.get(open) != Some(&'[') {
        return None;
    }
    let close = (open + 1..chars.len()).find(|&i| chars[i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let paren = (close + 2..chars.len()).find(|&i| chars[i] == ')')?;
    let label: String = chars[open + 1..close].iter().collect();
    Some((label, paren + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentences(text: &str) -> Vec<String> {
        split_chunks(text, ChunkMode::Sentence)
            .into_iter()
            .map(|c| c.display.trim().to_string())
            .collect()
    }

    #[test]
    fn splits_plain_sentences() {
        assert_eq!(
            sentences("One two. Three four! Five?"),
            vec!["One two.", "Three four!", "Five?"]
        );
    }

    /// A Windows-authored CRLF file must not leave a stray `\r` embedded in a
    /// wrapped sentence, nor a doubled blank line where a `\r\n\r\n` gap is.
    #[test]
    fn crlf_line_endings_are_normalised() {
        let normalised = normalise_line_endings(
            "One line that\r\nwraps onto another before the stop.\r\n\r\nSecond paragraph.\r\n",
        );
        assert!(!normalised.contains('\r'), "{normalised:?}");
        assert_eq!(
            sentences(&normalised),
            vec![
                "One line that\nwraps onto another before the stop.",
                "Second paragraph."
            ]
        );
    }

    #[test]
    fn keeps_abbreviations_together() {
        assert_eq!(
            sentences("Dr. Smith went to St. Ives. He left."),
            vec!["Dr. Smith went to St. Ives.", "He left."]
        );
    }

    #[test]
    fn keeps_initials_together() {
        assert_eq!(sentences("J. R. R. Tolkien wrote it."), vec!["J. R. R. Tolkien wrote it."]);
    }

    #[test]
    fn chunk_offsets_index_the_source() {
        let text = "One two. Three four.";
        for chunk in split_chunks(text, ChunkMode::Sentence) {
            assert_eq!(&text[chunk.start..chunk.end], chunk.display);
        }
    }

    #[test]
    fn paragraphs_split_on_blank_lines() {
        let out = split_chunks("Alpha line.\n\nBeta line.", ChunkMode::Paragraph);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].display.trim(), "Beta line.");
    }

    #[test]
    fn long_text_is_capped() {
        let long = "word ".repeat(400); // 2000 chars, no sentence breaks
        for chunk in split_chunks(&long, ChunkMode::Sentence) {
            assert!(chunk.display.chars().count() <= MAX_CHUNK_CHARS);
        }
    }

    #[test]
    fn markdown_syntax_is_not_read_aloud() {
        let md = "# Title\n\nSome **bold** and a [link](http://x).\n\n```\ncode();\n```\n";
        let out = strip_markdown(md);
        assert!(out.contains("Title"));
        assert!(out.contains("Some bold and a link."));
        assert!(!out.contains("code();"));
        assert!(!out.contains("http://x"));
    }

    /// A paragraph split across source lines must come back as one sentence,
    /// not several, or the voice pauses in the middle of it.
    #[test]
    fn markdown_paragraphs_are_reflowed() {
        let md = "# Title\n\nThe work was,\nfrankly, a mess.\n\nNext para.\n";
        let out = strip_markdown(md);
        assert!(out.contains("The work was, frankly, a mess."), "{out:?}");
        assert_eq!(sentences(&out).len(), 3, "{out:?}");
    }

    #[test]
    fn markdown_list_items_stay_separate() {
        let out = strip_markdown("- first item\n- second item\n");
        assert!(out.contains("first item\n\nsecond item"), "{out:?}");
    }

    #[test]
    fn markdown_images_are_announced() {
        assert!(strip_markdown("![a red bus](bus.png)").contains("Image: a red bus"));
    }

    /// Splitting a long unbroken paragraph must stay linear. The old code
    /// counted the whole remaining text once per chunk, so an 8 MB paragraph
    /// took nearly two seconds on the UI thread.
    #[test]
    fn long_unbroken_text_splits_quickly() {
        let text = "word ".repeat(2 * 1024 * 1024 / 5); // ~2 MB, no sentence ends
        let started = std::time::Instant::now();
        let chunks = split_chunks(&text, ChunkMode::Sentence);
        assert!(chunks.len() > 1000);
        // Generous enough for a debug build on a slow machine, tight enough to
        // fail loudly if the quadratic scan ever comes back.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    /// HTML is not read as prose any more, and must not be read as markup
    /// either: a page opened from the command line or through the dialog's
    /// "All files" is turned away by name.
    #[test]
    fn markup_files_are_turned_away_rather_than_read_as_tags() {
        let dir = std::env::temp_dir().join(format!("accessengine-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let page = dir.join("page.html");
        std::fs::write(&page, "<p>Hello</p>").expect("writes");
        let refusal = Document::from_path(&page, ChunkMode::Sentence)
            .expect_err("html is no longer opened")
            .to_string();
        assert!(refusal.contains("plain text"), "{refusal}");

        // The plain-text formats it still opens are unaffected.
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "Hello.").expect("writes");
        assert!(Document::from_path(&notes, ChunkMode::Sentence).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf16_is_reported_rather_than_mangled() {
        assert!(decode_text(&[0xFF, 0xFE, 0x41, 0x00]).is_err());
    }
}

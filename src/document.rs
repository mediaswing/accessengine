//! Loading a file and cutting it into speakable chunks.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{t, tn};

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
    pub fn label(&self) -> String {
        match self {
            ChunkMode::Sentence => t!("chunk.sentence"),
            ChunkMode::Paragraph => t!("chunk.paragraph"),
        }
    }
}

/// The largest file the reader will open, and the largest amount of prose it
/// will make out of one. Refusing absurd files is what keeps the UI thread from
/// freezing on a read, and the second use is the same promise kept on the way
/// out: a format that expands as it is read — a table, whose every cell gains
/// the name of its column — must not turn a file the reader accepted into
/// several gigabytes of text.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

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

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "txt", "text", "md", "markdown", "csv", "log", "json", "rst", "org", "ppt", "pptx", "pptm",
    "pps", "ppsx", "doc", "docx", "docm", "dot", "dotx", "dotm", "pdf",
];

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
        if meta.len() > MAX_BYTES {
            bail!(t!(
                "error.file_too_large",
                path = path.display(),
                size = format!("{:.1}", meta.len() as f64 / 1_048_576.0)
            ));
        }

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if UNSUPPORTED_MARKUP.contains(&ext.as_str()) {
            bail!(t!(
                "error.markup_file",
                path = path.display(),
                kind = ext
            ));
        }

        // A presentation, a word processor document and a PDF are containers of
        // one sort or another rather than text, so each is opened by the module
        // that understands the container rather than decoded as characters
        // first.
        let text = if crate::powerpoint::handles(&ext) {
            crate::powerpoint::text_from_file(path)?
        } else if crate::word::handles(&ext) {
            crate::word::text_from_file(path)?
        } else if crate::pdf::handles(&ext) {
            crate::pdf::text_from_file(path)?
        } else {
            let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let text = normalise_line_endings(&decode_text(&raw)?);
            match ext.as_str() {
                "md" | "markdown" | "rst" => strip_markdown(&text),
                "csv" => table_to_prose(&text),
                _ => text,
            }
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
        bail!(t!("error.utf16"));
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

// ------------------------------------------------------------------- tables

/// Delimiters worth trying, in the order they win ties.
const DELIMITERS: [char; 4] = [',', ';', '\t', '|'];

/// How much of the file [`sniff_delimiter`] judges by, and how many records it
/// looks at within that.
const SNIFF_BYTES: usize = 64 * 1024;
const SNIFF_RECORDS: usize = 20;

/// Which delimiter this file uses.
///
/// Sniffed rather than assumed. A `.csv` exported by a spreadsheet in a country
/// where the comma is the decimal separator is semicolon-separated, and one
/// dumped out of a database is as often tab-separated; both are called `.csv`
/// by the program that wrote them.
///
/// The judgement is consistency rather than raw count: a table with one prose
/// column would otherwise be declared comma-separated on the strength of the
/// commas inside that column's sentences. A real delimiter gives every record
/// the same number of fields, and a stray one does not.
fn sniff_delimiter(text: &str) -> char {
    let limit = text.len().min(SNIFF_BYTES);
    let head = &text[..(0..=limit)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)];

    let mut best = (0usize, 0usize, ',');
    for delimiter in DELIMITERS {
        let records = parse_records(head, delimiter);
        let widths: Vec<usize> = records.iter().take(SNIFF_RECORDS).map(Vec::len).collect();
        // The width most records agree on, and how many of them agree.
        let Some(&width) = widths
            .iter()
            .max_by_key(|w| widths.iter().filter(|other| other == w).count())
        else {
            continue;
        };
        if width < 2 {
            continue;
        }
        let agreeing = widths.iter().filter(|w| **w == width).count();
        if (agreeing, width) > (best.0, best.1) {
            best = (agreeing, width, delimiter);
        }
    }
    best.2
}

/// Split a delimited file into records and fields, following RFC 4180: a field
/// wrapped in double quotes may hold the delimiter, a line break, or a doubled
/// `""` standing for one quote.
///
/// Never fails. A file that breaks the rules — an unclosed quote, a stray one
/// mid-field — is still cut into something, because the alternative is
/// refusing to read a spreadsheet somebody has already been handed.
fn parse_records(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            // Only a quote that opens the field quotes it; one appearing
            // partway through is a character somebody typed.
            '"' if field.is_empty() => quoted = true,
            _ if c == delimiter => record.push(std::mem::take(&mut field)),
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }

    for record in &mut records {
        for field in record.iter_mut() {
            let trimmed = field.trim();
            if trimmed.len() != field.len() {
                *field = trimmed.to_string();
            }
        }
    }
    // A trailing newline leaves one empty record behind it, and a file may end
    // in several; none of them is a row of the table.
    records.retain(|record| record.iter().any(|field| !field.is_empty()));
    records
}

/// Read a delimited file out the way a person reads a table aloud.
/// End whatever came before without ending the paragraph — a line break, or
/// the join between two paragraphs sharing one table cell.
///
/// Here rather than with any one reader because every format that arrives as
/// something other than plain text builds its paragraphs this way: see
/// [`crate::powerpoint`], [`crate::word`] and [`crate::pdf`].
pub fn separate(current: &mut String) {
    if !current.is_empty() && !current.ends_with(' ') {
        current.push(' ');
    }
}

/// End the paragraph, keeping it only if it had words in it.
pub fn flush(current: &mut String, paragraphs: &mut Vec<String>) {
    let text = current.trim().to_string();
    if !text.is_empty() {
        paragraphs.push(text);
    }
    current.clear();
}

fn table_to_prose(text: &str) -> String {
    let delimiter = sniff_delimiter(text);
    records_to_prose(parse_records(text, delimiter))
}

/// Read a table out the way a person reads one aloud: which row, then each
/// column's name with the cell under it.
///
/// The alternative is what the reader used to do — speak the file as lines of
/// text — which turns `Alice,30,Leeds` into "Alice thirty Leeds" and leaves the
/// listener to remember, from a header line half a screen back, which number
/// was the age. Naming the column beside every value is how a screen reader
/// reads a table, and for the same reason: by the fourth row nobody is still
/// holding the header in their head.
///
/// An empty cell is left out rather than announced. The document pane shows
/// exactly what will be spoken, so a cell that says nothing is visibly absent
/// rather than silently dropped.
///
/// Takes records rather than a file, because a table is not only a `.csv`: a
/// slide in a presentation can hold one too, and it would be a strange reader
/// that named the columns of a spreadsheet and then read the same four values
/// off a slide as four unrelated words. See [`crate::powerpoint`].
pub fn records_to_prose(mut records: Vec<Vec<String>>) -> String {
    if records.is_empty() {
        return String::new();
    }

    // One record is not a table, whatever it is: read it as the list it is.
    if records.len() == 1 {
        return sentence(&records[0].join(", "));
    }

    // A first row that is all numbers is data that happens to be first, not a
    // header: naming a column "42" for the rest of the file helps nobody.
    let has_header = !records[0].iter().all(|cell| looks_numeric(cell));
    let headings = if has_header {
        column_names(&records.remove(0))
    } else {
        let width = records.iter().map(Vec::len).max().unwrap_or(0);
        (1..=width).map(|n| t!("table.column", number = n)).collect()
    };

    let rows = tn!("table.rows", records.len());
    let columns = tn!("table.columns", headings.len());
    let mut out = String::new();
    out.push_str(&sentence(&if has_header {
        t!(
            "table.summary_headings",
            rows = rows,
            columns = columns,
            headings = headings.join(", ")
        )
    } else {
        t!("table.summary", rows = rows, columns = columns)
    }));

    for (index, record) in records.iter().enumerate() {
        // Naming the column beside every value is the point of this, and it is
        // also what makes the prose several times the size of the file it came
        // from — twenty times over, for a table of one-character cells under
        // long headings. The reader accepts files up to [`MAX_BYTES`]; what it
        // makes of one is held to the same figure, and says where it stopped.
        if out.len() as u64 >= MAX_BYTES {
            out.push_str("\n\n");
            out.push_str(&sentence(&t!(
                "table.cut_short",
                row = index,
                total = records.len()
            )));
            break;
        }
        out.push_str("\n\n");
        out.push_str(&sentence(&t!("table.row", number = index + 1)));
        for (column, cell) in record.iter().enumerate() {
            if cell.is_empty() {
                continue;
            }
            let heading = headings
                .get(column)
                .cloned()
                // A row with more cells than the header has columns still says
                // where each one sits.
                .unwrap_or_else(|| t!("table.column", number = column + 1));
            out.push(' ');
            out.push_str(&sentence(&t!(
                "table.cell",
                heading = heading,
                value = cell
            )));
        }
    }
    out
}

/// The name to read before each cell of a column.
fn column_names(first: &[String]) -> Vec<String> {
    first
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let name = name.trim().trim_end_matches(':').trim();
            if name.is_empty() {
                t!("table.column", number = index + 1)
            } else {
                name.to_string()
            }
        })
        .collect()
}

/// Whether a cell is a bare number, give or take the punctuation a spreadsheet
/// puts in one.
fn looks_numeric(cell: &str) -> bool {
    let trimmed = cell.trim_matches(|c: char| !c.is_alphanumeric());
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '_'))
}

/// End a fragment with a full stop, unless it already ends with something that
/// closes a sentence — the reader splits on those, and a doubled stop is an
/// audible pause in the wrong place.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
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

    /// The prose a table is read as, in English.
    ///
    /// Pinned, because the words are the language's now: another test
    /// switching it while this one runs would otherwise fail this one, once
    /// every few dozen runs.
    fn prose(csv: &str) -> String {
        crate::i18n::with_language("en", || table_to_prose(csv))
    }

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

    // ----------------------------------------------------------------- tables

    #[test]
    fn a_table_reads_each_cell_under_its_column() {
        let out = prose("Name,Age,City\nAlice,30,Leeds\nBo,41,Hull\n");
        assert_eq!(
            out,
            "A table of 2 rows and 3 columns: Name, Age, City.\n\n\
             Row 1. Name: Alice. Age: 30. City: Leeds.\n\n\
             Row 2. Name: Bo. Age: 41. City: Hull."
        );
    }

    /// The point of the whole thing: every value arrives with the name of the
    /// column it came from, so nothing depends on remembering a header line
    /// that was read out four rows ago.
    #[test]
    fn no_value_is_spoken_without_its_column() {
        let out = prose("Name,Age\nAlice,30\nBo,41\n");
        for (heading, value) in [("Name", "Alice"), ("Age", "30"), ("Name", "Bo"), ("Age", "41")] {
            assert!(out.contains(&format!("{heading}: {value}")), "{out:?}");
        }
    }

    /// A quoted field may hold the delimiter, a line break and doubled quotes,
    /// none of which may reach the listener as structure.
    #[test]
    fn quoted_fields_keep_what_is_inside_them() {
        let out = prose(
            "Name,Note\n\
             Alice,\"Leeds, then York\"\n\
             Bo,\"She said \"\"no\"\"\"\n\
             Cai,\"two\nlines\"\n",
        );
        assert!(out.contains("Note: Leeds, then York."), "{out:?}");
        assert!(out.contains("Note: She said \"no\"."), "{out:?}");
        assert!(out.contains("Note: two\nlines."), "{out:?}");
        assert!(out.contains("A table of 3 rows"), "{out:?}");
    }

    /// A spreadsheet saved where the comma is the decimal point separates with
    /// semicolons, and still calls the file `.csv`.
    #[test]
    fn the_delimiter_is_sniffed_rather_than_assumed() {
        for (text, cell) in [
            ("Name;Cost\nAlice;1,50\nBo;2,75\n", "Cost: 1,50"),
            ("Name\tCost\nAlice\t150\nBo\t275\n", "Cost: 150"),
            ("Name|Cost\nAlice|150\nBo|275\n", "Cost: 150"),
        ] {
            assert!(prose(text).contains(cell), "{text:?}");
        }
    }

    /// One prose column full of commas must not be mistaken for the separator.
    #[test]
    fn commas_inside_a_column_do_not_outvote_the_real_delimiter() {
        let out = prose(
            "Name;Note\n\
             Alice;one, two, three, four\n\
             Bo;five, six, seven, eight\n",
        );
        assert!(out.contains("Note: one, two, three, four."), "{out:?}");
    }

    /// A file that is all numbers has no header to name the columns with, and
    /// must not end up with a column called "42".
    #[test]
    fn a_numeric_first_row_is_data_rather_than_a_header() {
        let out = prose("1,2\n3,4\n");
        assert!(out.starts_with("A table of 2 rows and 2 columns."), "{out:?}");
        assert!(out.contains("Row 1. Column 1: 1. Column 2: 2."), "{out:?}");
    }

    /// Empty cells are left out — the document pane shows what will be spoken,
    /// so an absent cell is visibly absent — and a row with more cells than
    /// the header has columns still says where the extras sit.
    #[test]
    fn blank_cells_are_left_out_and_extra_ones_are_placed() {
        let out = prose("Name,Age,City\nAlice,,Leeds\nBo,41,Hull,spare\n");
        assert!(out.contains("Row 1. Name: Alice. City: Leeds."), "{out:?}");
        assert!(out.contains("Column 4: spare."), "{out:?}");
    }

    /// A value that already ends a sentence must not gain a second full stop:
    /// the splitter would hear the pair as an empty sentence between two
    /// cells.
    #[test]
    fn a_cell_that_ends_in_a_stop_does_not_gain_another() {
        let out = prose("Name,Note\nAlice,Ready.\nBo,Waiting?\n");
        assert!(out.contains("Note: Ready."), "{out:?}");
        assert!(!out.contains("Ready.."), "{out:?}");
        assert!(!out.contains("Waiting?."), "{out:?}");
    }

    /// One line is a list, not a table, and reads as one.
    #[test]
    fn a_single_line_is_read_as_the_list_it_is() {
        assert_eq!(prose("Alice,30,Leeds\n"), "Alice, 30, Leeds.");
        assert_eq!(prose(""), "");
    }

    /// Each row is its own paragraph, so the reader can be sent from row to
    /// row, and every cell is its own sentence within it.
    #[test]
    fn rows_are_paragraphs_and_cells_are_sentences() {
        let prose = prose("Name,Age\nAlice,30\nBo,41\n");
        let rows = split_chunks(&prose, ChunkMode::Paragraph);
        assert_eq!(rows.len(), 3, "summary and two rows: {rows:?}");
        let cells = sentences(&prose);
        assert!(cells.contains(&"Name: Alice.".to_string()), "{cells:?}");
        assert!(cells.contains(&"Age: 41.".to_string()), "{cells:?}");
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


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

use crate::t;
use std::path::Path;

/// A zip archive: every `.pptx` is one.
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 0x03, 0x04];
/// A Compound File Binary container: every `.ppt`, and every other Office file
/// of that generation.
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

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
    } else if raw.starts_with(&CFB_MAGIC) {
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
            out.push_str(&format!("Slide {number}. No text on this slide."));
            continue;
        }
        out.push_str(&format!("Slide {number}."));
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
/// A deliberately small scanner rather than an XML parser. Everything wanted
/// here is in two elements — `<a:p>` is a paragraph and `<a:t>` is a run of
/// text inside one — and both a table cell and a text box put their words in
/// exactly those, so the shape they sit in never has to be understood.
fn slide_paragraphs(xml: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut rest = xml;

    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];

        let name = tag
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '/'])
            .next()
            .unwrap_or("");
        match name {
            // A paragraph ends where the next begins, so both edges flush: an
            // unclosed one still reaches the listener.
            "a:p" if !tag.starts_with('/') => flush(&mut current, &mut paragraphs),
            "a:p" => flush(&mut current, &mut paragraphs),
            // A soft line break inside a paragraph. Kept as a space, because a
            // newline mid-paragraph makes several speech back ends pause as
            // though at a full stop.
            "a:br" => current.push(' '),
            "a:t" if !tag.starts_with('/') => {
                let Some(end) = rest.find('<') else { break };
                current.push_str(&decode_entities(&rest[..end]));
                rest = &rest[end..];
            }
            _ => {}
        }
    }
    flush(&mut current, &mut paragraphs);
    paragraphs
}

fn flush(current: &mut String, paragraphs: &mut Vec<String>) {
    let text = current.trim().to_string();
    if !text.is_empty() {
        paragraphs.push(text);
    }
    current.clear();
}

/// The five named entities XML defines, and the numeric form.
///
/// Anything else is left as it stands: an undefined entity is not this app's
/// to guess at, and `&` on its own is far more likely to be a typo in
/// somebody's slide than a reference to anything.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|end| *end <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
            TEXT_CHARS_ATOM => Some(utf16_le(body)),
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

fn utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
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

/// Enough of [MS-CFB] to pull one named stream out of a legacy Office file.
mod cfb {
    use anyhow::{bail, Context, Result};

    /// Sector numbers at or above this are markers rather than sectors: end of
    /// chain, free, and the two that name the allocation tables themselves.
    const FIRST_MARKER: u32 = 0xFFFF_FFFA;
    pub const DIRECTORY_ENTRY_BYTES: usize = 128;
    /// A ceiling on how many sectors one chain may be, so that a file whose
    /// allocation table points in a circle stops rather than spins. Enough for
    /// a 64 MB stream of 512-byte sectors, which is the largest file the reader
    /// accepts at all. A chain is held to the smaller of this and the number of
    /// sectors the file actually has: a two-sector loop would otherwise be
    /// followed a quarter of a million times, turning a kilobyte of malformed
    /// input into a hundred megabytes of output.
    const MAX_CHAIN: usize = 256 * 1024;

    pub struct CompoundFile<'a> {
        data: &'a [u8],
        sector_size: usize,
        fat: Vec<u32>,
        mini_fat: Vec<u32>,
        mini_stream: Vec<u8>,
        mini_cutoff: u32,
        directory: Vec<u8>,
    }

    impl<'a> CompoundFile<'a> {
        pub fn open(data: &'a [u8]) -> Result<Self> {
            if data.len() < 512 {
                bail!("the file is too short to be a compound file");
            }
            let word = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
            let long =
                |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);

            // Version 3 uses 512-byte sectors and version 4 uses 4096; nothing
            // else has ever been defined, and an arbitrary shift here would be
            // an arbitrary allocation below.
            let sector_shift = word(30);
            if !matches!(sector_shift, 9 | 12) {
                bail!("this compound file uses a sector size no version defines");
            }
            let sector_size = 1usize << sector_shift;

            let mut file = Self {
                data,
                sector_size,
                fat: Vec::new(),
                mini_fat: Vec::new(),
                mini_stream: Vec::new(),
                mini_cutoff: long(56),
                directory: Vec::new(),
            };

            // The DIFAT: 109 entries in the header, then a chain of sectors
            // holding the rest, each ending in a pointer to the next.
            let mut difat: Vec<u32> = (0..109).map(|i| long(76 + i * 4)).collect();
            let mut next = long(68);
            let mut seen = 0usize;
            while next < FIRST_MARKER && seen < MAX_CHAIN {
                let sector = file.sector(next)?;
                let entries = sector_size / 4 - 1;
                difat.extend((0..entries).map(|i| {
                    u32::from_le_bytes([
                        sector[i * 4],
                        sector[i * 4 + 1],
                        sector[i * 4 + 2],
                        sector[i * 4 + 3],
                    ])
                }));
                next = u32::from_le_bytes([
                    sector[sector_size - 4],
                    sector[sector_size - 3],
                    sector[sector_size - 2],
                    sector[sector_size - 1],
                ]);
                seen += 1;
            }

            let fat_sectors = long(44) as usize;
            for &sector_number in difat.iter().take(fat_sectors) {
                if sector_number >= FIRST_MARKER {
                    continue;
                }
                let sector = file.sector(sector_number)?;
                file.fat.extend((0..sector_size / 4).map(|i| {
                    u32::from_le_bytes([
                        sector[i * 4],
                        sector[i * 4 + 1],
                        sector[i * 4 + 2],
                        sector[i * 4 + 3],
                    ])
                }));
            }
            if file.fat.is_empty() {
                bail!("this compound file has no allocation table");
            }

            file.directory = file.chain(long(48), None)?;

            // The mini stream is one ordinary stream, held by the root entry,
            // that every stream shorter than the cutoff is carved out of.
            let root = file
                .entry(0)
                .context("this compound file has no root entry")?;
            file.mini_stream = file.chain(root.start, Some(root.size))?;
            let mut mini_fat = file.chain(long(60), None)?;
            mini_fat.truncate(long(64) as usize * sector_size);
            file.mini_fat = mini_fat
                .as_chunks::<4>()
                .0
                .iter()
                .map(|four| u32::from_le_bytes(*four))
                .collect();

            Ok(file)
        }

        /// One sector's bytes, by number. Sector zero starts immediately after
        /// the 512-byte header, whatever the sector size is.
        fn sector(&self, number: u32) -> Result<&'a [u8]> {
            // Checked rather than plain arithmetic: `number` comes out of the
            // file, and on a 32-bit target a large one overflows — which is a
            // panic in a debug build and a wrong slice in a release one.
            (number as u64 + 1)
                .checked_mul(self.sector_size as u64)
                .and_then(|at| usize::try_from(at).ok())
                .and_then(|at| self.data.get(at..at.checked_add(self.sector_size)?))
                .context("this compound file points past its own end")
        }

        /// Follow a chain through the allocation table, concatenating it.
        fn chain(&self, start: u32, size: Option<u64>) -> Result<Vec<u8>> {
            let limit = MAX_CHAIN.min(self.data.len() / self.sector_size + 1);
            let mut out = Vec::new();
            let mut next = start;
            let mut visited = 0usize;
            while next < FIRST_MARKER {
                if visited >= limit {
                    bail!("a chain in this compound file never ends");
                }
                out.extend_from_slice(self.sector(next)?);
                next = *self
                    .fat
                    .get(next as usize)
                    .context("this compound file points outside its allocation table")?;
                visited += 1;
            }
            if let Some(size) = size {
                out.truncate(size as usize);
            }
            Ok(out)
        }

        /// The same, through the mini allocation table, for the short streams
        /// that live inside the mini stream rather than in sectors of their own.
        fn mini_chain(&self, start: u32, size: u64) -> Result<Vec<u8>> {
            let mini_size = 64usize;
            let limit = MAX_CHAIN.min(self.mini_stream.len() / mini_size + 1);
            let mut out = Vec::new();
            let mut next = start;
            let mut visited = 0usize;
            while next < FIRST_MARKER && (out.len() as u64) < size {
                if visited >= limit {
                    bail!("a chain in this compound file never ends");
                }
                let at = usize::try_from(next as u64 * mini_size as u64).ok();
                out.extend_from_slice(
                    at.and_then(|at| self.mini_stream.get(at..at.checked_add(mini_size)?))
                        .context("this compound file points past its own mini stream")?,
                );
                next = *self
                    .mini_fat
                    .get(next as usize)
                    .context("this compound file points outside its mini allocation table")?;
                visited += 1;
            }
            out.truncate(size as usize);
            Ok(out)
        }

        fn entry(&self, index: usize) -> Option<Entry> {
            let at = index * DIRECTORY_ENTRY_BYTES;
            let raw = self.directory.get(at..at + DIRECTORY_ENTRY_BYTES)?;
            // The name is UTF-16 with its terminating nul counted in the length.
            let name_bytes = u16::from_le_bytes([raw[64], raw[65]]).saturating_sub(2) as usize;
            let units: Vec<u16> = raw
                .get(..name_bytes.min(64))?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            Some(Entry {
                name: String::from_utf16_lossy(&units),
                kind: raw[66],
                start: u32::from_le_bytes([raw[116], raw[117], raw[118], raw[119]]),
                size: u64::from_le_bytes(raw[120..128].try_into().ok()?),
            })
        }

        /// The contents of the named stream, or `None` if the file has no such
        /// stream.
        ///
        /// The two are told apart deliberately: a stream that is there and
        /// cannot be read is a different thing from one that was never there,
        /// and reporting the first as the second sends whoever reads the
        /// message looking for the wrong problem.
        pub fn stream(&self, wanted: &str) -> Result<Option<Vec<u8>>> {
            let count = self.directory.len() / DIRECTORY_ENTRY_BYTES;
            let Some(entry) = (0..count)
                .filter_map(|index| self.entry(index))
                .find(|entry| entry.kind == 2 && entry.name == wanted)
            else {
                return Ok(None);
            };
            if entry.size < self.mini_cutoff as u64 {
                self.mini_chain(entry.start, entry.size).map(Some)
            } else {
                self.chain(entry.start, Some(entry.size)).map(Some)
            }
        }
    }

    struct Entry {
        name: String,
        /// 1 is a storage, 2 a stream, 5 the root.
        kind: u8,
        start: u32,
        size: u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_empty_slide_is_counted_rather_than_skipped() {
        let out = lay_out(&[
            vec!["First.".to_string()],
            Vec::new(),
            vec!["Third.".to_string()],
        ]);
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
        let refusal = text_from_bytes(b"Just some text").unwrap_err().to_string();
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
        let out = text_from_bytes(&pptx(&names)).expect("reads");

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
        let empty = pptx(&[]);
        let refusal = text_from_bytes(&empty).unwrap_err().to_string();
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
        file[..8].copy_from_slice(&CFB_MAGIC);
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
        let out = text_from_bytes(&file).expect("reads");
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
        assert_eq!(
            text_from_bytes(&file).expect("reads"),
            "Slide 1.\n\nThe only slide"
        );
    }

    #[test]
    fn a_password_protected_presentation_says_so_rather_than_reading_noise() {
        let stream = container(CRYPT_SESSION_CONTAINER, &[0u8; 16]);
        let file = compound_file("PowerPoint Document", &stream);
        let refusal = text_from_bytes(&file).unwrap_err().to_string();
        assert!(refusal.contains("password-protected"), "{refusal}");
    }

    #[test]
    fn a_compound_file_that_is_not_a_presentation_says_so() {
        let file = compound_file("WordDocument", b"not a deck");
        let refusal = text_from_bytes(&file).unwrap_err().to_string();
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

        let refusal = text_from_bytes(&archive).unwrap_err().to_string();
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
        let refusal = text_from_bytes(&file).unwrap_err().to_string();
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

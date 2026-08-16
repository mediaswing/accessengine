//! The file as a whole: every object in it, and the tree of pages built out of
//! them.
//!
//! # Why the cross reference table is ignored
//!
//! A PDF is meant to be read backwards. The last line points at a cross
//! reference table, that table gives the offset of every object, and a reader
//! seeks to the ones it wants. This module does not do that. It scans the
//! whole file for `12 0 obj` headers instead and reads each object where it
//! finds it.
//!
//! That is deliberate. Wrong offsets are the single commonest fault in real
//! files — anything that appends to a PDF without rewriting the table properly
//! leaves them stale, and a reader that trusts them then reports a perfectly
//! readable document as damaged. Scanning finds the objects wherever they
//! actually are. The cost is reading the file once, which for the documents a
//! person sits and listens to is a few milliseconds, and losing the ability to
//! tell two generations of one object number apart — see [`super::object`].
//!
//! Where an object is defined more than once, the definition latest in the
//! file wins, because that is where an incremental edit appends its new
//! version.

use super::filters;
use super::object::{Dict, Lexer, Object, find, is_regular, is_white};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

/// Returned for a reference that points at nothing, which is legal — the
/// specification says a missing object *is* null — and common in files with a
/// damaged tail.
static NULL: Object = Object::Null;

/// A ceiling on pages, so a malformed tree that somehow slips past the cycle
/// check still ends.
const MAX_PAGES: usize = 50_000;

pub struct Document<'a> {
    data: &'a [u8],
    /// Object number to where it was defined and what it is. The offset is
    /// kept so that a later definition always beats an earlier one, including
    /// when one of the two lives inside an object stream.
    objects: HashMap<u32, (usize, Object)>,
    /// Dictionaries that could be the file's trailer: the real ones, and the
    /// cross reference streams that replace them in newer files.
    trailers: Vec<Dict>,
}

/// One page, with the resources that apply to it.
pub struct Page<'a> {
    pub dict: &'a Dict,
    /// `/Resources` is inheritable: a page that names none uses its parent's,
    /// and a file whose fonts are declared once on the tree root is perfectly
    /// ordinary. Resolved during the walk, since a page on its own cannot see
    /// its ancestors.
    pub resources: Option<&'a Dict>,
}

impl<'a> Document<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut objects = HashMap::new();
        scan_objects(data, &mut objects);

        let mut document = Self {
            data,
            objects,
            trailers: Vec::new(),
        };
        document.expand_object_streams();
        document.trailers = document.collect_trailers();

        if document.objects.is_empty() {
            bail!("no PDF objects could be found in the file");
        }
        Ok(document)
    }

    /// Whether the file's contents are encrypted.
    ///
    /// Includes the very common case of a document with no password at all,
    /// only restrictions on printing or copying: those are still encrypted,
    /// and the strings and streams inside are still unreadable without
    /// undoing it.
    pub fn is_encrypted(&self) -> bool {
        self.trailers
            .iter()
            .any(|trailer| matches!(trailer.get("Encrypt"), Some(object) if *object != Object::Null))
    }

    /// Follows a reference to the object it names. Chains are bounded: a file
    /// can contain a reference that points at itself.
    pub fn resolve<'b>(&'b self, object: &'b Object) -> &'b Object {
        let mut current = object;
        for _ in 0..32 {
            let Object::Ref(number) = current else {
                return current;
            };
            match self.objects.get(number) {
                Some((_, next)) => current = next,
                None => return &NULL,
            }
        }
        &NULL
    }

    /// A dictionary entry with any reference already followed.
    pub fn get<'b>(&'b self, dict: &'b Dict, key: &str) -> Option<&'b Object> {
        match dict.get(key).map(|object| self.resolve(object)) {
            Some(Object::Null) | None => None,
            Some(object) => Some(object),
        }
    }

    pub fn get_dict<'b>(&'b self, dict: &'b Dict, key: &str) -> Option<&'b Dict> {
        self.get(dict, key).and_then(Object::as_dict)
    }

    pub fn get_name<'b>(&'b self, dict: &'b Dict, key: &str) -> Option<&'b str> {
        self.get(dict, key).and_then(Object::as_name)
    }

    /// The decoded contents of a stream object.
    pub fn stream_data(&self, object: &Object) -> Result<Vec<u8>> {
        let object = self.resolve(object);
        let Object::Stream(dict, range) = object else {
            bail!("not a stream");
        };
        let raw = self.data.get(range.clone()).unwrap_or_default();
        filters::decode(dict, raw, &|value| self.resolve(value).clone())
    }

    /// The pages, in the order the document puts them.
    ///
    /// Falls back to every `/Type /Page` object in the file when the tree is
    /// missing or unreadable, ordered by object number — writers emit pages in
    /// order often enough that this usually reads correctly, and some order is
    /// better than refusing a damaged file outright.
    pub fn pages(&self) -> Vec<Page<'_>> {
        let mut pages = Vec::new();
        if let Some(root) = self.catalog()
            && let Some(tree) = root.get("Pages")
        {
            let mut seen = HashSet::new();
            self.walk_pages(tree, None, &mut seen, &mut pages);
        }
        if pages.is_empty() {
            pages = self.loose_pages();
        }
        pages
    }

    /// The document catalogue: `/Root` from a trailer, or failing that any
    /// object that says it is one.
    fn catalog(&self) -> Option<&Dict> {
        self.trailers
            .iter()
            .find_map(|trailer| self.get_dict(trailer, "Root"))
            .or_else(|| {
                self.in_file_order()
                    .find_map(|object| match object.as_dict() {
                        Some(dict) if self.get_name(dict, "Type") == Some("Catalog") => Some(dict),
                        _ => None,
                    })
            })
    }

    fn walk_pages<'b>(
        &'b self,
        node: &'b Object,
        inherited: Option<&'b Dict>,
        seen: &mut HashSet<u32>,
        out: &mut Vec<Page<'b>>,
    ) {
        if out.len() >= MAX_PAGES {
            return;
        }
        // A tree that points back at itself is rare but not unknown, and it
        // must not become an endless walk.
        if let Object::Ref(number) = node
            && !seen.insert(*number)
        {
            return;
        }
        let Some(dict) = self.resolve(node).as_dict() else {
            return;
        };
        let resources = self.get_dict(dict, "Resources").or(inherited);

        // `/Type` is missing often enough that the presence of `/Kids` is the
        // more dependable question.
        match self.get(dict, "Kids").and_then(Object::as_array) {
            Some(kids) if self.get_name(dict, "Type") != Some("Page") => {
                for kid in kids {
                    self.walk_pages(kid, resources, seen, out);
                }
            }
            _ => out.push(Page { dict, resources }),
        }
    }

    fn loose_pages(&self) -> Vec<Page<'_>> {
        let mut numbers: Vec<&u32> = self.objects.keys().collect();
        numbers.sort_unstable();
        numbers
            .into_iter()
            .filter_map(|number| self.objects.get(number))
            .filter_map(|(_, object)| object.as_dict())
            .filter(|dict| self.get_name(dict, "Type") == Some("Page"))
            .map(|dict| Page {
                resources: self.get_dict(dict, "Resources"),
                dict,
            })
            .take(MAX_PAGES)
            .collect()
    }

    /// Every object, ordered by where it sits in the file — which for the
    /// fallbacks that need it is the closest thing to document order there is.
    fn in_file_order(&self) -> impl Iterator<Item = &Object> {
        let mut entries: Vec<&(usize, Object)> = self.objects.values().collect();
        entries.sort_unstable_by_key(|(offset, _)| *offset);
        entries.into_iter().map(|(_, object)| object)
    }

    /// Unpacks the object streams, which is where a file written this century
    /// keeps most of its dictionaries — including, often, the page tree.
    ///
    /// An object stream holds a run of objects with no `… obj` headers, so the
    /// scan cannot see them; it has a table of numbers and offsets at the
    /// front instead.
    fn expand_object_streams(&mut self) {
        let containers: Vec<(usize, Object)> = self
            .objects
            .values()
            .filter(|(_, object)| {
                object
                    .as_dict()
                    .and_then(|dict| dict.get("Type"))
                    .and_then(Object::as_name)
                    == Some("ObjStm")
            })
            .cloned()
            .collect();

        for (offset, container) in containers {
            let Ok(data) = self.stream_data(&container) else {
                continue;
            };
            let Some(dict) = container.as_dict() else {
                continue;
            };
            let count = self.get(dict, "N").and_then(Object::as_i64).unwrap_or(0);
            let first = self.get(dict, "First").and_then(Object::as_i64).unwrap_or(0);
            let (Ok(count), Ok(first)) = (usize::try_from(count), usize::try_from(first)) else {
                continue;
            };

            let mut header = Lexer::new(&data);
            let mut entries = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                let (Some(number), Some(position)) = (
                    header.next_object().and_then(|o| o.as_i64()),
                    header.next_object().and_then(|o| o.as_i64()),
                ) else {
                    break;
                };
                let (Ok(number), Ok(position)) = (u32::try_from(number), usize::try_from(position))
                else {
                    break;
                };
                entries.push((number, position));
            }

            for (number, position) in entries {
                let Some(start) = first.checked_add(position) else {
                    continue;
                };
                if start >= data.len() {
                    continue;
                }
                let Some(object) = Lexer::at(&data, start).next_object() else {
                    continue;
                };
                // The objects inside share their container's position in the
                // file, so an appended revision still wins over the original.
                match self.objects.get(&number) {
                    Some((earlier, _)) if *earlier > offset => {}
                    _ => {
                        self.objects.insert(number, (offset, object));
                    }
                }
            }
        }
    }

    /// Every dictionary that could be a trailer, latest in the file first.
    ///
    /// Both spellings are collected: the `trailer` keyword of a classic file,
    /// and the dictionary of a cross reference stream, which is where a newer
    /// file keeps `/Root` and `/Encrypt` instead.
    fn collect_trailers(&self) -> Vec<Dict> {
        let mut found: Vec<(usize, Dict)> = Vec::new();
        let mut pos = 0usize;
        while let Some(index) = find(self.data, b"trailer", pos) {
            pos = index + b"trailer".len();
            let mut lexer = Lexer::at(self.data, pos);
            if let Some(Object::Dict(dict)) = lexer.next_object() {
                found.push((index, dict));
            }
        }
        for (offset, object) in self.objects.values() {
            if let Some(dict) = object.as_dict()
                && self.get_name(dict, "Type") == Some("XRef")
            {
                found.push((*offset, dict.clone()));
            }
        }
        found.sort_unstable_by_key(|(offset, _)| std::cmp::Reverse(*offset));
        found.into_iter().map(|(_, dict)| dict).collect()
    }
}

/// Reads every `<number> <generation> obj … endobj` in the file.
///
/// Streams are stepped over rather than searched, both because it is faster
/// and because compressed data is quite capable of containing the bytes `obj`
/// by chance.
fn scan_objects(data: &[u8], objects: &mut HashMap<u32, (usize, Object)>) {
    let mut pos = 0usize;
    while let Some(index) = find(data, b"obj", pos) {
        pos = index + 3;
        // `obj` has to be a word of its own: `/Subject` ends in those letters.
        if data.get(index + 3).is_some_and(|&byte| is_regular(byte)) {
            continue;
        }
        let Some((number, start)) = header_before(data, index) else {
            continue;
        };
        let mut lexer = Lexer::at(data, index + 3);
        let Some(object) = lexer.next_object() else {
            continue;
        };
        pos = lexer.pos.max(pos);
        match objects.get(&number) {
            Some((earlier, _)) if *earlier > start => {}
            _ => {
                objects.insert(number, (start, object));
            }
        }
    }
}

/// Reads the `12 0` in front of an `obj` keyword, returning the object number
/// and where the header starts.
fn header_before(data: &[u8], keyword: usize) -> Option<(u32, usize)> {
    let mut pos = keyword;

    let digits_before = |data: &[u8], mut pos: usize| -> Option<(u64, usize)> {
        let end = pos;
        while pos > 0 && data[pos - 1].is_ascii_digit() {
            pos -= 1;
        }
        if pos == end || end - pos > 10 {
            return None;
        }
        let text = std::str::from_utf8(&data[pos..end]).ok()?;
        Some((text.parse().ok()?, pos))
    };
    let space_before = |data: &[u8], mut pos: usize| -> Option<usize> {
        let end = pos;
        while pos > 0 && is_white(data[pos - 1]) {
            pos -= 1;
        }
        (pos < end).then_some(pos)
    };

    pos = space_before(data, pos)?;
    let (_generation, before) = digits_before(data, pos)?;
    pos = space_before(data, before)?;
    let (number, start) = digits_before(data, pos)?;

    // The header has to begin a token, or `120 0 obj` would also be read as
    // object 20.
    if start > 0 && is_regular(data[start - 1]) {
        return None;
    }
    u32::try_from(number).ok().map(|number| (number, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole small PDF, written out the way a plain writer does: a catalogue,
    /// a page tree, one page, and a content stream.
    const SIMPLE: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /Font << /F1 5 0 R >> >> >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj
4 0 obj << /Length 24 >> stream
BT (hello) Tj ET
endstream endobj
5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj
trailer << /Root 1 0 R /Size 6 >>
%%EOF";

    #[test]
    fn finds_every_object_and_follows_references() {
        let doc = Document::parse(SIMPLE).unwrap();
        assert_eq!(doc.objects.len(), 5);
        let catalog = doc.catalog().expect("a catalogue");
        assert_eq!(doc.get_name(catalog, "Type"), Some("Catalog"));
    }

    #[test]
    fn walks_the_page_tree_and_inherits_resources() {
        let doc = Document::parse(SIMPLE).unwrap();
        let pages = doc.pages();
        assert_eq!(pages.len(), 1);
        // The page names no resources of its own; the tree root's have to
        // reach it.
        let fonts = pages[0]
            .resources
            .and_then(|resources| doc.get_dict(resources, "Font"))
            .expect("inherited fonts");
        assert!(fonts.contains_key("F1"));
    }

    #[test]
    fn reads_a_page_content_stream() {
        let doc = Document::parse(SIMPLE).unwrap();
        let pages = doc.pages();
        let contents = pages[0].dict.get("Contents").unwrap();
        let data = doc.stream_data(contents).unwrap();
        assert_eq!(data, b"BT (hello) Tj ET");
    }

    /// An appended edit repeats an object number with a new definition, and
    /// the later one is the current one.
    #[test]
    fn a_later_definition_of_an_object_wins() {
        let mut source = SIMPLE.to_vec();
        source.extend_from_slice(b"\n5 0 obj << /Type /Font /BaseFont /Courier >> endobj\n");
        let doc = Document::parse(&source).unwrap();
        let font = doc.resolve(&Object::Ref(5)).as_dict().unwrap();
        assert_eq!(doc.get_name(font, "BaseFont"), Some("Courier"));
    }

    #[test]
    fn a_page_tree_that_points_at_itself_still_returns() {
        let source: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [2 0 R 3 0 R] >> endobj
3 0 obj << /Type /Page >> endobj
trailer << /Root 1 0 R >>";
        let doc = Document::parse(source).unwrap();
        assert_eq!(doc.pages().len(), 1);
    }

    /// Without a usable tree the pages still have to be found, or a file with
    /// a damaged catalogue reads as empty.
    #[test]
    fn falls_back_to_loose_page_objects() {
        let source: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Page /Contents 9 0 R >> endobj
2 0 obj << /Type /Page /Contents 9 0 R >> endobj";
        let doc = Document::parse(source).unwrap();
        assert_eq!(doc.pages().len(), 2);
    }

    #[test]
    fn spots_an_encrypted_file() {
        let source: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Catalog >> endobj
trailer << /Root 1 0 R /Encrypt 8 0 R >>";
        assert!(Document::parse(source).unwrap().is_encrypted());
    }

    #[test]
    fn a_word_ending_in_obj_is_not_an_object_header() {
        // `/Subject` ends in the same three letters, and the number in front
        // of it would otherwise make a plausible-looking header.
        let source: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Catalog >> endobj
2 0 /Subject (x)";
        let doc = Document::parse(source).unwrap();
        assert_eq!(doc.objects.len(), 1);
    }
}

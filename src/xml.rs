//! The parts of reading XML that more than one format needs.
//!
//! Three readers in this app walk XML with a small hand-written tag scanner
//! rather than a parser crate — [`crate::powerpoint`] over a `.pptx`,
//! [`crate::word`] over a `.docx`, and [`crate::playlist`] over a `media.xml`.
//! The scanners themselves stay with the format they read, because a slide, a
//! page and a playlist are laid out differently and the element names differ
//! down to the namespace prefix. What sits here is what does not differ.

/// The five named entities XML defines, and the numeric form.
///
/// Anything else is left as it stands: an undefined entity is not this app's
/// to guess at, and `&` on its own is far more likely to be a typo in
/// somebody's document than a reference to anything.
pub fn decode_entities(text: &str) -> String {
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

/// A table being read out of a document, and how deep inside the table element
/// the scanner is.
///
/// The rows finished, the row in hand, and a depth rather than a flag: neither
/// DrawingML nor WordprocessingML puts a table inside a table cell in any file
/// this has been shown, but both allow it and both are written by programs
/// other than Office.
#[derive(Default)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub row: Vec<String>,
    pub depth: usize,
}

impl Table {
    /// The table as paragraphs, through the same builder a spreadsheet uses.
    pub fn finish(&mut self) -> Vec<String> {
        // A last row the file never closed, and empty rows used for spacing,
        // are neither of them rows of the table.
        if !self.row.is_empty() {
            self.rows.push(std::mem::take(&mut self.row));
        }
        self.rows
            .retain(|row| row.iter().any(|cell| !cell.is_empty()));

        let prose = crate::document::records_to_prose(std::mem::take(&mut self.rows));
        prose
            .split("\n\n")
            .filter(|paragraph| !paragraph.trim().is_empty())
            .map(str::to_string)
            .collect()
    }
}

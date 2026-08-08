//! Reading a spreadsheet export aloud as a table.
//!
//! A CSV read out row by row is a wall of bare values: "Ada, Lovelace, 36" says
//! nothing about which is the name and which the age, and by the third row
//! nobody is still counting columns. So the first row is taken as the headings
//! and every value is spoken under its own — "First name: Ada" — which is the
//! one thing that makes a table make sense without seeing it.
//!
//! Headings come from a spreadsheet, so they are often `first_name` rather than
//! `First name`. Underscores become spaces before anything is spoken.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// The same ceiling plain text has: a table is read whole into memory, and a
/// mistyped path to a database dump should be a message rather than an
/// allocation failure.
const MAX_CSV_BYTES: u64 = 64 * 1024 * 1024;

/// The separators worth telling apart. A `.tsv` is tabs, and Excel writes
/// semicolons wherever the locale's decimal separator is a comma — both are
/// common enough that reading one as a single column per row would be a
/// frequent and baffling failure.
const DELIMITERS: [char; 3] = [',', ';', '\t'];

pub fn extract(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?
        .len();
    if size > MAX_CSV_BYTES {
        bail!(
            "{} is {:.0} MB, which is more table than this app will read at once",
            path.file_name().unwrap_or_default().to_string_lossy(),
            size as f64 / (1024.0 * 1024.0)
        );
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let text = super::txt::decode(&bytes);
    let rows = parse(&text, delimiter_of(&text));
    if rows.is_empty() {
        bail!(
            "{} has no rows in it",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(speak(&rows))
}

/// Whichever candidate separator appears most often in the first line. Counted
/// without regard for quoting, which cannot pick the wrong one: a heading with
/// a comma inside quotes still leaves commas ahead of tabs and semicolons that
/// are not there at all.
fn delimiter_of(text: &str) -> char {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    DELIMITERS
        .into_iter()
        .max_by_key(|&candidate| first.matches(candidate).count())
        .unwrap_or(',')
}

/// Splits CSV text into rows of fields, following RFC 4180: a field wrapped in
/// double quotes may hold the delimiter, a line break, or a doubled quote
/// standing for one of itself. A quote anywhere else is an ordinary character,
/// since a hand-written file is likelier to contain 5" than to mean anything
/// by it.
///
/// Rows that are entirely empty are dropped — a trailing newline is not a row,
/// and neither is the blank line spreadsheets like to leave at the end.
fn parse(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
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
            '"' if field.is_empty() => quoted = true,
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ if c == delimiter => row.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }

    rows.retain(|row| row.iter().any(|cell| !cell.trim().is_empty()));
    rows
}

/// Turns parsed rows into something worth listening to: how big the table is,
/// then every value under the heading it belongs to.
fn speak(rows: &[Vec<String>]) -> String {
    let headings: Vec<String> = rows[0]
        .iter()
        .enumerate()
        .map(|(index, raw)| heading(raw, index))
        .collect();
    let body = &rows[1..];

    let mut out = format!(
        "Table with {} and {}.",
        counted(body.len(), "row", "rows"),
        counted(headings.len(), "column", "columns")
    );

    // With no rows to hang them on, the headings would otherwise go unsaid —
    // and "what are the columns?" is the only question a heading-only file can
    // answer.
    if body.is_empty() {
        out.push_str("\n\nThe columns are: ");
        out.push_str(&headings.join(", "));
        out.push('.');
        return out;
    }

    out.push('\n');
    for (index, row) in body.iter().enumerate() {
        let values: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(column, value)| {
                let heading = headings
                    .get(column)
                    .cloned()
                    .unwrap_or_else(|| format!("Column {}", column + 1));
                phrase(&heading, value)
            })
            .collect();
        out.push_str(&format!("\nRow {}. {}", index + 1, values.join(" ")));
    }
    out
}

/// A heading as it should be heard: underscores are spaces, runs of whitespace
/// are one space, and a column with no heading at all is named by its position
/// rather than announced as nothing.
fn heading(raw: &str, index: usize) -> String {
    let spaced = raw.replace('_', " ");
    let cleaned = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        format!("Column {}", index + 1)
    } else {
        cleaned
    }
}

/// One value under its heading. A cell that runs onto several lines is flattened
/// to one, since the line breaks inside it are the spreadsheet's formatting and
/// not something to pause for.
fn phrase(heading: &str, value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return format!("{heading}: empty.");
    }
    if value.ends_with(['.', '!', '?']) {
        format!("{heading}: {value}")
    } else {
        format!("{heading}: {value}.")
    }
}

/// "no rows", "1 row", "12 rows" — a count as it would be said aloud.
fn counted(n: usize, singular: &str, plural: &str) -> String {
    match n {
        0 => format!("no {plural}"),
        1 => format!("1 {singular}"),
        _ => format!("{n} {plural}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_table_under_its_headings() {
        let spoken = speak(&parse(
            "first_name,last_name,age\nAda,Lovelace,36\nAlan,Turing,41\n",
            ',',
        ));
        assert_eq!(
            spoken,
            "Table with 2 rows and 3 columns.\n\
             \nRow 1. first name: Ada. last name: Lovelace. age: 36.\
             \nRow 2. first name: Alan. last name: Turing. age: 41."
        );
    }

    /// The counts are the first thing said, so a table of one must not
    /// announce itself as "1 rows".
    #[test]
    fn counts_agree_with_what_they_count() {
        let spoken = speak(&parse("name,age\nAda,36\n", ','));
        assert!(
            spoken.starts_with("Table with 1 row and 2 columns."),
            "{spoken}"
        );
        assert_eq!(counted(0, "row", "rows"), "no rows");
    }

    /// A quoted field is one value however much punctuation is in it.
    #[test]
    fn quoted_fields_keep_their_commas_quotes_and_line_breaks() {
        let rows = parse(
            "name,note\n\"Lovelace, Ada\",\"said \"\"hello\"\"\nand then left\"\n",
            ',',
        );
        assert_eq!(rows[1][0], "Lovelace, Ada");
        assert_eq!(rows[1][1], "said \"hello\"\nand then left");
        // The embedded newline must not become a pause mid-row.
        assert_eq!(
            phrase("note", &rows[1][1]),
            "note: said \"hello\" and then left."
        );
    }

    #[test]
    fn semicolons_and_tabs_are_recognised_as_separators() {
        assert_eq!(delimiter_of("a;b;c\n1;2;3"), ';');
        assert_eq!(delimiter_of("a\tb\tc\n1\t2\t3"), '\t');
        assert_eq!(delimiter_of("a,b,c\n1,2,3"), ',');
        // A comma inside a quoted heading does not make it a comma-separated
        // file when every real separator is a semicolon.
        assert_eq!(delimiter_of("\"Lovelace, Ada\";age;city"), ';');
    }

    #[test]
    fn windows_line_endings_do_not_leave_stray_returns() {
        let rows = parse("a,b\r\n1,2\r\n", ',');
        assert_eq!(rows, vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    /// Ragged rows are what real exports look like; neither shape may panic.
    #[test]
    fn rows_longer_or_shorter_than_the_headings_still_read() {
        let spoken = speak(&parse("name,age\nAda\nAlan,41,extra\n", ','));
        assert!(spoken.contains("Row 1. name: Ada."), "{spoken}");
        assert!(
            spoken.contains("Row 2. name: Alan. age: 41. Column 3: extra."),
            "{spoken}"
        );
    }

    #[test]
    fn an_empty_cell_is_said_rather_than_skipped() {
        let spoken = speak(&parse("name,age\nAda,\n", ','));
        assert!(spoken.contains("name: Ada. age: empty."), "{spoken}");
    }

    /// A file with headings and nothing else still has something to say.
    #[test]
    fn a_table_with_no_rows_names_its_columns() {
        let spoken = speak(&parse("first_name,age\n", ','));
        assert_eq!(
            spoken,
            "Table with no rows and 2 columns.\n\nThe columns are: first name, age."
        );
    }

    /// A heading is a label, not prose: underscores are separators in every
    /// spreadsheet export and sound like nothing at all when spoken.
    #[test]
    fn underscores_in_headings_become_spaces() {
        assert_eq!(heading("date_of_birth", 0), "date of birth");
        assert_eq!(heading("  spaced   out  ", 0), "spaced out");
        assert_eq!(heading("", 2), "Column 3");
    }

    /// The whole path off disk, on a file shaped the way Excel writes one: a
    /// byte-order mark in front and CRLF line endings throughout.
    #[test]
    fn a_csv_file_is_read_from_disk_as_a_table() {
        let path = std::env::temp_dir().join("soe-csv-test.csv");
        std::fs::write(
            &path,
            "\u{FEFF}first_name,city\r\nAda,\"London, England\"\r\n".as_bytes(),
        )
        .unwrap();

        let spoken = extract(&path).expect("a CSV file should read");
        std::fs::remove_file(&path).ok();

        assert_eq!(
            spoken,
            "Table with 1 row and 2 columns.\n\
             \nRow 1. first name: Ada. city: London, England."
        );
    }

    /// Blank lines are formatting, and a trailing newline is not a row.
    #[test]
    fn blank_lines_are_not_counted_as_rows() {
        let rows = parse("a,b\n\n1,2\n\n\n", ',');
        assert_eq!(rows.len(), 2);
    }
}

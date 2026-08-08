//! Text extraction from `.docx`.
//!
//! A `.docx` is a zip archive whose main part, `word/document.xml`, holds the
//! body as WordprocessingML. We only need the readable text, so rather than
//! model the schema we stream the XML and keep the parts that a person would
//! actually hear: the contents of `<w:t>` runs, with paragraph and line breaks
//! turned into newlines and `<w:tab/>` into a tab.

use anyhow::{Context, Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::io::{BufReader, Read};
use std::path::Path;

pub fn extract(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file)).with_context(|| {
        format!(
            "{} is not a readable .docx file",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;

    let part = main_document_name(&mut archive)?;
    let mut xml = String::new();
    archive
        .by_name(&part)
        .with_context(|| format!("could not read {part} from the document"))?
        .read_to_string(&mut xml)
        .context("the document body was not valid UTF-8")?;

    Ok(super::tidy(&parse_body(&xml)?))
}

/// Finds the main document part. It is `word/document.xml` in every file Word
/// itself writes, but some generators use a different name under `word/`.
fn main_document_name<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Result<String> {
    const CONVENTIONAL: &str = "word/document.xml";
    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    if names.iter().any(|n| n == CONVENTIONAL) {
        return Ok(CONVENTIONAL.to_string());
    }
    names
        .into_iter()
        .find(|n| n.starts_with("word/document") && n.ends_with(".xml"))
        .ok_or_else(|| anyhow!("the file has no Word document body inside it"))
}

/// Elements whose children are formatting instructions rather than readable
/// text. Skipping them wholesale keeps stray tab stops and style names out.
fn is_properties_element(local: &[u8]) -> bool {
    matches!(
        local,
        b"pPr" | b"rPr" | b"sectPr" | b"tblPr" | b"tcPr" | b"trPr" | b"numPr"
    )
}

fn parse_body(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut out = String::new();
    let mut in_text_run = false;
    // Depth of nesting inside a properties element; non-zero means "ignore".
    let mut skipping = 0usize;

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let local = e.local_name();
                let local = local.as_ref();
                if skipping > 0 {
                    if is_properties_element(local) {
                        skipping += 1;
                    }
                    continue;
                }
                if is_properties_element(local) {
                    skipping = 1;
                } else if local == b"t" {
                    in_text_run = true;
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                let local = local.as_ref();
                if skipping > 0 {
                    if is_properties_element(local) {
                        skipping -= 1;
                    }
                    continue;
                }
                match local {
                    b"t" => in_text_run = false,
                    // A paragraph and a table row each end a line of speech.
                    b"p" | b"tr" => out.push('\n'),
                    // Cells within a row read better separated by a pause.
                    b"tc" => out.push('\t'),
                    _ => {}
                }
            }
            Event::Empty(e) if skipping == 0 => match e.local_name().as_ref() {
                b"br" | b"cr" => out.push('\n'),
                b"tab" => out.push('\t'),
                _ => {}
            },
            Event::Text(e) if in_text_run && skipping == 0 => {
                out.push_str(&e.xml10_content()?);
            }
            // The reader reports `&amp;`, `&#233;` and friends as their own
            // events rather than inlining them into the surrounding text.
            Event::GeneralRef(e) if in_text_run && skipping == 0 => {
                let name = e.xml10_content()?;
                out.push_str(&quick_xml::escape::unescape(&format!("&{name};"))?);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{extract, parse_body};

    /// Builds a minimal but structurally real .docx on disk.
    fn write_fixture(name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types/>"#).unwrap();
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn extracts_from_a_real_zip_container() {
        let path = write_fixture(
            "soe-docx-test.docx",
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:body>
              <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Quarterly Report</w:t></w:r></w:p>
              <w:p><w:r><w:t xml:space="preserve">Revenue rose 12% </w:t></w:r>
                   <w:r><w:t>year over year &amp; margins held.</w:t></w:r></w:p>
            </w:body></w:document>"#,
        );
        let text = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(
            text,
            "Quarterly Report\nRevenue rose 12% year over year & margins held."
        );
    }

    #[test]
    fn rejects_a_file_that_is_not_a_zip() {
        let path = std::env::temp_dir().join("soe-not-a-docx.docx");
        std::fs::write(&path, b"this is plain text, not a zip").unwrap();
        let error = extract(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("not a readable .docx"), "got: {error}");
    }

    #[test]
    fn joins_runs_and_breaks_paragraphs() {
        let xml = r#"<w:document><w:body>
            <w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second</w:t><w:br/><w:t>line</w:t></w:r></w:p>
        </w:body></w:document>"#;
        assert_eq!(parse_body(xml).unwrap(), "Hello world\nSecond\nline\n");
    }

    #[test]
    fn ignores_formatting_properties() {
        // The tab stop inside pPr must not become a tab in the output, and the
        // style name inside rPr must not be spoken.
        let xml = r#"<w:p>
            <w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr>
            <w:r><w:rPr><w:rStyle w:val="Strong"/></w:rPr><w:t>Only this</w:t></w:r>
        </w:p>"#;
        assert_eq!(parse_body(xml).unwrap(), "Only this\n");
    }

    #[test]
    fn preserves_significant_whitespace_and_entities() {
        let xml = r#"<w:p><w:r><w:t xml:space="preserve">a &amp; b </w:t></w:r>
            <w:r><w:t>c</w:t></w:r></w:p>"#;
        assert_eq!(parse_body(xml).unwrap(), "a & b c\n");
    }

    #[test]
    fn resolves_named_and_numeric_character_references() {
        let xml = r#"<w:p><w:r><w:t>caf&#233; &amp; cr&#xE8;me &lt;3</w:t></w:r></w:p>"#;
        assert_eq!(parse_body(xml).unwrap(), "café & crème <3\n");
    }
}

//! Text extraction from `.pptx`.
//!
//! A `.pptx` is a zip archive, like a `.docx`, but where a Word document has
//! one body part a presentation has one part per slide and no part that says
//! what is on any of them. Reading it aloud is therefore mostly a question of
//! *order* and *role* rather than of markup:
//!
//! * **Order** comes from `ppt/presentation.xml`, whose `<p:sldIdLst>` lists
//!   the slides as the deck shows them. Sorting the parts by filename is not
//!   the same thing — moving slide 12 to the front in PowerPoint reorders that
//!   list and leaves `slide12.xml` called `slide12.xml` — so the list is
//!   followed, with a numeric sort of the parts as the fallback for a file
//!   that has no list.
//! * **Role** comes from each shape's `<p:ph>` placeholder. A slide is a heap
//!   of floating text boxes with no reading order worth the name, and the one
//!   distinction that matters aloud is which of them is the title. It is said
//!   first, as part of the line announcing the slide, so a listener always
//!   knows where they are.
//!
//! Three kinds of shape are worth hearing and are otherwise easy to lose:
//!
//! * **Speaker notes.** Very often the actual sentences the slide's three
//!   bullet points were an aide-memoire for. They are a separate part linked
//!   from the slide, so nothing that reads only the slides will find them.
//! * **Picture alt text.** The one part of an image that was written for
//!   somebody who cannot see it. A picture that has none is still announced,
//!   since "there is an undescribed image here" is a fact about the deck and
//!   silence is indistinguishable from there being no picture at all.
//! * **Tables**, whose cells are ordinary paragraphs once the frame around
//!   them is ignored.
//!
//! Furniture is left out. The slide number, footer and date placeholders repeat
//! on every slide of most decks, and hearing the same footer forty times is
//! worse than not hearing it at all — the slide number in particular, since the
//! app announces a slide's number itself and the placeholder would say it
//! again, differently, in the middle of the text.
//!
//! Hidden slides are skipped, and do not take up a number. They were hidden
//! deliberately, and a deck read aloud should match the deck as presented.

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::io::{BufReader, Read, Seek};
use std::path::Path;

/// The most decompressed XML one part is allowed to be. A slide is a few
/// kilobytes in any real deck; this is far past the worst of them and far short
/// of what a zip bomb wants to hand over. See [`super::docx`], which guards the
/// same way for the same reason.
const MAX_PART_BYTES: u64 = 32 * 1024 * 1024;

/// The most decompressed XML the whole deck is allowed to be. A per-part limit
/// alone is no protection here: a presentation may hold any number of slides,
/// and a thousand parts each just under the part limit is the same attack
/// spread out.
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

pub fn extract(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file)).with_context(|| {
        format!(
            "{} is not a readable .pptx file",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;

    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    let mut budget = Budget::new(MAX_TOTAL_BYTES);

    let order = slide_order(&mut archive, &names, &mut budget)?;
    if order.is_empty() {
        bail!(
            "{} has no slides inside it",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    let mut slides = Vec::new();
    for part in &order {
        let Some(xml) = read_part(&mut archive, part, &mut budget)? else {
            continue;
        };
        let Some(mut slide) = parse_slide(&xml)? else {
            // Hidden, and hidden slides take up neither a number nor a breath.
            continue;
        };
        slide.notes = notes_for(&mut archive, part, &names, &mut budget)?;
        slides.push(slide);
    }

    if slides.is_empty() {
        bail!(
            "every slide in {} is hidden",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(super::tidy(&speak(&slides)))
}

/// How much decompressed XML is left before the deck as a whole is refused.
///
/// A running total rather than a per-part check, so that neither one enormous
/// slide nor ten thousand merely large ones can get past it.
struct Budget {
    left: u64,
}

impl Budget {
    fn new(total: u64) -> Self {
        Self { left: total }
    }

    /// The ceiling for the next part: whatever is left of the deck's budget,
    /// but never more than one part is allowed to be on its own.
    fn allowance(&self) -> u64 {
        self.left.min(MAX_PART_BYTES)
    }

    fn spend(&mut self, used: u64) {
        self.left = self.left.saturating_sub(used);
    }
}

/// Reads one XML part, or `None` when the archive does not have it.
///
/// Bounded by what comes *out* of the decompressor rather than by the size of
/// the entry on disk: a few hundred kilobytes of zip can expand to gigabytes of
/// XML, and an allocation failure is a worse answer than a refusal for someone
/// who depends on this app to read their post.
fn read_part<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<String>> {
    let allowance = budget.allowance();
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("could not read {name}")),
    };

    let mut xml = String::new();
    entry
        .take(allowance + 1)
        .read_to_string(&mut xml)
        .with_context(|| format!("{name} was not valid UTF-8"))?;
    if xml.len() as u64 > allowance {
        bail!(
            "the text inside this presentation expands to more than {} MB, which is more than \
             this app will read — it may be a damaged or deliberately malformed file",
            MAX_TOTAL_BYTES / (1024 * 1024)
        );
    }
    budget.spend(xml.len() as u64);
    Ok(Some(xml))
}

/// The slide parts, in the order the deck shows them.
///
/// `ppt/presentation.xml` names its slides by relationship id, and
/// `ppt/_rels/presentation.xml.rels` says which part each id points at, so the
/// order needs both. A deck missing either falls back to the slide parts sorted
/// numerically, which is the right order for every deck whose slides have never
/// been reordered and a reasonable guess for the rest.
fn slide_order<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    names: &[String],
    budget: &mut Budget,
) -> Result<Vec<String>> {
    let listed = (|| {
        let presentation = read_part(archive, "ppt/presentation.xml", budget).ok()??;
        let rels = read_part(archive, "ppt/_rels/presentation.xml.rels", budget).ok()??;
        let targets = relationships(&rels).ok()?;
        let ordered: Vec<String> = slide_ids(&presentation)
            .ok()?
            .into_iter()
            .filter_map(|id| targets.iter().find(|(rid, _, _)| *rid == id))
            .map(|(_, _, target)| resolve("ppt/", target))
            .filter(|part| names.iter().any(|name| name == part))
            .collect();
        (!ordered.is_empty()).then_some(ordered)
    })();

    Ok(listed.unwrap_or_else(|| numbered_slides(names)))
}

/// `ppt/slides/slideN.xml`, sorted by N rather than by name — otherwise
/// `slide10.xml` is read before `slide2.xml`, which is the classic way to
/// present a deck in the wrong order.
fn numbered_slides(names: &[String]) -> Vec<String> {
    let mut slides: Vec<(u32, String)> = names
        .iter()
        .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        .filter_map(|name| {
            let digits: String = name
                .trim_start_matches("ppt/slides/slide")
                .trim_end_matches(".xml")
                .to_string();
            digits.parse().ok().map(|n| (n, name.clone()))
        })
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    slides.into_iter().map(|(_, name)| name).collect()
}

/// The `r:id`s in `<p:sldIdLst>`, in order.
fn slide_ids(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut ids = Vec::new();
    let mut in_list = false;

    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == b"sldIdLst" => in_list = true,
            Event::End(e) if e.local_name().as_ref() == b"sldIdLst" => break,
            Event::Start(e) | Event::Empty(e) if in_list && e.local_name().as_ref() == b"sldId" => {
                // `id` is the deck's own numbering and `r:id` the relationship;
                // only the latter says which part this slide is.
                if let Some(rid) = relationship_id(&e) {
                    ids.push(rid);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(ids)
}

/// The `r:id` attribute, matched on the full qualified name rather than the
/// local one — `id` and `r:id` share a local name and mean different things.
fn relationship_id(element: &BytesStart) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.as_ref().ends_with(b":id") && a.key.local_name().as_ref() == b"id")
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// The `<Relationship>` entries of a `.rels` part, as `(Id, Type, Target)`.
fn relationships(xml: &str) -> Result<Vec<(String, String, String)>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut out = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"Relationship" => {
                let id = attribute(&e, "Id");
                let kind = attribute(&e, "Type");
                let target = attribute(&e, "Target");
                if let (Some(id), Some(kind), Some(target)) = (id, kind, target) {
                    out.push((id, kind, target));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Turns a relationship target into a name inside the archive.
///
/// Targets are written relative to the directory of the part that declares
/// them — a slide's notes are `../notesSlides/notesSlide1.xml` — and a leading
/// slash means from the root of the package instead. The `..` segments have to
/// be walked off rather than left in place, since nothing in the zip is called
/// `ppt/slides/../notesSlides/notesSlide1.xml`.
fn resolve(base_dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The speaker notes attached to a slide, if it has any worth saying.
fn notes_for<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    slide_part: &str,
    names: &[String],
    budget: &mut Budget,
) -> Result<Option<String>> {
    let (dir, file) = match slide_part.rsplit_once('/') {
        Some((dir, file)) => (format!("{dir}/"), file),
        None => (String::new(), slide_part),
    };
    let rels_part = format!("{dir}_rels/{file}.rels");
    if !names.iter().any(|name| name == &rels_part) {
        return Ok(None);
    }
    let Some(rels) = read_part(archive, &rels_part, budget)? else {
        return Ok(None);
    };
    let Some((_, _, target)) = relationships(&rels)?
        .into_iter()
        .find(|(_, kind, _)| kind.rsplit('/').next() == Some("notesSlide"))
    else {
        return Ok(None);
    };

    let part = resolve(&dir, &target);
    let Some(xml) = read_part(archive, &part, budget)? else {
        return Ok(None);
    };
    // A notes part is a slide in miniature and parses as one; only its body
    // text is wanted, since its own placeholders are the same repeated
    // furniture a slide's are.
    let notes = parse_slide(&xml)?
        .map(|slide| slide.body.join("\n"))
        .filter(|text: &String| !text.trim().is_empty());
    Ok(notes)
}

/// What one slide has to say, sorted into the roles that are said differently.
#[derive(Debug, Default, PartialEq, Eq)]
struct Slide {
    title: Option<String>,
    /// Every other paragraph, in the order the part lists them.
    body: Vec<String>,
    /// One entry per picture: its alt text, or `None` for a picture that has
    /// none.
    pictures: Vec<Option<String>>,
    notes: Option<String>,
}

/// The placeholder roles worth telling apart. Everything else on a slide is
/// ordinary body text whatever it calls itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Title,
    /// Slide number, footer, date, header: the same words on every slide of the
    /// deck, and never the point of any of them.
    Furniture,
    Body,
}

fn role_of(value: &str) -> Role {
    match value {
        "title" | "ctrTitle" => Role::Title,
        "sldNum" | "ftr" | "dt" | "hdr" => Role::Furniture,
        _ => Role::Body,
    }
}

/// An attribute by local name, so a `p:ph`'s `type` is found whatever prefix
/// the generator bound the namespace to.
fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name.as_bytes())
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Reads one slide part. `None` means the slide is hidden.
fn parse_slide(xml: &str) -> Result<Option<Slide>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut slide = Slide::default();
    // What the shape currently open is, and what it has said so far. A shape's
    // `<p:ph>` arrives before its text, but its `<p:cNvPr>` alt text may not,
    // so both are held until the shape closes.
    let mut role = Role::Body;
    let mut is_picture = false;
    let mut in_shape = false;
    let mut alt: Option<String> = None;
    let mut paragraphs: Vec<String> = Vec::new();
    let mut paragraph = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"sld" => {
                // `show="0"` is a slide the presenter took out of the deck
                // without deleting it.
                if matches!(attribute(&e, "show").as_deref(), Some("0") | Some("false")) {
                    return Ok(None);
                }
            }
            Event::Start(e) => match e.local_name().as_ref() {
                // Each of these begins a shape. Any shape still open is
                // finished off first: a group encloses its children, and its
                // own properties must not be handed to them.
                name @ (b"sp" | b"pic" | b"graphicFrame" | b"grpSp") => {
                    finish_shape(
                        &mut slide,
                        role,
                        is_picture,
                        in_shape,
                        &mut alt,
                        &mut paragraphs,
                    );
                    in_shape = name != b"grpSp";
                    is_picture = name == b"pic";
                    role = Role::Body;
                }
                b"cNvPr" if in_shape => alt = descr(&e),
                b"ph" if in_shape => {
                    role = attribute(&e, "type").map_or(Role::Body, |t| role_of(&t));
                }
                b"t" => in_text = true,
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                b"cNvPr" if in_shape => alt = descr(&e),
                b"ph" if in_shape => {
                    role = attribute(&e, "type").map_or(Role::Body, |t| role_of(&t));
                }
                // A soft line break inside a paragraph. A space rather than a
                // newline: it is a wrapping decision on a slide, not a pause.
                b"br" => push_space(&mut paragraph),
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    let text = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !text.is_empty() {
                        paragraphs.push(text);
                    }
                    paragraph.clear();
                }
                b"sp" | b"pic" | b"graphicFrame" | b"grpSp" => {
                    finish_shape(
                        &mut slide,
                        role,
                        is_picture,
                        in_shape,
                        &mut alt,
                        &mut paragraphs,
                    );
                    in_shape = false;
                    is_picture = false;
                    role = Role::Body;
                }
                _ => {}
            },
            Event::Text(e) if in_text => paragraph.push_str(&e.xml10_content()?),
            // The reader reports `&amp;` and friends as their own events rather
            // than inlining them into the surrounding text.
            Event::GeneralRef(e) if in_text => {
                let name = e.xml10_content()?;
                paragraph.push_str(&quick_xml::escape::unescape(&format!("&{name};"))?);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    finish_shape(
        &mut slide,
        role,
        is_picture,
        in_shape,
        &mut alt,
        &mut paragraphs,
    );
    Ok(Some(slide))
}

fn push_space(paragraph: &mut String) {
    if !paragraph.ends_with(' ') && !paragraph.is_empty() {
        paragraph.push(' ');
    }
}

/// A picture's alt text — the `descr` attribute, which is the field
/// PowerPoint's "Alt Text" pane writes to.
///
/// Deliberately not `name`, which sits beside it and is filled in
/// automatically: announcing "Picture 3" is worse than announcing nothing,
/// because it sounds like a description and is not one.
fn descr(element: &BytesStart) -> Option<String> {
    attribute(element, "descr")
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| !value.is_empty())
}

/// Files whatever the shape that has just closed collected into the slide.
fn finish_shape(
    slide: &mut Slide,
    role: Role,
    is_picture: bool,
    in_shape: bool,
    alt: &mut Option<String>,
    paragraphs: &mut Vec<String>,
) {
    let alt = alt.take();
    let paragraphs = std::mem::take(paragraphs);
    if !in_shape {
        return;
    }
    if is_picture {
        slide.pictures.push(alt);
    }
    match role {
        Role::Furniture => {}
        Role::Title => {
            let text = paragraphs.join(" ");
            if !text.trim().is_empty() {
                // A deck with two title placeholders on one slide is malformed
                // but real; the first wins and the rest become body text rather
                // than being dropped.
                match &slide.title {
                    None => slide.title = Some(text),
                    Some(_) => slide.body.push(text),
                }
            }
        }
        Role::Body => slide.body.extend(paragraphs),
    }
}

/// Turns the slides into something worth listening to: how big the deck is,
/// then each slide announced by number and title before anything on it.
fn speak(slides: &[Slide]) -> String {
    let mut out = format!("Presentation with {}.", counted(slides.len()));
    let total = slides.len();

    for (index, slide) in slides.iter().enumerate() {
        out.push_str("\n\n");
        // The number and the title in one breath, so a listener who has lost
        // their place gets both at once rather than a bare number.
        match &slide.title {
            Some(title) => out.push_str(&sentence(&format!(
                "Slide {} of {total}. {}",
                index + 1,
                title.trim()
            ))),
            None => out.push_str(&format!("Slide {} of {total}.", index + 1)),
        }

        for paragraph in &slide.body {
            out.push('\n');
            out.push_str(&sentence(paragraph));
        }

        for picture in &slide.pictures {
            out.push('\n');
            match picture {
                Some(alt) => out.push_str(&sentence(&format!("Image: {alt}"))),
                // Worth saying. A picture nobody described is a hole in the
                // slide, and silence here is indistinguishable from a slide
                // that simply has no picture on it.
                None => out.push_str("Image with no description."),
            }
        }

        if let Some(notes) = &slide.notes {
            out.push_str("\n\nSpeaker notes. ");
            out.push_str(&sentence(notes.trim()));
        }
    }
    out
}

/// A line of slide text with a full stop on the end, so the synthesiser stops
/// between one bullet point and the next instead of running them together.
/// Bullets are written without punctuation far more often than not.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.ends_with(['.', '!', '?', ':', ';', ',']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// "1 slide", "12 slides" — a count as it would be said aloud.
fn counted(n: usize) -> String {
    if n == 1 {
        "1 slide".to_string()
    } else {
        format!("{n} slides")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A slide's XML, wrapped in just enough of the real schema to be parsed
    /// the way PowerPoint's own output is.
    fn slide(shapes: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
            <p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>{shapes}</p:spTree></p:cSld></p:sld>"#
        )
    }

    /// A text shape: an optional placeholder type, then one paragraph per line.
    fn shape(placeholder: Option<&str>, lines: &[&str]) -> String {
        let ph = placeholder.map_or(String::new(), |t| format!(r#"<p:ph type="{t}"/>"#));
        let paragraphs: String = lines
            .iter()
            .map(|line| format!("<a:p><a:r><a:t>{line}</a:t></a:r></a:p>"))
            .collect();
        format!(
            "<p:sp><p:nvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:txBody>{paragraphs}</p:txBody></p:sp>"
        )
    }

    fn picture(descr: Option<&str>) -> String {
        let attr = descr.map_or(String::new(), |d| format!(r#" descr="{d}""#));
        format!(r#"<p:pic><p:nvPicPr><p:cNvPr id="4" name="Picture 3"{attr}/></p:nvPicPr></p:pic>"#)
    }

    /// Builds a structurally real .pptx on disk from named parts.
    fn write_fixture(name: &str, parts: &[(&str, String)]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types/>"#).unwrap();
        for (part, body) in parts {
            zip.start_file(*part, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn a_slide_is_announced_by_number_and_title_before_anything_on_it() {
        let parsed = parse_slide(&slide(&format!(
            "{}{}",
            shape(Some("title"), &["Quarterly Results"]),
            shape(None, &["Revenue up 12 percent", "Costs held flat"])
        )))
        .unwrap()
        .unwrap();

        assert_eq!(parsed.title.as_deref(), Some("Quarterly Results"));
        assert_eq!(parsed.body, ["Revenue up 12 percent", "Costs held flat"]);

        let spoken = speak(&[parsed]);
        assert_eq!(
            spoken,
            "Presentation with 1 slide.\n\n\
             Slide 1 of 1. Quarterly Results.\n\
             Revenue up 12 percent.\n\
             Costs held flat."
        );
    }

    /// A bullet point is written without punctuation far more often than not,
    /// and three of them run together are one baffling sentence.
    #[test]
    fn bullets_without_punctuation_still_get_a_full_stop() {
        assert_eq!(sentence("Revenue up"), "Revenue up.");
        // Punctuation the slide brought itself is left alone rather than
        // doubled.
        assert_eq!(sentence("Revenue up."), "Revenue up.");
        assert_eq!(sentence("Why now?"), "Why now?");
        assert_eq!(sentence("As follows:"), "As follows:");
    }

    /// The slide number, footer and date are the same words on every slide of
    /// a deck. Read out, they bury the slide they are on.
    #[test]
    fn the_repeated_furniture_of_a_deck_is_left_out() {
        let parsed = parse_slide(&slide(&format!(
            "{}{}{}{}",
            shape(Some("title"), &["Findings"]),
            shape(Some("sldNum"), &["7"]),
            shape(Some("ftr"), &["Commercial in confidence"]),
            shape(Some("dt"), &["22 August 2026"])
        )))
        .unwrap()
        .unwrap();

        assert_eq!(parsed.title.as_deref(), Some("Findings"));
        assert!(parsed.body.is_empty(), "{:?}", parsed.body);
    }

    /// Alt text is the one part of a picture written for somebody who cannot
    /// see it, and a picture without it is still a fact about the slide.
    #[test]
    fn a_pictures_alt_text_is_read_and_a_missing_one_is_admitted_to() {
        let parsed = parse_slide(&slide(&format!(
            "{}{}",
            picture(Some("A bar chart of quarterly revenue")),
            picture(None)
        )))
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed.pictures,
            [Some("A bar chart of quarterly revenue".to_string()), None]
        );

        let spoken = speak(&[parsed]);
        assert!(
            spoken.contains("Image: A bar chart of quarterly revenue."),
            "{spoken}"
        );
        assert!(spoken.contains("Image with no description."), "{spoken}");
        // The automatic name is not a description and must never stand in for
        // one.
        assert!(!spoken.contains("Picture 3"), "{spoken}");
    }

    /// A group's own properties belong to the group, not to whatever shape was
    /// being read before it — which is what happens if a group does not close
    /// the shape ahead of it.
    #[test]
    fn a_group_does_not_hand_its_own_alt_text_to_the_shape_before_it() {
        let parsed = parse_slide(&slide(&format!(
            "{}<p:grpSp><p:nvGrpSpPr><p:cNvPr id=\"9\" descr=\"a group\"/></p:nvGrpSpPr>{}</p:grpSp>",
            picture(Some("A photograph of the site")),
            shape(None, &["Inside the group"])
        )))
        .unwrap()
        .unwrap();

        assert_eq!(
            parsed.pictures,
            [Some("A photograph of the site".to_string())]
        );
        assert_eq!(parsed.body, ["Inside the group"]);
    }

    /// A hidden slide was taken out of the deck deliberately, and a deck read
    /// aloud should match the deck as presented.
    #[test]
    fn a_hidden_slide_is_skipped_and_does_not_take_up_a_number() {
        let hidden = format!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="p" show="0"><p:cSld><p:spTree>{}</p:spTree></p:cSld></p:sld>"#,
            shape(Some("title"), &["Draft, do not show"])
        );
        assert_eq!(parse_slide(&hidden).unwrap(), None);

        let path = write_fixture(
            "soe-pptx-hidden.pptx",
            &[
                (
                    "ppt/slides/slide1.xml",
                    slide(&shape(Some("title"), &["First"])),
                ),
                ("ppt/slides/slide2.xml", hidden),
                (
                    "ppt/slides/slide3.xml",
                    slide(&shape(Some("title"), &["Last"])),
                ),
            ],
        );
        let spoken = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(
            spoken.starts_with("Presentation with 2 slides."),
            "{spoken}"
        );
        assert!(spoken.contains("Slide 2 of 2. Last."), "{spoken}");
        assert!(!spoken.contains("do not show"), "{spoken}");
    }

    /// Sorting the parts by name puts slide 10 before slide 2, which is the
    /// classic way to present a deck in the wrong order.
    #[test]
    fn slides_fall_back_to_numeric_order_rather_than_alphabetical() {
        let names: Vec<String> = (1..=11)
            .map(|n| format!("ppt/slides/slide{n}.xml"))
            .collect();
        let ordered = numbered_slides(&names);
        assert_eq!(ordered.first().unwrap(), "ppt/slides/slide1.xml");
        assert_eq!(ordered[1], "ppt/slides/slide2.xml");
        assert_eq!(ordered.last().unwrap(), "ppt/slides/slide11.xml");
    }

    /// Reordering a deck in PowerPoint rewrites the list and leaves the parts
    /// named as they were, so the list has to win.
    #[test]
    fn the_decks_own_running_order_beats_the_part_names() {
        let path = write_fixture(
            "soe-pptx-order.pptx",
            &[
                (
                    "ppt/presentation.xml",
                    r#"<?xml version="1.0"?><p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>
                       <p:sldId id="258" r:id="rId9"/><p:sldId id="256" r:id="rId7"/>
                       </p:sldIdLst></p:presentation>"#
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    r#"<?xml version="1.0"?><Relationships>
                       <Relationship Id="rId7" Type="http://x/slide" Target="slides/slide1.xml"/>
                       <Relationship Id="rId9" Type="http://x/slide" Target="slides/slide2.xml"/>
                       </Relationships>"#
                        .to_string(),
                ),
                (
                    "ppt/slides/slide1.xml",
                    slide(&shape(Some("title"), &["Written first"])),
                ),
                (
                    "ppt/slides/slide2.xml",
                    slide(&shape(Some("title"), &["Shown first"])),
                ),
            ],
        );
        let spoken = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(spoken.contains("Slide 1 of 2. Shown first."), "{spoken}");
        assert!(spoken.contains("Slide 2 of 2. Written first."), "{spoken}");
    }

    /// Speaker notes are very often the sentences the bullet points were an
    /// aide-memoire for, and nothing that reads only the slides will find them.
    #[test]
    fn speaker_notes_are_found_through_the_slides_relationships_and_read() {
        let path = write_fixture(
            "soe-pptx-notes.pptx",
            &[
                (
                    "ppt/slides/slide1.xml",
                    slide(&shape(Some("title"), &["Findings"])),
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    r#"<?xml version="1.0"?><Relationships>
                       <Relationship Id="rId2" Type="http://x/notesSlide"
                                     Target="../notesSlides/notesSlide1.xml"/>
                       </Relationships>"#
                        .to_string(),
                ),
                (
                    "ppt/notesSlides/notesSlide1.xml",
                    slide(&format!(
                        "{}{}",
                        shape(Some("sldNum"), &["1"]),
                        shape(None, &["Mention the Bristol contract here"])
                    )),
                ),
            ],
        );
        let spoken = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(
            spoken.contains("Speaker notes. Mention the Bristol contract here."),
            "{spoken}"
        );
        // The notes part's own slide-number placeholder is furniture too.
        assert!(!spoken.contains("Speaker notes. 1"), "{spoken}");
    }

    /// Targets are relative to the part that declares them, and nothing in the
    /// zip is called `ppt/slides/../notesSlides/notesSlide1.xml`.
    #[test]
    fn relationship_targets_resolve_to_names_that_are_actually_in_the_archive() {
        assert_eq!(
            resolve("ppt/slides/", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(
            resolve("ppt/", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        // A leading slash means from the root of the package instead.
        assert_eq!(
            resolve("ppt/slides/", "/ppt/media/image1.png"),
            "ppt/media/image1.png"
        );
    }

    /// `id` and `r:id` sit side by side on a `<p:sldId>` and mean entirely
    /// different things; reading the wrong one orders the deck by nothing.
    #[test]
    fn the_relationship_id_is_read_rather_than_the_decks_own_numbering() {
        let ids = slide_ids(
            r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>
               <p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId8"/>
               </p:sldIdLst></p:presentation>"#,
        )
        .unwrap();
        assert_eq!(ids, ["rId7", "rId8"]);
    }

    /// Character references arrive as their own events rather than inlined,
    /// and a slide saying `&amp;` should be heard as "and", not as markup.
    #[test]
    fn character_references_come_through_as_the_characters_they_stand_for() {
        let parsed = parse_slide(&slide(&shape(None, &["Marks &amp; Spencer, caf&#233;"])))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.body, ["Marks & Spencer, café"]);
    }

    /// The whole path off disk, on the simplest deck that is still a deck.
    #[test]
    fn a_pptx_file_is_read_from_disk_as_a_presentation() {
        let path = write_fixture(
            "soe-pptx-basic.pptx",
            &[(
                "ppt/slides/slide1.xml",
                slide(&format!(
                    "{}{}",
                    shape(Some("ctrTitle"), &["Annual Review"]),
                    shape(Some("subTitle"), &["Prepared for the board"])
                )),
            )],
        );
        let spoken = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(
            spoken,
            "Presentation with 1 slide.\n\n\
             Slide 1 of 1. Annual Review.\n\
             Prepared for the board."
        );
    }

    /// A slide in the shape PowerPoint actually writes one, rather than the
    /// trimmed-down shapes above.
    ///
    /// Worth its own test because every difference here is a way the reader
    /// could quietly return nothing on a real deck while passing every other
    /// test in this file: namespaces bound to their real URIs rather than to
    /// one-letter stand-ins, run properties sitting inside the run alongside
    /// the text, a table whose cells are paragraphs several elements down, and
    /// an `mc:AlternateContent` block wrapping a shape the way Office writes
    /// one for a feature older versions cannot render.
    #[test]
    fn a_slide_shaped_the_way_powerpoint_writes_one_reads_in_full() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
    <p:grpSpPr><a:xfrm><a:off x="0" y="0"/></a:xfrm></p:grpSpPr>
    <p:sp>
      <p:nvSpPr>
        <p:cNvPr id="2" name="Title 1"/>
        <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
        <p:nvPr><p:ph type="ctrTitle"/></p:nvPr>
      </p:nvSpPr>
      <p:spPr/>
      <p:txBody>
        <a:bodyPr/><a:lstStyle/>
        <a:p><a:r><a:rPr lang="en-GB" dirty="0"/><a:t>Second-quarter results</a:t></a:r>
             <a:endParaRPr lang="en-GB" dirty="0"/></a:p>
      </p:txBody>
    </p:sp>
    <p:sp>
      <p:nvSpPr>
        <p:cNvPr id="3" name="Content Placeholder 2"/>
        <p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
        <p:nvPr><p:ph idx="1"/></p:nvPr>
      </p:nvSpPr>
      <p:txBody>
        <a:bodyPr/><a:lstStyle/>
        <a:p><a:pPr lvl="0"/><a:r><a:rPr lang="en-GB"/><a:t>Revenue up 12 percent</a:t></a:r></a:p>
        <a:p><a:pPr lvl="1"/><a:r><a:rPr lang="en-GB"/><a:t>Driven by the Bristol contract</a:t></a:r></a:p>
      </p:txBody>
    </p:sp>
    <mc:AlternateContent>
      <mc:Choice Requires="a14">
        <p:sp>
          <p:nvSpPr><p:cNvPr id="7" name="Note"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
          <p:txBody><a:bodyPr/><a:p><a:r><a:t>Figures are unaudited</a:t></a:r></a:p></p:txBody>
        </p:sp>
      </mc:Choice>
    </mc:AlternateContent>
    <p:graphicFrame>
      <p:nvGraphicFramePr><p:cNvPr id="5" name="Table 4"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>
      <a:graphic><a:graphicData><a:tbl>
        <a:tr h="370840">
          <a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Region</a:t></a:r></a:p></a:txBody></a:tc>
          <a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Change</a:t></a:r></a:p></a:txBody></a:tc>
        </a:tr>
        <a:tr h="370840">
          <a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Midlands</a:t></a:r></a:p></a:txBody></a:tc>
          <a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>up 4 percent</a:t></a:r></a:p></a:txBody></a:tc>
        </a:tr>
      </a:tbl></a:graphicData></a:graphic>
    </p:graphicFrame>
    <p:pic>
      <p:nvPicPr>
        <p:cNvPr id="6" name="Picture 5" descr="A bar chart of quarterly revenue"/>
        <p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/>
      </p:nvPicPr>
      <p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
    </p:pic>
    <p:sp>
      <p:nvSpPr><p:cNvPr id="8" name="Slide Number Placeholder 7"/><p:cNvSpPr/>
        <p:nvPr><p:ph type="sldNum" sz="quarter" idx="12"/></p:nvPr></p:nvSpPr>
      <p:txBody><a:bodyPr/><a:p><a:fld id="{X}" type="slidenum"><a:t>4</a:t></a:fld></a:p></p:txBody>
    </p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;

        let slide = parse_slide(xml).unwrap().expect("a visible slide");
        assert_eq!(slide.title.as_deref(), Some("Second-quarter results"));
        // The bullets, the shape Office wrapped for compatibility, and every
        // cell of the table — in the order the slide lists them.
        assert_eq!(
            slide.body,
            [
                "Revenue up 12 percent",
                "Driven by the Bristol contract",
                "Figures are unaudited",
                "Region",
                "Change",
                "Midlands",
                "up 4 percent",
            ]
        );
        assert_eq!(
            slide.pictures,
            [Some("A bar chart of quarterly revenue".to_string())]
        );

        // The slide-number placeholder is furniture, and the app announces the
        // number itself — hearing "4" in the middle of the text would be the
        // deck's own numbering arriving twice, differently.
        let spoken = speak(&[slide]);
        assert!(
            spoken.contains("Slide 1 of 1. Second-quarter results."),
            "{spoken}"
        );
        // The placeholder holds a `<a:fld>` whose text is the number 4. The app
        // announces a slide's number itself, so letting that through would say
        // the deck's own numbering a second time, differently, mid-slide.
        assert!(!spoken.contains("\n4."), "{spoken}");
    }

    /// Reads a real presentation off disk and prints what the app would say
    /// about it.
    ///
    /// Ignored and driven by an environment variable, for the same reason the
    /// video pipeline test is: it needs a file this repository does not carry
    /// and cannot generate. Everything else in this module is a fixture
    /// written to exercise a rule, which means every one of them is shaped by
    /// the same understanding of the format that the reader is — so they
    /// cannot catch a convention nobody thought to fake.
    ///
    /// What counts as a real file here is one written by a real producer:
    /// PowerPoint, Keynote's export, Google Slides' download, LibreOffice, or
    /// python-pptx. Hand-assembled XML in a zip is another fixture wearing a
    /// file extension.
    ///
    ///     SOE_SAMPLE_PPTX=~/deck.pptx cargo test real_presentation -- --ignored --nocapture
    #[test]
    #[ignore = "needs a real .pptx; set SOE_SAMPLE_PPTX to one"]
    fn a_real_presentation_reads_end_to_end() {
        let path = std::env::var("SOE_SAMPLE_PPTX")
            .expect("set SOE_SAMPLE_PPTX to the path of a real .pptx");
        let spoken = extract(std::path::Path::new(&path)).expect("the deck should read");

        eprintln!("\n=== {path} ===\n{spoken}\n");

        assert!(
            spoken.starts_with("Presentation with "),
            "no opening count: {spoken}"
        );
        assert!(spoken.contains("Slide 1 of "), "no first slide: {spoken}");
        // A deck nobody can hear anything from is the failure this is looking
        // for: the parts were found, but no text came out of them.
        let words = spoken.split_whitespace().count();
        assert!(
            words > 20,
            "only {words} words came out of the deck: {spoken}"
        );
    }

    /// A zip that is not a presentation, and a presentation with nothing in
    /// it, both have to fail as messages rather than as panics.
    #[test]
    fn a_file_with_no_slides_in_it_is_refused_by_name() {
        let path = write_fixture("soe-pptx-empty.pptx", &[]);
        let error = extract(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("no slides"), "{error}");
        assert!(error.contains("soe-pptx-empty.pptx"), "{error}");
    }
}

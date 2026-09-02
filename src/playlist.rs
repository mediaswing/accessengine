//! Playlists: a zip holding a `media.xml` and the audio it names.
//!
//! The format is somebody else's and deliberately small:
//!
//! ```xml
//! <media version="1.2">
//!     <content pos="1" type="M">bed.mp3</content>
//!     <content pos="2" type="B">bulletin.mp3</content>
//! </media>
//! ```
//!
//! `pos` is the running order — read from the attribute rather than from the
//! order the elements happen to appear in, because the two are not promised to
//! agree and the attribute is the one that says what it means. `type` is `M`
//! for music and `B` for spoken word, which the player uses to decide where a
//! fade belongs; see [`Kind`].
//!
//! Read with the same small tag scanner `powerpoint` uses on a `.pptx`, and
//! for the same reason: this is four elements deep at most, an XML crate would
//! be a dependency earning its keep on one file, and the entity decoder is
//! already written. Every limit here exists because the file arrives from
//! outside — see the `MAX_*` constants.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::powerpoint::decode_entities;
use crate::t;

/// A zip begins `PK\x03\x04`. Checked rather than trusting the extension, the
/// way `powerpoint` decides what a `.ppt` really is.
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// The manifest is a handful of lines; anything of this size is not one.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// A running order longer than this is not a running order.
const MAX_ITEMS: usize = 10_000;
/// One track, read into memory to be decoded. Generous for an hour of MP3 and
/// still a ceiling, because a zip entry's declared size is a claim.
pub const MAX_TRACK_BYTES: u64 = 256 * 1024 * 1024;

/// The audio this app can decode.
///
/// Two formats, because those are the two `rodio` features this app builds
/// with: MP3 is what every cloud voice is asked for and so what the app itself
/// writes, and WAV is what the interface cues are. Anything else is refused by
/// name in the file dialog rather than opened and then failing to decode —
/// adding decoders for formats this app never produces would be a media
/// library's worth of dependency for a convenience.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav"];

/// What a track is, which decides how it is joined to its neighbour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `type="M"`.
    Music,
    /// `type="B"`.
    Spoken,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "M" => Some(Self::Music),
            "B" => Some(Self::Spoken),
            _ => None,
        }
    }

    /// What the track list calls it. In words, not a colour or an icon: this
    /// is the difference between two rows that otherwise look identical.
    pub fn label(self) -> String {
        match self {
            Self::Music => t!("player.kind_music"),
            Self::Spoken => t!("player.kind_spoken"),
        }
    }
}

/// Where a track's bytes actually are.
///
/// A playlist's tracks live inside the zip; a file opened on its own is just
/// itself. Both reach the player as one type so it never has to care which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    File(PathBuf),
    ZipEntry { archive: PathBuf, entry: String },
}

impl Origin {
    /// Read the audio, capped, because the size a zip entry declares is a
    /// claim and reading to the end of a lie is the whole attack.
    pub fn read(&self) -> Result<Vec<u8>> {
        match self {
            Self::File(path) => {
                let length = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                if length > MAX_TRACK_BYTES {
                    bail!(t!("error.track_too_large"));
                }
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))
            }
            Self::ZipEntry { archive, entry } => {
                let file = std::fs::File::open(archive)
                    .with_context(|| format!("reading {}", archive.display()))?;
                let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
                    .with_context(|| t!("error.unreadable_zip"))?;
                let mut inside = zip
                    .by_name(entry)
                    .with_context(|| format!("reading {entry}"))?;
                let mut bytes = Vec::new();
                inside
                    .by_ref()
                    .take(MAX_TRACK_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("reading {entry}"))?;
                if bytes.len() as u64 > MAX_TRACK_BYTES {
                    bail!(t!("error.track_too_large"));
                }
                Ok(bytes)
            }
        }
    }

    /// What to show in the track list.
    pub fn name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            Self::ZipEntry { entry, .. } => entry
                .rsplit('/')
                .next()
                .unwrap_or(entry.as_str())
                .to_string(),
        }
    }
}

/// One entry of a running order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub kind: Kind,
    pub origin: Origin,
}

impl Track {
    pub fn name(&self) -> String {
        self.origin.name()
    }
}

/// Whether this file is a zip with a running order in it.
///
/// Opened and looked inside rather than judged by its extension: "a zip
/// containing a `media.xml`" is what the format is, and a `.zip` of holiday
/// photographs is not one.
pub fn is_playlist(path: &Path) -> bool {
    manifest_of(path).is_some()
}

/// The manifest inside a zip, if it has one.
fn manifest_of(path: &Path) -> Option<String> {
    let mut header = [0u8; 4];
    {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path).ok()?;
        file.read_exact(&mut header).ok()?;
    }
    if header != ZIP_MAGIC {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let name = manifest_name(&zip)?;
    let mut entry = zip.by_name(&name).ok()?;
    let mut xml = String::new();
    entry
        .by_ref()
        .take(MAX_MANIFEST_BYTES)
        .read_to_string(&mut xml)
        .ok()?;
    Some(xml)
}

/// The manifest may sit at the root or inside a folder, because a zip made by
/// dragging a folder in has everything one level down.
fn manifest_name(zip: &zip::ZipArchive<impl std::io::Read + std::io::Seek>) -> Option<String> {
    let mut found: Option<String> = None;
    for index in 0..zip.len() {
        let Some(name) = zip.name_for_index(index) else {
            continue;
        };
        let leaf = name.rsplit('/').next().unwrap_or(name);
        if leaf.eq_ignore_ascii_case("media.xml") {
            // The shallowest one wins, so a stray copy in a subfolder cannot
            // outrank the real manifest at the root.
            let depth = name.matches('/').count();
            let better = found
                .as_ref()
                .is_none_or(|existing| depth < existing.matches('/').count());
            if better {
                found = Some(name.to_string());
            }
        }
    }
    found
}

/// Read a zip's running order.
pub fn from_zip(path: &Path) -> Result<Vec<Track>> {
    let Some(xml) = manifest_of(path) else {
        bail!(t!("error.not_a_playlist"));
    };
    let entries = parse_manifest(&xml);
    if entries.is_empty() {
        bail!(t!("error.empty_playlist"));
    }

    // Every name in the archive, to match the manifest's names against.
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| t!("error.unreadable_zip"))?;
    let names: Vec<String> = (0..zip.len())
        .filter_map(|index| zip.name_for_index(index).map(str::to_string))
        .collect();

    let mut tracks = Vec::new();
    let mut missing = Vec::new();
    for entry in entries {
        match resolve(&entry.file, &names) {
            Some(name) => tracks.push(Track {
                kind: entry.kind,
                origin: Origin::ZipEntry {
                    archive: path.to_path_buf(),
                    entry: name,
                },
            }),
            // Named in the running order but not in the zip. Collected rather
            // than fatal: a bulletin missing its closing music is still worth
            // hearing, and the ones that are there should still play.
            None => missing.push(entry.file),
        }
    }

    if tracks.is_empty() {
        bail!(t!(
            "error.playlist_files_missing",
            files = missing.join(", ")
        ));
    }
    if !missing.is_empty() {
        log::warn!(
            "{} named in {} but not in it: {}",
            missing.len(),
            path.display(),
            missing.join(", ")
        );
    }
    Ok(tracks)
}

/// Match a name from the manifest to a name in the archive.
///
/// Exactly first, then by the last path segment, then ignoring case. A
/// manifest is written by hand or by another program, and `Bed.mp3` against
/// `audio/bed.mp3` is a difference nobody means.
fn resolve(wanted: &str, names: &[String]) -> Option<String> {
    let wanted = wanted.trim().trim_start_matches("./");
    if wanted.is_empty() {
        return None;
    }
    let leaf_of = |name: &str| name.rsplit('/').next().unwrap_or(name).to_string();
    let wanted_leaf = leaf_of(wanted);

    names
        .iter()
        .find(|name| name.as_str() == wanted)
        .or_else(|| names.iter().find(|name| leaf_of(name) == wanted_leaf))
        .or_else(|| {
            names
                .iter()
                .find(|name| leaf_of(name).eq_ignore_ascii_case(&wanted_leaf))
        })
        .cloned()
}

/// One `<content>` element, before its file has been found.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    pos: u32,
    kind: Kind,
    file: String,
}

/// The running order out of a `media.xml`.
///
/// Sorted by `pos`, which is the point of the attribute: the elements are not
/// promised to be written in order, and this is the field that says what the
/// order is. A stable sort, so two items claiming the same position keep the
/// order the file wrote them in rather than swapping about between runs.
fn parse_manifest(xml: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut rest = xml;

    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];

        let name = tag
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '\r', '/'])
            .next()
            .unwrap_or("");
        if name != "content" || tag.starts_with('/') {
            continue;
        }

        // The file is the element's text, up to the closing tag.
        let Some(end) = rest.find('<') else { break };
        let file = decode_entities(rest[..end].trim());
        rest = &rest[end..];

        // A row with no type, no position or no file cannot be played, and
        // guessing at any of the three would be worse than leaving it out.
        let (Some(kind), Some(pos)) = (
            attribute(tag, "type").and_then(|v| Kind::parse(&v)),
            attribute(tag, "pos").and_then(|v| v.trim().parse::<u32>().ok()),
        ) else {
            log::warn!("a <content> element in the playlist has no usable pos and type");
            continue;
        };
        if file.is_empty() {
            log::warn!("<content pos=\"{pos}\"> names no file");
            continue;
        }

        entries.push(Entry { pos, kind, file });
        if entries.len() >= MAX_ITEMS {
            log::warn!("playlist truncated at {MAX_ITEMS} items");
            break;
        }
    }

    entries.sort_by_key(|entry| entry.pos);
    entries
}

/// One attribute out of a start tag, quoted either way.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        rest = &rest[at + name.len()..];
        // `type` must not match the `type` inside `contenttype`.
        if before.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ':') {
            continue;
        }
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let after = &after[quote.len_utf8()..];
        let end = after.find(quote)?;
        return Some(decode_entities(&after[..end]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example file, with the placeholder names it ships with.
    const EXAMPLE: &str = r#"<media version="1.2">
    <content pos="1" type="M">MUSIC_FILE_HERE</content>
    <content pos="2" type="B">SPOKEN_WORD_FILE</content>
    <content pos="3" type="M">MUSIC_FILE_HERE</content>
</media>"#;

    #[test]
    fn the_example_manifest_reads_as_three_tracks_in_order() {
        let entries = parse_manifest(EXAMPLE);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].pos, 1);
        assert_eq!(entries[0].kind, Kind::Music);
        assert_eq!(entries[0].file, "MUSIC_FILE_HERE");
        assert_eq!(entries[1].kind, Kind::Spoken);
        assert_eq!(entries[1].file, "SPOKEN_WORD_FILE");
        assert_eq!(entries[2].pos, 3);
    }

    /// `pos` is the running order, not the order the elements are written in.
    /// That is the whole reason the attribute exists.
    #[test]
    fn the_running_order_comes_from_pos_not_from_the_file_order() {
        let xml = r#"<media version="1.2">
            <content pos="3" type="M">last.mp3</content>
            <content pos="1" type="B">first.mp3</content>
            <content pos="2" type="M">middle.mp3</content>
        </media>"#;
        let files: Vec<String> = parse_manifest(xml).into_iter().map(|e| e.file).collect();
        assert_eq!(files, ["first.mp3", "middle.mp3", "last.mp3"]);
    }

    #[test]
    fn both_types_are_read_and_anything_else_is_left_out() {
        let xml = r#"<media version="1.2">
            <content pos="1" type="M">m.mp3</content>
            <content pos="2" type="b">lower.mp3</content>
            <content pos="3" type="X">unknown.mp3</content>
            <content pos="4">no-type.mp3</content>
            <content type="B">no-pos.mp3</content>
            <content pos="5" type="B"></content>
        </media>"#;
        let entries = parse_manifest(xml);
        // The lower-case `b` is the same type; the other four cannot be played.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, Kind::Music);
        assert_eq!(entries[1].kind, Kind::Spoken);
        assert_eq!(entries[1].file, "lower.mp3");
    }

    #[test]
    fn a_file_name_with_markup_characters_in_it_survives() {
        let xml = r#"<media version="1.2">
            <content pos="1" type="B">Bob &amp; Alice&apos;s bulletin.mp3</content>
        </media>"#;
        let entries = parse_manifest(xml);
        assert_eq!(entries[0].file, "Bob & Alice's bulletin.mp3");
    }

    #[test]
    fn attributes_are_read_in_either_quote_and_not_from_a_longer_name() {
        assert_eq!(
            attribute(r#"content pos="2" type='B'"#, "pos").as_deref(),
            Some("2")
        );
        assert_eq!(
            attribute(r#"content pos="2" type='B'"#, "type").as_deref(),
            Some("B")
        );
        // `type` must not be found inside `contenttype`.
        assert_eq!(
            attribute(r#"content contenttype="X" type="M""#, "type").as_deref(),
            Some("M")
        );
        assert_eq!(attribute("content pos=\"1\"", "type"), None);
        assert_eq!(attribute("content type=unquoted", "type"), None);
    }

    /// Nothing usable in it is not a playlist, and it must not be reported as
    /// an empty one that plays silence.
    #[test]
    fn a_manifest_with_nothing_playable_is_no_tracks() {
        assert!(parse_manifest("<media version=\"1.2\"></media>").is_empty());
        assert!(parse_manifest("not xml at all").is_empty());
        assert!(parse_manifest("").is_empty());
        // An unclosed element must not spin.
        assert!(parse_manifest("<content pos=\"1\" type=\"B\">").is_empty());
    }

    #[test]
    fn a_name_is_matched_past_a_folder_and_past_its_case() {
        let names = vec![
            "bulletin/audio/Bed.mp3".to_string(),
            "bulletin/media.xml".to_string(),
            "bulletin/audio/story.mp3".to_string(),
        ];
        // Exactly.
        assert_eq!(
            resolve("bulletin/audio/story.mp3", &names).as_deref(),
            Some("bulletin/audio/story.mp3")
        );
        // By leaf, because the manifest need not repeat the folder.
        assert_eq!(
            resolve("story.mp3", &names).as_deref(),
            Some("bulletin/audio/story.mp3")
        );
        // And past a difference in case nobody meant.
        assert_eq!(
            resolve("bed.mp3", &names).as_deref(),
            Some("bulletin/audio/Bed.mp3")
        );
        assert_eq!(resolve("absent.mp3", &names), None);
        assert_eq!(resolve("   ", &names), None);
    }

    /// A zip built here and read back, which is the only way to know the
    /// manifest, the archive and the name matching agree with each other.
    fn build_zip(name: &str, manifest: &str, files: &[(&str, &[u8])]) -> PathBuf {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!(
            "accessengine-playlist-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bulletin.zip");

        let file = std::fs::File::create(&path).expect("creates");
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("bulletin/media.xml", options)
            .expect("entry");
        zip.write_all(manifest.as_bytes()).expect("writes");
        for (entry, bytes) in files {
            zip.start_file(*entry, options).expect("entry");
            zip.write_all(bytes).expect("writes");
        }
        zip.finish().expect("finishes");
        path
    }

    #[test]
    fn a_zip_with_a_running_order_reads_back_as_tracks_in_order() {
        let manifest = r#"<media version="1.2">
            <content pos="2" type="B">story.mp3</content>
            <content pos="1" type="M">bed.mp3</content>
        </media>"#;
        let path = build_zip(
            "round-trip",
            manifest,
            &[
                ("bulletin/audio/bed.mp3", b"not really mp3"),
                ("bulletin/audio/story.mp3", b"nor is this"),
            ],
        );

        assert!(is_playlist(&path), "a zip with a media.xml is a playlist");
        let tracks = from_zip(&path).expect("reads");
        assert_eq!(tracks.len(), 2);
        // `pos` decides, not the order in the file.
        assert_eq!(tracks[0].kind, Kind::Music);
        assert_eq!(tracks[0].name(), "bed.mp3");
        assert_eq!(tracks[1].kind, Kind::Spoken);
        assert_eq!(tracks[1].name(), "story.mp3");
        // And the bytes come back out of the archive.
        assert_eq!(tracks[1].origin.read().expect("reads"), b"nor is this");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A running order naming files that are not there should still play the
    /// ones that are, rather than refusing the whole bulletin.
    #[test]
    fn a_missing_file_costs_its_own_track_and_no_others() {
        let manifest = r#"<media version="1.2">
            <content pos="1" type="B">present.mp3</content>
            <content pos="2" type="M">absent.mp3</content>
        </media>"#;
        let path = build_zip("partial", manifest, &[("bulletin/present.mp3", b"here")]);

        let tracks = from_zip(&path).expect("reads");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].name(), "present.mp3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// An ordinary zip is not a playlist, and must not be opened as one.
    #[test]
    fn a_zip_with_no_manifest_is_not_a_playlist() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!(
            "accessengine-playlist-{}-plain",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("holiday.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("creates"));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("beach.jpg", options).expect("entry");
        zip.write_all(b"not a manifest").expect("writes");
        zip.finish().expect("finishes");

        assert!(!is_playlist(&path));
        assert!(from_zip(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And neither is something that is not a zip at all, which must be
    /// answered without reading the whole file.
    #[test]
    fn a_file_that_is_not_a_zip_is_not_a_playlist() {
        let dir =
            std::env::temp_dir().join(format!("accessengine-playlist-{}-text", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "just some words").expect("writes");
        assert!(!is_playlist(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_track_is_named_by_its_file_rather_than_its_whole_path() {
        let track = Track {
            kind: Kind::Spoken,
            origin: Origin::ZipEntry {
                archive: PathBuf::from("/tmp/bulletin.zip"),
                entry: "bulletin/audio/story.mp3".to_string(),
            },
        };
        assert_eq!(track.name(), "story.mp3");
        let loose = Track {
            kind: Kind::Spoken,
            origin: Origin::File(PathBuf::from("/tmp/reading.mp3")),
        };
        assert_eq!(loose.name(), "reading.mp3");
    }
}

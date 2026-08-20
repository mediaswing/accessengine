//! Playlists: a `.zip` of audio files, played one after another.
//!
//! The archive is the playlist. Every WAV or MP3 inside it is a track, and if
//! the archive also carries a `media.txt` then that file decides the running
//! order and says which tracks are speech and which are music — see
//! [`parse_manifest`]. Nothing is ever unpacked to disk: the archive is held
//! open and each track is decompressed into memory as its turn comes, so a
//! playlist leaves no temporary files behind for someone to find later, and no
//! entry name from the archive is ever used as a path to write to.
//!
//! # What is refused
//!
//! A zip says how big its contents are, and a zip can lie. Everything read
//! here is bounded twice over — by the size the archive claims and by the
//! bytes that actually come out of the decompressor — because the app being
//! taken down by an allocation failure is a worse outcome, for someone who
//! depends on it to hear their post, than being told the file was refused.
//! Entries whose names do not stay inside the archive, AppleDouble and
//! `__MACOSX` leftovers, and anything that is not a playable audio file are
//! all left out of the running order rather than failing the whole playlist.

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// The extension that opens a playlist rather than a single recording.
pub const PLAYLIST_EXTENSIONS: &[&str] = &["zip"];

/// The file inside the archive that sets the running order, if it is there.
const MANIFEST: &str = "media.txt";

/// The most tracks one playlist will hold. Past this it is not a playlist
/// someone assembled, and the list of it stops being something a person can
/// hold in their head or a screen reader can usefully walk.
const MAX_TRACKS: usize = 500;

/// The largest a single track may be once decompressed. An hour of MP3 is
/// about 60 MB, so this is several albums' worth of headroom and still far
/// short of what a zip bomb wants to hand over.
const MAX_TRACK_BYTES: u64 = 256 * 1024 * 1024;

/// The largest `media.txt` may be. A running order for [`MAX_TRACKS`] tracks
/// is a few tens of kilobytes.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// What a track is, which is the whole of what `media.txt` adds beyond order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackKind {
    /// Someone talking. The default: a playlist with no manifest is a stack of
    /// recordings, and recordings do not play over one another.
    #[default]
    Speech,
    /// Music, which is allowed to start underneath the speech before it — see
    /// [`crate::audio::OVERLAP`].
    Music,
}

impl TrackKind {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "music" => Some(Self::Music),
            "speech" | "voice" | "spoken" => Some(Self::Speech),
            _ => None,
        }
    }
}

/// One entry of the running order.
pub struct Track {
    /// The last component of the entry's name, which is what a listener is
    /// told and all that is ever shown. The full path inside the archive is
    /// deliberately not kept: it has no meaning outside the zip and is exactly
    /// the sort of string that should never reach a file system call.
    pub name: String,
    pub kind: TrackKind,
    /// Where the entry sits in the archive's own directory.
    index: usize,
}

/// A zip of audio files, open and ready to be played through.
pub struct Playlist {
    archive: zip::ZipArchive<BufReader<File>>,
    tracks: Vec<Track>,
    /// The playlist's own name — the zip's file name — for the status line.
    pub name: String,
    /// True when a `media.txt` inside the archive set the running order.
    pub ordered: bool,
    /// Names the manifest asked for that the archive does not hold, and
    /// playable files the manifest never mentioned. Neither is fatal; both are
    /// worth a line in the log, because a track that silently never plays is
    /// the one failure a listener cannot see.
    pub missing: Vec<String>,
    pub unlisted: Vec<String>,
}

impl Playlist {
    /// True for a file the player should open as a playlist rather than as a
    /// single recording.
    pub fn is_playlist(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                PLAYLIST_EXTENSIONS
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known))
            })
    }

    /// Opens an archive and works out the running order. The archive stays
    /// open for as long as the playlist does, so the tracks that play are the
    /// tracks that were listed — a file swapped underneath us mid-playlist
    /// cannot substitute one entry for another.
    pub fn open(path: &Path) -> Result<Self> {
        let shown = path.file_name().unwrap_or_default().to_string_lossy();
        let file = File::open(path).with_context(|| format!("could not open {shown}"))?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .with_context(|| format!("{shown} is not a readable zip file"))?;

        let (candidates, manifest) = read_entries(&mut archive);
        if candidates.is_empty() {
            bail!("{shown} has no WAV or MP3 files in it to play");
        }

        let manifest = match manifest {
            Some(index) => read_manifest(&mut archive, index)?,
            None => Vec::new(),
        };
        let ordered = !manifest.is_empty();
        let (tracks, missing, unlisted) = arrange(candidates, manifest);

        if tracks.is_empty() {
            bail!("{shown} has no WAV or MP3 files in it to play");
        }
        Ok(Self {
            archive,
            tracks,
            name: shown.into_owned(),
            ordered,
            missing,
            unlisted,
        })
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn track(&self, at: usize) -> Option<&Track> {
        self.tracks.get(at)
    }

    /// The bytes of one track, decompressed into memory.
    ///
    /// Bounded by what comes *out* of the decompressor rather than by the size
    /// the archive claims, since those are two different numbers and only one
    /// of them is a fact.
    pub fn read(&mut self, at: usize) -> Result<Vec<u8>> {
        let Some(track) = self.tracks.get(at) else {
            bail!("there is no track {} in this playlist", at + 1);
        };
        let (name, index) = (track.name.clone(), track.index);

        let entry = self
            .archive
            .by_index(index)
            .with_context(|| format!("could not read {name} from the playlist"))?;
        let mut bytes = Vec::with_capacity(entry.size().min(MAX_TRACK_BYTES) as usize);
        entry
            .take(MAX_TRACK_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("could not read {name} from the playlist"))?;

        if bytes.len() as u64 > MAX_TRACK_BYTES {
            bail!(
                "{name} unpacks to more than {} MB, which is more than this app will play — it \
                 may be a damaged or deliberately malformed file",
                MAX_TRACK_BYTES / (1024 * 1024)
            );
        }
        if bytes.is_empty() {
            bail!("{name} is empty");
        }
        Ok(bytes)
    }
}

/// One playable entry found in the archive, before the manifest has had its say.
struct Candidate {
    name: String,
    index: usize,
}

/// Walks the archive once: every playable entry, and where the manifest is.
fn read_entries<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> (Vec<Candidate>, Option<usize>) {
    let mut candidates = Vec::new();
    let mut manifest = None;

    for index in 0..archive.len() {
        // One unreadable entry — an encrypted one, say — is not a reason to
        // refuse the rest of the playlist.
        let Ok(entry) = archive.by_index_raw(index) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` is `None` for anything that would climb out of the
        // archive — an absolute path, a `..` that goes too far, a NUL byte.
        // Nothing here is ever used as a path, but an entry willing to lie
        // about where it lives is not one to take audio from either.
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        if is_platform_litter(&path) {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };

        if name.eq_ignore_ascii_case(MANIFEST) {
            // The first one wins, so an archive carrying two cannot make the
            // running order depend on which was zipped last.
            manifest = manifest.or(Some(index));
            continue;
        }
        if !is_playable(&name) || entry.size() > MAX_TRACK_BYTES {
            continue;
        }
        if candidates.len() == MAX_TRACKS {
            break;
        }
        candidates.push(Candidate { name, index });
    }

    // The order the archive happens to store its entries in is not an order
    // anyone chose. Without a manifest, by name is at least the order the
    // files appear in every file browser the listener has already seen.
    candidates.sort_by(|a, b| natural_order(&a.name, &b.name));
    (candidates, manifest)
}

/// Whether the player has a decoder for this file. Kept in step with
/// [`crate::audio::PLAYABLE_EXTENSIONS`], which is the list the file chooser
/// offers for a single recording.
fn is_playable(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            crate::audio::PLAYABLE_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// Files the operating system put in the archive rather than the person who
/// made it. macOS writes an AppleDouble `._track.mp3` beside every real file
/// and a `__MACOSX` folder to keep them in, and those resource forks are
/// playable-looking enough to end up in the running order twice over.
fn is_platform_litter(path: &Path) -> bool {
    path.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        component == "__MACOSX" || component.starts_with("._") || component == ".DS_Store"
    })
}

/// Compares names the way a person reads them, so `track2` comes before
/// `track10` — which is the whole reason a playlist that was zipped without a
/// manifest still plays in the order its maker had in mind.
fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = chunks(a).into_iter();
    let mut right = chunks(b).into_iter();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => {
                let order = match (&a, &b) {
                    (Chunk::Number(a), Chunk::Number(b)) => a.cmp(b),
                    (Chunk::Text(a), Chunk::Text(b)) => a.cmp(b),
                    // A number sorts before a word, so `2.mp3` precedes `a.mp3`.
                    (Chunk::Number(_), Chunk::Text(_)) => std::cmp::Ordering::Less,
                    (Chunk::Text(_), Chunk::Number(_)) => std::cmp::Ordering::Greater,
                };
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
enum Chunk {
    /// Capped rather than arbitrary-precision: a run of digits longer than
    /// this is not a track number, and saturating keeps two absurd ones
    /// comparing equal instead of panicking.
    Number(u64),
    Text(String),
}

/// Splits a name into runs of digits and runs of everything else, lower-cased
/// so that `B.mp3` and `b.mp3` do not sort a whole alphabet apart.
fn chunks(name: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut rest = name;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits > 0 {
            out.push(Chunk::Number(rest[..digits].parse().unwrap_or(u64::MAX)));
            rest = &rest[digits..];
            continue;
        }
        let text = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        out.push(Chunk::Text(rest[..text].to_lowercase()));
        rest = &rest[text..];
    }
    out
}

/// Reads and parses `media.txt`, which is advisory: a manifest that cannot be
/// made sense of leaves the playlist in name order rather than refusing to
/// play it.
fn read_manifest<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    index: usize,
) -> Result<Vec<Listed>> {
    let mut text = String::new();
    let read = archive
        .by_index(index)
        .context("could not read media.txt from the playlist")?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut text);
    if read.is_err() || text.len() as u64 > MAX_MANIFEST_BYTES {
        // Not an error: an unreadable running order is a running order the
        // playlist can do without.
        crate::log::line("playlist: media.txt could not be read as text; ignoring it");
        return Ok(Vec::new());
    }
    Ok(parse_manifest(&text))
}

/// One line of the running order, as the manifest gives it.
#[derive(Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub kind: TrackKind,
    /// The `pos` attribute, when there is one. Absent means "wherever this
    /// appears in the file", which is what a hand-written list means too.
    pub pos: Option<u64>,
}

/// Reads a `media.txt`.
///
/// The documented form is XML — `<audio type="music" pos="1">intro.mp3</audio>`
/// — but the file is called `.txt`, so a plain list of file names, one per
/// line, is read as well. Someone who opens a text file and types the names
/// they want in the order they want them has written a valid running order,
/// and telling them otherwise would be a pointless piece of pedantry.
///
/// No entity is ever fetched: `quick-xml` reports a reference as an event of
/// its own and only the five XML built-ins are resolved, so a manifest cannot
/// reach out to a file or a URL on the way past.
pub fn parse_manifest(text: &str) -> Vec<Listed> {
    let listed = parse_manifest_xml(text);
    if listed.is_empty() {
        parse_manifest_lines(text)
    } else {
        listed
    }
}

fn parse_manifest_xml(text: &str) -> Vec<Listed> {
    let mut reader = Reader::from_str(text);
    // A hand-edited running order is not going to be well-formed every time,
    // and an unclosed tag is no reason to lose the entries before it.
    reader.config_mut().check_end_names = false;

    let mut listed = Vec::new();
    let mut open: Option<(TrackKind, Option<u64>)> = None;
    let mut name = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.local_name().as_ref() == b"audio" => {
                open = Some((
                    attribute(&element, "type")
                        .as_deref()
                        .and_then(TrackKind::parse)
                        .unwrap_or_default(),
                    attribute(&element, "pos").and_then(|pos| pos.trim().parse().ok()),
                ));
                name.clear();
            }
            Ok(Event::Text(text)) if open.is_some() => {
                if let Ok(text) = text.xml10_content() {
                    name.push_str(&text);
                }
            }
            // `&amp;` and friends arrive as events of their own rather than
            // inlined, and a file name is allowed to contain an ampersand.
            Ok(Event::GeneralRef(reference)) if open.is_some() => {
                if let Ok(entity) = reference.xml10_content()
                    && let Ok(text) = quick_xml::escape::unescape(&format!("&{entity};"))
                {
                    name.push_str(&text);
                }
            }
            Ok(Event::End(element)) if element.local_name().as_ref() == b"audio" => {
                if let Some((kind, pos)) = open.take() {
                    let name = name.trim();
                    if !name.is_empty() {
                        listed.push(Listed {
                            name: name.to_string(),
                            kind,
                            pos,
                        });
                    }
                }
                name.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        if listed.len() == MAX_TRACKS {
            break;
        }
    }
    listed
}

/// A plain list: one file name per line, `#` starting a comment. Everything is
/// speech, since a format with nowhere to say otherwise cannot ask for music.
fn parse_manifest_lines(text: &str) -> Vec<Listed> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        // A stray tag from a half-written XML manifest is not a file name.
        .filter(|line| !line.is_empty() && !line.starts_with('<'))
        .take(MAX_TRACKS)
        .map(|line| Listed {
            name: line.to_string(),
            kind: TrackKind::Speech,
            pos: None,
        })
        .collect()
}

/// An attribute by local name, so `type` is found whether or not the manifest
/// bothered with a namespace.
fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name.as_bytes())
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Puts the archive's playable entries into the order the manifest asks for.
///
/// Anything the manifest never mentioned is appended in name order rather than
/// dropped. A file that is in the zip but not in the list is far more likely to
/// be an oversight than a deliberate exclusion, and a track that never plays
/// and never says why is the one kind of failure a listener cannot notice.
fn arrange(
    candidates: Vec<Candidate>,
    mut manifest: Vec<Listed>,
) -> (Vec<Track>, Vec<String>, Vec<String>) {
    if manifest.is_empty() {
        let tracks = candidates
            .into_iter()
            .map(|candidate| Track {
                name: candidate.name,
                kind: TrackKind::Speech,
                index: candidate.index,
            })
            .collect();
        return (tracks, Vec::new(), Vec::new());
    }

    // `pos` decides, and where it is absent the order written in the file
    // does — which for a manifest that numbers nothing is the whole order.
    // A stable sort is what keeps that second half true.
    manifest.sort_by_key(|entry| entry.pos.unwrap_or(u64::MAX));

    let mut taken = vec![false; candidates.len()];
    let mut tracks = Vec::new();
    let mut missing = Vec::new();

    for entry in &manifest {
        // Matched on the last component and without regard to case: a manifest
        // written on Windows says `Intro.MP3` for a file zipped as
        // `audio/intro.mp3`, and refusing that helps nobody.
        let wanted = Path::new(&entry.name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        let found = candidates
            .iter()
            .enumerate()
            .find(|(at, candidate)| !taken[*at] && candidate.name.eq_ignore_ascii_case(&wanted));
        match found {
            Some((at, candidate)) => {
                taken[at] = true;
                tracks.push(Track {
                    name: candidate.name.clone(),
                    kind: entry.kind,
                    index: candidate.index,
                });
            }
            None => missing.push(entry.name.clone()),
        }
    }

    let mut unlisted = Vec::new();
    for (at, candidate) in candidates.into_iter().enumerate() {
        if taken[at] {
            continue;
        }
        unlisted.push(candidate.name.clone());
        tracks.push(Track {
            name: candidate.name,
            kind: TrackKind::Speech,
            index: candidate.index,
        });
    }
    tracks.truncate(MAX_TRACKS);
    (tracks, missing, unlisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The example running order that ships with the app, verbatim.
    const EXAMPLE: &str = r#"<music>
    <audio type="music" pos="1">intro.mp3</audio>
    <audio type="speech" pos="2">briefing.mp3</audio>
    <audio type="music" pos="3">words.mp3</audio>
</music>"#;

    fn listed(name: &str, kind: TrackKind, pos: Option<u64>) -> Listed {
        Listed {
            name: name.to_string(),
            kind,
            pos,
        }
    }

    /// Builds a zip on disk and hands back its path.
    fn write_zip(name: &str, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn names(list: &Playlist) -> Vec<&str> {
        (0..list.len())
            .filter_map(|at| list.track(at))
            .map(|track| track.name.as_str())
            .collect()
    }

    fn kinds(list: &Playlist) -> Vec<TrackKind> {
        (0..list.len())
            .filter_map(|at| list.track(at))
            .map(|track| track.kind)
            .collect()
    }

    #[test]
    fn the_example_running_order_reads_as_written() {
        assert_eq!(
            parse_manifest(EXAMPLE),
            vec![
                listed("intro.mp3", TrackKind::Music, Some(1)),
                listed("briefing.mp3", TrackKind::Speech, Some(2)),
                listed("words.mp3", TrackKind::Music, Some(3)),
            ]
        );
    }

    /// `pos` is the whole point of `pos`: a manifest whose entries are written
    /// out of order still asks for the order it numbers.
    #[test]
    fn the_position_attribute_decides_the_order_not_the_line_it_is_on() {
        let path = write_zip(
            "ae-playlist-pos-test.zip",
            &[
                ("one.mp3", b"1"),
                ("two.mp3", b"2"),
                ("three.mp3", b"3"),
                (
                    "media.txt",
                    br#"<music>
                        <audio type="speech" pos="3">three.mp3</audio>
                        <audio type="speech" pos="1">one.mp3</audio>
                        <audio type="speech" pos="2">two.mp3</audio>
                    </music>"#,
                ),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(list.ordered);
        assert_eq!(names(&list), ["one.mp3", "two.mp3", "three.mp3"]);
    }

    /// An unnumbered manifest is still an order — the one it is written in.
    #[test]
    fn an_unnumbered_manifest_keeps_the_order_it_was_written_in() {
        let order = parse_manifest(
            r#"<music>
                <audio type="speech">last.mp3</audio>
                <audio type="music">first.mp3</audio>
            </music>"#,
        );
        assert_eq!(
            order.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            ["last.mp3", "first.mp3"]
        );
    }

    /// The file is called `.txt`, so a plain list of names is a running order
    /// too — someone who never opens the README will write one of these.
    #[test]
    fn a_plain_list_of_names_is_read_as_a_running_order() {
        assert_eq!(
            parse_manifest("# the bulletin\nintro.mp3\n\nbriefing.mp3   \n"),
            vec![
                listed("intro.mp3", TrackKind::Speech, None),
                listed("briefing.mp3", TrackKind::Speech, None),
            ]
        );
    }

    /// What the manifest is for, beyond order: which tracks are music.
    #[test]
    fn the_manifest_says_which_tracks_are_music() {
        let path = write_zip(
            "ae-playlist-kinds-test.zip",
            &[
                ("briefing.mp3", b"speech"),
                ("outro.mp3", b"music"),
                (
                    "media.txt",
                    br#"<music>
                        <audio type="speech" pos="1">briefing.mp3</audio>
                        <audio type="music" pos="2">outro.mp3</audio>
                    </music>"#,
                ),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(kinds(&list), [TrackKind::Speech, TrackKind::Music]);
    }

    /// A track the manifest never mentions is played rather than dropped, and
    /// one it asks for that is not there is reported rather than ignored. A
    /// track that silently never plays is the one failure a listener cannot
    /// see for themselves.
    #[test]
    fn nothing_in_the_zip_goes_unplayed_and_nothing_missing_goes_unsaid() {
        let path = write_zip(
            "ae-playlist-extra-test.zip",
            &[
                ("listed.mp3", b"a"),
                ("unlisted.wav", b"b"),
                ("media.txt", b"listed.mp3\nabsent.mp3\n"),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(names(&list), ["listed.mp3", "unlisted.wav"]);
        assert_eq!(list.missing, ["absent.mp3"]);
        assert_eq!(list.unlisted, ["unlisted.wav"]);
    }

    /// Without a manifest the order is the names, read the way a person reads
    /// them — which is the order the person who numbered the files meant.
    #[test]
    fn without_a_manifest_tracks_play_in_the_order_their_names_are_read() {
        let path = write_zip(
            "ae-playlist-natural-test.zip",
            &[
                ("track10.mp3", b"j"),
                ("track2.mp3", b"b"),
                ("track1.mp3", b"a"),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(!list.ordered);
        assert_eq!(names(&list), ["track1.mp3", "track2.mp3", "track10.mp3"]);
        assert_eq!(kinds(&list), [TrackKind::Speech; 3]);
    }

    /// macOS puts a resource fork beside every file it zips. Played, they are
    /// three seconds of noise between every pair of real tracks.
    #[test]
    fn the_junk_macos_puts_in_a_zip_is_not_part_of_the_playlist() {
        let path = write_zip(
            "ae-playlist-litter-test.zip",
            &[
                ("__MACOSX/._intro.mp3", b"resource fork"),
                ("bulletin/._briefing.mp3", b"resource fork"),
                ("bulletin/briefing.mp3", b"the real one"),
                ("bulletin/.DS_Store", b"folder settings"),
            ],
        );
        let mut list = Playlist::open(&path).unwrap();
        assert_eq!(names(&list), ["briefing.mp3"]);
        assert_eq!(list.read(0).unwrap(), b"the real one");
        std::fs::remove_file(&path).ok();
    }

    /// A zip is not a playlist just because it is a zip, and being told so at
    /// the moment of choosing beats a Play button that does nothing.
    #[test]
    fn a_zip_with_no_audio_in_it_is_refused_when_it_is_chosen() {
        let path = write_zip(
            "ae-playlist-empty-test.zip",
            &[("notes.txt", b"no audio here"), ("photo.png", b"nor here")],
        );
        let refusal = Playlist::open(&path).err().map(|error| error.to_string());
        std::fs::remove_file(&path).ok();
        let error = refusal.expect("a zip with no audio in it should be refused");

        assert!(
            error.contains("no WAV or MP3"),
            "unhelpful refusal: {error}"
        );
    }

    /// Nothing is ever unpacked, so a name that climbs out of the archive has
    /// nowhere to climb to — but an entry willing to lie about where it lives
    /// is not one to take audio from either.
    ///
    /// Only the `..` case is built here: `ZipWriter` strips a leading slash on
    /// the way in, so an absolute name cannot be written with it. That case is
    /// `enclosed_name`'s to refuse, and it does.
    #[test]
    fn an_entry_that_points_outside_the_archive_is_left_out() {
        let path = write_zip(
            "ae-playlist-traversal-test.zip",
            &[("../../escape.mp3", b"nope"), ("honest.mp3", b"fine")],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(names(&list), ["honest.mp3"]);
    }

    /// A manifest is advisory. One that is gibberish leaves the playlist in
    /// name order rather than refusing to play a zip full of perfectly good
    /// audio.
    #[test]
    fn a_manifest_that_makes_no_sense_costs_the_order_not_the_playlist() {
        let path = write_zip(
            "ae-playlist-gibberish-test.zip",
            &[
                ("b.mp3", b"b"),
                ("a.mp3", b"a"),
                ("media.txt", b"<music><audio type=\"music\" pos=\""),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(names(&list), ["a.mp3", "b.mp3"]);
    }

    /// Case and folder come from whichever machine wrote the zip, and neither
    /// is something the person who wrote the running order was thinking about.
    #[test]
    fn a_manifest_matches_a_name_whatever_its_case_or_folder() {
        let path = write_zip(
            "ae-playlist-case-test.zip",
            &[
                ("audio/briefing.mp3", b"a"),
                ("audio/intro.mp3", b"b"),
                (
                    "media.txt",
                    br#"<music>
                        <audio type="music" pos="1">Intro.MP3</audio>
                        <audio type="speech" pos="2">audio/BRIEFING.mp3</audio>
                    </music>"#,
                ),
            ],
        );
        let list = Playlist::open(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(names(&list), ["intro.mp3", "briefing.mp3"]);
        assert!(list.missing.is_empty(), "{:?}", list.missing);
    }

    /// A few kilobytes on disk that unpack to a quarter of a gigabyte is the
    /// shape a zip bomb comes in, and the app has to still be running
    /// afterwards to say so. Left out of the running order rather than
    /// refused: the honest track beside it is still perfectly playable.
    ///
    /// This is the cap on the size the archive *claims*. The one on the bytes
    /// that actually arrive — for an archive whose directory lies — cannot be
    /// built with `ZipWriter`, which writes down the sizes it really wrote.
    #[test]
    fn a_track_that_unpacks_far_past_the_limit_is_left_out_of_the_playlist() {
        let path = std::env::temp_dir().join("ae-playlist-bomb-test.zip");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("bomb.mp3", options).unwrap();
        // Zeroes compress to almost nothing, which is the whole trick: this is
        // a few kilobytes on disk claiming to be a quarter of a gigabyte.
        let block = vec![0u8; 1024 * 1024];
        for _ in 0..(MAX_TRACK_BYTES / block.len() as u64 + 2) {
            zip.write_all(&block).unwrap();
        }
        zip.start_file("honest.mp3", options).unwrap();
        zip.write_all(b"a real track").unwrap();
        zip.finish().unwrap();

        let mut list = Playlist::open(&path).unwrap();
        let found: Vec<String> = names(&list).iter().map(|n| n.to_string()).collect();
        let bytes = list.read(0).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(found, ["honest.mp3"]);
        assert_eq!(bytes, b"a real track");
    }

    #[test]
    fn only_a_zip_opens_as_a_playlist() {
        assert!(Playlist::is_playlist(Path::new("bulletin.zip")));
        assert!(Playlist::is_playlist(Path::new("BULLETIN.ZIP")));
        assert!(!Playlist::is_playlist(Path::new("bulletin.mp3")));
        assert!(!Playlist::is_playlist(Path::new("bulletin")));
    }
}

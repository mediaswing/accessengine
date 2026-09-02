//! Saving a document as an MP3 file.
//!
//! Only the ElevenLabs engine can do this, and the reason is not a shortcut:
//! that API hands back MP3 frames, so an export is the same requests playback
//! already makes, written to a file rather than to the sound card. The system
//! voices cannot be recorded at all — the `tts` crate speaks to the audio
//! device and offers no way to render into a buffer on any of its back ends —
//! so the UI says so plainly rather than silently producing something else.
//!
//! Consecutive segments are simply appended. MP3 is a sequence of independent
//! frames and every segment is requested at the same rate and bitrate, so the
//! result is a valid file that any player will read end to end.

use crate::{t, tn};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::speech::elevenlabs::{self, VoiceRequest};

#[derive(Debug)]
pub enum Event {
    /// How many sentences have been written so far.
    Progress(usize),
    Finished {
        path: PathBuf,
        bytes: u64,
        elapsed: Duration,
    },
    /// Stopped at the user's request; nothing was left on disk.
    Cancelled,
    Failed(String),
}

/// A single export, running on its own thread.
///
/// One at a time: these are long, they cost the user's quota, and a second
/// export competing for the same rate limit would only make both slower.
#[derive(Default)]
pub struct Export {
    pending: Option<Receiver<Event>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Sentences written, and how many there are altogether.
    pub progress: (usize, usize),
    /// Where it is being written, for the progress line.
    pub destination: Option<PathBuf>,
}

impl Export {
    pub fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    /// Begin writing `texts` to `path`. `repaint` wakes the UI as progress
    /// arrives, the same way the vision and speech workers do.
    pub fn start(
        &mut self,
        path: PathBuf,
        texts: Vec<String>,
        request: VoiceRequest,
        repaint: impl Fn() + Send + 'static,
    ) {
        if self.is_running() {
            return;
        }
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let total = texts.len();
        let destination = path.clone();

        let spawned = std::thread::Builder::new()
            .name("mp3-export".to_string())
            .spawn(move || {
                let started = Instant::now();
                let outcome = {
                    let progress = |done: usize| {
                        let _ = tx.send(Event::Progress(done));
                        repaint();
                    };
                    write_mp3(&path, &texts, &request, &flag, &progress)
                };
                let event = match outcome {
                    Ok(Some(bytes)) => Event::Finished {
                        path,
                        bytes,
                        elapsed: started.elapsed(),
                    },
                    Ok(None) => Event::Cancelled,
                    Err(e) => Event::Failed(format!("{e:#}")),
                };
                let _ = tx.send(event);
                repaint();
            });

        match spawned {
            Ok(_) => {
                self.pending = Some(rx);
                self.cancel = Some(cancel);
                self.progress = (0, total);
                self.destination = Some(destination);
            }
            Err(e) => log::error!("could not spawn the export thread: {e}"),
        }
    }

    /// Ask the worker to stop. It finishes the request already in flight, then
    /// removes the part-written file.
    pub fn cancel(&mut self) {
        if let Some(flag) = &self.cancel {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Call each frame. Progress is folded in here; anything else ends the run.
    pub fn poll(&mut self) -> Option<Event> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(Event::Progress(done)) => {
                self.progress.0 = done;
                None
            }
            Ok(event) => {
                self.finish();
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finish();
                Some(Event::Failed("the export ended unexpectedly".to_string()))
            }
        }
    }

    fn finish(&mut self) {
        self.pending = None;
        self.cancel = None;
        self.progress = (0, 0);
        self.destination = None;
    }
}

impl Drop for Export {
    /// Closing the window should not leave a request queue running against
    /// somebody's paid account.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A first guess at how long one sentence takes: the round trip to ElevenLabs
/// dominates, and it is much the same whether the sentence is six words or
/// twenty. Deliberately a little pessimistic — an export that finishes sooner
/// than promised is a good surprise — and replaced by a real measurement as
/// soon as there is one.
const ASSUMED_PER_SENTENCE: Duration = Duration::from_millis(1500);

/// How long an export is likely to take.
///
/// Seeded with a guess and corrected by measurement: every ElevenLabs sentence
/// the app waits on, whether played or saved, is a timing of exactly the work
/// an export is made of.
#[derive(Clone, Copy, Debug)]
pub struct Estimate {
    per_sentence: Duration,
    measured: bool,
}

impl Default for Estimate {
    fn default() -> Self {
        Self {
            per_sentence: ASSUMED_PER_SENTENCE,
            measured: false,
        }
    }
}

impl Estimate {
    /// Fold in an observed time for one sentence.
    ///
    /// The first real measurement replaces the guess outright; after that it is
    /// a rolling average, so one slow response on a bad connection does not
    /// throw the figure the user is shown.
    pub fn record(&mut self, per_sentence: Duration) {
        // A zero or absurd sample says the measurement was of something else —
        // a cached response, a clock jump — not of a round trip.
        if per_sentence.is_zero() || per_sentence > Duration::from_secs(120) {
            return;
        }
        self.per_sentence = if self.measured {
            (self.per_sentence * 7 + per_sentence * 3) / 10
        } else {
            self.measured = true;
            per_sentence
        };
    }

    pub fn for_sentences(&self, sentences: usize) -> Duration {
        self.per_sentence * sentences as u32
    }

    /// Whether the figure comes from this machine and this account, or is still
    /// the built-in guess. Worth saying out loud: the two deserve different
    /// amounts of trust.
    pub fn is_measured(&self) -> bool {
        self.measured
    }
}

/// A duration in words, rounded to something a person would actually say.
///
/// Never precise, and never pretending to be: the point is to tell someone
/// whether this is a cup of tea or an afternoon.
pub fn approximate_duration(d: Duration) -> String {
    let seconds = d.as_secs();
    match seconds {
        0..=44 => t!("duration.under_a_minute"),
        45..=89 => t!("duration.about_a_minute"),
        90..=3599 => tn!("duration.minutes", (seconds as f64 / 60.0).round() as u64),
        _ => {
            let hours = seconds / 3600;
            let minutes = ((seconds % 3600) as f64 / 60.0).round() as u64;
            let (hours, minutes) = if minutes == 60 {
                (hours + 1, 0)
            } else {
                (hours, minutes)
            };
            if minutes == 0 {
                tn!("duration.hours", hours)
            } else {
                // The hours carry the plural; the minutes are never one, since
                // a single minute past the hour rounds away above.
                tn!("duration.hours_minutes", hours, minutes = minutes)
            }
        }
    }
}

/// Write every sentence to `destination`, returning the size on success and
/// `None` if the user cancelled.
fn write_mp3(
    destination: &Path,
    texts: &[String],
    request: &VoiceRequest,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize),
) -> Result<Option<u64>> {
    let http = elevenlabs::client()?;
    let synthesise = |text: &str| elevenlabs::synthesise(&http, request, text);
    write_with(destination, texts, &synthesise, cancel, progress)
}

fn write_with(
    destination: &Path,
    texts: &[String],
    synthesise: &dyn Fn(&str) -> Result<Vec<u8>>,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize),
) -> Result<Option<u64>> {
    // Written beside the destination and renamed at the end. A cancelled or
    // failed export must never leave a file that looks like a finished one —
    // least of all under a name the user chose and expects to be able to play.
    let partial = partial_path(destination);
    let result = stream_segments(&partial, texts, synthesise, cancel, progress);

    match result {
        Ok(Some(bytes)) => {
            std::fs::rename(&partial, destination).inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })?;
            log::info!(
                "saved {} sentences to {} ({bytes} bytes)",
                texts.len(),
                destination.display()
            );
            Ok(Some(bytes))
        }
        other => {
            let _ = std::fs::remove_file(&partial);
            other
        }
    }
}

/// The file half of an export, with synthesis passed in.
///
/// Separated so the part that can lose someone's data — the scratch file, the
/// cancel check, the cleanup — is testable without a network call or an API
/// key, which is the half worth having tests for.
fn stream_segments(
    partial: &Path,
    texts: &[String],
    synthesise: &dyn Fn(&str) -> Result<Vec<u8>>,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize),
) -> Result<Option<u64>> {
    let file = File::create(partial)
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut writer = BufWriter::new(file);
    let mut written = 0u64;

    for (index, text) in texts.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            log::info!("export cancelled after {index} of {}", texts.len());
            return Ok(None);
        }
        // Sentences are synthesised one at a time, as playback does: several
        // at once would hit the account's rate limit rather than go faster.
        let audio = synthesise(text)
            .with_context(|| format!("synthesising sentence {} of {}", index + 1, texts.len()))?;
        writer
            .write_all(&audio)
            .with_context(|| format!("writing {}", partial.display()))?;
        written += audio.len() as u64;
        progress(index + 1);
    }

    writer
        .flush()
        .with_context(|| format!("writing {}", partial.display()))?;
    Ok(Some(written))
}

/// The scratch name an unfinished export is written under.
fn partial_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export.mp3".to_string());
    destination.with_file_name(format!("{name}.part"))
}

/// A filename to offer in a save dialog, from the document's title.
///
/// The title comes from a filename, but a document built from an image
/// description has whatever the picture was called in it, so anything that
/// would change which directory this lands in is removed rather than trusted.
pub fn suggested_filename(title: &str, extension: &str) -> String {
    let stem: String = title
        .rsplit_once('.')
        .map_or(title, |(stem, _)| stem)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_matches('.').trim();
    if stem.is_empty() {
        format!("untitled.{extension}")
    } else {
        format!("{stem}.{extension}")
    }
}

/// Give the chosen path the expected extension if the user did not type one.
pub fn with_extension(path: PathBuf, extension: &str) -> PathBuf {
    let already = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(extension));
    if already {
        path
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        path.with_file_name(format!("{name}.{extension}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "accessengine-export-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("reading.mp3")
    }

    fn sentences(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("sentence {i}")).collect()
    }

    /// Segments are appended in order and land under the chosen name, with no
    /// scratch file left behind.
    #[test]
    fn a_finished_export_is_the_segments_end_to_end() {
        let destination = scratch("finished");
        let cancel = AtomicBool::new(false);
        let written = write_with(
            &destination,
            &sentences(3),
            &|text: &str| Ok(text.as_bytes().to_vec()),
            &cancel,
            &|_| {},
        )
        .expect("writes");

        assert_eq!(written, Some(30), "three ten-byte segments");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("reads"),
            "sentence 0sentence 1sentence 2"
        );
        assert!(!partial_path(&destination).exists(), "scratch file left behind");
        let _ = std::fs::remove_dir_all(destination.parent().unwrap());
    }

    /// A cancelled export leaves nothing at all — least of all a file under the
    /// name the user chose, which they would reasonably expect to be complete.
    #[test]
    fn a_cancelled_export_leaves_no_file() {
        let destination = scratch("cancelled");
        let cancel = AtomicBool::new(false);
        let synthesised = std::cell::Cell::new(0usize);

        let outcome = write_with(
            &destination,
            &sentences(5),
            &|text: &str| {
                cancel.store(true, Ordering::Relaxed); // as if the user pressed Cancel
                Ok(text.as_bytes().to_vec())
            },
            &cancel,
            &|done| synthesised.set(done),
        )
        .expect("cancelling is not an error");

        assert!(outcome.is_none(), "cancelling must not report a size");
        assert!(!destination.exists(), "a cancelled export wrote a file");
        assert!(!partial_path(&destination).exists(), "scratch file left behind");
        assert_eq!(synthesised.get(), 1, "it should stop at the next sentence");
        let _ = std::fs::remove_dir_all(destination.parent().unwrap());
    }

    /// The same for a failure part-way through: half a document is not a file
    /// anyone wants to discover later under a name that promises the whole one.
    #[test]
    fn a_failed_export_leaves_no_file() {
        let destination = scratch("failed");
        let cancel = AtomicBool::new(false);

        let error = write_with(
            &destination,
            &sentences(4),
            &|text: &str| {
                if text.ends_with('2') {
                    anyhow::bail!("ElevenLabs quota exhausted")
                }
                Ok(text.as_bytes().to_vec())
            },
            &cancel,
            &|_| {},
        )
        .expect_err("the failure should surface");

        assert!(format!("{error:#}").contains("quota"), "{error:#}");
        assert!(format!("{error:#}").contains("sentence 3 of 4"), "{error:#}");
        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
        let _ = std::fs::remove_dir_all(destination.parent().unwrap());
    }

    #[test]
    fn durations_are_described_the_way_someone_would_say_them() {
        // The wording is the language's, so the language has to be pinned:
        // another test switching it while this one runs would otherwise fail
        // this one, once every few dozen runs.
        crate::i18n::with_language("en", durations_in_english);
    }

    fn durations_in_english() {
        let secs = |s| approximate_duration(Duration::from_secs(s));
        assert_eq!(secs(3), "less than a minute");
        assert_eq!(secs(44), "less than a minute");
        assert_eq!(secs(60), "about a minute");
        assert_eq!(secs(90), "about 2 minutes");
        assert_eq!(secs(600), "about 10 minutes");
        assert_eq!(secs(3600), "about 1 hour");
        assert_eq!(secs(4500), "about 1 hour 15 minutes");
        assert_eq!(secs(7200), "about 2 hours");
        // 59.5 minutes past the hour must not read as "1 hour 60 minutes".
        assert_eq!(secs(7195), "about 2 hours");
    }

    #[test]
    fn the_first_measurement_replaces_the_guess() {
        let mut estimate = Estimate::default();
        assert!(!estimate.is_measured());
        let guessed = estimate.for_sentences(10);

        estimate.record(Duration::from_millis(500));
        assert!(estimate.is_measured());
        assert_eq!(estimate.for_sentences(10), Duration::from_secs(5));
        assert!(estimate.for_sentences(10) < guessed);

        // Later samples move it, but only part of the way.
        estimate.record(Duration::from_millis(1500));
        let per = estimate.for_sentences(1);
        assert!(
            per > Duration::from_millis(500) && per < Duration::from_millis(1500),
            "{per:?}"
        );
    }

    /// A nonsense sample must not become the number shown to the user.
    #[test]
    fn absurd_measurements_are_ignored() {
        let mut estimate = Estimate::default();
        estimate.record(Duration::ZERO);
        estimate.record(Duration::from_secs(600));
        assert!(!estimate.is_measured());
        assert_eq!(estimate.for_sentences(1), ASSUMED_PER_SENTENCE);
    }

    #[test]
    fn the_suggested_name_drops_the_old_extension() {
        assert_eq!(suggested_filename("chapter one.txt", "mp3"), "chapter one.mp3");
        assert_eq!(suggested_filename("notes", "mp3"), "notes.mp3");
        assert_eq!(suggested_filename("report.final.md", "mp3"), "report.final.mp3");
        assert_eq!(suggested_filename("holiday.jpg", "txt"), "holiday.txt");
    }

    /// A title reaches this from a file on disk or from an image description,
    /// so it must not be able to steer the save dialog somewhere else.
    #[test]
    fn the_suggested_name_cannot_carry_a_path() {
        for title in ["../../etc/passwd", "C:\\Windows\\system32\\x", "a/b/c.txt"] {
            let name = suggested_filename(title, "mp3");
            assert!(!name.contains('/'), "{name}");
            assert!(!name.contains('\\'), "{name}");
            assert!(!name.contains(':'), "{name}");
        }
        assert_eq!(suggested_filename("   ", "mp3"), "untitled.mp3");
        assert_eq!(suggested_filename("...", "txt"), "untitled.txt");
    }

    #[test]
    fn the_extension_is_added_only_when_missing() {
        assert_eq!(
            with_extension(PathBuf::from("/tmp/a.mp3"), "mp3"),
            PathBuf::from("/tmp/a.mp3")
        );
        assert_eq!(
            with_extension(PathBuf::from("/tmp/a.MP3"), "mp3"),
            PathBuf::from("/tmp/a.MP3")
        );
        assert_eq!(
            with_extension(PathBuf::from("/tmp/reading"), "mp3"),
            PathBuf::from("/tmp/reading.mp3")
        );
        assert_eq!(
            with_extension(PathBuf::from("/tmp/notes"), "txt"),
            PathBuf::from("/tmp/notes.txt")
        );
    }

    /// The unfinished file must sit beside the destination — same directory,
    /// so the final rename cannot cross a filesystem — and never be mistaken
    /// for the finished one.
    #[test]
    fn the_partial_file_sits_beside_the_destination() {
        let partial = partial_path(Path::new("/tmp/audio/book.mp3"));
        assert_eq!(partial, PathBuf::from("/tmp/audio/book.mp3.part"));
        assert_eq!(partial.parent(), Path::new("/tmp/audio/book.mp3").parent());
    }
}

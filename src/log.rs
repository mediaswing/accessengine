//! A log of what happened this session, written to disk for debugging.
//!
//! Deliberately session-scoped: the file is started fresh at launch, and the
//! one from the previous run is kept alongside it. Keeping the previous run
//! matters more than it sounds — the natural response to "it did something
//! odd" is to restart the app, and a log that only ever held the current
//! session would be destroyed by exactly that.
//!
//! What goes in here is aimed at the failure this module was written after: an
//! image that came back transcribed as invented text, from a vision model that
//! reported perfect success. Nothing in the response said anything was wrong,
//! so an error log would have been empty. What was actually needed was the
//! content of the exchange — what was sent, what came back. That is what is
//! recorded: the shape and outcome of every call to something outside this
//! process, whether or not it failed.
//!
//! Two things are deliberately *not* recorded. The text of the user's documents
//! never goes in, only its length — this app is opened on private letters and
//! medical results, and a debug file that accumulates them is a worse problem
//! than the bug it helps fix. Neither does the ElevenLabs API key; anything
//! shaped like one is scrubbed on the way in, since the whole point of the file
//! is that people paste it into bug reports. See [`redact`].

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Stops a runaway job filling the disk. A session that hits this has already
/// logged far more than anyone will read, and the beginning — where the useful
/// part usually is — is what gets kept.
const MAX_BYTES: usize = 2 * 1024 * 1024;

struct Session {
    file: File,
    written: usize,
    /// Set once the cap is hit, so the "log truncated" note is written once
    /// rather than on every line after it.
    capped: bool,
    started: Instant,
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

fn session() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// Where this session's log lives, and where the previous one was moved to.
///
/// `directories` has no log directory of its own, so these are the platform
/// conventions written out: macOS keeps user logs in `~/Library/Logs`, where
/// Console.app finds them without being told where to look, and Windows has no
/// equivalent so the app's own local data directory is used.
pub fn path() -> Option<PathBuf> {
    Some(directory()?.join("accessengine.log"))
}

fn previous_path() -> Option<PathBuf> {
    Some(directory()?.join("accessengine.previous.log"))
}

#[cfg(target_os = "macos")]
fn directory() -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    Some(home.join("Library").join("Logs").join("accessengine"))
}

#[cfg(not(target_os = "macos"))]
fn directory() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("io", "accessengine", "accessengine")?;
    Some(dirs.data_local_dir().join("logs"))
}

/// Opens this session's log, moving the previous one aside.
///
/// Every failure here is swallowed. A log is a debugging aid, and an app that
/// refused to start because it could not write one would be a worse bug than
/// anything the log could help diagnose.
pub fn start(version: &str) {
    let Some(path) = path() else { return };
    let Some(directory) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(directory).is_err() {
        return;
    }
    if let Some(previous) = previous_path() {
        // Best effort: on the first ever run there is nothing to move.
        let _ = std::fs::rename(&path, previous);
    }

    let Ok(file) = File::create(&path) else {
        return;
    };
    if let Ok(mut guard) = session().lock() {
        *guard = Some(Session {
            file,
            written: 0,
            capped: false,
            started: Instant::now(),
        });
    }

    line(format!("accessengine {version} on {}", platform()));
    line(format!("session started {}", wall_clock()));
    line(format!("log file {}", path.display()));
}

fn platform() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Appends one line, stamped with how long the app has been running.
///
/// Elapsed time rather than clock time on every line: what a log is read for is
/// which step was slow or where it stopped, and seconds-since-launch answers
/// that at a glance. The wall clock is in the header for anyone matching the
/// file against when something happened.
pub fn line(message: impl AsRef<str>) {
    let Ok(mut guard) = session().lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else { return };

    if state.capped {
        return;
    }
    if state.written >= MAX_BYTES {
        state.capped = true;
        let _ = writeln!(
            state.file,
            "\n--- log truncated: {MAX_BYTES} bytes reached ---"
        );
        let _ = state.file.flush();
        return;
    }

    let seconds = state.started.elapsed().as_secs_f64();
    let text = format!("[{seconds:8.3}s] {}\n", redact(message.as_ref()));
    // Flushed per line rather than buffered: a log whose last few lines are
    // missing is least useful in exactly the case it is most wanted, which is
    // the app having stopped unexpectedly.
    if state.file.write_all(text.as_bytes()).is_ok() {
        let _ = state.file.flush();
        state.written += text.len();
    }
}

/// This session's log, for the button that copies it to the clipboard.
pub fn contents() -> String {
    let Some(path) = path() else {
        return "No log file is being written on this system.".to_string();
    };
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| format!("The log at {} could not be read: {error}", path.display()))
}

/// Removes anything shaped like an ElevenLabs API key.
///
/// The key is never deliberately logged, so this is a backstop rather than the
/// main defence — but it guards the case that matters, which is a key arriving
/// inside an error message quoted back from somewhere else. Keys are `sk_`
/// followed by a run of key characters; the prefix is left in place so the log
/// still shows that a key was present.
fn redact(message: &str) -> String {
    const PREFIX: &str = "sk_";
    if !message.contains(PREFIX) {
        return message.to_string();
    }

    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at]);
        let after = &rest[at + PREFIX.len()..];
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .unwrap_or(after.len());
        // Short runs are words like `sk_test`, not keys; a real key is long.
        if end >= 8 {
            let _ = write!(out, "sk_<redacted, {end} characters>");
            rest = &after[end..];
        } else {
            out.push_str(PREFIX);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Formats the current time as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Written out rather than pulled from a date crate: this is the only place in
/// the app that needs a calendar date, and it is one function against a
/// dependency that would otherwise be carried for a single log header.
fn wall_clock() -> String {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "an unknown time".to_string();
    };
    let seconds = now.as_secs() as i64;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days since the Unix epoch to a calendar date, by Howard Hinnant's
/// `civil_from_days` — the standard branch-free form of this conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
    // year and makes the month arithmetic below uniform.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_scrubbed_but_its_presence_is_still_visible() {
        let scrubbed = redact("failed with key sk_abcdef0123456789 in the header");
        assert!(!scrubbed.contains("abcdef0123456789"), "{scrubbed}");
        assert!(scrubbed.contains("sk_<redacted"), "{scrubbed}");
        assert!(scrubbed.starts_with("failed with key "));
        assert!(scrubbed.ends_with(" in the header"));
    }

    #[test]
    fn several_keys_in_one_line_are_all_scrubbed() {
        let scrubbed = redact("sk_aaaaaaaaaaaaaaaa and sk_bbbbbbbbbbbbbbbb");
        assert!(!scrubbed.contains("aaaaaaaa"), "{scrubbed}");
        assert!(!scrubbed.contains("bbbbbbbb"), "{scrubbed}");
        assert_eq!(scrubbed.matches("sk_<redacted").count(), 2);
    }

    /// The scrubber must not mangle ordinary text that happens to start `sk_`.
    #[test]
    fn short_sk_words_are_left_alone() {
        assert_eq!(redact("the sk_test fixture"), "the sk_test fixture");
        assert_eq!(redact("nothing to do here"), "nothing to do here");
    }

    #[test]
    fn dates_convert_against_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, which is where a wrong conversion shows up first.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}

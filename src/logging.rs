//! File logging.
//!
//! Installs a `log` implementation so everything the app and its dependencies
//! emit lands in one file per day, plus an in-memory tail the Diagnostics panel
//! shows without touching the disk. Records are flushed on every write: a log
//! that loses the last few lines is worthless for diagnosing a crash.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines kept in memory for the in-app log viewer.
const TAIL_CAPACITY: usize = 500;
/// Rotate once a day's file passes this size, keeping one previous generation.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Days of logs to keep. Older files are deleted at startup.
const KEEP_DAYS: i64 = 14;
/// Filename shape: `accessengine-2026-09-02.log`.
const FILE_PREFIX: &str = "accessengine-";
const FILE_SUFFIX: &str = ".log";

/// Dependencies that are only interesting when something is badly wrong.
/// Without this a debug-level run is thousands of lines of frame timing.
const NOISY_CRATES: &[&str] = &[
    "wgpu", "wgpu_core", "wgpu_hal", "naga", "calloop", "winit", "egui", "egui_glow", "egui_wgpu",
    "eframe", "arboard", "objc", "cpal", "symphonia", "zbus", "polling", "mio", "hyper",
    "hyper_util", "reqwest", "rustls", "h2", "want", "tower", "tracing",
];

/// The file currently being written, and which day it belongs to.
struct DailyFile {
    writer: Option<BufWriter<File>>,
    /// Days since the epoch, so a run that crosses midnight rolls over.
    day: i64,
    path: PathBuf,
}

impl DailyFile {
    fn open(day: i64) -> Self {
        let path = log_path_for(day);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        rotate_if_large(&path);
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(BufWriter::new);
        Self { writer, day, path }
    }

    /// Move to today's file once the clock passes midnight.
    fn roll_to(&mut self, day: i64) {
        if day == self.day {
            return;
        }
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
        *self = Self::open(day);
    }
}

struct FileLogger {
    file: Mutex<DailyFile>,
    tail: Mutex<VecDeque<String>>,
    verbose: bool,
}

static LOGGER: OnceLock<&'static FileLogger> = OnceLock::new();

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if !self.verbose && metadata.level() > Level::Info {
            let target = metadata.target();
            if NOISY_CRATES
                .iter()
                .any(|c| target == *c || target.starts_with(&format!("{c}::")))
            {
                return false;
            }
        }
        true
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = now_utc();
        let line = format!(
            "{} {:<5} [{}] {}",
            now.timestamp(),
            record.level(),
            record.target(),
            record.args()
        );

        if let Ok(mut tail) = self.tail.lock() {
            if tail.len() == TAIL_CAPACITY {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }

        if let Ok(mut guard) = self.file.lock() {
            guard.roll_to(now.days);
            if let Some(writer) = guard.writer.as_mut() {
                // Nothing useful to do if the log itself fails to write.
                let _ = writeln!(writer, "{line}");
                let _ = writer.flush();
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(writer) = guard.writer.as_mut() {
                let _ = writer.flush();
            }
        }
    }
}

/// The current instant, split into the parts a log line needs.
struct Utc {
    days: i64,
    seconds_of_day: i64,
    millis: u32,
}

fn now_utc() -> Utc {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    Utc {
        days: secs.div_euclid(86_400),
        seconds_of_day: secs.rem_euclid(86_400),
        millis: now.subsec_millis(),
    }
}

impl Utc {
    /// A UTC date and time, hand-formatted so the app does not take a date
    /// library just to stamp a log line.
    fn timestamp(&self) -> String {
        let (h, m, s) = (
            self.seconds_of_day / 3600,
            (self.seconds_of_day % 3600) / 60,
            self.seconds_of_day % 60,
        );
        let millis = self.millis;
        format!("{}T{h:02}:{m:02}:{s:02}.{millis:03}Z", date_string(self.days))
    }
}

/// `YYYY-MM-DD` for a count of days since the epoch.
fn date_string(days: i64) -> String {
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Howard Hinnant's days-to-civil-date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The inverse, for working out how old a dated filename is.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A folder chosen in Settings, when the user would rather the logs went
/// somewhere they can reach. Read by [`log_dir`]; set at startup and again
/// whenever the setting is applied.
///
/// Locking order: whoever holds the open file may take this, never the other
/// way round, so [`set_dir`] releases it before reopening anything.
static OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where the logs live when nobody has said otherwise: one file per day, in
/// their own folder so the data directory does not fill up with them. Falls
/// back to the temp directory if the platform data directory is unavailable,
/// so logging never blocks startup.
pub fn default_log_dir() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("logs")
}

/// Where the logs are being written right now.
pub fn log_dir() -> PathBuf {
    match OVERRIDE.lock() {
        Ok(guard) => guard.clone().unwrap_or_else(default_log_dir),
        Err(_) => default_log_dir(),
    }
}

/// Write the logs somewhere else from now on. `None` means the platform's own
/// folder again.
///
/// The folder is created and written to before the switch is made: a log path
/// that turns out to be unusable should fail here, where there is someone to
/// tell, rather than silently stop recording.
pub fn set_dir(dir: Option<PathBuf>) -> std::io::Result<PathBuf> {
    let target = dir.clone().unwrap_or_else(default_log_dir);
    std::fs::create_dir_all(&target)?;
    let probe = target.join(".accessengine-write-test");
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);

    if let Ok(mut guard) = OVERRIDE.lock() {
        *guard = dir;
    }
    reopen();
    log::info!("logging to {}", target.display());
    Ok(target)
}

/// Delete the log files and start a fresh one.
///
/// Only files this app named: the folder is one the user can open and keep
/// their own things in, and deleting something unrecognised there would be
/// the wrong call.
pub fn clear_logs() -> std::io::Result<usize> {
    let dir = log_dir();
    // Let go of the open file first. Today's log is usually one of the files
    // about to be deleted, and Windows will not delete a file that is open.
    if let Some(logger) = LOGGER.get() {
        if let Ok(mut guard) = logger.file.lock() {
            if let Some(writer) = guard.writer.as_mut() {
                let _ = writer.flush();
            }
            guard.writer = None;
        }
        if let Ok(mut tail) = logger.tail.lock() {
            tail.clear();
        }
    }

    let mut removed = 0usize;
    let mut failure: Option<std::io::Error> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().and_then(day_of_file).is_none() {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => failure = Some(e),
            }
        }
    }
    reopen();
    match failure {
        Some(e) => Err(e),
        None => Ok(removed),
    }
}

/// Close today's file and open it again, after the folder or its contents
/// changed underneath us.
fn reopen() {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    if let Ok(mut guard) = logger.file.lock() {
        if let Some(writer) = guard.writer.as_mut() {
            let _ = writer.flush();
        }
        *guard = DailyFile::open(now_utc().days);
    }
}

fn log_path_for(day: i64) -> PathBuf {
    log_dir().join(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", date_string(day)))
}

/// The file being written right now. Re-read it rather than caching: a session
/// running over midnight moves on to the next day's file.
pub fn log_path() -> PathBuf {
    match LOGGER.get().and_then(|l| l.file.lock().ok()) {
        Some(guard) => guard.path.clone(),
        None => log_path_for(now_utc().days),
    }
}

/// The date in a log filename, or `None` for anything else in the folder.
fn day_of_file(name: &str) -> Option<i64> {
    let rest = name.strip_prefix(FILE_PREFIX)?;
    // Accepts both `-2026-09-02.log` and the `.log.1` a size rotation leaves.
    let date = rest.strip_suffix(FILE_SUFFIX).or_else(|| {
        rest.strip_suffix(".1")
            .and_then(|r| r.strip_suffix(FILE_SUFFIX))
    })?;
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Delete logs older than [`KEEP_DAYS`]. Files that are not dated logs are left
/// alone: this runs over a directory the user can open, and deleting something
/// unrecognised there would be the wrong call.
fn prune_old_logs(dir: &Path, today: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(day_of_file) else {
            continue;
        };
        if today - day > KEEP_DAYS {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Earlier versions wrote a single `accessengine.log` beside the settings.
/// Move it into the logs folder under the day it was last written, so the
/// history survives the change and ages out with everything else.
fn migrate_single_file_log(dir: &Path) {
    let Some(old) = crate::config::data_dir().map(|d| d.join("accessengine.log")) else {
        return;
    };
    let Ok(meta) = std::fs::metadata(&old) else {
        return;
    };
    let day = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64).div_euclid(86_400))
        .unwrap_or_else(|| now_utc().days);

    let target = dir.join(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", date_string(day)));
    if !target.exists() {
        let _ = std::fs::rename(&old, &target);
    }
}

fn rotate_if_large(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() > MAX_LOG_BYTES {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// Install the logger. Call once, early in `main`.
///
/// Verbosity is `info` by default and `debug` when `ACCESSENGINE_DEBUG=1`;
/// `ACCESSENGINE_LOG` takes an explicit level (`error`/`warn`/`info`/`debug`/`trace`).
pub fn init() -> PathBuf {
    // The folder from the settings file, if it can be used; the platform's own
    // otherwise, because a log nobody can write is worse than one in a place
    // the user did not pick.
    if let Some(chosen) = crate::config::configured_log_dir() {
        match std::fs::create_dir_all(&chosen) {
            Ok(()) => {
                if let Ok(mut guard) = OVERRIDE.lock() {
                    *guard = Some(chosen);
                }
            }
            Err(e) => eprintln!(
                "accessengine: cannot use the configured log folder {}: {e}",
                chosen.display()
            ),
        }
    }

    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let today = now_utc().days;
    migrate_single_file_log(&dir);
    prune_old_logs(&dir, today);

    let level = std::env::var("ACCESSENGINE_LOG")
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or_else(|| {
            if std::env::var("ACCESSENGINE_DEBUG").is_ok_and(|v| v != "0") {
                LevelFilter::Debug
            } else {
                LevelFilter::Info
            }
        });

    let file = DailyFile::open(today);
    let path = file.path.clone();
    let opened = file.writer.is_some();

    let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
        file: Mutex::new(file),
        tail: Mutex::new(VecDeque::with_capacity(TAIL_CAPACITY)),
        // Only `trace` lifts the noise filter. Someone who sets
        // ACCESSENGINE_DEBUG=1 wants this app's debug lines, not six thousand
        // lines of wgpu adapter capabilities per minute.
        verbose: level >= LevelFilter::Trace,
    }));
    let _ = LOGGER.set(logger);

    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }

    if opened {
        log::info!(
            "AccessEngine {} starting on {} ({}); log level {level}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
    } else {
        eprintln!("accessengine: could not open log file at {}", path.display());
    }
    path
}

/// The most recent log lines, oldest first, for the Diagnostics panel.
pub fn tail(limit: usize) -> Vec<String> {
    let Some(logger) = LOGGER.get() else {
        return Vec::new();
    };
    let Ok(tail) = logger.tail.lock() else {
        return Vec::new();
    };
    let skip = tail.len().saturating_sub(limit);
    tail.iter().skip(skip).cloned().collect()
}

/// Hand a path to the desktop's default handler: a folder opens in the file
/// manager, a wordlist opens in the user's text editor.
pub fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    // `explorer` rather than `cmd /C start`: cmd re-parses its command line, so
    // a path containing `&` or a quote would be read as further commands. This
    // takes the path as a single argument and never involves a shell.
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();

    match result {
        Ok(_) => log::debug!("opened {}", path.display()),
        Err(e) => log::warn!("could not open {}: {e}", path.display()),
    }
}

/// Whether a string is plainly an http(s) URL.
fn is_web_address(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://") || url.starts_with("http://")
}

/// Open a URL in the default browser. Kept separate from [`open_path`] since
/// a URL is not a filesystem path, even though the commands involved happen
/// to be the same.
///
/// Only `http` and `https` are handed over. The one URL this opens arrives in
/// a GitHub API response, so it is not attacker-controlled in any ordinary
/// sense — but it is the one string here that comes off the network and goes
/// to a process launcher as its first argument, where a leading `-` would be
/// read as a flag rather than an address.
pub fn open_url(url: &str) {
    let url = url.trim();
    if !is_web_address(url) {
        log::warn!("refusing to open {url:?}: not an http(s) address");
        return;
    }
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    match result {
        Ok(_) => log::debug!("opened {url}"),
        Err(e) => log::warn!("could not open {url}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, to check the 400-year cycle handling.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn days_from_civil_is_the_inverse() {
        for day in [0_i64, 19_723, 19_782, 20_698, -1, -365] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn timestamp_is_iso8601_shaped() {
        let ts = now_utc().timestamp();
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.as_bytes()[10], b'T');
    }

    /// Anything that is not plainly an http(s) address must not reach the
    /// platform opener. The function has no return value, so this asserts on
    /// the classifier the guard uses rather than on the spawn.
    #[test]
    fn only_http_addresses_are_openable() {
        for url in ["https://example.com/x", "http://example.com"] {
            assert!(is_web_address(url), "{url}");
        }
        for url in [
            "-a/Calculator",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "",
            " ftp://example.com",
        ] {
            assert!(!is_web_address(url), "{url}");
        }
    }

    #[test]
    fn log_files_are_named_by_date() {
        let name = log_path_for(20_698);
        let name = name.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "accessengine-2026-09-02.log");
        assert_eq!(day_of_file(name), Some(20_698));
        // The size-rotated generation dates the same day.
        assert_eq!(day_of_file("accessengine-2026-09-02.log.1"), Some(20_698));
    }

    #[test]
    fn other_files_are_not_treated_as_logs() {
        assert_eq!(day_of_file("config.json"), None);
        assert_eq!(day_of_file("accessengine.log"), None);
        assert_eq!(day_of_file("accessengine-notadate.log"), None);
        assert_eq!(day_of_file("accessengine-2026-13-02.log"), None);
        assert_eq!(day_of_file("accessengine-2026-09-02.log.bak"), None);
    }

    #[test]
    fn pruning_keeps_recent_logs_and_leaves_strangers_alone() {
        let dir = std::env::temp_dir().join("accessengine-prune-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let today = 20_698;
        let recent = dir.join(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", date_string(today - 1)));
        let old = dir.join(format!(
            "{FILE_PREFIX}{}{FILE_SUFFIX}",
            date_string(today - KEEP_DAYS - 1)
        ));
        let stranger = dir.join("notes.txt");
        for path in [&recent, &old, &stranger] {
            std::fs::write(path, "x").unwrap();
        }

        prune_old_logs(&dir, today);

        assert!(recent.exists(), "a log inside the window should survive");
        assert!(!old.exists(), "a log past the window should be deleted");
        assert!(stranger.exists(), "unrelated files must be left alone");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

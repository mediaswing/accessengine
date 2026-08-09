//! ElevenLabs API key storage.
//!
//! The key is a secret, so it never goes in the JSON config file, and it is
//! never passed on a command line where another process could read it out of
//! the process list — it goes over stdin in both directions.
//!
//! * **macOS**: the login keychain, reached through the `security` CLI so the
//!   app doesn't have to link against Security.framework.
//! * **Windows**: DPAPI, through PowerShell's `ConvertFrom-SecureString`. The
//!   ciphertext sits in the app's own config directory and can only be read
//!   back by the same Windows account on the same machine.
//!
//! An `ELEVENLABS_API_KEY` environment variable always wins, which keeps CI and
//! one-off runs simple and stores nothing at all.

use anyhow::{Result, bail};
// Every platform-specific path needs these; a platform with no backend at all
// needs none of them.
#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::Context as _;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::Write as _;
// Windows launches PowerShell through `sysexec`, which builds the `Command`
// itself; only the macOS path constructs one here.
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Stdio;

/// The keychain tool, by absolute path rather than by name.
///
/// This is the process the key is handed to over stdin, so which binary
/// actually receives it must not be a question `PATH` gets to answer — a
/// `security` earlier on the path would be handed the key and the app would
/// never know. Every other program this app runs on macOS is named the same
/// way: `/usr/bin/say`, `/usr/bin/sips`, `/usr/bin/which`.
#[cfg(target_os = "macos")]
const SECURITY: &str = "/usr/bin/security";

/// The macOS keychain item. Deliberately still the old name: renaming it would
/// strand the key of anyone who saved one before the app was called accessengine.
#[cfg(target_os = "macos")]
const SERVICE: &str = "speech-output-engine";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "elevenlabs";
pub const ENV_VAR: &str = "ELEVENLABS_API_KEY";

/// How the API key dialog describes where the key will be kept. Platform-
/// specific because "your login keychain" means nothing on Windows.
#[cfg(target_os = "macos")]
pub const STORAGE_DESCRIPTION: &str = "in your login keychain";
#[cfg(target_os = "windows")]
pub const STORAGE_DESCRIPTION: &str = "encrypted for your Windows account";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const STORAGE_DESCRIPTION: &str = "in this computer's secure storage";

/// Where the key currently in use came from, so the UI can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    /// Saved by the app, wherever this platform saves secrets.
    Keychain,
    None,
}

/// Reads the key from the environment, then from storage.
pub fn load() -> (Option<String>, KeySource) {
    if let Ok(key) = std::env::var(ENV_VAR) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            crate::log::line(format!("keychain: using the key from {ENV_VAR}"));
            return (Some(key), KeySource::Env);
        }
    }
    match read() {
        Ok(Some(key)) => {
            crate::log::line("keychain: loaded a saved key");
            (Some(key), KeySource::Keychain)
        }
        // A missing key is the normal first-run state, and a storage backend
        // that is broken shouldn't stop the app opening — the system voices
        // don't need a key at all. The two are worth telling apart in the log,
        // since only one of them is a problem.
        Ok(None) => {
            crate::log::line("keychain: no saved key found");
            (None, KeySource::None)
        }
        Err(error) => {
            crate::log::line(format!("keychain: could not be read — {error:#}"));
            (None, KeySource::None)
        }
    }
}

// --------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
fn read() -> Result<Option<String>> {
    read_item(SERVICE, ACCOUNT)
}

/// The service and account are parameters so the round-trip test can use a
/// scratch item instead of the one holding the user's real key.
#[cfg(target_os = "macos")]
fn read_item(service: &str, account: &str) -> Result<Option<String>> {
    let out = Command::new(SECURITY)
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("failed to run `security`")?;
    if !out.status.success() {
        // Exit code 44 is "item not found", which is a normal first-run state.
        return Ok(None);
    }
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if key.is_empty() { None } else { Some(key) })
}

/// Stores the key in the login keychain, replacing any previous value.
///
/// `-w` as the last option makes `security` prompt for the password rather than
/// take it as an argument, which keeps the key out of the process list. What
/// that prompt actually wants is the password *twice* — "password data for new
/// item:" and then "retype password for new item:" — so the key is written
/// twice. Sending it once is the bug this comment exists to prevent coming
/// back: `security` reads the lone value, hits EOF on the retype, decides the
/// two don't match, and then stores an item with an **empty password while
/// still exiting 0**. Nothing looks wrong until the next launch reads the key
/// back and finds nothing there.
#[cfg(target_os = "macos")]
pub fn store(key: &str) -> Result<()> {
    store_item(SERVICE, ACCOUNT, key)?;
    verify_stored(key)
}

#[cfg(target_os = "macos")]
fn store_item(service: &str, account: &str, key: &str) -> Result<()> {
    let mut child = Command::new(SECURITY)
        .args([
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-U", // update if it already exists
            "-w",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run `security`")?;
    child
        .stdin
        .take()
        .context("could not pass the key to `security`")?
        .write_all(format!("{key}\n{key}\n").as_bytes())
        .context("could not pass the key to `security`")?;

    let out = child
        .wait_with_output()
        .context("`security` did not finish cleanly")?;
    if !out.status.success() {
        bail!(
            "could not save the key to the keychain: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Reads the key straight back and checks it arrived intact.
///
/// `security` exits 0 on the failure that matters here — an item written with
/// an empty password — so its exit status is not enough on its own to report
/// "API key saved" to someone who will not find out otherwise until the next
/// time they open the app.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn verify_stored(key: &str) -> Result<()> {
    match read() {
        Ok(Some(stored)) if stored == key => {
            crate::log::line("keychain: key saved and read back intact");
            Ok(())
        }
        Ok(Some(_)) => {
            crate::log::line("keychain: the key read back did not match the one saved");
            bail!("the key was not stored correctly — please try entering it again")
        }
        Ok(None) => {
            // The exact failure this check exists for: storage reported
            // success and kept nothing.
            crate::log::line("keychain: storage reported success but saved nothing");
            bail!("the key was not stored correctly — please try entering it again")
        }
        Err(error) => {
            crate::log::line(format!("keychain: could not read the key back — {error:#}"));
            Err(error.context("the key could not be read back after saving"))
        }
    }
}

/// Removes the stored key. Succeeds if there was nothing to remove.
#[cfg(target_os = "macos")]
pub fn clear() -> Result<()> {
    Command::new(SECURITY)
        .args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])
        .output()
        .context("failed to run `security`")?;
    Ok(())
}

// ------------------------------------------------------------------- Windows

/// The DPAPI ciphertext file. Alongside the config, because it is per-user data
/// in exactly the same sense — it is just the part that is encrypted.
#[cfg(target_os = "windows")]
fn credential_path() -> Result<std::path::PathBuf> {
    let config = crate::config::Config::path().context("could not locate a config directory")?;
    let dir = config
        .parent()
        .context("could not locate a config directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir.join("elevenlabs.dpapi"))
}

/// Runs a PowerShell script with no window, handing it `stdin_text` if given
/// and returning its standard output.
///
/// The interpreter is located by absolute path — see [`crate::sysexec`]. That
/// matters more here than anywhere else in the app: this is the process the API
/// key is handed to in plaintext, so letting `PATH` or the executable's own
/// directory decide which binary receives it would be handing the key to
/// whoever won that search.
#[cfg(target_os = "windows")]
fn powershell(script: &str, stdin_text: Option<&str>) -> Result<std::process::Output> {
    let mut child = crate::sysexec::powershell(script)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run PowerShell")?;

    if let Some(text) = stdin_text {
        child
            .stdin
            .take()
            .context("could not pass the key to PowerShell")?
            .write_all(text.as_bytes())
            .context("could not pass the key to PowerShell")?;
    }
    child
        .wait_with_output()
        .context("PowerShell did not finish cleanly")
}

#[cfg(target_os = "windows")]
use crate::sysexec::ps_quote;

#[cfg(target_os = "windows")]
fn read() -> Result<Option<String>> {
    read_at(&credential_path()?)
}

/// The path is a parameter so the round-trip test can use a scratch file
/// instead of the one holding the user's real key.
///
/// Both `InputEncoding` and `OutputEncoding` are pinned to UTF-8 before
/// touching the console streams. Left alone, `[Console]::In`/`[Console]::Out`
/// default to the system's legacy OEM code page whenever the process has no
/// real console attached — exactly the case here, since PowerShell is always
/// launched with `CREATE_NO_WINDOW` — so any non-ASCII character in the key
/// comes back as the wrong character, or several. That doesn't fail loudly:
/// `verify_stored` reads a mismatched key back and reports "the key was not
/// stored correctly", which is confusing when DPAPI actually stored it fine.
#[cfg(target_os = "windows")]
fn read_at(path: &std::path::Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    // `ConvertTo-SecureString` on a DPAPI blob only succeeds for the account
    // that wrote it, which is the whole point of storing it this way.
    let script = format!(
        "\
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$blob = (Get-Content -LiteralPath {} -Raw).Trim()
$secure = ConvertTo-SecureString -String $blob
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try {{ [Console]::Out.Write([Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)) }}
finally {{ [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }}",
        ps_quote(&path.to_string_lossy())
    );

    let out = powershell(&script, None)?;
    if !out.status.success() {
        return Ok(None);
    }
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if key.is_empty() { None } else { Some(key) })
}

/// Encrypts the key with DPAPI and writes it, replacing any previous value.
#[cfg(target_os = "windows")]
pub fn store(key: &str) -> Result<()> {
    store_at(&credential_path()?, key)?;
    verify_stored(key)
}

/// See [`read_at`] for why `InputEncoding` is set before the key is read off
/// the console stream — without it, a non-ASCII character sent over stdin
/// arrives as the wrong character.
#[cfg(target_os = "windows")]
fn store_at(path: &std::path::Path, key: &str) -> Result<()> {
    let script = format!(
        "\
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.Encoding]::UTF8
$plain = [Console]::In.ReadToEnd().Trim()
$secure = ConvertTo-SecureString -String $plain -AsPlainText -Force
ConvertFrom-SecureString -SecureString $secure | Set-Content -LiteralPath {} -Encoding ASCII",
        ps_quote(&path.to_string_lossy())
    );

    let out = powershell(&script, Some(key))?;
    if !out.status.success() {
        bail!(
            "could not save the key: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Removes the stored key. Succeeds if there was nothing to remove.
#[cfg(target_os = "windows")]
pub fn clear() -> Result<()> {
    let path = credential_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

// ------------------------------------------------------- everywhere else

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn store(_key: &str) -> Result<()> {
    bail!(
        "this system has no supported secure storage, so the key can only be given \
         in the {ENV_VAR} environment variable"
    )
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clear() -> Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// A scratch item, so the test never touches the one holding a real key.
    const TEST_SERVICE: &str = "accessengine-keychain-test";
    const TEST_ACCOUNT: &str = "roundtrip";

    /// Stores a key and reads it straight back.
    ///
    /// The bug this exists for stored an item with an *empty password* and
    /// exited 0, so the app said "API key saved" and the key was gone by the
    /// next launch. Asserting on the exit status alone would have passed
    /// throughout; only reading the value back catches it.
    #[test]
    fn a_stored_key_reads_back_unchanged() {
        let key = "sk_test_0123456789abcdef";

        store_item(TEST_SERVICE, TEST_ACCOUNT, key).expect("the key should store");
        let read_back = read_item(TEST_SERVICE, TEST_ACCOUNT);

        // Removed before asserting, so a failure doesn't leave the item behind.
        let _ = Command::new(SECURITY)
            .args([
                "delete-generic-password",
                "-s",
                TEST_SERVICE,
                "-a",
                TEST_ACCOUNT,
            ])
            .output();

        assert_eq!(
            read_back.expect("reading back should succeed").as_deref(),
            Some(key),
            "the key did not survive the round trip"
        );
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    fn scratch_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Skipped on the GitHub Actions Windows runner: confirmed over three
    /// separate `windows-latest` release runs that `ConvertTo-SecureString`
    /// fails there with "the module could not be loaded" —
    /// `CouldNotAutoloadMatchingModule` — every single time, serialized
    /// against every other test or not. That rules out a race with this
    /// crate's own tests; it is the runner image itself that cannot load
    /// `Microsoft.PowerShell.Security`, the same way it has no audio device
    /// for `tts::system::tests::speaking_spawns_a_killable_process`. A real
    /// Windows machine — including this one — runs both tests and passes.
    fn skip_on_windows_ci() -> bool {
        std::env::var_os("CI").is_some()
    }

    #[test]
    fn a_stored_key_reads_back_unchanged() {
        if skip_on_windows_ci() {
            return;
        }
        let path = scratch_path("accessengine-keychain-test-ascii.dpapi");
        let key = "sk_test_0123456789abcdef";

        store_at(&path, key).expect("the key should store");
        let read_back = read_at(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            read_back.expect("reading back should succeed").as_deref(),
            Some(key),
            "the key did not survive the round trip"
        );
    }

    /// The bug this exists for: `[Console]::In`/`[Console]::Out` default to
    /// the system's legacy OEM code page rather than UTF-8 whenever the
    /// process has no real console attached, which silently mangled any
    /// non-ASCII character sent across either stream. Real ElevenLabs keys
    /// are plain ASCII, but a key pasted from a formatted web page can carry
    /// stray Unicode (curly quotes, a non-breaking space in the middle), and
    /// this is the case that caught it: a key that round-trips correctly only
    /// if the console streams are read and written as UTF-8.
    #[test]
    fn a_key_with_non_ascii_characters_reads_back_unchanged() {
        if skip_on_windows_ci() {
            return;
        }
        let path = scratch_path("accessengine-keychain-test-unicode.dpapi");
        let key = "sk_café_日本語_0123456789";

        store_at(&path, key).expect("the key should store");
        let read_back = read_at(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            read_back.expect("reading back should succeed").as_deref(),
            Some(key),
            "a non-ASCII key did not survive the round trip"
        );
    }
}

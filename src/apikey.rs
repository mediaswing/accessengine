//! ElevenLabs API key storage.
//!
//! The key is kept in a small file of its own next to the JSON config, in the
//! directory the platform sets aside for this app's per-user settings —
//! `%APPDATA%` on Windows, `~/Library/Application Support` on macOS.
//!
//! **The key is stored in plain text.** That is a deliberate trade, and this is
//! the reasoning, because it is the kind of decision that gets quietly
//! "improved" back into the bug it replaced:
//!
//! The app used to put the key in the macOS login keychain, via
//! `security add-generic-password -w`, and in a DPAPI blob on Windows, via
//! PowerShell. Both worked by handing the key to another program over stdin,
//! and both had a failure mode that produced a *silent* wrong answer rather
//! than an error. `security -w` does not read the password from stdin at all —
//! it reads it from the controlling terminal with `readpassphrase()`. Launched
//! from a terminal, it therefore ignored the piped key, sat there prompting on
//! a window the user could not see was waiting, and stored whatever they
//! eventually typed. The Windows side needed the console encoding pinned to
//! UTF-8 on both streams to stop non-ASCII characters coming back mangled, and
//! could not load `Microsoft.PowerShell.Security` at all on some machines.
//!
//! A plaintext file is readable by anything already running as this user. So is
//! a DPAPI blob, which decrypts for exactly that user; and the keychain item
//! was written with an ACL that let `security` read it back without a prompt,
//! so it too was one subprocess away. The security those backends bought over a
//! 0600 file in the user's own profile was thin, and the reliability they cost
//! was not.
//!
//! An `ELEVENLABS_API_KEY` environment variable always wins, which keeps CI and
//! one-off runs simple and stores nothing at all.

use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;

pub const ENV_VAR: &str = "ELEVENLABS_API_KEY";

/// How the API key dialog describes where the key will be kept.
pub const STORAGE_DESCRIPTION: &str = "in this app's settings folder";

/// Where the key currently in use came from, so the UI can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    /// Saved by the app, in the file below.
    Stored,
    None,
}

/// The key file. Kept separate from `config.json` rather than made a field of
/// it, for two reasons: the config is rewritten every time any setting changes,
/// and it is the file to ask someone to paste when something goes wrong.
/// Neither is a thing to do to a secret.
fn path() -> Result<PathBuf> {
    let config = crate::config::Config::path().context("could not locate a config directory")?;
    let dir = config
        .parent()
        .context("could not locate a config directory")?;
    Ok(dir.join("elevenlabs.key"))
}

/// Reads the key from the environment, then from the file.
pub fn load() -> (Option<String>, KeySource) {
    if let Ok(key) = std::env::var(ENV_VAR) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            crate::log::line(format!("api key: using the key from {ENV_VAR}"));
            return (Some(key), KeySource::Env);
        }
    }
    match read() {
        Ok(Some(key)) => {
            crate::log::line("api key: loaded the saved key");
            (Some(key), KeySource::Stored)
        }
        Ok(None) => match legacy::migrate() {
            Ok(Some(key)) => {
                crate::log::line("api key: moved the key out of the old secure storage");
                (Some(key), KeySource::Stored)
            }
            Ok(None) => {
                crate::log::line("api key: no saved key found");
                (None, KeySource::None)
            }
            Err(error) => {
                crate::log::line(format!(
                    "api key: the old storage could not be read — {error:#}"
                ));
                (None, KeySource::None)
            }
        },
        // A key file that cannot be read shouldn't stop the app opening — the
        // system voices need no key at all.
        Err(error) => {
            crate::log::line(format!("api key: could not be read — {error:#}"));
            (None, KeySource::None)
        }
    }
}

fn read() -> Result<Option<String>> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let key = text.trim().to_string();
            Ok(if key.is_empty() { None } else { Some(key) })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Writes the key, replacing any previous value, and reads it straight back.
///
/// The read-back is not ceremony. The failure this whole module was rewritten
/// to escape was storage that *reported success and kept nothing*, and the
/// person who finds that out is the user, on their next launch, long after the
/// app told them the key was saved. Confirming it is on disk costs a few
/// microseconds.
pub fn store(key: &str) -> Result<()> {
    let path = path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    write_private(&path, key)?;

    match read() {
        Ok(Some(stored)) if stored == key => {
            crate::log::line("api key: saved and read back intact");
            Ok(())
        }
        Ok(_) => {
            crate::log::line("api key: the file did not contain the key after saving");
            bail!("the key was not stored correctly — please try entering it again")
        }
        Err(error) => {
            crate::log::line(format!("api key: could not read the key back — {error:#}"));
            Err(error.context("the key could not be read back after saving"))
        }
    }
}

/// Writes `key` to `path` so that only this user can read it.
///
/// Written to a temporary file and renamed, so a failure part-way through
/// leaves the previous key intact rather than a truncated one — and on Unix the
/// mode is set by `OpenOptions` as the file is created, so there is never an
/// instant where the key exists as a world-readable file.
fn write_private(path: &std::path::Path, key: &str) -> Result<()> {
    use std::io::Write as _;

    let temporary = temporary_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    // One closure so the three fallible steps share a single `?` and one error
    // message, rather than repeating the same context three times.
    let write = || -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(key.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()
    };
    write().with_context(|| format!("could not write {}", temporary.display()))?;

    if let Err(error) = std::fs::rename(&temporary, path) {
        // Otherwise a failed rename leaves the key sitting in a second file
        // nobody ever reads or deletes.
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("could not write {}", path.display()));
    }
    Ok(())
}

/// The name the key is written under before being renamed into place: the same
/// name with `.tmp` on the end. A function rather than `with_extension`, which
/// would have to be told the real extension to keep and would quietly write
/// somewhere else if the file were ever renamed.
fn temporary_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Removes the stored key. Succeeds if there was nothing to remove.
///
/// Clears the old secure storage too, so "Remove Saved Key" removes the key
/// rather than removing the copy and leaving one behind to be migrated back in
/// at the next launch.
pub fn clear() -> Result<()> {
    legacy::clear();
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

/// Reading — never writing — the secure storage releases up to 1.2.5 used, so
/// that upgrading does not silently lose a key somebody already saved.
///
/// This is one-shot: [`migrate`] copies the key into the file above and deletes
/// the old item, so the slow, fragile path runs once and then never again.
mod legacy {
    use super::*;

    /// Moves a key out of the old storage into the new file, if there is one.
    pub fn migrate() -> Result<Option<String>> {
        let Some(key) = read()? else {
            return Ok(None);
        };
        // Order matters: only forget the old copy once the new one is on disk
        // and verified. A crash between the two costs a duplicate, not the key.
        super::store(&key)?;
        clear();
        Ok(Some(key))
    }

    // --------------------------------------------------------------- macOS

    /// The keychain item, under the name the app had when it wrote it.
    #[cfg(target_os = "macos")]
    const SERVICE: &str = "speech-output-engine";
    #[cfg(target_os = "macos")]
    const ACCOUNT: &str = "elevenlabs";
    /// By absolute path, not by name — `PATH` does not get to decide which
    /// binary is asked for the key. See [`crate::sysexec`].
    #[cfg(target_os = "macos")]
    const SECURITY: &str = "/usr/bin/security";

    #[cfg(target_os = "macos")]
    fn read() -> Result<Option<String>> {
        let out = std::process::Command::new(SECURITY)
            .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
            .output()
            .context("failed to run `security`")?;
        if !out.status.success() {
            // Exit code 44 is "item not found" — the normal case from here on.
            return Ok(None);
        }
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if key.is_empty() { None } else { Some(key) })
    }

    #[cfg(target_os = "macos")]
    pub fn clear() {
        let _ = std::process::Command::new(SECURITY)
            .args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])
            .output();
    }

    // ------------------------------------------------------------- Windows

    /// The DPAPI ciphertext file, written next to the config.
    #[cfg(target_os = "windows")]
    fn credential_path() -> Result<PathBuf> {
        Ok(super::path()?.with_file_name("elevenlabs.dpapi"))
    }

    /// `ConvertTo-SecureString` on a DPAPI blob only succeeds for the account
    /// that wrote it, which was the point of storing it this way.
    ///
    /// `OutputEncoding` is pinned to UTF-8 before the console stream is
    /// touched: with no console attached — PowerShell is launched with
    /// `CREATE_NO_WINDOW` — `[Console]::Out` otherwise defaults to the legacy
    /// OEM code page and mangles any non-ASCII character in the key.
    #[cfg(target_os = "windows")]
    fn read() -> Result<Option<String>> {
        use std::process::Stdio;

        let path = credential_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let script = format!(
            "\
$ErrorActionPreference = 'Stop'
Import-Module Microsoft.PowerShell.Security
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$blob = (Get-Content -LiteralPath {} -Raw).Trim()
$secure = ConvertTo-SecureString -String $blob
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try {{ [Console]::Out.Write([Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)) }}
finally {{ [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }}",
            crate::sysexec::ps_quote(&path.to_string_lossy())
        );

        let out = crate::sysexec::powershell(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .context("failed to run PowerShell")?;
        if !out.status.success() {
            return Ok(None);
        }
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if key.is_empty() { None } else { Some(key) })
    }

    #[cfg(target_os = "windows")]
    pub fn clear() {
        if let Ok(path) = credential_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    // ------------------------------------------------------ everywhere else

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn read() -> Result<Option<String>> {
        Ok(None)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn clear() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    /// The round trip the old backends kept failing: a key that goes in comes
    /// back out, character for character.
    #[test]
    fn a_stored_key_reads_back_unchanged() {
        let path = scratch("accessengine-key-roundtrip.key");
        let key = "sk_test_0123456789abcdef";

        write_private(&path, key).expect("the key should write");
        let read_back = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(read_back.expect("reading back should succeed").trim(), key);
    }

    /// A key pasted from a formatted web page can carry stray Unicode, and the
    /// Windows backend this replaced silently mangled it into a key that looked
    /// stored and did not work.
    #[test]
    fn a_key_with_non_ascii_characters_reads_back_unchanged() {
        let path = scratch("accessengine-key-unicode.key");
        let key = "sk_café_日本語_0123456789";

        write_private(&path, key).expect("the key should write");
        let read_back = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(read_back.expect("reading back should succeed").trim(), key);
    }

    /// Writing over an existing key replaces it, and leaves no `.tmp` file
    /// holding the previous one behind.
    #[test]
    fn writing_replaces_a_previous_key_and_leaves_no_temporary_file() {
        let path = scratch("accessengine-key-replace.key");

        write_private(&path, "sk_first").expect("the first key should write");
        write_private(&path, "sk_second").expect("the second key should write");
        let read_back = std::fs::read_to_string(&path);
        let temporary_exists = temporary_path(&path).exists();
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            read_back.expect("reading back should succeed").trim(),
            "sk_second"
        );
        assert!(
            !temporary_exists,
            "the temporary file should have been renamed away"
        );
    }

    /// The file holds a secret, so it must not be readable by other accounts on
    /// a shared machine.
    #[cfg(unix)]
    #[test]
    fn the_key_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = scratch("accessengine-key-mode.key");
        write_private(&path, "sk_test").expect("the key should write");
        let mode = std::fs::metadata(&path)
            .expect("the file should exist")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_file(&path);

        assert_eq!(mode, 0o600, "the key file should be owner-read/write only");
    }
}

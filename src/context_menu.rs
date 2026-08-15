//! Registers (and unregisters) a Windows Explorer right-click "Speak to file"
//! entry, and keeps a durable copy of the running app for it to point at.
//!
//! The app ships as a single portable `.exe` with no installer, which people
//! run straight out of `Downloads` and often delete afterwards. A right-click
//! entry that pointed at that exe would stop working the moment that happens,
//! so "installing" the entry also copies the running exe into this app's own
//! settings folder and registers the command against *that* copy — see
//! [`install`].
//!
//! Everything here is `HKEY_CURRENT_USER` only: no admin privileges needed,
//! and nothing outside this user's own profile is touched.
//!
//! Windows only. Every other platform gets the same function signatures
//! reporting "not installed" and refusing to install, the same convention
//! [`crate::ollama::package_manager`] uses for winget/Homebrew — so nothing
//! that calls this module needs its own `#[cfg]`.

use crate::extract;
use std::path::PathBuf;

/// Extensions the right-click entry covers.
///
/// Deliberately not [`extract::IMAGE_EXTENSIONS`] or [`extract::VIDEO_EXTENSIONS`]:
/// those go through Ollama/ffmpeg and can run for minutes, and a headless run
/// has no progress bar or cancel button to show for it. Built from the same
/// lists the Open dialog uses, so this can never cover a file type
/// [`extract::extract_document`] doesn't actually handle.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn extensions() -> Vec<&'static str> {
    [
        extract::TEXT_EXTENSIONS,
        extract::DOC_EXTENSIONS,
        extract::TABLE_EXTENSIONS,
    ]
    .concat()
}

/// Where the app's own copy lives once installed, whether or not it exists
/// yet.
pub fn install_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("io", "accessengine", "accessengine")?;
    Some(dirs.data_dir().join("accessengine.exe"))
}

pub use imp::{install, is_installed, uninstall};

#[cfg(target_os = "windows")]
mod imp {
    use super::{extensions, install_path};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use winreg::HKCU;

    /// The verb Explorer shows in the right-click menu, and the registry key
    /// it lives under. Defined here rather than beside [`extensions`] so the
    /// platforms without a registry are not left holding two unused
    /// constants.
    const VERB: &str = "SpeakToFile";
    const VERB_LABEL: &str = "Speak to file";

    /// `SystemFileAssociations` is the key Explorer offers precisely for
    /// adding a verb to a file type without becoming its default handler —
    /// unlike registering directly under `.ext`, nothing already associated
    /// with these extensions is disturbed.
    fn verb_key_path(ext: &str) -> String {
        format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\{VERB}")
    }

    fn command_line(exe: &Path) -> String {
        format!(r#""{}" --speak-to-file "%1""#, exe.display())
    }

    pub fn is_installed() -> bool {
        extensions()
            .iter()
            .all(|ext| HKCU.open_subkey(verb_key_path(ext)).is_ok())
    }

    /// Copies the running exe into [`install_path`] (skipped if it is
    /// already running from there) and points the right-click entry at that
    /// copy for every extension in [`extensions`].
    ///
    /// Safe to call again later: the copy is refreshed to whatever is
    /// currently running, and the registry entries are overwritten rather
    /// than duplicated.
    pub fn install() -> Result<PathBuf> {
        let target =
            install_path().context("could not locate a settings folder to install the app into")?;
        let running =
            std::env::current_exe().context("could not locate the running application")?;

        // `target` may not exist yet, in which case its `canonicalize()` is
        // `Err` and this is simply `false` — nothing installed to compare to.
        let already_there = running.canonicalize().ok() == target.canonicalize().ok();
        if !already_there {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            std::fs::copy(&running, &target).with_context(|| {
                format!(
                    "could not copy the app to {} — it may be locked by another running copy",
                    target.display()
                )
            })?;
        }

        for ext in extensions() {
            let (verb, _) = HKCU
                .create_subkey(verb_key_path(ext))
                .with_context(|| format!("could not add a right-click entry for .{ext} files"))?;
            verb.set_value("", &VERB_LABEL).with_context(|| {
                format!("could not name the right-click entry for .{ext} files")
            })?;
            let (command, _) = verb
                .create_subkey("command")
                .with_context(|| format!("could not add a right-click entry for .{ext} files"))?;
            command
                .set_value("", &command_line(&target))
                .with_context(|| {
                    format!("could not point the right-click entry for .{ext} files at the app")
                })?;
        }
        Ok(target)
    }

    /// Removes the registry entries. The copied exe, if any, is left in
    /// place — it is harmless on its own, and may be the very process this
    /// call is running from.
    pub fn uninstall() -> Result<()> {
        for ext in extensions() {
            match HKCU.delete_subkey_all(verb_key_path(ext)) {
                Ok(()) => {}
                // Already off is not a failure to turn off.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not remove the right-click entry for .{ext} files")
                    });
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_verb_lives_under_system_file_associations_not_the_extension_itself() {
            let path = verb_key_path("txt");
            // Registering here, rather than directly under `.txt`, is what
            // keeps this from becoming — or disturbing — the default handler.
            assert!(path.contains("SystemFileAssociations"));
            assert!(path.ends_with(&format!(r".txt\shell\{VERB}")));
        }

        #[test]
        fn the_command_quotes_the_path_and_forwards_the_clicked_file() {
            let line = command_line(Path::new(r"C:\Users\a b\accessengine.exe"));
            assert_eq!(
                line,
                r#""C:\Users\a b\accessengine.exe" --speak-to-file "%1""#
            );
        }

        /// The real thing, against the real `HKEY_CURRENT_USER` and the real
        /// `%APPDATA%`: install, check every extension actually registered
        /// and points at a copied exe that runs, then uninstall and check
        /// it's gone.
        ///
        /// Ignored by default since it writes to the real registry and
        /// filesystem rather than a sandbox — this crate has no way to fake
        /// either. Run it with:
        ///     cargo test context_menu -- --ignored --nocapture
        #[test]
        #[ignore = "writes to the real registry and %APPDATA%"]
        fn install_registers_every_extension_and_uninstall_removes_them() {
            assert!(
                !is_installed(),
                "a previous run left the right-click menu installed"
            );

            let target = install().expect("install should succeed");
            assert!(target.exists(), "the app was not copied to {target:?}");
            assert!(is_installed());

            for ext in extensions() {
                let command: String = HKCU
                    .open_subkey(format!("{}\\command", verb_key_path(ext)))
                    .and_then(|key| key.get_value(""))
                    .unwrap_or_else(|_| panic!("no command registered for .{ext}"));
                assert!(command.contains(&target.display().to_string()), "{command}");
                assert!(command.contains("--speak-to-file"), "{command}");
            }

            uninstall().expect("uninstall should succeed");
            assert!(!is_installed());
            assert!(
                target.exists(),
                "uninstall should leave the copied exe in place"
            );
            std::fs::remove_file(&target).ok();
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use anyhow::{Result, bail};
    use std::path::PathBuf;

    pub fn is_installed() -> bool {
        false
    }

    pub fn install() -> Result<PathBuf> {
        bail!("the right-click menu is only available on Windows")
    }

    pub fn uninstall() -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_covered_extensions_match_what_extract_document_actually_reads() {
        let covered = extensions();
        for ext in ["txt", "text", "md", "markdown", "log", "docx", "csv", "tsv"] {
            assert!(covered.contains(&ext), "{ext} is missing from {covered:?}");
        }
        // Deliberately excluded — see the doc comment on `extensions`.
        for ext in ["jpg", "png", "heic", "mp4", "mov"] {
            assert!(!covered.contains(&ext), "{ext} should not be covered");
        }
    }
}

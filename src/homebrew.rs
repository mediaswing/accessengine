//! Getting Homebrew itself installed, when Ollama's own installer needs it
//! and it isn't there yet.
//!
//! Homebrew's own installer needs `sudo`: to create its install prefix
//! (`/opt/homebrew` on Apple Silicon, `/usr/local` on Intel), and on a Mac
//! that hasn't got them, to install the Command Line Tools. On macOS it checks
//! for that access up front, whether or not the prefix already belongs to the
//! user — so there is no arrangement this app can make beforehand that would
//! let the installer run without a password at all.
//!
//! What there is, is `SUDO_ASKPASS`: the installer explicitly supports it, and
//! `sudo` runs the program it names whenever it needs a password instead of
//! reading a terminal that a GUI app hasn't got. So rather than hand the whole
//! thing off to a Terminal window, this app writes a small askpass helper that
//! asks with a macOS dialog, and otherwise runs Homebrew's own installer
//! exactly like [`crate::ollama::install`] runs `brew install ollama`:
//! unattended, streamed a line at a time, with no window of its own at all.
//!
//! The password goes from that dialog to `sudo` and nowhere else. This process
//! never sees it, and the helper — which holds no secret, only the dialog —
//! is deleted as soon as the install finishes. `sudo` remembers the
//! authorisation for a few minutes, so one dialog usually covers the whole
//! install; a long Command Line Tools download can outlast that and ask again.

#[cfg(not(target_os = "windows"))]
use std::path::PathBuf;

/// Where Homebrew installs `brew` itself: `/opt/homebrew` on Apple Silicon,
/// `/usr/local` on Intel. Only ever consulted on macOS, by `ollama`'s own
/// package-manager detection.
#[cfg(not(target_os = "windows"))]
pub fn path() -> Option<PathBuf> {
    ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

/// Shown to the user before anything runs, because installing software is not
/// something to do by surprise. This is Homebrew's own documented install
/// command; see <https://brew.sh>. The app downloads and runs that same
/// script itself rather than shelling out to `curl`.
pub const INSTALL_COMMAND: &str = r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#;

pub use platform::install;

// --------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result, bail};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// The script [`super::INSTALL_COMMAND`] pipes into `bash`.
    const INSTALLER_URL: &str =
        "https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh";

    /// Downloads Homebrew's installer.
    ///
    /// Fetched here rather than by a `curl` subprocess so that a network
    /// failure is reported as one, instead of arriving as an empty script that
    /// `bash` runs to no effect. For the same reason the result is checked to
    /// look like a shell script at all: a captive portal or an intercepting
    /// proxy answers with a page of HTML, and that is not something to hand to
    /// a shell.
    fn fetch_installer() -> Result<String> {
        let script = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("speech-output-engine/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not create an HTTP client")?
            .get(INSTALLER_URL)
            .send()
            .context("could not download Homebrew's installer")?
            .error_for_status()
            .context("Homebrew's installer could not be downloaded")?
            .text()
            .context("Homebrew's installer could not be read")?;

        if !script.starts_with("#!") {
            bail!(
                "what came back from {INSTALLER_URL} is not Homebrew's installer. \
                 Check the network connection and try again."
            );
        }
        Ok(script)
    }

    /// The helper `sudo` runs to ask for a password, and the file it reports a
    /// cancelled dialog through.
    ///
    /// Both are deleted when this is dropped, however the install ends.
    struct Askpass {
        script: PathBuf,
        outcome: PathBuf,
    }

    /// What the helper writes to its outcome file when the user dismisses the
    /// dialog, so that "you cancelled" can be told apart from "the password
    /// was wrong" — `sudo` reports both the same way.
    const CANCELLED: &str = "cancelled";

    /// The helper's text. Built as a function so the tests can exercise the
    /// script itself; `outcome` is interpolated into single quotes, and is
    /// always a path this process just made in the temporary directory.
    fn askpass_script(outcome: &str) -> String {
        format!(
            r#"#!/bin/bash
# Written by Speech Output Engine so that macOS's own sudo has somewhere to ask
# for a password during the Homebrew install, and deleted as soon as that
# install finishes. The password is printed straight to sudo, which is the only
# thing that reads it; nothing here writes it anywhere.
if ! password="$(/usr/bin/osascript <<'APPLESCRIPT'
display dialog "Speech Output Engine needs your Mac password to install Homebrew.

This is the password you use to log in to this Mac. It goes straight to macOS and is not saved." default answer "" with hidden answer with title "Install Homebrew" with icon caution
text returned of result
APPLESCRIPT
)"; then
  printf '%s' '{CANCELLED}' > '{outcome}'
  exit 1
fi
printf '%s\n' "$password"
"#
        )
    }

    impl Askpass {
        fn new() -> Result<Self> {
            let (outcome_file, outcome) = crate::sysexec::create_scratch_file("soe-askpass", "txt")
                .context("could not prepare the password prompt")?;
            drop(outcome_file);

            let text = outcome
                .to_str()
                .filter(|path| !path.contains('\''))
                .map(askpass_script)
                .with_context(|| {
                    format!(
                        "the temporary directory has a name this cannot work in ({})",
                        outcome.display()
                    )
                })?;

            let (mut script_file, script) =
                crate::sysexec::create_scratch_file("soe-askpass", "sh")
                    .context("could not prepare the password prompt")?;
            let guard = Self {
                script: script.clone(),
                outcome,
            };
            script_file
                .write_all(text.as_bytes())
                .and_then(|()| script_file.flush())
                .context("could not write the password prompt")?;
            drop(script_file);

            // Created readable and writable by its owner only; `sudo` needs it
            // executable as well, and by nobody else.
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .context("could not make the password prompt runnable")?;

            Ok(guard)
        }

        fn script(&self) -> &Path {
            &self.script
        }

        /// True if the user dismissed the dialog rather than typing anything.
        fn was_cancelled(&self) -> bool {
            std::fs::read_to_string(&self.outcome)
                .map(|outcome| outcome.trim() == CANCELLED)
                .unwrap_or(false)
        }
    }

    impl Drop for Askpass {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.script);
            let _ = std::fs::remove_file(&self.outcome);
        }
    }

    /// Runs Homebrew's own installer unattended, streaming its output a line at
    /// a time and answering its password prompt with a dialog.
    pub fn install(mut on_line: impl FnMut(String)) -> Result<()> {
        let script = fetch_installer()?;
        let askpass = Askpass::new()?;

        // The installer is passed as the text of the command to run, which is
        // what the documented `bash -c "$(curl …)"` amounts to once the outer
        // shell has done the substitution: no outer shell here, so the
        // substitution is done above instead.
        let mut child = Command::new("/bin/bash")
            .arg("-c")
            .arg(&script)
            // Where sudo goes for the password it cannot ask a GUI app for.
            .env("SUDO_ASKPASS", askpass.script())
            // Skips the installer's own "Press RETURN to continue" prompt,
            // which would otherwise wait forever on stdin that is closed.
            .env("NONINTERACTIVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not run the Homebrew installer")?;

        // These write progress to stderr and results to stdout; both are
        // useful, and neither is worth losing.
        let stderr = child.stderr.take().map(|pipe| {
            std::thread::spawn(move || {
                BufReader::new(pipe)
                    .lines()
                    .map_while(Result::ok)
                    .collect::<Vec<_>>()
            })
        });
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                on_line(line);
            }
        }
        let status = child
            .wait()
            .context("the Homebrew installer did not finish cleanly")?;
        if let Some(handle) = stderr
            && let Ok(lines) = handle.join()
        {
            for line in lines {
                on_line(line);
            }
        }

        if !status.success() {
            if askpass.was_cancelled() {
                bail!("Homebrew was not installed: the password prompt was cancelled");
            }
            bail!(
                "installing Homebrew failed (exit status {status}). Try installing it by \
                 hand from https://brew.sh."
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Runs a copy of the helper with `osascript` swapped for `stub`, so
        /// the real dialog never opens. Returns what `sudo` would have read
        /// and whether the helper reported a cancellation.
        fn run_askpass(stub: &str) -> (bool, String, bool) {
            let askpass = Askpass::new().expect("the helper should be written");
            let script = std::fs::read_to_string(askpass.script()).unwrap();
            let stubbed = script.replace("/usr/bin/osascript", stub);
            std::fs::write(askpass.script(), &stubbed).unwrap();

            let output = Command::new("/bin/bash")
                .arg(askpass.script())
                .output()
                .expect("the helper should run");
            (
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).to_string(),
                askpass.was_cancelled(),
            )
        }

        /// The one thing `sudo` asks of an askpass helper: print the password
        /// on stdout, and nothing else.
        #[test]
        fn the_helper_prints_what_the_dialog_returned() {
            let (ok, stdout, cancelled) = run_askpass("/bin/echo 'hunter2'");
            assert!(ok, "the helper should succeed when the dialog does");
            assert_eq!(stdout, "hunter2\n");
            assert!(!cancelled);
        }

        /// A dismissed dialog must not reach sudo as an empty password and be
        /// reported as a wrong one.
        #[test]
        fn a_dismissed_dialog_is_reported_as_a_cancellation() {
            let (ok, stdout, cancelled) = run_askpass("/usr/bin/false");
            assert!(!ok);
            assert!(stdout.is_empty(), "nothing should reach sudo: {stdout:?}");
            assert!(cancelled);
        }

        /// A password with a space, a quote or a `$` in it has to arrive
        /// exactly as typed.
        #[test]
        fn an_awkward_password_survives_the_helper() {
            let awkward = r#"a b'c$d"e\f"#;
            let (ok, stdout, _) = run_askpass(&format!("/bin/echo {}", shell_quote(awkward)));
            assert!(ok);
            assert_eq!(stdout, format!("{awkward}\n"));
        }

        fn shell_quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', r"'\''"))
        }

        /// Both temporary files are the app's own business and nobody else's,
        /// and neither outlives the install.
        #[test]
        fn the_helper_is_private_and_cleaned_up() {
            use std::os::unix::fs::PermissionsExt as _;

            let (script, outcome) = {
                let askpass = Askpass::new().unwrap();
                let mode = std::fs::metadata(askpass.script())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o700, "the helper is readable by others");
                (askpass.script.clone(), askpass.outcome.clone())
            };

            assert!(!script.exists(), "the helper was left behind");
            assert!(!outcome.exists(), "the outcome file was left behind");
        }
    }
}

// ------------------------------------------------------- everywhere else

#[cfg(not(target_os = "macos"))]
mod platform {
    use anyhow::{Result, bail};

    pub fn install(_on_line: impl FnMut(String)) -> Result<()> {
        bail!("Homebrew can only be installed on macOS")
    }
}

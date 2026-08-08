//! Getting Homebrew itself installed, when Ollama's own installer needs it
//! and it isn't there yet.
//!
//! Homebrew's own installer needs `sudo` the first time it runs, to create
//! and take ownership of its install prefix (`/opt/homebrew` on Apple
//! Silicon, `/usr/local` on Intel) — and there is nowhere for a headless
//! child process with its stdin closed to type that password into. So rather
//! than hand the whole thing off to a Terminal window, this app claims the
//! prefix itself first, through macOS's own authorization dialog — the same
//! Touch ID / password sheet any other installer uses. Once that is done,
//! Homebrew's own installer needs no further privilege and runs exactly like
//! [`crate::ollama::install`] runs `brew install ollama`: unattended,
//! streamed a line at a time, with no window of its own at all.

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

/// Shown to the user before anything runs, because installing software is
/// not something to do by surprise. This is Homebrew's own documented
/// install command; see <https://brew.sh>.
pub const INSTALL_COMMAND: &str = r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#;

pub use platform::install;

// --------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result, bail};
    use std::io::{BufRead as _, BufReader};
    use std::process::{Command, Stdio};

    /// The prefix Homebrew installs into on this Mac's processor.
    fn prefix() -> Result<&'static str> {
        let arch = Command::new("/usr/bin/uname")
            .arg("-m")
            .output()
            .context("could not determine this Mac's processor architecture")?;
        Ok(if String::from_utf8_lossy(&arch.stdout).trim() == "arm64" {
            "/opt/homebrew"
        } else {
            "/usr/local"
        })
    }

    /// True if the current user can already write to `prefix` — either it
    /// exists and is theirs, or its parent would let them create it. If so,
    /// Homebrew's own installer needs no elevation at all.
    fn is_writable(prefix: &str) -> bool {
        let probe = std::path::Path::new(prefix)
            .join(format!(".accessengine-write-test-{}", std::process::id()));
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    /// Creates `prefix` and hands it to the current user, prompting through
    /// macOS's own authorization dialog if that needs elevating. Asks for no
    /// password at all when the prefix is already usable.
    ///
    /// Only the prefix directory itself is touched — not `-R` over whatever
    /// might already be under it — since on Intel Macs `/usr/local` often
    /// already exists with unrelated content other tools put there, and this
    /// has no business reassigning ownership of any of that. A freshly
    /// created, empty prefix is all Homebrew's installer actually needs to
    /// build the rest of itself as this user, unprivileged.
    fn claim_prefix(prefix: &str) -> Result<()> {
        if is_writable(prefix) {
            return Ok(());
        }

        let user = Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .context("could not determine the current user")?;
        if !user.status.success() {
            bail!("could not determine the current user");
        }
        let user = String::from_utf8_lossy(&user.stdout).trim().to_string();
        // About to be embedded in a shell command that runs as root, so it is
        // checked against the character set a real username is actually made
        // of rather than trusted as-is.
        if user.is_empty()
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            bail!("could not safely use the current username ({user}) here");
        }

        let script = format!(
            "do shell script \"mkdir -p {prefix} && chown {user} {prefix}\" \
             with administrator privileges with prompt \
             \"Speech Output Engine needs your password to set up Homebrew.\""
        );
        let output = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .context("could not run osascript")?;
        if output.status.success() {
            return Ok(());
        }
        let message = String::from_utf8_lossy(&output.stderr);
        // -128 is AppleScript's code for "the user cancelled the prompt".
        if message.contains("-128") {
            bail!("Homebrew was not installed: the password prompt was cancelled");
        }
        bail!("could not prepare {prefix}: {}", message.trim());
    }

    /// Claims the install prefix, then runs Homebrew's own installer
    /// unattended, streaming its output a line at a time.
    pub fn install(mut on_line: impl FnMut(String)) -> Result<()> {
        let prefix = prefix()?;
        claim_prefix(prefix)?;

        let mut child = Command::new("/bin/bash")
            .arg("-c")
            .arg("$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)")
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
            bail!(
                "installing Homebrew failed (exit status {status}). Try installing it by \
                 hand from https://brew.sh."
            );
        }
        Ok(())
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

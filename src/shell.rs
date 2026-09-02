//! The right-click entry, and the little script behind it.
//!
//! Settings can add **Save as MP3 with AccessEngine** to the file manager's
//! context menu. Two things go on disk to make that happen:
//!
//! 1. **A script**, in this app's own settings folder — `%APPDATA%` on
//!    Windows, `~/Library/Application Support` on macOS, `~/.config` on Linux.
//!    It is three lines, and all it does is call this same binary with
//!    `--convert`. It exists because the registration below wants something
//!    stable to point at, and because a shell script is the thing every one of
//!    these systems knows how to run.
//! 2. **A registration**, which is different on every platform and is the
//!    whole reason this module is as long as it is.
//!
//! The script does not carry any settings of its own. It calls the app, and
//! the app reads `config.json` — so the engine, the voice, the wordlists and
//! the chunking are whatever the window was last set to, and changing them in
//! the window changes what the right-click entry does with no reinstalling.
//! **No credential is ever written into the script.**
//!
//! The binary's location is baked in at install time, because that is the one
//! thing the script cannot look up. Move or reinstall the app and the entry
//! needs adding again; the Settings tab says so.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;
use crate::t;

/// What the entry is called in the menu, and what the files it makes are for.
fn menu_label() -> String {
    t!("shell.menu_label")
}

/// Where the helper script lives.
pub fn script_path() -> Option<PathBuf> {
    config::config_dir().map(|dir| dir.join(script_name()))
}

fn script_name() -> &'static str {
    if cfg!(windows) {
        "accessengine-convert.cmd"
    } else {
        "accessengine-convert.sh"
    }
}

/// Whether the entry looks to be installed.
///
/// The script is the part this app owns on every platform, so it is what gets
/// asked. A user who has taken the entry out of their file manager by hand and
/// left the script behind will be told to remove it and add it again, which is
/// the right advice either way.
pub fn is_installed() -> bool {
    script_path().is_some_and(|path| path.exists()) && registration_exists()
}

/// Put the script in place and register it.
pub fn install() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding this program on disk")?;
    let script = write_script(&exe)?;
    register(&script)?;
    log::info!(
        "installed the right-click entry, calling {}",
        script.display()
    );
    Ok(script)
}

/// Take both away again.
pub fn remove() -> Result<()> {
    unregister()?;
    if let Some(script) = script_path() {
        if script.exists() {
            std::fs::remove_file(&script)
                .with_context(|| format!("removing {}", script.display()))?;
        }
    }
    log::info!("removed the right-click entry");
    Ok(())
}

/// Write the script that the menu entry runs.
fn write_script(exe: &Path) -> Result<PathBuf> {
    let path = script_path().context("no settings folder to put the script in")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, script_body(exe))
        .with_context(|| format!("writing {}", path.display()))?;
    make_executable(&path);
    Ok(path)
}

/// The script itself.
///
/// One command, quoted, so a path with a space in it — which is most of them
/// on Windows and macOS — survives being handed on.
fn script_body(exe: &Path) -> String {
    let exe = exe.display();
    if cfg!(windows) {
        format!(
            "@echo off\r\n\
             rem  Written by {app}. Adds \"{label}\" to the right-click menu.\r\n\
             rem  Settings come from the app's own config.json, not from here.\r\n\
             \"{exe}\" --convert \"%~1\"\r\n",
            app = crate::APP_NAME,
            label = menu_label(),
        )
    } else {
        format!(
            "#!/bin/sh\n\
             #  Written by {app}. Adds \"{label}\" to the right-click menu.\n\
             #  Settings come from the app's own config.json, not from here.\n\
             exec \"{exe}\" --convert \"$1\"\n",
            app = crate::APP_NAME,
            label = menu_label(),
        )
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)) {
        log::warn!("could not make {} executable: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

// ------------------------------------------------------------------ Windows

/// Registered per file type under `SystemFileAssociations`, so the entry
/// appears on the documents this app can actually read and nowhere else.
/// Registering under `*` would put it on every file on the machine, including
/// the ones it would refuse.
#[cfg(windows)]
const REGISTRY_KEY: &str = "AccessEngineConvert";

#[cfg(windows)]
fn register(script: &Path) -> Result<()> {
    use crate::document::SUPPORTED_EXTENSIONS;
    for extension in SUPPORTED_EXTENSIONS {
        let key = format!(
            "HKCU\\Software\\Classes\\SystemFileAssociations\\.{extension}\\shell\\{REGISTRY_KEY}"
        );
        reg(&["add", &key, "/ve", "/d", &menu_label(), "/f"])?;
        reg(&[
            "add",
            &format!("{key}\\command"),
            "/ve",
            "/d",
            &format!("\"{}\" \"%1\"", script.display()),
            "/f",
        ])?;
    }
    Ok(())
}

#[cfg(windows)]
fn unregister() -> Result<()> {
    use crate::document::SUPPORTED_EXTENSIONS;
    for extension in SUPPORTED_EXTENSIONS {
        let key = format!(
            "HKCU\\Software\\Classes\\SystemFileAssociations\\.{extension}\\shell\\{REGISTRY_KEY}"
        );
        // A key that was never there is not a failure to remove it.
        let _ = reg(&["delete", &key, "/f"]);
    }
    Ok(())
}

#[cfg(windows)]
fn registration_exists() -> bool {
    use crate::document::SUPPORTED_EXTENSIONS;
    let Some(extension) = SUPPORTED_EXTENSIONS.first() else {
        return false;
    };
    let key = format!(
        "HKCU\\Software\\Classes\\SystemFileAssociations\\.{extension}\\shell\\{REGISTRY_KEY}"
    );
    reg(&["query", &key]).is_ok()
}

#[cfg(windows)]
fn reg(arguments: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    /// Do not flash a console window up behind the app for each key written.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("reg")
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("running reg.exe to change the registry")?;
    if !output.status.success() {
        anyhow::bail!(
            "{}",
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        );
    }
    Ok(())
}

// -------------------------------------------------------------------- macOS

/// A Quick Action in `~/Library/Services`, which is what a right-click entry
/// is on macOS. It is a folder pretending to be a file: two property lists,
/// one saying what the menu item is called and one describing a single "Run
/// Shell Script" step that calls our script.
#[cfg(target_os = "macos")]
fn workflow_path() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| {
        dirs.home_dir()
            .join("Library/Services")
            .join(format!("{}.workflow", menu_label()))
    })
}

#[cfg(target_os = "macos")]
fn register(script: &Path) -> Result<()> {
    let bundle = workflow_path().context("no home folder to install into")?;
    let resources = bundle.join("Contents/Resources");
    std::fs::create_dir_all(&resources)
        .with_context(|| format!("creating {}", resources.display()))?;

    std::fs::write(bundle.join("Contents/Info.plist"), info_plist())
        .context("writing the Quick Action's Info.plist")?;
    std::fs::write(resources.join("document.wflow"), workflow_plist(script))
        .context("writing the Quick Action's workflow")?;

    // Finder does not notice a new service until the pasteboard server is
    // told to look again. Not fatal if it fails: it happens on its own at the
    // next login, and saying "restart to see it" is better than refusing.
    let flushed = std::process::Command::new("/System/Library/CoreServices/pbs")
        .arg("-flush")
        .status();
    if let Err(e) = flushed {
        log::warn!("could not ask macOS to re-read its services: {e}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn unregister() -> Result<()> {
    if let Some(bundle) = workflow_path() {
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle)
                .with_context(|| format!("removing {}", bundle.display()))?;
        }
    }
    let _ = std::process::Command::new("/System/Library/CoreServices/pbs")
        .arg("-flush")
        .status();
    Ok(())
}

#[cfg(target_os = "macos")]
fn registration_exists() -> bool {
    workflow_path().is_some_and(|bundle| bundle.join("Contents/Info.plist").exists())
}

/// The bundle's Info.plist: what the menu item says, and what it applies to.
#[cfg(target_os = "macos")]
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>{label}</string>
    <key>CFBundleIdentifier</key>
    <string>org.AccessEngine.AccessEngine.convert</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>NSServices</key>
    <array>
        <dict>
            <key>NSMenuItem</key>
            <dict>
                <key>default</key>
                <string>{label}</string>
            </dict>
            <key>NSMessage</key>
            <string>runWorkflowAsService</string>
            <key>NSRequiredContext</key>
            <dict>
                <key>NSApplicationIdentifier</key>
                <string>com.apple.finder</string>
            </dict>
            <key>NSSendFileTypes</key>
            <array>
                <string>public.content</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
"#,
        label = xml_escape(&menu_label()),
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// The workflow: one "Run Shell Script" step, taking the selected files as
/// arguments and handing each to our script in turn.
#[cfg(target_os = "macos")]
fn workflow_plist(script: &Path) -> String {
    // `inputMethod` 1 means the selection arrives as arguments rather than on
    // standard input, which is what lets a file name with a space in it stay
    // one file name.
    let command = format!("for f in \"$@\"; do \"{}\" \"$f\"; done", script.display());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AMApplicationBuild</key>
    <string>512</string>
    <key>AMApplicationVersion</key>
    <string>2.10</string>
    <key>AMDocumentVersion</key>
    <string>2</string>
    <key>actions</key>
    <array>
        <dict>
            <key>action</key>
            <dict>
                <key>AMAccepts</key>
                <dict>
                    <key>Container</key>
                    <string>List</string>
                    <key>Optional</key>
                    <true/>
                    <key>Types</key>
                    <array>
                        <string>com.apple.cocoa.string</string>
                    </array>
                </dict>
                <key>AMActionVersion</key>
                <string>2.0.3</string>
                <key>AMProvides</key>
                <dict>
                    <key>Container</key>
                    <string>List</string>
                    <key>Types</key>
                    <array>
                        <string>com.apple.cocoa.string</string>
                    </array>
                </dict>
                <key>ActionBundlePath</key>
                <string>/System/Library/Automator/Run Shell Script.action</string>
                <key>ActionName</key>
                <string>Run Shell Script</string>
                <key>ActionParameters</key>
                <dict>
                    <key>COMMAND_STRING</key>
                    <string>{command}</string>
                    <key>CheckedForUserDefaultShell</key>
                    <true/>
                    <key>inputMethod</key>
                    <integer>1</integer>
                    <key>shell</key>
                    <string>/bin/sh</string>
                    <key>source</key>
                    <string></string>
                </dict>
                <key>BundleIdentifier</key>
                <string>com.apple.RunShellScript</string>
                <key>CFBundleVersion</key>
                <string>2.0.3</string>
                <key>CanShowSelectedItemsWhenRun</key>
                <false/>
                <key>CanShowWhenRun</key>
                <true/>
                <key>Category</key>
                <array>
                    <string>AMCategoryUtilities</string>
                </array>
                <key>Class Name</key>
                <string>RunShellScriptAction</string>
                <key>InputUUID</key>
                <string>6E7A1B3C-0000-4000-A000-000000000001</string>
                <key>Keywords</key>
                <array>
                    <string>Shell</string>
                    <string>Script</string>
                    <string>Command</string>
                    <string>Run</string>
                    <string>Unix</string>
                </array>
                <key>OutputUUID</key>
                <string>6E7A1B3C-0000-4000-A000-000000000002</string>
                <key>UUID</key>
                <string>6E7A1B3C-0000-4000-A000-000000000003</string>
                <key>UnlocalizedApplications</key>
                <array>
                    <string>Automator</string>
                </array>
                <key>arguments</key>
                <dict/>
                <key>isViewVisible</key>
                <integer>1</integer>
                <key>location</key>
                <string>309.000000:253.000000</string>
                <key>nibPath</key>
                <string>/System/Library/Automator/Run Shell Script.action/Contents/Resources/Base.lproj/main.nib</string>
            </dict>
            <key>isViewVisible</key>
            <integer>1</integer>
        </dict>
    </array>
    <key>connectors</key>
    <dict/>
    <key>workflowMetaData</key>
    <dict>
        <key>serviceApplicationBundleID</key>
        <string>com.apple.finder</string>
        <key>serviceApplicationPath</key>
        <string>/System/Library/CoreServices/Finder.app</string>
        <key>serviceInputTypeIdentifier</key>
        <string>com.apple.Automator.fileSystemObject</string>
        <key>serviceOutputTypeIdentifier</key>
        <string>com.apple.Automator.nothing</string>
        <key>serviceProcessesInput</key>
        <integer>0</integer>
        <key>workflowTypeIdentifier</key>
        <string>com.apple.Automator.servicesMenu</string>
    </dict>
</dict>
</plist>
"#,
        command = xml_escape(&command),
    )
}

// -------------------------------------------------------------------- Linux

/// Nautilus — the file manager Ubuntu ships — runs anything executable in this
/// folder from a **Scripts** submenu on the right-click menu. That is the only
/// mechanism that is both standard and needs no root, so it is the one used.
/// Other file managers have their own arrangements and are not covered; the
/// Settings tab says which one this installs for.
#[cfg(all(unix, not(target_os = "macos")))]
fn scripts_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".local/share/nautilus/scripts"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn entry_path() -> Option<PathBuf> {
    scripts_dir().map(|dir| dir.join(menu_label()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn register(script: &Path) -> Result<()> {
    let entry = entry_path().context("no home folder to install into")?;
    if let Some(parent) = entry.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Nautilus hands the selection in an environment variable, one path per
    // line, so that a name with a space in it is not split into two.
    let body = format!(
        "#!/bin/sh\n\
         #  Written by {app}. Runs \"{label}\" on what is selected.\n\
         printf '%s' \"$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\" | while IFS= read -r f; do\n\
         \t[ -n \"$f\" ] && \"{script}\" \"$f\"\n\
         done\n",
        app = crate::APP_NAME,
        label = menu_label(),
        script = script.display(),
    );
    std::fs::write(&entry, body).with_context(|| format!("writing {}", entry.display()))?;
    make_executable(&entry);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unregister() -> Result<()> {
    if let Some(entry) = entry_path() {
        if entry.exists() {
            std::fs::remove_file(&entry)
                .with_context(|| format!("removing {}", entry.display()))?;
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn registration_exists() -> bool {
    entry_path().is_some_and(|entry| entry.exists())
}

/// The five characters that cannot appear as themselves inside a plist string.
#[cfg(target_os = "macos")]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever else changes, the script must never carry a credential: it is
    /// a plain file in a folder, and the whole point is that it asks the app.
    #[test]
    fn the_script_holds_no_settings_of_its_own() {
        let body = script_body(Path::new("/Applications/accessengine"));
        assert!(body.contains("--convert"), "{body}");
        assert!(body.contains("/Applications/accessengine"), "{body}");
        for secret in ["api_key", "sk_", "sk-", "AKIA", "secret", "AIza"] {
            assert!(
                !body.contains(secret),
                "{secret} appeared in the script:\n{body}"
            );
        }
    }

    /// A path with a space in it is the ordinary case on macOS and Windows,
    /// and an unquoted one would convert the wrong file or nothing at all.
    #[test]
    fn the_script_quotes_what_it_is_given() {
        let body = script_body(Path::new("/Applications/Access Engine/accessengine"));
        if cfg!(windows) {
            assert!(body.contains("\"%~1\""), "{body}");
        } else {
            assert!(body.contains("\"$1\""), "{body}");
        }
        assert!(
            body.contains("\"/Applications/Access Engine/accessengine\""),
            "{body}"
        );
    }

    #[test]
    fn the_script_is_named_for_the_platform_that_runs_it() {
        let name = script_name();
        if cfg!(windows) {
            assert!(name.ends_with(".cmd"), "{name}");
        } else {
            assert!(name.ends_with(".sh"), "{name}");
        }
    }

    /// macOS reads these two files, not this app, so "it looks like a plist"
    /// is not good enough — the system's own parser is asked.
    ///
    /// This is the check that would have caught a bundle Finder silently
    /// ignores, which is the failure this feature is most likely to have.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_quick_action_is_a_property_list_macos_itself_accepts() {
        let dir = std::env::temp_dir().join(format!("accessengine-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        for (name, body) in [
            ("Info.plist", info_plist()),
            (
                "document.wflow",
                workflow_plist(Path::new("/tmp/a b/convert.sh")),
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, &body).expect("writes");
            let output = std::process::Command::new("plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .expect("plutil is part of macOS");
            assert!(
                output.status.success(),
                "{name} is not a property list macOS will read: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The label reaches a property list, where an ampersand would end the
    /// document early and leave a bundle macOS refuses to read.
    #[cfg(target_os = "macos")]
    #[test]
    fn markup_characters_cannot_break_the_property_list() {
        assert_eq!(xml_escape("Fish & Chips"), "Fish &amp; Chips");
        assert_eq!(xml_escape("a<b>c"), "a&lt;b&gt;c");
        let plist = workflow_plist(Path::new("/tmp/a b/script.sh"));
        assert!(plist.contains("/tmp/a b/script.sh"), "{plist}");
        assert!(
            plist.contains("com.apple.Automator.servicesMenu"),
            "{plist}"
        );
    }
}

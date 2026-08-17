//! accessengine — the Speech Output Engine.
//!
//! Opens a `.txt`, `.docx`, image or video file and either reads it aloud or
//! saves the speech as a WAV or MP3. Speech comes from the voices built into
//! macOS and Windows, or from ElevenLabs when an API key is available. Images
//! are turned into text by a vision model running locally through Ollama, and
//! video by taking stills with ffmpeg and reading each of them the same way.
//!
//! The whole app is one form of full-width controls that can be driven entirely
//! from the keyboard; see [`app`] for why it is shaped that way.

// Don't open a console window alongside the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apikey;
mod app;
mod audio;
mod config;
mod context_menu;
mod dictionary;
mod extract;
mod ffmpeg;
mod geocode;
mod homebrew;
mod i18n;
mod jobs;
mod log;
mod ollama;
mod sysexec;
mod theme;
mod tts;
mod update;

fn main() -> eframe::Result<()> {
    // First, so that anything going wrong during startup is in the log too.
    log::start(env!("CARGO_PKG_VERSION"));

    // Before anything that says a word, including the headless path below,
    // which puts its failure in a message box.
    i18n::apply_setting(&config::Config::load().language);

    // reqwest is built without a built-in crypto provider (see Cargo.toml), so
    // one has to be installed before the first HTTPS request or the ElevenLabs
    // client fails to build. Failure here means another provider is already
    // installed, which is just as good.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // The Windows right-click "Speak to file" entry launches the app this
    // way — see `context_menu` and `jobs::speak_to_file`. Handled before any
    // window is built, so a run started from Explorer never flashes one up.
    if let Some(path) = speak_to_file_argument() {
        speak_to_file_and_exit(path);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(t!("app.title"))
            // The pane rail plus the form, with room to spare on both.
            .with_inner_size([800.0, 740.0])
            .with_min_inner_size([700.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        &t!("app.title"),
        options,
        Box::new(|cc| Ok(Box::new(app::SpeechApp::new(cc)))),
    )
}

/// The path after `--speak-to-file` on the command line, if that's how this
/// run was started.
fn speak_to_file_argument() -> Option<std::path::PathBuf> {
    parse_speak_to_file_argument(std::env::args_os())
}

/// Takes the arguments as a parameter rather than reading them itself, so the
/// parsing can be tested without a real process command line to point it at.
fn parse_speak_to_file_argument(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if arg == "--speak-to-file" {
            return args.next().map(std::path::PathBuf::from);
        }
    }
    None
}

/// Runs the headless "Speak to file" pipeline and exits — this run was
/// launched from Explorer's right-click menu, not by someone expecting a
/// window, so nothing here ever calls `eframe::run_native`.
fn speak_to_file_and_exit(path: std::path::PathBuf) -> ! {
    let config = config::Config::load();
    let (api_key, _) = apikey::load();

    match jobs::speak_to_file(&path, &config, api_key.as_deref()) {
        Ok(saved) => {
            log::line(format!("speak-to-file: wrote {}", saved.display()));
            std::process::exit(0);
        }
        Err(error) => {
            log::line(format!("speak-to-file: failed — {error:#}"));
            // The only GUI this headless path ever shows: silence would leave
            // a failure looking like nothing happened at all.
            rfd::MessageDialog::new()
                .set_title(t!("app.title"))
                .set_description(format!("{error:#}"))
                .set_level(rfd::MessageLevel::Error)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> impl Iterator<Item = std::ffi::OsString> {
        words
            .iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_plain_open_with_path_is_not_a_speak_to_file_run() {
        assert_eq!(
            parse_speak_to_file_argument(args(&["accessengine.exe", "C:\\notes.txt"])),
            None
        );
    }

    #[test]
    fn the_path_after_the_flag_is_taken_whatever_comes_before_it() {
        assert_eq!(
            parse_speak_to_file_argument(args(&[
                "accessengine.exe",
                "--speak-to-file",
                "C:\\notes.txt",
            ])),
            Some(std::path::PathBuf::from("C:\\notes.txt"))
        );
    }

    #[test]
    fn a_flag_with_nothing_after_it_is_not_a_path() {
        assert_eq!(
            parse_speak_to_file_argument(args(&["accessengine.exe", "--speak-to-file"])),
            None
        );
    }
}

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
mod dictionary;
mod extract;
mod ffmpeg;
mod geocode;
mod homebrew;
mod jobs;
mod log;
mod ollama;
mod sysexec;
mod theme;
mod tts;
mod update;

/// The name shown to the user, which is not the name of the binary.
const APP_TITLE: &str = "Speech Output Engine";

fn main() -> eframe::Result<()> {
    // First, so that anything going wrong during startup is in the log too.
    log::start(env!("CARGO_PKG_VERSION"));

    // reqwest is built without a built-in crypto provider (see Cargo.toml), so
    // one has to be installed before the first HTTPS request or the ElevenLabs
    // client fails to build. Failure here means another provider is already
    // installed, which is just as good.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            // The pane rail plus the form, with room to spare on both.
            .with_inner_size([800.0, 740.0])
            .with_min_inner_size([700.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(app::SpeechApp::new(cc)))),
    )
}

// The Accessibility Engine — a graphical text-to-speech reader.
// Copyright (C) 2026 Will Richards
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
// FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
// more details.
//
// You should have received a copy of the GNU General Public License along
// with this program. If not, see <https://www.gnu.org/licenses/>.

//! The Accessibility Engine — a graphical text-to-speech reader.
//!
//! Binary name: `accessengine`.

// Do not pop a console window behind the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod config;
mod document;
mod export;
mod logging;
mod powerpoint;
mod speech;
mod theme;
mod update;
mod vision;
mod wordlist;

/// The name shown to users, in the title bar and the docs.
pub const APP_NAME: &str = "The Accessibility Engine";

fn main() -> eframe::Result {
    logging::init();
    install_panic_hook();

    // `accessengine <file>` opens that file straight away, which is also what
    // makes the app usable as a "read this to me" handler from a file manager.
    let initial_file = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    if let Some(path) = &initial_file {
        log::info!("opening {} from the command line", path.display());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([840.0, 560.0])
            .with_app_id("accessengine"),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(app::AccessEngine::new(cc, initial_file)))),
    )
}

/// Route panics into the log file. Without this a crash in a worker thread
/// leaves nothing behind but a line on a stderr nobody is watching.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        log::error!("panic in thread '{name}' at {location}: {}", info);
        previous(info);
    }));
}

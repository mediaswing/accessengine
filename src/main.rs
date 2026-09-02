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
mod cli;
mod config;
mod document;
mod export;
mod i18n;
mod logging;
mod player;
mod playlist;
mod powerpoint;
mod shell;
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

    // `accessengine <file>` opens that file straight away, which is what makes
    // the app usable as a "read this to me" handler from a file manager.
    // `--convert` is the other way round: it does the work and never opens a
    // window, which is what the right-click entry needs. See `cli`.
    let initial_file = match cli::parse(std::env::args_os().skip(1)) {
        cli::Invocation::Window(path) => path,
        cli::Invocation::Help => {
            report(&cli::usage());
            return Ok(());
        }
        cli::Invocation::Version => {
            report(&format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")));
            return Ok(());
        }
        cli::Invocation::Convert { input, output } => {
            std::process::exit(match cli::convert(&input, output) {
                Ok(written) => {
                    report(&format!("Saved {}", written.display()));
                    0
                }
                Err(e) => {
                    // To the log as well as the console: on Windows a release
                    // build has no console to print to, and the log is then
                    // the only account of why nothing happened.
                    log::error!("converting {}: {e:#}", input.display());
                    report(&format!("Could not convert {}: {e:#}", input.display()));
                    1
                }
            });
        }
    };
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

/// Say something on the way out of a run that never opens a window.
///
/// Nothing but `println!` on the platforms that have a console. A Windows
/// release build is linked as a GUI subsystem binary and has none, so the same
/// words go to the log, which is where the script that called us tells the
/// user to look.
fn report(message: &str) {
    println!("{message}");
    log::info!("{message}");
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

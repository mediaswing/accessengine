//! The command line, and the conversion it exists for.
//!
//! Almost every use of this app is the window. The exception is the right-click
//! entry the Settings tab can install — see [`crate::shell`] — which has to
//! turn a file into an MP3 without anything appearing on screen, using the
//! settings already chosen in the app.
//!
//! So this is deliberately not a general command-line interface. It is one
//! verb, and it does exactly what pressing Apply with **Save the reading as an
//! MP3** does: the same document reader, the same wordlists, the same voice,
//! the same requests. Anything it did differently would be a second way for
//! the app to behave, and the wordlists are the reason that matters — a word a
//! safety list keeps out of a reading must stay out of a file converted
//! without the window ever opening.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{self, Config};
use crate::document::Document;
use crate::export;
use crate::speech;
use crate::wordlist::{self, WordlistSet};
use crate::APP_NAME;

/// What the arguments asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Open the window, optionally on a file.
    Window(Option<PathBuf>),
    /// Convert a file and exit, saying nothing unless something went wrong.
    Convert {
        input: PathBuf,
        /// Where to write. `None` means beside the input, same name, `.mp3`.
        output: Option<PathBuf>,
    },
    Help,
    Version,
}

/// Read the arguments.
///
/// Hand-written rather than a parser crate: there is one flag with one operand,
/// and the argument this app is overwhelmingly given is a bare path from a file
/// manager, which must keep working exactly as it does now.
pub fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Invocation {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut convert = false;
    let mut expecting_output = false;

    for argument in args {
        if expecting_output {
            output = Some(PathBuf::from(argument));
            expecting_output = false;
            continue;
        }
        match argument.to_str() {
            Some("--convert" | "-c") => convert = true,
            Some("--out" | "-o") => expecting_output = true,
            Some("--help" | "-h") => return Invocation::Help,
            Some("--version" | "-V") => return Invocation::Version,
            // A path, which is the ordinary case. Only the first is taken:
            // this converts one file, and silently reading the second as
            // something else would be worse than ignoring it.
            _ => {
                if input.is_none() {
                    input = Some(PathBuf::from(argument));
                }
            }
        }
    }

    match (convert, input) {
        (true, Some(input)) => Invocation::Convert { input, output },
        // `--convert` with nothing to convert is a mistake worth naming rather
        // than quietly opening the window.
        (true, None) => Invocation::Help,
        (false, path) => Invocation::Window(path),
    }
}

pub fn usage() -> String {
    format!(
        "{APP_NAME} {}\n\n\
         Usage:\n  \
         accessengine [FILE]                 open the window, on FILE if given\n  \
         accessengine --convert FILE         save FILE as an MP3 beside itself\n  \
         accessengine --convert FILE -o OUT  save it as OUT instead\n  \
         accessengine --help | --version\n\n\
         Converting uses the settings already saved in the app: the speech\n\
         engine, the voice, the wordlists and how the text is cut up. It needs\n\
         one of the cloud engines, because the system voices cannot be recorded.\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Convert one file, using the settings the app already holds.
///
/// Returns where it was written.
pub fn convert(input: &Path, output: Option<PathBuf>) -> Result<PathBuf> {
    let cfg = Config::load();

    // Both refusals name the fix, because the person reading this is looking
    // at a file manager that has just done nothing.
    if !cfg.engine.is_cloud() {
        bail!(
            "converting needs one of the cloud speech engines — the system voices cannot be \
             recorded to a file. Choose one on the Settings tab of {APP_NAME}, then try again."
        );
    }
    if !cfg.has_credentials(cfg.engine) {
        bail!(
            "there are no {} credentials saved. Enter them on the General tab of {APP_NAME}, \
             and tick \"Remember these on this computer\" so the command line can use them too.",
            cfg.engine.provider_name()
        );
    }
    let (voice_id, _) = cfg.cloud_voice(cfg.engine);
    if voice_id.is_empty() {
        bail!(
            "no {} voice has been chosen. Pick one on the General tab of {APP_NAME}.",
            cfg.engine.provider_name()
        );
    }

    let document = Document::from_path(input, cfg.chunk_mode)
        .with_context(|| format!("reading {}", input.display()))?;

    // The same wordlists the window would apply, from the same folder.
    let mut wordlists = WordlistSet {
        policy: cfg.block_policy,
        bleep_text: cfg.bleep_text.clone(),
        lists: Vec::new(),
    };
    if let Some(dir) = config::wordlist_dir() {
        wordlists.lists = wordlist::discover(&dir, &cfg.disabled_wordlists);
    }

    let plan = speech::build_plan(&document, &wordlists, cfg.wordlists_enabled);
    if plan.items.is_empty() {
        bail!("there is nothing to read in {}", input.display());
    }
    log::info!(
        "converting {}: {} chunks, {} to speak, {} skipped by wordlists",
        input.display(),
        document.chunks.len(),
        plan.items.len(),
        plan.skipped
    );

    // An explicit --out is the caller saying where it goes, overwrite and
    // all. A destination this app picked is not: right-clicking report.docx
    // in a folder that already holds a hand-made report.mp3 must not silently
    // destroy it, and there is no save dialog on this path to ask.
    let destination = match output {
        Some(path) => path,
        None => free_beside(input)?,
    };
    let request = cfg
        .voice_request()
        .context("no cloud engine is configured")?;
    let texts: Vec<String> = plan.items.into_iter().map(|item| item.text).collect();

    let bytes = export::write_blocking(&destination, &texts, &request)?;
    log::info!("wrote {} ({bytes} bytes)", destination.display());
    Ok(destination)
}

/// The same name as the input, as an `.mp3`, in the same folder.
fn beside(input: &Path) -> PathBuf {
    input.with_extension("mp3")
}

/// The same, but stepping aside rather than over anything already there.
///
/// `report.mp3`, then `report (2).mp3`, and so on. Bounded because a folder
/// that already holds a hundred of them is a mistake to report, not to add to.
fn free_beside(input: &Path) -> Result<PathBuf> {
    let first = beside(input);
    if !first.exists() {
        return Ok(first);
    }
    let stem = first
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 2..=99 {
        let candidate = first.with_file_name(format!("{stem} ({n}).mp3"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "{} already exists, and so do the next 98 names after it. \
         Pass --out to say where this one should go.",
        first.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Invocation {
        parse(args.iter().map(std::ffi::OsString::from))
    }

    /// Nothing the right-click entry writes may land on top of a file that is
    /// already there: there is no save dialog on that path to ask first.
    #[test]
    fn a_chosen_destination_steps_aside_rather_than_over() {
        let dir = std::env::temp_dir().join(format!("accessengine-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let input = dir.join("report.docx");
        std::fs::write(&input, b"x").expect("writing the input");
        assert_eq!(free_beside(&input).expect("a free name"), dir.join("report.mp3"));

        std::fs::write(dir.join("report.mp3"), b"precious").expect("writing an mp3");
        assert_eq!(
            free_beside(&input).expect("a free name"),
            dir.join("report (2).mp3")
        );

        std::fs::write(dir.join("report (2).mp3"), b"also precious").expect("writing");
        assert_eq!(
            free_beside(&input).expect("a free name"),
            dir.join("report (3).mp3")
        );

        // And the file that was already there is still what it was.
        assert_eq!(
            std::fs::read(dir.join("report.mp3")).expect("reading back"),
            b"precious"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The argument this app is overwhelmingly given is a bare path from a
    /// file manager, and that must go on opening the window.
    #[test]
    fn a_bare_path_still_opens_the_window() {
        assert_eq!(
            parsed(&["notes.md"]),
            Invocation::Window(Some(PathBuf::from("notes.md")))
        );
        assert_eq!(parsed(&[]), Invocation::Window(None));
    }

    #[test]
    fn convert_takes_the_file_and_an_optional_destination() {
        assert_eq!(
            parsed(&["--convert", "notes.md"]),
            Invocation::Convert {
                input: PathBuf::from("notes.md"),
                output: None
            }
        );
        assert_eq!(
            parsed(&["--convert", "notes.md", "--out", "/tmp/reading.mp3"]),
            Invocation::Convert {
                input: PathBuf::from("notes.md"),
                output: Some(PathBuf::from("/tmp/reading.mp3"))
            }
        );
        // The flags may come in either order, since a shell script writing
        // them is as likely to put the path first as last.
        assert_eq!(
            parsed(&["notes.md", "-c"]),
            Invocation::Convert {
                input: PathBuf::from("notes.md"),
                output: None
            }
        );
    }

    /// A destination that looks like a flag is still a destination: `-o` takes
    /// whatever comes next, or a file called `--help` could never be written.
    #[test]
    fn the_destination_is_whatever_follows_the_flag() {
        assert_eq!(
            parsed(&["--convert", "in.md", "-o", "--help"]),
            Invocation::Convert {
                input: PathBuf::from("in.md"),
                output: Some(PathBuf::from("--help"))
            }
        );
    }

    #[test]
    fn asking_for_help_wins_over_anything_else() {
        assert_eq!(parsed(&["--help"]), Invocation::Help);
        assert_eq!(parsed(&["--convert", "--help", "x.md"]), Invocation::Help);
        assert_eq!(parsed(&["--version"]), Invocation::Version);
        // `--convert` with no file cannot do anything, so it says how.
        assert_eq!(parsed(&["--convert"]), Invocation::Help);
    }

    /// Only the first path is taken. Converting the second file instead, or as
    /// well, would be a surprise nobody asked for.
    #[test]
    fn only_the_first_path_is_used() {
        assert_eq!(
            parsed(&["--convert", "first.md", "second.md"]),
            Invocation::Convert {
                input: PathBuf::from("first.md"),
                output: None
            }
        );
    }

    #[test]
    fn the_default_destination_sits_beside_the_input() {
        assert_eq!(
            beside(Path::new("/tmp/notes.md")),
            PathBuf::from("/tmp/notes.mp3")
        );
        assert_eq!(
            beside(Path::new("/tmp/report")),
            PathBuf::from("/tmp/report.mp3")
        );
        assert_eq!(
            beside(Path::new("/tmp/deck.final.pptx")),
            PathBuf::from("/tmp/deck.final.mp3")
        );
    }

    /// The usage text is what somebody sees when the right-click entry has
    /// gone wrong, so it has to name the verb and the settings it depends on.
    #[test]
    fn the_usage_text_says_what_converting_depends_on() {
        let usage = usage();
        assert!(usage.contains("--convert"), "{usage}");
        assert!(usage.contains("cloud"), "{usage}");
        assert!(usage.contains(env!("CARGO_PKG_VERSION")), "{usage}");
    }
}

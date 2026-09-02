//! Every word the interface says, looked up by key instead of written in place.
//!
//! A reader for people who need one is a poor fit for an interface that only
//! speaks English. The voices already do not care: the system engines ship
//! dozens of languages and ElevenLabs's multilingual model reads most of
//! Europe. This module is the other half — the words around them — written in
//! another language by somebody who does not program. Language files are plain
//! text, they can be dropped into a folder without rebuilding anything, and
//! see [`catalogue`] for the format.
//!
//! Three properties are deliberate.
//!
//! **English is always there.** It is compiled into the binary and any key a
//! translation has not reached yet falls back to it, so a language file is
//! useful from its first line rather than only its last. Nobody has to finish
//! before they can test.
//!
//! **Lookups work from any thread.** The ElevenLabs worker and the Ollama
//! request each build their own failure messages away from the UI thread, so
//! the active language has to be reachable from them as well.
//!
//! **The lookup happens every frame**, not once at startup, which is what
//! makes changing language redraw the whole interface with no restart.

pub mod catalogue;

use catalogue::Catalogue;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

/// The languages built into the binary. English must be first: it is the
/// fallback for every other one, and [`Registry::english`] assumes index 0.
///
/// Compiled in rather than only shipped alongside, so the app cannot arrive
/// somewhere with no words at all.
const EMBEDDED: &[&str] = &[include_str!("../../assets/lang/en.toml")];

/// The saved value meaning "whatever language this computer is set to".
pub const AUTO: &str = "auto";

/// Why a file in the languages folder could not be used at all.
///
/// Distinct from [`catalogue::Problem`], which is a *line* the parser could not
/// read inside a file that was otherwise loaded. These are whole files that
/// never became a language, and they matter more: a bad line costs one string,
/// whereas one of these is a translator's afternoon disappearing without a
/// word. They used to be reported only through `log`, which this app installs
/// no implementation for, so in practice they were reported nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileReason {
    /// There, but unreadable — permissions, most likely.
    Unreadable,
    /// No `code` line, so there is no way to tell what language it is.
    NoCode,
    /// `code = "en"`. English is the fallback every other language is measured
    /// against, so an incomplete file replacing it would leave keys with
    /// nothing behind them at all.
    WouldReplaceEnglish,
}

/// A file in the languages folder that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProblem {
    pub path: PathBuf,
    pub reason: FileReason,
}

/// The language files that are loaded, and which one is in use.
struct Registry {
    /// Index 0 is always English.
    catalogues: Vec<Catalogue>,
    current: usize,
    /// Files in the folder that never became a language. Kept so the Settings
    /// pane can say so rather than leaving a translator staring at a picker
    /// their file is not in.
    folder_problems: Vec<FileProblem>,
}

impl Registry {
    fn load() -> Self {
        Self::assemble(folder_catalogues())
    }

    /// The built-in languages with the folder's laid over them, split from
    /// [`Registry::load`] so a test can hand it a folder of its own — the real
    /// one is inside the user's data directory, which is not somewhere a test
    /// should be writing.
    fn assemble(
        (extras, mut folder_problems): (Vec<(PathBuf, Catalogue)>, Vec<FileProblem>),
    ) -> Self {
        let mut catalogues: Vec<Catalogue> =
            EMBEDDED.iter().map(|text| Catalogue::parse(text)).collect();

        // A file in the user's folder replaces the built-in language of the
        // same code. That is what lets a translator improve a shipped
        // translation, not only add a new one.
        for (path, extra) in extras {
            match catalogues
                .iter()
                .position(|existing| same_code(&existing.code, &extra.code))
            {
                // Never index 0 — see [`FileReason::WouldReplaceEnglish`].
                Some(0) => folder_problems.push(FileProblem {
                    path,
                    reason: FileReason::WouldReplaceEnglish,
                }),
                Some(index) => catalogues[index] = extra,
                None => catalogues.push(extra),
            }
        }

        Self {
            catalogues,
            current: 0,
            folder_problems,
        }
    }

    fn english(&self) -> &Catalogue {
        &self.catalogues[0]
    }

    fn current(&self) -> &Catalogue {
        &self.catalogues[self.current]
    }
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(|| RwLock::new(Registry::load()));

/// Where a translator's own language files live: beside the wordlists, in the
/// settings folder that Diagnostics already names and that the Wordlists tab
/// already has a button to open.
///
/// `None` on a system with no such folder, which is the same case that leaves
/// the app running on its defaults with nothing saved.
pub fn languages_dir() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("languages"))
}

/// Reads every `.toml` in the languages folder, with whatever could not be
/// used. A folder that is missing is the normal case, not an error.
///
/// Each catalogue comes back paired with the file it came from, so a problem
/// found later — a file whose code turns out to be English's — can still name
/// the file it is about.
fn folder_catalogues() -> (Vec<(PathBuf, Catalogue)>, Vec<FileProblem>) {
    match languages_dir() {
        Some(dir) => read_folder(&dir),
        None => (Vec::new(), Vec::new()),
    }
}

/// The part of the above that does the work, taking the folder as an argument
/// so a test can hand it one — the real folder is inside the user's own data
/// directory, which is not somewhere a test should be writing.
fn read_folder(dir: &std::path::Path) -> (Vec<(PathBuf, Catalogue)>, Vec<FileProblem>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };

    let (mut found, mut problems) = (Vec::new(), Vec::new());
    // Read in a fixed order, so the picker and the problem list do not shuffle
    // between runs on a filesystem that does not sort its directory entries.
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            problems.push(FileProblem {
                path,
                reason: FileReason::Unreadable,
            });
            continue;
        };
        let catalogue = Catalogue::parse(&text);
        if catalogue.code.trim().is_empty() {
            problems.push(FileProblem {
                path,
                reason: FileReason::NoCode,
            });
            continue;
        }
        found.push((path, catalogue));
    }
    (found, problems)
}

/// Two language codes naming the same language, give or take punctuation and
/// case: `pt-BR`, `pt_br` and `PT-br` are one language as far as this is
/// concerned.
fn same_code(a: &str, b: &str) -> bool {
    normalise(a) == normalise(b)
}

fn normalise(code: &str) -> String {
    code.trim().replace('_', "-").to_ascii_lowercase()
}

/// The part before the region: `fr` from `fr-CA`.
fn primary(code: &str) -> String {
    normalise(code)
        .split('-')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Every language that can be picked, as `(code, name in its own language)`.
pub fn available() -> Vec<(String, String)> {
    let registry = REGISTRY.read().expect("language registry");
    registry
        .catalogues
        .iter()
        .map(|c| {
            let name = if c.name.trim().is_empty() {
                c.code.clone()
            } else {
                c.name.clone()
            };
            (c.code.clone(), name)
        })
        .collect()
}

/// The code of the language in use.
pub fn current_code() -> String {
    REGISTRY
        .read()
        .expect("language registry")
        .current()
        .code
        .clone()
}

/// The language in use, named in its own language — what the Settings pane
/// shows, and what a status message says after a change.
pub fn current_name() -> String {
    let code = current_code();
    available()
        .into_iter()
        .find(|(candidate, _)| *candidate == code)
        .map_or(code, |(_, name)| name)
}

/// The files in the languages folder that never became a language, for the
/// Settings pane to show. Always empty until somebody puts a file there.
pub fn folder_problems() -> Vec<FileProblem> {
    REGISTRY
        .read()
        .expect("language registry")
        .folder_problems
        .clone()
}

/// Whatever is wrong with the language file in use, for the Settings pane to
/// show. Empty for the built-in languages, since a test holds them to it.
pub fn current_problems() -> Vec<catalogue::Problem> {
    REGISTRY
        .read()
        .expect("language registry")
        .current()
        .problems
        .clone()
}

/// Switches language, by exact code or by falling back to the language without
/// its region — someone whose system says `fr-CA` should get French rather than
/// English when only `fr` is installed.
///
/// Returns the code actually selected, which is `en` if nothing matched.
pub fn set_language(code: &str) -> String {
    let mut registry = REGISTRY.write().expect("language registry");
    registry.current = choose(&registry.catalogues, code);
    registry.current().code.clone()
}

/// Which of these languages `code` asks for: the exact match, else the same
/// language without its region, else English at index 0.
///
/// Split out from [`set_language`] so it can be held to its promise without a
/// second language having to ship in the binary to test it with.
fn choose(catalogues: &[Catalogue], code: &str) -> usize {
    catalogues
        .iter()
        .position(|c| same_code(&c.code, code))
        .or_else(|| {
            catalogues
                .iter()
                .position(|c| primary(&c.code) == primary(code))
        })
        .unwrap_or(0)
}

/// Re-reads the languages folder, keeping the current language selected if it
/// is still there.
///
/// This is the translator's edit-and-see-it loop: change a line, press the
/// button in Settings, watch the interface change. Without it the loop runs
/// through a restart, which is slow enough to make a long file a chore.
pub fn reload() -> String {
    let wanted = current_code();
    {
        let mut registry = REGISTRY.write().expect("language registry");
        *registry = Registry::load();
    }
    set_language(&wanted)
}

/// The language the operating system is set to, as a code like `fr` or `pt-BR`.
///
/// `ACCESSENGINE_LOCALE` overrides it, which is how a translator sees their
/// file in place without changing what the whole computer is set to.
pub fn system_language() -> Option<String> {
    std::env::var("ACCESSENGINE_LOCALE")
        .ok()
        .filter(|tag| !tag.trim().is_empty())
        .or_else(sys_locale::get_locale)
        .map(|tag| {
            // `fr_FR.UTF-8` — the encoding is no use here.
            tag.split(['.', '@'])
                .next()
                .unwrap_or(&tag)
                .trim()
                .replace('_', "-")
        })
        .filter(|tag| !tag.is_empty() && tag != "C" && tag != "POSIX")
}

/// Resolves the saved setting to a language and selects it.
///
/// `auto` asks the operating system; anything else is a code the user picked
/// in Settings. Returns the code actually in use, which the caller shows.
pub fn apply_setting(setting: &str) -> String {
    let wanted = if setting.trim().eq_ignore_ascii_case(AUTO) || setting.trim().is_empty() {
        system_language().unwrap_or_else(|| "en".to_string())
    } else {
        setting.to_string()
    };
    let chosen = set_language(&wanted);
    log::info!("language: {setting} resolved to {chosen}");
    chosen
}

/// Runs `body` with `code` in use, then puts the language back.
///
/// The active language is process-wide, and `cargo test` runs tests in threads
/// that share it — so any test whose result depends on the language has to hold
/// this lock, including the ones that expect English. Without it a test asking
/// what an English error message says can be answered in French by a test
/// running beside it, and the failure appears once in every few dozen runs.
#[cfg(test)]
pub fn with_language<T>(code: &str, body: impl FnOnce() -> T) -> T {
    static IN_USE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that panicked while holding it poisoned it; the language is still
    // perfectly usable, and turning one failure into every later failure would
    // only hide the first.
    let _guard = IN_USE.lock().unwrap_or_else(|held| held.into_inner());

    let previous = current_code();
    set_language(code);
    let result = body();
    set_language(&previous);
    result
}

/// Looks up `key`, substitutes the values, and falls back to English for
/// anything the current language has not translated.
///
/// A key that exists in neither is returned as itself. That is a bug rather
/// than a state a user should reach — a test checks every key at every call
/// site — and showing the key is how it gets noticed rather than hidden.
pub fn text(key: &str, values: &[(&str, String)]) -> String {
    let registry = REGISTRY.read().expect("language registry");
    let template = registry
        .current()
        .get(key)
        .or_else(|| registry.english().get(key))
        .unwrap_or(key);
    substitute(template, values)
}

/// The counted form of [`text`], choosing the variant `n` calls for in the
/// language in use and binding `n` itself as a placeholder.
///
/// The fallback is one step subtler than for [`text`]: a language that has not
/// translated a counted message falls back to *English's* plural rule as well
/// as its words, since choosing a Polish variant of an English sentence would
/// pick a form that is not there.
pub fn plural(key: &str, n: u64, values: &[(&str, String)]) -> String {
    let registry = REGISTRY.read().expect("language registry");
    let template = registry
        .current()
        .plural_get(key, n)
        .or_else(|| registry.english().plural_get(key, n))
        .unwrap_or(key);

    let mut all = vec![("n", n.to_string())];
    all.extend(values.iter().map(|(k, v)| (*k, v.clone())));
    substitute(template, &all)
}

fn substitute(template: &str, values: &[(&str, String)]) -> String {
    let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
    catalogue::fill(template, &pairs)
}

/// A word from the interface's vocabulary.
///
/// ```ignore
/// t!("common.cancel")                     // plain
/// t!("status.loaded", title = title)      // {title} substituted
/// ```
///
/// Keys are checked against the English file by a test rather than by the
/// compiler, which is why they are written as literals here: a key assembled at
/// run time is invisible to that test.
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::text($key, &[])
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::text(
            $key,
            &[$((stringify!($name), ::std::string::ToString::to_string(&$value))),+],
        )
    };
}

/// A counted word from the interface's vocabulary, where the number decides
/// which wording the language uses.
///
/// ```ignore
/// tn!("changes.total", hits.len())                     // {n} is the count
/// tn!("settings.language.line_problems", lines.len())  // and so on
/// ```
///
/// Both examples name keys the app really uses, because the test that checks
/// the language files reads this file too and cannot tell a doc comment from a
/// call site — an invented key here is an invented key it will go looking for.
#[macro_export]
macro_rules! tn {
    ($key:literal, $n:expr) => {
        $crate::i18n::plural($key, ($n) as u64, &[])
    };
    ($key:literal, $n:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::plural(
            $key,
            ($n) as u64,
            &[$((stringify!($name), ::std::string::ToString::to_string(&$value))),+],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The assumption the whole fallback chain rests on.
    #[test]
    fn english_is_the_first_embedded_language() {
        let english = Catalogue::parse(EMBEDDED[0]);
        assert_eq!(english.code, "en");
        assert!(!english.is_empty());
    }

    #[test]
    fn every_embedded_language_parses_without_a_single_problem() {
        for text in EMBEDDED {
            let catalogue = Catalogue::parse(text);
            assert!(
                catalogue.problems.is_empty(),
                "{}: {:?}",
                catalogue.code,
                catalogue.problems
            );
            assert!(
                !catalogue.name.trim().is_empty(),
                "{} has no name",
                catalogue.code
            );
        }
    }

    #[test]
    fn language_codes_are_unique_and_compared_loosely() {
        let codes: Vec<String> = EMBEDDED
            .iter()
            .map(|text| normalise(&Catalogue::parse(text).code))
            .collect();
        let mut sorted = codes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "two languages share a code");

        assert!(same_code("pt_BR", "PT-br"));
        assert!(!same_code("pt-BR", "pt"));
        assert_eq!(primary("fr-CA"), "fr");
    }

    /// Someone whose system is set to Canadian French should get French, not
    /// English, when only `fr` is installed.
    ///
    /// Asked of a list built here rather than of the running app, because
    /// English is at present the only language in the binary and this is a
    /// promise made to the second one, whenever it arrives.
    #[test]
    fn a_regional_code_falls_back_to_the_plain_language() {
        let catalogues: Vec<Catalogue> = ["en", "fr", "pt-BR"]
            .iter()
            .map(|code| Catalogue::parse(&format!("code = \"{code}\"\n")))
            .collect();

        assert_eq!(choose(&catalogues, "fr-CA"), 1, "fr-CA should reach fr");
        assert_eq!(choose(&catalogues, "fr"), 1);
        assert_eq!(choose(&catalogues, "pt-br"), 2, "codes match loosely");
        // Brazilian Portuguese is the nearest thing to Portuguese here.
        assert_eq!(choose(&catalogues, "pt-PT"), 2);
        // And an unknown language lands on English rather than on nothing.
        assert_eq!(choose(&catalogues, "xx"), 0);

        with_language("en", || assert_eq!(set_language("xx"), "en"));
    }

    /// The three ways a file in the languages folder can fail to become a
    /// language, each of which used to vanish into a `log` call this app has no
    /// implementation for — leaving a translator staring at a picker their file
    /// was not in, with nothing anywhere saying why.
    #[test]
    fn a_file_that_cannot_be_used_says_which_and_why() {
        use std::hash::{BuildHasher as _, RandomState};

        let dir = std::env::temp_dir().join(format!(
            "accessengine-lang-test-{:016x}",
            RandomState::new().hash_one(std::process::id())
        ));
        std::fs::create_dir_all(&dir).expect("a temp folder");

        std::fs::write(
            dir.join("good.toml"),
            "code = \"nl\"\nname = \"Nederlands\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("nameless.toml"), "name = \"Mystery\"\n").unwrap();
        std::fs::write(
            dir.join("english.toml"),
            "code = \"EN\"\nname = \"English\"\n",
        )
        .unwrap();
        // Not a language file at all, and not anybody's mistake either.
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();

        // Assembled rather than merely read, because the English collision is
        // only detectable once a file is matched against the loaded languages.
        let registry = Registry::assemble(read_folder(&dir));
        let _ = std::fs::remove_dir_all(&dir);

        // Dutch joined the two built-in languages; nothing else did.
        let codes: Vec<&str> = registry
            .catalogues
            .iter()
            .map(|c| c.code.as_str())
            .collect();
        assert_eq!(codes, ["en", "nl"]);

        let mut said: Vec<(&str, &FileReason)> = registry
            .folder_problems
            .iter()
            .map(|p| (p.path.file_name().unwrap().to_str().unwrap(), &p.reason))
            .collect();
        said.sort_by_key(|(name, _)| *name);
        assert_eq!(
            said,
            [
                // Capitalised `EN` still counts as English, since codes are
                // matched case-insensitively.
                ("english.toml", &FileReason::WouldReplaceEnglish),
                ("nameless.toml", &FileReason::NoCode),
            ]
        );
    }

    #[test]
    fn a_missing_key_comes_back_as_itself_rather_than_as_blank() {
        with_language("en", || {
            assert_eq!(text("no.such.key.exists", &[]), "no.such.key.exists");
        });
    }

    /// A translation that has fallen behind still says something everywhere.
    ///
    /// English is the only language in the binary today, so this is what holds
    /// the fallback honest: every key answers, and answers with words.
    #[test]
    fn nothing_comes_back_empty() {
        let english = english();
        with_language("en", || {
            for key in english.keys() {
                assert!(!text(key, &[]).is_empty(), "`{key}` came back empty");
            }
        });
    }

    // ------------------------------------------------- holding the files honest
    //
    // The tests below are the whole safety net for a system whose keys the
    // compiler cannot check. Between them they mean a translator can only break
    // their own file, and only in ways the app reports back to them.

    use std::collections::BTreeSet;

    fn english() -> Catalogue {
        Catalogue::parse(EMBEDDED[0])
    }

    /// Every `.rs` file in the crate, read off disk at test time.
    fn sources() -> Vec<String> {
        fn walk(dir: &std::path::Path, into: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("the source tree").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, into);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    into.push(std::fs::read_to_string(&path).expect("a source file"));
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        files
    }

    /// Every key written as a literal after the given macro opener.
    ///
    /// A deliberately literal scan rather than a parse: it can only find keys
    /// written out in full at the call site, which is exactly the rule this
    /// system asks call sites to follow. Whitespace between the bracket and the
    /// quote is skipped, because rustfmt puts a long call on several lines.
    ///
    /// Anything found that is not shaped like a key is discarded — most of it
    /// being this function's own source, which the scan reads along with every
    /// other file and which necessarily contains the text it searches for.
    fn literal_keys(text: &str, opener: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find(opener) {
            // `tn!(` ends in `n!(`, so scanning for `t!(` must not match it.
            let preceded_by_word = rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            rest = &rest[at + opener.len()..];
            if preceded_by_word {
                continue;
            }
            let argument = rest.trim_start();
            let Some(quoted) = argument.strip_prefix('"') else {
                continue;
            };
            let Some(end) = quoted.find('"') else {
                continue;
            };
            let key = &quoted[..end];
            let shaped_like_a_key = !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
            if shaped_like_a_key {
                found.push(key.to_string());
            }
        }
        found
    }

    /// The keys the app looks up, split into the plain ones and the counted
    /// ones — the second set being how a plural key is told from an ordinary
    /// key that happens to end in `.other`.
    ///
    /// Every key in this app is a literal at its call site, month names
    /// included, so there is nothing here to list by hand and nothing that can
    /// drift out of step with the scan.
    fn keys_used() -> (BTreeSet<String>, BTreeSet<String>) {
        let (mut plain, mut counted) = (BTreeSet::new(), BTreeSet::new());
        for file in sources() {
            plain.extend(literal_keys(&file, "t!("));
            counted.extend(literal_keys(&file, "tn!("));
        }
        (plain, counted)
    }

    /// Every key, counted ones included.
    fn all_keys_used() -> BTreeSet<String> {
        let (mut plain, counted) = keys_used();
        plain.extend(counted);
        plain
    }

    /// A counted key is written `categories.foreign` at the call site and
    /// `categories.foreign.one` in the file, so a file key is reduced to its
    /// stem before the two are compared — but only when the call sites really
    /// do treat it as counted, or an ordinary key ending in `.other` would be
    /// mistaken for a plural and held to forms it has no use for.
    fn stem<'a>(key: &'a str, counted: &BTreeSet<String>) -> &'a str {
        for suffix in [".one", ".few", ".many", ".other"] {
            if let Some(head) = key.strip_suffix(suffix) {
                if counted.contains(head) {
                    return head;
                }
            }
        }
        key
    }

    #[test]
    fn every_key_the_app_asks_for_is_in_the_english_file() {
        let english = english();
        let (_, counted) = keys_used();
        let known: BTreeSet<&str> = english.keys().map(|key| stem(key, &counted)).collect();
        let used = all_keys_used();
        let missing: Vec<&String> = used
            .iter()
            .filter(|key| !known.contains(key.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "asked for but not in en.toml: {missing:#?}"
        );
    }

    /// The other direction, which is what stops the file growing entries that
    /// every translator then dutifully translates for nothing.
    #[test]
    fn every_line_in_the_english_file_is_asked_for_somewhere() {
        let english = english();
        let (_, counted) = keys_used();
        let used = all_keys_used();
        let unused: Vec<&str> = english
            .keys()
            .map(|key| stem(key, &counted))
            .filter(|key| !used.contains(*key))
            .collect();
        assert!(unused.is_empty(), "in en.toml but never used: {unused:#?}");
    }

    /// The rest of a call's arguments, from just past its key to the bracket
    /// that closes the call.
    ///
    /// Anything nested inside a further pair of brackets is left out, because
    /// the values these messages take are sometimes other messages — a count
    /// arriving as `duration = tn!("duration.minutes", …)` — and without this
    /// the inner call's own bindings would be read as the outer one's. `None` if the call is never closed, which means the
    /// scan has wandered into something that was not a call to begin with.
    fn argument_list(after_key: &str) -> Option<String> {
        let mut out = String::new();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut chars = after_key.chars();

        while let Some(c) = chars.next() {
            if in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' | '[' => depth += 1,
                ')' | ']' if depth == 0 => return Some(out),
                ')' | ']' => depth -= 1,
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        None
    }

    /// The names bound in an argument list: every `name =` in it, which is the
    /// only form these macros accept.
    fn names_bound(args: &str) -> BTreeSet<String> {
        let chars: Vec<char> = args.chars().collect();
        let mut names = BTreeSet::new();

        for (at, c) in chars.iter().enumerate() {
            if *c != '=' {
                continue;
            }
            // `==` and its relatives are comparisons, not bindings. Nothing in
            // an argument list should be comparing anything, but a scan that
            // reads them as bindings would report a name nobody wrote.
            let doubled = chars.get(at + 1) == Some(&'=')
                || (at > 0 && matches!(chars[at - 1], '=' | '!' | '<' | '>'));
            if doubled {
                continue;
            }

            let mut end = at;
            while end > 0 && chars[end - 1].is_whitespace() {
                end -= 1;
            }
            let mut start = end;
            while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
                start -= 1;
            }
            if start < end {
                names.insert(chars[start..end].iter().collect());
            }
        }
        names
    }

    /// Every call to the given macro, as the key it asks for and the names it
    /// binds.
    ///
    /// The same literal scan as [`literal_keys`], carried one step further —
    /// and discarding anything not shaped like a key for the same reason, this
    /// function's own source being among the text it reads.
    fn calls(text: &str, opener: &str) -> Vec<(String, BTreeSet<String>)> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find(opener) {
            let preceded_by_word = rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            rest = &rest[at + opener.len()..];
            if preceded_by_word {
                continue;
            }
            let argument = rest.trim_start();
            let Some(quoted) = argument.strip_prefix('"') else {
                continue;
            };
            let Some(end) = quoted.find('"') else {
                continue;
            };
            let key = &quoted[..end];
            let shaped_like_a_key = !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
            if !shaped_like_a_key {
                continue;
            }
            let Some(args) = argument_list(&quoted[end + 1..]) else {
                continue;
            };
            found.push((key.to_owned(), names_bound(&args)));
        }
        found
    }

    /// A placeholder the file declares and the call site never fills reaches
    /// the user as a literal `{year}` in the middle of a sentence — the
    /// substituter re-emits an unmatched name rather than guessing at one.
    ///
    /// Neither test either side of this one can see it. The file-against-file
    /// check below compares translations with English, and a placeholder
    /// nobody binds is one both files carry and both are equally wrong about;
    /// the key-coverage checks above look at keys and never at what goes into
    /// them. This is the only thing standing between a renamed value and a
    /// sentence with a brace in it.
    #[test]
    fn every_placeholder_a_message_uses_is_bound_where_it_is_asked_for() {
        let english = english();
        let mut unbound: Vec<String> = Vec::new();

        for file in sources() {
            // `tn!` binds `n` itself, so it is never the call site's to pass.
            for (opener, given) in [("t!(", ""), ("tn!(", "n")] {
                for (key, bound) in calls(&file, opener) {
                    // A counted key is written bare at the call site and lives
                    // in the file under its forms, any of which may carry the
                    // placeholder. A key English has never heard of is the
                    // business of the test above, not this one.
                    let wanted: BTreeSet<String> = [
                        key.clone(),
                        format!("{key}.one"),
                        format!("{key}.few"),
                        format!("{key}.many"),
                        format!("{key}.other"),
                    ]
                    .iter()
                    .filter_map(|candidate| english.get(candidate))
                    .flat_map(catalogue::placeholders)
                    .filter(|name| name != given)
                    .collect();

                    for name in wanted.difference(&bound) {
                        unbound.push(format!("`{key}` never binds {{{name}}}"));
                    }
                }
            }
        }

        unbound.sort();
        unbound.dedup();
        assert!(
            unbound.is_empty(),
            "these would print a brace at the user: {unbound:#?}"
        );
    }

    /// The failure this catches reaches the user as a literal `{name}` printed
    /// in the middle of a sentence — which is why it is caught here instead.
    ///
    /// English is the only language in the binary today, so this and the two
    /// below have nothing to say yet. They are the standard a second language
    /// is held to on the day it is added, and adding one is meant to be a file
    /// and a line in [`EMBEDDED`], not a test to write as well.
    #[test]
    fn every_translation_keeps_the_placeholders_english_uses() {
        let english = english();
        for text in &EMBEDDED[1..] {
            let other = Catalogue::parse(text);
            for key in other.keys() {
                let (Some(theirs), Some(ours)) = (other.get(key), english.get(key)) else {
                    // A key English does not have is caught by the test below.
                    continue;
                };
                let mut theirs = catalogue::placeholders(theirs);
                let mut ours = catalogue::placeholders(ours);
                theirs.sort();
                theirs.dedup();
                ours.sort();
                ours.dedup();
                assert_eq!(
                    theirs, ours,
                    "{}: `{key}` uses different placeholders from English",
                    other.code
                );
            }
        }
    }

    /// A shipped language is held to the full set. Anything less is a language
    /// that falls back to English mid-sentence, which reads worse than English
    /// throughout.
    #[test]
    fn every_shipped_language_is_complete() {
        let english = english();
        for text in &EMBEDDED[1..] {
            let other = Catalogue::parse(text);
            let theirs: BTreeSet<&str> = other.keys().collect();
            let ours: BTreeSet<&str> = english.keys().collect();

            let missing: Vec<&&str> = ours.difference(&theirs).collect();
            assert!(
                missing.is_empty(),
                "{} is missing: {missing:#?}",
                other.code
            );
            let extra: Vec<&&str> = theirs.difference(&ours).collect();
            assert!(
                extra.is_empty(),
                "{} has entries English does not: {extra:#?}",
                other.code
            );
        }
    }

    /// Whatever plural rule a language declares, it has to supply every form
    /// that rule can ask for — otherwise the hole only shows up on the day
    /// somebody records exactly three entries.
    #[test]
    fn every_counted_message_has_the_forms_its_rule_needs() {
        let (_, counted) = keys_used();
        for text in EMBEDDED {
            let catalogue = Catalogue::parse(text);
            for key in &counted {
                for category in catalogue.plural.categories() {
                    let wanted = format!("{key}.{}", category.suffix());
                    assert!(
                        catalogue.contains(&wanted),
                        "{}: `{wanted}` is missing",
                        catalogue.code
                    );
                }
            }
        }
    }

    /// The bundled faces have to be able to draw every language that ships, or
    /// it arrives as rows of `?` — a failure invisible to everyone whose own
    /// language renders perfectly well, which is to say invisible to whoever
    /// merges the translation.
    ///
    /// The charmaps are read straight out of the font files the app installs,
    /// rather than asked of `Fonts::has_glyph`, which answers false for every
    /// character sharing a face with the replacement glyph — that is most of
    /// the arrows and symbols already in the app's own labels.
    #[test]
    fn the_bundled_fonts_can_draw_every_shipped_language() {
        use ab_glyph::{Font as _, FontRef};

        let definitions = crate::theme::font_definitions();
        let chain = definitions
            .families
            .get(&egui::FontFamily::Proportional)
            .expect("the proportional family");
        let faces: Vec<FontRef<'_>> = chain
            .iter()
            .map(|name| {
                let data = definitions.font_data.get(name).expect("a named font");
                FontRef::try_from_slice(&data.font).expect("a readable font file")
            })
            .collect();
        let covered = |c: char| faces.iter().any(|face| face.glyph_id(c).0 != 0);

        for text in EMBEDDED {
            let catalogue = Catalogue::parse(text);
            let mut uncovered: Vec<char> = catalogue
                .keys()
                .filter_map(|key| catalogue.get(key))
                .chain(std::iter::once(catalogue.name.as_str()))
                // Space and the like are laid out, never drawn.
                .flat_map(str::chars)
                .filter(|c| !c.is_whitespace() && !c.is_control() && !covered(*c))
                .collect();
            uncovered.sort_unstable();
            uncovered.dedup();
            assert!(
                uncovered.is_empty(),
                "{}: no bundled font draws {uncovered:?}",
                catalogue.code
            );
        }
    }
}


//! Every word the interface says, looked up by key instead of written in place.
//!
//! An app built for people who cannot see the screen depends completely on the
//! words a screen reader announces, so a reader speaking English at someone who
//! does not read English is the same problem as unreadable small print. This
//! module is what lets those words be written in another language by somebody
//! who does not program: language files are plain text, they can be dropped
//! into a folder without rebuilding anything, and see [`catalogue`] for the
//! format.
//!
//! Three properties are deliberate.
//!
//! **English is always there.** It is compiled into the binary and any key a
//! translation has not reached yet falls back to it, so a language file is
//! useful from its first line rather than only its last. Nobody has to finish
//! before they can test.
//!
//! **Lookups work from any thread.** Job threads build their own status
//! messages — [`crate::jobs::Update::Status`] carries a finished `String` — so
//! the active language has to be reachable from them as well as from the UI.
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
const EMBEDDED: &[&str] = &[
    include_str!("../../assets/lang/en.toml"),
    include_str!("../../assets/lang/fr.toml"),
];

/// The value bound to `{C}` in every lookup: the modifier key this platform
/// actually uses.
///
/// Bound centrally rather than left to each call site because it appears in
/// most of the tooltips, and a translator should be able to move it around a
/// sentence like any other placeholder without knowing what it will become.
pub const MODIFIER: &str = if cfg!(target_os = "macos") {
    "⌘"
} else {
    "Ctrl+"
};

/// The language files that are loaded, and which one is in use.
struct Registry {
    /// Index 0 is always English.
    catalogues: Vec<Catalogue>,
    current: usize,
}

impl Registry {
    fn load() -> Self {
        let mut catalogues: Vec<Catalogue> =
            EMBEDDED.iter().map(|text| Catalogue::parse(text)).collect();

        // A file in the user's folder replaces the built-in language of the
        // same code. That is what lets a translator improve a shipped
        // translation, not only add a new one.
        for extra in folder_catalogues() {
            match catalogues
                .iter()
                .position(|existing| same_code(&existing.code, &extra.code))
            {
                // Never index 0: English is the fallback everything else is
                // measured against, and an incomplete file replacing it would
                // leave keys with nothing behind them at all.
                Some(0) => continue,
                Some(index) => catalogues[index] = extra,
                None => catalogues.push(extra),
            }
        }

        Self {
            catalogues,
            current: 0,
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

/// Where a translator's own language files live.
///
/// Beside the config rather than beside the binary: on macOS the binary is
/// inside a signed app bundle, and a folder the user is invited to put files
/// into should not be one where adding a file breaks the signature.
pub fn languages_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("io", "accessengine", "accessengine")?;
    Some(dirs.config_dir().join("languages"))
}

/// Reads every `.toml` in the languages folder. A folder that is missing is the
/// normal case, not an error.
fn folder_catalogues() -> Vec<Catalogue> {
    let Some(dir) = languages_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            crate::log::line(format!(
                "language file could not be read: {}",
                path.display()
            ));
            continue;
        };
        let catalogue = Catalogue::parse(&text);
        if catalogue.code.trim().is_empty() {
            crate::log::line(format!(
                "language file has no `code` line, ignored: {}",
                path.display()
            ));
            continue;
        }
        crate::log::line(format!(
            "language file loaded: {} ({}, {} entries, {} problems)",
            path.display(),
            catalogue.code,
            catalogue.keys().count(),
            catalogue.problems.len()
        ));
        found.push(catalogue);
    }
    found
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

    let exact = registry
        .catalogues
        .iter()
        .position(|c| same_code(&c.code, code));
    let loose = || {
        registry
            .catalogues
            .iter()
            .position(|c| primary(&c.code) == primary(code))
    };

    registry.current = exact.or_else(loose).unwrap_or(0);
    registry.current().code.clone()
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
/// Asked of the OS rather than read from `LANG`, which on macOS is whatever the
/// terminal happened to export and is missing entirely for an app launched from
/// the Finder. Environment variables are the last resort rather than the first.
pub fn system_language() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
            .ok()?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !value.is_empty() {
                return Some(value.replace('_', "-"));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let output = crate::sysexec::powershell("(Get-Culture).Name")
            .output()
            .ok()?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }

    for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            // `fr_FR.UTF-8` — the encoding is no use here.
            let tag = value.split('.').next().unwrap_or_default().trim();
            if !tag.is_empty() && tag != "C" && tag != "POSIX" {
                return Some(tag.replace('_', "-"));
            }
        }
    }
    None
}

/// Resolves the saved setting to a language and selects it.
///
/// `auto` asks the operating system; anything else is a code the user picked
/// in Settings. Returns the code actually in use, which the caller shows.
pub fn apply_setting(setting: &str) -> String {
    let wanted = if setting.trim().eq_ignore_ascii_case("auto") || setting.trim().is_empty() {
        system_language().unwrap_or_else(|| "en".to_string())
    } else {
        setting.to_string()
    };
    let chosen = set_language(&wanted);
    crate::log::line(format!("language: {setting} resolved to {chosen}"));
    chosen
}

/// Runs `body` with `code` in use, then puts the language back.
///
/// The active language is process-wide, and `cargo test` runs tests in threads
/// that share it — so any test whose result depends on the language has to hold
/// this lock, including the ones that expect English. Without it a test asking
/// what the English prompt says can be answered in French by a test running
/// beside it, and the failure appears once in every few dozen runs.
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

/// Whether `value` is still the shipped prompt for `key` in *some* language,
/// meaning the user has not written their own.
///
/// Checked against every language rather than only the current one, which is
/// what makes switching language a second and a third time keep working: after
/// English to French, the saved prompt is French, and it is still nobody's
/// writing but ours.
///
/// The comparison is trimmed but otherwise exact. Anything else risks deciding
/// that a prompt somebody carefully edited is close enough to a default to
/// throw away, and losing someone's writing is far worse than leaving a prompt
/// in the previous language with a note saying so.
pub fn is_untouched_prompt(key: &str, value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }
    let registry = REGISTRY.read().expect("language registry");
    registry
        .catalogues
        .iter()
        .filter_map(|catalogue| catalogue.get(key))
        .any(|shipped| shipped.trim() == value)
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

/// Fills in the supplied values plus `{C}`, which every string may use.
fn substitute(template: &str, values: &[(&str, String)]) -> String {
    let mut pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
    if !pairs.iter().any(|(name, _)| *name == "C") {
        pairs.push(("C", MODIFIER));
    }
    catalogue::fill(template, &pairs)
}

/// A word from the interface's vocabulary.
///
/// ```ignore
/// t!("read.file.caption")                   // plain
/// t!("status.loaded", name = file_name)     // {name} substituted
/// ```
///
/// Every string may also use `{C}` for this platform's modifier key without
/// binding anything.
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
/// tn!("status.reading", count)                        // {n} is the count
/// tn!("settings.diagnostics.copied", lines)           // and so on
/// ```
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
    #[test]
    fn a_regional_code_falls_back_to_the_plain_language() {
        with_language("en", || {
            assert_eq!(set_language("fr-CA"), "fr");
            assert_eq!(set_language("fr"), "fr");
            // And an unknown language lands on English rather than on nothing.
            assert_eq!(set_language("xx"), "en");
        });
    }

    #[test]
    fn a_missing_key_comes_back_as_itself_rather_than_as_blank() {
        with_language("en", || {
            assert_eq!(text("no.such.key.exists", &[]), "no.such.key.exists");
        });
    }

    /// The one placeholder no call site has to remember to bind.
    #[test]
    fn the_modifier_key_is_always_available() {
        assert_eq!(substitute("{C}O", &[]), format!("{MODIFIER}O"));
        // And an explicit binding still wins, so nothing is trapped.
        assert_eq!(substitute("{C}", &[("C", "Alt+".to_string())]), "Alt+");
    }

    #[test]
    fn auto_resolves_to_something_that_exists() {
        with_language("en", || {
            let chosen = apply_setting("auto");
            assert!(available().iter().any(|(code, _)| *code == chosen));
        });
    }

    /// The rule that decides whether switching language may replace somebody's
    /// prompt. Getting it wrong in one direction leaves a French app asking the
    /// model in English; in the other, it silently deletes something the user
    /// wrote themselves, which is much the worse of the two.
    #[test]
    fn a_prompt_is_only_replaceable_while_it_is_still_ours() {
        let english = english();
        let french = Catalogue::parse(EMBEDDED[1]);

        for key in ["prompt.vision", "prompt.frame", "prompt.narration"] {
            let ours = english.get(key).expect("an English prompt");
            let theirs = french.get(key).expect("a French prompt");

            assert!(is_untouched_prompt(key, ours));
            // Every shipped language counts, not only the one in use — which is
            // what lets the language be changed a second and a third time.
            assert!(is_untouched_prompt(key, theirs));
            // Whitespace around it is not authorship.
            assert!(is_untouched_prompt(key, &format!("\n  {ours}  \n")));
            // Nor is an empty box.
            assert!(is_untouched_prompt(key, "   "));

            // But one word of the user's own makes the whole thing theirs.
            assert!(!is_untouched_prompt(key, &format!("{ours} Be brief.")));
            assert!(!is_untouched_prompt(key, "Read the sign."));
        }
    }

    /// A translation not yet started still leaves a working app, which is the
    /// property that lets somebody test their file from its first line.
    #[test]
    fn an_untranslated_key_falls_back_to_english() {
        let english = english();
        with_language("fr", || {
            for key in english.keys().take(20) {
                assert!(
                    !text(key, &[]).is_empty(),
                    "`{key}` came back empty in French"
                );
            }
        });
    }

    // ------------------------------------------------- holding the files honest
    //
    // The five tests below are the whole safety net for a system whose keys the
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
    /// The handful assembled at run time are computed from the same tables the
    /// app uses rather than listed by hand, so the two cannot drift.
    fn keys_used() -> (BTreeSet<String>, BTreeSet<String>) {
        let (mut plain, mut counted) = (BTreeSet::new(), BTreeSet::new());
        for file in sources() {
            plain.extend(literal_keys(&file, "t!("));
            counted.extend(literal_keys(&file, "tn!("));
        }
        for name in crate::app::SHORTCUTS {
            plain.insert(format!("shortcuts.{name}.keys"));
            plain.insert(format!("shortcuts.{name}.what"));
        }
        for (_, description) in crate::config::VISION_MODELS {
            plain.insert((*description).to_string());
        }
        (plain, counted)
    }

    /// Every key, counted ones included.
    fn all_keys_used() -> BTreeSet<String> {
        let (mut plain, counted) = keys_used();
        plain.extend(counted);
        plain
    }

    /// A counted key is written `status.reading` at the call site and
    /// `status.reading.one` in the file, so a file key is reduced to its stem
    /// before the two are compared — but only when the call sites really do
    /// treat it as counted, or an ordinary key ending in `.other` would be
    /// mistaken for a plural and held to forms it has no use for.
    fn stem<'a>(key: &'a str, counted: &BTreeSet<String>) -> &'a str {
        for suffix in [".one", ".few", ".many", ".other"] {
            if let Some(head) = key.strip_suffix(suffix)
                && counted.contains(head)
            {
                return head;
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

    /// The failure this catches reaches the user as a literal `{name}` printed
    /// in the middle of a sentence — which is why it is caught here instead.
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
    /// somebody replaces exactly three words.
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
                .flat_map(str::chars)
                // Space and the like are laid out, never drawn.
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

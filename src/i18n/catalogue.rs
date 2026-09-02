//! The language-file format, and everything that can be done with one file.
//!
//! The format is deliberately small enough to explain in a paragraph, because
//! the person writing one is a translator rather than a programmer and the
//! whole point of this system is that they never have to open Rust:
//!
//! ```text
//! # A comment. Every entry here carries its English source in one of these.
//! code   = "fr"
//! name   = "Français"
//! plural = "french"
//!
//! # Where this app keeps your categories and spending
//! pane.database.subtitle = "Où cette application garde vos catégories et vos dépenses"
//!
//! # A long one, continued by putting another quoted string underneath.
//! pane.spending.no_categories = "Ajoutez d'abord une catégorie — une dépense doit "
//!                               "bien aller quelque part."
//! ```
//!
//! Two decisions are worth the words:
//!
//! **Nothing here fails as a unit.** A line the parser cannot make sense of
//! becomes a [`Problem`] and the rest of the file is still read. A translator
//! mid-file should not be shown an app stripped of its text because of one
//! stray quote, and a user who was sent a well-meant but slightly broken file
//! should still get the ninety percent of it that parses.
//!
//! **Placeholders are named, never positional.** `{name}` rather than `{}`,
//! because word order is the first thing a translation changes and a format
//! that fixes the order of the inserted values is a format that mistranslates
//! by design.

use std::collections::BTreeMap;

/// A line the parser could not use, kept so the Settings pane can say what is
/// wrong with a file instead of silently ignoring part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// 1-based, so it matches what the translator's text editor shows.
    pub line: usize,
    pub what: String,
}

/// How a language forms its plurals.
///
/// Only the rules the shipped languages need are written. This is not an
/// attempt at CLDR: the app has a handful of counted messages, and a rule that
/// exists but has never been checked by someone who speaks the language is
/// worth less than an honest fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluralRule {
    /// One form for 1, one for everything else. English, German, Spanish,
    /// Italian, Dutch, the Scandinavian languages.
    #[default]
    OneOther,
    /// French and Brazilian Portuguese: zero takes the singular too.
    French,
    /// Polish.
    Polish,
    /// Russian, Ukrainian.
    Russian,
}

/// Which of a plural key's variants applies. The names are CLDR's, so a
/// translator who has met the idea elsewhere meets the same words here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    One,
    Few,
    Many,
    Other,
}

impl Category {
    /// The suffix this category wears on the end of a key.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Few => "few",
            Self::Many => "many",
            Self::Other => "other",
        }
    }
}

impl PluralRule {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "one_other" => Some(Self::OneOther),
            "french" => Some(Self::French),
            "polish" => Some(Self::Polish),
            "russian" => Some(Self::Russian),
            _ => None,
        }
    }

    /// Which variant `n` calls for.
    pub fn category(self, n: u64) -> Category {
        // Shared by Polish and Russian: 2, 3, 4 and anything ending in them,
        // except the teens, which behave like the large numbers.
        let slavic_few = matches!(n % 10, 2..=4) && !matches!(n % 100, 12..=14);

        match self {
            Self::OneOther => {
                if n == 1 {
                    Category::One
                } else {
                    Category::Other
                }
            }
            Self::French => {
                if n <= 1 {
                    Category::One
                } else {
                    Category::Other
                }
            }
            Self::Polish => {
                if n == 1 {
                    Category::One
                } else if slavic_few {
                    Category::Few
                } else {
                    Category::Many
                }
            }
            Self::Russian => {
                if n % 10 == 1 && n % 100 != 11 {
                    Category::One
                } else if slavic_few {
                    Category::Few
                } else {
                    Category::Many
                }
            }
        }
    }

    /// Every variant a counted message must supply for this rule. Checked by a
    /// test, so a language cannot ship with a hole that only shows up on the
    /// day someone replaces exactly three words.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn categories(self) -> &'static [Category] {
        match self {
            Self::OneOther | Self::French => &[Category::One, Category::Other],
            Self::Polish | Self::Russian => &[Category::One, Category::Few, Category::Many],
        }
    }
}

/// One language file, parsed.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    /// The language tag: `en`, `fr`, `pt-BR`. Matched case-insensitively.
    pub code: String,
    /// The language's name *in that language*, so the picker reads "Français"
    /// and not "French" — someone looking for their own language is looking
    /// for the word they call it by.
    pub name: String,
    pub plural: PluralRule,
    entries: BTreeMap<String, String>,
    pub problems: Vec<Problem>,
}

impl Catalogue {
    /// Reads a language file. Never fails: whatever could not be understood
    /// ends up in [`Catalogue::problems`].
    pub fn parse(text: &str) -> Self {
        let mut entries = BTreeMap::new();
        let mut problems = Vec::new();
        // The key the next bare quoted string would continue.
        let mut last_key: Option<String> = None;

        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                // A blank line ends a continuation, so a stray quoted string
                // further down the file cannot silently join itself to an
                // entry it was never near.
                if trimmed.is_empty() {
                    last_key = None;
                }
                continue;
            }

            // A line that is only a quoted string continues the entry above.
            if trimmed.starts_with('"') {
                match (&last_key, read_value(trimmed)) {
                    (Some(key), Ok(more)) => {
                        entries.entry(key.clone()).and_modify(|value: &mut String| {
                            value.push_str(&more);
                        });
                    }
                    (None, _) => problems.push(Problem {
                        line,
                        what: "a continued line with no entry above it to continue".to_string(),
                    }),
                    (_, Err(what)) => problems.push(Problem { line, what }),
                }
                continue;
            }

            let Some((key, rest)) = trimmed.split_once('=') else {
                problems.push(Problem {
                    line,
                    what: "no `=`, so this is neither a comment nor an entry".to_string(),
                });
                last_key = None;
                continue;
            };

            let key = key.trim().to_string();
            if let Some(bad) = key.chars().find(|c| !is_key_char(*c)) {
                problems.push(Problem {
                    line,
                    what: format!("`{bad}` cannot appear in a key"),
                });
                last_key = None;
                continue;
            }

            match read_value(rest.trim()) {
                Ok(value) => {
                    if entries.insert(key.clone(), value).is_some() {
                        problems.push(Problem {
                            line,
                            what: format!("`{key}` was already given a value further up"),
                        });
                    }
                    last_key = Some(key);
                }
                Err(what) => {
                    problems.push(Problem { line, what });
                    last_key = None;
                }
            }
        }

        let take = |entries: &mut BTreeMap<String, String>, key: &str| {
            entries.remove(key).unwrap_or_default()
        };
        let code = take(&mut entries, "code");
        let name = take(&mut entries, "name");
        let plural_name = take(&mut entries, "plural");

        // An unrecognised rule is a problem worth reporting rather than a
        // reason to refuse the file: every other line in it is still good, and
        // one-and-other is right for most of Europe.
        let plural = if plural_name.trim().is_empty() {
            PluralRule::default()
        } else {
            PluralRule::parse(&plural_name).unwrap_or_else(|| {
                problems.push(Problem {
                    line: 0,
                    what: format!(
                        "`plural = \"{plural_name}\"` is not a rule this app knows; \
                         using one-and-other"
                    ),
                });
                PluralRule::default()
            })
        };

        Self {
            code,
            name,
            plural,
            entries,
            problems,
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value for a counted key, choosing the variant this language's rule
    /// calls for and falling back along the way rather than giving up: a file
    /// that supplies only `.other` still says something sensible.
    pub fn plural_get(&self, key: &str, n: u64) -> Option<&str> {
        let wanted = self.plural.category(n);
        let ordered = [wanted, Category::Other, Category::Many, Category::One];
        ordered
            .iter()
            .find_map(|category| self.get(&format!("{key}.{}", category.suffix())))
    }
}

/// Whether `c` may appear in a key. Deliberately narrow: keys are written by
/// this project, never by a translator, and anything outside this set is far
/// more likely to be a typo than an intention.
fn is_key_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'
}

/// Reads one `"…"` and whatever follows it, which must be nothing or a comment.
///
/// Trailing comments are allowed because the file's whole convention is that
/// the English source sits beside the translation, and a short one reads better
/// on the same line than above it.
fn read_value(text: &str) -> Result<String, String> {
    let mut chars = text.chars();
    if chars.next() != Some('"') {
        return Err("a value has to be wrapped in double quotes".to_string());
    }

    let mut value = String::new();
    let mut closed = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                closed = true;
                break;
            }
            '\\' => match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('"') => value.push('"'),
                Some('\\') => value.push('\\'),
                Some(other) => {
                    return Err(format!("`\\{other}` is not an escape this format knows"));
                }
                None => return Err("the line ends in a backslash".to_string()),
            },
            _ => value.push(c),
        }
    }

    if !closed {
        return Err("the closing quote is missing".to_string());
    }
    let rest = chars.as_str().trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(format!("`{rest}` is left over after the closing quote"));
    }
    Ok(value)
}

/// Substitutes `{name}` for each supplied value.
///
/// A placeholder with nothing bound to it is left standing rather than blanked,
/// so the mistake shows up as `{name}` in the interface — loud, findable, and
/// caught by a test long before that. `{{` and `}}` write literal braces.
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                match values.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    None if closed => {
                        out.push('{');
                        out.push_str(&name);
                        out.push('}');
                    }
                    None => {
                        out.push('{');
                        out.push_str(&name);
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Every `{name}` in a template, in the order they appear.
///
/// Used by the test that holds translations to the same placeholders as their
/// English source — the mistake that otherwise reaches a user as a literal
/// `{name}` printed in the middle of a sentence.
#[cfg_attr(not(test), allow(dead_code))]
pub fn placeholders(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        names.push(name);
                        break;
                    }
                    name.push(c);
                }
            }
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_file_reads_back_as_written() {
        let cat = Catalogue::parse(
            r#"
                code   = "fr"
                name   = "Français"
                plural = "french"

                # Add New Category
                categories.add = "Ajouter une catégorie"
            "#,
        );
        assert_eq!(cat.code, "fr");
        assert_eq!(cat.name, "Français");
        assert_eq!(cat.plural, PluralRule::French);
        assert_eq!(cat.get("categories.add"), Some("Ajouter une catégorie"));
        assert!(cat.problems.is_empty(), "{:?}", cat.problems);
        // The header is not left behind as a translatable entry.
        assert!(!cat.contains("code"));
    }

    #[test]
    fn a_quoted_line_underneath_continues_the_entry_above() {
        let cat = Catalogue::parse(
            r#"
                settings.hint = "The file is created if it is not there yet, "
                                "along with any folders leading to it."
            "#,
        );
        assert_eq!(
            cat.get("settings.hint"),
            Some(
                "The file is created if it is not there yet, along with any folders leading to it."
            )
        );
        assert!(cat.problems.is_empty(), "{:?}", cat.problems);
    }

    #[test]
    fn a_blank_line_ends_a_continuation() {
        let cat = Catalogue::parse("a = \"one\"\n\n\"stray\"\n");
        assert_eq!(cat.get("a"), Some("one"));
        assert_eq!(cat.problems.len(), 1);
        assert_eq!(cat.problems[0].line, 3);
    }

    #[test]
    fn escapes_and_trailing_comments_are_understood() {
        let cat =
            Catalogue::parse(r#"a = "line\none\ttabbed \"quoted\" \\ done"   # a trailing note"#);
        assert_eq!(cat.get("a"), Some("line\none\ttabbed \"quoted\" \\ done"));
        assert!(cat.problems.is_empty(), "{:?}", cat.problems);
    }

    /// The point of the format: one bad line costs one line, not the file.
    #[test]
    fn a_broken_line_does_not_take_the_rest_of_the_file_with_it() {
        let cat = Catalogue::parse(
            "good.one = \"kept\"\nbroken = \"no closing quote\ngood.two = \"also kept\"\n",
        );
        assert_eq!(cat.get("good.one"), Some("kept"));
        assert_eq!(cat.get("good.two"), Some("also kept"));
        assert!(!cat.contains("broken"));
        assert_eq!(cat.problems.len(), 1);
        assert_eq!(cat.problems[0].line, 2);
    }

    #[test]
    fn every_kind_of_malformed_line_is_reported_with_its_number() {
        let cases = [
            ("no equals sign here", "no `=`"),
            ("Key.Upper = \"x\"", "cannot appear in a key"),
            ("a = unquoted", "double quotes"),
            ("a = \"x\" leftover", "left over"),
            ("a = \"bad \\q escape\"", "not an escape"),
        ];
        for (text, expected) in cases {
            let cat = Catalogue::parse(text);
            assert_eq!(cat.problems.len(), 1, "for {text:?}");
            assert!(
                cat.problems[0].what.contains(expected),
                "for {text:?}, got {:?}",
                cat.problems[0].what
            );
        }
    }

    #[test]
    fn a_key_given_twice_is_reported_rather_than_quietly_taking_the_first() {
        let cat = Catalogue::parse("a = \"first\"\na = \"second\"\n");
        assert_eq!(cat.problems.len(), 1);
        assert!(cat.problems[0].what.contains("already given"));
    }

    #[test]
    fn an_unknown_plural_rule_falls_back_and_says_so() {
        let cat = Catalogue::parse("plural = \"klingon\"\na = \"x\"\n");
        assert_eq!(cat.plural, PluralRule::OneOther);
        assert_eq!(cat.problems.len(), 1);
        assert_eq!(cat.get("a"), Some("x"));
    }

    #[test]
    fn plurals_pick_the_variant_the_language_calls_for() {
        assert_eq!(PluralRule::OneOther.category(0), Category::Other);
        assert_eq!(PluralRule::OneOther.category(1), Category::One);
        assert_eq!(PluralRule::OneOther.category(2), Category::Other);

        // The difference that makes French its own rule.
        assert_eq!(PluralRule::French.category(0), Category::One);
        assert_eq!(PluralRule::French.category(1), Category::One);
        assert_eq!(PluralRule::French.category(2), Category::Other);

        assert_eq!(PluralRule::Polish.category(1), Category::One);
        assert_eq!(PluralRule::Polish.category(3), Category::Few);
        assert_eq!(PluralRule::Polish.category(5), Category::Many);
        // The teens go with the large numbers, which is the whole trap.
        assert_eq!(PluralRule::Polish.category(13), Category::Many);
        assert_eq!(PluralRule::Polish.category(22), Category::Few);

        assert_eq!(PluralRule::Russian.category(21), Category::One);
        assert_eq!(PluralRule::Russian.category(11), Category::Many);
        assert_eq!(PluralRule::Russian.category(2), Category::Few);
    }

    #[test]
    fn a_counted_message_chooses_by_the_number() {
        let cat = Catalogue::parse("n.one = \"{n} word\"\nn.other = \"{n} words\"\n");
        assert_eq!(cat.plural_get("n", 1), Some("{n} word"));
        assert_eq!(cat.plural_get("n", 4), Some("{n} words"));
    }

    /// A translator who has written only `.other` so far gets a working app.
    #[test]
    fn a_missing_variant_falls_back_instead_of_vanishing() {
        let cat = Catalogue::parse("n.other = \"{n} words\"\n");
        assert_eq!(cat.plural_get("n", 1), Some("{n} words"));
        assert_eq!(cat.plural_get("missing", 1), None);
    }

    #[test]
    fn placeholders_are_substituted_by_name() {
        assert_eq!(
            fill(
                "Saved {format} to {path}.",
                &[("format", "CSV"), ("path", "~/Documents")]
            ),
            "Saved CSV to ~/Documents."
        );
    }

    /// The reason the placeholders are named at all: a translation is free to
    /// put them in whatever order its grammar wants.
    #[test]
    fn a_translation_may_reorder_the_values() {
        let values = [("format", "CSV"), ("path", "~/Documents")];
        assert_eq!(
            fill("{format} enregistré dans {path}.", &values),
            "CSV enregistré dans ~/Documents."
        );
    }

    #[test]
    fn an_unbound_placeholder_stays_visible_rather_than_blanking() {
        assert_eq!(fill("Saved {path}.", &[]), "Saved {path}.");
        assert_eq!(fill("Saved {unterminated", &[]), "Saved {unterminated");
    }

    #[test]
    fn doubled_braces_write_one_brace() {
        assert_eq!(fill("{{n}} is {n}", &[("n", "3")]), "{n} is 3");
        assert!(placeholders("{{n}}").is_empty());
    }

    #[test]
    fn placeholders_are_listed_in_order() {
        assert_eq!(
            placeholders("{format} saved as {path}, {path} again"),
            ["format", "path", "path"]
        );
    }
}

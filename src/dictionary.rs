//! Word replacements applied to a document just before it is spoken.
//!
//! Two quite different jobs, one mechanism. The first is pronunciation: a
//! synthesiser that says "Siobhan" wrongly will say "Shi-vawn" correctly, and a
//! reader who needs that fix needs it in every document, not once. The second is
//! substitution — swapping a word for a gentler one so a document can be read
//! out in a room with children in it.
//!
//! Matching is case-insensitive and the replacement takes on the case of what it
//! replaced, so one rule covers "shit", "Shit" at the start of a sentence, and
//! "SHIT" in a heading without the user writing three.
//!
//! The original file is never touched. Replacement happens on the way to the
//! voice, so a rule can be added or removed and the next Apply uses it.

use serde::{Deserialize, Serialize};

/// One rule: say `to` wherever the document says `from`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Replacement {
    pub from: String,
    pub to: String,
    /// Match complete words only — "cat" leaves "catalogue" alone. On by
    /// default, because the alternative surprises people.
    pub whole_word: bool,
}

impl Default for Replacement {
    fn default() -> Self {
        Self {
            from: String::new(),
            to: String::new(),
            whole_word: true,
        }
    }
}

impl Replacement {
    /// A rule with nothing to look for does nothing, and is skipped rather than
    /// treated as an error — a half-typed row in the editor is a normal state.
    pub fn is_usable(&self) -> bool {
        !self.from.trim().is_empty()
    }
}

/// Applies every rule in order, returning the new text and how many
/// replacements were made.
///
/// Rules are applied one after another over the whole text, so a later rule can
/// act on what an earlier one produced. That is predictable to explain and lets
/// rules build on each other; it does mean rule order matters, which is why the
/// editor keeps them in the order they were added.
pub fn apply(text: &str, rules: &[Replacement]) -> (String, usize) {
    let mut text = text.to_string();
    let mut total = 0;
    for rule in rules.iter().filter(|rule| rule.is_usable()) {
        let (next, count) = apply_one(&text, rule);
        text = next;
        total += count;
    }
    (text, total)
}

fn apply_one(text: &str, rule: &Replacement) -> (String, usize) {
    let haystack: Vec<char> = text.chars().collect();
    let needle: Vec<char> = rule.from.trim().chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return (text.to_string(), 0);
    }

    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut index = 0;
    while index < haystack.len() {
        if matches_at(&haystack, &needle, index, rule.whole_word) {
            let matched = &haystack[index..index + needle.len()];
            out.push_str(&recase(&rule.to, matched));
            // Continue *after* the match, so a rule whose replacement contains
            // its own trigger — "cat" to "cat sound" — cannot loop.
            index += needle.len();
            count += 1;
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    (out, count)
}

/// True if `needle` sits at `index`, ignoring case, and — when `whole_word` is
/// set — is not glued to a letter or digit on either side.
fn matches_at(haystack: &[char], needle: &[char], index: usize, whole_word: bool) -> bool {
    let end = index + needle.len();
    if end > haystack.len() {
        return false;
    }
    if !haystack[index..end]
        .iter()
        .zip(needle)
        .all(|(a, b)| same_letter(*a, *b))
    {
        return false;
    }
    if whole_word {
        let before_is_word = index
            .checked_sub(1)
            .is_some_and(|i| is_word_char(haystack[i]));
        let after_is_word = haystack.get(end).copied().is_some_and(is_word_char);
        if before_is_word || after_is_word {
            return false;
        }
    }
    true
}

/// Case-insensitive comparison that also works outside ASCII, so "CAFÉ" matches
/// a rule written as "café".
fn same_letter(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Apostrophes count as part of a word so that a rule for "dont" does not fire
/// inside "don't", which would leave a stray "'t" behind.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '\'' || ch == '’'
}

/// Gives `replacement` the leading capital of the text it is replacing, so one
/// rule covers a word mid-sentence and the same word starting one.
///
/// A word in full capitals is deliberately *not* reproduced in full capitals.
/// The only reader of this text is a speech synthesiser, and several of them
/// spell an all-capitals word out letter by letter — so "GNU" replaced by
/// "guh-noo" must not become "GUH-NOO", or the fix causes the very problem it
/// was written to solve.
fn recase(replacement: &str, matched: &[char]) -> String {
    let starts_with_capital = matched
        .iter()
        .find(|c| c.is_alphabetic())
        .is_some_and(|c| c.is_uppercase());
    if !starts_with_capital {
        return replacement.to_string();
    }
    let mut chars = replacement.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => replacement.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Replacement, apply};

    fn rule(from: &str, to: &str) -> Replacement {
        Replacement {
            from: from.to_string(),
            to: to.to_string(),
            whole_word: true,
        }
    }

    #[test]
    fn replaces_a_word_and_counts_it() {
        let (text, count) = apply("The cat sat.", &[rule("cat", "dog")]);
        assert_eq!(text, "The dog sat.");
        assert_eq!(count, 1);
    }

    #[test]
    fn whole_word_matching_leaves_longer_words_alone() {
        let (text, count) = apply("A catalogue of cats and one cat.", &[rule("cat", "dog")]);
        assert_eq!(text, "A catalogue of cats and one dog.");
        assert_eq!(count, 1);
    }

    #[test]
    fn partial_matching_is_available_when_asked_for() {
        let mut partial = rule("cat", "dog");
        partial.whole_word = false;
        let (text, count) = apply("catalogue", &[partial]);
        assert_eq!(text, "dogalogue");
        assert_eq!(count, 1);
    }

    /// The substitution case from the brief: one rule, every capitalisation.
    #[test]
    fn the_replacement_takes_the_leading_capital_of_what_it_replaced() {
        let (text, count) = apply("shit. Shit! SHIT?", &[rule("shit", "shite")]);
        // Note the last one: capitals are not carried over wholesale, because a
        // synthesiser may spell an all-capitals word out letter by letter.
        assert_eq!(text, "shite. Shite! Shite?");
        assert_eq!(count, 3);
    }

    /// The pronunciation case: a name respelt phonetically.
    #[test]
    fn multi_word_phrases_are_replaced() {
        let (text, _) = apply(
            "Ask Siobhan about the GNU licence.",
            &[rule("Siobhan", "Shivawn"), rule("GNU", "guh-noo")],
        );
        assert_eq!(text, "Ask Shivawn about the Guh-noo licence.");
    }

    #[test]
    fn matching_ignores_case_in_the_rule_itself() {
        let (text, count) = apply("MRI scan", &[rule("mri", "em arr eye")]);
        assert_eq!(text, "Em arr eye scan");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_rule_containing_its_own_trigger_does_not_loop() {
        let (text, count) = apply("cat", &[rule("cat", "cat sound")]);
        assert_eq!(text, "cat sound");
        assert_eq!(count, 1);
    }

    #[test]
    fn rules_are_applied_in_order_so_they_can_build_on_each_other() {
        let (text, count) = apply("one", &[rule("one", "two"), rule("two", "three")]);
        assert_eq!(text, "three");
        assert_eq!(count, 2);
    }

    #[test]
    fn empty_and_half_written_rules_are_ignored() {
        let (text, count) = apply("unchanged", &[rule("", "something"), rule("   ", "x")]);
        assert_eq!(text, "unchanged");
        assert_eq!(count, 0);
    }

    #[test]
    fn a_rule_with_no_replacement_removes_the_word() {
        let (text, count) = apply("well um yes", &[rule("um", "")]);
        assert_eq!(text, "well  yes");
        assert_eq!(count, 1);
    }

    #[test]
    fn matching_works_beyond_ascii() {
        let (text, count) = apply("CAFÉ and café", &[rule("café", "kaffay")]);
        assert_eq!(text, "Kaffay and kaffay");
        assert_eq!(count, 2);
    }

    #[test]
    fn an_apostrophe_keeps_a_word_intact() {
        // "dont" must not match inside "don't" and leave "'t" stranded.
        let (text, count) = apply("don't", &[rule("dont", "do not")]);
        assert_eq!(text, "don't");
        assert_eq!(count, 0);
    }

    #[test]
    fn punctuation_and_line_breaks_are_word_boundaries() {
        let (text, count) = apply("(cat)\ncat, cat.", &[rule("cat", "dog")]);
        assert_eq!(text, "(dog)\ndog, dog.");
        assert_eq!(count, 3);
    }

    #[test]
    fn text_with_no_matches_is_returned_unchanged() {
        let original = "Nothing here matches the rule at all.";
        let (text, count) = apply(original, &[rule("absent", "present")]);
        assert_eq!(text, original);
        assert_eq!(count, 0);
    }
}

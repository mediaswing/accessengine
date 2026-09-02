//! Wordlists: user-editable rules applied to text before it is spoken.
//!
//! Two jobs, one mechanism:
//!   * make a document safe to read out in a classroom or an open-plan office
//!   * fix pronunciation of names, jargon and abbreviations
//!
//! File format (plain text, editable in any editor):
//!
//! ```text
//! # comment
//! [pronounce]
//! Gloucester = Gloster
//! SQL = sequel
//!
//! [replace]
//! damn = darn
//!
//! [block]
//! rudeword
//! swear*
//! ```
//!
//! Matching is case-insensitive and whole-word. Multi-word phrases on the left
//! are matched as phrases. A trailing or leading `*` is a wildcard, so `swear*`
//! catches "swearing" and "swears".

use crate::t;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// What a matching rule does to the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleKind {
    /// Unsafe for the audience: handled according to [`BlockPolicy`].
    Block,
    /// Swap for a specific, milder word supplied by the list author.
    Replace,
    /// Say it differently; not a safety concern.
    Pronounce,
}

impl RuleKind {
    /// What to call this kind of rule on screen.
    ///
    /// Separate from [`fmt::Display`], which the log uses: a log line read
    /// months later by whoever is diagnosing the problem is easier to search
    /// for in one language.
    pub fn label(&self) -> String {
        match self {
            RuleKind::Block => t!("rule.blocked"),
            RuleKind::Replace => t!("rule.replaced"),
            RuleKind::Pronounce => t!("rule.pronunciation"),
        }
    }
}

impl fmt::Display for RuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RuleKind::Block => "blocked",
            RuleKind::Replace => "replaced",
            RuleKind::Pronounce => "pronunciation",
        };
        f.write_str(s)
    }
}

/// How to handle a word matched by a `[block]` rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockPolicy {
    /// Say a placeholder instead (default: "beep").
    Bleep,
    /// Say nothing at all in place of the word.
    Remove,
    /// Drop the whole sentence containing the word.
    SkipSentence,
}

impl BlockPolicy {
    pub const ALL: [BlockPolicy; 3] = [
        BlockPolicy::Bleep,
        BlockPolicy::Remove,
        BlockPolicy::SkipSentence,
    ];

    pub fn label(&self) -> String {
        match self {
            BlockPolicy::Bleep => t!("block.bleep"),
            BlockPolicy::Remove => t!("block.nothing"),
            BlockPolicy::SkipSentence => t!("block.skip"),
        }
    }
}

#[derive(Clone, Debug)]
struct Rule {
    kind: RuleKind,
    /// `None` for `[block]` entries, which defer to the [`BlockPolicy`].
    replacement: Option<String>,
    /// Which list this came from, so the UI can explain a change.
    origin: String,
}

/// How a pattern with `*` in it should be matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wild {
    Prefix, // "swear*"
    Suffix, // "*ing"
    Both,   // "*swear*"
}

#[derive(Clone, Debug)]
struct WildRule {
    stem: String,
    wild: Wild,
    rule: Rule,
}

/// One loaded wordlist file.
#[derive(Clone, Debug)]
pub struct Wordlist {
    pub path: PathBuf,
    pub name: String,
    pub enabled: bool,
    /// Exact (possibly multi-word) matches, keyed by lowercased phrase.
    exact: HashMap<String, Rule>,
    wild: Vec<WildRule>,
    /// Longest phrase in `exact`, in words. Bounds the n-gram scan.
    max_phrase_words: usize,
    pub counts: [usize; 3], // block, replace, pronounce
}

impl Wordlist {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading wordlist {}", path.display()))?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "wordlist".to_string());
        Ok(Self::parse(&text, name, path.to_path_buf()))
    }

    pub fn parse(text: &str, name: String, path: PathBuf) -> Self {
        let mut exact: HashMap<String, Rule> = HashMap::new();
        let mut wild: Vec<WildRule> = Vec::new();
        let mut max_phrase_words = 1usize;
        let mut counts = [0usize; 3];
        // Entries before any [section] header are treated as blocks, which is
        // what a bare list of words dropped in by a teacher will look like.
        let mut section = RuleKind::Block;

        for raw in text.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = match header.trim().to_ascii_lowercase().as_str() {
                    "pronounce" | "pronunciation" | "say" => RuleKind::Pronounce,
                    "replace" | "swap" => RuleKind::Replace,
                    _ => RuleKind::Block,
                };
                continue;
            }

            let (pattern, replacement) = match line.split_once('=') {
                Some((l, r)) => (l.trim(), Some(r.trim().to_string())),
                None => (line, None),
            };
            if pattern.is_empty() {
                continue;
            }

            // A `[block]` entry may still carry an explicit replacement, in
            // which case it behaves like `[replace]` but still counts as a
            // safety change in the review panel.
            let kind = section;
            let rule = Rule {
                kind,
                replacement: replacement.filter(|r| !r.is_empty()),
                origin: name.clone(),
            };
            counts[match kind {
                RuleKind::Block => 0,
                RuleKind::Replace => 1,
                RuleKind::Pronounce => 2,
            }] += 1;

            let lower = pattern.to_lowercase();
            let starred_start = lower.starts_with('*');
            let starred_end = lower.ends_with('*');
            if starred_start || starred_end {
                let stem = lower.trim_matches('*').to_string();
                if stem.is_empty() {
                    continue;
                }
                let w = match (starred_start, starred_end) {
                    (true, true) => Wild::Both,
                    (true, false) => Wild::Suffix,
                    _ => Wild::Prefix,
                };
                wild.push(WildRule {
                    stem,
                    wild: w,
                    rule,
                });
            } else {
                let words = lower.split_whitespace().count().max(1);
                max_phrase_words = max_phrase_words.max(words);
                // Normalise internal whitespace so "New  York" matches "New York".
                let key = lower.split_whitespace().collect::<Vec<_>>().join(" ");
                exact.insert(key, rule);
            }
        }

        Wordlist {
            path,
            name,
            enabled: true,
            exact,
            wild,
            max_phrase_words,
            counts,
        }
    }
}

fn strip_comment(line: &str) -> &str {
    // `#` starts a comment, but only at the start of a line or after
    // whitespace, so a replacement like "C# = C sharp" survives.
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// Lists shipped with the app. They are written into the user's wordlist
/// folder on first run and never touched again, so edits always survive an
/// upgrade.
pub const BUNDLED: &[(&str, &str)] = &[
    (
        "pronunciation.wordlist",
        include_str!("../assets/wordlists/pronunciation.wordlist"),
    ),
    (
        "classroom-safe.wordlist",
        include_str!("../assets/wordlists/classroom-safe.wordlist"),
    ),
];

const WORDLIST_EXTENSIONS: &[&str] = &["wordlist", "txt", "list"];

/// Write the bundled lists into `dir`, skipping any that already exist.
pub fn install_bundled(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in BUNDLED {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        std::fs::write(&path, contents)
            .with_context(|| format!("writing {}", path.display()))?;
        log::info!("installed bundled wordlist {}", path.display());
    }
    Ok(())
}

/// Load every wordlist in `dir`. Unreadable files are logged and skipped
/// rather than taking the whole app down.
pub fn discover(dir: &Path, disabled: &[String]) -> Vec<Wordlist> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::warn!("no wordlist folder at {}", dir.display());
        return Vec::new();
    };

    let mut lists: Vec<Wordlist> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .map(|x| x.to_string_lossy().to_ascii_lowercase())
                    .is_some_and(|x| WORDLIST_EXTENSIONS.contains(&x.as_str()))
        })
        .filter_map(|p| match Wordlist::load(&p) {
            Ok(list) => Some(list),
            Err(e) => {
                log::error!("skipping wordlist {}: {e:#}", p.display());
                None
            }
        })
        .collect();

    for list in &mut lists {
        list.enabled = !disabled.contains(&list.name);
    }
    lists.sort_by(|a, b| a.name.cmp(&b.name));
    log::info!("loaded {} wordlists from {}", lists.len(), dir.display());
    lists
}

/// One change made to a chunk of text, for the "what was changed" panel.
#[derive(Clone, Debug)]
pub struct Hit {
    pub original: String,
    pub replacement: String,
    pub kind: RuleKind,
    pub origin: String,
}

/// The result of running the wordlists over one chunk of text.
#[derive(Clone, Debug, Default)]
pub struct Applied {
    pub text: String,
    pub hits: Vec<Hit>,
    /// True when a `[block]` rule fired under [`BlockPolicy::SkipSentence`].
    pub skipped: bool,
}

/// All enabled wordlists plus the policy for handling blocked words.
#[derive(Clone, Debug)]
pub struct WordlistSet {
    pub lists: Vec<Wordlist>,
    pub policy: BlockPolicy,
    pub bleep_text: String,
}

impl Default for WordlistSet {
    fn default() -> Self {
        Self {
            lists: Vec::new(),
            policy: BlockPolicy::Bleep,
            bleep_text: "beep".to_string(),
        }
    }
}

impl WordlistSet {
    pub fn active_count(&self) -> usize {
        self.lists.iter().filter(|l| l.enabled).count()
    }

    pub fn has_active_rules(&self) -> bool {
        self.lists
            .iter()
            .any(|l| l.enabled && (!l.exact.is_empty() || !l.wild.is_empty()))
    }

    fn max_phrase_words(&self) -> usize {
        self.lists
            .iter()
            .filter(|l| l.enabled)
            .map(|l| l.max_phrase_words)
            .max()
            .unwrap_or(1)
    }

    /// Look a phrase up across every enabled list. Earlier lists win, and
    /// within a list Block beats Replace beats Pronounce, so a safety rule is
    /// never masked by a pronunciation entry for the same word.
    fn lookup(&self, phrase: &str) -> Option<&Rule> {
        let mut best: Option<&Rule> = None;
        for list in self.lists.iter().filter(|l| l.enabled) {
            if let Some(r) = list.exact.get(phrase) {
                best = Some(match best {
                    Some(prev) if priority(prev.kind) <= priority(r.kind) => prev,
                    _ => r,
                });
            }
        }
        best
    }

    fn lookup_wild(&self, word: &str) -> Option<&Rule> {
        let mut best: Option<&Rule> = None;
        for list in self.lists.iter().filter(|l| l.enabled) {
            for wr in &list.wild {
                let hit = match wr.wild {
                    Wild::Prefix => word.starts_with(&wr.stem),
                    Wild::Suffix => word.ends_with(&wr.stem),
                    Wild::Both => word.contains(&wr.stem),
                };
                if hit {
                    best = Some(match best {
                        Some(prev) if priority(prev.kind) <= priority(wr.rule.kind) => prev,
                        _ => &wr.rule,
                    });
                }
            }
        }
        best
    }

    /// Rewrite one chunk of text according to the enabled lists.
    pub fn apply(&self, input: &str) -> Applied {
        if !self.has_active_rules() {
            return Applied {
                text: input.to_string(),
                hits: Vec::new(),
                skipped: false,
            };
        }

        let tokens = tokenize(input);
        let max_words = self.max_phrase_words();
        let mut out = String::with_capacity(input.len());
        let mut hits = Vec::new();
        let mut skipped = false;

        // Indices of `tokens` that are words (as opposed to separators).
        let word_positions: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_word)
            .map(|(i, _)| i)
            .collect();

        let mut consumed_until = 0usize; // token index
        let mut wi = 0usize; // index into word_positions

        while wi < word_positions.len() {
            let start_tok = word_positions[wi];
            if start_tok < consumed_until {
                wi += 1;
                continue;
            }

            // Try the longest phrase first, down to a single word.
            let mut matched: Option<(usize, &Rule)> = None; // (end token idx exclusive, rule)
            let remaining_words = word_positions.len() - wi;
            for n in (1..=max_words.min(remaining_words)).rev() {
                let end_tok = word_positions[wi + n - 1] + 1;
                let phrase: String = tokens[start_tok..end_tok]
                    .iter()
                    .map(|t| t.text.as_str())
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if let Some(rule) = self.lookup(&phrase) {
                    matched = Some((end_tok, rule));
                    break;
                }
            }

            // Fall back to wildcard rules, single words only.
            if matched.is_none() {
                let word_lower = tokens[start_tok].text.to_lowercase();
                if let Some(rule) = self.lookup_wild(&word_lower) {
                    matched = Some((start_tok + 1, rule));
                }
            }

            match matched {
                Some((end_tok, rule)) => {
                    // Emit any separators sitting before this match.
                    for t in &tokens[consumed_until..start_tok] {
                        out.push_str(&t.text);
                    }
                    let original: String = tokens[start_tok..end_tok]
                        .iter()
                        .map(|t| t.text.as_str())
                        .collect();

                    let replacement = match (rule.kind, &rule.replacement) {
                        // A pronunciation respelling is used exactly as
                        // written. Inheriting the source's case would turn
                        // `SQL = sequel` into "SEQUEL", and several back ends
                        // spell all-caps words out letter by letter — the very
                        // thing the rule exists to prevent.
                        (RuleKind::Pronounce, Some(r)) => r.clone(),
                        (_, Some(r)) => match_case(&original, r),
                        (RuleKind::Block, None) => match self.policy {
                            BlockPolicy::Bleep => match_case(&original, &self.bleep_text),
                            BlockPolicy::Remove => String::new(),
                            BlockPolicy::SkipSentence => {
                                skipped = true;
                                String::new()
                            }
                        },
                        // A `[replace]`/`[pronounce]` entry with no `= value`
                        // has nothing to say; leave the word alone.
                        (_, None) => original.clone(),
                    };

                    if replacement != original {
                        hits.push(Hit {
                            original: original.clone(),
                            replacement: replacement.clone(),
                            kind: rule.kind,
                            origin: rule.origin.clone(),
                        });
                    }
                    out.push_str(&replacement);
                    consumed_until = end_tok;
                    // Advance past every word we just swallowed.
                    while wi < word_positions.len() && word_positions[wi] < end_tok {
                        wi += 1;
                    }
                }
                None => {
                    wi += 1;
                }
            }
        }

        for t in &tokens[consumed_until..] {
            out.push_str(&t.text);
        }

        if skipped {
            return Applied {
                text: String::new(),
                hits,
                skipped: true,
            };
        }

        if hits.is_empty() {
            // Nothing fired: hand back the original bytes rather than a
            // whitespace-normalised copy, so untouched documents are untouched.
            return Applied {
                text: input.to_string(),
                hits,
                skipped: false,
            };
        }

        Applied {
            text: tidy_spaces(&out),
            hits,
            skipped: false,
        }
    }
}

fn priority(kind: RuleKind) -> u8 {
    match kind {
        RuleKind::Block => 0,
        RuleKind::Replace => 1,
        RuleKind::Pronounce => 2,
    }
}

struct Token {
    text: String,
    is_word: bool,
}

/// Split into alternating word / non-word runs, preserving everything so the
/// text can be reassembled byte-for-byte if nothing matches.
fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_word = false;

    for (i, &ch) in chars.iter().enumerate() {
        // Apostrophes and hyphens sit inside words ("don't", "twenty-one").
        let mut is_word_char = ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}' || ch == '-';
        // `#` and `+` belong to the word only when welded straight onto it, so
        // "C#" and "C++" are matchable but a markdown "# Heading" is not.
        if !is_word_char
            && (ch == '#' || ch == '+')
            && current_is_word
            && current.chars().next_back().is_some_and(|p| {
                p.is_alphanumeric() || p == '#' || p == '+'
            })
        {
            is_word_char = true;
        }
        // A full stop between two alphanumerics is part of the word, not the
        // end of a sentence: "e.g", "Node.js", "U.S.A", "3.14". The final stop
        // of "e.g." is followed by a space, so it stays a separator — which is
        // why such rules are written without their trailing dot.
        if !is_word_char
            && ch == '.'
            && i > 0
            && chars[i - 1].is_alphanumeric()
            && chars.get(i + 1).is_some_and(|c| c.is_alphanumeric())
        {
            is_word_char = true;
        }
        if current.is_empty() {
            current_is_word = is_word_char;
        } else if is_word_char != current_is_word {
            tokens.push(Token {
                text: std::mem::take(&mut current),
                is_word: current_is_word,
            });
            current_is_word = is_word_char;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(Token {
            text: current,
            is_word: current_is_word,
        });
    }
    tokens
}

/// Make a replacement follow the capitalisation of the word it replaces, so
/// "Damn it" becomes "Darn it" rather than "darn it".
fn match_case(original: &str, replacement: &str) -> String {
    let letters: Vec<char> = original.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return replacement.to_string();
    }
    let all_upper = letters.len() > 1 && letters.iter().all(|c| c.is_uppercase());
    if all_upper {
        return replacement.to_uppercase();
    }
    if letters[0].is_uppercase() {
        let mut chars = replacement.chars();
        return match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
    }
    replacement.to_string()
}

/// Removing a word leaves doubled spaces and stranded punctuation; clean up so
/// the synthesiser does not pause oddly.
fn tidy_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            last_was_space = false;
            out.push(ch);
        }
    }
    // " ." -> "." and " ," -> ","
    let mut cleaned = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' && i + 1 < chars.len() && matches!(chars[i + 1], '.' | ',' | '!' | '?' | ';' | ':')
        {
            i += 1;
            continue;
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(src: &str) -> WordlistSet {
        WordlistSet {
            lists: vec![Wordlist::parse(src, "test".into(), PathBuf::from("test"))],
            ..Default::default()
        }
    }

    #[test]
    fn pronounces_words_and_preserves_punctuation() {
        let s = set("[pronounce]\nGloucester = Gloster\nSQL = sequel");
        let r = s.apply("We drove to Gloucester, then wrote SQL.");
        assert_eq!(r.text, "We drove to Gloster, then wrote sequel.");
        assert_eq!(r.hits.len(), 2);
    }

    #[test]
    fn blocks_with_placeholder_and_matches_case() {
        let s = set("[block]\nrudeword");
        let r = s.apply("That RUDEWORD again.");
        assert_eq!(r.text, "That BEEP again.");
        assert_eq!(r.hits[0].kind, RuleKind::Block);
    }

    #[test]
    fn skip_sentence_policy_drops_the_chunk() {
        let mut s = set("[block]\nrudeword");
        s.policy = BlockPolicy::SkipSentence;
        let r = s.apply("That rudeword again.");
        assert!(r.skipped);
        assert!(r.text.is_empty());
    }

    #[test]
    fn remove_policy_tidies_spacing() {
        let mut s = set("[block]\nrudeword");
        s.policy = BlockPolicy::Remove;
        let r = s.apply("That rudeword .");
        assert_eq!(r.text, "That.");
    }

    #[test]
    fn wildcards_match_word_families() {
        let s = set("[block]\nswear*");
        let r = s.apply("No swearing please.");
        assert_eq!(r.text, "No beep please.");
    }

    #[test]
    fn multi_word_phrases_win_over_single_words() {
        let s = set("[pronounce]\nSQL Server = sequel server\nSQL = ess queue ell");
        let r = s.apply("Install SQL Server now.");
        assert_eq!(r.text, "Install sequel server now.");
    }

    #[test]
    fn block_beats_pronounce_for_the_same_word() {
        let s = set("[pronounce]\nfoo = eff oh oh\n[block]\nfoo");
        let r = s.apply("A foo here.");
        assert_eq!(r.text, "A beep here.");
    }

    /// Case is inherited for safety substitutions, so a sentence still reads
    /// correctly, but never for pronunciation respellings.
    #[test]
    fn case_is_inherited_only_where_it_helps() {
        let replace = set("[replace]\ndamn = darn");
        assert_eq!(replace.apply("Damn it.").text, "Darn it.");
        assert_eq!(replace.apply("DAMN it.").text, "DARN it.");

        let pronounce = set("[pronounce]\nSQL = sequel");
        assert_eq!(pronounce.apply("Use SQL.").text, "Use sequel.");
    }

    #[test]
    fn untouched_text_round_trips_exactly() {
        let s = set("[block]\nnothingmatches");
        let input = "Line one.\n\nLine  two —  with dashes.";
        assert_eq!(s.apply(input).text, input);
    }

    #[test]
    fn hash_inside_a_replacement_is_not_a_comment() {
        let s = set("[pronounce]\nC# = C sharp");
        assert_eq!(s.apply("I like C#.").text, "I like C sharp.");
    }

    /// Abbreviations keep their internal stops but not the sentence-ending one.
    #[test]
    fn dotted_abbreviations_match() {
        let s = set("[pronounce]\ne.g = for example\nNode.js = node jay ess");
        assert_eq!(
            s.apply("Use a runtime, e.g. Node.js today.").text,
            "Use a runtime, for example. node jay ess today."
        );
    }

    /// Decimals must not be torn in half by the same rule.
    #[test]
    fn decimal_numbers_stay_whole() {
        let s = set("[block]\n14");
        assert_eq!(s.apply("Pi is 3.14 exactly.").text, "Pi is 3.14 exactly.");
    }

    /// The lists that actually ship must parse and do something. A typo in an
    /// asset file is otherwise invisible until a user hits it.
    #[test]
    fn bundled_lists_parse_and_apply() {
        let mut s = WordlistSet::default();
        for (name, contents) in BUNDLED {
            let list = Wordlist::parse(contents, (*name).to_string(), PathBuf::from(name));
            assert!(
                list.counts.iter().sum::<usize>() > 0,
                "{name} parsed to no rules at all"
            );
            s.lists.push(list);
        }

        let out = s.apply("We drove to Gloucester and wrote SQL, damn it.");
        assert!(out.text.contains("Gloster"), "{}", out.text);
        assert!(out.text.contains("sequel"), "{}", out.text);
        assert!(out.text.contains("darn"), "{}", out.text);
        assert!(!out.text.contains("damn"), "{}", out.text);
    }

    #[test]
    fn contractions_stay_whole() {
        let s = set("[block]\ndon");
        // "don't" must not be split into "don" + "'t".
        assert_eq!(s.apply("I don't mind.").text, "I don't mind.");
    }
}

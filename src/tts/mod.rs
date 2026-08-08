//! The two speech engines, and the text preparation they share.

pub mod elevenlabs;
pub mod system;

/// A voice offered by either engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    /// Engine-specific identifier: a voice id for ElevenLabs, the `say` name
    /// on macOS.
    pub id: String,
    pub name: String,
    /// Language tag or category, shown after the name to tell similar voices apart.
    pub detail: String,
}

impl Voice {
    pub fn display(&self) -> String {
        if self.detail.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, self.detail)
        }
    }
}

/// Splits text into pieces no longer than `max_chars`, preferring to break at
/// paragraph then sentence then word boundaries so the synthesised chunks join
/// without audible clipping mid-word.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    assert!(max_chars > 0, "max_chars must be positive");

    let mut chunks = Vec::new();
    let mut current = String::new();

    for unit in split_units(text) {
        // A single unit longer than the limit has to be broken by force.
        if unit.chars().count() > max_chars {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            current = String::new();
            chunks.extend(split_hard(&unit, max_chars));
            continue;
        }
        if current.chars().count() + unit.chars().count() > max_chars {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            current = String::new();
        }
        current.push_str(&unit);
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

/// Breaks text after sentence-ending punctuation and after newlines, keeping
/// the delimiter attached to the unit it ends.
fn split_units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        current.push(ch);
        let ends_sentence = matches!(ch, '.' | '!' | '?' | '。' | '！' | '？' | '\n')
            && chars
                .peek()
                .is_none_or(|next| next.is_whitespace() || *next == '"' || *next == '\'');
        if ends_sentence {
            // Absorb the whitespace that follows so chunks don't start with it.
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                current.push(chars.next().unwrap());
            }
            units.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

/// Last resort for a "sentence" that is longer than a whole chunk: break on
/// whitespace, and on character boundaries if even that fails.
fn split_hard(unit: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in unit.split_inclusive(char::is_whitespace) {
        if word.chars().count() > max_chars {
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            // `current` is replaced by the trailing remainder below, so there
            // is no need to clear it first.
            let mut piece = String::new();
            for ch in word.chars() {
                if piece.chars().count() >= max_chars {
                    chunks.push(std::mem::take(&mut piece));
                }
                piece.push(ch);
            }
            current = piece;
            continue;
        }
        if current.chars().count() + word.chars().count() > max_chars {
            chunks.push(current.trim().to_string());
            current = String::new();
        }
        current.push_str(word);
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_stays_in_one_chunk() {
        assert_eq!(chunk_text("Hello there.", 100), vec!["Hello there."]);
    }

    #[test]
    fn breaks_between_sentences_not_inside_them() {
        let text = "One two three. Four five six. Seven eight nine.";
        for chunk in chunk_text(text, 30) {
            assert!(chunk.chars().count() <= 30, "chunk too long: {chunk:?}");
            assert!(chunk.ends_with('.'), "chunk broke mid-sentence: {chunk:?}");
        }
    }

    #[test]
    fn every_chunk_respects_the_limit_even_without_punctuation() {
        let text = "word ".repeat(500);
        let chunks = chunk_text(&text, 40);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 40, "chunk too long: {chunk:?}");
        }
    }

    #[test]
    fn a_single_oversized_token_is_split_rather_than_dropped() {
        let text = "a".repeat(95);
        let chunks = chunk_text(&text, 10);
        assert_eq!(chunks.len(), 10);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn no_text_is_lost_when_chunking() {
        let text = "Alpha beta. Gamma delta epsilon! Zeta?\n\nEta theta iota kappa lambda.";
        let rejoined: String = chunk_text(text, 20).join(" ");
        let strip = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(strip(&rejoined), strip(text));
    }

    /// The case the app is actually asked about: a document well past
    /// ElevenLabs' 10,000-character per-request ceiling. It has to come apart
    /// into requests the API will accept and go back together with every word
    /// present and in order — there is no length at which speaking has to be
    /// refused.
    #[test]
    fn a_document_past_the_api_limit_splits_and_reassembles_intact() {
        const API_LIMIT: usize = 10_000;

        let sentences: Vec<String> = (0..900)
            .map(|n| format!("This is sentence number {n} of a rather long document."))
            .collect();
        let text = sentences.join(" ");
        assert!(
            text.chars().count() > 3 * API_LIMIT,
            "the fixture should be several requests long"
        );

        // 4,500 is what the ElevenLabs client uses; the point is that it is
        // under the limit and that nothing is lost at the seams.
        let chunks = chunk_text(&text, 4_500);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 4_500, "chunk too long to send");
            assert!(chunk.chars().count() <= API_LIMIT);
        }

        let strip = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(strip(&chunks.join(" ")), strip(&text));
        // Word order matters as much as word count: a listener would hear it.
        assert!(chunks[0].starts_with("This is sentence number 0"));
        assert!(chunks.last().unwrap().ends_with("document."));
    }

    #[test]
    fn text_exactly_at_the_limit_is_not_split() {
        let text = "a".repeat(4_500);
        assert_eq!(chunk_text(&text, 4_500).len(), 1);
        assert_eq!(chunk_text(&"a".repeat(4_501), 4_500).len(), 2);
    }

    #[test]
    fn multibyte_text_is_not_split_mid_character() {
        let text = "日本語のテキストです。これは二番目の文です。";
        let chunks = chunk_text(text, 12);
        assert_eq!(chunks.concat().chars().count(), text.chars().count());
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 12);
        }
    }
}

//! Turning text into the terms an index is built from.
//!
//! This is the first thing that touches a document and the last thing that
//! touches a query, and both sides have to agree, so there is one function here
//! and both call it. A tokeniser that is even slightly different on the two
//! sides produces an index that cannot find the words it contains, and that bug
//! is invisible until someone searches for the one word that hits the
//! difference.
//!
//! It works on bytes with a character decode only where it has to. Text in the
//! wild is overwhelmingly ASCII, so the common path is a byte comparison per
//! byte, and anything above `0x7f` is decoded and classified properly rather
//! than being waved through as part of a word. Waving it through is quicker and
//! wrong: an em dash between two words would glue them into one term.
//!
//! There is no stemming and no stop word list. Both of them throw away
//! information that a phrase query and a ranking function want back, and both
//! are decisions for a layer that knows what language the field is in.

/// The longest term this will produce, in bytes after lowercasing.
///
/// A term longer than this is truncated at a character boundary rather than
/// dropped, because the thing that produces a four kilobyte word is usually a
/// base64 blob or a minified script, and truncating keeps a prefix that can
/// still be matched while keeping the dictionary from growing without a bound.
pub const MAX_TERM_BYTES: usize = 255;

/// A reusable buffer for turning text into terms.
///
/// The buffer is the whole reason this is a struct. Lowercasing has to write
/// somewhere, and a analyser that allocated per term would spend more time in
/// the allocator than in the scan. One of these is reused across every document
/// in an indexing run.
#[derive(Debug, Default)]
pub struct Analyzer {
    term: Vec<u8>,
}

impl Analyzer {
    /// Creates an analyser with its buffer already sized for the longest term.
    #[must_use]
    pub fn new() -> Self {
        Self {
            term: Vec::with_capacity(MAX_TERM_BYTES),
        }
    }

    /// Calls `on_term` with each term in `text` and its position, and returns
    /// how many terms there were.
    ///
    /// The byte slice handed to the closure is only valid until the next term,
    /// which is what lets this run without allocating. A caller that needs to
    /// keep a term copies it.
    ///
    /// The count that comes back is the field length that BM25 needs, and
    /// counting it here rather than making the caller do it is what keeps the
    /// two definitions of a term from drifting apart.
    pub fn analyze(&mut self, text: &str, mut on_term: impl FnMut(&[u8], u32)) -> u32 {
        let bytes = text.as_bytes();
        let mut at = 0;
        let mut position = 0;
        while at < bytes.len() {
            let (ch, width) = char_at(text, at);
            if standalone(ch) {
                self.emit(&text[at..at + width], &mut position, &mut on_term);
                at += width;
                continue;
            }
            if !ch.is_alphanumeric() {
                at += width;
                continue;
            }
            let start = at;
            at += width;
            while at < bytes.len() {
                let (ch, width) = char_at(text, at);
                if ch.is_alphanumeric() && !standalone(ch) {
                    at += width;
                    continue;
                }
                if joiner(ch) && continues(text, at + width) {
                    at += width;
                    continue;
                }
                break;
            }
            self.emit(&text[start..at], &mut position, &mut on_term);
        }
        position
    }

    /// Lowercases one word into the buffer and hands it over.
    fn emit(&mut self, word: &str, position: &mut u32, on_term: &mut impl FnMut(&[u8], u32)) {
        self.term.clear();
        if word.is_ascii() {
            self.term
                .extend(word.bytes().map(|b| b.to_ascii_lowercase()));
        } else {
            // to_lowercase yields more than one character for a few inputs, the
            // German sharp s being the one everybody meets first, so this
            // cannot be a one character to one character map.
            let mut buffer = [0u8; 4];
            for ch in word.chars() {
                for lower in ch.to_lowercase() {
                    self.term
                        .extend_from_slice(lower.encode_utf8(&mut buffer).as_bytes());
                }
            }
        }
        if self.term.len() > MAX_TERM_BYTES {
            let mut cut = MAX_TERM_BYTES;
            while cut > 0 && self.term[cut] & 0xC0 == 0x80 {
                cut -= 1;
            }
            self.term.truncate(cut);
        }
        on_term(&self.term, *position);
        *position += 1;
    }
}

/// Decodes the character at `at`, which must be a character boundary.
///
/// The ASCII branch is the one that runs, and it is a comparison and a cast
/// rather than a slice and a decode.
#[inline]
fn char_at(text: &str, at: usize) -> (char, usize) {
    let byte = text.as_bytes()[at];
    if byte < 0x80 {
        return (char::from(byte), 1);
    }
    let ch = text[at..]
        .chars()
        .next()
        .expect("at is a character boundary inside the string");
    (ch, ch.len_utf8())
}

/// Whether a character is its own term regardless of what surrounds it.
///
/// Han and kana are written without spaces, so a run of them would otherwise
/// become one term as long as the sentence, which is a term that matches
/// nothing. One character per term at least makes the text findable. Proper
/// segmentation, or the bigrams that most engines fall back to, needs a
/// dictionary of the language, and a dictionary is a dependency this crate does
/// not have.
#[inline]
const fn standalone(ch: char) -> bool {
    matches!(ch,
        '\u{3040}'..='\u{30ff}'   // hiragana and katakana
        | '\u{3400}'..='\u{4dbf}' // unified ideographs, extension A
        | '\u{4e00}'..='\u{9fff}' // unified ideographs
        | '\u{f900}'..='\u{faff}' // compatibility ideographs
    )
}

/// Whether a character holds a word together when a word is on both sides.
///
/// Without this, `don't` becomes `don` and `t`, and `t` is a term that appears
/// in a good fraction of English documents and means nothing in any of them.
#[inline]
const fn joiner(ch: char) -> bool {
    matches!(ch, '\'' | '\u{2019}')
}

/// Whether a word continues at `at`, which is what a joiner has to see ahead of
/// it to be kept.
#[inline]
fn continues(text: &str, at: usize) -> bool {
    if at >= text.len() {
        return false;
    }
    let (ch, _) = char_at(text, at);
    ch.is_alphanumeric() && !standalone(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        let mut analyzer = Analyzer::new();
        let mut out = Vec::new();
        analyzer.analyze(text, |term, _| {
            out.push(String::from_utf8(term.to_vec()).expect("terms stay valid utf8"));
        });
        out
    }

    #[test]
    fn a_sentence_becomes_its_words() {
        assert_eq!(
            terms("the quick brown fox"),
            ["the", "quick", "brown", "fox"]
        );
    }

    #[test]
    fn punctuation_separates_and_is_not_kept() {
        assert_eq!(terms("a,b;c.d!e?f"), ["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn case_does_not_survive() {
        assert_eq!(terms("The QUICK Fox"), ["the", "quick", "fox"]);
    }

    #[test]
    fn positions_count_up_from_zero() {
        let mut analyzer = Analyzer::new();
        let mut positions = Vec::new();
        let count = analyzer.analyze("one two three", |_, position| positions.push(position));
        assert_eq!(positions, [0, 1, 2]);
        assert_eq!(count, 3);
    }

    #[test]
    fn an_apostrophe_inside_a_word_keeps_the_word_together() {
        assert_eq!(terms("don't it's o'brien"), ["don't", "it's", "o'brien"]);
        assert_eq!(terms("don\u{2019}t"), ["don\u{2019}t"]);
    }

    #[test]
    fn an_apostrophe_at_the_edge_of_a_word_is_a_separator() {
        assert_eq!(terms("'quoted'"), ["quoted"]);
        assert_eq!(terms("dogs' bones"), ["dogs", "bones"]);
    }

    #[test]
    fn digits_are_terms_and_join_letters() {
        assert_eq!(
            terms("utf8 rfc 1234 v2beta"),
            ["utf8", "rfc", "1234", "v2beta"]
        );
    }

    #[test]
    fn accented_letters_are_part_of_the_word_and_are_lowercased() {
        assert_eq!(terms("Café MÜLLER naïve"), ["café", "müller", "naïve"]);
    }

    #[test]
    fn a_dash_that_is_not_ascii_still_separates() {
        // The reason the non ASCII path decodes and classifies rather than
        // assuming anything above 0x7f belongs to a word.
        assert_eq!(terms("east\u{2014}west"), ["east", "west"]);
    }

    #[test]
    fn one_character_lowercases_into_two() {
        // The Turkish dotted capital I lowercases into a letter and a combining
        // dot, which is why the fold cannot be a character for a character.
        assert_eq!(terms("\u{0130}stanbul"), ["i\u{0307}stanbul"]);
    }

    #[test]
    fn han_and_kana_are_one_term_each() {
        assert_eq!(terms("現場"), ["現", "場"]);
        assert_eq!(terms("東京タワー"), ["東", "京", "タ", "ワ", "ー"]);
    }

    #[test]
    fn han_next_to_latin_does_not_swallow_it() {
        assert_eq!(terms("現場genba"), ["現", "場", "genba"]);
        assert_eq!(terms("genba現場"), ["genba", "現", "場"]);
    }

    #[test]
    fn nothing_in_gives_nothing_out() {
        assert!(terms("").is_empty());
        assert!(terms("   ,.;  ").is_empty());
    }

    #[test]
    fn a_term_too_long_is_cut_at_a_character_boundary() {
        let long = "é".repeat(400);
        let out = terms(&long);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() <= MAX_TERM_BYTES);
        // The check that matters: it is still a string, so the cut did not land
        // in the middle of a character.
        assert!(out[0].chars().all(|c| c == 'é'));
        assert_eq!(out[0].len(), 254);
    }

    #[test]
    fn a_term_exactly_at_the_limit_is_kept_whole() {
        let word = "a".repeat(MAX_TERM_BYTES);
        assert_eq!(terms(&word), std::slice::from_ref(&word));
    }

    #[test]
    fn the_same_text_analysed_twice_gives_the_same_terms() {
        // The buffer is reused, so this is the test that it is cleared.
        let text = "Indexing and querying have to agree, or nothing is found.";
        assert_eq!(terms(text), terms(text));
    }
}

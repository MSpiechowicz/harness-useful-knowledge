//! The words of a text as the list filter and the keyword query see them.
//!
//! A word is a run of letters and digits, lowercased, with the accents of the Latin letters
//! removed (`Café` is `cafe`), so a person finds `café` by typing `cafe`. This is the folding the
//! local database applies, with the same exception: `ł` stays as it is.

use std::collections::BTreeSet;

use crate::limits::QUERY_TOKENS_MAX;

/// The most distinct words kept of one chunk: a chunk is at most 510 tokens, so none is cut.
const TERMS_MAX: usize = 600;

/// The letter `character` folds to (lowercase, no accent), or itself lowercased.
fn fold(character: char) -> char {
    const FOLDED: [(&str, char); 14] = [
        ("àáâãäåāăą", 'a'),
        ("çćĉċč", 'c'),
        ("ďđ", 'd'),
        ("èéêëēĕėęě", 'e'),
        ("ĝğġģ", 'g'),
        ("ìíîïĩīĭįı", 'i'),
        ("ñńņňŉ", 'n'),
        ("òóôõöøōŏő", 'o'),
        ("ŕŗř", 'r'),
        ("śŝşš", 's'),
        ("ţťŧ", 't'),
        ("ùúûüũūŭůűų", 'u'),
        ("ýÿŷ", 'y'),
        ("źżž", 'z'),
    ];

    let lower = character.to_lowercase().next().unwrap_or(character);

    FOLDED
        .iter()
        .find(|(accented, _)| accented.contains(lower))
        .map_or(lower, |(_, plain)| *plain)
}

/// `word` folded (see the module docs).
pub(super) fn folded(word: &str) -> String {
    word.chars().map(fold).collect()
}

/// The distinct words of `text`, folded and sorted, for a chunk's `terms` array.
pub(super) fn of_text(text: &str) -> Vec<String> {
    let words: BTreeSet<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(folded)
        .collect();

    words.into_iter().take(TERMS_MAX).collect()
}

/// The first [`QUERY_TOKENS_MAX`] distinct words of a query, lowercased and in the order written
/// (not folded: the search index folds both sides). No word is search syntax.
pub(super) fn query_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();

    for word in text.split(|character: char| !character.is_alphanumeric()) {
        if word.is_empty() || words.len() >= QUERY_TOKENS_MAX {
            continue;
        }

        let word = word.to_lowercase();

        if !words.contains(&word) {
            words.push(word);
        }
    }

    words
}

/// The words of `text` in order, folded, repeats kept: what a phrase is made of.
pub(super) fn phrase_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(QUERY_TOKENS_MAX)
        .map(folded)
        .collect()
}

/// The folded words of `text`, repeats kept and none cut, joined by single spaces.
fn folded_words(text: &str) -> String {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(folded)
        .collect();

    words.join(" ")
}

/// What a chunk stores in `phrase_text` for the phrase filter to match on the server: the folded
/// words of its heading path, a newline, then the folded words of its text, each joined by single
/// spaces. A phrase is words joined by single spaces too, so it cannot span the newline: a phrase
/// is in the heading or in the text, as [`has_phrase`] reads them.
pub(super) fn phrase_text(heading_path: &str, text: &str) -> String {
    format!("{}\n{}", folded_words(heading_path), folded_words(text))
}

/// The regular expression that finds `phrase` in a [`phrase_text`]: its words next to each other,
/// the first starting a word and the last a prefix of one. The words are folded letters and digits,
/// so nothing in them is regex syntax (they are escaped anyway).
pub(super) fn phrase_regex(phrase: &[String]) -> String {
    let words: Vec<String> = phrase.iter().map(|word| regex::escape(word)).collect();

    format!("(^|[ \n]){}", words.join(" "))
}

/// True when `text` holds the `phrase` words next to each other in that order, the last as a
/// prefix of a word (the check of the phrase filter, with the folding of the `terms` index).
pub(super) fn has_phrase(text: &str, phrase: &[String]) -> bool {
    let Some((last, head)) = phrase.split_last() else {
        return false;
    };

    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(folded)
        .collect();

    words
        .windows(phrase.len())
        .any(|window| window[..head.len()] == *head && window[head.len()].starts_with(last.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_is_the_words_next_to_each_other_the_last_as_a_prefix() {
        let phrase = phrase_words("Version 1");

        assert!(has_phrase("Release notes, version 1", &phrase));
        assert!(has_phrase("version 1.2 is out", &phrase), "the last word is a prefix");
        assert!(has_phrase("VERSION\n10", &phrase));
        assert!(!has_phrase("version two and 1 more", &phrase), "scattered words");
        assert!(!has_phrase("1 version", &phrase), "wrong order");
        assert!(has_phrase("Café menu", &phrase_words("cafe me")), "accents fold");
        assert!(!has_phrase("anything", &[]));
        assert!(has_phrase("a b", &phrase_words("b")), "one word is a prefix match");
    }

    #[test]
    fn words_are_lowercase_without_latin_accents_and_l_with_stroke_stays() {
        assert_eq!(of_text("Café PROTÉGÉ, café!"), ["cafe", "protege"]);
        assert_eq!(of_text("Zażółć gęślą"), ["gesla", "zazołc"]);
        assert_eq!(of_text("a_b-c 42"), ["42", "a", "b", "c"]);
        assert!(of_text(" ?! ").is_empty());
    }

    #[test]
    fn query_words_keep_order_drop_repeats_and_cut_a_long_query() {
        assert_eq!(query_words("Git CLI git"), ["git", "cli"]);
        assert_eq!(query_words("title:\"x\" OR *"), ["title", "x", "or"]);
        assert!(query_words("  ?! ").is_empty());

        let text: Vec<String> = (0..100).map(|n| format!("w{n}")).collect();
        assert_eq!(query_words(&text.join(" ")).len(), QUERY_TOKENS_MAX);
    }
}

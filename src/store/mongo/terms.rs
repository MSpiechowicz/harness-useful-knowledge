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

#[cfg(test)]
mod tests {
    use super::*;

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

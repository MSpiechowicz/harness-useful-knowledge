//! Who added a document, as `docs list` filters, sorts and counts it.
//!
//! A name is the `added_by_name` of the row, matched exactly (case-sensitive, the harness it was
//! written through ignored). A row with no name, or an empty one, has no known author. The facet is
//! the count of documents per name under every filter but the author one, so a person can see who
//! else they could pick.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::limits::{AUTHOR_FACET_MAX, AUTHOR_GROUPS_READ_MAX};
use crate::support::author;

/// Which documents a listing keeps by who added them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorFilter {
    /// The documents added by exactly this name.
    Named(String),
    /// The documents with no known author.
    Unknown,
}

impl AuthorFilter {
    /// The filter for a name as the person typed it: trimmed and checked as an author's name is.
    pub fn named(raw: &str) -> Option<Self> {
        author::clean(raw).map(Self::Named)
    }

    /// The name this filter selects, if it names one.
    pub fn name(filter: Option<&Self>) -> Option<&str> {
        match filter? {
            Self::Named(name) => Some(name),
            Self::Unknown => None,
        }
    }

    /// The SQL condition on the name column `column` that keeps these documents; `None` keeps all of
    /// them. `bind` is given the name and returns the text of the parameter that now carries it. A
    /// database without the author columns has no named document and no known author, so a name
    /// keeps none (and binds nothing) and unknown keeps all.
    pub fn condition(
        filter: Option<&Self>,
        column: &str,
        authors: bool,
        bind: impl FnOnce(&str) -> String,
    ) -> Option<String> {
        match filter? {
            Self::Named(_) if !authors => Some("1 = 0".to_owned()),
            Self::Named(name) => Some(format!("nullif({column}, '') = {}", bind(name))),
            Self::Unknown if !authors => None,
            Self::Unknown => Some(format!("({column} IS NULL OR {column} = '')")),
        }
    }
}

/// Most characters of a name a group reads: one more than a name has, so a longer one is still
/// seen to be too long and counts as unknown, without its whole text reaching this process.
pub const NAME_READ_MAX: usize = author::NAME_MAX + 1;

/// Most bytes of a name SQLite and libSQL give to a group: [`author::NAME_MAX`] characters of at
/// most four bytes each. SQLite counts the characters of a text by its lead bytes and stops at a NUL,
/// so a cut by characters says nothing of the bytes that follow, and the gate is on bytes.
pub const NAME_BYTES_MAX: usize = author::NAME_MAX * 4;

/// The SQL of the facet's groups in SQLite and libSQL: the name of each group as the hex of its
/// bytes (NULL for no known author), and its count under `filter` (a `WHERE` clause without the
/// author condition), the unknown row first, then the biggest, at most [`AUTHOR_GROUPS_READ_MAX`]
/// rows.
///
/// A name that is not text, or longer than [`NAME_BYTES_MAX`] bytes, is NULL: unknown, and never
/// read. Hex because a server cannot send text that is not valid UTF-8, and a name with a NUL in it
/// is not cut at it: [`unhex`] and the validation of the facet see every byte. The key is computed
/// in a subquery that is not flattened (`LIMIT -1`), so grouping and sorting hold the short key and
/// not a copy of the whole column. A database without the author columns has one unknown group.
pub fn groups_sql_sqlite(column: &str, authors: bool, filter: &str) -> String {
    if !authors {
        return no_authors_sql(filter);
    }

    let bytes = format!("CAST({column} AS BLOB)");
    let key = format!(
        "CASE WHEN typeof({column}) = 'text' AND length({bytes}) <= {NAME_BYTES_MAX} THEN nullif(hex({bytes}), '') END"
    );

    format!(
        "SELECT k, count(*) FROM (SELECT {key} AS k FROM knowledge_documents d WHERE {filter} LIMIT -1)
         GROUP BY k ORDER BY (k IS NULL) DESC, count(*) DESC LIMIT {AUTHOR_GROUPS_READ_MAX}"
    )
}

/// The SQL of the facet's groups in PostgreSQL: the name (NULL for no known author) cut to
/// [`NAME_READ_MAX`] characters, and its count under `filter`, in the order of [`groups_sql_sqlite`].
/// A name cut short is not a name (it is not what its cleaned form is, or it is too long), so it
/// counts as unknown all the same.
pub fn groups_sql_postgres(column: &str, authors: bool, filter: &str) -> String {
    if !authors {
        return no_authors_sql(filter);
    }

    let key = format!("nullif(left({column}, {NAME_READ_MAX}), '')");

    format!(
        "SELECT {key}, count(*) FROM knowledge_documents d WHERE {filter}
         GROUP BY {key} ORDER BY ({key} IS NULL) DESC, count(*) DESC LIMIT {AUTHOR_GROUPS_READ_MAX}"
    )
}

/// A database without the author columns: every document is one unknown group.
fn no_authors_sql(filter: &str) -> String {
    format!("SELECT CAST(NULL AS TEXT), count(*) FROM knowledge_documents d WHERE {filter}")
}

/// The text whose bytes are written in `hex` (as SQLite's `hex` writes them), `None` when it is not
/// hex or the bytes are not valid UTF-8.
pub fn unhex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }

    let digit = |byte: u8| char::from(byte).to_digit(16).map(|digit| digit as u8);
    let bytes = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect::<Option<Vec<u8>>>()?;

    String::from_utf8(bytes).ok()
}

/// How many documents each person added, and how many have no known author.
///
/// A place reads at most [`AUTHOR_GROUPS_READ_MAX`] groups, the places are added up (see
/// [`Self::merge`]) and the answer is cut to [`AUTHOR_FACET_MAX`] people when it is printed (see
/// [`Self::capped`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorFacet {
    people: BTreeMap<String, usize>,
    unknown: usize,
}

impl AuthorFacet {
    /// The facet of the groups a database answered: a name is `None` for no known author. A name
    /// that is not one as it stands (see [`author::clean`]: an untrimmed, over-long or invisible one
    /// that somebody wrote into the database) counts as unknown and is never shown.
    pub fn from_groups(groups: impl IntoIterator<Item = (Option<String>, usize)>) -> Self {
        let mut facet = Self::default();

        for (name, count) in groups {
            match name {
                Some(name) if author::clean(&name).as_deref() == Some(name.as_str()) => {
                    *facet.people.entry(name).or_default() += count;
                }
                _ => facet.unknown += count,
            }
        }

        facet
    }

    /// Adds the counts of `other` (another place) to these.
    pub fn merge(&mut self, other: Self) {
        for (name, count) in other.people {
            *self.people.entry(name).or_default() += count;
        }

        self.unknown += other.unknown;
    }

    /// The documents with no known author.
    pub fn unknown(&self) -> usize {
        self.unknown
    }

    /// The people, most documents first, then by name with ASCII letters folded to lower case, then
    /// by the name as written.
    pub fn people(&self) -> Vec<(&str, usize)> {
        let mut people: Vec<(&str, usize)> = self
            .people
            .iter()
            .map(|(name, count)| (name.as_str(), *count))
            .collect();

        people.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| fold(a.0).cmp(&fold(b.0)))
                .then_with(|| a.0.cmp(b.0))
        });

        people
    }

    /// This facet cut to [`AUTHOR_FACET_MAX`] people. The person the list is filtered to is always
    /// among them: when they are beyond the cut their count is `total`, the documents the filter
    /// kept (which is also 0 for a name nobody added under the other filters).
    pub fn capped(&self, selected: Option<&str>, total: usize) -> Self {
        let mut people = self.people();

        let kept = selected.is_none_or(|name| people.iter().take(AUTHOR_FACET_MAX).any(|(found, _)| *found == name));

        if kept {
            people.truncate(AUTHOR_FACET_MAX);
        } else if let Some(name) = selected {
            people.retain(|(found, _)| *found != name);
            people.truncate(AUTHOR_FACET_MAX - 1);
            people.push((name, total));
        }

        Self {
            people: people
                .into_iter()
                .map(|(name, count)| (name.to_owned(), count))
                .collect(),
            unknown: self.unknown,
        }
    }

    /// `{"people": [{"name", "count"}], "unknown": N}`.
    pub fn json(&self) -> Value {
        let people: Vec<Value> = self
            .people()
            .into_iter()
            .map(|(name, count)| json!({ "name": name, "count": count }))
            .collect();

        json!({ "people": people, "unknown": self.unknown })
    }
}

/// A name as people are ordered by it: ASCII letters folded to lower case, the rest by bytes, the
/// one fold every store sorts a name by.
pub fn fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: Option<&str>, count: usize) -> (Option<String>, usize) {
        (name.map(str::to_owned), count)
    }

    #[test]
    fn people_are_ordered_by_count_then_folded_name_then_name() {
        let facet = AuthorFacet::from_groups([
            group(Some("bob"), 2),
            group(Some("Ada"), 1),
            group(Some("Zed"), 3),
            group(Some("ada"), 1),
            group(Some("Bob"), 2),
            group(Some("Émile"), 1),
        ]);

        assert_eq!(
            facet.people(),
            [("Zed", 3), ("Bob", 2), ("bob", 2), ("Ada", 1), ("ada", 1), ("Émile", 1)]
        );
    }

    #[test]
    fn a_name_that_is_not_fit_to_show_counts_as_unknown() {
        let facet = AuthorFacet::from_groups([
            group(Some("Ada"), 1),
            group(None, 2),
            group(Some(""), 3),
            group(Some("  "), 4),
            group(Some(" Bob "), 5),
            group(Some("Eve\u{202e}"), 6),
            group(Some(&"x".repeat(81)), 7),
        ]);

        assert_eq!((facet.people(), facet.unknown()), (vec![("Ada", 1)], 27));
        assert!(!facet.json().to_string().contains("202e"));
    }

    #[test]
    fn hex_is_the_text_of_valid_bytes_only() {
        assert_eq!(unhex("416461").as_deref(), Some("Ada"));
        assert_eq!(unhex("").as_deref(), Some(""));
        assert_eq!(unhex("F09F918D").as_deref(), Some("\u{1f44d}"));
        assert_eq!(unhex("41FF"), None, "not UTF-8");
        assert_eq!(unhex("4"), None, "half a byte");
        assert_eq!(unhex("4G"), None, "not hex");

        // A name with a NUL in it is text, and is no known author.
        let facet = AuthorFacet::from_groups([(unhex("416461007461696C"), 1)]);
        assert_eq!((facet.people(), facet.unknown()), (vec![], 1));
    }

    #[test]
    fn places_are_added_up() {
        let mut facet = AuthorFacet::from_groups([group(Some("Ada"), 2), group(None, 1)]);
        facet.merge(AuthorFacet::from_groups([
            group(Some("Ada"), 3),
            group(Some("Bob"), 1),
            group(None, 4),
        ]));

        assert_eq!((facet.people(), facet.unknown()), (vec![("Ada", 5), ("Bob", 1)], 5));
    }

    fn crowd(people: usize) -> AuthorFacet {
        AuthorFacet::from_groups((0..people).map(|n| (Some(format!("Person {n:03}")), 1 + n % 7)))
    }

    #[test]
    fn at_most_two_hundred_people_are_kept() {
        let facet = crowd(250);
        assert_eq!(facet.people().len(), 250);

        let capped = facet.capped(None, 0);
        assert_eq!(capped.people().len(), AUTHOR_FACET_MAX);

        // The ones kept are the biggest.
        let smallest_kept = capped.people().last().map(|(_, count)| *count);
        assert!(
            facet
                .people()
                .iter()
                .skip(AUTHOR_FACET_MAX)
                .all(|(_, count)| Some(*count) <= smallest_kept)
        );
    }

    #[test]
    fn the_selected_person_is_kept_with_the_total_when_beyond_the_cap() {
        let facet = crowd(250);
        let beyond = facet.people()[249].0.to_owned();

        let capped = facet.capped(Some(&beyond), 1);
        let people = capped.people();

        assert_eq!(people.len(), AUTHOR_FACET_MAX);
        assert!(people.contains(&(beyond.as_str(), 1)));

        // A name nobody added under the other filters is there too, with the total (0).
        let capped = facet.capped(Some("Nobody"), 0);
        assert_eq!(capped.people().len(), AUTHOR_FACET_MAX);
        assert!(capped.people().contains(&("Nobody", 0)));

        // A selected person inside the cap changes nothing.
        let first = facet.people()[0].0.to_owned();
        assert_eq!(facet.capped(Some(&first), 99), facet.capped(None, 0));
    }

    #[test]
    fn the_facet_prints_its_people_and_the_unknown_count() {
        let facet = AuthorFacet::from_groups([group(Some("Ada"), 3), group(None, 2)]);

        assert_eq!(
            facet.json(),
            json!({ "people": [{ "name": "Ada", "count": 3 }], "unknown": 2 })
        );
        assert_eq!(AuthorFacet::default().json(), json!({ "people": [], "unknown": 0 }));
    }

    #[test]
    fn a_filter_is_a_trimmed_valid_name_or_unknown() {
        assert_eq!(
            AuthorFilter::named("  Ada "),
            Some(AuthorFilter::Named("Ada".to_owned()))
        );
        assert_eq!(AuthorFilter::named("  "), None);
        assert_eq!(AuthorFilter::named("Ada\nLovelace"), None);
        assert_eq!(AuthorFilter::named(&"x".repeat(81)), None);
    }

    #[test]
    fn without_the_author_columns_a_name_keeps_nothing_and_unknown_keeps_all() {
        let named = AuthorFilter::Named("Ada".to_owned());
        let bind = |_: &str| unreachable!("no column, no parameter");

        assert_eq!(
            AuthorFilter::condition(Some(&named), "d.a", false, bind).as_deref(),
            Some("1 = 0")
        );
        assert_eq!(
            AuthorFilter::condition(Some(&AuthorFilter::Unknown), "d.a", false, bind),
            None
        );
        assert_eq!(AuthorFilter::condition(None, "d.a", true, bind), None);
    }
}

//! What tells a good document from a stale one: who wrote it, how often it helped, and the
//! lookups an import uses to find a document it may replace.
//!
//! The columns behind these are added to a database in place, without a schema version (see
//! [`Columns`]), so an older server keeps using it and a read-only role reads without them.

use std::sync::Once;

use super::StoreError;
use crate::support::author;
use crate::support::redact::redact_text;

/// Who wrote a document: the git `user.name` of the writer, and the MCP client that wrote it for
/// them (`None` for the person's own import).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub name: String,
    pub via: Option<String>,
}

impl Author {
    /// The author a stored name and client make: none without a (valid) name. What a database holds
    /// may have been written by anybody with access to it, so it is cleaned as a new name is: a
    /// name that is too long or holds a control or invisible character is none, the client is cut
    /// to one line of at most 64 characters (see [`crate::support::author`]).
    pub fn stored(name: Option<String>, via: Option<String>) -> Option<Self> {
        let name = author::clean(&name?)?;
        let via = via.as_deref().and_then(author::clean_via);

        Some(Self { name, via })
    }
}

/// How often a document was shown to an agent and how often it helped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocUsage {
    pub shown: i64,
    pub useful: i64,
    /// Unix seconds of the latest time it was found useful.
    pub last_useful_at: Option<i64>,
}

impl DocUsage {
    /// The usage a stored row has: none when the database has no counters (`shown` is NULL).
    pub fn stored(shown: Option<i64>, useful: Option<i64>, last_useful_at: Option<i64>) -> Option<Self> {
        shown.map(|shown| Self {
            shown,
            useful: useful.unwrap_or(0),
            last_useful_at,
        })
    }
}

/// Which documents a listing keeps by whether agents found them useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsefulFilter {
    /// Documents an agent found useful at least once.
    Ever,
    /// Documents shown to agents at least [`Self::SHOWN_MIN`] times and never found useful.
    Never,
}

impl UsefulFilter {
    /// How often a document has to be shown before never being useful says something about it.
    pub const SHOWN_MIN: i64 = 3;

    /// Parses `ever` or `never`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ever" => Some(Self::Ever),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// True when a document with `usage` (`None` when the database has no counters) is kept.
    pub fn keeps(filter: Option<Self>, usage: Option<DocUsage>) -> bool {
        let Some(filter) = filter else {
            return true;
        };

        let Some(usage) = usage else {
            return false;
        };

        match filter {
            Self::Ever => usage.useful >= 1,
            Self::Never => usage.shown >= Self::SHOWN_MIN && usage.useful == 0,
        }
    }

    /// The SQL condition on the counter columns `shown` and `useful` that keeps these documents;
    /// `None` keeps all of them (no filter). Without the counters no document qualifies. A column
    /// that holds NULL counts as 0.
    pub fn condition(filter: Option<Self>, (shown, useful): (&str, &str), usage: bool) -> Option<String> {
        let filter = filter?;

        if !usage {
            return Some("1 = 0".to_owned());
        }

        Some(match filter {
            Self::Ever => format!("coalesce({useful}, 0) >= 1"),
            Self::Never => format!(
                "(coalesce({shown}, 0) >= {} AND coalesce({useful}, 0) = 0)",
                Self::SHOWN_MIN
            ),
        })
    }
}

/// Where [`super::KnowledgeStore::find_by_title`] looks: a title only matches within one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleScope {
    /// Documents linked to this repository key.
    Repo(String),
    /// Documents linked to no repository.
    Global,
}

/// The most titles one scope is scanned for (a team keeps hundreds of documents, not millions).
pub const TITLE_SCAN_MAX: usize = 20_000;

/// A title as titles are compared: trimmed, runs of whitespace as one space, lower case.
pub fn normalise_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The ids among `titles` whose title equals `wanted` once both are normalised, in id order.
pub fn matching_titles(titles: impl IntoIterator<Item = (i64, String)>, wanted: &str) -> Vec<i64> {
    let wanted = normalise_title(wanted);
    let mut ids: Vec<i64> = titles
        .into_iter()
        .filter(|(_, title)| normalise_title(title) == wanted)
        .map(|(id, _)| id)
        .collect();

    ids.sort_unstable();
    ids
}

/// Which of the optional columns of a document a database has, so a statement names only those.
///
/// They are tracked apart: a read-only role on a database that no writer of this version has
/// opened yet has neither, and keeps working without the fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Columns {
    /// `added_by_name`, `added_by_via`, `updated_by_name` and `updated_by_via`.
    pub authors: bool,
    /// The counters `shown`, `useful` and `last_useful_at`.
    pub usage: bool,
}

impl Columns {
    pub const NONE: Self = Self {
        authors: false,
        usage: false,
    };

    pub const ALL: Self = Self {
        authors: true,
        usage: true,
    };

    /// What the column names `found` in the documents table say.
    pub fn from_names(found: &[String]) -> Self {
        let has = |names: &[&str]| names.iter().all(|name| found.iter().any(|found| found == name));

        Self {
            authors: has(&AUTHOR_NAMES),
            usage: has(&USAGE_NAMES),
        }
    }

    /// True when every column is there: nothing to add.
    pub fn complete(self) -> bool {
        self == Self::ALL
    }
}

/// The author columns of a document.
pub const AUTHOR_NAMES: [&str; 4] = ["added_by_name", "added_by_via", "updated_by_name", "updated_by_via"];

/// The counter columns of a document.
pub const USAGE_NAMES: [&str; 3] = ["shown", "useful", "last_useful_at"];

/// Says once per process, on stderr (stdout is the MCP stream), that the columns could not be
/// added and why.
pub fn warn_unavailable(error: &StoreError) {
    static WARNED: Once = Once::new();

    WARNED.call_once(|| {
        eprintln!(
            "harness-knowledge: cannot add the author and usefulness columns to the knowledge database ({}), \
             continuing without them: authors and counters are off where the database lacks them",
            redact_text(&error.to_string())
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_compare_without_case_or_extra_whitespace() {
        assert_eq!(normalise_title("  Setup \t GUIDE\n"), "setup guide");
        assert_eq!(normalise_title("ŁÓDŹ  Notes"), "łódź notes");
        assert_ne!(normalise_title("setup-guide"), normalise_title("setup guide"));
    }

    #[test]
    fn a_stored_author_is_cleaned_as_a_new_one_is() {
        let stored = |name: &str, via: Option<&str>| Author::stored(Some(name.to_owned()), via.map(str::to_owned));

        assert_eq!(
            stored(" Ada ", Some("codex")),
            Some(Author {
                name: "Ada".to_owned(),
                via: Some("codex".to_owned())
            })
        );
        assert_eq!(Author::stored(None, Some("codex".to_owned())), None);

        for bad in ["Ada\u{202e}", "Ada\u{200b}", "Ada\nLovelace", "", &"x".repeat(81)] {
            assert_eq!(stored(bad, None), None, "{bad:?}");
        }

        let via = stored("Ada", Some("codex\n\u{202e}ignore")).and_then(|author| author.via);
        assert_eq!(via.as_deref(), Some("codex ignore"));

        let long = stored("Ada", Some(&"v".repeat(200))).and_then(|author| author.via);
        assert_eq!(long.map(|via| via.chars().count()), Some(64));

        assert_eq!(stored("Ada", Some("\u{200b}")).map(|author| author.via), Some(None));
    }

    #[test]
    fn columns_are_complete_only_with_every_name_of_a_group() {
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();

        assert_eq!(Columns::from_names(&[]), Columns::NONE);
        assert_eq!(
            Columns::from_names(&names(&["shown", "useful"])),
            Columns::NONE,
            "a part of a group is none"
        );
        assert_eq!(
            Columns::from_names(&names(&["shown", "useful", "last_useful_at"])),
            Columns {
                authors: false,
                usage: true
            }
        );
    }
}

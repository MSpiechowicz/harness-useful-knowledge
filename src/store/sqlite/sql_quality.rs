//! The SQL the local SQLite file and the libSQL server share for the columns added in place to
//! `knowledge_documents` (the authors and the usefulness counters), and for what reads them.
//!
//! A select list always has the same 20 columns, in the order of [`document_columns`]: a column
//! the database lacks is read as NULL, so the row mappers do not change with the schema.

use crate::store::quality::{AUTHOR_NAMES, USAGE_NAMES};
use crate::store::{Columns, Document, StoreError};

/// The columns of a document aliased `d` that every database has, in the order of `Document`'s
/// fields.
const BASE_COLUMNS: &str = "d.id, d.title, d.source_name, d.source_key, d.format, d.content_sha256, \
     d.file_sha256, d.bytes, d.chunk_count, d.created_at, d.updated_at, d.origin, d.warnings";

/// The columns a mapper reads: the base ones (0-12), the authors (13-16) and the usage (17-19).
pub(crate) fn document_columns(columns: Columns) -> String {
    let authors = if columns.authors {
        "d.added_by_name, d.added_by_via, d.updated_by_name, d.updated_by_via"
    } else {
        "NULL, NULL, NULL, NULL"
    };
    let usage = if columns.usage {
        "d.shown, d.useful, d.last_useful_at"
    } else {
        "NULL, NULL, NULL"
    };

    format!("{BASE_COLUMNS}, {authors}, {usage}")
}

/// The definitions of the added columns: nullable, or with a default, so an older server's inserts
/// stay valid and adding them rewrites no row.
const ADDED: [(&str, &str); 7] = [
    ("added_by_name", "TEXT"),
    ("added_by_via", "TEXT"),
    ("updated_by_name", "TEXT"),
    ("updated_by_via", "TEXT"),
    ("shown", "INTEGER NOT NULL DEFAULT 0"),
    ("useful", "INTEGER NOT NULL DEFAULT 0"),
    ("last_useful_at", "INTEGER"),
];

/// The names of the added columns the documents table has.
pub(crate) fn present_sql() -> String {
    let names: Vec<String> = AUTHOR_NAMES
        .iter()
        .chain(USAGE_NAMES.iter())
        .map(|name| format!("'{name}'"))
        .collect();

    format!(
        "SELECT name FROM pragma_table_info('knowledge_documents') WHERE name IN ({})",
        names.join(", ")
    )
}

/// The statements that add the columns missing from `found` (the names [`present_sql`] listed).
pub(crate) fn add_statements(found: &[String]) -> Vec<String> {
    ADDED
        .iter()
        .filter(|(name, _)| !found.iter().any(|present| present == name))
        .map(|(name, definition)| format!("ALTER TABLE knowledge_documents ADD COLUMN {name} {definition}"))
        .collect()
}

/// Sets both authors of the document `?1` to the name `?2` and the client `?3`: a new document.
pub(crate) const STAMP_NEW: &str = "UPDATE knowledge_documents
     SET added_by_name = ?2, added_by_via = ?3, updated_by_name = ?2, updated_by_via = ?3 WHERE id = ?1";

/// Sets the latest author of the document `?1`: an update keeps who added it.
pub(crate) const STAMP_UPDATE: &str =
    "UPDATE knowledge_documents SET updated_by_name = ?2, updated_by_via = ?3 WHERE id = ?1";

/// The author names and clients to bind to a stamp: the ones of `doc` for a new document, else the
/// latest ones.
pub(crate) fn stamp(doc: &Document) -> (&'static str, Option<&str>, Option<&str>) {
    let author = if doc.id == 0 { &doc.added_by } else { &doc.updated_by };

    (
        if doc.id == 0 { STAMP_NEW } else { STAMP_UPDATE },
        author.as_ref().map(|author| author.name.as_str()),
        author.as_ref().and_then(|author| author.via.as_deref()),
    )
}

/// Counts a time shown on the documents in the JSON array `?1`. A counter stops at the largest
/// integer (an overflow would turn it into a float, which a reader does not expect).
pub(crate) const SHOWN_SQL: &str = "UPDATE knowledge_documents
     SET shown = CASE WHEN shown < 9223372036854775807 THEN shown + 1 ELSE shown END
     WHERE id IN (SELECT value FROM json_each(?1))";

/// Counts a time found useful, at `?2`, on the documents in the JSON array `?1` (saturating, like
/// [`SHOWN_SQL`]).
pub(crate) const USEFUL_SQL: &str = "UPDATE knowledge_documents
     SET useful = CASE WHEN useful < 9223372036854775807 THEN useful + 1 ELSE useful END, last_useful_at = ?2
     WHERE id IN (SELECT value FROM json_each(?1))";

/// The ids and titles of the documents linked to no repository, at most `?1`.
pub(crate) const TITLES_GLOBAL_SQL: &str = "SELECT d.id, d.title FROM knowledge_documents d
     WHERE NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id) ORDER BY d.id LIMIT ?1";

/// The ids and titles of the documents linked to the repository `?2`, at most `?1`.
pub(crate) const TITLES_REPO_SQL: &str = "SELECT d.id, d.title FROM knowledge_documents d
     WHERE EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = ?2)
     ORDER BY d.id LIMIT ?1";

/// Each document with a chunk whose hash is in the JSON array `?1`, with how many distinct ones.
pub(crate) const SHARING_SQL: &str = "SELECT doc_id, count(DISTINCT chunk_sha) FROM knowledge_chunks
     WHERE chunk_sha IN (SELECT value FROM json_each(?1)) GROUP BY doc_id ORDER BY doc_id";

/// The hashes as the JSON array the statements above take; fails over the limit.
pub(crate) fn shas_json(shas: &[String]) -> Result<String, StoreError> {
    use crate::limits::SHARED_SHAS_MAX;

    if shas.len() > SHARED_SHAS_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {SHARED_SHAS_MAX} chunk hashes at once"
        )));
    }

    Ok(serde_json::to_string(shas).unwrap_or_else(|_| "[]".to_owned()))
}

/// The ids as the JSON array the counter statements take.
pub(crate) fn ids_json(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned())
}

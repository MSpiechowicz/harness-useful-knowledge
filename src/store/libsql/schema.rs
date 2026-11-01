//! The schema of the libSQL database and its version.
//!
//! The same tables, FTS5 keyword index and triggers as the local file (see
//! [`crate::store::sqlite::schema`]), with the vectors in an `F32_BLOB(384)` column that has a
//! cosine ANN index. Every table is prefixed `knowledge_`, except `libsql_vector_meta_shadow`, which
//! the server creates once for all vector indexes of the database.
//!
//! The keyword tokenizer is the local file's, `unicode61 remove_diacritics 2`: it folds `é` to `e`
//! but not `ł`, so `lodz` does not find `Łódź` (the same on both stores, kept as is).
//!
//! The version lives in `knowledge_meta` (`schema_version`), not in `PRAGMA user_version`: a team
//! may keep this database next to other tools' tables, and the pragma is shared by the whole file.
//! A migration is one batch run under `BEGIN IMMEDIATE`, together with the new version, so a
//! crash leaves the old version and two clients migrating at once queue.

use super::sql::{Sql, args, query_row};
use crate::store::StoreError;
use crate::store::sqlite::sql_shared::{keyword_index, tables};

/// The schema version this server writes: how many entries [`MIGRATIONS`] has.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Version 1: the tables of the local file, except that
///
/// - `knowledge_chunks.embedding` is an `F32_BLOB(384)` (the server refuses another size) that is
///   NULL exactly when `model_id` is, and
/// - `knowledge_chunks_vec` is the vector index: cosine, so `vector_top_k` returns the chunks
///   closest in angle, which for the L2-normalised vectors stored is the dot product. Its
///   `search_l=3200` is how many chunks one `vector_top_k` can return (200 by default, whatever `k`
///   is), which a search narrowed to a small repository needs (see [`super::search`]).
const V1: &str = concat!(
    tables!(),
    "
CREATE TABLE knowledge_chunks (
    id INTEGER PRIMARY KEY,
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    title TEXT NOT NULL,
    heading_path TEXT NOT NULL,
    text TEXT NOT NULL,
    chunk_sha TEXT NOT NULL,
    tokens INTEGER NOT NULL,
    char_start INTEGER NOT NULL,
    char_end INTEGER NOT NULL,
    embedding F32_BLOB(384),
    model_id TEXT,
    UNIQUE (doc_id, ordinal),
    CHECK ((embedding IS NULL) = (model_id IS NULL))
);
CREATE INDEX knowledge_chunks_sha ON knowledge_chunks (chunk_sha, model_id);
CREATE INDEX knowledge_chunks_vec ON knowledge_chunks (libsql_vector_idx(embedding, 'metric=cosine', 'search_l=3200'));
",
    keyword_index!(),
    "
INSERT INTO knowledge_meta (key, value) VALUES ('dims', '384'), ('generation', '0');
"
);

/// The migrations in order: entry `n` takes a database from version `n` to `n + 1`.
const MIGRATIONS: [&str; 1] = [V1];

/// The version of the database: `None` when it has no knowledge tables yet.
pub(super) fn version(db: &mut dyn Sql) -> Result<Option<i64>, StoreError> {
    let has_meta = query_row(
        db,
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'knowledge_meta'",
        args![],
    )?;

    if has_meta.is_none() {
        return Ok(None);
    }

    let row = query_row(
        db,
        "SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'schema_version'",
        args![],
    )?;

    // Tables without a version are a migration that did not finish: it ran in a transaction, so
    // this does not happen, and is reported rather than guessed at.
    match row {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Err(StoreError::Backend(
            "the knowledge tables have no schema version".to_owned(),
        )),
    }
}

/// Brings the database from version `from` (0 for none) up to [`SCHEMA_VERSION`].
pub(super) fn upgrade(db: &mut dyn Sql, from: i64) -> Result<(), StoreError> {
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(usize::try_from(from).unwrap_or(0)) {
        db.batch(sql)?;
        db.execute(
            "INSERT INTO knowledge_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            args![(index + 1).to_string()],
        )?;
    }

    Ok(())
}

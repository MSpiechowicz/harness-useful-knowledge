//! The PostgreSQL schema and its version (the `schema_version` row of `knowledge_meta`).
//!
//! The tables live in the connection's current schema (the first `search_path` entry), so a team
//! can give the knowledge its own database, or its own schema through the URL's
//! `options=-csearch_path=...`. Every table, index and sequence is prefixed `knowledge_`, so the
//! database can be shared with other tools (harness-memory keeps its own tables and locks).
//!
//! Migrations run in one transaction under an advisory lock, so clients starting together migrate
//! once. A client that finds a newer version changes nothing.
//!
//! # Adding a version
//!
//! A migration on a team's database can run for minutes (a stored generated column rewrites the
//! table, a GIN index reads every row), so the rules below keep it from failing forever:
//!
//! - It runs in its own open ([`super::PostgresStore::open_with`] with [`crate::backend::Upgrade::Run`]),
//!   which [`crate::backend`] gives a long bound of its own and not the probe deadline of an
//!   ordinary open, and the transaction lifts the session's statement limit and waits longer for
//!   locks (the idle limit stays) (see [`migrate_with`]).
//! - A change that only adds (a column, an index) leaves a read of the older database working:
//!   queries written for the new version must still answer on the old one, or wait for the writer
//!   to upgrade it. A change that readers of the old version cannot cope with raises
//!   [`READABLE_FROM`] to its version, and a read of a database older than that says it has not
//!   been upgraded yet.
//! - Version numbers are released: a version that shipped is never edited, a change is a new
//!   entry of [`migrations`].

use postgres::{Client, GenericClient};

use super::error::{backend, column};
use super::vector::{self, Fault};
use crate::store::{DIMS, StoreError};

/// The schema version this server writes: how many entries [`migrations`] has.
pub const SCHEMA_VERSION: i64 = 2;

/// The oldest schema version a read works with: an older database is refused to a read until a
/// writer has upgraded it. Version 2 raised it from 1: a reader of version 1 would show a draft as a
/// live document, so it waits for the upgrade (and a version 1 server stops at a database of
/// version 2, see [`SCHEMA_VERSION`]). See the module docs for when to raise it.
pub(super) const READABLE_FROM: i64 = 2;

/// How long the migration waits for a lock another session holds (the session's own limit is
/// 10 s, which suits a read or a write but not a migration queued behind running writes).
const MIGRATION_LOCK_TIMEOUT: &str = "5min";

/// How long the migration's transaction may sit idle before the server ends it. Only the statement
/// timeout is lifted: a client that stalls or dies mid-migration must not hold its locks forever.
const MIGRATION_IDLE_TIMEOUT: &str = "60s";

/// The advisory lock migrations take (database-wide, "knowledge" and 01). Writes hold it shared, so
/// a migration waits for running writes and later writes see its version.
pub(super) const MIGRATE_LOCK: i64 = 0x6b6e_6f77_6c65_6401;

/// The state of a database after [`migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// At [`SCHEMA_VERSION`].
    Current,
    /// Written by a newer server, left untouched.
    Newer,
}

/// The name of the HNSW index on the embeddings.
pub(super) const HNSW_INDEX: &str = "knowledge_chunks_embedding";

/// The accented letters the keyword index treats as their plain letters, grouped by that letter.
/// `translate` is immutable, so it can sit in a generated column. Only letters that decompose into
/// a plain letter and a mark are listed, like SQLite's `remove_diacritics`, so `ł`, `ø` and `đ`
/// stay letters of their own. The table is part of version 1: a letter added later needs a
/// migration that rebuilds the column.
const FOLDS: [(&str, char); 38] = [
    ("ÀÁÂÃÄÅĀĂĄ", 'A'),
    ("àáâãäåāăą", 'a'),
    ("ÇĆĈĊČ", 'C'),
    ("çćĉċč", 'c'),
    ("Ď", 'D'),
    ("ď", 'd'),
    ("ÈÉÊËĒĔĖĘĚ", 'E'),
    ("èéêëēĕėęě", 'e'),
    ("ĜĞĠĢ", 'G'),
    ("ĝğġģ", 'g'),
    ("Ĥ", 'H'),
    ("ĥ", 'h'),
    ("ÌÍÎÏĨĪĬĮİ", 'I'),
    ("ìíîïĩīĭį", 'i'),
    ("Ĵ", 'J'),
    ("ĵ", 'j'),
    ("Ķ", 'K'),
    ("ķ", 'k'),
    ("ĹĻĽ", 'L'),
    ("ĺļľ", 'l'),
    ("ÑŃŅŇ", 'N'),
    ("ñńņň", 'n'),
    ("ÒÓÔÕÖŌŎŐ", 'O'),
    ("òóôõöōŏő", 'o'),
    ("ŔŖŘ", 'R'),
    ("ŕŗř", 'r'),
    ("ŚŜŞŠ", 'S'),
    ("śŝşš", 's'),
    ("ŢŤ", 'T'),
    ("ţť", 't'),
    ("ÙÚÛÜŨŪŬŮŰŲ", 'U'),
    ("ùúûüũūŭůűų", 'u'),
    ("Ŵ", 'W'),
    ("ŵ", 'w'),
    ("ÝŶŸ", 'Y'),
    ("ýÿŷ", 'y'),
    ("ŹŻŽ", 'Z'),
    ("źżž", 'z'),
];

/// `expression` (a text) with the letters of [`FOLDS`] replaced by their plain letters.
pub(super) fn fold(expression: &str) -> String {
    let mut from = String::new();
    let mut to = String::new();

    for (accented, plain) in FOLDS {
        for letter in accented.chars() {
            from.push(letter);
            to.push(plain);
        }
    }

    format!("translate({expression}, '{from}', '{to}')")
}

/// The version table. Its rows are the schema version, the vector size, the write counter and
/// the model that made the vectors.
const META: &str = "
CREATE TABLE knowledge_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Version 1.
///
/// - `knowledge_meta`: `schema_version`, `dims`, `generation` (the write counter) and, once the
///   first chunk is embedded, `model_id`.
/// - `knowledge_documents`: one row per imported file. `source_key` and `content_sha256` are
///   indexed for finding a file again, neither is unique.
/// - `knowledge_doc_links`: the repositories a document is linked to, with their path globs. A
///   document with no row is global.
/// - `knowledge_chunks`: the unit that is searched. `title` copies the document's title (a rename
///   rewrites the copies), because `tsv` is generated from the row's own columns: the title weighs
///   A, the heading path B, the text the default D, all in the `simple` configuration (no
///   stemming, no stop words) with accents folded (see [`fold`]). `tsv_heading` and `tsv_text` are
///   the heading path alone and the text alone: `tsv` concatenates the three, so the positions of
///   one field continue those of the one before it, and a phrase (`a <-> b`) would match across
///   fields. A phrase is looked for in each of these two instead. The embedding is
///   [`DIMS`] floats of pgvector and always comes with its model.
/// - `knowledge_chunks_tsv`, `knowledge_chunks_tsv_heading`, `knowledge_chunks_tsv_text`: the GIN
///   indexes of the three.
/// - [`HNSW_INDEX`]: the HNSW index of the embeddings by cosine distance.
fn tables() -> String {
    format!(
        "
CREATE TABLE knowledge_documents (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    title TEXT NOT NULL,
    source_name TEXT NOT NULL,
    source_key TEXT NOT NULL,
    format TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    file_sha256 TEXT NOT NULL,
    bytes BIGINT NOT NULL,
    chunk_count BIGINT NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('user', 'agentApproved')),
    warnings TEXT[] NOT NULL DEFAULT '{{}}',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX knowledge_documents_source ON knowledge_documents (source_key);
CREATE INDEX knowledge_documents_content ON knowledge_documents (content_sha256);

CREATE TABLE knowledge_doc_links (
    doc_id BIGINT NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    repo_key TEXT NOT NULL,
    globs TEXT[] NOT NULL DEFAULT '{{}}',
    PRIMARY KEY (doc_id, repo_key)
);
CREATE INDEX knowledge_doc_links_repo ON knowledge_doc_links (repo_key, doc_id);

CREATE TABLE knowledge_chunks (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    doc_id BIGINT NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    ordinal BIGINT NOT NULL,
    title TEXT NOT NULL,
    heading_path TEXT NOT NULL,
    text TEXT NOT NULL,
    chunk_sha TEXT NOT NULL,
    tokens BIGINT NOT NULL,
    char_start BIGINT NOT NULL,
    char_end BIGINT NOT NULL,
    embedding vector({DIMS}),
    model_id TEXT,
    tsv TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('simple'::regconfig, {title}), 'A')
        || setweight(to_tsvector('simple'::regconfig, {heading}), 'B')
        || to_tsvector('simple'::regconfig, {text})
    ) STORED,
    tsv_heading TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, {heading})) STORED,
    tsv_text TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, {text})) STORED,
    UNIQUE (doc_id, ordinal),
    CHECK ((embedding IS NULL) = (model_id IS NULL))
);
CREATE INDEX knowledge_chunks_sha ON knowledge_chunks (chunk_sha, model_id);
CREATE INDEX knowledge_chunks_tsv ON knowledge_chunks USING GIN (tsv);
CREATE INDEX knowledge_chunks_tsv_heading ON knowledge_chunks USING GIN (tsv_heading);
CREATE INDEX knowledge_chunks_tsv_text ON knowledge_chunks USING GIN (tsv_text);
CREATE INDEX {HNSW_INDEX} ON knowledge_chunks USING hnsw (embedding vector_cosine_ops);

INSERT INTO knowledge_meta (key, value) VALUES ('schema_version', '1'), ('dims', '{DIMS}'), ('generation', '0');
",
        title = fold("title"),
        heading = fold("heading_path"),
        text = fold("text"),
    )
}

/// Version 2: the review of drafts. The columns are added to the documents table, so every existing
/// document is live at revision 0 (a constant default adds a column without rewriting the table).
///
/// - `status`: `live`, `draft`, `waiting` or `changesRequested`.
/// - `pending_of`: the live document a row is the pending version of. A live row has none
///   (`knowledge_documents_pending_not_live`), and a document has at most one pending version (the
///   partial unique index `knowledge_documents_pending`). Deleting the live document deletes it.
/// - `review_rev`: raised by every content change and transition, the compare-and-set of a transition.
/// - `knowledge_doc_events`: the history. `knowledge_doc_comments`: the comments of reviews, a reply
///   pointing at its parent (`quote`: the lines a line comment was written on, a JSON array of
///   `{line, kind, text}`, see [`crate::store::QuoteLine`], null for any other comment). `knowledge_doc_snapshots`: the section texts as of an ask, a JSON array
///   of `{heading, text}` at the revision the ask made.
/// - `knowledge_doc_files`: the original file of an import, its bytes in one `bytea` (at most
///   [`crate::limits::PDF_BYTES_MAX`], far below the 1 GiB a value may have, and PostgreSQL keeps it
///   out of the row in its TOAST table, so reading a document never reads it).
///
/// Everything but the documents table is removed with its document.
const REVIEW: &str = "
ALTER TABLE knowledge_documents ADD COLUMN status TEXT NOT NULL DEFAULT 'live'
    CHECK (status IN ('live', 'draft', 'waiting', 'changesRequested'));
ALTER TABLE knowledge_documents ADD COLUMN review_rev BIGINT NOT NULL DEFAULT 0;
ALTER TABLE knowledge_documents ADD COLUMN pending_of BIGINT
    REFERENCES knowledge_documents(id) ON DELETE CASCADE;
ALTER TABLE knowledge_documents ADD CONSTRAINT knowledge_documents_pending_not_live
    CHECK (pending_of IS NULL OR status <> 'live');
CREATE UNIQUE INDEX knowledge_documents_pending ON knowledge_documents (pending_of) WHERE pending_of IS NOT NULL;
CREATE INDEX knowledge_documents_status ON knowledge_documents (status);

CREATE TABLE knowledge_doc_events (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    doc_id BIGINT NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    by_name TEXT,
    comment TEXT,
    at BIGINT NOT NULL
);
CREATE INDEX knowledge_doc_events_doc ON knowledge_doc_events (doc_id, id);

CREATE TABLE knowledge_doc_comments (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    doc_id BIGINT NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    review_id BIGINT,
    section TEXT,
    occurrence BIGINT,
    side TEXT CHECK (side IS NULL OR side IN ('old', 'new')),
    line_from BIGINT,
    line_to BIGINT,
    body TEXT NOT NULL,
    by_name TEXT,
    at BIGINT NOT NULL,
    parent_id BIGINT REFERENCES knowledge_doc_comments(id) ON DELETE CASCADE,
    resolved_at BIGINT,
    resolved_by TEXT,
    revision BIGINT NOT NULL,
    quote TEXT
);
CREATE INDEX knowledge_doc_comments_doc ON knowledge_doc_comments (doc_id, id);

CREATE TABLE knowledge_doc_files (
    doc_id BIGINT PRIMARY KEY REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    bytes BYTEA NOT NULL
);

CREATE TABLE knowledge_doc_snapshots (
    doc_id BIGINT NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    revision BIGINT NOT NULL,
    sections TEXT NOT NULL,
    PRIMARY KEY (doc_id, revision)
);
";

/// The migrations in order: entry `n` takes a database from version `n` to `n + 1`.
pub(super) fn migrations() -> Vec<String> {
    vec![format!("{META}{}", tables()), REVIEW.to_owned()]
}

/// Only the migration of version 1, for the tests of the upgrade from it.
#[cfg(test)]
pub(super) fn version_one() -> Vec<String> {
    migrations().into_iter().take(1).collect()
}

/// The version stored in the database: `None` for one that has no knowledge tables.
pub(super) fn stored_version(client: &mut impl GenericClient) -> Result<Option<i64>, StoreError> {
    // The catalog is read directly: `to_regclass` can answer from a cached "no such table" after
    // this session waited for the migration lock (advisory locks do not refresh that cache) and
    // another server created the table meanwhile.
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_class c
                            WHERE c.relname = 'knowledge_meta' AND c.relkind = 'r' AND pg_table_is_visible(c.oid))",
            &[],
        )
        .map_err(backend)?;

    if !column::<bool>(&row, 0)? {
        return Ok(None);
    }

    let row = client
        .query_opt("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let text: String = column(&row, 0)?;

    text.parse()
        .map(Some)
        .map_err(|_| StoreError::Backend("the stored schema version is not a number".to_owned()))
}

/// The version in `knowledge_meta`, read with one primary-key lookup: the check each read makes
/// before it answers, so a client that was connected when a newer server migrated the database stops
/// reading from it ([`StoreError::NewerSchema`]). Unlike [`stored_version`] it assumes the tables
/// exist, which they do for an open store.
pub(super) fn current_version(client: &mut impl GenericClient) -> Result<Option<i64>, StoreError> {
    let row = client
        .query_opt("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .map_err(backend)?;

    row.map(|row| {
        let text: String = column(&row, 0)?;

        text.parse()
            .map_err(|_| StoreError::Backend("the stored schema version is not a number".to_owned()))
    })
    .transpose()
}

/// Brings the database up to [`SCHEMA_VERSION`], adding pgvector first when the database lacks
/// it. A newer database is left alone. Everything runs in one transaction under the migration
/// lock, so a server that waits for it finds the work done.
pub(super) fn migrate(client: &mut Client) -> Result<SchemaState, Fault> {
    migrate_with(client, &migrations())
}

/// [`migrate`] with `steps` as the migrations (entry `n` takes version `n` to `n + 1`).
///
/// The transaction is not bound by the limits of an ordinary session, which would end a migration
/// of a real-sized database every time it is tried: it has no statement timeout and waits
/// [`MIGRATION_LOCK_TIMEOUT`] for a lock (`SET LOCAL`, so the session's own limits are back when it
/// ends). It keeps a finite idle limit ([`MIGRATION_IDLE_TIMEOUT`]), so a stalled client cannot hold
/// the migration's locks.
pub(super) fn migrate_with(client: &mut Client, steps: &[String]) -> Result<SchemaState, Fault> {
    let target = i64::try_from(steps.len()).unwrap_or(i64::MAX);

    match stored_version(client)? {
        Some(version) if version > target => return Ok(SchemaState::Newer),
        Some(version) if version == target => return Ok(SchemaState::Current),
        _ => {}
    }

    let mut tx = client.transaction().map_err(backend)?;
    tx.batch_execute(&format!(
        "SET LOCAL statement_timeout = 0; \
         SET LOCAL idle_in_transaction_session_timeout = '{MIGRATION_IDLE_TIMEOUT}'; \
         SET LOCAL lock_timeout = '{MIGRATION_LOCK_TIMEOUT}';"
    ))
    .map_err(backend)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK])
        .map_err(backend)?;

    // Another server may have migrated while this one waited for the lock.
    let from = match stored_version(&mut tx)? {
        Some(version) if version > target => return Ok(SchemaState::Newer),
        Some(version) => usize::try_from(version).unwrap_or(0),
        None => 0,
    };

    vector::ensure(&mut tx)?;

    for (index, sql) in steps.iter().enumerate().skip(from) {
        tx.batch_execute(sql).map_err(backend)?;
        tx.execute(
            "UPDATE knowledge_meta SET value = $1 WHERE key = 'schema_version'",
            &[&(index + 1).to_string()],
        )
        .map_err(backend)?;
    }

    tx.commit().map_err(backend)?;

    Ok(SchemaState::Current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_version_is_the_number_of_migrations() {
        assert_eq!(i64::try_from(migrations().len()), Ok(SCHEMA_VERSION));
    }

    #[test]
    fn every_table_index_and_sequence_is_prefixed() {
        for sql in migrations() {
            for line in sql.lines().filter(|line| line.starts_with("CREATE")) {
                // `CREATE TABLE name (` and `CREATE INDEX name ON`: the name is the third word
                // (once `UNIQUE` is left out).
                let line = line.replacen("CREATE UNIQUE", "CREATE", 1);
                let name = line.split_whitespace().nth(2);

                assert!(name.is_some_and(|name| name.starts_with("knowledge_")), "{line}");
            }
        }
    }

    #[test]
    fn the_fold_table_maps_each_letter_once_to_a_plain_letter() {
        let mut seen = std::collections::HashSet::new();

        for (accented, plain) in FOLDS {
            assert!(plain.is_ascii_alphabetic());

            for letter in accented.chars() {
                assert!(!letter.is_ascii(), "{letter}");
                assert!(seen.insert(letter), "{letter} is mapped twice");
            }
        }

        let sql = fold("x");
        assert!(sql.starts_with("translate(x, '") && sql.ends_with("')"));
        assert!(!sql.contains("''"), "no quote can end the literal early");
    }

    #[test]
    fn polish_letters_fold_except_the_stroked_l() {
        let from: String = FOLDS.iter().flat_map(|(accented, _)| accented.chars()).collect();

        for letter in "ąćęńóśźżĄĆĘŃÓŚŹŻ".chars() {
            assert!(from.contains(letter), "{letter}");
        }

        for letter in ['ł', 'Ł', 'ø', 'Ø'] {
            assert!(!from.contains(letter), "{letter} stays a letter of its own");
        }
    }
}

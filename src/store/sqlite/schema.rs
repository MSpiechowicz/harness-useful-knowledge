//! The database schema and its version (`PRAGMA user_version`).
//!
//! Every table is prefixed `knowledge_`. A migration is a batch of statements run in one
//! `IMMEDIATE` transaction together with the new `user_version`, so a crash leaves the old version
//! and two servers opening one file migrate it once.

use rusqlite::{Connection, TransactionBehavior};

use crate::store::DIMS;

/// The schema version this server writes: how many entries [`MIGRATIONS`] has.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The state of a database after [`migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// At [`SCHEMA_VERSION`].
    Current,
    /// Written by a newer server; left untouched.
    Newer,
}

/// The bytes of one stored embedding (little-endian `f32` for each of [`DIMS`]).
pub const EMBEDDING_BYTES: usize = DIMS * 4;

/// Version 1.
///
/// - `knowledge_meta`: `schema_version`, `dims`, `generation` (the write counter) and, once the
///   first chunk is embedded, `model_id`.
/// - `knowledge_documents`: one row per imported file. `source_key` and `content_sha256` are
///   indexed for finding a file again, neither is unique.
/// - `knowledge_doc_links`: the repositories a document is linked to, with their path globs as
///   JSON. A document with no row is global.
/// - `knowledge_chunks`: the unit that is searched. `title` copies the document's title, because
///   the external-content index reads its columns from this table (a trigger on the document keeps
///   the copies current). The embedding is [`EMBEDDING_BYTES`] bytes and always comes with its
///   model.
/// - `knowledge_chunks_fts`: the FTS5 index of title, heading path and text, kept current by
///   triggers.
const V1: &str = "
CREATE TABLE knowledge_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE knowledge_documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    source_name TEXT NOT NULL,
    source_key TEXT NOT NULL,
    format TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    file_sha256 TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    chunk_count INTEGER NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('user', 'agentApproved')),
    warnings TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX knowledge_documents_source ON knowledge_documents (source_key);
CREATE INDEX knowledge_documents_content ON knowledge_documents (content_sha256);

CREATE TABLE knowledge_doc_links (
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    repo_key TEXT NOT NULL,
    globs TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (doc_id, repo_key)
) WITHOUT ROWID;
CREATE INDEX knowledge_doc_links_repo ON knowledge_doc_links (repo_key, doc_id);

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
    embedding BLOB,
    model_id TEXT,
    UNIQUE (doc_id, ordinal),
    CHECK ((embedding IS NULL) = (model_id IS NULL)),
    CHECK (embedding IS NULL OR length(embedding) = 1536)
);
CREATE INDEX knowledge_chunks_sha ON knowledge_chunks (chunk_sha, model_id);

CREATE VIRTUAL TABLE knowledge_chunks_fts USING fts5(
    title, heading_path, text,
    content = 'knowledge_chunks', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER knowledge_chunks_fts_insert AFTER INSERT ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (rowid, title, heading_path, text)
        VALUES (new.id, new.title, new.heading_path, new.text);
END;
CREATE TRIGGER knowledge_chunks_fts_delete AFTER DELETE ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (knowledge_chunks_fts, rowid, title, heading_path, text)
        VALUES ('delete', old.id, old.title, old.heading_path, old.text);
END;
CREATE TRIGGER knowledge_chunks_fts_update AFTER UPDATE OF title, heading_path, text ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (knowledge_chunks_fts, rowid, title, heading_path, text)
        VALUES ('delete', old.id, old.title, old.heading_path, old.text);
    INSERT INTO knowledge_chunks_fts (rowid, title, heading_path, text)
        VALUES (new.id, new.title, new.heading_path, new.text);
END;
CREATE TRIGGER knowledge_documents_retitle AFTER UPDATE OF title ON knowledge_documents BEGIN
    UPDATE knowledge_chunks SET title = new.title WHERE doc_id = new.id;
END;

INSERT INTO knowledge_meta (key, value) VALUES ('schema_version', '1'), ('dims', '384'), ('generation', '0');
";

/// The migrations in order: entry `n` takes a database from version `n` to `n + 1`.
const MIGRATIONS: [&str; 1] = [V1];

/// Brings the database up to [`SCHEMA_VERSION`]; a newer one is left alone.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<SchemaState> {
    if user_version(conn)? > SCHEMA_VERSION {
        return Ok(SchemaState::Newer);
    }

    if user_version(conn)? == SCHEMA_VERSION {
        return Ok(SchemaState::Current);
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Another server may have migrated while this one waited for the lock.
    let from = user_version(&tx)?;

    for (index, sql) in MIGRATIONS.iter().enumerate().skip(usize::try_from(from).unwrap_or(0)) {
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
    }

    tx.commit()?;
    Ok(SchemaState::Current)
}

/// The version stored in the file (0 for a file nothing was written to).
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(conn: &Connection) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'trigger') ORDER BY name")
            .expect("prepare");

        statement
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("names")
    }

    #[test]
    fn an_empty_database_migrates_to_the_current_version() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        assert_eq!(migrate(&mut conn).expect("migrate"), SchemaState::Current);
        assert_eq!(user_version(&conn).expect("version"), SCHEMA_VERSION);

        let names = tables(&conn);
        for expected in [
            "knowledge_meta",
            "knowledge_documents",
            "knowledge_doc_links",
            "knowledge_chunks",
            "knowledge_chunks_fts",
        ] {
            assert!(names.iter().any(|name| name == expected), "{expected} in {names:?}");
        }

        assert!(
            names.iter().all(|name| name.starts_with("knowledge_")),
            "every table and trigger is prefixed: {names:?}"
        );

        let meta: Vec<(String, String)> = conn
            .prepare("SELECT key, value FROM knowledge_meta ORDER BY key")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .and_then(Iterator::collect)
            })
            .expect("meta");
        assert_eq!(
            meta,
            [
                ("dims".to_owned(), DIMS.to_string()),
                ("generation".to_owned(), "0".to_owned()),
                ("schema_version".to_owned(), SCHEMA_VERSION.to_string()),
            ]
        );
    }

    #[test]
    fn migrating_again_changes_nothing() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        migrate(&mut conn).expect("first");
        conn.execute("UPDATE knowledge_meta SET value = '7' WHERE key = 'generation'", [])
            .expect("update");

        assert_eq!(migrate(&mut conn).expect("second"), SchemaState::Current);

        let generation: String = conn
            .query_row("SELECT value FROM knowledge_meta WHERE key = 'generation'", [], |row| {
                row.get(0)
            })
            .expect("generation");
        assert_eq!(generation, "7");
    }

    #[test]
    fn a_newer_database_is_left_alone() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .expect("version");

        assert_eq!(migrate(&mut conn).expect("migrate"), SchemaState::Newer);
        assert!(tables(&conn).is_empty());
    }
}

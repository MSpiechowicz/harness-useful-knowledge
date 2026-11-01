//! [`KnowledgeStore`] over a shared PostgreSQL database with pgvector, so a team keeps one set of
//! documents.
//!
//! The semantics are the SQLite backend's: the same scopes, title rule, replace-on-put, cleaning,
//! limits and paging. The keyword index is a GIN index over a generated `tsvector`, the vector
//! index an HNSW index (see [`search`] and [`schema`]).
//!
//! Every write runs in a transaction that first takes one advisory lock, so writers queue like
//! they do on the SQLite file and two writers cannot deadlock on the rows they share. The
//! transaction also re-reads the schema version under the shared migration lock, so a client stops
//! writing as soon as a newer server migrates the database, not only when it connects. Sessions
//! set a lock, statement and idle-in-transaction timeout, so a stuck client cannot hold the others
//! for long.
//!
//! A read never creates the schema: [`PostgresStore::open`] with [`Access::Read`] answers `None`
//! for a database that has no knowledge tables yet. Only [`Access::Create`] migrates, and that is
//! the one place pgvector is created (see [`vector`]).
//!
//! One connection is opened lazily and kept for the life of the process. A dropped connection is
//! opened again before the next call. A read that failed because the connection closed is tried
//! once more, a write only when it failed before `COMMIT` was sent (the server rolls back a
//! transaction whose connection closes, so nothing was saved). A write whose `COMMIT` failed is
//! never repeated: the server may have committed it, and running it again would apply it twice.
//!
//! Errors carry the SQLSTATE with a fixed description, never the server's message, stored text, the
//! URL or a password (see [`error`]).

mod connect;
mod documents;
mod error;
mod listing;
mod meta;
mod probe;
pub mod schema;
mod search;
mod session;
#[cfg(test)]
mod tests;
pub mod vector;

use std::collections::HashMap;

use postgres::{Client, Transaction};

pub use connect::UrlError;
pub use probe::{Probe, probe};
use schema::{SCHEMA_VERSION, SchemaState};
use vector::Version;
pub use vector::{Fault, Problem};

use super::{Chunk, ChunkHit, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link, ListQuery};
use super::{PendingChunk, RepoCounts, Scope, Stats, StoreError, clean};
use crate::backend::{Access, OpenError};
use crate::config::Secrets;
use error::{backend, ended};

/// The advisory lock every write transaction takes ("knowledge" and 02).
const WRITE_LOCK: i64 = 0x6b6e_6f77_6c65_6402;

/// The PostgreSQL backend. See the module docs.
pub struct PostgresStore {
    settings: connect::Settings,
    client: Client,
    newer: bool,
    /// The pgvector this database has, `None` when it was dropped after the tables were made.
    pgvector: Option<Version>,
}

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore")
            .field("newer", &self.newer)
            .field("pgvector", &self.pgvector)
            .finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Connects to `url` (`postgres://` or `postgresql://`, see [`connect`] for the TLS rules) as
    /// `access` says: [`Access::Create`] migrates the schema (adding pgvector first), the others
    /// answer `None` for a database without knowledge tables and never change it. A database
    /// written by a newer server connects, but every operation then fails with
    /// [`StoreError::NewerSchema`].
    pub fn open(url: &str, access: Access) -> Result<Option<Self>, Fault> {
        let settings = connect::parse(url).map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut client = session::open(&settings)?;

        let state = match (schema::stored_version(&mut client)?, access) {
            (None, Access::Create) => schema::migrate(&mut client)?,
            (None, _) => return Ok(None),
            (Some(version), _) if version > SCHEMA_VERSION => SchemaState::Newer,
            (Some(version), Access::Read) if version < SCHEMA_VERSION => {
                return Err(StoreError::Backend(
                    "the database is from an older server and has not been upgraded yet".to_owned(),
                )
                .into());
            }
            (Some(_), Access::Read) => SchemaState::Current,
            (Some(_), _) => schema::migrate(&mut client)?,
        };

        let pgvector = vector::installed(&mut client)?;

        Ok(Some(Self {
            settings,
            client,
            newer: state == SchemaState::Newer,
            pgvector,
        }))
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// The schema version of the database.
    pub fn schema_version(&mut self) -> Result<i64, StoreError> {
        self.read(|client| Ok(schema::stored_version(client)?.unwrap_or(0)))
    }

    /// The pgvector version of the database.
    pub fn pgvector(&self) -> Option<Version> {
        self.pgvector
    }

    /// Fails once the database is known to be newer, and opens the connection again when it closed.
    fn ready(&mut self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        if self.client.is_closed() {
            self.client = session::open(&self.settings)?;
        }

        Ok(())
    }

    /// Runs the read `operation` on a live client, once more after reconnecting when it failed
    /// because the connection closed meanwhile.
    fn read<T>(&mut self, mut operation: impl FnMut(&mut Client) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.ready()?;

        match operation(&mut self.client) {
            Err(error) if self.client.is_closed() || ended(&error) => {
                self.client = session::open(&self.settings)?;
                operation(&mut self.client)
            }
            other => other,
        }
    }

    /// Runs `operation` in a write transaction (see [`transact`]). It runs once more after
    /// reconnecting only when the connection closed before `COMMIT` was sent, see the module docs.
    fn write<T>(
        &mut self,
        mut operation: impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.ready()?;

        let result = match transact(&mut self.client, &mut operation) {
            Err(Failure::BeforeCommit(error)) if self.client.is_closed() || ended(&error) => {
                self.client = session::open(&self.settings)?;
                transact(&mut self.client, &mut operation)
            }
            other => other,
        };

        let error = match result {
            Ok(value) => return Ok(value),
            Err(Failure::BeforeCommit(error)) => error,
            Err(Failure::AtCommit(error)) if self.client.is_closed() => uncertain(error),
            Err(Failure::AtCommit(error)) => error,
        };

        if matches!(error, StoreError::NewerSchema) {
            self.newer = true;
        }

        Err(error)
    }
}

/// Why a write attempt failed: before `COMMIT` was sent (nothing was saved, so it may run again)
/// or at `COMMIT` (the server may have saved it, so it must not).
enum Failure {
    BeforeCommit(StoreError),
    AtCommit(StoreError),
}

/// One write attempt: a transaction that takes [`WRITE_LOCK`] (and [`schema::MIGRATE_LOCK`]
/// shared), fails with [`StoreError::NewerSchema`] when a newer server migrated the database
/// meanwhile, runs `operation`, counts the write and commits. A failure drops the transaction,
/// which rolls it back.
fn transact<T>(
    client: &mut Client,
    operation: &mut impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, Failure> {
    let before = |error: postgres::Error| Failure::BeforeCommit(backend(error));

    // The migration lock, held shared, keeps a newer server's migration (which holds it alone)
    // from running between the version check and the commit.
    let mut tx = client.transaction().map_err(before)?;
    tx.execute(
        "SELECT pg_advisory_xact_lock($1), pg_advisory_xact_lock_shared($2)",
        &[&WRITE_LOCK, &schema::MIGRATE_LOCK],
    )
    .map_err(before)?;

    let version = schema::stored_version(&mut tx).map_err(Failure::BeforeCommit)?;

    if version.is_some_and(|found| found > SCHEMA_VERSION) {
        return Err(Failure::BeforeCommit(StoreError::NewerSchema));
    }

    let result = operation(&mut tx).map_err(Failure::BeforeCommit)?;
    meta::bump(&mut tx).map_err(Failure::BeforeCommit)?;
    tx.commit().map_err(|error| Failure::AtCommit(backend(error)))?;

    Ok(result)
}

/// The error of a `COMMIT` whose connection closed: the server may or may not have saved it.
fn uncertain(error: StoreError) -> StoreError {
    match error {
        StoreError::Backend(message) => StoreError::Backend(format!(
            "{message}. The connection closed while the write was committed, so it may have been saved: \
             read it back before repeating it"
        )),
        other => other,
    }
}

impl KnowledgeStore for PostgresStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.read(|client| meta::get(client, key))
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        meta::check_settable(key)?;
        self.write(|tx| meta::set(tx, key, value))
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.read(meta::generation)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let title = clean::title(&doc.title)?;
        let links = clean::links(links)?;
        clean::chunks(chunks)?;

        self.write(|tx| documents::put(tx, doc, &title, chunks, &links))
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        self.read(|client| documents::find(client, "content_sha256", sha))
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        self.read(|client| documents::find(client, "source_key", key))
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        if chunk_shas.is_empty() {
            return Ok(HashMap::new());
        }

        self.read(|client| documents::reusable_embeddings(client, model_id, chunk_shas))
    }

    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        self.read(|client| documents::pending_chunks(client, model_id, after, limit))
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        clean::embeddings(model_id, embeddings)?;

        self.write(|tx| documents::set_embeddings(tx, model_id, embeddings))
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let links = clean::links(links)?;

        self.write(|tx| documents::set_links(tx, id, &links))
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        let title = clean::title(title)?;

        self.write(|tx| documents::rename(tx, id, &title))
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        self.write(|tx| documents::delete(tx, ids))
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.read(|client| listing::list(client, query))
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        self.read(|client| listing::get(client, id, detail))
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.read(|client| search::keyword_top_k(client, query, scope, k))
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        let pgvector = self.pgvector;

        self.read(|client| search::vector_top_k(client, pgvector, vector, scope, k))
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.read(listing::repos)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.read(listing::stats)
    }
}

/// The store `backend::open` hands out for a PostgreSQL URL: `None` when `access` does not create
/// the schema and the database has none. Failures are one readable line without the URL (and with
/// every secret of it scrubbed).
pub fn open_boxed(url: &str, access: Access, secrets: &Secrets) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match PostgresStore::open(url, access) {
        Ok(store) => Ok(store.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
        Err(fault) => Err(open_error(fault, secrets)),
    }
}

/// What a failed open is called: a database without pgvector (or with too old a one, or an index
/// still building) lacks a capability, anything else is a failure.
fn open_error(fault: Fault, secrets: &Secrets) -> OpenError {
    match fault {
        Fault::Problem(problem) if problem.is_missing() => OpenError::Missing {
            code: problem.code(),
            message: secrets.scrub(&problem.message()),
        },
        fault => OpenError::Failed(secrets.scrub(&format!(
            "cannot open the PostgreSQL database: {}",
            fault_message(fault)
        ))),
    }
}

/// What `fault` says, without the prefix of a store error.
fn fault_message(fault: Fault) -> String {
    match fault {
        Fault::Problem(problem) => problem.message(),
        Fault::Store(StoreError::Backend(message)) => message,
        Fault::Store(other) => other.to_string(),
    }
}

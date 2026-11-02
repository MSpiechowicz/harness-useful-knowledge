//! [`KnowledgeStore`] on a libSQL server (`sqld` self-hosted, or Turso), so a whole team shares one
//! database.
//!
//! libSQL is SQLite, so this backend keeps the tables, the FTS5 keyword index and the keyword and
//! list statements of [`crate::store::sqlite`] (shared through `sql_shared`), and differs in the
//! vectors only: an `F32_BLOB(384)` column with a cosine ANN index, searched by `vector_top_k`
//! (see [`search`]). Requests go over the server's HTTP pipeline API ([`hrana`]): a write holds
//! one server connection from `BEGIN IMMEDIATE` to `COMMIT`, so two clients writing or migrating
//! at once queue, and each write re-reads the schema version under that lock, so a client
//! connected before a newer server migrated stops writing ([`StoreError::NewerSchema`]).
//!
//! The connection is opened lazily and kept: the HTTP agent reuses its sockets and opens a new
//! one when a kept one broke, a read is repeated once on such a break, and every write starts on
//! a fresh stream. A write whose `COMMIT` got no answer may have been applied: it fails saying so
//! and is never sent again.
//!
//! Opening never writes unless asked: [`LibsqlStore::open_read_only`] answers `None` for a
//! database with no knowledge tables, [`LibsqlStore::open_existing`] migrates only one that has
//! them, and [`LibsqlStore::connect`] creates them.
//!
//! Inserting a vector updates the ANN graph, which dominates a write: a document of 4000 embedded
//! chunks takes about two minutes in its one transaction on a laptop-class server (the transport
//! is a few seconds of it), and another writer waits for the lock meanwhile.
//!
//! TLS is required except for a server on the same machine ([`endpoint`]); the token is sent as
//! `Authorization: Bearer` and appears in no message, like the URL. Proxy variables are ignored
//! ([`hrana`]). [`probe`] reports what the server can do, for `check`.

mod columns;
mod documents;
mod endpoint;
mod hrana;
mod listing;
mod meta;
mod probe;
mod quality;
mod schema;
mod search;
mod sql;
#[cfg(test)]
mod tests;
mod wire;

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use hrana::{COUNT_BUDGET, Client, Stream};
use sql::Sql;
use wire::{Outcome, execute};

pub use hrana::CONNECT_TIMEOUT;
pub use probe::{Capabilities, NO_VECTOR_MESSAGE, Trouble, probe};
pub use schema::SCHEMA_VERSION;

use crate::backend::{Access, OpenError};
use crate::config::Secrets;

use super::{ChunkHit, ChunkMatches, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link, ListQuery};
use super::{Columns, PendingChunk, RepoCounts, Scope, Stats, StoreError, TitleScope, clean};

/// How long a write keeps asking for a lock the server reports busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between those attempts.
const BUSY_RETRY: Duration = Duration::from_millis(50);

/// What opening may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Nothing: a database without tables is `None`, an old one an error.
    Read,
    /// Migrates a database that has tables, `None` for one that has none.
    Existing,
    /// Creates the tables and migrates.
    Create,
    /// Counters only: like [`Mode::Read`] (nothing is created, migrated or added, a database that is
    /// not current is an error), and every request is given up after [`COUNT_BUDGET`].
    Counters,
}

/// The team backend on a libSQL server. See the module docs.
pub struct LibsqlStore {
    client: Client,
    newer: bool,
    /// False for a store opened with [`LibsqlStore::open_read_only`]: its writes are refused.
    writable: bool,
    /// The columns added in place that the database has (see [`columns`]).
    columns: Columns,
}

impl std::fmt::Debug for LibsqlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibsqlStore")
            .field("newer", &self.newer)
            .field("writable", &self.writable)
            .finish_non_exhaustive()
    }
}

impl LibsqlStore {
    /// Connects to the server at `url` (`libsql://`, `https://`, or `http://` for localhost only)
    /// with an optional access token, creating the tables when the database has none and
    /// migrating it. A database written by a newer server connects, but every operation then
    /// fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str, token: Option<&str>) -> Result<Self, StoreError> {
        Self::open(url, token, Mode::Create)?
            .ok_or_else(|| StoreError::Backend("the database was not opened".to_owned()))
    }

    /// Like [`Self::connect`] when the database has its tables, `None` when it has none yet.
    pub fn open_existing(url: &str, token: Option<&str>) -> Result<Option<Self>, StoreError> {
        Self::open(url, token, Mode::Existing)
    }

    /// Connects for counting alone: nothing is created, migrated or added (a database that lacks
    /// the counter columns counts nothing, an older one is an error), and every request is given
    /// up after [`COUNT_BUDGET`]. `None` when the database has no tables yet.
    pub fn open_counters(url: &str, token: Option<&str>) -> Result<Option<Self>, StoreError> {
        Self::open(url, token, Mode::Counters)
    }

    /// Connects without creating or migrating anything: `None` when the database has no tables
    /// yet, an error when it is from an older server that has not upgraded it.
    pub fn open_read_only(url: &str, token: Option<&str>) -> Result<Option<Self>, StoreError> {
        Self::open(url, token, Mode::Read)
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    fn open(url: &str, token: Option<&str>, mode: Mode) -> Result<Option<Self>, StoreError> {
        Self::open_within(url, token, mode, hrana::CONNECT_TIMEOUT)
    }

    fn open_within(
        url: &str,
        token: Option<&str>,
        mode: Mode,
        connect_timeout: Duration,
    ) -> Result<Option<Self>, StoreError> {
        let client = Client::within(endpoint::parse(url)?, token, connect_timeout)?;
        let mut first = client.single();

        if mode == Mode::Counters {
            first.budget = Some(COUNT_BUDGET);
        }

        let found = schema::version(&mut first)?;
        let mut store = Self {
            client,
            newer: false,
            writable: matches!(mode, Mode::Existing | Mode::Create),
            columns: Columns::NONE,
        };

        match (found, mode) {
            (None, Mode::Read | Mode::Existing | Mode::Counters) => return Ok(None),
            (Some(version), Mode::Read | Mode::Counters) if version < SCHEMA_VERSION => {
                return Err(StoreError::Backend(
                    "the database is from an older server and has not been upgraded yet".to_owned(),
                ));
            }
            (Some(SCHEMA_VERSION), _) => {}
            (Some(version), _) if version > SCHEMA_VERSION => store.newer = true,
            _ => store.newer = store.migrate()?,
        }

        if !store.newer {
            store.columns = store.open_columns(mode)?;
        }

        Ok(Some(store))
    }

    /// Brings the database to [`SCHEMA_VERSION`] under the write lock (another client may have
    /// migrated meanwhile). True when it turned out to be from a newer server.
    fn migrate(&mut self) -> Result<bool, StoreError> {
        self.transact(|db| match schema::version(db)? {
            Some(version) if version > SCHEMA_VERSION => Ok(true),
            Some(SCHEMA_VERSION) => Ok(false),
            found => {
                schema::upgrade(db, found.unwrap_or(0))?;
                Ok(false)
            }
        })
    }

    fn check_version(&self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(())
    }

    /// Runs `op` on a one-shot stream: every statement is its own request.
    fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.check_version()?;
        op(&mut self.client.single())
    }

    /// Runs `op` in a write transaction that takes the write lock first, committed when `op`
    /// succeeds and rolled back when it fails. The server's schema version is re-read under the
    /// lock, and the generation grows with every committed write.
    fn write<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.check_version()?;

        if !self.writable {
            return Err(StoreError::Backend("the database was opened read-only".to_owned()));
        }

        let written = self.transact(|db| {
            let version: Option<i64> = meta::get(db, "schema_version")?.and_then(|text| text.parse().ok());

            if version.is_some_and(|version| version > SCHEMA_VERSION) {
                return Err(StoreError::NewerSchema);
            }

            let done = op(db)?;
            meta::bump(db)?;

            Ok(done)
        });

        if matches!(written, Err(StoreError::NewerSchema)) {
            self.newer = true;
        }

        written
    }

    /// Runs `op` between `BEGIN IMMEDIATE` and `COMMIT` on one server connection, rolling back
    /// when it fails. The commit is sent once; see [`hrana::Stream::commit`].
    fn transact<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let mut stream = begin(&self.client)?;

        match op(&mut stream) {
            Ok(done) => {
                stream.commit()?;
                Ok(done)
            }
            Err(error) => {
                // The error that ended the transaction is the one to report.
                stream.rollback().ok();
                Err(error)
            }
        }
    }
}

impl KnowledgeStore for LibsqlStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.read(|db| meta::get(db, key))
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        super::sqlite::sql_shared::check_settable(key)?;
        self.write(|db| meta::set(db, key, value))
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.read(meta::generation)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[super::Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let title = clean::title(&doc.title)?;
        let links = clean::links(links)?;
        clean::chunks(chunks)?;

        let columns = self.columns;

        self.write(|db| {
            let id = documents::put(db, doc, &title, chunks, &links)?;
            documents::stamp_authors(db, columns, id, doc)?;

            Ok(id)
        })
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        let columns = self.columns;

        self.read(|db| documents::find(db, columns, "content_sha256", sha))
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        let columns = self.columns;

        self.read(|db| documents::find(db, columns, "source_key", key))
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        self.read(|db| documents::reusable_embeddings(db, model_id, chunk_shas))
    }

    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        self.read(|db| documents::pending_chunks(db, model_id, after, limit))
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        clean::embeddings(model_id, embeddings)?;

        self.write(|db| documents::set_embeddings(db, model_id, embeddings))
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count_useful(ids, at)
    }

    fn find_by_title(&mut self, title: &str, scope: &TitleScope) -> Result<Vec<Document>, StoreError> {
        let columns = self.columns;

        self.read(|db| quality::by_title(db, columns, title, scope))
    }

    fn docs_sharing_chunks(&mut self, chunk_shas: &[String]) -> Result<Vec<(i64, usize)>, StoreError> {
        self.read(|db| quality::sharing(db, chunk_shas))
    }

    /// True for a database opened for writing, as far as this store can tell: a token that may
    /// only read is not noticed. A handle opened to read says nothing.
    fn remote_writable(&mut self) -> Option<bool> {
        self.writable.then_some(true)
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let links = clean::links(links)?;

        self.write(|db| documents::set_links(db, id, &links))
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        let title = clean::title(title)?;

        self.write(|db| documents::rename(db, id, &title))
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        self.write(|db| documents::delete(db, ids))
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        let columns = self.columns;

        self.read(|db| listing::list(db, columns, query))
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        let columns = self.columns;

        self.read(|db| listing::get(db, columns, id, detail))
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.read(|db| search::keyword_top_k(db, query, scope, k))
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.read(|db| search::vector_top_k(db, vector, scope, k))
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        self.read(|db| listing::matching_chunks(db, doc_id, text, limit))
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.read(listing::repos)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.read(listing::stats)
    }
}

/// A stream inside `BEGIN IMMEDIATE`, with foreign keys enforced (deleting a document relies on
/// them to remove its chunks and links; the pragma is a no-op inside a transaction, so it goes
/// first). A server that rejects the pragma gets no write. The server queues a writer while
/// another holds the lock; a busy answer is retried until [`BUSY_TIMEOUT`].
fn begin(client: &Client) -> Result<Stream<'_>, StoreError> {
    let mut stream = client.stream();
    let deadline = Instant::now() + BUSY_TIMEOUT;

    let mut outcomes = stream.send(vec![execute("PRAGMA foreign_keys = ON"), execute("BEGIN IMMEDIATE")])?;
    let mut started = outcomes.remove(1);

    if let Outcome::Error { error } = outcomes.remove(0) {
        stream.close();
        return Err(StoreError::Backend(format!(
            "the libSQL server rejected PRAGMA foreign_keys = ON ({}), which deleting a document needs to remove \
             its chunks, so nothing was written",
            error.code().unwrap_or("no error code")
        )));
    }

    loop {
        match started {
            Outcome::Ok { .. } => return Ok(stream),
            Outcome::Error { error } if error.is_busy() && Instant::now() < deadline => {
                thread::sleep(BUSY_RETRY);
                started = stream.send(vec![execute("BEGIN IMMEDIATE")])?.remove(0);
            }
            Outcome::Error { error } => {
                stream.close();
                return Err(error.into_store_error());
            }
        }
    }
}

/// The store `backend::open` hands out for a libSQL URL: `None` when `access` does not create the
/// tables and the database has none, and connecting is given up on after `connect_timeout`. A failure is one readable line without the URL or the token
/// (every secret of them is scrubbed from it).
pub fn open_boxed(
    url: &str,
    token: Option<&str>,
    access: Access,
    connect_timeout: Duration,
    secrets: &Secrets,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    let mode = match access {
        Access::Read => Mode::Read,
        Access::Existing => Mode::Existing,
        Access::Create => Mode::Create,
        Access::Counters => Mode::Counters,
    };
    let opened = LibsqlStore::open_within(url, token, mode, connect_timeout);

    match opened {
        Ok(store) => Ok(store.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
        Err(error) => Err(open_error(error, secrets)),
    }
}

/// What a failed open is called: a server without FTS5 or without vectors lacks a capability.
fn open_error(error: StoreError, secrets: &Secrets) -> OpenError {
    match error {
        StoreError::NoFts5 => OpenError::no_fts5(),
        StoreError::Backend(message) if message == NO_VECTOR_MESSAGE => OpenError::Missing {
            code: "libsql-no-vector",
            message,
        },
        StoreError::Unreachable(message) => OpenError::Unreachable(secrets.scrub(&message)),
        error => OpenError::Failed(secrets.scrub(&format!("cannot open the libSQL database: {error}"))),
    }
}

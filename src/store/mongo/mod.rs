//! [`KnowledgeStore`] over a MongoDB database with a search engine, so a team can share one
//! knowledge base: Atlas, or MongoDB 8.2 and newer with `mongot`. Keywords are searched with
//! `$search` and vectors with `$vectorSearch` (see [`search`]); a server without `mongot` is told
//! by `check` as `no-mongot` ([`probe`]).
//!
//! # Layout
//!
//! The database is the one in the URL path (`harness_knowledge` when the URL names none) and holds
//! three collections, `knowledge_meta`, `knowledge_documents` and `knowledge_chunks`, and two
//! search indexes on the chunks. See [`schema`] for the fields and the indexes. Ids are `i64`
//! like the trait's, taken from counters in the meta collection with `$inc`.
//!
//! # Writes
//!
//! A write runs in a transaction when the deployment has them (a replica set or a sharded
//! cluster), retried from the start on transient errors and write conflicts, so a document and its
//! chunks appear together or not at all. Without transactions the writes are ordered (the
//! document, then the chunks) and a failed one cleans up what it wrote, as far as the database
//! answers: a new document is removed, a replaced one loses its content hash and chunks so the
//! next import of the file stores it again. The write counter grows with every write.
//!
//! # Reads
//!
//! A read never creates anything: [`MongoStore::connect_read_only`] answers `None` for a database
//! nobody wrote to yet. The list, the lookups and the counts are strongly consistent. Both search
//! indexes are eventually consistent (a chunk is searchable about a second after its write), and
//! a search on an index that is still being built fails with a message that says so.
//!
//! # Connection
//!
//! `mongodb://` or `mongodb+srv://`. A server that is not on this machine must use TLS (turned on
//! when the URL leaves it unset, `tls=false` and disabled certificate checks are refused).
//! Loopback hosts count as this machine only with `directConnection=true` or `loadBalanced=true`,
//! and Unix sockets always do. `tlsCAFile` and `tlsCertificateKeyFile` must be absolute paths, as
//! a relative one would be read from the project. One client is opened for the process and
//! reused, and the driver reconnects it when the connection breaks. Error messages carry error
//! kinds, codes and plain code names, never server text, the URL or its credentials.

mod connection;
mod ctx;
mod documents;
mod listing;
mod meta;
mod migrate;
mod open;
pub mod probe;
mod quality;
pub mod schema;
mod search;
mod terms;
#[cfg(test)]
mod tests;
mod write;

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mongodb::bson::{Bson, Document as Row};
use mongodb::error::Error;
use mongodb::sync::{Client, Database};

use open::{Opening, connect_client, connect_client_for, transactions};
pub use open::{open_boxed, open_boxed_with};

use super::upgrade::{Plan, plan};
use crate::backend::{Access, Opened, Upgrade};
use ctx::{Ctx, Fail};
use schema::IndexState;

use super::{
    Chunk, ChunkHit, ChunkMatches, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link, ListQuery,
};
use super::{PendingChunk, RepoCounts, Scope, Stats, StoreError, TitleScope, clean};

/// The database used when the URL path names none.
pub const DEFAULT_DATABASE: &str = "harness_knowledge";

/// How long the driver looks for a usable server when the URL does not say.
pub const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of the database a connection may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Reads only, nothing is created or migrated.
    ReadOnly,
    /// Reads and writes what is there (migrating it), creates nothing that is not.
    Existing,
    /// Reads and writes, creating the database when it does not exist.
    Create,
    /// Counts and nothing else: nothing is created, migrated or brought back, and writes other than
    /// the counters are refused.
    Counters,
}

/// The MongoDB team backend. See the module docs.
pub struct MongoStore {
    db: Database,
    client: Client,
    transactions: bool,
    newer: bool,
    /// Opened with [`MongoStore::connect_read_only`]: writes are refused.
    read_only: bool,
    /// Both search indexes answered queries once, so they are not asked about again.
    searchable: bool,
    /// A counter write is running on its own thread (see [`quality`]).
    counting: &'static AtomicBool,
}

impl std::fmt::Debug for MongoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoStore")
            .field("database", &self.db.name())
            .field("transactions", &self.transactions)
            .field("newer", &self.newer)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

impl MongoStore {
    /// Connects to the database of `url`, creating and migrating it. A database written by a
    /// newer server connects, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        Ok(Self::open(url, Mode::Create)?.expect("a created database exists"))
    }

    /// Connects to the database of `url` and migrates it when it exists, `None` when nobody wrote
    /// to it yet: for commands that change a document and must not create the database.
    pub fn connect_existing(url: &str) -> Result<Option<Self>, StoreError> {
        Self::open(url, Mode::Existing)
    }

    /// Connects for reading only, so nothing is created, migrated or changed. `None` when nobody
    /// wrote to the database yet.
    pub fn connect_read_only(url: &str) -> Result<Option<Self>, StoreError> {
        Self::open(url, Mode::ReadOnly)
    }

    fn open(url: &str, mode: Mode) -> Result<Option<Self>, StoreError> {
        match Self::open_checked(url, mode, SERVER_SELECTION_TIMEOUT, Upgrade::Run)? {
            Opened::Ready(store) => Ok(store),
            Opened::NeedsUpgrade => Err(StoreError::Backend("the database needs an upgrade".to_owned())),
        }
    }

    /// [`Self::open`], telling a server without `mongot` apart from other failures. With
    /// [`Upgrade::Defer`] a database that has data from an older version is left as it is and the
    /// answer is [`Opened::NeedsUpgrade`]: the caller upgrades it in an open of its own (with
    /// [`Upgrade::Run`]) under a bound that suits an upgrade (see [`schema`]).
    fn open_checked(
        url: &str,
        mode: Mode,
        selection_timeout: Duration,
        upgrade: Upgrade,
    ) -> Result<Opened<Self>, Opening> {
        let (client, db) = connect_client_for(url, selection_timeout, mode)?;
        let transactions = transactions(&client)?;
        let failed = |error: Error| match connection::missing(&error) {
            Some((code, message)) => Opening::Missing { code, message },
            None => Opening::Failed(connection::backend(&error)),
        };

        let version = schema::version(&db).map_err(failed)?;
        let access = match mode {
            Mode::ReadOnly => Access::Read,
            Mode::Existing => Access::Existing,
            Mode::Create => Access::Create,
            Mode::Counters => Access::Counters,
        };

        let newer = match plan(
            (version > 0).then_some(version),
            access,
            schema::SCHEMA_VERSION,
            schema::READABLE_FROM,
        ) {
            Plan::Absent => return Ok(Opened::Ready(None)),
            // A writer also brings back a search index that was dropped, whatever the version.
            Plan::Current if matches!(mode, Mode::Existing | Mode::Create) => schema::migrate(&db).map_err(failed)?,
            Plan::Current => false,
            Plan::Newer => true,
            Plan::TooOld => {
                return Err(Opening::Failed(StoreError::Backend(
                    "the database is from an older server and has not been upgraded yet".to_owned(),
                )));
            }
            Plan::Migrate { creates: false } if upgrade == Upgrade::Defer => return Ok(Opened::NeedsUpgrade),
            Plan::Migrate { .. } => schema::migrate(&db).map_err(failed)?,
        };

        Ok(Opened::Ready(Some(Self {
            db,
            client,
            transactions,
            newer,
            read_only: matches!(mode, Mode::ReadOnly | Mode::Counters),
            searchable: false,
            counting: quality::counting_gate(),
        })))
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// True when writes run in transactions.
    pub fn uses_transactions(&self) -> bool {
        self.transactions
    }

    /// The name of the database in use.
    pub fn database_name(&self) -> &str {
        self.db.name()
    }

    /// Waits until both search indexes answer queries, at most `wait`, and returns the state each
    /// ended in. An import that wants its documents searchable at once can use it.
    pub fn wait_for_indexes(&self, wait: Duration) -> Result<(IndexState, IndexState), StoreError> {
        schema::wait_for_indexes(&self.db, wait).map_err(|error| connection::backend(&error))
    }

    /// A context for reads, outside any transaction.
    fn reader(&self) -> Result<Ctx<'_>, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(Ctx::new(&self.db, None))
    }

    /// Fails once the stored schema is newer than this server, also when it changed after connect.
    fn check_version(&mut self) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        let version = schema::version(&self.db).map_err(|error| connection::backend(&error))?;

        if version > schema::SCHEMA_VERSION {
            self.newer = true;
            return Err(StoreError::NewerSchema);
        }

        Ok(())
    }

    /// Fails with what is wrong when a search index cannot answer yet.
    fn check_searchable(&mut self) -> Result<(), StoreError> {
        if self.searchable {
            return Ok(());
        }

        let states = schema::search_states(&self.db).map_err(|error| connection::backend(&error))?;
        let keyword = schema::state_of(&states, schema::KEYWORD_INDEX);
        let vector = schema::state_of(&states, schema::VECTOR_INDEX);

        if keyword == IndexState::Ready && vector == IndexState::Ready {
            self.searchable = true;
            return Ok(());
        }

        if keyword == IndexState::Missing || vector == IndexState::Missing {
            return Err(StoreError::Backend(
                "the search indexes of the knowledge database are missing, the next import creates them".to_owned(),
            ));
        }

        Err(StoreError::Backend(
            "the search indexes of the knowledge database are still being built, try again in a moment".to_owned(),
        ))
    }

    /// Reads with `op`.
    fn read<T>(&self, op: impl FnOnce(&mut Ctx<'_>) -> Result<T, Fail>) -> Result<T, StoreError> {
        op(&mut self.reader()?).map_err(Fail::into_store)
    }
}

impl KnowledgeStore for MongoStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.read(|ctx| meta::get(ctx, key))
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        meta::check_settable(key)?;
        self.write(|ctx| meta::set(ctx, key, value))
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.read(meta::generation)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let title = clean::title(&doc.title)?;
        let links = clean::links(links)?;
        clean::chunks(chunks)?;

        // A new document's id is taken once, so a retry of the write is the same write.
        let id = match doc.id {
            0 => self.read(|ctx| ctx.reserve_ids("documents", 1))?,
            id => id,
        };

        let begun = Cell::new(false);
        let stored = self.write(|ctx| documents::put(ctx, id, doc, &title, (chunks, &links), &begun));

        // In a transaction a failure leaves nothing. Outside one it may have left a part.
        if stored.is_err() && begun.get() && !self.transactions {
            let _ = self.read(|ctx| {
                documents::undo_put(ctx, id, doc.id == 0);
                Ok(())
            });
        }

        stored
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        self.read(|ctx| documents::find(ctx, "content_sha256", sha))
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        self.read(|ctx| documents::find(ctx, "source_key", key))
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        self.read(|ctx| documents::reusable_embeddings(ctx, model_id, chunk_shas))
    }

    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        self.read(|ctx| documents::pending_chunks(ctx, model_id, after, limit))
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        clean::embeddings(model_id, embeddings)?;

        self.write(|ctx| documents::set_embeddings(ctx, model_id, embeddings))
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count_useful(ids, at)
    }

    fn find_by_title(&mut self, title: &str, scope: &TitleScope) -> Result<Vec<Document>, StoreError> {
        self.read(|ctx| quality::by_title(ctx, title, scope))
    }

    fn docs_sharing_chunks(&mut self, chunk_shas: &[String]) -> Result<Vec<(i64, usize)>, StoreError> {
        self.read(|ctx| quality::sharing(ctx, chunk_shas))
    }

    /// Whether the connected user holds the `update` privilege on the documents, as the server
    /// reports it; `None` when it does not say.
    fn remote_writable(&mut self) -> Option<bool> {
        self.may_update()
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let links = clean::links(links)?;

        self.write(|ctx| documents::set_links(ctx, id, &links))
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        let title = clean::title(title)?;

        self.write(|ctx| documents::rename(ctx, id, &title))
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        self.write(|ctx| documents::delete(ctx, ids))
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.read(|ctx| listing::list(ctx, query))
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        self.read(|ctx| listing::get(ctx, id, detail))
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.reader()?;
        self.check_searchable()?;
        self.read(|ctx| search::keyword_top_k(ctx, query, scope, k))
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.reader()?;
        search::check_vector(vector)?;
        self.check_searchable()?;

        // Only the vectors of the current model compare, once one is recorded.
        let model = self.meta("model_id")?;

        self.read(|ctx| search::vector_top_k(ctx, vector, scope, model.as_deref(), k))
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        self.read(|ctx| listing::matching_chunks(ctx, doc_id, text, limit))
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.read(listing::repos)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.read(|ctx| {
            let model_id = meta::get(ctx, "model_id")?;
            let generation = meta::generation(ctx)?;
            let schema_version = meta::get(ctx, "schema_version")?.and_then(|version| version.parse().ok());

            listing::stats(
                ctx,
                model_id,
                generation,
                schema_version.unwrap_or(schema::SCHEMA_VERSION),
            )
        })
    }
}

// Reading values back from a stored document.

/// An integer field (0 when missing or not a number).
fn int(row: &Row, key: &str) -> i64 {
    match row.get(key) {
        Some(Bson::Int64(value)) => *value,
        Some(Bson::Int32(value)) => i64::from(*value),
        Some(Bson::Double(value)) => *value as i64,
        _ => 0,
    }
}

/// A string field, `None` when missing or not a string.
fn text(row: &Row, key: &str) -> Option<String> {
    row.get_str(key).ok().map(str::to_owned)
}

/// The strings of an array field (empty when missing).
fn strings(row: &Row, key: &str) -> Vec<String> {
    row.get_array(key)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

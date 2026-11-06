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
//! writing as soon as a newer server migrates the database, not only when it connects. Every read
//! re-reads it too (one primary-key lookup in front of the read, see [`PostgresStore::read`]), so a
//! client connected before a newer server migrated stops reading as well: a version that keeps
//! drafts apart from live documents must not be read by one that does not know them. Sessions
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

mod columns;
mod connect;
mod documents;
mod error;
mod listing;
mod meta;
mod probe;
mod quality;
mod review;
pub mod schema;
mod search;
mod session;
#[cfg(test)]
mod tests;
pub mod vector;

use std::collections::HashMap;
use std::time::Duration;

use postgres::{Client, Transaction};

pub use connect::{CONNECT_TIMEOUT, UrlError};
pub use probe::{Probe, probe};
use schema::{READABLE_FROM, SCHEMA_VERSION, SchemaState};
use vector::Version;
pub use vector::{Fault, Problem};

use super::review::{
    COMMENT_BODY_MAX, actor, check_change, check_comments, check_file_size, comment_body, file_name, file_sha,
};
use super::upgrade::{Plan, plan};
use super::{
    AuthorFacet, Comment, History, NewComment, PendingChunk, PendingVersion, RepoCounts, ReviewChange, ReviewReply,
};
use super::{
    Chunk, ChunkHit, ChunkMatches, Columns, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link,
    ListQuery,
};
use super::{Scope, SectionText, Stats, StoreError, StoredFile, TitleScope, Visibility, clean};
use crate::backend::{Access, OpenError, Opened, Upgrade};
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
    /// The columns added in place that the database has (see [`columns`]).
    columns: Columns,
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
        Self::open_within(url, access, connect::CONNECT_TIMEOUT)
    }

    /// [`PostgresStore::open`] giving up on a connection after `connect_timeout` unless the URL
    /// names its own `connect_timeout`.
    pub fn open_within(url: &str, access: Access, connect_timeout: Duration) -> Result<Option<Self>, Fault> {
        match Self::open_with(url, access, connect_timeout, Upgrade::Run)? {
            Opened::Ready(store) => Ok(store),
            Opened::NeedsUpgrade => Err(StoreError::Backend("the database needs an upgrade".to_owned()).into()),
        }
    }

    /// [`PostgresStore::open_within`] that, with [`Upgrade::Defer`], changes nothing of a database
    /// that has tables from an older version and answers [`Opened::NeedsUpgrade`] instead: the
    /// caller runs the upgrade in an open of its own (with [`Upgrade::Run`]), under a bound that
    /// suits a migration (see [`schema`]).
    pub fn open_with(
        url: &str,
        access: Access,
        connect_timeout: Duration,
        upgrade: Upgrade,
    ) -> Result<Opened<Self>, Fault> {
        let settings =
            connect::parse_within(url, connect_timeout).map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut client = session::open(&settings)?;

        let found = schema::stored_version(&mut client)?;

        let state = match plan(found, access, SCHEMA_VERSION, READABLE_FROM) {
            Plan::Absent => return Ok(Opened::Ready(None)),
            Plan::Current => SchemaState::Current,
            Plan::Newer => SchemaState::Newer,
            Plan::TooOld => {
                return Err(StoreError::Backend(
                    "the database is from an older server and has not been upgraded yet".to_owned(),
                )
                .into());
            }
            Plan::Migrate { creates: false } if upgrade == Upgrade::Defer => return Ok(Opened::NeedsUpgrade),
            Plan::Migrate { .. } => schema::migrate(&mut client)?,
        };

        let pgvector = vector::installed(&mut client)?;
        let newer = state == SchemaState::Newer;
        let columns = if newer {
            Columns::NONE
        } else {
            open_columns(&mut client, access)?
        };

        let mut store = Self {
            settings,
            client,
            newer,
            pgvector,
            columns,
        };

        // A writer takes out what an earlier version let in: a team database keeps no folder of
        // this computer. A reader, or a role that may not write, leaves it for a writer.
        if matches!(access, Access::Existing | Access::Create) && !store.newer {
            store.clean_local_paths();
        }

        Ok(Opened::Ready(Some(store)))
    }

    /// Deletes every link to a folder of this computer (a `path:` key): a team database keeps only
    /// `git:` repositories. Idempotent, and a plain look when there is nothing to delete. The
    /// number of links deleted.
    pub fn drop_local_paths(&mut self) -> Result<usize, StoreError> {
        if self.read(documents::count_local_paths)? == 0 {
            return Ok(0);
        }

        self.write_if(|tx| documents::drop_local_paths(tx))
    }

    /// [`Self::drop_local_paths`] on opening, saying on stderr what it did or could not do (a
    /// read-only role sees that line on each open, and nothing else changes for it).
    fn clean_local_paths(&mut self) {
        match self.drop_local_paths() {
            Ok(removed) => clean::report_local_paths(removed, 0),
            Err(_) => {
                let left = self.read(documents::count_local_paths).unwrap_or(0);

                clean::report_local_paths(0, left);
            }
        }
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
    /// because the connection closed meanwhile. The schema version is read first, every time: a
    /// database a newer server migrated meanwhile fails the read with [`StoreError::NewerSchema`]
    /// (and every later call, see [`Self::ready`]). The client is synchronous, so the check is a
    /// lookup of its own in front of the read, not a column of it.
    fn read<T>(&mut self, mut operation: impl FnMut(&mut Client) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.ready()?;

        let mut checked = |client: &mut Client| {
            if schema::current_version(client)?.is_some_and(|found| found > SCHEMA_VERSION) {
                return Err(StoreError::NewerSchema);
            }

            operation(client)
        };

        let result = match checked(&mut self.client) {
            Err(error) if self.client.is_closed() || ended(&error) => {
                self.client = session::open(&self.settings)?;
                checked(&mut self.client)
            }
            other => other,
        };

        if matches!(result, Err(StoreError::NewerSchema)) {
            self.newer = true;
        }

        result
    }

    /// Runs `operation` in a write transaction (see [`transact`]). It runs once more after
    /// reconnecting only when the connection closed before `COMMIT` was sent, see the module docs.
    fn write<T>(
        &mut self,
        mut operation: impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.write_if(|tx| operation(tx).map(|done| (done, true)))
    }

    /// [`Self::write`] for an `operation` that says whether it changed anything: a refusal commits
    /// nothing and does not count as a write (the generation stays).
    fn write_if<T>(
        &mut self,
        mut operation: impl FnMut(&mut Transaction<'_>) -> Result<(T, bool), StoreError>,
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
/// meanwhile, runs `operation`, counts the write (when `operation` changed anything) and commits. A
/// failure drops the transaction, which rolls it back.
fn transact<T>(
    client: &mut Client,
    operation: &mut impl FnMut(&mut Transaction<'_>) -> Result<(T, bool), StoreError>,
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

    let (result, changed) = operation(&mut tx).map_err(Failure::BeforeCommit)?;

    if changed {
        meta::bump(&mut tx).map_err(Failure::BeforeCommit)?;
    }

    tx.commit().map_err(|error| Failure::AtCommit(backend(error)))?;

    Ok(result)
}

/// Adds the columns the database lacks when `access` writes, and reports which it has now. A
/// database that refuses them (a role without the right to change the schema) is used with what it
/// has, with one line on stderr.
fn open_columns(client: &mut Client, access: Access) -> Result<Columns, StoreError> {
    if matches!(access, Access::Existing | Access::Create)
        && let Err(error) = columns::ensure(client)
    {
        super::quality::warn_unavailable(&error);
    }

    columns::present(client)
}

/// The error of a `COMMIT` whose connection closed: the server may or may not have saved it.
fn uncertain(error: StoreError) -> StoreError {
    match error {
        StoreError::Backend(message) | StoreError::Unreachable(message) => StoreError::Backend(format!(
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

    fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError> {
        for (key, _) in entries {
            meta::check_settable(key)?;
        }

        self.write(|tx| {
            for (key, value) in entries {
                meta::set(tx, key, value)?;
            }

            Ok(())
        })
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.read(meta::generation)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let title = clean::title(&doc.title)?;
        let links = clean::team_links(links)?;
        clean::chunks(chunks)?;

        let columns = self.columns;

        self.write(|tx| {
            let id = documents::put(tx, doc, &title, chunks, &links)?;
            documents::stamp_authors(tx, columns, id, doc)?;

            Ok(id)
        })
    }

    fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError> {
        let columns = self.columns;

        self.read(|client| documents::find(client, columns, "content_sha256", sha, visibility))
    }

    fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError> {
        // A folder of this computer is not asked about here.
        if clean::is_local_source_key(key) {
            return Ok(None);
        }

        let columns = self.columns;

        self.read(|client| documents::find(client, columns, "source_key", key, visibility))
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

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count_useful(ids, at)
    }

    fn find_by_title(
        &mut self,
        title: &str,
        scope: &TitleScope,
        visibility: Visibility,
    ) -> Result<Vec<Document>, StoreError> {
        if scope.is_local_path() {
            return Ok(Vec::new());
        }

        let columns = self.columns;

        self.read(|client| quality::by_title(client, columns, title, scope, visibility))
    }

    fn docs_sharing_chunks(
        &mut self,
        chunk_shas: &[String],
        visibility: Visibility,
    ) -> Result<Vec<(i64, usize)>, StoreError> {
        self.read(|client| quality::sharing(client, chunk_shas, visibility))
    }

    /// Whether this role has the UPDATE privilege on the documents; `None` when the probe fails.
    fn remote_writable(&mut self) -> Option<bool> {
        self.read(|client| Ok(quality::may_update(client))).ok().flatten()
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let links = clean::team_links(links)?;

        self.write(|tx| documents::set_links(tx, id, &links))
    }

    fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError> {
        let title = clean::title(title)?;

        self.write(|tx| documents::rename(tx, id, &title, by, at))
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        self.write(|tx| documents::delete(tx, ids))
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        // A team database holds no link to a folder of this computer, and is not asked for one.
        if query.filter.is_local_path() {
            return Ok(DocumentPage {
                items: Vec::new(),
                total: 0,
                authors: AuthorFacet::default(),
            });
        }

        let columns = self.columns;

        self.read(|client| listing::list(client, columns, query))
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        let columns = self.columns;

        self.read(|client| listing::get(client, columns, id, detail))
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        let scope = &*scope.for_team();
        self.read(|client| search::keyword_top_k(client, query, scope, k))
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        let scope = &*scope.for_team();
        let pgvector = self.pgvector;

        self.read(|client| search::vector_top_k(client, pgvector, vector, scope, k))
    }

    fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError> {
        let columns = self.columns;

        self.read(|client| Ok(documents::pending_of_many(client, columns, &[id])?.remove(&id)))
    }

    fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        let (by, comment) = check_change(change)?;

        self.write_if(|tx| review::set_review(tx, id, expect, change, (by.as_deref(), comment.as_deref())))
    }

    fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        if !change.action.goes_live() {
            return Err(StoreError::Invalid("only approving or going live promotes".to_owned()));
        }

        let (by, comment) = check_change(change)?;
        let columns = self.columns;

        self.write_if(|tx| review::promote(tx, columns, id, expect, change, (by.as_deref(), comment.as_deref())))
    }

    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError> {
        self.read(|client| review::history(client, id))
    }

    fn authorship_of(
        &mut self,
        ids: &[i64],
        per_doc: usize,
    ) -> Result<HashMap<i64, crate::store::Authorship>, StoreError> {
        self.read(|client| review::authorship_of(client, ids, per_doc))
    }

    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError> {
        self.read(|client| review::writers_since_live(client, id))
    }

    fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        self.read(|client| review::section_texts(client, id))
    }

    fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        self.read(|client| review::asked_sections(client, id, revision))
    }

    fn add_comments(
        &mut self,
        id: i64,
        by: Option<&str>,
        at: i64,
        revision: i64,
        comments: &[NewComment],
    ) -> Result<Vec<i64>, StoreError> {
        let comments = check_comments(comments)?;
        let by = actor(by);

        self.write(|tx| review::add_comments(tx, id, by.as_deref(), at, revision, &comments))
    }

    fn reply_comment(
        &mut self,
        id: i64,
        parent: i64,
        by: Option<&str>,
        at: i64,
        body: &str,
    ) -> Result<Option<i64>, StoreError> {
        let body = comment_body(body, COMMENT_BODY_MAX)?;
        let by = actor(by);

        self.write_if(|tx| review::reply_comment(tx, id, parent, by.as_deref(), at, &body))
    }

    fn resolve_comment(
        &mut self,
        id: i64,
        comment: i64,
        resolved: bool,
        by: Option<&str>,
        at: i64,
    ) -> Result<bool, StoreError> {
        let by = actor(by);

        self.write_if(|tx| review::resolve_comment(tx, id, comment, resolved, by.as_deref(), at))
    }

    fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError> {
        self.write_if(|tx| review::delete_comment(tx, id, comment))
    }

    fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError> {
        self.read(|client| review::comments(client, id))
    }

    fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        let name = file_name(name)?;
        check_file_size(bytes)?;
        let sha256 = file_sha(bytes);

        self.write_if(|tx| review::put_file(tx, id, &name, bytes, &sha256))
    }

    fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError> {
        self.read(|client| review::file(client, id))
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        self.read(|client| listing::matching_chunks(client, doc_id, text, limit))
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.read(listing::repos)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.read(listing::stats)
    }
}

/// The store `backend::open` hands out for a PostgreSQL URL: `None` when `access` does not create
/// the schema and the database has none, and connecting is given up on after `connect_timeout`.
/// Failures are one readable line without the URL (and with every secret of it scrubbed).
pub fn open_boxed(
    url: &str,
    access: Access,
    connect_timeout: Duration,
    secrets: &Secrets,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match open_boxed_with(url, access, connect_timeout, Upgrade::Run, secrets)? {
        Opened::Ready(store) => Ok(store),
        Opened::NeedsUpgrade => Err(OpenError::Failed(
            "cannot open the PostgreSQL database: it needs an upgrade".to_owned(),
        )),
    }
}

/// [`open_boxed`] that may leave an upgrade of the database to a later open (see
/// [`PostgresStore::open_with`]).
pub fn open_boxed_with(
    url: &str,
    access: Access,
    connect_timeout: Duration,
    upgrade: Upgrade,
    secrets: &Secrets,
) -> Result<Opened<Box<dyn KnowledgeStore>>, OpenError> {
    match PostgresStore::open_with(url, access, connect_timeout, upgrade) {
        Ok(opened) => Ok(opened.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
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
        Fault::Problem(Problem::Unreachable(message)) | Fault::Store(StoreError::Unreachable(message)) => {
            OpenError::Unreachable(secrets.scrub(&format!("cannot reach the PostgreSQL database: {message}")))
        }
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
        Fault::Store(StoreError::Backend(message) | StoreError::Unreachable(message)) => message,
        Fault::Store(other) => other.to_string(),
    }
}

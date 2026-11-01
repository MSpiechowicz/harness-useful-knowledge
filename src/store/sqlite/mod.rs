//! [`KnowledgeStore`] over one SQLite file (WAL, FTS5 keyword index, vectors as BLOBs).
//!
//! Several processes (the MCP servers of running agents, the desktop's commands, the embedder)
//! share the file: every write runs in an `IMMEDIATE` transaction and waits up to 5 s for the
//! lock, so writers queue instead of failing with BUSY. Input is cleaned and checked before that
//! transaction opens, so a rejected call never holds it.
//!
//! A read never creates the file: [`SqliteStore::open_read_only`] answers `None` for a database
//! that does not exist yet (callers then answer as for an empty one).
//!
//! The vectors are searched by brute force: a scope prefilter in SQL, then the dot product of the
//! stored vectors with the query (all are L2-normalised, so it is the cosine), see [`search`].

mod documents;
mod listing;
mod meta;
pub mod schema;
mod search;
pub(crate) mod sql_shared;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use super::{ChunkHit, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link, ListQuery};
use super::{RepoCounts, Scope, Stats, StoreError, clean};
use schema::SchemaState;

/// How long a statement waits for another process's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between attempts to initialize a database another process holds.
const BUSY_RETRY: Duration = Duration::from_millis(20);

/// The local backend. See the module docs.
pub struct SqliteStore {
    conn: Connection,
    newer: bool,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("newer", &self.newer)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens the database at `path`, creating it when missing, and migrates it. On Unix a created
    /// parent folder is private (0700) and the file is 0600. A database written by a newer server
    /// opens, but every operation then fails with [`StoreError::NewerSchema`].
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        prepare_file(path)
            .map_err(|error| StoreError::Backend(format!("cannot create the database: {}", error.kind())))?;

        Self::connect(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    /// Opens the database at `path` for reading and writing when the file exists (migrating it),
    /// `None` when it does not: for commands that change a document and must not create the file.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, StoreError> {
        if !exists(path)? {
            return Ok(None);
        }

        Self::connect(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map(Some)
    }

    /// Opens the database at `path` read-only, so it cannot be created, migrated or changed.
    /// `None` when the file does not exist or nothing was written to it yet (a server that has
    /// not finished creating it): there is nothing to read.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, StoreError> {
        if !exists(path)? {
            return Ok(None);
        }

        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;

        let version = schema::user_version(&conn)?;

        if version == 0 {
            return Ok(None);
        }

        if version < schema::SCHEMA_VERSION {
            return Err(StoreError::Backend(
                "the database is from an older server and has not been upgraded yet".to_owned(),
            ));
        }

        Ok(Some(Self {
            conn,
            newer: version > schema::SCHEMA_VERSION,
        }))
    }

    /// An in-memory database, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        schema::migrate(&mut conn)?;

        Ok(Self { conn, newer: false })
    }

    /// True when the database was written by a newer server.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// The schema version of the file.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(schema::user_version(&self.conn)?)
    }

    fn connect(path: &Path, flags: OpenFlags) -> Result<Self, StoreError> {
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;

        // Switching to WAL and the first migration can report BUSY without waiting while another
        // process opens the same new file, so they are retried until the busy timeout.
        let deadline = Instant::now() + BUSY_TIMEOUT;
        let state = loop {
            match initialize(&mut conn) {
                Err(error) if is_busy(&error) && Instant::now() < deadline => thread::sleep(BUSY_RETRY),
                other => break other?,
            }
        };

        Ok(Self {
            conn,
            newer: state == SchemaState::Newer,
        })
    }

    fn conn(&self) -> Result<&Connection, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        Ok(&self.conn)
    }

    /// Runs `op` in a write transaction that takes the write lock first, committed when `op`
    /// succeeds and rolled back when it fails. The generation grows with every committed write.
    fn write<T>(&mut self, op: impl FnOnce(&rusqlite::Transaction) -> Result<T, StoreError>) -> Result<T, StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let done = op(&tx)?;
        meta::bump(&tx)?;
        tx.commit()?;

        Ok(done)
    }
}

impl KnowledgeStore for SqliteStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        meta::get(self.conn()?, key)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        meta::check_settable(key)?;
        self.write(|tx| meta::set(tx, key, value))
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        meta::generation(self.conn()?)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[super::Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let title = clean::title(&doc.title)?;
        let links = clean::links(links)?;
        clean::chunks(chunks)?;

        self.write(|tx| documents::put(tx, doc, &title, chunks, &links))
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        documents::find(self.conn()?, "content_sha256", sha)
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        documents::find(self.conn()?, "source_key", key)
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        documents::reusable_embeddings(self.conn()?, model_id, chunk_shas)
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
        listing::list(self.conn()?, query)
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        listing::get(self.conn()?, id, detail)
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        search::keyword_top_k(self.conn()?, query, scope, k)
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        search::vector_top_k(self.conn()?, vector, scope, k)
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        listing::repos(self.conn()?)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        listing::stats(self.conn()?)
    }
}

/// Switches to WAL and migrates.
fn initialize(conn: &mut Connection) -> rusqlite::Result<SchemaState> {
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    schema::migrate(conn)
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// True when `error` is SQLite saying it was built without FTS5.
pub(super) fn is_missing_fts5(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("no such module: fts5"))
}

/// Whether this SQLite build has FTS5: the keyword index cannot exist without it.
pub fn fts5_available() -> bool {
    let probe = || -> rusqlite::Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE VIRTUAL TABLE probe USING fts5(text)")
    };

    probe().is_ok()
}

/// Whether the file exists (a folder or a missing file does not count as a database).
fn exists(path: &Path) -> Result<bool, StoreError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StoreError::Backend(format!(
            "cannot read the database: {}",
            error.kind()
        ))),
    }
}

/// Creates the parent folder (0700) and the file (0600) before SQLite opens it.
fn prepare_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);

        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

        builder.create(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    options.open(path).map(drop)
}

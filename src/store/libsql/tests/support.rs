//! The server under test: a lock, an emptied database and raw access to it.

use std::fs::File;

use super::super::hrana::Client;
use super::super::sql::{Sql, Value, args, query_row};
use super::super::{LibsqlStore, begin, endpoint};
use crate::store::KnowledgeStore;
use crate::store::testing::Seed;

pub const URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_LIBSQL_URL";
pub const TOKEN_VAR: &str = "HARNESS_KNOWLEDGE_TEST_LIBSQL_TOKEN";

/// Everything the schema creates, dropped before each case (the FTS table takes its shadow tables
/// and the triggers with it, the vector index its own shadow table).
const RESET: &str = "
PRAGMA foreign_keys = OFF;
DROP TABLE IF EXISTS knowledge_chunks_fts;
DROP INDEX IF EXISTS knowledge_chunks_vec;
DROP TABLE IF EXISTS knowledge_chunks;
DROP TABLE IF EXISTS knowledge_doc_links;
DROP TABLE IF EXISTS knowledge_documents;
DROP TABLE IF EXISTS knowledge_meta;
";

/// The libSQL server under test, held exclusively while this lives.
pub struct Server {
    pub url: String,
    pub token: Option<String>,
    _lock: File,
}

/// The emptied server, or `None` (with a message) when no server is configured.
pub fn server() -> Option<Server> {
    let Some(url) = std::env::var(URL_VAR).ok().filter(|url| !url.trim().is_empty()) else {
        eprintln!("skipped: set {URL_VAR} (e.g. http://127.0.0.1:58080) to run this case on a libSQL server");
        return None;
    };

    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("harness-knowledge-libsql-tests.lock"))
        .expect("lock file");
    lock.lock().expect("lock");

    let token = std::env::var(TOKEN_VAR).ok().filter(|token| !token.is_empty());
    let server = Server {
        url,
        token,
        _lock: lock,
    };
    server.batch(RESET);

    Some(server)
}

impl Server {
    /// A store on the database (another one each call, like another server process).
    pub fn store(&self) -> LibsqlStore {
        LibsqlStore::connect(&self.url, self.token.as_deref()).expect("connect")
    }

    pub fn client(&self) -> Client {
        Client::new(endpoint::parse(&self.url).expect("url"), self.token.as_deref()).expect("client")
    }

    /// Runs `sql` (several statements) directly, bypassing the store.
    pub fn batch(&self, sql: &str) {
        self.client().single().batch(sql).expect("raw batch");
    }

    /// Runs one statement directly.
    pub fn execute(&self, sql: &str) -> Result<(), crate::store::StoreError> {
        self.client().single().execute(sql, args![]).map(drop)
    }

    /// The integer the query `sql` returns, read directly.
    pub fn integer(&self, sql: &str) -> i64 {
        query_row(&mut self.client().single(), sql, args![])
            .expect("raw query")
            .expect("a row")
            .get(0)
            .expect("an integer")
    }

    /// Runs `sql` once per row of `rows` in one transaction, bypassing the store: for seeding
    /// thousands of rows in a few requests.
    pub fn insert_many(&self, sql: &str, rows: &[Vec<Value>]) {
        let client = self.client();
        let mut stream = begin(&client).expect("begin");

        stream.execute_many(sql, rows).expect("insert");
        stream.commit().expect("commit");
    }
}

/// Inserts `seed` and returns the document id.
pub fn insert(store: &mut dyn KnowledgeStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

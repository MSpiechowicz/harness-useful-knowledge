//! A throwaway PostgreSQL schema per test, for the store suites.
//!
//! `HARNESS_KNOWLEDGE_TEST_POSTGRES_URL` names a server with pgvector the tests may create schemas
//! and databases in (for example `postgres://postgres:test@127.0.0.1:55433/postgres`). Each
//! [`database`] creates a schema with a unique name, points the store's URL at it
//! (`options=-csearch_path=...`) and drops it with everything in it when the [`Database`] goes.
//! pgvector is created once in `public`, so every schema reaches it the way a team's schemas do.
//! `HARNESS_KNOWLEDGE_TEST_POSTGRES_PLAIN_URL` names a server without pgvector (`postgres:17`)
//! for the negative tests. Without a variable each test prints that it is skipped and passes.
//!
//! The helper's own connection uses no TLS, so the URL has to reach a server that accepts that
//! (a local test server does).
#![allow(dead_code)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use postgres::{Client, NoTls};

use crate::backend::Access;
use crate::store::KnowledgeStore;
use crate::store::postgres::PostgresStore;
use crate::store::testing::Seed;

pub const URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_POSTGRES_URL";
pub const PLAIN_URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_POSTGRES_PLAIN_URL";

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Tests create pgvector one at a time: two `CREATE EXTENSION`s at once can collide.
static EXTENSION: Mutex<()> = Mutex::new(());

/// The URL in variable `name`, or `None` (after saying so) when it is unset.
pub fn url_of(name: &str) -> Option<String> {
    let url = std::env::var(name).ok().filter(|url| !url.trim().is_empty());

    if url.is_none() {
        eprintln!("skipped: set {name} to run the PostgreSQL store tests");
    }

    url
}

/// A name no other test uses.
pub fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);

    format!(
        "{prefix}_{}_{}_{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// `url` with the path replaced by `database`.
pub fn with_database(url: &str, database: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(base, query)| (base, Some(query)));
    let (scheme, rest) = base.split_once("://").expect("a URL with a scheme");
    let authority = rest.split('/').next().expect("an authority");
    let query = query.map(|query| format!("?{query}")).unwrap_or_default();

    format!("{scheme}://{authority}/{database}{query}")
}

/// `url` with the user and password replaced.
pub fn with_login(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL with a scheme");
    let after = rest.rsplit_once('@').map_or(rest, |(_, after)| after);

    format!("{scheme}://{user}:{password}@{after}")
}

/// A client on `url` for statements below the store.
pub fn client(url: &str) -> Client {
    Client::connect(url, NoTls).expect("the test server accepts the connection")
}

/// One test's schema, dropped (with its tables) on drop.
pub struct Database {
    admin_url: String,
    schema: String,
    url: String,
}

/// A fresh schema on the pgvector server, or `None` (after saying so) when [`URL_VAR`] is unset.
pub fn database() -> Option<Database> {
    database_on(URL_VAR, true)
}

/// A fresh schema on the server without pgvector, or `None` when [`PLAIN_URL_VAR`] is unset.
pub fn plain_database() -> Option<Database> {
    database_on(PLAIN_URL_VAR, false)
}

fn database_on(variable: &str, with_vector: bool) -> Option<Database> {
    let admin_url = url_of(variable)?;
    let schema = unique("hk_test");
    let mut admin = client(&admin_url);

    if with_vector {
        let _guard = EXTENSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        admin
            .batch_execute("CREATE EXTENSION IF NOT EXISTS vector SCHEMA public")
            .expect("create pgvector in public");
    }

    admin
        .batch_execute(&format!("CREATE SCHEMA \"{schema}\""))
        .expect("create the test schema");

    let url = options(&admin_url, &format!("-csearch_path={schema}"));

    Some(Database { admin_url, schema, url })
}

/// `url` with the libpq `options` parameter set to `options`.
fn options(url: &str, options: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let encoded = options.replace('=', "%3D").replace(' ', "%20");

    format!("{url}{separator}options={encoded}")
}

impl Database {
    /// A store on this schema, migrating it on first use.
    pub fn open(&self) -> PostgresStore {
        open(&self.url)
    }

    /// The store URL of this schema.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The store URL of this schema with more session options (`-cenable_seqscan=off`).
    pub fn url_with_options(&self, more: &str) -> String {
        options(&self.admin_url, &format!("-csearch_path={} {more}", self.schema))
    }

    /// The store URL with more query parameters (`key=value&...`).
    pub fn url_with(&self, params: &str) -> String {
        format!("{}&{params}", self.url)
    }

    /// A plain client whose `search_path` is this schema, for checks below the store.
    pub fn admin(&self) -> Client {
        let mut client = client(&self.admin_url);
        client
            .batch_execute(&format!("SET search_path = \"{}\", public", self.schema))
            .expect("search path");
        client
    }

    /// The name of the schema.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The URL of the server the schema is on, without the schema.
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    /// The tables of the schema.
    pub fn tables(&self) -> Vec<String> {
        self.admin()
            .query(
                "SELECT table_name::text FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1",
                &[&self.schema],
            )
            .expect("tables")
            .iter()
            .map(|row| row.get(0))
            .collect()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let mut admin = client(&self.admin_url);
        let _ = admin.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema));
    }
}

/// The store on `url`, created when the database has none.
pub fn open(url: &str) -> PostgresStore {
    PostgresStore::open(url, Access::Create)
        .unwrap_or_else(|fault| panic!("open the test database: {:?}", crate::store::StoreError::from(fault)))
        .expect("a created database opens")
}

/// Inserts `seed` and returns the document id.
pub fn insert(store: &mut PostgresStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

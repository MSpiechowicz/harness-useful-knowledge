//! Behaviour of the MongoDB store through the [`KnowledgeStore`](crate::store::KnowledgeStore)
//! trait, on a real server with a search engine.
//!
//! The `team_*` tests need one and skip themselves (with a message) without it. They read the URL
//! from [`URL_VAR`], and every test gets a database of its own on that server, dropped when the
//! test ends. A local Atlas Local (mongod and mongot, a replica set):
//!
//! ```text
//! docker run -d --name hk-mongo-atlas -p 127.0.0.1:27017:27017 mongodb/mongodb-atlas-local:latest
//! HARNESS_KNOWLEDGE_TEST_MONGO_URL='mongodb://localhost:27017/?directConnection=true' cargo test team_mongo
//! ```
//!
//! The negative tests ask a server without `mongot` (`mongo:8` community) from [`PLAIN_URL_VAR`].
//!
//! The search indexes are eventually consistent, so a test that searches what it just wrote waits
//! with [`eventually`] (polling, at most [`WAIT`]) and never sleeps blindly.

mod capability;
mod documents;
mod embeddings;
mod listing;
mod ordered;
mod phrase;
mod search;
mod titles;
mod upgrade;
mod url;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mongodb::bson::{Document, doc};
use mongodb::sync::{Client, Database};

use super::MongoStore;
use super::schema::{CHUNKS, IndexState};
use crate::store::testing::Seed;
use crate::store::{Detail, KnowledgeStore, Section, StoreError};

/// The variable naming the test server (with a search engine).
pub(super) const URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_MONGO_URL";

/// The variable naming a server without a search engine, for the negative tests.
pub(super) const PLAIN_URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_MONGO_PLAIN_URL";

/// The longest a test waits for the search engine to catch up.
pub(super) const WAIT: Duration = Duration::from_secs(30);

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A database of its own on the test server, dropped on drop.
pub(super) struct TestDb {
    /// The URL of the database (the test server's URL with its name as the path).
    pub url: String,
    pub name: String,
    client: Client,
}

impl TestDb {
    /// A new, empty database on the server `var` names, or `None` (with a message) when unset.
    pub fn on(var: &str) -> Option<Self> {
        let Ok(base) = std::env::var(var) else {
            eprintln!("skipped: set {var} to a MongoDB URL to run the MongoDB store tests");
            return None;
        };

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "hkt_{}_{}_{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );

        let client = Client::with_uri_str(&base).expect("test server client");

        Some(Self {
            url: with_database(&base, &name),
            name,
            client,
        })
    }

    /// A new, empty database on the test server with a search engine.
    pub fn new() -> Option<Self> {
        Self::on(URL_VAR)
    }

    /// The database itself, for checks behind the store's back.
    pub fn raw(&self) -> Database {
        self.client.database(&self.name)
    }

    /// True when the server has a database of this name (it has once something was written).
    pub fn exists(&self) -> bool {
        self.client
            .list_database_names()
            .run()
            .expect("list databases")
            .contains(&self.name)
    }

    /// The documents of `collection` matching `filter`.
    pub fn count(&self, collection: &str, filter: Document) -> u64 {
        self.raw()
            .collection::<Document>(collection)
            .count_documents(filter)
            .run()
            .expect("count")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = self.raw().drop().run();
    }
}

/// `base` with its path set to `/name` (the options kept).
fn with_database(base: &str, name: &str) -> String {
    let (scheme, rest) = base.split_once("://").expect("a URL with a scheme");
    let (address, query) = match rest.split_once('?') {
        Some((address, query)) => (address, Some(query)),
        None => (rest, None),
    };
    let authority = address.split('/').next().unwrap_or(address);

    let mut url = format!("{scheme}://{authority}/{name}");

    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }

    url
}

/// A fresh database with a store on it whose search indexes answer queries, or `None` (with a
/// message) when no test server is configured.
pub(super) fn store() -> Option<(TestDb, MongoStore)> {
    let db = TestDb::new()?;
    let store = MongoStore::connect(&db.url).expect("connect to the test database");

    let (keyword, vector) = store.wait_for_indexes(WAIT).expect("wait for the indexes");
    assert_eq!(
        (keyword, vector),
        (IndexState::Ready, IndexState::Ready),
        "indexes ready"
    );

    Some((db, store))
}

/// Inserts `seed` and returns the document id.
pub(super) fn insert(store: &mut MongoStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

/// Calls `probe` until it returns `Some` (a result that holds), polling every 200 ms for at most
/// [`WAIT`]; fails with `what` and the last answer when it never does. For the answers of the
/// search indexes, which catch up with a write about a second later.
pub(super) fn eventually<T: std::fmt::Debug>(
    what: &str,
    mut probe: impl FnMut() -> Result<T, StoreError>,
    holds: impl Fn(&T) -> bool,
) -> T {
    let deadline = Instant::now() + WAIT;

    loop {
        let answer = probe().unwrap_or_else(|error| panic!("{what}: {error}"));

        if holds(&answer) {
            return answer;
        }

        assert!(Instant::now() < deadline, "{what}: still {answer:?} after {WAIT:?}");
        thread::sleep(Duration::from_millis(200));
    }
}

/// The outline of document `id`.
pub(super) fn sections(store: &mut MongoStore, id: i64) -> Vec<Section> {
    let detail = Detail {
        sections: true,
        chunk: None,
    };

    store
        .get(id, &detail)
        .expect("get")
        .expect("document")
        .sections
        .expect("sections")
}

/// How many chunks the database holds, behind the store's back.
pub(super) fn chunk_count(db: &TestDb) -> u64 {
    db.count(CHUNKS, doc! {})
}

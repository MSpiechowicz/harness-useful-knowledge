use super::support::{client, database, insert, open, unique};
use crate::backend::Access;
use crate::store::postgres::schema::SCHEMA_VERSION;
use crate::store::postgres::{Fault, PostgresStore};
use crate::store::testing::Seed;
use crate::store::{Detail, KnowledgeStore, ListQuery, Scope, StoreError};

fn open_as(url: &str, access: Access) -> Option<PostgresStore> {
    PostgresStore::open(url, access).unwrap_or_else(|fault| panic!("open: {:?}", StoreError::from(fault)))
}

#[test]
fn team_opening_creates_the_schema_and_reopening_keeps_the_data() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let mut again = db.open();
    assert_eq!(
        again
            .get(id, &Detail::default())
            .expect("get")
            .expect("document")
            .doc
            .title,
        "Guide"
    );
    assert_eq!(
        again.meta("schema_version").expect("meta").as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );
    assert_eq!(again.meta("dims").expect("meta").as_deref(), Some("384"));
    assert_eq!(again.generation().expect("generation"), 1);
    assert!(again.pgvector().is_some_and(|version| version.has_hnsw()));
}

#[test]
fn team_reading_and_changing_never_create_the_schema_and_creating_does() {
    let Some(db) = database() else { return };

    assert!(open_as(db.url(), Access::Read).is_none());
    assert!(open_as(db.url(), Access::Existing).is_none());
    assert!(db.tables().is_empty(), "nothing was created: {:?}", db.tables());

    assert!(open_as(db.url(), Access::Create).is_some());
    assert_eq!(
        db.tables(),
        [
            "knowledge_chunks",
            "knowledge_doc_comments",
            "knowledge_doc_events",
            "knowledge_doc_files",
            "knowledge_doc_links",
            "knowledge_doc_snapshots",
            "knowledge_documents",
            "knowledge_meta"
        ]
    );

    let mut read = open_as(db.url(), Access::Read).expect("a read opens what exists");
    assert_eq!(read.stats().expect("stats").documents, 0);
    assert!(read.list(&super::listing::everything()).expect("list").items.is_empty());
    assert!(open_as(db.url(), Access::Existing).is_some());
}

#[test]
fn team_every_object_of_the_schema_is_prefixed() {
    let Some(db) = database() else { return };
    drop(db.open());

    let names: Vec<String> = db
        .admin()
        .query(
            "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 ORDER BY 1",
            &[&db.schema()],
        )
        .expect("objects")
        .iter()
        .map(|row| row.get(0))
        .collect();

    assert!(names.len() > 8, "tables, indexes and sequences: {names:?}");
    assert!(names.iter().all(|name| name.starts_with("knowledge_")), "{names:?}");
    assert!(
        names.iter().any(|name| name == "knowledge_chunks_embedding"),
        "the HNSW index"
    );
    assert!(names.iter().any(|name| name == "knowledge_chunks_tsv"), "the GIN index");
}

#[test]
fn team_a_newer_database_is_left_alone() {
    let Some(db) = database() else { return };
    let mut first = db.open();
    insert(&mut first, &Seed::new("Guide").chunk("A", "x"));

    // A newer server migrated while this one was running: its next write notices.
    db.admin()
        .batch_execute("UPDATE knowledge_meta SET value = '99' WHERE key = 'schema_version'")
        .expect("version");

    let (doc, chunks, links) = Seed::new("Second").chunk("A", "y").parts();
    assert!(matches!(
        first.put_document(&doc, &chunks, &links),
        Err(StoreError::NewerSchema)
    ));
    assert!(
        matches!(first.stats(), Err(StoreError::NewerSchema)),
        "and every read after it"
    );

    for access in [Access::Read, Access::Existing, Access::Create] {
        let mut newer = open_as(db.url(), access).expect("it opens");
        assert!(newer.is_newer());
        assert!(matches!(
            newer.list(&ListQuery {
                limit: 5,
                ..super::listing::everything()
            }),
            Err(StoreError::NewerSchema)
        ));
        assert!(matches!(
            newer.keyword_top_k("x", &Scope::everything(), 5),
            Err(StoreError::NewerSchema)
        ));
    }

    let version: String = db
        .admin()
        .query_one("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .expect("version")
        .get(0);
    assert_eq!(version, "99", "nothing downgraded it");
}

#[test]
fn team_an_older_database_is_not_read_before_it_is_upgraded() {
    let Some(db) = database() else { return };
    drop(db.open());

    db.admin()
        .batch_execute("UPDATE knowledge_meta SET value = '0' WHERE key = 'schema_version'")
        .expect("version");

    match PostgresStore::open(db.url(), Access::Read) {
        Err(Fault::Store(StoreError::Backend(message))) => assert!(message.contains("older server"), "{message}"),
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[test]
fn team_a_dropped_connection_is_opened_again_for_reads_and_writes() {
    let Some(db) = database() else { return };
    let name = unique("hk-reconnect");
    let mut store = open(&db.url_with(&format!("application_name={name}")));
    insert(&mut store, &Seed::new("First").chunk("A", "one"));

    let terminate = || {
        client(db.admin_url())
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1",
                &[&name],
            )
            .expect("terminate");
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    terminate();
    assert_eq!(store.stats().expect("a read after the drop").documents, 1);

    terminate();
    insert(&mut store, &Seed::new("Second").chunk("A", "two"));
    assert_eq!(store.stats().expect("stats").documents, 2);
}

#[test]
fn team_servers_starting_together_migrate_once() {
    let Some(db) = database() else { return };
    let url = db.url().to_owned();

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || open(&url).schema_version().expect("version"))
        })
        .collect();

    for thread in threads {
        assert_eq!(thread.join().expect("thread"), SCHEMA_VERSION);
    }

    let rows: i64 = db
        .admin()
        .query_one("SELECT count(*) FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .expect("count")
        .get(0);
    assert_eq!(rows, 1);
    assert_eq!(db.tables().len(), 8);
}

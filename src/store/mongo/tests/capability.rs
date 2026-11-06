//! Opening the database, what a read creates, and what `check` learns from a server.

use std::time::{Duration, Instant};

use mongodb::bson::doc;

use super::{PLAIN_URL_VAR, TestDb, URL_VAR, WAIT, insert, store};
use crate::backend::{Access, OpenError};
use crate::config::Secrets;
use crate::store::mongo::probe::probe;
use crate::store::mongo::schema::{CHUNKS, IndexState, KEYWORD_INDEX, META, SCHEMA_VERSION, VECTOR_INDEX};
use crate::store::mongo::{MongoStore, SERVER_SELECTION_TIMEOUT, open_boxed};
use crate::store::testing::{MODEL, Seed};
use crate::store::{KnowledgeStore, ListQuery, RepoFilter, Sort, StoreError};

fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        author: None,
        want_authors: false,
        sort: Sort::Updated,
        descending: true,
        offset: 0,
        limit: 50,
        status: crate::store::StatusFilter::All,
    }
}

#[test]
fn team_mongo_opening_creates_the_schema_and_the_indexes_and_reopening_keeps_the_data() {
    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let mut again = MongoStore::connect(&db.url).expect("reopen");
    assert_eq!(
        again
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .doc
            .title,
        "Guide"
    );
    assert_eq!(again.meta("schema_version").expect("meta").as_deref(), Some("2"));
    assert_eq!(again.meta("dims").expect("meta").as_deref(), Some("384"));
    assert_eq!(again.database_name(), db.name);
    assert!(again.uses_transactions(), "a replica set runs writes in transactions");

    let mut regular = db
        .raw()
        .collection::<mongodb::bson::Document>(CHUNKS)
        .list_index_names()
        .run()
        .expect("indexes");
    regular.sort();
    assert_eq!(
        regular,
        [
            "_id_",
            "chunks_position",
            "chunks_sha",
            "chunks_terms",
            "chunks_title_terms"
        ]
    );

    let mut collections = db.raw().list_collection_names().run().expect("collections");
    collections.sort();
    assert_eq!(
        collections,
        [
            "knowledge_chunks",
            "knowledge_comments",
            "knowledge_documents",
            "knowledge_events",
            "knowledge_files",
            "knowledge_meta",
            "knowledge_snapshots",
        ]
    );
    assert!(collections.iter().all(|name| name.starts_with("knowledge_")));
}

#[test]
fn team_mongo_the_database_is_the_url_path_and_the_default_is_harness_knowledge() {
    let Some(db) = TestDb::new() else { return };

    let store = MongoStore::connect(&db.url).expect("connect");
    assert_eq!(store.database_name(), db.name);
    assert!(db.exists());
}

#[test]
fn team_mongo_reading_and_changing_never_create_the_database_and_creating_does() {
    let Some(db) = TestDb::new() else { return };

    assert!(MongoStore::connect_read_only(&db.url).expect("read").is_none());
    assert!(MongoStore::connect_existing(&db.url).expect("existing").is_none());
    assert!(!db.exists(), "nothing was created");

    MongoStore::connect(&db.url).expect("create");
    assert!(db.exists());

    assert!(MongoStore::connect_read_only(&db.url).expect("read").is_some());
    assert!(MongoStore::connect_existing(&db.url).expect("existing").is_some());
}

#[test]
fn team_mongo_a_read_only_store_reads_and_cannot_write() {
    let Some((db, mut writer)) = store() else { return };
    insert(&mut writer, &Seed::new("Guide").chunk("Intro", "hello"));

    let mut reader = MongoStore::connect_read_only(&db.url).expect("open").expect("exists");
    assert_eq!(reader.stats().expect("stats").documents, 1);
    assert_eq!(reader.list(&everything()).expect("list").total, 1);

    let (doc, chunks, links) = Seed::new("Other").chunk("A", "b").parts();
    assert!(matches!(
        reader.put_document(&doc, &chunks, &links),
        Err(StoreError::Backend(_))
    ));
    assert!(matches!(
        reader.set_meta("model_id", MODEL),
        Err(StoreError::Backend(_))
    ));
    assert!(matches!(reader.delete(&[1]), Err(StoreError::Backend(_))));
    assert_eq!(reader.stats().expect("stats").documents, 1);
}

#[test]
fn team_mongo_a_database_from_a_newer_server_is_left_alone() {
    let Some((db, store)) = store() else { return };
    drop(store);

    db.raw()
        .collection::<mongodb::bson::Document>(META)
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": SCHEMA_VERSION + 1 } },
        )
        .run()
        .expect("bump the version");

    let mut newer = MongoStore::connect(&db.url).expect("opens");
    assert!(newer.is_newer());
    assert!(matches!(newer.stats(), Err(StoreError::NewerSchema)));
    assert!(matches!(newer.delete(&[1]), Err(StoreError::NewerSchema)));

    let mut read = MongoStore::connect_read_only(&db.url).expect("open").expect("exists");
    assert!(matches!(read.list(&everything()), Err(StoreError::NewerSchema)));
}

#[test]
fn team_mongo_a_dropped_search_index_comes_back_with_the_next_writer() {
    let Some((db, store)) = store() else { return };
    drop(store);

    db.raw()
        .run_command(doc! { "dropSearchIndex": CHUNKS, "name": KEYWORD_INDEX })
        .run()
        .expect("drop the index");

    let again = MongoStore::connect(&db.url).expect("reconnect");
    let (keyword, vector) = again.wait_for_indexes(WAIT).expect("wait");
    assert_eq!((keyword, vector), (IndexState::Ready, IndexState::Ready));
}

#[test]
fn team_mongo_a_search_before_the_index_answers_says_so_instead_of_returning_nothing() {
    let Some((db, store)) = store() else { return };
    drop(store);

    db.raw()
        .run_command(doc! { "dropSearchIndex": CHUNKS, "name": VECTOR_INDEX })
        .run()
        .expect("drop the index");

    // Opened for reading, so the missing index is not created again.
    let mut reader = MongoStore::connect_read_only(&db.url).expect("open").expect("exists");
    let error = reader
        .vector_top_k(&crate::store::testing::axis(0), &crate::store::Scope::everything(), 5)
        .expect_err("no index");

    assert!(error.to_string().contains("search indexes"), "{error}");
    assert!(!error.to_string().contains(';'), "{error}");
}

#[test]
fn team_mongo_check_of_a_database_nobody_wrote_to_is_ready_and_creates_nothing() {
    let Some(db) = TestDb::new() else { return };

    let found = probe(&db.url).expect("probe");
    assert_eq!(found.schema, None);
    assert!(found.ready());
    assert_eq!(found.problem(), None);
    assert_eq!(found.vector_index(), "ready");
    assert!(!db.exists(), "a check changes nothing");
}

#[test]
fn team_mongo_check_of_a_database_in_use_reports_its_schema_model_and_indexes() {
    let Some((db, mut store)) = store() else { return };
    store.set_meta("model_id", MODEL).expect("model");

    let found = probe(&db.url).expect("probe");
    assert_eq!(found.schema, Some(SCHEMA_VERSION));
    assert_eq!(found.model_id.as_deref(), Some(MODEL));
    assert_eq!((found.keyword, found.vector), (IndexState::Ready, IndexState::Ready));
    assert_eq!(found.problem(), None);
}

#[test]
fn team_mongo_check_reports_index_building_while_an_index_is_missing_or_pending() {
    let Some(db) = TestDb::new() else { return };
    MongoStore::connect(&db.url).expect("create");

    // Right after the indexes were asked for they are either still pending or already ready, and
    // they are ready within the wait.
    let deadline = Instant::now() + WAIT;

    loop {
        let found = probe(&db.url).expect("probe");

        match found.problem() {
            None => break,
            Some(problem) => {
                assert_eq!(problem.code, "index-building");
                assert!(Instant::now() < deadline, "the indexes never became ready");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // A dropped index is missing, which is also reported as index-building.
    db.raw()
        .run_command(doc! { "dropSearchIndex": CHUNKS, "name": VECTOR_INDEX })
        .run()
        .expect("drop the index");

    let deadline = Instant::now() + WAIT;

    let missing = loop {
        let found = probe(&db.url).expect("probe");

        if found.vector == IndexState::Missing || Instant::now() >= deadline {
            break found;
        }

        std::thread::sleep(Duration::from_millis(200));
    };

    assert_eq!(missing.vector, IndexState::Missing);
    assert_eq!(missing.vector_index(), "missing");
    let problem = missing.problem().expect("problem");
    assert_eq!(problem.code, "index-building");
    assert!(problem.message.contains("missing"), "{}", problem.message);
}

#[test]
fn team_mongo_check_with_the_wrong_password_is_auth_and_never_shows_it() {
    let Ok(base) = std::env::var(URL_VAR) else {
        eprintln!("skipped: set {URL_VAR} to a MongoDB URL to run the MongoDB store tests");
        return;
    };

    let url = base.replacen("://", "://nobody:hunter2-not-the-password@", 1);
    let problem = probe(&url).expect_err("refused");

    assert_eq!(problem.code, "auth");
    assert!(!problem.message.contains("hunter2"), "{}", problem.message);

    let error = MongoStore::connect(&url).expect_err("refused");
    assert!(!error.to_string().contains("hunter2"), "{error}");
}

#[test]
fn a_server_that_does_not_answer_is_unreachable() {
    let url = "mongodb://127.0.0.1:1/?directConnection=true&serverSelectionTimeoutMS=1500";

    let problem = probe(url).expect_err("nothing listens");
    assert_eq!(problem.code, "unreachable");
    assert!(!problem.message.is_empty());
    assert!(!problem.message.contains(';'), "{}", problem.message);

    assert!(matches!(MongoStore::connect(url), Err(StoreError::Unreachable(_))));
}

#[test]
fn a_remote_url_without_tls_is_refused_before_any_connection() {
    let problem = probe("mongodb://db.example.com/?tls=false").expect_err("refused");

    assert_eq!(problem.code, "unreachable");
    assert!(problem.message.contains("TLS is required"), "{}", problem.message);
}

#[test]
fn team_mongo_a_server_without_mongot_is_no_mongot_and_creates_nothing() {
    let Some(db) = TestDb::on(PLAIN_URL_VAR) else { return };

    let problem = probe(&db.url).expect_err("no search engine");
    assert_eq!(problem.code, "no-mongot");
    assert!(problem.message.contains("mongot"), "{}", problem.message);
    assert!(!problem.message.contains(';'), "{}", problem.message);

    let error = MongoStore::connect(&db.url).expect_err("no search engine");
    assert!(matches!(error, StoreError::Backend(_)));
    assert!(error.to_string().contains("mongot"), "{error}");

    assert!(MongoStore::connect_read_only(&db.url).expect("read").is_none());
    assert!(MongoStore::connect_existing(&db.url).expect("existing").is_none());
    assert!(!db.exists(), "the refused open created no collection and no index");
}

#[test]
fn team_mongo_opening_a_server_without_mongot_is_a_missing_capability() {
    let Some(db) = TestDb::on(PLAIN_URL_VAR) else { return };
    let secrets = Secrets::of(&db.url, None);

    match open_boxed(&db.url, Access::Create, SERVER_SELECTION_TIMEOUT, &secrets) {
        Err(OpenError::Missing { code, message }) => {
            assert_eq!(code, "no-mongot");
            assert!(message.contains("mongot"), "{message}");
        }
        other => panic!("expected a missing capability: {:?}", other.map(|_| ())),
    }

    // Nothing was written, so a read or a change finds no database.
    for access in [Access::Read, Access::Existing] {
        assert!(
            open_boxed(&db.url, access, SERVER_SELECTION_TIMEOUT, &secrets)
                .expect("open")
                .is_none()
        );
    }

    assert!(!db.exists(), "the refused open created nothing");
}

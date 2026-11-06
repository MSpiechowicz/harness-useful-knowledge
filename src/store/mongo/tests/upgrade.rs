//! The machinery of a schema upgrade: a field filled in pages of 500, recorded version by version,
//! and run again after a stop (a test upgrade after the real ones runs through
//! [`schema::migrate_with`]), and the upgrade from version 1 to version 2 (drafts and their review).

use std::time::Duration;

use mongodb::bson::{Bson, Document, doc};
use mongodb::sync::Database;

use super::{TestDb, WAIT, eventually, insert, store};
use crate::backend::{Access, Upgrade};
use crate::store::mongo::migrate::{PAGE, update_in_pages};
use crate::store::mongo::schema::{
    self, CHUNKS, DOCUMENTS, IndexState, KEYWORD_INDEX, LEGACY_KEYWORD_INDEX, LEGACY_VECTOR_INDEX, META, UPGRADES,
    VECTOR_INDEX,
};
use crate::store::mongo::{MongoStore, SERVER_SELECTION_TIMEOUT, open_boxed_with};
use crate::store::testing::{MODEL, Seed, axis};
use crate::store::{Detail, KnowledgeStore, ListQuery, RepoFilter, Scope, Sort, Status, StatusFilter, StoreError};

/// Gives every chunk that has none a `marker`, one page of updates at a time.
fn mark_chunks(db: &Database) -> mongodb::error::Result<()> {
    let chunks = db.collection::<Document>(CHUNKS);
    let mut updates = Vec::new();

    for row in chunks.find(doc! { "marker": { "$exists": false } }).run()? {
        let row = row?;
        let id = row.get_i64("_id").unwrap_or_default();
        let text = row.get_str("text").unwrap_or_default().len() as i64;

        updates.push((doc! { "_id": id }, doc! { "$set": { "marker": text } }));
    }

    update_in_pages(db, CHUNKS, updates)
}

/// The upgrades of this server, then `more` that a later server would have.
fn after_the_real_ones(more: &[schema::Upgrade]) -> Vec<schema::Upgrade> {
    UPGRADES.iter().chain(more).copied().collect()
}

fn version(db: &Database) -> i64 {
    db.collection::<Document>(META)
        .find_one(doc! { "_id": "schema" })
        .run()
        .expect("find")
        .expect("schema")
        .get_i64("version")
        .expect("version")
}

#[test]
fn team_mongo_an_upgrade_fills_a_field_in_pages_and_records_the_version_it_reached() {
    let Some((db, mut store)) = store() else { return };

    // Three pages and a part of one.
    let chunks = PAGE * 2 + 120;
    let mut seed = Seed::new("Big");

    for n in 0..chunks {
        seed = seed.chunk(&format!("Part {n}"), "text of the part");
    }

    insert(&mut store, &seed);
    drop(store);

    let raw = db.raw();
    assert_eq!(db.count(CHUNKS, doc! { "marker": { "$exists": true } }), 0);
    assert_eq!(version(&raw), schema::SCHEMA_VERSION);

    let newer = schema::migrate_with(&raw, &after_the_real_ones(&[mark_chunks])).expect("the upgrade runs");

    assert!(!newer);
    assert_eq!(
        db.count(CHUNKS, doc! { "marker": { "$exists": true } }),
        chunks as u64,
        "every chunk has it"
    );
    assert_eq!(version(&raw), 3, "the version it reached is recorded");

    // Run again, it finds nothing to do.
    assert!(!schema::migrate_with(&raw, &after_the_real_ones(&[mark_chunks])).expect("again"));
    assert_eq!(version(&raw), 3);
}

#[test]
fn team_mongo_a_database_that_a_later_server_upgraded_is_left_alone() {
    let Some((db, store)) = store() else { return };
    drop(store);
    let raw = db.raw();

    // The later server knows two upgrades after the real ones.
    assert!(!schema::migrate_with(&raw, &after_the_real_ones(&[mark_chunks, mark_chunks])).expect("upgrades"));
    assert_eq!(version(&raw), 4);

    // This one knows one, so what it finds is newer, and it changes nothing.
    assert!(schema::migrate_with(&raw, &after_the_real_ones(&[mark_chunks])).expect("looks"));
    assert_eq!(version(&raw), 4);
}

// The upgrade from version 1 to version 2 (drafts and their review).

/// The listing of every row that is not a pending version, as the person sees it.
fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        author: None,
        want_authors: false,
        status: StatusFilter::All,
        sort: Sort::Updated,
        descending: false,
        offset: 0,
        limit: 50,
    }
}

/// The definition of a version 1 keyword index: the current one without the review state.
fn legacy_keyword_definition() -> Document {
    let mut definition = schema::keyword_definition();

    definition
        .get_document_mut("mappings")
        .and_then(|mappings| mappings.get_document_mut("fields"))
        .expect("fields")
        .remove("review");

    definition
}

/// The definition of a version 1 vector index: the current one without the review filter.
fn legacy_vector_definition() -> Document {
    let mut definition = schema::vector_definition();

    definition
        .get_array_mut("fields")
        .expect("fields")
        .retain(|field| field.as_document().and_then(|field| field.get_str("path").ok()) != Some("review"));

    definition
}

/// The state of the search index `name`.
fn index_state(db: &Database, name: &str) -> IndexState {
    schema::state_of(&schema::search_states(db).expect("search indexes"), name)
}

/// Waits until the search index `name` is `wanted`.
fn index_becomes(db: &Database, name: &str, wanted: IndexState) {
    eventually(
        &format!("the index {name}"),
        || Ok(index_state(db, name)),
        |state| *state == wanted,
    );
}

/// Turns the database of a store that was just created into what version 1 left behind: no field,
/// index or collection of version 2, and the two version 1 search indexes in place of the new ones.
fn downgrade_to_version_one(db: &TestDb) {
    let raw = db.raw();

    raw.collection::<Document>(DOCUMENTS)
        .update_many(
            doc! {},
            doc! { "$unset": { "status": "", "review_rev": "", "pending_of": "" } },
        )
        .run()
        .expect("unset the document fields");
    raw.collection::<Document>(CHUNKS)
        .update_many(doc! {}, doc! { "$unset": { "review": "" } })
        .run()
        .expect("unset the chunk field");

    for index in ["documents_status", "documents_pending"] {
        raw.collection::<Document>(DOCUMENTS)
            .drop_index(index)
            .run()
            .expect("drop the index");
    }

    for collection in [
        "knowledge_events",
        "knowledge_comments",
        "knowledge_files",
        "knowledge_snapshots",
    ] {
        raw.collection::<Document>(collection)
            .drop()
            .run()
            .expect("drop the collection");
    }

    raw.collection::<Document>(META)
        .update_one(doc! { "_id": "schema" }, doc! { "$set": { "version": 1_i64 } })
        .run()
        .expect("version 1");

    for index in [KEYWORD_INDEX, VECTOR_INDEX] {
        raw.run_command(doc! { "dropSearchIndex": CHUNKS, "name": index })
            .run()
            .expect("drop a search index");
        index_becomes(&raw, index, IndexState::Missing);
    }

    raw.run_command(doc! {
        "createSearchIndexes": CHUNKS,
        "indexes": [
            { "name": LEGACY_KEYWORD_INDEX, "type": "search", "definition": legacy_keyword_definition() },
            { "name": LEGACY_VECTOR_INDEX, "type": "vectorSearch", "definition": legacy_vector_definition() },
        ],
    })
    .run()
    .expect("create the version 1 indexes");

    for index in [LEGACY_KEYWORD_INDEX, LEGACY_VECTOR_INDEX] {
        index_becomes(&raw, index, IndexState::Ready);
    }
}

#[test]
fn team_mongo_a_database_of_version_one_upgrades_with_every_document_live_and_the_new_search_indexes() {
    let Some((db, mut store)) = store() else { return };
    let linked = insert(
        &mut store,
        &Seed::new("Linked guide")
            .chunk("Intro", "upgrade keyword words")
            .embedded("Usage", "upgrade vector words", axis(0))
            .link("git:h/o/r", &["src/**"]),
    );
    let global = insert(
        &mut store,
        &Seed::new("Global guide").embedded("Intro", "global words", axis(1)),
    );
    store.set_meta("model_id", MODEL).expect("model");
    drop(store);

    downgrade_to_version_one(&db);
    let raw = db.raw();

    assert_eq!(version(&raw), 1);
    assert_eq!(db.count(DOCUMENTS, doc! { "status": { "$exists": true } }), 0);
    assert_eq!(db.count(CHUNKS, doc! { "review": { "$exists": true } }), 0);

    // A reader that knows version 2 waits for a writer to upgrade the database, and an open that is
    // told to defer leaves it as it is.
    match MongoStore::connect_read_only(&db.url) {
        Err(StoreError::Backend(message)) => assert!(message.contains("not been upgraded yet"), "{message}"),
        other => panic!(
            "expected the not-upgraded answer, got {:?}",
            other.map(|store| store.is_some())
        ),
    }

    let secrets = crate::config::Secrets::of(&db.url, None);
    let deferred = open_boxed_with(
        &db.url,
        Access::Existing,
        SERVER_SELECTION_TIMEOUT,
        Upgrade::Defer,
        &secrets,
    );
    assert!(matches!(deferred, Ok(crate::backend::Opened::NeedsUpgrade)));
    assert_eq!(version(&raw), 1, "nothing was upgraded");

    // The writer upgrades it.
    let mut writer = MongoStore::connect_existing(&db.url)
        .expect("upgrade")
        .expect("a database");

    assert_eq!(version(&raw), 2);
    assert_eq!(db.count(DOCUMENTS, doc! { "status": "live", "review_rev": 0_i64 }), 2);
    assert_eq!(db.count(DOCUMENTS, doc! { "pending_of": { "$exists": true } }), 0);
    assert_eq!(db.count(CHUNKS, doc! { "review": "live" }), 3);
    assert_eq!(db.count(CHUNKS, doc! { "review": { "$ne": "live" } }), 0);

    let mut names = raw
        .collection::<Document>(DOCUMENTS)
        .list_index_names()
        .run()
        .expect("indexes");
    names.sort();
    assert!(names.contains(&"documents_status".to_owned()), "{names:?}");
    assert!(names.contains(&"documents_pending".to_owned()), "{names:?}");

    let mut collections = raw.list_collection_names().run().expect("collections");
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

    // The new search indexes were created by name (they build in the background).
    for index in [KEYWORD_INDEX, VECTOR_INDEX] {
        assert_ne!(index_state(&raw, index), IndexState::Missing, "{index} was created");
    }

    let page = writer.list(&everything()).expect("list");
    assert_eq!(page.total, 2);
    assert!(
        page.items
            .iter()
            .all(|row| row.doc.status == Status::Live && row.doc.review_rev == 0 && row.pending.is_none())
    );
    assert!(writer.get(linked, &Detail::default()).expect("get").is_some());

    // Once the new indexes answer, the writer drops the version 1 ones, and search works on the new.
    assert_eq!(
        writer.wait_for_indexes(WAIT).expect("wait"),
        (IndexState::Ready, IndexState::Ready)
    );
    index_becomes(&raw, LEGACY_KEYWORD_INDEX, IndexState::Missing);
    index_becomes(&raw, LEGACY_VECTOR_INDEX, IndexState::Missing);

    let scope = Scope::everything();
    let found = eventually(
        "the upgraded documents are searchable",
        || writer.keyword_top_k("upgrade", &scope, 10),
        |hits| !hits.is_empty(),
    );
    assert!(found.iter().all(|hit| hit.doc_id == linked));
    assert!(found.iter().all(|hit| hit.status == Status::Live));
    assert_eq!(
        writer.vector_top_k(&axis(1), &scope, 1).expect("vector")[0].doc_id,
        global
    );

    // Running it again changes nothing.
    drop(writer);
    let again = MongoStore::connect_existing(&db.url)
        .expect("again")
        .expect("a database");
    assert_eq!(version(&raw), 2);
    assert_eq!(
        again.wait_for_indexes(WAIT).expect("wait"),
        (IndexState::Ready, IndexState::Ready)
    );
}

#[test]
fn team_mongo_an_upgrade_stopped_half_way_is_finished_by_running_it_again() {
    let Some((db, mut store)) = store() else { return };
    let ids: Vec<i64> = (0..3)
        .map(|n| insert(&mut store, &Seed::new(&format!("Guide {n}")).chunk("A", "words")))
        .collect();
    drop(store);

    downgrade_to_version_one(&db);
    let raw = db.raw();

    // The upgrade got as far as the documents of one of the three, and no further.
    raw.collection::<Document>(DOCUMENTS)
        .update_one(
            doc! { "_id": ids[0] },
            doc! { "$set": { "status": "live", "review_rev": 0_i64 } },
        )
        .run()
        .expect("half");
    assert_eq!(version(&raw), 1);

    MongoStore::connect_existing(&db.url)
        .expect("upgrade")
        .expect("a database");

    assert_eq!(version(&raw), 2);
    assert_eq!(db.count(DOCUMENTS, doc! { "status": "live", "review_rev": 0_i64 }), 3);
    assert_eq!(db.count(CHUNKS, doc! { "review": "live" }), 3);
}

#[test]
fn team_mongo_a_fresh_database_has_a_unique_pending_version_per_live_document() {
    let Some((db, store)) = store() else { return };
    drop(store);

    let documents = db.raw().collection::<Document>(DOCUMENTS);
    let row = |id: i64, pending_of: Option<i64>| {
        let mut row = doc! { "_id": id, "status": "draft", "review_rev": 0_i64 };

        if let Some(live) = pending_of {
            row.insert("pending_of", live);
        }

        row
    };

    // Rows without `pending_of` do not take part in the index.
    documents
        .insert_many([row(1, None), row(2, None), row(3, Some(1))])
        .run()
        .expect("insert");

    let duplicate = documents
        .insert_one(row(4, Some(1)))
        .run()
        .expect_err("a second pending version");
    assert!(duplicate.to_string().contains("11000"), "{duplicate}");

    documents
        .insert_one(row(5, Some(2)))
        .run()
        .expect("another live document");
    assert_eq!(db.count(DOCUMENTS, doc! { "pending_of": { "$exists": true } }), 2);
}

#[test]
fn team_mongo_a_database_a_newer_server_upgraded_meanwhile_is_refused_at_the_next_look() {
    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("A", "words"));

    // The version read at connect stays good for a while.
    store.schema_recheck = Duration::from_secs(3600);
    db.raw()
        .collection::<Document>(META)
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": schema::SCHEMA_VERSION + 1 } },
        )
        .run()
        .expect("a newer server upgrades");

    assert!(store.get(id, &Detail::default()).expect("within the time").is_some());
    assert!(!store.is_newer());

    // Then the next read looks again and fails closed, and so does everything after it.
    store.schema_recheck = Duration::ZERO;
    assert!(matches!(
        store.get(id, &Detail::default()),
        Err(StoreError::NewerSchema)
    ));
    assert!(store.is_newer());

    store.schema_recheck = Duration::from_secs(3600);
    assert!(matches!(store.stats(), Err(StoreError::NewerSchema)));
    assert!(matches!(store.delete(&[id]), Err(StoreError::NewerSchema)));
    assert!(matches!(
        store.keyword_top_k("words", &Scope::everything(), 5),
        Err(StoreError::NewerSchema)
    ));

    // A reader connected before the upgrade learns it the same way.
    drop(store);
    db.raw()
        .collection::<Document>(META)
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": schema::SCHEMA_VERSION } },
        )
        .run()
        .expect("back");
    let mut reader = MongoStore::connect_read_only(&db.url).expect("open").expect("exists");
    assert_eq!(reader.stats().expect("stats").documents, 1);

    db.raw()
        .collection::<Document>(META)
        .update_one(
            doc! { "_id": "schema" },
            doc! { "$set": { "version": Bson::Int64(schema::SCHEMA_VERSION + 1) } },
        )
        .run()
        .expect("newer again");
    reader.schema_recheck = Duration::ZERO;
    assert!(matches!(reader.list(&everything()), Err(StoreError::NewerSchema)));
}

#[test]
fn team_mongo_an_upgrade_of_more_than_a_page_fills_every_chunk() {
    let Some((db, mut store)) = store() else { return };

    // More than two pages of chunks, and more than one page of documents.
    let mut big = Seed::new("Big");

    for n in 0..PAGE * 2 + 120 {
        big = big.chunk(&format!("Part {n}"), "text of the part");
    }

    insert(&mut store, &big);

    for n in 0..PAGE + 30 {
        insert(
            &mut store,
            &Seed::new(&format!("Small {n}"))
                .source(&format!("/s/{n}"))
                .chunk("A", "small"),
        );
    }

    drop(store);

    downgrade_to_version_one(&db);
    assert_eq!(db.count(CHUNKS, doc! { "review": { "$exists": true } }), 0);

    MongoStore::connect_existing(&db.url)
        .expect("upgrade")
        .expect("a database");

    let chunks = (PAGE * 2 + 120 + PAGE + 30) as u64;
    let documents = (PAGE + 31) as u64;

    assert_eq!(db.count(CHUNKS, doc! { "review": "live" }), chunks);
    assert_eq!(db.count(CHUNKS, doc! {}), chunks);
    assert_eq!(
        db.count(DOCUMENTS, doc! { "status": "live", "review_rev": 0_i64 }),
        documents
    );
    assert_eq!(version(&db.raw()), 2);
}

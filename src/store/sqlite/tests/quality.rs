//! Authors, usefulness counters, titles and shared chunks in the local file, and the columns that
//! hold them being added in place.

use rusqlite::Connection;

use crate::store::sqlite::schema::{SCHEMA_VERSION, migrate, user_version};
use crate::store::testing::{
    Seed, absent_columns_contract, authors_contract, counters_contract, insert, shared_chunks_contract, store,
    titles_contract, useful_listing_contract,
};
use crate::store::{DocUsage, KnowledgeStore, SqliteStore};

const ADDED: [&str; 7] = [
    "added_by_name",
    "added_by_via",
    "updated_by_name",
    "updated_by_via",
    "shown",
    "useful",
    "last_useful_at",
];

fn folder() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// Removes the added columns, leaving the file as an older server's.
fn strip(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open");

    for name in ADDED {
        conn.execute_batch(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
            .expect("drop column");
    }
}

#[test]
fn authors_follow_the_contract() {
    authors_contract(&mut store());
}

#[test]
fn counters_follow_the_contract() {
    counters_contract(&mut store());
}

#[test]
fn the_useful_filter_and_sorts_follow_the_contract() {
    useful_listing_contract(&mut store());
}

#[test]
fn titles_follow_the_contract() {
    titles_contract(&mut store());
}

#[test]
fn shared_chunks_follow_the_contract() {
    shared_chunks_contract(&mut store());
}

#[test]
fn a_file_without_the_columns_reads_without_the_fields_and_counts_nothing() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");

    let mut writer = SqliteStore::open(&path).expect("create");
    let id = insert(&mut writer, &Seed::new("Guide").chunk("Intro", "hello").by("Ada", None));
    drop(writer);
    strip(&path);

    let mut reader = SqliteStore::open_read_only(&path).expect("open").expect("a database");
    absent_columns_contract(&mut reader, id);
}

#[test]
fn opening_for_writing_adds_the_columns_and_keeps_the_rows() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");

    let mut writer = SqliteStore::open(&path).expect("create");
    let id = insert(&mut writer, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(writer);
    strip(&path);

    let mut again = SqliteStore::open_existing(&path).expect("open").expect("a database");
    let before = again.get(id, &Default::default()).expect("get").expect("document");

    assert_eq!(
        before.usage,
        Some(DocUsage {
            shown: 0,
            useful: 0,
            last_useful_at: None
        })
    );
    assert_eq!(before.doc.title, "Guide");
    assert_eq!((before.doc.added_by, before.doc.updated_by), (None, None));

    again.record_shown(&[id]).expect("shown");
    assert_eq!(
        again
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|u| u.shown),
        Some(1)
    );
}

#[test]
fn a_file_extended_by_this_version_keeps_its_schema_version_and_opens_read_only_and_for_an_older_insert() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");

    // A file as an older server leaves it: migrated, no added columns.
    {
        let mut conn = Connection::open(&path).expect("open");
        migrate(&mut conn).expect("migrate");
        assert_eq!(user_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    let writer = SqliteStore::open(&path).expect("extend");
    assert_eq!(writer.schema_version().expect("version"), SCHEMA_VERSION);
    drop(writer);

    // The read-only open refuses on the version alone, which did not change.
    let mut reader = SqliteStore::open_read_only(&path).expect("open").expect("a database");
    assert_eq!(reader.schema_version().expect("version"), SCHEMA_VERSION);
    assert!(
        reader
            .list(&crate::store::ListQuery {
                filter: crate::store::RepoFilter::All,
                format: None,
                text: String::new(),
                useful: None,
                sort: crate::store::Sort::Title,
                descending: false,
                offset: 0,
                limit: 10,
            })
            .is_ok()
    );
    drop(reader);

    // An older server's insert names the old columns only: the new ones take their defaults.
    let conn = Connection::open(&path).expect("open");
    conn.execute(
        "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
             bytes, chunk_count, origin, warnings, created_at, updated_at)
         VALUES ('Old', 'old.md', '/old', 'markdown', 'c', 'f', 1, 0, 'user', '[]', 1, 1)",
        [],
    )
    .expect("an older insert");
    drop(conn);

    let mut newer = SqliteStore::open_read_only(&path).expect("open").expect("a database");
    let rows = newer
        .list(&crate::store::ListQuery {
            filter: crate::store::RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            sort: crate::store::Sort::Title,
            descending: false,
            offset: 0,
            limit: 10,
        })
        .expect("list")
        .items;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].usage.map(|usage| (usage.shown, usage.useful)), Some((0, 0)));
    assert_eq!(rows[0].doc.added_by, None);
}

#[test]
fn counting_never_waits_for_a_lock_another_connection_holds() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");

    let mut counter = SqliteStore::open(&path).expect("create");
    let id = insert(&mut counter, &Seed::new("Guide").chunk("Intro", "hello"));

    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold the write lock");

    let started = std::time::Instant::now();
    assert!(counter.record_shown(&[id]).is_err(), "a busy file drops the count");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "and does not wait for it"
    );

    holder.execute_batch("ROLLBACK").expect("release");
    counter.record_shown(&[id]).expect("counts again");
    assert_eq!(
        counter
            .get(id, &Default::default())
            .expect("get")
            .expect("doc")
            .usage
            .map(|u| u.shown),
        Some(1)
    );
}

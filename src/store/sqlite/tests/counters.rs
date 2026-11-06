//! The counters-only open: it counts and does nothing else, and it never waits.

use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::store::sqlite::schema::user_version;
use crate::store::testing::{Seed, insert};
use crate::store::{KnowledgeStore, SqliteStore};

const ADDED: [&str; 7] = [
    "added_by_name",
    "added_by_via",
    "updated_by_name",
    "updated_by_via",
    "shown",
    "useful",
    "last_useful_at",
];

/// A database with one document, and its path.
fn seeded() -> (tempfile::TempDir, std::path::PathBuf, i64) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("knowledge.sqlite3");
    let mut store = SqliteStore::open(&path).expect("create");
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));

    (dir, path, id)
}

fn strip(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open");

    for name in ADDED {
        conn.execute_batch(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
            .expect("drop column");
    }
}

/// The names of the columns of the documents table, and the schema version.
fn schema_of(path: &std::path::Path) -> (Vec<String>, i64) {
    let conn = Connection::open(path).expect("open");
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('knowledge_documents') ORDER BY cid")
        .expect("prepare");
    let names = statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("names");

    (names, user_version(&conn).expect("version"))
}

#[test]
fn a_counters_handle_counts_in_a_current_database() {
    let (_dir, path, id) = seeded();

    let mut counters = SqliteStore::open_counters(&path).expect("open").expect("a database");
    counters.record_shown(&[id]).expect("shown");
    counters.record_useful(&[id], 7).expect("useful");

    let usage = SqliteStore::open_read_only(&path)
        .expect("open")
        .expect("a database")
        .get(id, &Default::default())
        .expect("get")
        .expect("document")
        .usage
        .expect("counters");

    assert_eq!((usage.shown, usage.useful, usage.last_useful_at), (1, 1, Some(7)));
}

#[test]
fn a_counters_handle_does_not_create_a_file_or_a_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("knowledge.sqlite3");

    assert!(SqliteStore::open_counters(&path).expect("open").is_none());
    assert!(!path.exists());

    std::fs::write(&path, b"").expect("an empty file");
    assert!(SqliteStore::open_counters(&path).expect("open").is_none());
    assert_eq!(
        std::fs::metadata(&path).expect("file").len(),
        0,
        "nothing written to it"
    );
}

#[test]
fn a_counters_handle_adds_no_column_and_counts_nothing_without_them() {
    let (_dir, path, id) = seeded();
    strip(&path);
    let before = schema_of(&path);

    let mut counters = SqliteStore::open_counters(&path).expect("open").expect("a database");
    counters.record_shown(&[id]).expect("does nothing");
    counters.record_useful(&[id], 7).expect("does nothing");
    drop(counters);

    assert_eq!(schema_of(&path), before, "no column, no version change");
}

#[test]
fn a_counters_handle_does_not_migrate_a_file_that_is_not_at_the_current_version() {
    let (_dir, path, _) = seeded();
    Connection::open(&path)
        .expect("open")
        .execute_batch("PRAGMA user_version = 0")
        .expect("a version before the first");
    let before = schema_of(&path);

    // Opening for writing would run the migration from version 0.
    let counters = SqliteStore::open_counters(&path).expect("open");

    assert!(counters.is_none(), "not counted in");
    assert_eq!(schema_of(&path), before, "not migrated");
    assert_eq!(before.1, 0);
}

#[test]
fn a_counters_handle_never_waits_for_a_lock_to_open_or_to_count() {
    let (_dir, path, id) = seeded();
    strip(&path);

    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold the write lock");

    // Opening for writing would wait for this lock to add the columns.
    let started = Instant::now();
    let mut counters = SqliteStore::open_counters(&path).expect("open").expect("a database");
    counters.record_shown(&[id]).expect("does nothing without columns");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );

    holder.execute_batch("ROLLBACK").expect("release");
    drop(counters);

    // With the columns present a count meets the lock and fails at once, as busy.
    let (_dir, path, id) = seeded();
    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold the write lock");

    let started = Instant::now();
    let mut counters = SqliteStore::open_counters(&path).expect("open").expect("a database");
    let busy = counters.record_shown(&[id]);

    assert!(matches!(busy, Err(crate::store::StoreError::Busy)), "{busy:?}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );

    // The handle stays good, and the lock still is not waited for.
    holder.execute_batch("ROLLBACK").expect("release");
    counters.record_shown(&[id]).expect("counts again");
}

fn counters_of(path: &std::path::Path, id: i64) -> Option<(i64, i64)> {
    SqliteStore::open_read_only(path)
        .expect("open")
        .expect("a database")
        .get(id, &Default::default())
        .expect("a row that is read whatever it holds")
        .expect("document")
        .usage
        .map(|usage| (usage.shown, usage.useful))
}

#[test]
fn a_counter_stops_at_the_largest_integer_instead_of_becoming_a_float() {
    let (_dir, path, id) = seeded();
    Connection::open(&path)
        .expect("open")
        .execute_batch("UPDATE knowledge_documents SET shown = 9223372036854775806, useful = 9223372036854775807")
        .expect("near the end");

    let mut counters = SqliteStore::open_counters(&path).expect("open").expect("a database");

    for _ in 0..3 {
        counters.record_shown(&[id]).expect("shown");
        counters.record_useful(&[id], 7).expect("useful");
    }

    assert_eq!(counters_of(&path, id), Some((i64::MAX, i64::MAX)));

    let kind: String = Connection::open(&path)
        .expect("open")
        .query_row(
            "SELECT typeof(shown) || typeof(useful) FROM knowledge_documents",
            [],
            |row| row.get(0),
        )
        .expect("types");
    assert_eq!(kind, "integerinteger");
}

#[test]
fn a_counter_or_a_name_another_type_was_put_in_reads_instead_of_failing_the_row() {
    let (_dir, path, id) = seeded();
    let conn = Connection::open(&path).expect("open");

    conn.execute_batch(
        "UPDATE knowledge_documents SET shown = 'many', useful = 1e30, last_useful_at = 'yesterday',
             added_by_name = x'00ff', added_by_via = 'codex', updated_by_name = 'Ada', updated_by_via = x'00ff'",
    )
    .expect("hand edits");

    let mut store = SqliteStore::open_read_only(&path).expect("open").expect("a database");
    let detail = store
        .get(id, &Default::default())
        .expect("the row reads")
        .expect("document");

    let usage = detail.usage.expect("counters");
    assert_eq!(
        (usage.shown, usage.useful, usage.last_useful_at),
        (0, i64::MAX, Some(0))
    );
    assert_eq!(detail.doc.added_by, None);
    assert_eq!(
        detail.doc.updated_by.map(|author| (author.name, author.via)),
        Some(("Ada".to_owned(), None))
    );

    let listed = store
        .list(&crate::store::ListQuery {
            filter: crate::store::RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            author: None,
            want_authors: false,
            sort: crate::store::Sort::Useful,
            descending: false,
            offset: 0,
            limit: 10,
            status: crate::store::StatusFilter::All,
        })
        .expect("the listing reads");
    assert_eq!(listed.items.len(), 1);
}

#[test]
fn a_normal_write_that_waits_out_a_lock_is_a_database_error_not_busy() {
    let (_dir, path, _) = seeded();
    let mut store = SqliteStore::open(&path).expect("open");
    store
        .conn
        .busy_timeout(Duration::from_millis(50))
        .expect("a short wait");

    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold the write lock");

    let timed_out = store.set_meta("model_id", "some-model").expect_err("the lock is held");

    assert!(
        matches!(&timed_out, crate::store::StoreError::Backend(message) if message == "DatabaseBusy"),
        "{timed_out:?}"
    );
}

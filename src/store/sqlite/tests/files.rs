use crate::store::sqlite::schema::SCHEMA_VERSION;
use crate::store::testing::{Seed, insert};
use crate::store::{KnowledgeStore, SqliteStore, StoreError};

fn folder() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn opening_creates_the_file_and_the_schema_and_reopening_keeps_the_data() {
    let dir = folder();
    let path = dir.path().join("sub").join("knowledge.sqlite3");

    let mut store = SqliteStore::open(&path).expect("create");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let mut again = SqliteStore::open(&path).expect("reopen");
    assert_eq!(
        again
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .doc
            .title,
        "Guide"
    );
    assert_eq!(again.meta("schema_version").expect("meta").as_deref(), Some("1"));
    assert_eq!(again.meta("dims").expect("meta").as_deref(), Some("384"));
}

#[cfg(unix)]
#[test]
fn the_database_and_its_folder_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = folder();
    let path = dir.path().join("data").join("knowledge.sqlite3");
    SqliteStore::open(&path).expect("create");

    let mode = |path: &std::path::Path| std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().expect("parent")), 0o700);
}

#[test]
fn a_read_only_open_of_a_missing_database_creates_nothing() {
    let dir = folder();
    let path = dir.path().join("data").join("knowledge.sqlite3");

    assert!(SqliteStore::open_read_only(&path).expect("open").is_none());
    assert!(SqliteStore::open_existing(&path).expect("open").is_none());
    assert!(!dir.path().join("data").exists());
    assert!(names(dir.path()).is_empty());
}

#[test]
fn a_file_nothing_was_written_to_reads_as_empty_and_stays_as_it_is() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    std::fs::write(&path, b"").expect("empty file");

    assert!(SqliteStore::open_read_only(&path).expect("open").is_none());
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
    assert_eq!(names(dir.path()), ["knowledge.sqlite3"]);
}

#[test]
fn a_read_only_store_reads_and_cannot_write() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    let mut writer = SqliteStore::open(&path).expect("create");
    insert(&mut writer, &Seed::new("Guide").chunk("Intro", "hello"));

    let mut reader = SqliteStore::open_read_only(&path).expect("open").expect("exists");
    assert_eq!(reader.stats().expect("stats").documents, 1);

    let (doc, chunks, links) = Seed::new("Other").chunk("A", "b").parts();
    assert!(matches!(
        reader.put_document(&doc, &chunks, &links),
        Err(StoreError::Backend(_))
    ));
    assert_eq!(reader.stats().expect("stats").documents, 1);
}

#[test]
fn a_database_from_a_newer_server_is_left_alone() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    let mut store = SqliteStore::open(&path).expect("create");
    drop(store.stats());
    drop(store);

    rusqlite::Connection::open(&path)
        .and_then(|conn| conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)))
        .expect("bump the version");

    let mut newer = SqliteStore::open(&path).expect("opens");
    assert!(newer.is_newer());
    assert!(matches!(newer.stats(), Err(StoreError::NewerSchema)));
    assert!(matches!(newer.delete(&[1]), Err(StoreError::NewerSchema)));

    let mut read = SqliteStore::open_read_only(&path).expect("open").expect("exists");
    assert!(matches!(
        read.list(&super::listing::everything()),
        Err(StoreError::NewerSchema)
    ));
}

#[test]
fn fts5_is_available_in_this_build() {
    assert!(crate::store::sqlite::fts5_available());
}

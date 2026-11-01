//! Opening, migrating, sharing the database between clients and the capability probe, on a real
//! server.

use std::thread;

use pretty_assertions::assert_eq;

use super::support::{insert, server};
use crate::store::libsql::{Capabilities, LibsqlStore, SCHEMA_VERSION, probe};
use crate::store::testing::Seed;
use crate::store::{Detail, KnowledgeStore, StoreError};

fn tables(server: &super::support::Server) -> i64 {
    server.integer("SELECT count(*) FROM sqlite_master WHERE name LIKE 'knowledge_%'")
}

#[test]
fn team_connecting_creates_the_schema_and_reconnecting_keeps_the_data() {
    let Some(server) = server() else { return };

    let mut store = server.store();
    assert_eq!(store.stats().expect("stats").schema_version, SCHEMA_VERSION);
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let mut again = server.store();
    let found = again.get(id, &Detail::default()).expect("get").expect("document");
    assert_eq!(found.doc.title, "Guide");
    assert_eq!(again.meta("schema_version").expect("meta").as_deref(), Some("1"));
    assert_eq!(again.meta("dims").expect("meta").as_deref(), Some("384"));
}

#[test]
fn team_a_read_only_or_existing_open_of_an_empty_database_creates_nothing() {
    let Some(server) = server() else { return };

    assert!(
        LibsqlStore::open_read_only(&server.url, server.token.as_deref())
            .expect("open")
            .is_none()
    );
    assert!(
        LibsqlStore::open_existing(&server.url, server.token.as_deref())
            .expect("open")
            .is_none()
    );
    assert_eq!(tables(&server), 0);

    server.store();
    assert!(
        LibsqlStore::open_read_only(&server.url, server.token.as_deref())
            .expect("open")
            .is_some()
    );
    assert!(
        LibsqlStore::open_existing(&server.url, server.token.as_deref())
            .expect("open")
            .is_some()
    );
}

#[test]
fn team_a_read_only_store_reads_and_cannot_write() {
    let Some(server) = server() else { return };
    let mut writer = server.store();
    insert(&mut writer, &Seed::new("Guide").chunk("Intro", "hello"));

    let mut reader = LibsqlStore::open_read_only(&server.url, server.token.as_deref())
        .expect("open")
        .expect("exists");
    assert_eq!(reader.stats().expect("stats").documents, 1);

    let (doc, chunks, links) = Seed::new("Other").chunk("A", "b").parts();
    assert!(matches!(
        reader.put_document(&doc, &chunks, &links),
        Err(StoreError::Backend(_))
    ));
    assert!(matches!(reader.delete(&[1]), Err(StoreError::Backend(_))));
    assert_eq!(reader.stats().expect("stats").documents, 1);
}

#[test]
fn team_a_database_from_a_newer_server_is_left_alone() {
    let Some(server) = server() else { return };
    drop(server.store());
    server
        .execute("UPDATE knowledge_meta SET value = '2' WHERE key = 'schema_version'")
        .expect("bump");

    let mut newer = server.store();
    assert!(newer.is_newer());
    assert!(matches!(newer.stats(), Err(StoreError::NewerSchema)));
    assert!(matches!(newer.delete(&[1]), Err(StoreError::NewerSchema)));
    assert_eq!(server.integer("SELECT count(*) FROM knowledge_documents"), 0);

    let mut read = LibsqlStore::open_read_only(&server.url, server.token.as_deref())
        .expect("open")
        .expect("exists");
    assert!(matches!(
        read.list(&super::listing::everything()),
        Err(StoreError::NewerSchema)
    ));
}

#[test]
fn team_a_client_connected_before_a_newer_server_migrated_stops_writing() {
    let Some(server) = server() else { return };
    let mut early = server.store();
    insert(&mut early, &Seed::new("Before").chunk("A", "x"));

    server
        .execute("UPDATE knowledge_meta SET value = '2' WHERE key = 'schema_version'")
        .expect("bump");

    let (doc, chunks, links) = Seed::new("After").chunk("A", "y").parts();
    assert!(matches!(
        early.put_document(&doc, &chunks, &links),
        Err(StoreError::NewerSchema)
    ));
    assert!(early.is_newer(), "and keeps refusing");
    assert_eq!(server.integer("SELECT count(*) FROM knowledge_documents"), 1);
}

#[test]
fn team_clients_that_connect_at_the_same_time_migrate_once() {
    let Some(server) = server() else { return };

    let stores: Vec<_> = (0..4)
        .map(|_| {
            let (url, token) = (server.url.clone(), server.token.clone());
            thread::spawn(move || LibsqlStore::connect(&url, token.as_deref()))
        })
        .collect();

    for handle in stores {
        handle.join().expect("thread").expect("connect");
    }

    assert_eq!(
        server.integer("SELECT count(*) FROM knowledge_meta WHERE key = 'schema_version'"),
        1
    );
    assert_eq!(
        server.integer("SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'schema_version'"),
        SCHEMA_VERSION
    );
}

#[test]
fn team_writers_at_the_same_time_queue_and_every_write_counts() {
    let Some(server) = server() else { return };
    drop(server.store());

    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let (url, token) = (server.url.clone(), server.token.clone());
            thread::spawn(move || {
                let mut store = LibsqlStore::connect(&url, token.as_deref()).expect("connect");

                for n in 0..3 {
                    let seed = Seed::new(&format!("Doc {writer}-{n}"))
                        .source(&format!("/docs/{writer}-{n}"))
                        .chunk("A", "words");
                    insert(&mut store, &seed);
                }
            })
        })
        .collect();

    for handle in writers {
        handle.join().expect("writer");
    }

    let mut store = server.store();
    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.generation), (12, 12));
}

#[test]
fn team_the_probe_finds_vectors_and_the_keyword_index_and_creates_nothing() {
    let Some(server) = server() else { return };

    assert_eq!(
        probe(&server.url, server.token.as_deref()),
        Ok(Capabilities {
            keyword: true,
            vector: true
        })
    );
    assert_eq!(tables(&server), 0);
}

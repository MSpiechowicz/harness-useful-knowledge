//! Counting never holds up a request, never changes the shape of the database and gives up on a
//! database that fails it.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::*;
use crate::config::Place;
use crate::config::{Routing, Where, backend_for};
use crate::mcp::counting::Counting;
use crate::store::routed::ids;
use crate::store::{Detail, DocUsage};

const ADDED: [&str; 7] = [
    "added_by_name",
    "added_by_via",
    "updated_by_name",
    "updated_by_via",
    "shown",
    "useful",
    "last_useful_at",
];

/// A computer with three documents that all match "install gateway", and their ids.
fn seeded() -> (World, [i64; 3]) {
    let mut world = World::new();
    let mut database = world.database();
    let docs = [
        ("Gateway guide", "Install the gateway with the installer.", 5.0),
        ("Gateway runbook", "Install the gateway by tagging a release.", 40.0),
        ("Gateway notes", "Install the gateway from the package.", 70.0),
    ]
    .map(|(title, text, angle)| {
        insert(
            &mut database,
            &Seed::new(title).embedded("Install", text, angled(angle)),
        )
    });
    drop(database);
    world.server.store = None;

    (world, docs)
}

fn connection(world: &World) -> Connection {
    Connection::open(world.knowledge().join(FILE_NAME)).expect("second connection")
}

/// Leaves the file as an older server's: none of the added columns.
fn strip(world: &World) {
    let conn = connection(world);

    for name in ADDED {
        conn.execute_batch(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
            .expect("drop column");
    }
}

/// The columns of the documents table and the schema version of the file.
fn schema_of(world: &World) -> (Vec<String>, i64) {
    let conn = connection(world);
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('knowledge_documents') ORDER BY cid")
        .expect("prepare");
    let names = statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("names");
    let version = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");

    (names, version)
}

fn search(world: &mut World) -> String {
    let project = project_arg(world);

    world.ok(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project }),
    )
}

fn feedback(world: &mut World, doc: i64) -> String {
    let project = project_arg(world);

    world.ok("knowledge_feedback", json!({ "ids": [doc], "project_path": project }))
}

fn usage(world: &World, doc: i64) -> DocUsage {
    let mut store = SqliteStore::open_read_only(&world.knowledge().join(FILE_NAME))
        .expect("opens")
        .expect("the database exists");

    store
        .get(doc, &Detail::default())
        .expect("reads")
        .expect("the document exists")
        .usage
        .expect("the database has counters")
}

#[test]
fn a_database_another_process_is_writing_does_not_hold_up_the_next_request() {
    // Without the added columns an open for writing would wait for the lock to add them.
    for extended in [true, false] {
        let (mut world, docs) = seeded();

        if !extended {
            strip(&world);
        }

        let lock = connection(&world);
        lock.execute_batch("BEGIN IMMEDIATE").expect("takes the write lock");

        // The count of the first request is dropped at once, not waited for, and the second request
        // is answered promptly too.
        let started = Instant::now();
        search(&mut world);
        let first = started.elapsed();

        let started = Instant::now();
        let fed = feedback(&mut world, docs[0]);
        let second = started.elapsed();

        assert!(first < Duration::from_millis(500), "{extended}: {first:?}");
        assert!(second < Duration::from_millis(500), "{extended}: {second:?}");
        assert!(fed.starts_with("marked 1 document useful"), "{fed}");

        lock.execute_batch("ROLLBACK").expect("releases it");
    }
}

#[test]
fn counting_leaves_the_columns_and_the_version_of_a_database_without_them_alone() {
    let (mut world, docs) = seeded();
    strip(&world);
    let before = schema_of(&world);

    search(&mut world);
    feedback(&mut world, docs[0]);
    search(&mut world);

    assert_eq!(schema_of(&world), before);
}

#[test]
fn a_database_that_is_not_at_this_version_is_not_counted_in_or_changed() {
    let (world, _) = seeded();
    connection(&world)
        .execute_batch("PRAGMA user_version = 0")
        .expect("a version before the first");
    let before = schema_of(&world);

    let mut counting = Counting::default();
    counting.queue_shown(&[1]);
    counting.flush(&world.server.env, &world.server.backend, world.server.routing);

    assert_eq!(schema_of(&world), before);
}

#[test]
fn after_a_write_fails_counting_pauses_instead_of_trying_after_every_reply() {
    let (mut world, docs) = seeded();

    // A trigger makes every counter write fail with an error that is not a busy lock.
    let trigger = connection(&world);
    trigger
        .execute_batch(
            "CREATE TRIGGER no_counting BEFORE UPDATE ON knowledge_documents
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .expect("trigger");

    search(&mut world);
    assert_eq!(usage(&world, docs[0]).shown, 0, "the write failed");

    trigger.execute_batch("DROP TRIGGER no_counting").expect("drop");

    feedback(&mut world, docs[0]);
    assert_eq!(usage(&world, docs[0]).useful, 0, "not tried again so soon");
}

#[test]
fn a_lock_only_drops_that_count_and_the_next_one_is_written() {
    let (mut world, docs) = seeded();

    let lock = connection(&world);
    lock.execute_batch("BEGIN IMMEDIATE").expect("takes the write lock");
    search(&mut world);
    lock.execute_batch("ROLLBACK").expect("releases it");

    feedback(&mut world, docs[0]);

    let counted = usage(&world, docs[0]);
    assert_eq!(
        (counted.shown, counted.useful),
        (0, 1),
        "the search's count was dropped, this one not"
    );
}

/// A server that accepts connections and never answers.
fn silent_server() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let accepting = listener.try_clone().expect("clone");

    std::thread::spawn(move || {
        let mut held = Vec::new();

        while let Ok((stream, _)) = accepting.accept() {
            held.push(stream);
        }
    });

    (listener, format!("postgres://nobody@127.0.0.1:{port}/none"))
}

#[test]
fn a_remote_that_never_answers_costs_one_bounded_flush_and_then_nothing() {
    let (world, docs) = seeded();
    let (_listener, url) = silent_server();
    let backend = backend_for(Some(url), None).expect("backend");
    let routing = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
    let local = ids::tag(Place::Local, docs[0]);

    let mut counting = Counting::default();
    let env = &world.server.env;

    counting.queue_shown(&[local]);
    let started = Instant::now();
    counting.flush(env, &backend, routing);
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "{:?}",
        started.elapsed()
    );

    counting.queue_useful(&[local], 5);
    let started = Instant::now();
    counting.flush(env, &backend, routing);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "no new attempt: {:?}",
        started.elapsed()
    );
}

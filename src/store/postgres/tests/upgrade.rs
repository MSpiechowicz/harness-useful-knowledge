//! The machinery of a schema upgrade: it is not bound by the limits of an ordinary session, and an
//! open that is told to defer leaves a database that needs one alone. There is no migration after
//! version 1 yet, so these add one of their own to [`schema::migrations`].

use super::support::{database, insert};
use crate::backend::{Access, Opened, Upgrade};
use crate::store::postgres::schema::{self, MIGRATE_LOCK, SchemaState};
use crate::store::postgres::{CONNECT_TIMEOUT, PostgresStore};
use crate::store::testing::Seed;

/// The migrations of version 1 and one more that takes `slow` to run.
fn steps(slow: &str) -> Vec<String> {
    let mut steps = schema::migrations();
    steps.push(format!("{slow}; ALTER TABLE knowledge_chunks ADD COLUMN extra TEXT;"));

    steps
}

#[test]
fn team_a_migration_outlasts_the_statement_timeout_of_the_session() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    insert(&mut store, &Seed::new("Guide").chunk("Setup", "alpha"));
    drop(store);

    let mut session = db.admin();
    session.batch_execute("SET statement_timeout = '300ms'").expect("limit");

    // The limit is real: an ordinary statement that runs longer is cancelled.
    assert!(session.batch_execute("SELECT pg_sleep(1.5)").is_err());

    let state = schema::migrate_with(&mut session, &steps("SELECT pg_sleep(1.5)")).expect("the migration completes");

    assert_eq!(state, SchemaState::Current);
    assert_eq!(schema::stored_version(&mut session).expect("version"), Some(2));

    let extra: i64 = session
        .query_one(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = $1 AND column_name = 'extra'",
            &[&db.schema()],
        )
        .expect("columns")
        .get(0);
    assert_eq!(extra, 1, "the step ran");

    // The session's own limit is back once the migration is done.
    let limit: String = session.query_one("SHOW statement_timeout", &[]).expect("show").get(0);
    assert_eq!(limit, "300ms");
}

#[test]
fn team_a_migration_lifts_the_statement_timeout_but_keeps_a_finite_idle_limit() {
    let Some(db) = database() else { return };
    drop(db.open());

    // The step fails the migration unless the limits are as intended while it runs.
    let check = "DO $$ BEGIN \
        IF current_setting('statement_timeout') <> '0' THEN RAISE EXCEPTION 'statement limit'; END IF; \
        IF current_setting('idle_in_transaction_session_timeout') <> '1min' THEN RAISE EXCEPTION 'idle limit'; END IF; \
        END $$";
    let mut session = db.admin();
    let state = schema::migrate_with(&mut session, &steps(check)).expect("the limits are as intended");

    assert_eq!(state, SchemaState::Current);
}

#[test]
fn team_a_migration_waits_for_running_writes_longer_than_the_lock_timeout_of_the_session() {
    let Some(db) = database() else { return };
    drop(db.open());

    // A write holds the migration lock shared for a while.
    let url = db.admin_url().to_owned();
    let (held, holding) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let mut client = super::support::client(&url);
        let mut tx = client.transaction().expect("transaction");
        tx.execute("SELECT pg_advisory_xact_lock_shared($1)", &[&MIGRATE_LOCK])
            .expect("lock");
        held.send(()).expect("held");
        std::thread::sleep(std::time::Duration::from_millis(900));
        tx.commit().expect("commit");
    });
    holding.recv().expect("the write holds the lock");

    let mut session = db.admin();
    session.batch_execute("SET lock_timeout = '200ms'").expect("limit");

    let state = schema::migrate_with(&mut session, &steps("SELECT 1")).expect("it waits and completes");
    writer.join().expect("writer");

    assert_eq!(state, SchemaState::Current);
}

#[test]
fn team_an_open_that_defers_leaves_a_database_that_needs_an_upgrade_alone() {
    let Some(db) = database() else { return };
    drop(db.open());

    let open = |access, upgrade| PostgresStore::open_with(db.url(), access, CONNECT_TIMEOUT, upgrade).expect("opens");

    assert!(
        matches!(open(Access::Existing, Upgrade::Defer), Opened::Ready(Some(_))),
        "current"
    );

    // A database from an older version: nothing is changed by a deferred open.
    db.admin()
        .batch_execute("UPDATE knowledge_meta SET value = '0' WHERE key = 'schema_version'")
        .expect("older");

    for access in [Access::Existing, Access::Create] {
        assert!(
            matches!(open(access, Upgrade::Defer), Opened::NeedsUpgrade),
            "{access:?}"
        );
    }

    let version: String = db
        .admin()
        .query_one("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .expect("version")
        .get(0);
    assert_eq!(version, "0", "nothing was upgraded");
}

//! Authors, usefulness counters, titles and shared chunks in PostgreSQL, and the columns that hold
//! them being added in place.

use super::support::{client, database, insert, unique, with_login};
use crate::backend::Access;
use crate::store::postgres::PostgresStore;
use crate::store::testing::{
    Seed, absent_columns_contract, authors_contract, counters_contract, shared_chunks_contract, titles_contract,
    useful_listing_contract,
};
use crate::store::{DocUsage, KnowledgeStore};

const DROP_ADDED: &str = "
ALTER TABLE knowledge_documents
    DROP COLUMN added_by_name, DROP COLUMN added_by_via, DROP COLUMN updated_by_name, DROP COLUMN updated_by_via,
    DROP COLUMN shown, DROP COLUMN useful, DROP COLUMN last_useful_at";

#[test]
fn team_authors_follow_the_contract() {
    let Some(db) = database() else { return };

    authors_contract(&mut db.open());
}

#[test]
fn team_counters_follow_the_contract() {
    let Some(db) = database() else { return };

    counters_contract(&mut db.open());
}

#[test]
fn team_the_useful_filter_and_sorts_follow_the_contract() {
    let Some(db) = database() else { return };

    useful_listing_contract(&mut db.open());
}

#[test]
fn team_titles_follow_the_contract() {
    let Some(db) = database() else { return };

    titles_contract(&mut db.open());
}

#[test]
fn team_shared_chunks_follow_the_contract() {
    let Some(db) = database() else { return };

    shared_chunks_contract(&mut db.open());
}

#[test]
fn team_a_database_without_the_columns_reads_without_the_fields_and_a_writer_adds_them() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello").by("Ada", None));
    drop(store);

    db.admin().batch_execute(DROP_ADDED).expect("drop the added columns");

    let mut reader = PostgresStore::open(db.url(), Access::Read)
        .expect("open to read")
        .expect("a database");
    absent_columns_contract(&mut reader, id);
    drop(reader);

    // The schema version is untouched by either open.
    let mut writer = PostgresStore::open(db.url(), Access::Existing)
        .expect("open to write")
        .expect("a database");
    assert_eq!(
        writer.schema_version().expect("version"),
        crate::store::postgres::schema::SCHEMA_VERSION
    );

    let found = writer.get(id, &Default::default()).expect("get").expect("document");
    assert_eq!(
        found.usage,
        Some(DocUsage {
            shown: 0,
            useful: 0,
            last_useful_at: None
        })
    );

    writer.record_shown(&[id]).expect("shown");
    assert_eq!(
        writer
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|u| u.shown),
        Some(1)
    );
}

#[test]
fn team_a_role_that_only_reads_opens_without_the_columns_and_says_it_cannot_write() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    assert_eq!(store.remote_writable(), Some(true), "the owner may update");
    drop(store);

    db.admin().batch_execute(DROP_ADDED).expect("drop the added columns");

    let role = unique("hk_reader");
    let schema = db.schema().to_owned();
    client(db.admin_url())
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD 'role-password';
             GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\";
             GRANT SELECT ON ALL TABLES IN SCHEMA \"{schema}\" TO \"{role}\""
        ))
        .expect("create the role");

    let url = with_login(db.url(), &role, "role-password");
    let outcome = std::panic::catch_unwind(|| {
        // Opening to write fails to add the columns (not the owner) and carries on without them.
        let mut store = PostgresStore::open(&url, Access::Existing)
            .expect("open")
            .expect("a database");

        absent_columns_contract(&mut store, id);
        assert_eq!(store.remote_writable(), Some(false));
    });

    let mut admin = client(db.admin_url());
    let _ = admin.batch_execute(&format!("DROP OWNED BY \"{role}\"; DROP ROLE IF EXISTS \"{role}\""));

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn team_stats_say_whether_the_remote_is_writable() {
    use crate::cli_docs::{Context, stats_reply};
    use crate::config::{Routing, Where, backend_for};
    use crate::support::Env;

    let Some(db) = database() else { return };
    let mut store = db.open();
    insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let root = tempfile::tempdir().expect("temp dir");
    let env = Env::for_tests("/home/u").with_var(crate::location::DIR_VAR, root.path().join("k").to_string_lossy());
    let remote = Routing::new(Where::Remote, Where::Remote);
    let writable = |url: &str| {
        let backend = backend_for(Some(url.to_owned()), None).expect("backend");
        let context = Context::new(&env, &backend, remote);

        stats_reply(&context, &[Some("--json")]).expect("stats").json["remoteWritable"].clone()
    };

    assert_eq!(writable(db.url()), serde_json::json!(true), "the owner");

    let role = unique("hk_reader");
    let schema = db.schema().to_owned();
    client(db.admin_url())
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD 'role-password';
             GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\";
             GRANT SELECT ON ALL TABLES IN SCHEMA \"{schema}\" TO \"{role}\""
        ))
        .expect("create the role");

    let url = with_login(db.url(), &role, "role-password");
    let outcome = std::panic::catch_unwind(|| {
        assert_eq!(writable(&url), serde_json::json!(false), "a role that only reads");
    });

    let mut admin = client(db.admin_url());
    let _ = admin.batch_execute(&format!("DROP OWNED BY \"{role}\"; DROP ROLE IF EXISTS \"{role}\""));

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The names among the added columns that the documents table of this schema has.
fn added_columns(db: &super::support::Database) -> Vec<String> {
    db.admin()
        .query(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = 'knowledge_documents'
               AND column_name IN ('added_by_name', 'shown', 'useful', 'last_useful_at')
             ORDER BY 1",
            &[&db.schema()],
        )
        .expect("columns")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

#[test]
fn team_a_counters_handle_counts_but_adds_no_column_and_upgrades_nothing() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    // Current: it counts.
    let mut counters = PostgresStore::open(db.url(), Access::Counters)
        .expect("open")
        .expect("a database");
    counters.record_shown(&[id]).expect("shown");
    counters.record_useful(&[id], 9).expect("useful");
    drop(counters);

    let usage = db.open().get(id, &Default::default()).expect("get").expect("doc").usage;
    assert_eq!(usage.map(|usage| (usage.shown, usage.useful)), Some((1, 1)));

    // Without the columns it counts nothing and does not add them.
    db.admin().batch_execute(DROP_ADDED).expect("drop the added columns");

    let mut counters = PostgresStore::open(db.url(), Access::Counters)
        .expect("open")
        .expect("a database");
    counters.record_shown(&[id]).expect("does nothing");
    drop(counters);

    assert_eq!(added_columns(&db), Vec::<String>::new(), "no DDL ran");

    // From an older version it is not counted in, and the version stays.
    db.admin()
        .batch_execute("UPDATE knowledge_meta SET value = '0' WHERE key = 'schema_version'")
        .expect("older");

    assert!(PostgresStore::open(db.url(), Access::Counters).is_err());

    let version: String = db
        .admin()
        .query_one("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
        .expect("version")
        .get(0);
    assert_eq!(version, "0", "not upgraded");
    assert_eq!(added_columns(&db), Vec::<String>::new());
}

#[test]
fn team_a_counter_at_the_largest_integer_stays_there_and_does_not_fail_the_others() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let full = insert(&mut store, &Seed::new("Full").source("/full").chunk("Intro", "hello"));
    let other = insert(&mut store, &Seed::new("Other").source("/other").chunk("Intro", "world"));

    db.admin()
        .batch_execute(&format!(
            "UPDATE knowledge_documents SET shown = 9223372036854775807, useful = 9223372036854775807 WHERE id = {full}"
        ))
        .expect("at the end");

    store.record_shown(&[full, other]).expect("shown");
    store.record_useful(&[full, other], 3).expect("useful");

    let counted = |store: &mut crate::store::postgres::PostgresStore, id| {
        let usage = store.get(id, &Default::default()).expect("get").expect("doc").usage;

        usage.map(|usage| (usage.shown, usage.useful))
    };
    assert_eq!(counted(&mut store, full), Some((i64::MAX, i64::MAX)));
    assert_eq!(counted(&mut store, other), Some((1, 1)));
}

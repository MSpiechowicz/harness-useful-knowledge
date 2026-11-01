//! What `check` finds on servers that cannot (or can) serve knowledge, see [`probe`].

use super::support::{client, database, plain_database, unique, url_of, with_database, with_login};
use crate::backend::Access;
use crate::store::KnowledgeStore;
use crate::store::postgres::vector::{Fault, Problem};
use crate::store::postgres::{PostgresStore, probe};
use crate::store::testing::Seed;

/// A database of its own on the pgvector server, dropped on drop.
struct Scratch {
    admin_url: String,
    name: String,
}

impl Scratch {
    fn new(admin_url: &str) -> Self {
        let name = unique("hk_scratch");
        client(admin_url)
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .expect("create the scratch database");

        Self {
            admin_url: admin_url.to_owned(),
            name,
        }
    }

    fn url(&self) -> String {
        with_database(&self.admin_url, &self.name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ =
            client(&self.admin_url).batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name));
    }
}

fn problem(url: &str) -> Problem {
    match probe(url) {
        Err(Fault::Problem(problem)) => problem,
        other => panic!("expected a problem: {other:?}"),
    }
}

#[test]
fn team_a_server_with_pgvector_probes_ready_and_nothing_is_created() {
    let Some(db) = database() else { return };

    let found = probe(db.url()).expect("ready");
    assert_eq!((found.schema, found.model_id), (None, None));
    assert!(db.tables().is_empty(), "a probe creates nothing");

    let mut store = db.open();
    store.set_meta("model_id", "multilingual-e5-small").expect("model");

    let found = probe(db.url()).expect("ready");
    assert_eq!(found.schema, Some(1));
    assert_eq!(found.model_id.as_deref(), Some("multilingual-e5-small"));
}

#[test]
fn team_a_vector_index_that_is_not_valid_is_still_building() {
    let Some(db) = database() else { return };
    drop(db.open());

    let mut admin = db.admin();
    let set_valid = |admin: &mut postgres::Client, valid: bool| {
        admin
            .execute(
                &format!(
                    "UPDATE pg_index SET indisvalid = {valid} WHERE indexrelid = '\"{}\".knowledge_chunks_embedding'::regclass",
                    db.schema()
                ),
                &[],
            )
            .expect("update the index state");
    };

    set_valid(&mut admin, false);
    assert_eq!(problem(db.url()), Problem::IndexBuilding);

    set_valid(&mut admin, true);
    assert!(probe(db.url()).is_ok());
}

#[test]
fn team_a_wrong_password_is_an_auth_problem_without_the_password() {
    let Some(db) = database() else { return };
    let url = with_login(db.url(), "postgres", "definitely-wrong-password");

    match probe(&url) {
        Err(Fault::Problem(found)) => {
            assert_eq!(found, Problem::Auth);
            assert!(!found.message().contains("definitely-wrong-password"));
        }
        // A server that trusts this address accepts any password, so there is nothing to refuse.
        Ok(_) => eprintln!("skipped: the server trusts this address"),
        other => panic!("expected the login to be refused: {other:?}"),
    }
}

#[test]
fn an_unreachable_server_is_a_problem_that_names_no_password() {
    let url = "postgres://someone:hunter2-secret@127.0.0.1:1/knowledge?sslmode=disable";

    let found = problem(url);
    assert_eq!(found.code(), "unreachable");
    assert!(!found.message().contains("hunter2-secret"), "{}", found.message());

    let opened = PostgresStore::open(url, Access::Read);
    match opened {
        Err(Fault::Store(error)) => assert!(!error.to_string().contains("hunter2-secret"), "{error}"),
        other => panic!("expected a failure: {other:?}"),
    }
}

#[test]
fn team_a_role_that_may_not_create_pgvector_gets_the_no_privilege_problem() {
    let Some(db) = database() else { return };
    let scratch = Scratch::new(db.admin_url());
    let role = unique("hk_role");

    // A fresh database without pgvector, and a role that may connect to it and nothing else.
    // (`public` already holds pgvector in the shared test database, but not in this one.)
    let mut admin = client(&scratch.url());
    admin
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD 'role-password';
             REVOKE ALL ON DATABASE \"{name}\" FROM PUBLIC;
             GRANT CONNECT ON DATABASE \"{name}\" TO \"{role}\";",
            name = scratch.name
        ))
        .expect("create the role");

    let url = with_login(&scratch.url(), &role, "role-password");
    let outcome = std::panic::catch_unwind(|| {
        assert_eq!(problem(&url), Problem::NoPrivilege);

        match PostgresStore::open(&url, Access::Create) {
            Err(Fault::Problem(found)) => assert_eq!(found, Problem::NoPrivilege),
            other => panic!("expected the missing right: {other:?}"),
        }

        let tables: i64 = client(&scratch.url())
            .query_one("SELECT count(*) FROM pg_tables WHERE tablename LIKE 'knowledge_%'", &[])
            .expect("tables")
            .get(0);
        assert_eq!(tables, 0, "the refused migration left nothing behind");
    });

    // The role goes after the database it holds a right on.
    drop(scratch);
    let _ = client(db.admin_url()).batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""));
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn team_creating_in_a_database_that_has_none_installs_pgvector_there() {
    let Some(db) = database() else { return };
    let scratch = Scratch::new(db.admin_url());

    // A database that was never told about pgvector: the first import creates it.
    let mut store = PostgresStore::open(&scratch.url(), Access::Create)
        .unwrap_or_else(|fault| panic!("{fault:?}"))
        .expect("created");
    let (doc, chunks, links) = Seed::new("Guide")
        .embedded("A", "x", crate::store::testing::axis(0))
        .parts();
    store.put_document(&doc, &chunks, &links).expect("put");

    let hits = store
        .vector_top_k(&crate::store::testing::axis(0), &crate::store::Scope::everything(), 5)
        .expect("vector");
    assert_eq!(hits.len(), 1);
    assert!(probe(&scratch.url()).is_ok());
}

#[test]
fn team_a_server_without_pgvector_is_the_no_pgvector_problem_and_stays_untouched() {
    let Some(db) = plain_database() else { return };

    let found = problem(db.url());
    assert_eq!(found, Problem::NoPgvector);
    assert_eq!(found.code(), "no-pgvector");

    match PostgresStore::open(db.url(), Access::Create) {
        Err(Fault::Problem(opened)) => assert_eq!(opened, Problem::NoPgvector),
        other => panic!("expected the missing extension: {other:?}"),
    }

    assert!(db.tables().is_empty(), "the refused migration left nothing behind");
    assert!(PostgresStore::open(db.url(), Access::Read).expect("read").is_none());
    assert!(
        url_of(super::support::PLAIN_URL_VAR).is_some(),
        "the variable is still set"
    );
}

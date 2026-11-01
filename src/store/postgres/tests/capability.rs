//! What `check` finds on servers that cannot (or can) serve knowledge, see [`probe`].

use super::support::{client, database, plain_database, unique, url_of, with_database, with_login};
use crate::backend::{Access, OpenError};
use crate::config::Secrets;
use crate::store::postgres::vector::{Fault, Problem, Version};
use crate::store::postgres::{CONNECT_TIMEOUT, PostgresStore, open_boxed, open_error, probe};
use crate::store::testing::Seed;
use crate::store::{KnowledgeStore, StoreError};

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

#[test]
fn a_server_that_cannot_be_reached_is_unreachable_within_the_connect_timeout_and_never_a_failure_of_the_url() {
    // Nobody listens on port 1 (refused), and 192.0.2.1 is reserved for documentation (never answers,
    // or no route). Either way the open ends at once or at the timeout, without the password.
    for url in [
        "postgres://someone:hunter2-secret@127.0.0.1:1/knowledge?sslmode=disable",
        "postgres://someone:hunter2-secret@192.0.2.1/knowledge?sslmode=require",
    ] {
        let secrets = Secrets::of(url, None);
        let started = std::time::Instant::now();

        let error = open_boxed(url, Access::Read, std::time::Duration::from_millis(500), &secrets)
            .err()
            .expect("cannot connect");

        assert!(matches!(&error, OpenError::Unreachable(_)), "{url}: {error:?}");
        assert!(!error.to_string().contains("hunter2-secret"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    // A URL the rules refuse is a failure, not an unreachable server.
    let refused = "postgres://someone@db.example.com/knowledge?sslmode=disable";
    let error = open_boxed(refused, Access::Read, CONNECT_TIMEOUT, &Secrets::of(refused, None))
        .err()
        .expect("refused");
    assert!(matches!(error, OpenError::Failed(_)), "{error:?}");
}

#[test]
fn a_missing_capability_is_told_apart_from_an_open_that_failed() {
    let url = "postgres://someone:hunter2-secret@db.example.com/knowledge";
    let secrets = Secrets::of(url, None);

    let missing = [
        (Problem::NoPgvector, "no-pgvector"),
        (Problem::NoPrivilege, "pgvector-no-privilege"),
        (Problem::TooOld(Version(0, 4, 4)), "pgvector-too-old"),
        (Problem::IndexBuilding, "index-building"),
    ];

    for (problem, code) in missing {
        let message = problem.message();

        assert_eq!(
            open_error(Fault::Problem(problem), &secrets),
            OpenError::Missing { code, message }
        );
    }

    match open_error(Fault::Problem(Problem::Auth), &secrets) {
        OpenError::Failed(message) => {
            assert!(message.starts_with("cannot open the PostgreSQL database"), "{message}")
        }
        other => panic!("not a missing capability: {other:?}"),
    }

    // A server that cannot be reached is told apart: a routed store answers from this computer meanwhile.
    for unreachable in [
        Fault::Problem(Problem::Unreachable("no route".to_owned())),
        Fault::Store(StoreError::Unreachable("no route".to_owned())),
    ] {
        match open_error(unreachable, &secrets) {
            OpenError::Unreachable(message) => {
                assert!(message.starts_with("cannot reach the PostgreSQL database"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    let leaked = Problem::Unreachable(format!("cannot reach {url}"));
    match open_error(Fault::Problem(leaked), &secrets) {
        OpenError::Unreachable(message) => assert!(!message.contains("hunter2-secret"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn team_opening_a_server_without_pgvector_is_a_missing_capability() {
    let Some(db) = plain_database() else { return };
    let secrets = Secrets::of(db.url(), None);

    for access in [Access::Create, Access::Existing, Access::Read] {
        let opened = open_boxed(db.url(), access, CONNECT_TIMEOUT, &secrets);

        match (access, opened) {
            (Access::Create, Err(OpenError::Missing { code, message })) => {
                assert_eq!(code, "no-pgvector");
                assert!(message.contains("pgvector"), "{message}");
            }
            // Nothing was created, so there is nothing to read or change.
            (_, Ok(None)) => {}
            (access, other) => panic!("{access:?}: {:?}", other.map(|_| ())),
        }
    }
}

//! Drafts and their review in PostgreSQL: the contracts every backend keeps, the schema of version 2
//! reached from a database of version 1, the rules the schema itself keeps, and the version check
//! before each read.
//!
//! Like the other `team_*` tests they run when `HARNESS_KNOWLEDGE_TEST_POSTGRES_URL` names a server.

use super::listing::everything;
use super::support::{database, insert};
use crate::backend::{Access, Opened, Upgrade};
use crate::store::postgres::schema::{self, SCHEMA_VERSION};
use crate::store::postgres::{CONNECT_TIMEOUT, Fault, PostgresStore};
use crate::store::testing::{
    Seed, authorship_contract, authorship_of_many_contract, authorship_window_contract, comments_contract,
    delete_comments_contract, discarding_a_pending_version_contract, drafts_hidden_from_agents_contract,
    files_contract, going_live_removes_the_comments_contract, largest_files_contract, local_paths_read_contract,
    local_paths_refused_contract, promote_contract, quotes_contract, rename_is_a_change_contract,
    review_transitions_contract, set_metas_contract, take_back_contract,
};
use crate::store::{
    Detail, KnowledgeStore, Link, NewComment, QuoteKind, QuoteLine, Scope, Side, Status, StoreError, Visibility,
};

#[test]
fn team_agents_never_see_a_row_that_is_not_live() {
    let Some(db) = database() else { return };

    drafts_hidden_from_agents_contract(&mut db.open());
}

#[test]
fn team_transitions_are_compare_and_set_and_recorded() {
    let Some(db) = database() else { return };

    review_transitions_contract(&mut db.open());
}

#[test]
fn team_a_promote_replaces_the_live_document_in_one_step() {
    let Some(db) = database() else { return };

    promote_contract(&mut db.open());
}

#[test]
fn team_discarding_a_pending_version_leaves_the_live_document() {
    let Some(db) = database() else { return };

    discarding_a_pending_version_contract(&mut db.open());
}

#[test]
fn team_a_rename_is_a_change_of_a_row_that_is_not_live() {
    let Some(db) = database() else { return };

    rename_is_a_change_contract(&mut db.open());
}

#[test]
fn team_everyone_who_changed_a_version_is_an_author_who_cannot_approve_it() {
    let Some(db) = database() else { return };

    authorship_contract(&mut db.open());
}

#[test]
fn team_a_writer_cannot_push_their_own_change_out_of_the_history_window() {
    let Some(db) = database() else { return };

    authorship_window_contract(&mut db.open());
}

#[test]
fn team_several_meta_keys_are_set_together() {
    let Some(db) = database() else { return };

    set_metas_contract(&mut db.open());
}

#[test]
fn team_taking_an_ask_back_is_a_compare_and_set_that_is_recorded() {
    let Some(db) = database() else { return };

    take_back_contract(&mut db.open());
}

#[test]
fn team_comments_follow_the_contract() {
    let Some(db) = database() else { return };

    comments_contract(&mut db.open());
}

#[test]
fn team_the_lines_of_a_comment_follow_the_contract() {
    let Some(db) = database() else { return };

    quotes_contract(&mut db.open());
}

#[test]
fn team_going_live_removes_the_comments() {
    let Some(db) = database() else { return };

    going_live_removes_the_comments_contract(&mut db.open());
}

#[test]
fn team_a_list_carries_authors_where_a_reviewer_needs_them() {
    let Some(db) = database() else { return };

    crate::cli_docs::review_tests::list_authors_contract(&mut db.open());
}

#[test]
fn team_a_folder_of_this_computer_is_read_as_the_global_documents() {
    let Some(db) = database() else { return };

    local_paths_read_contract(&mut db.open());
}

#[test]
fn team_the_authors_of_many_documents_are_read_together() {
    let Some(db) = database() else { return };

    authorship_of_many_contract(&mut db.open());
}

#[test]
fn team_a_pending_version_is_promoted_onto_a_live_document_only() {
    let Some(db) = database() else { return };

    crate::store::testing::promote_needs_a_live_target_contract(&mut db.open(), &mut |id| {
        db.admin()
            .batch_execute(&format!(
                "UPDATE knowledge_documents SET status = 'draft' WHERE id = {id}"
            ))
            .expect("not live");
    });
}

#[test]
fn team_a_crowd_of_writers_does_not_use_up_the_others_of_a_page() {
    let Some(db) = database() else { return };

    crate::store::testing::authorship_crowd_contract(&mut db.open());
}

#[test]
fn team_a_folder_of_this_computer_is_never_stored() {
    let Some(db) = database() else { return };

    local_paths_refused_contract(&mut db.open());
}

#[test]
fn team_the_cleanup_of_folder_links_handles_a_document_in_every_state() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let cases = crate::store::testing::folder_cases(&mut store);
    let before = crate::store::testing::folder_revisions(&mut store, &cases);
    drop(store);

    let rows: Vec<String> = cases
        .linked()
        .iter()
        .map(|id| format!("({id}, 'path:/home/x/{id}', '{{}}')"))
        .collect();
    db.admin()
        .batch_execute(&format!(
            "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES {}",
            rows.join(", ")
        ))
        .expect("rows");

    let mut writer = PostgresStore::open(db.url(), Access::Existing)
        .expect("open")
        .expect("a database");
    crate::store::testing::folder_cleanup_check(&mut writer, &cases, &before);
    assert_eq!(writer.drop_local_paths().expect("again"), 0, "idempotent");
}

#[test]
fn team_a_writer_takes_out_the_folder_links_an_earlier_version_let_in() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let seed = |title: &str, source: &str| Seed::new(title).source(source).chunk("Intro", "alpha").parts();

    let (doc, chunks, links) = seed("Linked", "/docs/linked");
    let linked = store.put_document(&doc, &chunks, &links).expect("a document");
    let (doc, chunks, links) = seed("Only folder", "/docs/only");
    let only = store.put_document(&doc, &chunks, &links).expect("a document");
    store
        .set_links(
            linked,
            &[Link {
                repo_key: "git:h/o/r".to_owned(),
                globs: Vec::new(),
            }],
        )
        .expect("a git link");
    db.admin()
        .batch_execute(&format!(
            "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES
                 ({linked}, 'path:/home/x/one', '{{}}'), ({only}, 'path:/home/x/two', '{{}}')"
        ))
        .expect("rows");
    drop(store);

    let count = |like: &str| -> i64 {
        db.admin()
            .query_one(
                "SELECT count(*) FROM knowledge_doc_links WHERE repo_key LIKE $1",
                &[&like],
            )
            .expect("count")
            .get(0)
    };
    assert_eq!(count("path:%"), 2);

    // A reader leaves them, a writer takes them out when it opens.
    assert!(PostgresStore::open(db.url(), Access::Read).expect("open").is_some());
    assert_eq!(count("path:%"), 2);

    let mut writer = PostgresStore::open(db.url(), Access::Existing)
        .expect("open")
        .expect("a database");
    assert_eq!(count("path:%"), 0);
    assert_eq!(count("git:%"), 1, "git links stay");
    // A document that only the folder linked is a draft now, out of the agents' reach, and says why;
    // it is not a global one that more people would see.
    assert!(
        writer
            .find_by_source_key("/docs/only", Visibility::Live)
            .expect("lookup")
            .is_none()
    );
    let drafted = writer
        .find_by_source_key("/docs/only", Visibility::WithDrafts)
        .expect("lookup")
        .expect("kept");
    assert_eq!((drafted.status, drafted.review_rev), (Status::Draft, 1));
    let history = writer.history(only).expect("history").expect("a history");
    assert_eq!(
        (history.events[0].kind, history.events[0].comment.as_deref()),
        (crate::store::EventKind::Edited, Some("folder link removed"))
    );
    assert_eq!(
        writer
            .find_by_source_key("/docs/linked", Visibility::Live)
            .expect("lookup")
            .expect("still live")
            .status,
        Status::Live
    );
    assert_eq!(writer.drop_local_paths().expect("again"), 0, "idempotent");
}

#[test]
fn team_deleting_comments_follows_the_contract() {
    let Some(db) = database() else { return };

    delete_comments_contract(&mut db.open());
}

#[test]
fn team_original_files_follow_the_contract() {
    let Some(db) = database() else { return };

    files_contract(&mut db.open());
}

#[test]
fn team_the_largest_files_round_trip() {
    let Some(db) = database() else { return };

    largest_files_contract(&mut db.open(), false);
    largest_files_contract(&mut db.open(), true);
}

#[test]
fn team_a_database_of_version_one_migrates_with_every_document_live() {
    let Some(db) = database() else { return };

    schema::migrate_with(&mut db.admin(), &schema::version_one()).expect("version 1");
    db.admin()
        .batch_execute(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, warnings, created_at, updated_at)
             VALUES ('Linked', 'a.md', '/a', 'markdown', 'c-a', 'f-a', 10, 1, 'user', '{}', 5, 6),
                    ('Global', 'b.md', '/b', 'markdown', 'c-b', 'f-b', 20, 1, 'agentApproved', '{}', 7, 8);
             INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (1, 'git:h/o/r', '{}');
             INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens, char_start,
                 char_end)
             VALUES (1, 0, 'Linked', 'Intro', 'upgrade words', 's-a', 2, 0, 13),
                    (2, 0, 'Global', 'Intro', 'upgrade global words', 's-b', 3, 0, 20);",
        )
        .expect("rows");

    let stored_version = || -> String {
        db.admin()
            .query_one("SELECT value FROM knowledge_meta WHERE key = 'schema_version'", &[])
            .expect("version")
            .get(0)
    };
    assert_eq!(stored_version(), "1");

    // A reader that knows version 2 waits for a writer to upgrade the database.
    match PostgresStore::open(db.url(), Access::Read) {
        Err(Fault::Store(StoreError::Backend(message))) => {
            assert!(message.contains("not been upgraded yet"), "{message}");
        }
        other => panic!(
            "expected the not-upgraded answer, got {:?}",
            other.map(|store| store.is_some())
        ),
    }

    // An open that defers leaves it alone.
    assert!(matches!(
        PostgresStore::open_with(db.url(), Access::Existing, CONNECT_TIMEOUT, Upgrade::Defer).expect("opens"),
        Opened::NeedsUpgrade
    ));
    assert_eq!(stored_version(), "1");

    let mut writer = PostgresStore::open(db.url(), Access::Existing)
        .expect("migrate")
        .expect("a database");
    assert_eq!(stored_version(), SCHEMA_VERSION.to_string());
    assert_eq!(writer.schema_version().expect("version"), SCHEMA_VERSION);

    let page = writer.list(&everything()).expect("list");
    assert_eq!(page.total, 2);
    assert!(page.items.iter().all(|row| row.doc.status == Status::Live
        && row.doc.pending_of.is_none()
        && row.doc.review_rev == 0
        && row.pending.is_none()));

    // The old rows are what agents see, and the keyword index still finds them.
    assert_eq!(
        writer
            .keyword_top_k("upgrade", &Scope::everything(), 10)
            .expect("search")
            .len(),
        2
    );
    assert!(writer.get(1, &Detail::default()).expect("get").is_some());
    assert!(
        writer
            .find_by_source_key("/a", Visibility::Live)
            .expect("find")
            .is_some()
    );

    let stats = writer.stats().expect("stats");
    assert_eq!(
        (stats.documents, stats.drafts, stats.waiting, stats.pending),
        (2, 0, 0, 0)
    );
    assert_eq!(stats.schema_version, SCHEMA_VERSION);

    // Now every other handle reads it, and the new tables are there.
    assert!(PostgresStore::open(db.url(), Access::Read).expect("open").is_some());

    for table in [
        "knowledge_doc_events",
        "knowledge_doc_comments",
        "knowledge_doc_files",
        "knowledge_doc_snapshots",
    ] {
        assert!(db.tables().iter().any(|name| name == table), "{table}");
    }

    // The upgraded database takes drafts.
    let draft = insert(
        &mut writer,
        &Seed::new("Fresh").status(Status::Draft).chunk("A", "fresh"),
    );
    assert!(writer.get(draft, &Detail::default()).expect("get").is_none());
}

#[test]
fn team_the_schema_refuses_what_the_review_forbids() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let live = insert(&mut store, &Seed::new("Live").chunk("A", "x"));
    let mut admin = db.admin();

    let mut insert_row = |status: &str, pending_of: Option<i64>| {
        admin.execute(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, warnings, created_at, updated_at, status, pending_of)
             VALUES ('T', 's.md', '/t', 'markdown', 'c', 'f', 1, 0, 'user', '{}', 1, 1, $1, $2)",
            &[&status, &pending_of],
        )
    };

    assert!(insert_row("bogus", None).is_err(), "the state is one of four");
    assert!(
        insert_row("live", Some(live)).is_err(),
        "a pending version is never live"
    );
    assert!(insert_row("draft", Some(987_654)).is_err(), "it points at a document");
    assert!(insert_row("changesRequested", None).is_ok());
    assert!(insert_row("draft", Some(live)).is_ok());
    assert!(
        insert_row("waiting", Some(live)).is_err(),
        "one pending version per live document"
    );

    // Deleting the live document takes the pending version with it.
    assert_eq!(store.delete(&[live]).expect("delete"), 1);

    let rows: i64 = db
        .admin()
        .query_one(
            "SELECT count(*) FROM knowledge_documents WHERE pending_of IS NOT NULL",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(rows, 0);
}

#[test]
fn team_a_newer_server_stops_every_read_of_a_store_that_is_already_open() {
    let Some(db) = database() else { return };
    let mut early = db.open();
    let id = insert(&mut early, &Seed::new("Before").chunk("A", "words"));

    assert!(early.get(id, &Detail::default()).expect("get").is_some());
    assert!(!early.is_newer());

    db.admin()
        .batch_execute(&format!(
            "UPDATE knowledge_meta SET value = '{}' WHERE key = 'schema_version'",
            SCHEMA_VERSION + 1
        ))
        .expect("a newer server migrated");

    assert!(matches!(
        early.get(id, &Detail::default()),
        Err(StoreError::NewerSchema)
    ));
    assert!(early.is_newer(), "and keeps refusing");
    assert!(matches!(
        early.keyword_top_k("words", &Scope::everything(), 5),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(early.list(&everything()), Err(StoreError::NewerSchema)));
    assert!(matches!(early.meta("model_id"), Err(StoreError::NewerSchema)));
    assert!(matches!(
        early.find_by_source_key("/docs/before", Visibility::Live),
        Err(StoreError::NewerSchema)
    ));
}

#[test]
fn team_every_kind_of_read_notices_a_newer_server() {
    let Some(db) = database() else { return };

    // Each read of a store that is open when a newer server migrates the database is the first to notice.
    type Read = fn(&mut PostgresStore) -> Result<(), StoreError>;
    let reads: [(&str, Read); 6] = [
        ("generation", |store| store.generation().map(drop)),
        ("stats", |store| store.stats().map(drop)),
        ("repos", |store| store.repos().map(drop)),
        ("history", |store| store.history(1).map(drop)),
        ("comments", |store| store.comments(1).map(drop)),
        ("vector", |store| {
            store
                .vector_top_k(&crate::store::testing::axis(0), &Scope::everything(), 3)
                .map(drop)
        }),
    ];

    for (name, read) in reads {
        let mut store = db.open();
        insert(&mut store, &Seed::new(name).chunk("A", "words"));
        read(&mut store).unwrap_or_else(|error| panic!("{name} at the version: {error}"));

        db.admin()
            .batch_execute(&format!(
                "UPDATE knowledge_meta SET value = '{}' WHERE key = 'schema_version'",
                SCHEMA_VERSION + 1
            ))
            .expect("a newer server migrated");
        assert!(matches!(read(&mut store), Err(StoreError::NewerSchema)), "{name}");

        db.admin()
            .batch_execute(&format!(
                "UPDATE knowledge_meta SET value = '{SCHEMA_VERSION}' WHERE key = 'schema_version'"
            ))
            .expect("back");
    }
}

#[test]
fn team_a_database_at_the_version_reads_on_every_call() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(&mut store, &Seed::new("Now").chunk("A", "words"));

    for _ in 0..3 {
        assert!(store.get(id, &Detail::default()).expect("get").is_some());
    }

    // Another server of the same version migrating the database again changes nothing.
    db.admin()
        .batch_execute(&format!(
            "UPDATE knowledge_meta SET value = '{SCHEMA_VERSION}' WHERE key = 'schema_version'"
        ))
        .expect("same version");
    assert!(store.get(id, &Detail::default()).expect("get").is_some());
    assert!(!store.is_newer());
}

#[test]
fn team_a_read_only_role_waits_for_an_upgrade_it_cannot_make() {
    use super::support::{client, unique, with_login};
    use crate::cli_docs::{Context, upgrade_reply};
    use crate::config::{Routing, Where, backend_for};
    use crate::support::Env;

    let Some(db) = database() else { return };

    schema::migrate_with(&mut db.admin(), &schema::version_one()).expect("version 1");

    let role = unique("hk_upgrade_reader");
    let name = db.schema().to_owned();
    client(db.admin_url())
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD 'role-password';
             GRANT USAGE ON SCHEMA \"{name}\" TO \"{role}\";
             GRANT SELECT ON ALL TABLES IN SCHEMA \"{name}\" TO \"{role}\""
        ))
        .expect("create the role");

    let outcome = std::panic::catch_unwind(|| {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(crate::location::DIR_VAR, root.path().join("k").to_string_lossy());
        let url = with_login(db.url(), &role, "role-password");
        let backend = backend_for(Some(url), None).expect("backend");
        let context = Context::new(&env, &backend, Routing::new(Where::Remote, Where::Remote));

        let reply = upgrade_reply(&context, &[Some("--json")]).expect("upgrade").json;
        assert_eq!(reply, serde_json::json!({ "upgraded": false, "waiting": true }));
    });

    let _ = client(db.admin_url()).batch_execute(&format!("DROP OWNED BY \"{role}\"; DROP ROLE IF EXISTS \"{role}\""));

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn team_a_database_of_version_one_upgraded_keeps_the_lines_a_comment_was_written_on() {
    let Some(db) = database() else { return };

    schema::migrate_with(&mut db.admin(), &schema::version_one()).expect("version 1");

    let mut writer = PostgresStore::open(db.url(), Access::Existing)
        .expect("migrate")
        .expect("a database");
    let line = QuoteLine {
        line: 1,
        kind: QuoteKind::Added,
        text: "first".to_owned(),
        cut: false,
    };
    let (doc, chunks, links) = Seed::new("Spec")
        .source("/spec")
        .status(Status::Draft)
        .chunk("Intro", "first")
        .parts();
    let id = writer.put_document(&doc, &chunks, &links).expect("a draft");
    writer
        .add_comments(
            id,
            Some("Bob"),
            8,
            0,
            &[
                NewComment::on_lines("Intro", Side::New, 1, 1, "with lines").with_quote(vec![line.clone()]),
                NewComment::on_lines("Intro", Side::New, 1, 1, "without"),
            ],
        )
        .expect("comments");
    let all = writer.comments(id).expect("comments");
    assert_eq!((all[0].quote.clone(), all[1].quote.clone()), (Some(vec![line]), None));
}

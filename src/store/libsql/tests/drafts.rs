//! Drafts and their review on a libSQL server: the contracts every backend keeps, the schema of
//! version 2 reached from a database of version 1, and the version check before each read.
//!
//! Like the other `team_*` cases they run when `HARNESS_KNOWLEDGE_TEST_LIBSQL_URL` names a server.

use super::super::schema;
use super::support::server;
use crate::store::libsql::{LibsqlStore, SCHEMA_VERSION};
use crate::store::testing::{
    authorship_contract, authorship_of_many_contract, authorship_window_contract, comments_contract,
    delete_comments_contract, discarding_a_pending_version_contract, drafts_hidden_from_agents_contract,
    files_contract, going_live_removes_the_comments_contract, largest_files_contract, local_paths_read_contract,
    local_paths_refused_contract, promote_contract, quotes_contract, rename_is_a_change_contract,
    review_transitions_contract, set_metas_contract, take_back_contract,
};
use crate::store::{Detail, KnowledgeStore, ListQuery, RepoFilter, Scope, Sort, Status, StatusFilter, StoreError};
use crate::store::{Link, NewComment, QuoteKind, QuoteLine, Side};
use crate::store::{Visibility, testing::Seed};

#[test]
fn team_agents_never_see_a_row_that_is_not_live() {
    let Some(server) = server() else { return };

    drafts_hidden_from_agents_contract(&mut server.store());
}

#[test]
fn team_transitions_are_compare_and_set_and_recorded() {
    let Some(server) = server() else { return };

    review_transitions_contract(&mut server.store());
}

#[test]
fn team_a_promote_replaces_the_live_document_in_one_step() {
    let Some(server) = server() else { return };

    promote_contract(&mut server.store());
}

#[test]
fn team_discarding_a_pending_version_leaves_the_live_document() {
    let Some(server) = server() else { return };

    discarding_a_pending_version_contract(&mut server.store());
}

#[test]
fn team_a_rename_is_a_change_of_a_row_that_is_not_live() {
    let Some(server) = server() else { return };

    rename_is_a_change_contract(&mut server.store());
}

#[test]
fn team_everyone_who_changed_a_version_is_an_author_who_cannot_approve_it() {
    let Some(server) = server() else { return };

    authorship_contract(&mut server.store());
}

#[test]
fn team_a_writer_cannot_push_their_own_change_out_of_the_history_window() {
    let Some(server) = server() else { return };

    authorship_window_contract(&mut server.store());
}

#[test]
fn team_several_meta_keys_are_set_together() {
    let Some(server) = server() else { return };

    set_metas_contract(&mut server.store());
}

#[test]
fn team_taking_an_ask_back_is_a_compare_and_set_that_is_recorded() {
    let Some(server) = server() else { return };

    take_back_contract(&mut server.store());
}

#[test]
fn team_comments_follow_the_contract() {
    let Some(server) = server() else { return };

    comments_contract(&mut server.store());
}

#[test]
fn team_the_lines_of_a_comment_follow_the_contract() {
    let Some(server) = server() else { return };

    quotes_contract(&mut server.store());
}

#[test]
fn team_going_live_removes_the_comments() {
    let Some(server) = server() else { return };

    going_live_removes_the_comments_contract(&mut server.store());
}

#[test]
fn team_a_list_carries_authors_where_a_reviewer_needs_them() {
    let Some(server) = server() else { return };

    crate::cli_docs::review_tests::list_authors_contract(&mut server.store());
}

#[test]
fn team_a_folder_of_this_computer_is_read_as_the_global_documents() {
    let Some(server) = server() else { return };

    local_paths_read_contract(&mut server.store());
}

#[test]
fn team_the_authors_of_many_documents_are_read_together() {
    let Some(server) = server() else { return };

    authorship_of_many_contract(&mut server.store());
}

#[test]
fn team_a_pending_version_is_promoted_onto_a_live_document_only() {
    let Some(server) = server() else { return };

    crate::store::testing::promote_needs_a_live_target_contract(&mut server.store(), &mut |id| {
        server.batch(&format!(
            "UPDATE knowledge_documents SET status = 'draft' WHERE id = {id}"
        ));
    });
}

#[test]
fn team_a_crowd_of_writers_does_not_use_up_the_others_of_a_page() {
    let Some(server) = server() else { return };

    crate::store::testing::authorship_crowd_contract(&mut server.store());
}

#[test]
fn team_a_folder_of_this_computer_is_never_stored() {
    let Some(server) = server() else { return };

    local_paths_refused_contract(&mut server.store());
}

#[test]
fn team_the_cleanup_of_folder_links_handles_a_document_in_every_state() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let cases = crate::store::testing::folder_cases(&mut store);
    let before = crate::store::testing::folder_revisions(&mut store, &cases);
    drop(store);

    let rows: Vec<String> = cases
        .linked()
        .iter()
        .map(|id| format!("({id}, 'path:/home/x/{id}', '[]')"))
        .collect();
    server.batch(&format!(
        "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES {}",
        rows.join(", ")
    ));

    let mut writer = LibsqlStore::open_existing(&server.url, server.token.as_deref())
        .expect("open")
        .expect("a database");
    crate::store::testing::folder_cleanup_check(&mut writer, &cases, &before);
    assert_eq!(writer.drop_local_paths().expect("again"), 0, "idempotent");
}

#[test]
fn team_a_writer_takes_out_the_folder_links_an_earlier_version_let_in() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
    server.batch(&format!(
        "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES
             ({linked}, 'path:/home/x/one', '[]'), ({only}, 'path:/home/x/two', '[]')"
    ));
    drop(store);

    let count = || server.integer("SELECT count(*) FROM knowledge_doc_links WHERE repo_key LIKE 'path:%'");
    assert_eq!(count(), 2);

    // A reader leaves them, a writer takes them out when it opens.
    assert!(
        LibsqlStore::open_read_only(&server.url, server.token.as_deref())
            .expect("open")
            .is_some()
    );
    assert_eq!(count(), 2);

    let mut writer = LibsqlStore::open_existing(&server.url, server.token.as_deref())
        .expect("open")
        .expect("a database");
    assert_eq!(count(), 0);
    assert_eq!(
        server.integer("SELECT count(*) FROM knowledge_doc_links WHERE repo_key = 'git:h/o/r'"),
        1,
        "git links stay"
    );
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
    let Some(server) = server() else { return };

    delete_comments_contract(&mut server.store());
}

#[test]
fn team_original_files_follow_the_contract() {
    let Some(server) = server() else { return };

    files_contract(&mut server.store());
}

#[test]
fn team_the_largest_files_round_trip() {
    let Some(server) = server() else { return };

    largest_files_contract(&mut server.store(), false);
}

fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        author: None,
        want_authors: false,
        status: StatusFilter::All,
        sort: Sort::Updated,
        descending: false,
        offset: 0,
        limit: 50,
    }
}

#[test]
fn team_a_database_of_version_one_migrates_with_every_document_live() {
    let Some(server) = server() else { return };

    schema::create_v1(&mut server.client().single()).expect("version 1");
    server.batch(
        "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256, bytes,
             chunk_count, origin, warnings, created_at, updated_at)
         VALUES ('Linked', 'a.md', '/a', 'markdown', 'c-a', 'f-a', 10, 1, 'user', '[]', 5, 6),
                ('Global', 'b.md', '/b', 'markdown', 'c-b', 'f-b', 20, 1, 'agentApproved', '[]', 7, 8);
         INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (1, 'git:h/o/r', '[]');
         INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens, char_start, char_end)
         VALUES (1, 0, 'Linked', 'Intro', 'upgrade words', 's-a', 2, 0, 13),
                (2, 0, 'Global', 'Intro', 'upgrade global words', 's-b', 3, 0, 20);",
    );

    // A reader that knows version 2 waits for a writer to upgrade the database.
    match LibsqlStore::open_read_only(&server.url, server.token.as_deref()) {
        Err(StoreError::Backend(message)) => assert!(message.contains("not been upgraded yet"), "{message}"),
        other => panic!(
            "expected the not-upgraded answer, got {:?}",
            other.map(|store| store.is_some())
        ),
    }
    assert_eq!(
        server.integer("SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'schema_version'"),
        1
    );

    let mut writer = LibsqlStore::open_existing(&server.url, server.token.as_deref())
        .expect("migrate")
        .expect("a database");
    assert_eq!(
        server.integer("SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'schema_version'"),
        SCHEMA_VERSION
    );

    let page = writer.list(&everything()).expect("list");
    assert_eq!(page.total, 2);
    assert!(
        page.items
            .iter()
            .all(|row| row.doc.status == Status::Live && row.doc.pending_of.is_none() && row.doc.review_rev == 0)
    );
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

    assert!(
        LibsqlStore::open_read_only(&server.url, server.token.as_deref())
            .expect("open")
            .is_some()
    );

    for table in [
        "knowledge_doc_events",
        "knowledge_doc_comments",
        "knowledge_doc_files",
        "knowledge_doc_snapshots",
    ] {
        assert_eq!(
            server.integer(&format!("SELECT count(*) FROM sqlite_master WHERE name = '{table}'")),
            1,
            "{table}"
        );
    }
}

#[test]
fn team_a_newer_server_stops_every_read_of_a_store_that_is_already_open() {
    let Some(server) = server() else { return };
    let mut early = server.store();
    let id = super::support::insert(&mut early, &Seed::new("Before").chunk("A", "words"));

    assert!(early.get(id, &Detail::default()).expect("get").is_some());
    assert!(!early.is_newer());

    server
        .execute(&format!(
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
}

#[test]
fn team_the_version_check_rides_with_the_read_and_a_database_at_the_version_reads() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = super::support::insert(&mut store, &Seed::new("Now").chunk("A", "words"));

    for _ in 0..3 {
        assert!(store.get(id, &Detail::default()).expect("get").is_some());
    }

    // Another server of the same version migrating the database again changes nothing.
    server
        .execute(&format!(
            "UPDATE knowledge_meta SET value = '{SCHEMA_VERSION}' WHERE key = 'schema_version'"
        ))
        .expect("same version");
    assert!(store.get(id, &Detail::default()).expect("get").is_some());
}

#[test]
fn team_a_database_of_version_one_upgraded_keeps_the_lines_a_comment_was_written_on() {
    let Some(server) = server() else { return };

    schema::create_v1(&mut server.client().single()).expect("version 1");

    let mut writer = LibsqlStore::open_existing(&server.url, server.token.as_deref())
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

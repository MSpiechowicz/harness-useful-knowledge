//! Drafts and their review in the local file: the contracts every backend keeps, and the schema
//! of version 2 reached from a file of version 1.

use rusqlite::Connection;

use crate::store::sqlite::schema::{self, SCHEMA_VERSION, user_version};
use crate::store::testing::{
    Seed, authorship_contract, authorship_of_many_contract, authorship_window_contract, comments_contract,
    delete_comments_contract, discarding_a_pending_version_contract, drafts_hidden_from_agents_contract,
    files_contract, going_live_removes_the_comments_contract, largest_files_contract, local_paths_kept_contract,
    promote_contract, quotes_contract, rename_is_a_change_contract, review_transitions_contract, set_metas_contract,
    store, take_back_contract,
};
use crate::store::{Detail, KnowledgeStore, ListQuery, RepoFilter, Scope, Sort, SqliteStore, Status, StatusFilter};
use crate::store::{NewComment, QuoteKind, QuoteLine, Side, StoreError, Visibility};

fn folder() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

#[test]
fn agents_never_see_a_row_that_is_not_live() {
    drafts_hidden_from_agents_contract(&mut store());
}

#[test]
fn transitions_are_compare_and_set_and_recorded() {
    review_transitions_contract(&mut store());
}

#[test]
fn a_promote_replaces_the_live_document_in_one_step() {
    promote_contract(&mut store());
}

#[test]
fn discarding_a_pending_version_leaves_the_live_document() {
    discarding_a_pending_version_contract(&mut store());
}

#[test]
fn a_rename_is_a_change_of_a_row_that_is_not_live() {
    rename_is_a_change_contract(&mut store());
}

#[test]
fn everyone_who_changed_a_version_is_an_author_who_cannot_approve_it() {
    authorship_contract(&mut store());
}

#[test]
fn a_writer_cannot_push_their_own_change_out_of_the_history_window() {
    authorship_window_contract(&mut store());
}

#[test]
fn several_meta_keys_are_set_together() {
    set_metas_contract(&mut store());
}

#[test]
fn taking_an_ask_back_is_a_compare_and_set_that_is_recorded() {
    take_back_contract(&mut store());
}

#[test]
fn comments_follow_the_contract() {
    comments_contract(&mut store());
}

#[test]
fn the_lines_of_a_comment_follow_the_contract() {
    quotes_contract(&mut store());
}

#[test]
fn going_live_removes_the_comments() {
    going_live_removes_the_comments_contract(&mut store());
}

#[test]
fn the_authors_of_many_documents_are_read_together() {
    authorship_of_many_contract(&mut store());
}

#[test]
fn a_pending_version_is_promoted_onto_a_live_document_only() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    let mut store = SqliteStore::open(&path).expect("open");

    crate::store::testing::promote_needs_a_live_target_contract(&mut store, &mut |id| {
        Connection::open(&path)
            .expect("open")
            .execute("UPDATE knowledge_documents SET status = 'draft' WHERE id = ?1", [id])
            .expect("not live");
    });
}

#[test]
fn a_crowd_of_writers_does_not_use_up_the_others_of_a_page() {
    crate::store::testing::authorship_crowd_contract(&mut store());
}

#[test]
fn this_computers_database_keeps_folder_links() {
    local_paths_kept_contract(&mut store());
}

#[test]
fn deleting_comments_follows_the_contract() {
    delete_comments_contract(&mut store());
}

#[test]
fn original_files_follow_the_contract() {
    files_contract(&mut store());
}

#[test]
fn the_largest_files_round_trip() {
    largest_files_contract(&mut store(), false);
    largest_files_contract(&mut store(), true);
}

/// A file as a server of version 1 left it, with a document linked to a repository and one global.
fn version_one(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open");
    conn.execute_batch("PRAGMA journal_mode = WAL").expect("wal");
    schema::create_v1(&conn).expect("version 1");

    conn.execute_batch(
        "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256, bytes,
             chunk_count, origin, warnings, created_at, updated_at)
         VALUES ('Linked', 'a.md', '/a', 'markdown', 'c-a', 'f-a', 10, 1, 'user', '[]', 5, 6),
                ('Global', 'b.md', '/b', 'markdown', 'c-b', 'f-b', 20, 1, 'agentApproved', '[]', 7, 8);
         INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (1, 'git:h/o/r', '[]');
         INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens, char_start, char_end)
         VALUES (1, 0, 'Linked', 'Intro', 'upgrade words', 's-a', 2, 0, 13),
                (2, 0, 'Global', 'Intro', 'upgrade global words', 's-b', 3, 0, 20);",
    )
    .expect("rows");
}

fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        status: StatusFilter::All,
        sort: Sort::Updated,
        descending: false,
        offset: 0,
        limit: 50,
    }
}

#[test]
fn a_file_of_version_one_migrates_with_every_document_live() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    version_one(&path);

    // A reader that knows version 2 waits for a writer to upgrade the file.
    match SqliteStore::open_read_only(&path) {
        Err(StoreError::Backend(message)) => assert!(message.contains("not been upgraded yet"), "{message}"),
        other => panic!("expected the not-upgraded answer, got {other:?}"),
    }
    assert!(matches!(SqliteStore::open_counters(&path), Err(StoreError::Backend(_))));
    assert_eq!(
        user_version(&Connection::open(&path).expect("open")).expect("version"),
        1
    );

    let mut writer = SqliteStore::open(&path).expect("migrate");
    assert_eq!(writer.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(writer.meta("schema_version").expect("meta").as_deref(), Some("2"));

    let page = writer.list(&everything()).expect("list");
    assert_eq!(page.total, 2);
    assert!(page.items.iter().all(|row| row.doc.status == Status::Live
        && row.doc.pending_of.is_none()
        && row.doc.review_rev == 0
        && row.pending.is_none()));

    // The old rows are what agents see, and the keyword index still finds them.
    let hits = writer
        .keyword_top_k("upgrade", &Scope::everything(), 10)
        .expect("search");
    assert_eq!(hits.len(), 2);
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
    assert_eq!(stats.schema_version, 2);

    // Now every other handle reads it, and the new tables are there.
    assert!(SqliteStore::open_read_only(&path).expect("open").is_some());
    let conn = Connection::open(&path).expect("open");

    for table in [
        "knowledge_doc_events",
        "knowledge_doc_comments",
        "knowledge_doc_files",
        "knowledge_doc_snapshots",
    ] {
        let found: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = ?1", [table], |row| {
                row.get(0)
            })
            .expect("table");
        assert_eq!(found, 1, "{table}");
    }
}

#[test]
fn the_schema_refuses_what_the_review_forbids() {
    let mut writer = store();
    let live = crate::store::testing::insert(&mut writer, &crate::store::testing::Seed::new("Live").chunk("A", "x"));

    let insert = |status: &str, pending_of: Option<i64>| {
        writer.conn.execute(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, warnings, created_at, updated_at, status, pending_of)
             VALUES ('T', 's.md', '/t', 'markdown', 'c', 'f', 1, 0, 'user', '[]', 1, 1, ?1, ?2)",
            rusqlite::params![status, pending_of],
        )
    };

    assert!(insert("bogus", None).is_err(), "the state is one of four");
    assert!(insert("live", Some(live)).is_err(), "a pending version is never live");
    assert!(insert("draft", Some(987_654)).is_err(), "it points at a document");
    assert!(insert("changesRequested", None).is_ok());
    assert!(insert("draft", Some(live)).is_ok());
    assert!(
        insert("waiting", Some(live)).is_err(),
        "one pending version per live document"
    );

    // Deleting the live document takes the pending version with it, chunks and index included.
    assert_eq!(writer.delete(&[live]).expect("delete"), 1);
    let rows: i64 = writer
        .conn
        .query_row(
            "SELECT count(*) FROM knowledge_documents WHERE pending_of IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(rows, 0);
}

#[test]
fn a_file_written_by_a_newer_server_stops_reads_even_while_the_store_is_open() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    let mut early = SqliteStore::open(&path).expect("open");
    let id = crate::store::testing::insert(&mut early, &crate::store::testing::Seed::new("Before").chunk("A", "x"));

    assert!(early.get(id, &Detail::default()).expect("get").is_some());

    Connection::open(&path)
        .expect("open")
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .expect("a newer server migrated");

    assert!(matches!(
        early.get(id, &Detail::default()),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(
        early.keyword_top_k("x", &Scope::everything(), 5),
        Err(StoreError::NewerSchema)
    ));
    assert!(matches!(early.list(&everything()), Err(StoreError::NewerSchema)));
    assert!(matches!(early.delete(&[id]), Err(StoreError::NewerSchema)));
}

#[test]
fn a_vector_hit_is_read_back_only_while_its_document_is_still_visible() {
    use crate::store::sqlite::search::chunk_hit;
    use crate::store::testing::{Seed, axis, insert, store};

    let mut db = store();
    let live = insert(&mut db, &Seed::new("Live").embedded("Intro", "words", axis(0)));
    let draft = insert(
        &mut db,
        &Seed::new("Draft")
            .source("/draft")
            .status(Status::Draft)
            .embedded("Intro", "words", axis(1)),
    );

    let chunk_of = |db: &mut SqliteStore, doc: i64| {
        db.keyword_top_k("words", &Scope::everything().with_drafts(), 10)
            .expect("search")
            .into_iter()
            .find(|hit| hit.doc_id == doc)
            .expect("a chunk")
            .chunk_id
    };
    let (live_chunk, draft_chunk) = (chunk_of(&mut db, live), chunk_of(&mut db, draft));

    // The scan saw both (the draft's document was visible then), the read-back of an agent's does not.
    let agents = Scope::everything();
    assert!(chunk_hit(&db.conn, live_chunk, 0.5, &agents).expect("read").is_some());
    assert!(
        chunk_hit(&db.conn, draft_chunk, 0.5, &agents).expect("read").is_none(),
        "a draft is never handed to an agent, even by the second read"
    );
    assert!(
        chunk_hit(&db.conn, draft_chunk, 0.5, &agents.clone().with_drafts())
            .expect("read")
            .is_some(),
        "the person's own scope reads it"
    );

    // A scope of one repository does not read back a chunk of a document linked to another.
    let linked = insert(
        &mut db,
        &Seed::new("Linked")
            .source("/linked")
            .embedded("Intro", "words", axis(2))
            .link("git:h/o/other", &[]),
    );
    let linked_chunk = chunk_of(&mut db, linked);
    let repo = Scope::from_repo(Some(crate::project::ProjectKey {
        key: "git:h/o/mine".to_owned(),
        root: std::path::PathBuf::new(),
        remote: None,
    }));
    assert!(chunk_hit(&db.conn, linked_chunk, 0.5, &repo).expect("read").is_none());
}

#[test]
fn a_file_of_version_one_upgraded_keeps_the_lines_a_comment_was_written_on() {
    let dir = folder();
    let path = dir.path().join("knowledge.sqlite3");
    version_one(&path);

    let mut writer = SqliteStore::open(&path).expect("migrate");
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

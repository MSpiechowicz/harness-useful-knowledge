//! Drafts and their review on MongoDB: the contracts every backend keeps, and what is the store's
//! own (the review state of the chunks, the original file in parts, the history and the snapshots).
//!
//! The search indexes are eventually consistent, so the contracts, which search right after a write,
//! run on a [`Settled`] store: before a search that follows a write it waits until both indexes have
//! caught up with it.

use std::thread;
use std::time::{Duration, Instant};

use mongodb::bson::{Bson, Document, doc};
use mongodb::sync::Database;

use super::{TestDb, WAIT, insert, store};
use crate::limits::FILE_BYTES_MAX;
use crate::store::mongo::MongoStore;
use crate::store::mongo::schema::{
    CHUNKS, COMMENTS, DOCUMENTS, EVENTS, FILE_PART_BYTES, FILES, KEYWORD_INDEX, SNAPSHOTS, VECTOR_INDEX,
};
use crate::store::testing::{
    Seed, authorship_contract, authorship_of_many_contract, authorship_window_contract, axis, comments_contract,
    delete_comments_contract, discarding_a_pending_version_contract, drafts_hidden_from_agents_contract,
    files_contract, going_live_removes_the_comments_contract, largest_files_contract, local_paths_read_contract,
    local_paths_refused_contract, promote_contract, quotes_contract, rename_is_a_change_contract,
    review_transitions_contract, set_metas_contract, take_back_contract,
};
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Comment, Detail, Document as Doc, DocumentDetail, DocumentPage, History,
    KnowledgeStore, Link, ListQuery, NewComment, PendingChunk, PendingVersion, RepoCounts, ReviewAction, ReviewChange,
    ReviewReply, Scope, SectionText, Stats, Status, StoreError, StoredFile, TitleScope, Visibility,
};

/// The model the sentinel chunks claim to be made by (no real chunk has it).
const SENTINEL_MODEL: &str = "sentinel-model";

/// The `review` of a sentinel chunk, which no search of the store ever returns.
const SENTINEL_REVIEW: &str = "sentinel";

/// A store whose searches see every write made before them.
///
/// Both search indexes read the chunks from the change stream in order, so once a chunk written
/// after a change is found by an index, that change is in it. A write marks the store dirty, and the
/// next search first writes such a sentinel chunk, waits until both indexes find it and removes it.
struct Settled {
    store: MongoStore,
    db: Database,
    dirty: bool,
    sentinels: i64,
}

impl Settled {
    fn settle(&mut self) {
        if !self.dirty {
            return;
        }

        self.dirty = false;
        self.sentinels += 1;

        let n = self.sentinels;
        let id = -1_000_000 - n;
        let word = format!("zzsentinel{n}");
        let vector: Vec<f64> = axis(7).iter().map(|value| f64::from(*value)).collect();
        let chunks = self.db.collection::<Document>(CHUNKS);

        chunks
            .insert_one(doc! {
                "_id": id, "doc_id": -1_i64, "ordinal": n, "title": "sentinel", "heading_path": "s",
                "text": &word, "review": SENTINEL_REVIEW, "model_id": SENTINEL_MODEL,
                "embedding": vector.clone(), "updated_at": 0_i64,
            })
            .run()
            .expect("insert the sentinel");

        let keyword = vec![doc! { "$search": {
            "index": KEYWORD_INDEX,
            "compound": {
                "filter": [{ "equals": { "path": "review", "value": SENTINEL_REVIEW } }],
                "must": [{ "text": { "query": &word, "path": "text" } }],
            },
        } }];
        let nearest = vec![doc! { "$vectorSearch": {
            "index": VECTOR_INDEX,
            "path": "embedding",
            "queryVector": vector,
            "numCandidates": 100_i64,
            "limit": 1_i64,
            "filter": { "model_id": SENTINEL_MODEL },
        } }];

        for (index, pipeline) in [("keyword", keyword), ("vector", nearest)] {
            let deadline = Instant::now() + WAIT;

            loop {
                let found = chunks
                    .aggregate(pipeline.clone())
                    .run()
                    .expect("search the sentinel")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("read the sentinel");

                if found.iter().any(|row| row.get_i64("_id") == Ok(id)) {
                    break;
                }

                assert!(Instant::now() < deadline, "the {index} index never caught up");
                thread::sleep(Duration::from_millis(100));
            }
        }

        chunks
            .delete_one(doc! { "_id": id })
            .run()
            .expect("remove the sentinel");
    }

    fn written<T>(&mut self, done: Result<T, StoreError>) -> Result<T, StoreError> {
        self.dirty = true;
        done
    }
}

impl KnowledgeStore for Settled {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.store.meta(key)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.store.set_meta(key, value)
    }

    fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError> {
        self.store.set_metas(entries)
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.store.generation()
    }

    fn put_document(&mut self, doc: &Doc, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let done = self.store.put_document(doc, chunks, links);
        self.written(done)
    }

    fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Doc>, StoreError> {
        self.store.find_by_content_sha(sha, visibility)
    }

    fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Doc>, StoreError> {
        self.store.find_by_source_key(key, visibility)
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<f32>>, StoreError> {
        self.store.reusable_embeddings(model_id, chunk_shas)
    }

    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        self.store.pending_chunks(model_id, after, limit)
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        let done = self.store.set_chunk_embeddings(model_id, embeddings);
        self.written(done)
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.store.record_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.store.record_useful(ids, at)
    }

    fn find_by_title(
        &mut self,
        title: &str,
        scope: &TitleScope,
        visibility: Visibility,
    ) -> Result<Vec<Doc>, StoreError> {
        self.store.find_by_title(title, scope, visibility)
    }

    fn docs_sharing_chunks(
        &mut self,
        chunk_shas: &[String],
        visibility: Visibility,
    ) -> Result<Vec<(i64, usize)>, StoreError> {
        self.store.docs_sharing_chunks(chunk_shas, visibility)
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let done = self.store.set_links(id, links);
        self.written(done)
    }

    fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError> {
        let done = self.store.rename(id, title, by, at);
        self.written(done)
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        let done = self.store.delete(ids);
        self.written(done)
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.store.list(query)
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        self.store.get(id, detail)
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.settle();
        self.store.keyword_top_k(query, scope, k)
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.settle();
        self.store.vector_top_k(vector, scope, k)
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        self.store.matching_chunks(doc_id, text, limit)
    }

    fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError> {
        self.store.pending_of(id)
    }

    fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        let done = self.store.set_review(id, expect, change);
        self.written(done)
    }

    fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        let done = self.store.promote(id, expect, change);
        self.written(done)
    }

    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError> {
        self.store.history(id)
    }

    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError> {
        self.store.writers_since_live(id)
    }

    fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        self.store.section_texts(id)
    }

    fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        self.store.asked_sections(id, revision)
    }

    fn add_comments(
        &mut self,
        id: i64,
        by: Option<&str>,
        at: i64,
        revision: i64,
        comments: &[NewComment],
    ) -> Result<Vec<i64>, StoreError> {
        self.store.add_comments(id, by, at, revision, comments)
    }

    fn reply_comment(
        &mut self,
        id: i64,
        parent: i64,
        by: Option<&str>,
        at: i64,
        body: &str,
    ) -> Result<Option<i64>, StoreError> {
        self.store.reply_comment(id, parent, by, at, body)
    }

    fn resolve_comment(
        &mut self,
        id: i64,
        comment: i64,
        resolved: bool,
        by: Option<&str>,
        at: i64,
    ) -> Result<bool, StoreError> {
        self.store.resolve_comment(id, comment, resolved, by, at)
    }

    fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError> {
        self.store.delete_comment(id, comment)
    }

    fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError> {
        self.store.comments(id)
    }

    fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        self.store.put_file(id, name, bytes)
    }

    fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError> {
        self.store.file(id)
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.store.repos()
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.store.stats()
    }
}

/// A fresh database with a [`Settled`] store on it.
fn settled() -> Option<(TestDb, Settled)> {
    let (db, store) = store()?;
    let raw = db.raw();

    Some((
        db,
        Settled {
            store,
            db: raw,
            dirty: false,
            sentinels: 0,
        },
    ))
}

#[test]
fn team_mongo_agents_never_see_a_row_that_is_not_live() {
    let Some((_db, mut store)) = settled() else { return };

    drafts_hidden_from_agents_contract(&mut store);
}

#[test]
fn team_mongo_transitions_are_compare_and_set_and_recorded() {
    let Some((_db, mut store)) = settled() else { return };

    review_transitions_contract(&mut store);
}

#[test]
fn team_mongo_a_promote_replaces_the_live_document_in_one_step() {
    let Some((_db, mut store)) = settled() else { return };

    promote_contract(&mut store);
}

#[test]
fn team_mongo_discarding_a_pending_version_leaves_the_live_document() {
    let Some((_db, mut store)) = settled() else { return };

    discarding_a_pending_version_contract(&mut store);
}

#[test]
fn team_mongo_a_rename_is_a_change_of_a_row_that_is_not_live() {
    let Some((_db, mut store)) = settled() else { return };

    rename_is_a_change_contract(&mut store);
}

#[test]
fn team_mongo_everyone_who_changed_a_version_is_an_author_who_cannot_approve_it() {
    let Some((_db, mut store)) = settled() else { return };

    authorship_contract(&mut store);
}

#[test]
fn team_mongo_a_writer_cannot_push_their_own_change_out_of_the_history_window() {
    let Some((_db, mut store)) = settled() else { return };

    authorship_window_contract(&mut store);
}

#[test]
fn team_mongo_several_meta_keys_are_set_together() {
    let Some((_db, mut store)) = settled() else { return };

    set_metas_contract(&mut store);
}

#[test]
fn team_mongo_taking_an_ask_back_is_a_compare_and_set_that_is_recorded() {
    let Some((_db, mut store)) = settled() else { return };

    take_back_contract(&mut store);
}

#[test]
fn team_mongo_comments_follow_the_contract() {
    let Some((_db, mut store)) = settled() else { return };

    comments_contract(&mut store);
}

#[test]
fn team_mongo_the_lines_of_a_comment_follow_the_contract() {
    let Some((_db, mut store)) = settled() else { return };

    quotes_contract(&mut store);
}

#[test]
fn team_mongo_going_live_removes_the_comments() {
    let Some((_db, mut store)) = settled() else { return };

    going_live_removes_the_comments_contract(&mut store);
}

#[test]
fn team_mongo_a_list_carries_authors_where_a_reviewer_needs_them() {
    let Some((_db, mut store)) = settled() else { return };

    crate::cli_docs::review_tests::list_authors_contract(&mut store);
}

#[test]
fn team_mongo_a_folder_of_this_computer_is_read_as_the_global_documents() {
    let Some((_db, mut store)) = settled() else { return };

    local_paths_read_contract(&mut store);
}

#[test]
fn team_mongo_the_authors_of_many_documents_are_read_together() {
    let Some((_db, mut store)) = settled() else { return };

    authorship_of_many_contract(&mut store);
}

#[test]
fn team_mongo_a_pending_version_is_promoted_onto_a_live_document_only() {
    let Some((_db, mut store)) = settled() else { return };

    let raw = _db.raw();

    crate::store::testing::promote_needs_a_live_target_contract(&mut store, &mut |id| {
        raw.collection::<Document>(DOCUMENTS)
            .update_one(doc! { "_id": id }, doc! { "$set": { "status": "draft" } })
            .run()
            .expect("not live");
    });
}

#[test]
fn team_mongo_a_crowd_of_writers_does_not_use_up_the_others_of_a_page() {
    let Some((_db, mut store)) = settled() else { return };

    crate::store::testing::authorship_crowd_contract(&mut store);
}

#[test]
fn team_mongo_a_folder_of_this_computer_is_never_stored() {
    let Some((_db, mut store)) = settled() else { return };

    local_paths_refused_contract(&mut store);
}

#[test]
fn team_mongo_the_cleanup_of_folder_links_handles_a_document_in_every_state() {
    let Some((db, mut store)) = settled() else { return };
    let cases = crate::store::testing::folder_cases(&mut store);
    let before = crate::store::testing::folder_revisions(&mut store, &cases);

    // As an earlier version stored them: the kept document has its git link and a folder.
    let raw = db.raw();

    for id in cases.linked() {
        let keys: Vec<String> = if id == cases.kept {
            vec!["git:h/o/r".to_owned(), format!("path:/home/x/{id}")]
        } else {
            vec![format!("path:/home/x/{id}")]
        };
        let links: Vec<Document> = keys
            .iter()
            .map(|key| doc! { "repo_key": key, "globs": Vec::<String>::new() })
            .collect();

        raw.collection::<Document>(DOCUMENTS)
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "links": links, "repo_keys": keys.clone(), "global": false } },
            )
            .run()
            .expect("document");
        raw.collection::<Document>(CHUNKS)
            .update_many(
                doc! { "doc_id": id },
                doc! { "$set": { "repo_keys": keys, "global": false } },
            )
            .run()
            .expect("chunks");
    }

    let mut writer = MongoStore::connect(&db.url).expect("open");
    crate::store::testing::folder_cleanup_check(&mut writer, &cases, &before);
    assert_eq!(writer.drop_local_paths().expect("again"), 0, "idempotent");
    assert_eq!(
        db.count(CHUNKS, doc! { "doc_id": cases.live_with_pending.1 }),
        0,
        "its chunks went too"
    );
}

#[test]
fn team_mongo_a_writer_takes_out_the_folder_links_an_earlier_version_let_in() {
    let Some((db, mut store)) = settled() else { return };
    let linked = insert(
        &mut store.store,
        &Seed::new("Linked").source("/docs/linked").chunk("Intro", "alpha"),
    );
    let only = insert(
        &mut store.store,
        &Seed::new("Only folder").source("/docs/only").chunk("Intro", "beta"),
    );

    // As an earlier version stored them.
    let raw = db.raw();
    let put_links = |id: i64, keys: &[&str]| {
        let links: Vec<Document> = keys
            .iter()
            .map(|key| doc! { "repo_key": *key, "globs": Vec::<String>::new() })
            .collect();

        raw.collection::<Document>(DOCUMENTS)
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "links": links, "repo_keys": keys.to_vec(), "global": false } },
            )
            .run()
            .expect("document");
        raw.collection::<Document>(CHUNKS)
            .update_many(
                doc! { "doc_id": id },
                doc! { "$set": { "repo_keys": keys.to_vec(), "global": false } },
            )
            .run()
            .expect("chunks");
    };
    put_links(linked, &["git:h/o/r", "path:/home/x/one"]);
    put_links(only, &["path:/home/x/two"]);

    let count = |collection: &str| db.count(collection, doc! { "repo_keys": { "$regex": "^path:" } });
    assert_eq!((count(DOCUMENTS), count(CHUNKS)), (2, 2));

    // A reader leaves them, a writer takes them out when it opens.
    drop(MongoStore::connect_read_only(&db.url).expect("read"));
    assert_eq!((count(DOCUMENTS), count(CHUNKS)), (2, 2));

    let mut writer = MongoStore::connect(&db.url).expect("open");
    assert_eq!((count(DOCUMENTS), count(CHUNKS)), (0, 0));
    assert_eq!(
        db.count(DOCUMENTS, doc! { "repo_keys": "git:h/o/r" }),
        1,
        "git links stay"
    );
    assert_eq!(db.count(CHUNKS, doc! { "repo_keys": "git:h/o/r" }), 1);
    // A document that only the folder linked is a draft now, out of the agents' reach, and says why.
    assert_eq!(
        db.count(DOCUMENTS, doc! { "_id": only, "status": "draft", "review_rev": 1_i64 }),
        1
    );
    assert_eq!(db.count(CHUNKS, doc! { "doc_id": only, "review": "draft" }), 1);
    assert_eq!(
        db.count(
            EVENTS,
            doc! { "doc_id": only, "kind": "edited", "comment": "folder link removed" }
        ),
        1
    );
    assert_eq!(db.count(DOCUMENTS, doc! { "_id": linked, "status": "live" }), 1);
    assert_eq!(writer.drop_local_paths().expect("again"), 0, "idempotent");
}

#[test]
fn team_mongo_deleting_comments_follows_the_contract() {
    let Some((_db, mut store)) = settled() else { return };

    delete_comments_contract(&mut store);
}

#[test]
fn team_mongo_original_files_follow_the_contract() {
    let Some((_db, mut store)) = settled() else { return };

    files_contract(&mut store);
}

#[test]
fn team_mongo_the_largest_files_round_trip() {
    let Some((_db, mut store)) = settled() else { return };

    largest_files_contract(&mut store, false);
}

#[test]
fn team_mongo_the_largest_pdf_round_trips() {
    let Some((_db, mut store)) = settled() else { return };

    largest_files_contract(&mut store, true);
}

#[test]
fn team_mongo_without_transactions_the_review_follows_the_same_contracts() {
    // Each contract starts from an empty database.
    let contracts: [fn(&mut dyn KnowledgeStore); 9] = [
        review_transitions_contract,
        promote_contract,
        comments_contract,
        delete_comments_contract,
        going_live_removes_the_comments_contract,
        quotes_contract,
        rename_is_a_change_contract,
        authorship_contract,
        authorship_window_contract,
    ];

    for contract in contracts {
        let Some((_db, mut store)) = settled() else { return };
        store.store.transactions = false;

        contract(&mut store);
    }
}

/// The values of `review` the chunks of the documents `ids` carry, sorted and without repeats.
fn reviews(db: &TestDb, id: i64) -> Vec<String> {
    let mut found: Vec<String> = db
        .raw()
        .collection::<Document>(CHUNKS)
        .find(doc! { "doc_id": id })
        .run()
        .expect("find")
        .map(|row| row.expect("row").get_str("review").unwrap_or("missing").to_owned())
        .collect();

    found.sort();
    found.dedup();
    found
}

fn change(action: ReviewAction, by: &str, at: i64) -> ReviewChange {
    ReviewChange::new(action, Some(by), at)
}

#[test]
fn team_mongo_the_chunks_carry_the_review_state_and_only_going_live_rewrites_them() {
    let Some((db, mut store)) = settled() else { return };

    let (doc, chunks, links) = Seed::new("Spec").chunk("A", "first").chunk("B", "second").parts();
    let live = store.put_document(&doc, &chunks, &links).expect("live");
    assert_eq!(reviews(&db, live), ["live"]);

    let (mut doc, chunks, links) = Seed::new("Draft")
        .source("/docs/draft")
        .chunk("A", "one")
        .chunk("B", "two")
        .parts();
    doc.status = Status::Draft;
    let draft = store.put_document(&doc, &chunks, &links).expect("draft");
    assert_eq!(reviews(&db, draft), ["draft"]);

    // Marks the chunks, so a rewrite would show.
    db.raw()
        .collection::<Document>(CHUNKS)
        .update_many(doc! { "doc_id": draft }, doc! { "$set": { "mark": true } })
        .run()
        .expect("mark");

    // Draft, waiting and changes requested are all `draft`: a change between them touches no chunk.
    let mut revision = 0;
    for action in [ReviewAction::Ask, ReviewAction::RequestChanges, ReviewAction::Ask] {
        let by = if action == ReviewAction::RequestChanges {
            "Bob"
        } else {
            "Ada"
        };
        let change = if action == ReviewAction::RequestChanges {
            change(action, by, 10).with_comment("again")
        } else {
            change(action, by, 10)
        };

        match store.set_review(draft, revision, &change).expect("transition") {
            ReviewReply::Done(done) => revision = done.revision,
            other => panic!("{other:?}"),
        }

        assert_eq!(reviews(&db, draft), ["draft"]);
        assert_eq!(
            db.count(CHUNKS, doc! { "doc_id": draft, "mark": true }),
            2,
            "no chunk was rewritten"
        );
    }

    // A pending version is `pending`, and goes `live` under the live id with the live links.
    let (mut pending, chunks, _) = Seed::new("Spec v2").source("/docs/spec-2").chunk("A", "new").parts();
    pending.status = Status::Draft;
    pending.pending_of = Some(live);
    let pending = store
        .put_document(
            &pending,
            &chunks,
            &[Link {
                repo_key: "git:h/o/other".to_owned(),
                globs: Vec::new(),
            }],
        )
        .expect("pending");
    assert_eq!(reviews(&db, pending), ["pending"]);

    store
        .set_links(
            live,
            &[Link {
                repo_key: "git:h/o/r".to_owned(),
                globs: Vec::new(),
            }],
        )
        .expect("link the live document");

    let expect = match store.pending_of(live).expect("pending of") {
        Some(version) => version.review_rev,
        None => panic!("no pending version"),
    };
    store
        .set_review(pending, expect, &change(ReviewAction::Ask, "Ada", 20))
        .expect("ask");
    let expect = store.pending_of(live).expect("pending of").expect("pending").review_rev;
    store
        .promote(pending, expect, &change(ReviewAction::Approve, "Bob", 21))
        .expect("promote");

    let moved: Vec<Document> = db
        .raw()
        .collection::<Document>(CHUNKS)
        .find(doc! { "doc_id": live })
        .run()
        .expect("find")
        .map(|row| row.expect("row"))
        .collect();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].get_str("review"), Ok("live"));
    assert_eq!(moved[0].get_str("title"), Ok("Spec v2"));
    assert_eq!(moved[0].get_array("repo_keys").map(Vec::len), Ok(1));
    assert_eq!(
        moved[0].get_array("repo_keys").expect("keys")[0],
        Bson::String("git:h/o/r".to_owned())
    );
    assert_eq!(moved[0].get_bool("global"), Ok(false));

    // A draft that goes live gets live chunks.
    store
        .set_review(draft, revision, &change(ReviewAction::GoLive, "Ada", 30))
        .expect("go live");
    assert_eq!(reviews(&db, draft), ["live"]);
}

#[test]
fn team_mongo_a_file_is_kept_in_parts_of_at_most_8_mib() {
    let Some((db, mut store)) = settled() else { return };
    let (doc, chunks, links) = Seed::new("Guide").chunk("A", "alpha").parts();
    let id = store.put_document(&doc, &chunks, &links).expect("document");

    let size = FILE_PART_BYTES * 2 + 1234;
    let bytes: Vec<u8> = (0..size).map(|n| (n % 247) as u8).collect();
    assert!(store.put_file(id, "guide.md", &bytes).expect("put"));

    let parts = db
        .raw()
        .collection::<Document>(FILES)
        .find(doc! { "doc_id": id })
        .sort(doc! { "part": 1 })
        .run()
        .expect("parts")
        .map(|row| row.expect("part"))
        .collect::<Vec<_>>();
    let lengths: Vec<usize> = parts
        .iter()
        .map(|part| match part.get("bytes") {
            Some(Bson::Binary(binary)) => binary.bytes.len(),
            other => panic!("{other:?}"),
        })
        .collect();

    assert_eq!(lengths, [FILE_PART_BYTES, FILE_PART_BYTES, 1234]);
    const { assert!(FILE_PART_BYTES <= 8 * 1024 * 1024) };
    assert!(u64::try_from(size).expect("size") < FILE_BYTES_MAX);
    assert_eq!(parts[0].get_str("name"), Ok("guide.md"));
    assert_eq!(parts[0].get_i64("size"), Ok(i64::try_from(size).expect("size")));
    assert!(parts[1].get("name").is_none(), "only the first part carries the header");
    assert_eq!(store.file(id).expect("file").expect("a file").bytes, bytes);

    // A shorter file leaves none of the old parts, and an empty one is still a file.
    assert!(store.put_file(id, "small.md", b"small").expect("replace"));
    assert_eq!(db.count(FILES, doc! { "doc_id": id }), 1);
    assert!(store.put_file(id, "empty.md", b"").expect("empty"));

    let empty = store.file(id).expect("file").expect("a file");
    assert_eq!((empty.name.as_str(), empty.bytes.len()), ("empty.md", 0));

    // Deleting the document deletes the file.
    store.delete(&[id]).expect("delete");
    assert_eq!(db.count(FILES, doc! {}), 0);
}

#[test]
fn team_mongo_the_history_lists_the_newest_200_and_counts_all() {
    let Some((db, mut store)) = settled() else { return };
    let (mut doc, chunks, links) = Seed::new("Spec").chunk("A", "alpha").parts();
    doc.status = Status::Draft;
    let id = store.put_document(&doc, &chunks, &links).expect("draft");

    let events: Vec<Document> = (0..250_i64)
        .map(
            |n| doc! { "_id": 10_000 + n, "doc_id": id, "kind": "edited", "by": "Ada", "at": n, "comment": Bson::Null },
        )
        .collect();
    db.raw()
        .collection::<Document>(EVENTS)
        .insert_many(events)
        .run()
        .expect("events");

    let history = store.history(id).expect("history").expect("a history");
    assert_eq!(history.total, 251, "the drafted event and 250 more");
    assert_eq!(history.events.len(), 200);
    assert_eq!(history.events[0].at, 249, "newest first");
    assert_eq!(history.events[199].at, 50);
}

#[test]
fn team_mongo_only_the_latest_20_asks_keep_their_sections() {
    let Some((db, mut store)) = settled() else { return };
    let (mut doc, chunks, links) = Seed::new("Spec").chunk("Intro", "words").parts();
    doc.status = Status::Draft;
    let id = store.put_document(&doc, &chunks, &links).expect("draft");

    // Ask, request changes, ask again: each ask is a revision with its snapshot.
    let mut revision = 0;
    let mut asked = Vec::new();

    for round in 0..22 {
        let reply = store
            .set_review(id, revision, &change(ReviewAction::Ask, "Ada", round))
            .expect("ask");
        let ReviewReply::Done(done) = reply else {
            panic!("{reply:?}")
        };
        asked.push(done.revision);

        let reply = store
            .set_review(
                id,
                done.revision,
                &change(ReviewAction::RequestChanges, "Bob", round).with_comment("more"),
            )
            .expect("request changes");
        let ReviewReply::Done(done) = reply else {
            panic!("{reply:?}")
        };
        revision = done.revision;
    }

    assert_eq!(db.count(SNAPSHOTS, doc! { "doc_id": id }), 20);
    assert_eq!(store.asked_sections(id, asked[0]).expect("snapshot"), None);
    assert_eq!(store.asked_sections(id, asked[1]).expect("snapshot"), None);
    assert!(store.asked_sections(id, asked[2]).expect("snapshot").is_some());
    assert!(store.asked_sections(id, asked[21]).expect("snapshot").is_some());
}

#[test]
fn team_mongo_deleting_a_live_document_takes_its_pending_version_and_everything_kept_for_review() {
    let Some((db, mut store)) = settled() else { return };
    let (doc, chunks, links) = Seed::new("Spec").chunk("A", "alpha").parts();
    let live = store.put_document(&doc, &chunks, &links).expect("live");
    let (mut pending, chunks, links) = Seed::new("Spec 2").source("/docs/spec-2").chunk("A", "beta").parts();
    pending.status = Status::Draft;
    pending.pending_of = Some(live);
    let pending = store.put_document(&pending, &chunks, &links).expect("pending");

    store.put_file(live, "a.md", b"a").expect("file");
    store.put_file(pending, "b.md", b"b").expect("file");
    store
        .add_comments(pending, Some("Cy"), 1, 0, &[NewComment::overall("hm")])
        .expect("comment");

    let expect = store.pending_of(live).expect("pending of").expect("pending").review_rev;
    store
        .set_review(pending, expect, &change(ReviewAction::Ask, "Ada", 5))
        .expect("ask");

    assert_eq!(store.delete(&[live]).expect("delete"), 1);

    for collection in [DOCUMENTS, CHUNKS, EVENTS, COMMENTS, FILES, SNAPSHOTS] {
        assert_eq!(db.count(collection, doc! {}), 0, "{collection} is empty");
    }
}

#[test]
fn team_mongo_a_hit_for_the_person_carries_the_state_of_its_document() {
    let Some((_db, mut store)) = settled() else { return };
    let mut docs = Vec::new();

    for (name, status) in [
        ("alpha", Status::Live),
        ("beta", Status::Draft),
        ("gamma", Status::Draft),
    ] {
        let (mut doc, chunks, links) = Seed::new(&format!("Guide {name}"))
            .source(&format!("/docs/{name}"))
            .chunk("A", "statewords here")
            .parts();
        doc.status = status;
        docs.push(store.put_document(&doc, &chunks, &links).expect("put"));
    }

    let expect = store
        .get(
            docs[2],
            &crate::store::Detail {
                drafts: true,
                ..Default::default()
            },
        )
        .expect("get")
        .expect("the draft")
        .doc
        .review_rev;
    store
        .set_review(docs[2], expect, &change(ReviewAction::Ask, "Ada", 1))
        .expect("ask");

    let hits = store
        .keyword_top_k("statewords", &Scope::everything().with_drafts(), 10)
        .expect("search");
    let mut states: Vec<(i64, Status)> = hits.iter().map(|hit| (hit.doc_id, hit.status)).collect();
    states.sort_by_key(|(id, _)| *id);

    assert_eq!(
        states,
        [
            (docs[0], Status::Live),
            (docs[1], Status::Draft),
            (docs[2], Status::Waiting)
        ]
    );

    let agent = store
        .keyword_top_k("statewords", &Scope::everything(), 10)
        .expect("search");
    assert_eq!(agent.iter().map(|hit| hit.doc_id).collect::<Vec<_>>(), [docs[0]]);
}

#[test]
fn team_mongo_a_refused_transition_writes_nothing() {
    let Some((db, mut store)) = settled() else { return };
    let (mut doc, chunks, links) = Seed::new("Spec").chunk("A", "alpha").parts();
    doc.status = Status::Draft;
    let id = store.put_document(&doc, &chunks, &links).expect("draft");

    let before = store.generation().expect("generation");
    let events = db.count(EVENTS, doc! {});

    for reply in [
        store
            .set_review(id, 9, &change(ReviewAction::Ask, "Ada", 1))
            .expect("stale"),
        store
            .set_review(id, 0, &change(ReviewAction::Approve, "Bob", 1))
            .expect("wrong state"),
        store
            .set_review(777, 0, &change(ReviewAction::Ask, "Ada", 1))
            .expect("unknown"),
    ] {
        assert!(!matches!(reply, ReviewReply::Done(_)), "{reply:?}");
    }

    assert_eq!(store.generation().expect("generation"), before);
    assert_eq!(db.count(EVENTS, doc! {}), events);
    assert_eq!(db.count(DOCUMENTS, doc! { "status": "draft", "review_rev": 0_i64 }), 1);
}

#[test]
fn team_mongo_a_comment_stored_without_the_field_of_its_lines_reads_as_having_none() {
    let Some((db, mut store)) = settled() else { return };
    let (doc, chunks, links) = Seed::new("Spec").status(Status::Draft).chunk("Intro", "alpha").parts();
    let id = store.put_document(&doc, &chunks, &links).expect("put document");

    db.raw()
        .collection::<Document>(COMMENTS)
        .insert_one(doc! {
            "_id": 900_i64, "doc_id": id, "review_id": 1_i64, "section": "Intro", "side": "new",
            "line_from": 1_i64, "line_to": 1_i64, "body": "written before the lines were kept",
            "at": 5_i64, "revision": 0_i64,
        })
        .run()
        .expect("a comment of an older server");

    let found = store.comments(id).expect("comments");
    assert_eq!((found.len(), found[0].quote.clone()), (1, None));
}

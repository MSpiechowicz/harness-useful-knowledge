//! Authors, usefulness counters, titles and shared chunks in MongoDB. The fields are schemaless,
//! so nothing is added to a database: a document an older server wrote simply lacks them.

use mongodb::bson::{Document, doc};
use mongodb::sync::Client;

use std::thread;

use super::{insert, store};
use crate::store::testing::{
    Seed, author_facet_asked_contract, author_filter_contract, author_invalid_names_contract,
    author_listing_scale_contract, authors_contract, counters_contract, shared_chunks_contract, titles_contract,
    useful_listing_contract,
};
use crate::store::{DocUsage, KnowledgeStore, StoreError};

#[test]
fn team_mongo_authors_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    authors_contract(&mut store);
}

#[test]
fn team_mongo_counters_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    counters_contract(&mut store);
}

#[test]
fn team_mongo_the_useful_filter_and_sorts_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    useful_listing_contract(&mut store);
}

#[test]
fn team_mongo_author_filter_and_sort_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    author_filter_contract(&mut store);
}

#[test]
fn team_mongo_author_names_that_are_not_names_are_never_shown() {
    let Some((_db, mut store)) = store() else { return };

    author_invalid_names_contract(&mut store);
}

#[test]
fn team_mongo_author_facet_is_read_only_when_asked() {
    let Some((_db, mut store)) = store() else { return };

    author_facet_asked_contract(&mut store);
}

#[test]
fn team_mongo_author_pages_of_a_thousand_documents_add_up() {
    let Some((_db, mut store)) = store() else { return };

    author_listing_scale_contract(&mut store);
}

#[test]
fn team_mongo_titles_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    titles_contract(&mut store);
}

#[test]
fn team_mongo_shared_chunks_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    shared_chunks_contract(&mut store);
}

#[test]
fn team_mongo_a_document_an_older_server_wrote_has_no_author_and_counts_from_zero() {
    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello").by("Ada", None));

    db.raw()
        .collection::<Document>(crate::store::mongo::schema::DOCUMENTS)
        .update_one(
            doc! { "_id": id },
            doc! { "$unset": {
                "added_by_name": "", "added_by_via": "", "updated_by_name": "", "updated_by_via": "",
                "shown": "", "useful": "", "last_useful_at": "",
            } },
        )
        .run()
        .expect("unset the fields");

    let found = store.get(id, &Default::default()).expect("get").expect("document");
    assert_eq!((found.doc.added_by, found.doc.updated_by), (None, None));
    assert_eq!(
        found.usage,
        Some(DocUsage {
            shown: 0,
            useful: 0,
            last_useful_at: None
        })
    );

    store.record_shown(&[id]).expect("shown");
    assert_eq!(
        store
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|u| u.shown),
        Some(1)
    );
}

#[test]
fn team_mongo_documents_without_counter_fields_sort_as_zero_and_are_never_shown_enough() {
    use crate::store::{ListQuery, RepoFilter, Sort, UsefulFilter};

    let Some((db, mut store)) = store() else { return };
    let old = insert(&mut store, &Seed::new("Old").chunk("1", "alpha").updated(1_000));
    let rated = insert(
        &mut store,
        &Seed::new("Rated").source("/rated").chunk("1", "beta").updated(2_000),
    );
    let fresh = insert(
        &mut store,
        &Seed::new("Fresh").source("/fresh").chunk("1", "gamma").updated(3_000),
    );

    store.record_useful(&[rated], 50).expect("useful");

    for _ in 0..UsefulFilter::SHOWN_MIN {
        store.record_shown(&[old, fresh]).expect("shown");
    }

    db.raw()
        .collection::<Document>(crate::store::mongo::schema::DOCUMENTS)
        .update_one(
            doc! { "_id": old },
            doc! { "$unset": { "shown": "", "useful": "", "last_useful_at": "" } },
        )
        .run()
        .expect("unset the fields");

    let page = |store: &mut crate::store::mongo::MongoStore, useful, sort| {
        store
            .list(&ListQuery {
                filter: RepoFilter::All,
                format: None,
                text: String::new(),
                useful,
                author: None,
                want_authors: false,
                sort,
                descending: false,
                offset: 0,
                limit: 50,
                status: crate::store::StatusFilter::All,
            })
            .expect("list")
            .items
            .iter()
            .map(|row| row.doc.id)
            .collect::<Vec<_>>()
    };

    assert_eq!(page(&mut store, Some(UsefulFilter::Ever), Sort::Updated), [rated]);
    assert_eq!(
        page(&mut store, Some(UsefulFilter::Never), Sort::Updated),
        [fresh],
        "old lost its counters, rated was found useful"
    );
    assert_eq!(page(&mut store, None, Sort::Useful), [old, fresh, rated]);
    assert_eq!(page(&mut store, None, Sort::LastUseful), [old, fresh, rated]);
}

#[test]
fn team_mongo_a_server_without_authentication_does_not_say_whether_it_is_writable() {
    let Some((_db, mut store)) = store() else { return };

    assert_eq!(store.remote_writable(), None);
}

#[test]
fn team_mongo_a_counters_handle_counts_and_brings_nothing_back() {
    use crate::backend::Access;
    use crate::config::Secrets;
    use crate::store::mongo::probe::probe;
    use crate::store::mongo::schema::{CHUNKS, IndexState, KEYWORD_INDEX};
    use crate::store::mongo::{SERVER_SELECTION_TIMEOUT, open_boxed};

    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    db.raw()
        .run_command(doc! { "dropSearchIndex": CHUNKS, "name": KEYWORD_INDEX })
        .run()
        .expect("drop the index");

    let secrets = Secrets::of(&db.url, None);
    let mut counters = open_boxed(&db.url, Access::Counters, SERVER_SELECTION_TIMEOUT, &secrets)
        .expect("open")
        .expect("a database");
    // On a busy server a counter write may give up waiting (by design) and still land after: it is never repeated.
    let shown = counters.record_shown(&[id]).is_ok();
    let useful = counters.record_useful(&[id], 9).is_ok();
    drop(counters);

    // A writer would have created the index again.
    let found = probe(&db.url).expect("probe");
    assert_eq!(found.keyword, IndexState::Missing, "no DDL ran");

    let mut reader = crate::store::mongo::MongoStore::connect_read_only(&db.url)
        .expect("open")
        .expect("exists");
    let mut counted = || {
        reader
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|usage| (usage.shown, usage.useful))
            .expect("counters")
    };

    // A write the caller gave up on may still be running on the server: wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut now = counted();

    while now != (1, 1) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        now = counted();
    }

    // Each counter is counted once at most, and surely once when its write said so.
    let (seen, helped) = now;
    assert!(seen <= 1 && helped <= 1, "counted twice: {now:?}");
    assert!(!shown || seen == 1, "a confirmed shown count is missing: {now:?}");
    assert!(!useful || helped == 1, "a confirmed useful count is missing: {now:?}");
}

#[test]
fn team_mongo_a_counter_of_another_type_or_at_the_end_is_skipped_and_the_rest_are_counted() {
    let Some((db, mut store)) = store() else { return };
    let good = insert(&mut store, &Seed::new("Good").chunk("Intro", "hello"));
    let text = insert(&mut store, &Seed::new("Text").chunk("Intro", "hello"));
    let end = insert(&mut store, &Seed::new("End").chunk("Intro", "hello"));

    let documents = db.raw().collection::<Document>(crate::store::mongo::schema::DOCUMENTS);
    documents
        .update_one(
            doc! { "_id": text },
            doc! { "$set": { "shown": "many", "useful": "many" } },
        )
        .run()
        .expect("hand edit");
    documents
        .update_one(
            doc! { "_id": end },
            doc! { "$set": { "shown": i64::MAX, "useful": i64::MAX } },
        )
        .run()
        .expect("hand edit");

    store.record_shown(&[good, text, end]).expect("the rest are counted");
    store
        .record_useful(&[good, text, end], 9)
        .expect("the rest are counted");

    let mut usage = |id| {
        store
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|usage| (usage.shown, usage.useful))
    };

    assert_eq!(usage(good), Some((1, 1)));
    assert_eq!(usage(end), Some((i64::MAX, i64::MAX)));
}

#[test]
fn a_stalled_counter_write_holds_one_gate_for_the_whole_process() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use crate::store::mongo::quality::run_gated;

    // Every store of a process counts through the gate `counting_gate` hands out, so two of them
    // meet the same one: here two callers of one gate stand for two stores.
    let gate: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let (release, stalled) = mpsc::channel::<()>();

    let first = run_gated(gate, move || {
        let _ = stalled.recv();
        Ok(())
    })
    .expect("the first write starts");

    let second = run_gated(gate, || Ok(()));
    assert!(
        matches!(second, Err(StoreError::Busy)),
        "a closed gate drops this count only: {second:?}"
    );

    release.send(()).expect("let the first end");
    first.recv().expect("answer").expect("written");

    // The gate opens as the thread ends, which is just before it answers.
    let third = run_gated(gate, || Ok(())).expect("a new write starts once the first ended");
    third.recv().expect("answer").expect("written");
}

/// Unlocks the server again when dropped, so a failing test does not leave it locked.
struct Unlock(Client);

impl Drop for Unlock {
    fn drop(&mut self) {
        let _ = self.0.database("admin").run_command(doc! { "fsyncUnlock": 1 }).run();
    }
}

#[test]
fn team_mongo_a_counter_write_the_server_does_not_get_to_ends_within_its_bound_and_opens_the_gate() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crate::store::mongo::quality::COUNT_SERVER_BUDGET;

    // The lock below stops every write on the test server: no other test runs meanwhile.
    let _alone = super::alone();
    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));

    let admin = Client::with_uri_str(&db.url).expect("client");
    admin
        .database("admin")
        .run_command(doc! { "fsync": 1, "lock": true })
        .run()
        .expect("lock the server for writes");
    let _unlock = Unlock(admin);

    let started = Instant::now();
    let early = store.record_shown(&[id]);
    assert!(
        matches!(early, Err(StoreError::Backend(_))),
        "the caller moves on: {early:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    // While the first write still waits the second is dropped (busy). Once the server gave up on it, the second runs and
    // the server gives up on that one too. It never waits longer.
    let second = Instant::now();
    let dropped = store.record_shown(&[id]);
    assert!(
        matches!(dropped, Err(StoreError::Busy | StoreError::Backend(_))) || dropped.is_ok(),
        "a write while the server is locked only drops its count: {dropped:?}"
    );
    assert!(second.elapsed() < Duration::from_secs(2), "{:?}", second.elapsed());

    // The server gives up on the write on its own, which ends the thread and opens the gate.
    let ends = Instant::now() + COUNT_SERVER_BUDGET + Duration::from_millis(500);
    while store.counting.load(Ordering::Acquire) && Instant::now() < ends {
        thread::sleep(Duration::from_millis(25));
    }

    assert!(
        !store.counting.load(Ordering::Acquire),
        "the gate is still closed after {:?}",
        started.elapsed()
    );
}

//! `docs reindex` as the store sees it: pages of pending chunks, vectors written back, nothing else
//! changed.

use std::cell::Cell;
use std::rc::Rc;

use pretty_assertions::assert_eq;

use super::*;
use crate::import::reindex::{self, BATCH, Which};
use crate::store::testing::{Seed, axis, insert};
use crate::store::{Detail, Scope};

/// The ids of every chunk of document `id`, in order.
fn chunk_ids(db: &mut SqliteStore, id: i64) -> Vec<i64> {
    let detail = Detail {
        sections: true,
        chunk: None,
    };

    db.get(id, &detail)
        .expect("get")
        .expect("document")
        .sections
        .expect("sections")
        .iter()
        .map(|section| section.chunk_id)
        .collect()
}

fn keyword_ids(db: &mut SqliteStore, word: &str) -> Vec<i64> {
    db.keyword_top_k(word, &Scope::everything(), 50)
        .expect("keyword")
        .iter()
        .map(|hit| hit.chunk_id)
        .collect()
}

#[test]
fn a_reindex_embeds_the_pending_chunks_in_place_and_a_second_pass_has_nothing_to_do() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = write(dir.path(), "a.md", &guide("Middle of a."));
    let b = write(dir.path(), "b.md", &guide("Middle of b."));
    let mut db = store();

    let events = run_import(&mut db, &manifest(&[&a, &b], &[]), None, &never);
    assert_eq!(done(&events)["embeddingPending"], 6);
    assert_eq!(db.stats().expect("stats").embedded_chunks, 0);

    let docs = all_documents(&mut db);
    let before: Vec<(i64, Vec<i64>, i64)> = docs
        .iter()
        .map(|row| (row.doc.id, chunk_ids(&mut db, row.doc.id), row.doc.updated_at))
        .collect();
    let found = keyword_ids(&mut db, "intro");
    let generation = db.generation().expect("generation");

    let mut embedder = Fake::new();
    let first = reindex::run(&mut db, &mut embedder, &Which::All, &never).expect("reindex");

    assert_eq!((first.documents, first.embedded), (2, 6));
    assert!(first.failed.is_empty() && !first.cancelled);
    assert_eq!(db.stats().expect("stats").embedded_chunks, 6);
    assert_eq!(db.stats().expect("stats").model_id.as_deref(), Some(MODEL));
    assert!(db.generation().expect("generation") > generation);

    // Nothing but the vectors changed: ids, dates and the keyword index are as they were.
    let after: Vec<(i64, Vec<i64>, i64)> = all_documents(&mut db)
        .iter()
        .map(|row| (row.doc.id, chunk_ids(&mut db, row.doc.id), row.doc.updated_at))
        .collect();
    assert_eq!(after, before);
    assert_eq!(keyword_ids(&mut db, "intro"), found);

    let again = reindex::run(&mut db, &mut embedder, &Which::All, &never).expect("reindex");
    assert_eq!((again.documents, again.embedded), (0, 0));
    assert_eq!(embedder.passages.len(), 6);

    let missing = reindex::run(&mut db, &mut embedder, &Which::Ids(vec![99]), &never).expect("reindex");
    assert_eq!(missing.failed, [(99, "document 99 not found".to_owned())]);
}

#[test]
fn a_reindex_embeds_what_an_import_would_have() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "guide.md", &guide("The middle part."));

    let mut imported = store();
    let mut at_import = Fake::new();
    run_import(&mut imported, &manifest(&[&path], &[]), Some(&mut at_import), &never);

    let mut later = store();
    run_import(&mut later, &manifest(&[&path], &[]), None, &never);
    let mut at_reindex = Fake::new();
    reindex::run(&mut later, &mut at_reindex, &Which::All, &never).expect("reindex");

    assert_eq!(at_reindex.passages, at_import.passages);
}

#[test]
fn chunks_are_embedded_in_batches_of_64() {
    let mut db = store();
    let mut seed = Seed::new("Big");

    for number in 0..150 {
        seed = seed.chunk(&format!("Part {number}"), &format!("text of part {number}"));
    }

    insert(&mut db, &seed);

    let mut embedder = Fake::new();
    let done = reindex::run(&mut db, &mut embedder, &Which::All, &never).expect("reindex");

    assert_eq!(BATCH, 64);
    assert_eq!(embedder.calls, [64, 64, 22]);
    assert_eq!((done.documents, done.embedded), (1, 150));
    assert_eq!(db.stats().expect("stats").embedded_chunks, 150);
}

#[test]
fn a_cancel_between_batches_keeps_the_batches_done_and_a_rerun_finishes() {
    let mut db = store();
    let mut seed = Seed::new("Big");

    for number in 0..150 {
        seed = seed.chunk(&format!("Part {number}"), &format!("text of part {number}"));
    }

    insert(&mut db, &seed);

    let asked = Rc::new(Cell::new(0));
    let counter = Rc::clone(&asked);
    let mut embedder = Fake::new();
    embedder.on_call = Some(Box::new(move || counter.set(counter.get() + 1)));

    // The cancel comes once the first batch has been embedded.
    let first = reindex::run(&mut db, &mut embedder, &Which::All, &|| asked.get() >= 1).expect("reindex");

    assert!(first.cancelled);
    assert_eq!((first.documents, first.embedded), (1, 64));
    assert_eq!(db.stats().expect("stats").embedded_chunks, 64);
    assert_eq!(embedder.calls, [64]);

    let rest = reindex::run(&mut db, &mut embedder, &Which::All, &never).expect("reindex");

    assert!(!rest.cancelled);
    assert_eq!(rest.embedded, 86);
    assert_eq!(db.stats().expect("stats").embedded_chunks, 150);
    assert_eq!(embedder.calls, [64, 64, 22]);
}

#[test]
fn a_new_model_makes_every_chunk_pending_and_becomes_the_databases_model() {
    let mut db = store();
    insert(
        &mut db,
        &Seed::new("Guide")
            .embedded("A", "first text", axis(2))
            .embedded("B", "second text", axis(3)),
    );
    db.set_meta("model_id", MODEL).expect("model");

    let mut same = Fake::new();
    let nothing = reindex::run(&mut db, &mut same, &Which::All, &never).expect("reindex");
    assert_eq!(nothing.embedded, 0, "the current model has nothing pending");

    let mut newer = Fake::new();
    newer.model = "next-model".to_owned();
    let switched = reindex::run(&mut db, &mut newer, &Which::All, &never).expect("reindex");

    assert_eq!((switched.documents, switched.embedded), (1, 2));
    assert_eq!(db.stats().expect("stats").model_id.as_deref(), Some("next-model"));
    assert_eq!(db.stats().expect("stats").embedded_chunks, 2);
    assert!(db.pending_chunks("next-model", 0, 10).expect("pending").is_empty());
    assert_eq!(db.pending_chunks(MODEL, 0, 10).expect("pending").len(), 2);
}

#[test]
fn naming_documents_reindexes_only_those() {
    let mut db = store();
    let wanted = insert(&mut db, &Seed::new("Wanted").chunk("A", "wanted text"));
    insert(&mut db, &Seed::new("Left").chunk("A", "left text"));

    let mut embedder = Fake::new();
    let done = reindex::run(&mut db, &mut embedder, &Which::Ids(vec![wanted]), &never).expect("reindex");

    assert_eq!((done.documents, done.embedded), (1, 1));
    assert_eq!(embedder.passages, ["Wanted › A\nwanted text"]);
    assert_eq!(db.pending_chunks(MODEL, 0, 10).expect("pending").len(), 1);
    assert!(
        db.stats().expect("stats").model_id.is_some(),
        "the first vectors record the model"
    );
}

#[test]
fn a_failing_embedder_ends_the_run_and_names_the_documents_of_the_batch() {
    let mut db = store();
    let a = insert(&mut db, &Seed::new("A").chunk("A", "a text"));
    let b = insert(&mut db, &Seed::new("B").chunk("A", "b text"));

    let mut embedder = Fake::new();
    embedder.fail = true;
    let done = reindex::run(&mut db, &mut embedder, &Which::All, &never).expect("reindex");

    assert_eq!(done.embedded, 0);
    assert_eq!(
        done.failed,
        [
            (a, "the embedder is down".to_owned()),
            (b, "the embedder is down".to_owned())
        ]
    );
    assert_eq!(db.stats().expect("stats").embedded_chunks, 0);
    assert_eq!(db.stats().expect("stats").model_id, None, "no model without a vector");
    assert_eq!(db.pending_chunks(MODEL, 0, 10).expect("pending").len(), 2);
}

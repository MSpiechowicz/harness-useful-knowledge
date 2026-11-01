//! Writes on a deployment without transactions (a standalone server): ordered, and a failed put
//! cleans up after itself. The test server is a replica set, so these tests switch the store to
//! the ordered path by hand.

use std::cell::Cell;

use mongodb::bson::doc;

use super::{insert, store};
use crate::store::mongo::ctx::Ctx;
use crate::store::mongo::documents;
use crate::store::mongo::schema::{CHUNKS, DOCUMENTS};
use crate::store::testing::Seed;
use crate::store::{Detail, KnowledgeStore};

#[test]
fn team_mongo_without_transactions_a_put_writes_the_document_then_its_chunks() {
    let Some((db, mut store)) = store() else { return };
    store.transactions = false;

    let id = insert(
        &mut store,
        &Seed::new("Guide")
            .chunk("A", "one")
            .chunk("B", "two")
            .link("git:h/o/a", &[]),
    );
    assert_eq!((db.count(DOCUMENTS, doc! {}), db.count(CHUNKS, doc! {})), (1, 2));

    let (mut again, chunks, links) = Seed::new("Guide").chunk("A", "only one").parts();
    again.id = id;
    store.put_document(&again, &chunks, &links).expect("replace");

    let detail = store.get(id, &Detail::default()).expect("get").expect("document");
    assert_eq!(detail.doc.chunk_count, 1);
    assert!(detail.links.is_empty());
    assert_eq!(db.count(CHUNKS, doc! { "doc_id": id }), 1);
    assert_eq!(store.generation().expect("generation"), 2);
}

#[test]
fn team_mongo_without_transactions_a_failed_new_document_is_removed_with_its_chunks() {
    let Some((db, mut store)) = store() else { return };
    store.transactions = false;

    // Two chunks at one position: the second insert hits the unique index, as a crash would stop
    // the write halfway. The checks of `put_document` would refuse this, so the write is made
    // directly.
    let (doc, mut chunks, _) = Seed::new("Guide").chunk("A", "one").chunk("B", "two").parts();
    chunks[1].ordinal = 0;

    let mut ctx = Ctx::new(&store.db, None);
    let id = ctx.reserve_ids("documents", 1).ok().expect("an id");
    let begun = Cell::new(false);

    let written = documents::put(&mut ctx, id, &doc, "Guide", (&chunks, &[]), &begun);
    assert!(written.is_err());
    assert!(begun.get(), "the document was written before the chunks failed");
    assert_eq!((db.count(DOCUMENTS, doc! {}), db.count(CHUNKS, doc! {})), (1, 1));

    documents::undo_put(&mut ctx, id, true);
    assert_eq!((db.count(DOCUMENTS, doc! {}), db.count(CHUNKS, doc! {})), (0, 0));
}

#[test]
fn team_mongo_without_transactions_a_failed_replace_loses_its_hash_so_the_next_import_stores_it_again() {
    let Some((db, mut store)) = store() else { return };
    store.transactions = false;

    let id = insert(
        &mut store,
        &Seed::new("Guide")
            .source("/docs/guide.md")
            .chunk("A", "one")
            .chunk("B", "two"),
    );
    let hash = store
        .find_by_source_key("/docs/guide.md")
        .expect("find")
        .expect("found")
        .content_sha256;

    let (mut doc, mut chunks, _) = Seed::new("Guide")
        .source("/docs/guide.md")
        .chunk("A", "x")
        .chunk("B", "y")
        .parts();
    chunks[1].ordinal = 0;
    doc.id = id;

    let mut ctx = Ctx::new(&store.db, None);
    let begun = Cell::new(false);
    assert!(documents::put(&mut ctx, id, &doc, "Guide", (&chunks, &[]), &begun).is_err());

    documents::undo_put(&mut ctx, id, false);

    assert_eq!(db.count(CHUNKS, doc! { "doc_id": id }), 0);
    assert!(
        store.find_by_content_sha(&hash).expect("find").is_none(),
        "the next import sees a change"
    );
    let kept = store
        .find_by_source_key("/docs/guide.md")
        .expect("find")
        .expect("the document stays");
    assert_eq!((kept.id, kept.chunk_count, kept.content_sha256.as_str()), (id, 0, ""));
}

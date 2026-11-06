//! The phrase filter of the list: matched on the server against each chunk's `phrase_text`, so no
//! match is dropped however many chunks have the words, and a phrase never spans a heading and the
//! text under it. A new database has the field from version 1.

use mongodb::bson::{Document, doc};

use super::{insert, store};
use crate::store::mongo::MongoStore;
use crate::store::mongo::schema::{CHUNKS, DOCUMENTS, META};
use crate::store::testing::Seed;
use crate::store::{DocumentPage, KnowledgeStore, ListQuery, RepoFilter, Sort};

fn filtered(store: &mut MongoStore, text: &str) -> DocumentPage {
    store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: text.to_owned(),
            useful: None,
            author: None,
            want_authors: false,
            sort: Sort::Title,
            descending: false,
            offset: 0,
            limit: 50,
            status: crate::store::StatusFilter::All,
        })
        .expect("list")
}

fn titles(page: &DocumentPage) -> Vec<&str> {
    page.items.iter().map(|row| row.doc.title.as_str()).collect()
}

#[test]
fn team_mongo_a_phrase_finds_every_document_however_many_chunks_have_its_words() {
    let Some((_db, mut store)) = store() else { return };

    // 5200 chunks have both words in the wrong order, more than the old cap on candidates, and the one
    // document that has the phrase comes after them.
    for name in ["Crowd one", "Crowd two"] {
        let mut crowd = Seed::new(name);

        for n in 0..2600 {
            crowd = crowd.chunk(&format!("Part {n}"), "beta comes before alpha here");
        }

        insert(&mut store, &crowd);
    }

    insert(&mut store, &Seed::new("Late").chunk("Only", "an alpha beta phrase"));

    let page = filtered(&mut store, "alpha beta");

    assert_eq!(titles(&page), ["Late"]);
    assert_eq!(page.total, 1);
}

#[test]
fn team_mongo_a_phrase_is_in_the_heading_or_in_the_text_and_never_across_them() {
    let Some((_db, mut store)) = store() else { return };

    insert(&mut store, &Seed::new("Across").chunk("Setup alpha", "beta gamma"));
    insert(
        &mut store,
        &Seed::new("Heading").chunk("Setup alpha beta", "unrelated words"),
    );
    insert(
        &mut store,
        &Seed::new("Text").chunk("Unrelated", "then alpha beta gamma follows"),
    );
    insert(&mut store, &Seed::new("Prefix").chunk("Unrelated", "alphabet soup"));
    insert(&mut store, &Seed::new("Inside").chunk("Unrelated", "xalpha beta"));
    insert(&mut store, &Seed::new("Letters").chunk("Unrelated", "plan a big step"));

    assert_eq!(titles(&filtered(&mut store, "alpha beta")), ["Heading", "Text"]);
    assert_eq!(titles(&filtered(&mut store, "setup alpha")), ["Across", "Heading"]);
    assert_eq!(
        titles(&filtered(&mut store, "alpha b")),
        ["Heading", "Text"],
        "the last word is a prefix"
    );
    assert_eq!(titles(&filtered(&mut store, "beta gamma")), ["Across", "Text"]);
    assert!(
        titles(&filtered(&mut store, "alpha gamma")).is_empty(),
        "words apart are no phrase"
    );
    assert_eq!(
        titles(&filtered(&mut store, "a b")),
        ["Letters"],
        "one-letter words work too"
    );
}

#[test]
fn team_mongo_a_new_database_has_the_phrase_text_and_the_title_keys_from_version_1() {
    let Some((db, mut store)) = store() else { return };

    insert(
        &mut store,
        &Seed::new("Łódź guide").chunk("Setup", "the alpha beta phrase"),
    );
    insert(&mut store, &Seed::new("Plain").chunk("Setup", "nothing here"));
    drop(store);

    let raw = db.raw();

    assert_eq!(
        raw.collection::<Document>(META)
            .find_one(doc! { "_id": "schema" })
            .run()
            .expect("find")
            .expect("schema")
            .get_i64("version"),
        Ok(2)
    );
    assert_eq!(db.count(CHUNKS, doc! {}), 2);
    assert_eq!(db.count(CHUNKS, doc! { "phrase_text": { "$exists": false } }), 0);

    let key = raw
        .collection::<Document>(DOCUMENTS)
        .find_one(doc! { "_id": 1_i64 })
        .run()
        .expect("find")
        .expect("document");
    assert_eq!(
        key.get_str("title_key").expect("key"),
        "Łódź guide".to_ascii_lowercase(),
        "ASCII letters fold, the rest stays"
    );

    let mut again = MongoStore::connect_read_only(&db.url)
        .expect("a read of version 1 opens")
        .expect("exists");
    assert_eq!(titles(&filtered(&mut again, "alpha beta")), ["Łódź guide"]);
}

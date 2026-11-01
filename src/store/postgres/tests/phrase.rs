//! A phrase of the list filter and of the matches of a document is found in the heading path or in
//! the text, never across the title, the heading and the text, and an older database is upgraded to
//! tell them apart.

use super::support::{database, insert};
use crate::backend::Access;
use crate::store::postgres::PostgresStore;
use crate::store::testing::Seed;
use crate::store::{KnowledgeStore, ListQuery, RepoFilter, Sort};

fn listed(store: &mut PostgresStore, text: &str) -> Vec<String> {
    let page = store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: text.to_owned(),
            sort: Sort::Title,
            descending: false,
            offset: 0,
            limit: 50,
        })
        .expect("list");

    page.items.into_iter().map(|row| row.doc.title).collect()
}

fn fill(store: &mut PostgresStore) -> (i64, i64) {
    // The title, the heading and the text follow each other as words, but are three fields.
    let across = insert(store, &Seed::new("Gateway").chunk("Setup alpha", "beta gamma"));
    let inside = insert(
        store,
        &Seed::new("Inside").chunk("Unrelated", "then alpha beta gamma follows"),
    );

    (across, inside)
}

#[test]
fn team_a_phrase_never_spans_the_title_the_heading_and_the_text() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let (across, inside) = fill(&mut store);

    assert!(listed(&mut store, "alpha beta").contains(&"Inside".to_owned()));
    assert!(
        !listed(&mut store, "alpha beta").contains(&"Gateway".to_owned()),
        "heading to text"
    );
    assert!(listed(&mut store, "gateway setup").is_empty(), "title to heading");
    assert_eq!(listed(&mut store, "setup alpha"), ["Gateway"], "inside the heading");
    assert_eq!(
        listed(&mut store, "beta gamma"),
        ["Gateway", "Inside"],
        "inside the text"
    );
    assert_eq!(
        listed(&mut store, "setup"),
        ["Gateway"],
        "one word is found in any field"
    );

    // The matches of one document follow the same rule.
    assert_eq!(
        store.matching_chunks(across, "alpha beta", 5).expect("matches").total,
        0
    );
    assert_eq!(
        store.matching_chunks(across, "setup alpha", 5).expect("matches").total,
        1
    );
    assert_eq!(store.matching_chunks(across, "beta gam", 5).expect("matches").total, 1);
    assert_eq!(
        store.matching_chunks(inside, "alpha beta", 5).expect("matches").total,
        1
    );
}

#[test]
fn team_an_older_database_is_upgraded_so_a_phrase_cannot_span_fields() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    fill(&mut store);
    drop(store);

    // What a version 1 database looks like: one `tsv`, no separate heading and text vectors.
    db.admin()
        .batch_execute(
            "ALTER TABLE knowledge_chunks DROP COLUMN tsv_heading, DROP COLUMN tsv_text;
             UPDATE knowledge_meta SET value = '1' WHERE key = 'schema_version';",
        )
        .expect("downgrade");

    let Err(refused) = PostgresStore::open(db.url(), Access::Read) else {
        panic!("an older database is refused to a read");
    };
    assert!(
        crate::store::StoreError::from(refused).to_string().contains("older"),
        "a read does not upgrade"
    );

    let mut upgraded = db.open();
    assert!(!listed(&mut upgraded, "alpha beta").contains(&"Gateway".to_owned()));
    assert_eq!(listed(&mut upgraded, "setup alpha"), ["Gateway"]);
}

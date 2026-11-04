//! A phrase of the list filter and of the matches of a document is found in the heading path or in
//! the text, never across the title, the heading and the text. A new database has the vectors that
//! tell them apart from version 1.

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
            useful: None,
            sort: Sort::Title,
            descending: false,
            offset: 0,
            limit: 50,
            status: crate::store::StatusFilter::All,
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
fn team_a_new_database_has_the_phrase_columns_and_their_indexes_from_version_1() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    fill(&mut store);

    assert_eq!(
        store.schema_version().expect("version"),
        crate::store::postgres::schema::SCHEMA_VERSION
    );

    let mut admin = db.admin();
    let columns: Vec<String> = admin
        .query(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = 'knowledge_chunks' AND column_name LIKE 'tsv%' ORDER BY 1",
            &[&db.schema()],
        )
        .expect("columns")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(columns, ["tsv", "tsv_heading", "tsv_text"]);

    let indexes: Vec<String> = admin
        .query(
            "SELECT indexname::text FROM pg_indexes WHERE schemaname = $1 AND indexname LIKE 'knowledge_chunks_tsv%' ORDER BY 1",
            &[&db.schema()],
        )
        .expect("indexes")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        indexes,
        [
            "knowledge_chunks_tsv",
            "knowledge_chunks_tsv_heading",
            "knowledge_chunks_tsv_text"
        ]
    );

    // A read of it works as it is.
    let mut read = PostgresStore::open(db.url(), Access::Read)
        .expect("opens")
        .expect("exists");
    assert_eq!(listed(&mut read, "setup alpha"), ["Gateway"]);
}

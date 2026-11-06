//! Authors, usefulness counters, titles and shared chunks on a libSQL server, and the columns that
//! hold them being added in place.

use super::support::{insert, server};
use crate::store::libsql::{LibsqlStore, SCHEMA_VERSION};
use crate::store::testing::{
    Seed, absent_columns_contract, author_facet_asked_contract, author_filter_contract, author_invalid_names_contract,
    author_listing_scale_contract, authors_contract, counters_contract, shared_chunks_contract, titles_contract,
    useful_listing_contract,
};
use crate::store::{DocUsage, KnowledgeStore};

const ADDED: [&str; 7] = [
    "added_by_name",
    "added_by_via",
    "updated_by_name",
    "updated_by_via",
    "shown",
    "useful",
    "last_useful_at",
];

#[test]
fn team_authors_follow_the_contract() {
    let Some(server) = server() else { return };

    authors_contract(&mut server.store());
}

#[test]
fn team_counters_follow_the_contract() {
    let Some(server) = server() else { return };

    counters_contract(&mut server.store());
}

#[test]
fn team_the_useful_filter_and_sorts_follow_the_contract() {
    let Some(server) = server() else { return };

    useful_listing_contract(&mut server.store());
}

#[test]
fn team_author_filter_and_sort_follow_the_contract() {
    let Some(server) = server() else { return };

    author_filter_contract(&mut server.store());
}

#[test]
fn team_author_names_that_are_not_names_are_never_shown() {
    let Some(server) = server() else { return };

    author_invalid_names_contract(&mut server.store());
}

#[test]
fn team_author_facet_is_read_only_when_asked() {
    let Some(server) = server() else { return };

    author_facet_asked_contract(&mut server.store());
}

#[test]
fn team_author_pages_of_a_thousand_documents_add_up() {
    let Some(server) = server() else { return };

    author_listing_scale_contract(&mut server.store());
}

#[test]
fn team_titles_follow_the_contract() {
    let Some(server) = server() else { return };

    titles_contract(&mut server.store());
}

#[test]
fn team_shared_chunks_follow_the_contract() {
    let Some(server) = server() else { return };

    shared_chunks_contract(&mut server.store());
}

#[test]
fn team_a_database_without_the_columns_reads_without_the_fields_and_a_writer_adds_them() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello").by("Ada", None));
    assert_eq!(store.remote_writable(), Some(true));
    drop(store);

    for name in ADDED {
        server
            .execute(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
            .expect("drop a column");
    }

    let mut reader = LibsqlStore::open_read_only(&server.url, server.token.as_deref())
        .expect("open to read")
        .expect("a database");
    absent_columns_contract(&mut reader, id);
    assert_eq!(reader.remote_writable(), None, "a handle opened to read does not know");
    drop(reader);

    // The schema version is untouched by either open.
    let mut writer = LibsqlStore::open_existing(&server.url, server.token.as_deref())
        .expect("open to write")
        .expect("a database");
    assert_eq!(
        server.integer("SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'schema_version'"),
        SCHEMA_VERSION
    );

    let found = writer.get(id, &Default::default()).expect("get").expect("document");
    assert_eq!(
        found.usage,
        Some(DocUsage {
            shown: 0,
            useful: 0,
            last_useful_at: None
        })
    );

    writer.record_shown(&[id]).expect("shown");
    assert_eq!(
        writer
            .get(id, &Default::default())
            .expect("get")
            .expect("document")
            .usage
            .map(|u| u.shown),
        Some(1)
    );
}

#[test]
fn team_a_counters_handle_counts_but_adds_no_column() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));
    drop(store);

    let mut counters = LibsqlStore::open_counters(&server.url, server.token.as_deref())
        .expect("open")
        .expect("a database");
    counters.record_shown(&[id]).expect("shown");
    counters.record_useful(&[id], 9).expect("useful");
    drop(counters);

    let mut reader = LibsqlStore::open_read_only(&server.url, server.token.as_deref())
        .expect("open")
        .expect("a database");
    let usage = reader.get(id, &Default::default()).expect("get").expect("doc").usage;
    assert_eq!(usage.map(|usage| (usage.shown, usage.useful)), Some((1, 1)));
    drop(reader);

    for name in ADDED {
        server
            .execute(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
            .expect("drop a column");
    }

    let mut counters = LibsqlStore::open_counters(&server.url, server.token.as_deref())
        .expect("open")
        .expect("a database");
    counters.record_shown(&[id]).expect("does nothing");
    drop(counters);

    let added = server.integer(
        "SELECT count(*) FROM pragma_table_info('knowledge_documents') WHERE name IN ('shown', 'useful', 'added_by_name')",
    );
    assert_eq!(added, 0, "no DDL ran");
}

#[test]
fn team_a_counter_at_the_largest_integer_stays_there_and_a_hand_edit_reads() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = insert(&mut store, &Seed::new("Guide").chunk("Intro", "hello"));

    server
        .execute("UPDATE knowledge_documents SET shown = 9223372036854775807")
        .expect("at the end");
    store.record_shown(&[id]).expect("shown");

    let usage = store.get(id, &Default::default()).expect("get").expect("doc").usage;
    assert_eq!(usage.map(|usage| usage.shown), Some(i64::MAX));

    server
        .execute("UPDATE knowledge_documents SET shown = 'many', added_by_name = x'00ff'")
        .expect("hand edits");

    let found = store.get(id, &Default::default()).expect("the row reads").expect("doc");
    assert_eq!(found.usage.map(|usage| usage.shown), Some(0));
    assert_eq!(found.doc.added_by, None);
}

/// A name that is not valid UTF-8, or has a NUL in it, is no known author. The server cannot send
/// text that is not valid UTF-8, so the facet reads the bytes of a name as hex. The page is cut to a
/// row the server can send: the one that is by Ada.
#[test]
fn team_a_name_that_is_not_valid_text_or_hides_bytes_is_no_known_author_in_the_facet() {
    use crate::store::{AuthorFilter, ListQuery, RepoFilter, Sort, StatusFilter};

    let Some(server) = server() else { return };
    let mut store = server.store();

    for title in ["Fine", "Nul", "Broken", "Lead"] {
        insert(
            &mut store,
            &Seed::new(title)
                .source(&format!("/{title}"))
                .chunk("1", "a")
                .by("x", None),
        );
    }

    server
        .execute("UPDATE knowledge_documents SET added_by_name = 'Ada' WHERE title = 'Fine'")
        .expect("write the name");
    server
        .execute("UPDATE knowledge_documents SET added_by_name = CAST(x'416461007461696C' AS TEXT) WHERE title = 'Nul'")
        .expect("write the name");
    server
        .execute("UPDATE knowledge_documents SET added_by_name = CAST(x'41FF' AS TEXT) WHERE title = 'Broken'")
        .expect("write the name");
    server
        .execute("UPDATE knowledge_documents SET added_by_name = CAST(x'C080808080' AS TEXT) WHERE title = 'Lead'")
        .expect("write the name");

    let list = |store: &mut crate::store::libsql::LibsqlStore, author: Option<AuthorFilter>| {
        store
            .list(&ListQuery {
                filter: RepoFilter::All,
                format: None,
                text: String::new(),
                useful: None,
                author,
                want_authors: true,
                status: StatusFilter::All,
                sort: Sort::Updated,
                descending: false,
                offset: 0,
                limit: 1,
            })
            .expect("list")
    };

    let page = list(&mut store, None);
    assert_eq!(page.total, 4);
    assert_eq!((page.authors.people(), page.authors.unknown()), (vec![("Ada", 1)], 3));

    let page = list(&mut store, Some(AuthorFilter::Named("Ada".to_owned())));
    assert_eq!((page.total, page.authors.people()), (1, vec![("Ada", 1)]));
}

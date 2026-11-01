use std::collections::HashSet;

use pretty_assertions::assert_eq;

use super::support::{Server, insert, server};
use crate::store::libsql::LibsqlStore;
use crate::store::libsql::sql::Value;
use crate::store::testing::{MODEL, Seed, axis};
use crate::store::{DocumentPage, KnowledgeStore, ListQuery, RepoFilter, Sort};

/// The query that lists every document, newest first, 50 to a page.
pub(super) fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        sort: Sort::Updated,
        descending: true,
        offset: 0,
        limit: 50,
    }
}

fn titles(page: &DocumentPage) -> Vec<&str> {
    page.items.iter().map(|row| row.doc.title.as_str()).collect()
}

/// 1500 documents: titles `Doc 0000` to `Doc 1499` in a scrambled order, sizes cycling through
/// 100 values, every third a PDF, every fifth linked to one of two repositories. Written
/// directly in a few requests: a thousand puts would take minutes over HTTP.
fn big_store(server: &Server) -> LibsqlStore {
    let store = server.store();
    let mut documents: Vec<Vec<Value>> = Vec::new();
    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut links: Vec<Vec<Value>> = Vec::new();

    for n in 0..1500_i64 {
        let scrambled = (n * 7919) % 1500;
        let id = n + 1;
        let title = format!("Doc {scrambled:04}");

        documents.push(vec![
            id.into(),
            title.clone().into(),
            format!("/docs/{scrambled}").into(),
            (if n % 3 == 0 { "pdf" } else { "markdown" }).into(),
            (100 + (n % 100) * 10).into(),
            (10_000 + n).into(),
        ]);
        chunks.push(vec![
            id.into(),
            title.into(),
            format!("body of document {scrambled}").into(),
        ]);

        if n % 5 == 0 {
            links.push(vec![
                id.into(),
                (if n % 10 == 0 { "git:h/o/a" } else { "git:h/o/b" }).into(),
            ]);
        }
    }

    server.insert_many(
        "INSERT INTO knowledge_documents (id, title, source_name, source_key, format, content_sha256, file_sha256,
             bytes, chunk_count, origin, created_at, updated_at)
         VALUES (?1, ?2, 'source.md', ?3, ?4, 'c', 'f', ?5, 1, 'user', ?6, ?6)",
        &documents,
    );
    server.insert_many(
        "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens, char_start, char_end)
         VALUES (?1, 0, ?2, 'Body', ?3, 's', 4, 0, 10)",
        &chunks,
    );
    server.insert_many(
        "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (?1, ?2, '[]')",
        &links,
    );

    store
}

#[test]
fn team_a_list_of_1500_documents_pages_without_gaps_or_repeats() {
    let Some(server) = server() else { return };
    let mut store = big_store(&server);
    let mut seen: HashSet<i64> = HashSet::new();
    let mut offset = 0;

    loop {
        let page = store
            .list(&ListQuery {
                sort: Sort::Title,
                descending: false,
                offset,
                limit: 200,
                ..everything()
            })
            .expect("list");

        assert_eq!(page.total, 1500);
        assert!(page.items.len() <= 200);

        if page.items.is_empty() {
            break;
        }

        for row in &page.items {
            assert!(seen.insert(row.doc.id), "document {} appeared twice", row.doc.id);
        }

        offset += page.items.len();
    }

    assert_eq!(seen.len(), 1500);
}

#[test]
fn team_sorting_by_title_updated_and_size_runs_both_ways_and_ties_go_by_id() {
    let Some(server) = server() else { return };
    let mut store = big_store(&server);

    let mut by = |sort, descending| {
        store
            .list(&ListQuery {
                sort,
                descending,
                limit: 200,
                offset: 300,
                ..everything()
            })
            .expect("list")
    };

    let title_up = by(Sort::Title, false);
    assert_eq!(titles(&title_up)[0], "Doc 0300");
    assert_eq!(titles(&title_up)[199], "Doc 0499");

    let title_down = by(Sort::Title, true);
    assert_eq!(titles(&title_down)[0], "Doc 1199");

    let updated_down = by(Sort::Updated, true);
    let times: Vec<i64> = updated_down.items.iter().map(|row| row.doc.updated_at).collect();
    assert_eq!(times[0], 10_000 + 1499 - 300);
    assert!(times.windows(2).all(|pair| pair[0] > pair[1]));

    let size_up = by(Sort::Size, false);
    let sizes: Vec<u64> = size_up.items.iter().map(|row| row.doc.bytes).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] <= pair[1]));

    // Within one size the older id comes first going up and last going down.
    let ids: Vec<i64> = size_up.items.iter().map(|row| row.doc.id).collect();
    for (pair, size) in ids.windows(2).zip(sizes.windows(2)) {
        if size[0] == size[1] {
            assert!(pair[0] < pair[1]);
        }
    }

    let size_down = by(Sort::Size, true);
    let sizes: Vec<u64> = size_down.items.iter().map(|row| row.doc.bytes).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]));
}

#[test]
fn team_filters_by_repository_format_and_text_combine_and_the_total_follows_them() {
    let Some(server) = server() else { return };
    let mut store = big_store(&server);

    let repo_a = store
        .list(&ListQuery {
            filter: RepoFilter::Repo("git:h/o/a".to_owned()),
            ..everything()
        })
        .expect("list");
    assert_eq!(repo_a.total, 150);
    assert!(repo_a.items.iter().all(|row| row.links[0].repo_key == "git:h/o/a"));

    let global = store
        .list(&ListQuery {
            filter: RepoFilter::Global,
            ..everything()
        })
        .expect("list");
    assert_eq!(global.total, 1200);
    assert!(global.items.iter().all(|row| row.links.is_empty()));

    let pdf = store
        .list(&ListQuery {
            format: Some("pdf".to_owned()),
            ..everything()
        })
        .expect("list");
    assert_eq!(pdf.total, 500);

    let both = store
        .list(&ListQuery {
            filter: RepoFilter::Repo("git:h/o/a".to_owned()),
            format: Some("pdf".to_owned()),
            ..everything()
        })
        .expect("list");
    assert_eq!(
        both.total, 50,
        "every fifth and every third is every fifteenth, half of them in a"
    );

    let one = store
        .list(&ListQuery {
            text: "doc 0042".to_owned(),
            ..everything()
        })
        .expect("list");
    assert_eq!(titles(&one), ["Doc 0042"]);
}

#[test]
fn team_the_text_filter_reads_titles_and_chunk_text_and_wildcards_are_plain_characters() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    insert(
        &mut store,
        &Seed::new("Gateway guide").chunk("Intro", "routing of requests"),
    );
    insert(
        &mut store,
        &Seed::new("Handbook").chunk("Billing", "invoices are monthly"),
    );
    insert(&mut store, &Seed::new("100% coverage").chunk("A", "tests"));
    insert(&mut store, &Seed::new("snake_case").chunk("A", "naming"));
    insert(&mut store, &Seed::new("snakeXcase").chunk("A", "naming"));

    let mut text = |text: &str| {
        let page = store
            .list(&ListQuery {
                text: text.to_owned(),
                sort: Sort::Title,
                descending: false,
                ..everything()
            })
            .expect("list");
        page.items.into_iter().map(|row| row.doc.title).collect::<Vec<_>>()
    };

    assert_eq!(text("gate"), ["Gateway guide"], "a word prefix in the title");
    assert_eq!(
        text("REQUESTS routing"),
        ["Gateway guide"],
        "all words, any case, in the text"
    );
    assert_eq!(text("invoice"), ["Handbook"], "a prefix of a word in the text");
    assert_eq!(text("ateway"), ["Gateway guide"], "a substring of the title");
    assert_eq!(
        text("gateway invoices"),
        Vec::<String>::new(),
        "words of two documents do not match"
    );
    assert_eq!(text("%"), ["100% coverage"]);
    assert_eq!(text("e_c"), ["snake_case"], "an underscore is not a wildcard");
    assert_eq!(text("\"( OR *"), Vec::<String>::new(), "query syntax is plain text");
}

#[test]
fn team_a_page_past_the_end_is_empty_with_the_total() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    insert(&mut store, &Seed::new("Only").chunk("A", "x"));

    let page = store
        .list(&ListQuery {
            offset: 50,
            ..everything()
        })
        .expect("list");
    assert_eq!((page.items.len(), page.total), (0, 1));
}

#[test]
fn team_counts_per_repository_and_overall() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    insert(
        &mut store,
        &Seed::new("A").chunk("h", "x").embedded("h", "y", axis(0)).bytes(10),
    );
    insert(
        &mut store,
        &Seed::new("B").chunk("h", "x").link("git:h/o/a", &[]).bytes(20),
    );
    insert(
        &mut store,
        &Seed::new("C")
            .chunk("h", "x")
            .link("git:h/o/a", &[])
            .link("git:h/o/b", &[])
            .bytes(30),
    );
    store.set_meta("model_id", MODEL).expect("model");

    let repos = store.repos().expect("repos");
    assert_eq!(repos.global, 1);
    assert_eq!(repos.repos, [("git:h/o/a".to_owned(), 2), ("git:h/o/b".to_owned(), 1)]);

    let stats = store.stats().expect("stats");
    assert_eq!(
        (stats.documents, stats.chunks, stats.embedded_chunks, stats.bytes),
        (3, 4, 1, 60)
    );
    assert_eq!((stats.global_documents, stats.repositories), (1, 2));
    assert_eq!(stats.model_id.as_deref(), Some(MODEL));
    assert_eq!(stats.schema_version, 1);
}

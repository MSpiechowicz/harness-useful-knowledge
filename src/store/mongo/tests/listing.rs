use std::collections::HashSet;
use std::thread;

use pretty_assertions::assert_eq;

use super::{TestDb, insert, store};
use crate::store::mongo::MongoStore;
use crate::store::testing::{Seed, axis};
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
/// 100 values, every third a PDF, every fifth linked to one of two repositories. Four writers,
/// each with a connection of its own, store them at once (which also makes them race for the
/// counters and the generation).
fn big_store() -> Option<(TestDb, MongoStore)> {
    const WRITERS: i64 = 4;

    let (db, store) = store()?;

    thread::scope(|scope| {
        for writer in 0..WRITERS {
            let url = db.url.clone();

            scope.spawn(move || {
                let mut store = MongoStore::connect(&url).expect("connect");

                for n in (writer..1500).step_by(WRITERS as usize) {
                    let scrambled = (n * 7919) % 1500;
                    let mut seed = Seed::new(&format!("Doc {scrambled:04}"))
                        .source(&format!("/docs/{scrambled}"))
                        .updated(10_000 + n)
                        .bytes(u64::try_from(100 + (n % 100) * 10).expect("size"))
                        .format(if n % 3 == 0 { "pdf" } else { "markdown" })
                        .chunk("Body", &format!("body of document {scrambled}"));

                    if n % 5 == 0 {
                        seed = seed.link(if n % 10 == 0 { "git:h/o/a" } else { "git:h/o/b" }, &[]);
                    }

                    insert(&mut store, &seed);
                }
            });
        }
    });

    Some((db, store))
}

#[test]
fn team_mongo_a_list_of_1500_documents_pages_sorts_and_filters() {
    let Some((_db, mut store)) = big_store() else { return };

    pages_without_gaps_or_repeats(&mut store);
    sorts_both_ways_and_ties_go_by_id(&mut store);
    filters_combine_and_the_total_follows_them(&mut store);
}

fn pages_without_gaps_or_repeats(store: &mut MongoStore) {
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

fn sorts_both_ways_and_ties_go_by_id(store: &mut MongoStore) {
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

    // Within one size the older id comes first going up.
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

fn filters_combine_and_the_total_follows_them(store: &mut MongoStore) {
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
fn team_mongo_the_text_filter_reads_titles_and_chunk_text_and_wildcards_are_plain_characters() {
    let Some((_db, mut store)) = store() else { return };
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
    insert(&mut store, &Seed::new("Politique").chunk("Sécurité", "Café protégé"));

    let text = |store: &mut MongoStore, text: &str| {
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

    assert_eq!(
        text(&mut store, "gate"),
        ["Gateway guide"],
        "a word prefix in the title"
    );
    assert_eq!(
        text(&mut store, "ROUTING of Requ"),
        ["Gateway guide"],
        "the words in order, any case, the last as a prefix"
    );
    assert_eq!(
        text(&mut store, "guide routing"),
        Vec::<String>::new(),
        "a title word and a text word are no phrase"
    );
    assert_eq!(
        text(&mut store, "invoice"),
        ["Handbook"],
        "a prefix of a word in the text"
    );
    assert_eq!(
        text(&mut store, "ateway"),
        ["Gateway guide"],
        "a substring of the title"
    );
    assert_eq!(
        text(&mut store, "gateway invoices"),
        Vec::<String>::new(),
        "words of two documents do not match"
    );
    assert_eq!(text(&mut store, "%"), ["100% coverage"]);
    assert_eq!(
        text(&mut store, "e_c"),
        ["snake_case"],
        "an underscore is not a wildcard"
    );
    assert_eq!(
        text(&mut store, "\"( OR *"),
        Vec::<String>::new(),
        "query syntax is plain text"
    );
    assert_eq!(
        text(&mut store, ".*"),
        Vec::<String>::new(),
        "a regular expression is plain text"
    );
    assert_eq!(text(&mut store, "cafe PROTEGE"), ["Politique"], "accents do not matter");
}

#[test]
fn team_mongo_the_text_filter_is_a_phrase_and_the_chunks_of_a_document_that_match_are_listed() {
    let Some((_db, mut store)) = store() else { return };
    let releases = insert(
        &mut store,
        &Seed::new("Releases")
            .chunk("Version notes", "Upgrade to version 1.2 today")
            .chunk("Other", "version two, then section 10 and 1")
            .chunk("Later", "and version 10 after that"),
    );
    insert(
        &mut store,
        &Seed::new("Scattered").chunk("Notes", "the version is stable, see section 10 and 1"),
    );

    let text = |store: &mut MongoStore, text: &str| {
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

    assert_eq!(
        text(&mut store, "version 1"),
        ["Releases"],
        "next to each other, not scattered"
    );
    assert_eq!(text(&mut store, "version"), ["Releases", "Scattered"]);
    assert_eq!(text(&mut store, "1 version"), Vec::<String>::new());

    let found = store.matching_chunks(releases, "version 1", 10).expect("matches");
    assert_eq!(found.total, 2);
    assert_eq!(
        found.chunks.iter().map(|chunk| chunk.ordinal).collect::<Vec<_>>(),
        [0, 2]
    );
    assert_eq!(found.chunks[0].heading_path, "Version notes");

    let titled = store.matching_chunks(releases, "releases", 10).expect("matches");
    assert_eq!(titled.total, 0, "the title is in every chunk, so it does not count");
}

#[test]
fn team_mongo_a_page_past_the_end_is_empty_with_the_total() {
    let Some((_db, mut store)) = store() else { return };
    insert(&mut store, &Seed::new("Only").chunk("A", "x"));

    let page = store
        .list(&ListQuery {
            offset: 50,
            ..everything()
        })
        .expect("list");
    assert_eq!((page.items.len(), page.total), (0, 1));

    let none = store
        .list(&ListQuery {
            limit: 0,
            ..everything()
        })
        .expect("list");
    assert_eq!((none.items.len(), none.total), (0, 1));
}

#[test]
fn team_mongo_counts_per_repository_and_overall() {
    let Some((_db, mut store)) = store() else { return };
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
    store.set_meta("model_id", crate::store::testing::MODEL).expect("model");

    let repos = store.repos().expect("repos");
    assert_eq!(repos.global, 1);
    assert_eq!(repos.repos, [("git:h/o/a".to_owned(), 2), ("git:h/o/b".to_owned(), 1)]);

    let stats = store.stats().expect("stats");
    assert_eq!(
        (stats.documents, stats.chunks, stats.embedded_chunks, stats.bytes),
        (3, 4, 1, 60)
    );
    assert_eq!((stats.global_documents, stats.repositories), (1, 2));
    assert_eq!(stats.model_id.as_deref(), Some(crate::store::testing::MODEL));
    assert_eq!(stats.schema_version, crate::store::mongo::schema::SCHEMA_VERSION);
}

#[test]
fn team_mongo_an_empty_database_lists_and_counts_nothing() {
    let Some((_db, mut store)) = store() else { return };

    let page = store.list(&everything()).expect("list");
    assert_eq!((page.items.len(), page.total), (0, 0));

    let repos = store.repos().expect("repos");
    assert_eq!((repos.global, repos.repos.len()), (0, 0));

    let stats = store.stats().expect("stats");
    assert_eq!(
        (stats.documents, stats.chunks, stats.bytes, stats.repositories),
        (0, 0, 0, 0)
    );
    assert_eq!(stats.model_id, None);
}

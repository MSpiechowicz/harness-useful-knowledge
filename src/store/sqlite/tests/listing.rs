use std::collections::HashSet;

use pretty_assertions::assert_eq;

use crate::store::testing::{Seed, insert, store};
use crate::store::{DocumentPage, KnowledgeStore, ListQuery, RepoFilter, Sort, SqliteStore};

/// The query that lists every document, newest first, 50 to a page.
pub(super) fn everything() -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
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
/// 100 values, every third a PDF, every fifth linked to one of two repositories.
fn big_store() -> SqliteStore {
    let mut store = store();

    for n in 0..1500_i64 {
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

    store
}

#[test]
fn a_list_of_1500_documents_pages_without_gaps_or_repeats() {
    let mut store = big_store();
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
fn sorting_by_title_updated_and_size_runs_both_ways_and_ties_go_by_id() {
    let mut store = big_store();

    let by = |store: &mut SqliteStore, sort, descending| {
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

    let title_up = by(&mut store, Sort::Title, false);
    assert_eq!(titles(&title_up)[0], "Doc 0300");
    assert_eq!(titles(&title_up)[199], "Doc 0499");

    let title_down = by(&mut store, Sort::Title, true);
    assert_eq!(titles(&title_down)[0], "Doc 1199");

    let updated_down = by(&mut store, Sort::Updated, true);
    let times: Vec<i64> = updated_down.items.iter().map(|row| row.doc.updated_at).collect();
    assert_eq!(times[0], 10_000 + 1499 - 300);
    assert!(times.windows(2).all(|pair| pair[0] > pair[1]));

    let size_up = by(&mut store, Sort::Size, false);
    let sizes: Vec<u64> = size_up.items.iter().map(|row| row.doc.bytes).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] <= pair[1]));

    // Within one size the older id comes first going up and last going down.
    let ids: Vec<i64> = size_up.items.iter().map(|row| row.doc.id).collect();
    for (pair, size) in ids.windows(2).zip(sizes.windows(2)) {
        if size[0] == size[1] {
            assert!(pair[0] < pair[1]);
        }
    }

    let size_down = by(&mut store, Sort::Size, true);
    let sizes: Vec<u64> = size_down.items.iter().map(|row| row.doc.bytes).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]));
}

#[test]
fn filters_by_repository_format_and_text_combine_and_the_total_follows_them() {
    let mut store = big_store();

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
            useful: None,
            ..everything()
        })
        .expect("list");
    assert_eq!(titles(&one), ["Doc 0042"]);
}

#[test]
fn the_text_filter_reads_titles_and_chunk_text_and_wildcards_are_plain_characters() {
    let mut store = store();
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

    let text = |store: &mut SqliteStore, text: &str| {
        let page = store
            .list(&ListQuery {
                text: text.to_owned(),
                useful: None,
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
        text(&mut store, "requests routing"),
        Vec::<String>::new(),
        "the same words in another order are no phrase"
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
}

#[test]
fn the_text_filter_is_a_phrase_whose_last_word_is_a_prefix() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Releases")
            .chunk("Notes", "Upgrade to version 1.2 today")
            .chunk("More", "and version 10 later"),
    );
    insert(
        &mut store,
        &Seed::new("Scattered").chunk("Notes", "the version is stable, see section 10 and 1"),
    );
    insert(&mut store, &Seed::new("Single").chunk("Notes", "versions"));

    let found = |store: &mut SqliteStore, text: &str| {
        let page = store
            .list(&ListQuery {
                text: text.to_owned(),
                useful: None,
                sort: Sort::Title,
                descending: false,
                ..everything()
            })
            .expect("list");
        page.items.into_iter().map(|row| row.doc.title).collect::<Vec<_>>()
    };

    assert_eq!(
        found(&mut store, "version 1"),
        ["Releases"],
        "next to each other, not scattered"
    );
    assert_eq!(found(&mut store, "Version 1.2"), ["Releases"]);
    assert_eq!(found(&mut store, "version 10"), ["Releases"]);
    assert_eq!(
        found(&mut store, "version"),
        ["Releases", "Scattered", "Single"],
        "one word is a prefix"
    );
    assert_eq!(found(&mut store, "1 version"), Vec::<String>::new(), "order matters");
    assert_eq!(
        found(&mut store, "elease"),
        ["Releases"],
        "the title stays a substring match"
    );
}

#[test]
fn the_chunks_that_match_are_the_ones_of_the_list_filter_by_position() {
    let mut store = store();
    let id = insert(
        &mut store,
        &Seed::new("Releases")
            .chunk("Intro", "nothing to see")
            .chunk("Version notes", "Upgrade to version 1.2 today")
            .chunk("Other", "version two, then section 10 and 1")
            .chunk("Later", "and version 10 after that"),
    );
    let other = insert(&mut store, &Seed::new("Other doc").chunk("Notes", "version 1 here too"));

    let matches = |store: &mut SqliteStore, doc: i64, text: &str, limit: usize| {
        let found = store.matching_chunks(doc, text, limit).expect("matches");
        (
            found.total,
            found.chunks.into_iter().map(|chunk| chunk.ordinal).collect::<Vec<_>>(),
        )
    };

    assert_eq!(matches(&mut store, id, "version 1", 10), (2, vec![1, 3]));
    assert_eq!(matches(&mut store, id, "VERSION 1.2", 10), (1, vec![1]));
    assert_eq!(
        matches(&mut store, id, "version", 2),
        (3, vec![1, 2]),
        "the total counts past the limit"
    );
    assert_eq!(
        matches(&mut store, id, "notes", 10),
        (1, vec![1]),
        "the heading path counts"
    );
    assert_eq!(
        matches(&mut store, id, "releases", 10),
        (0, vec![]),
        "the title is in every chunk, so it does not count"
    );
    assert_eq!(matches(&mut store, id, " ?! ", 10), (0, vec![]));
    assert_eq!(
        matches(&mut store, id, "\"( OR *", 10),
        (0, vec![]),
        "query syntax is plain text"
    );
    assert_eq!(matches(&mut store, other, "version 1", 10), (1, vec![0]));
    assert_eq!(matches(&mut store, 9999, "version", 10), (0, vec![]));

    let first = store
        .matching_chunks(id, "version 1", 1)
        .expect("matches")
        .chunks
        .remove(0);
    assert_eq!(first.heading_path, "Version notes");
    assert!(first.text.contains("1.2"));
}

#[test]
fn a_page_past_the_end_is_empty_with_the_total() {
    let mut store = store();
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
fn counts_per_repository_and_overall() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("A")
            .chunk("h", "x")
            .embedded("h", "y", crate::store::testing::axis(0))
            .bytes(10),
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
    assert_eq!(stats.schema_version, 1);
}

#[test]
fn sorting_by_format_chunks_and_repositories_keeps_each_column_in_order() {
    let mut store = big_store();

    let by = |store: &mut SqliteStore, sort, descending| {
        store
            .list(&ListQuery {
                sort,
                descending,
                limit: 200,
                offset: 0,
                ..everything()
            })
            .expect("list")
    };

    let formats: Vec<String> = by(&mut store, Sort::Format, false)
        .items
        .iter()
        .map(|row| row.doc.format.clone())
        .collect();
    assert!(formats.windows(2).all(|pair| pair[0] <= pair[1]));

    let chunks: Vec<usize> = by(&mut store, Sort::Chunks, true)
        .items
        .iter()
        .map(|row| row.doc.chunk_count)
        .collect();
    assert!(chunks.windows(2).all(|pair| pair[0] >= pair[1]));

    let first_repo = |row: &crate::store::DocumentRow| {
        row.links
            .iter()
            .map(|link| link.repo_key.clone())
            .min()
            .unwrap_or_default()
    };
    let repos: Vec<String> = by(&mut store, Sort::Repos, false)
        .items
        .iter()
        .map(first_repo)
        .collect();
    assert!(repos.windows(2).all(|pair| pair[0] <= pair[1]), "{repos:?}");
}

//! What every backend answers about authors, usefulness counters, titles and shared chunks (the
//! same checks run on each, on an empty database).

use super::Seed;
use crate::limits::SHARED_SHAS_MAX;
use crate::store::{Author, Detail, DocUsage, Document, KnowledgeStore, ListQuery, RepoFilter, Sort, StoreError};
use crate::store::{AuthorFilter, DocumentPage, DocumentRow, StatusFilter, TitleScope, UsefulFilter};

fn author(name: &str, via: Option<&str>) -> Option<Author> {
    Some(Author {
        name: name.to_owned(),
        via: via.map(str::to_owned),
    })
}

fn put(store: &mut dyn KnowledgeStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

fn read(store: &mut dyn KnowledgeStore, id: i64) -> crate::store::DocumentDetail {
    store
        .get(id, &Detail::default())
        .expect("get")
        .expect("the document exists")
}

/// Every document, in the order they were stored.
pub fn rows(store: &mut dyn KnowledgeStore) -> Vec<DocumentRow> {
    store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            author: None,
            want_authors: false,
            sort: Sort::Updated,
            descending: false,
            offset: 0,
            limit: 50,
            status: crate::store::StatusFilter::All,
        })
        .expect("list")
        .items
}

/// One page of every document, kept by `useful` and sorted.
fn page(
    store: &mut dyn KnowledgeStore,
    useful: Option<UsefulFilter>,
    sort: Sort,
    descending: bool,
    (offset, limit): (usize, usize),
) -> DocumentPage {
    store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            useful,
            author: None,
            want_authors: false,
            sort,
            descending,
            offset,
            limit,
            status: crate::store::StatusFilter::All,
        })
        .expect("list")
}

/// The ids of a page, in its order.
fn ids_of(page: &DocumentPage) -> Vec<i64> {
    page.items.iter().map(|row| row.doc.id).collect()
}

/// `--useful` keeps the documents found useful, or shown at least [`UsefulFilter::SHOWN_MIN`] times
/// and never found useful, the sorts by usefulness order by the counter (0 for a document nobody rated) and break ties by id in the
/// direction of the sort, and all of it pages and counts.
pub fn useful_listing_contract(store: &mut dyn KnowledgeStore) {
    let a = put(store, &Seed::new("A").source("/a").chunk("1", "alpha").updated(1_000));
    let b = put(store, &Seed::new("B").source("/b").chunk("1", "beta").updated(2_000));
    let c = put(store, &Seed::new("C").source("/c").chunk("1", "gamma").updated(3_000));
    let d = put(store, &Seed::new("D").source("/d").chunk("1", "delta").updated(4_000));

    // Three times each, but d: two are not enough to say that nobody found it useful.
    for _ in 0..UsefulFilter::SHOWN_MIN {
        store.record_shown(&[a, b, c]).expect("shown");
    }

    for _ in 1..UsefulFilter::SHOWN_MIN {
        store.record_shown(&[d]).expect("shown");
    }

    store.record_useful(&[a], 700).expect("useful");
    store.record_useful(&[a], 800).expect("useful again");
    store.record_useful(&[c], 900).expect("useful");

    let ever = Some(UsefulFilter::Ever);
    let never = Some(UsefulFilter::Never);
    let all = (0, 50);

    let kept = page(store, ever, Sort::Updated, false, all);
    assert_eq!((ids_of(&kept), kept.total), (vec![a, c], 2));

    let kept = page(store, never, Sort::Updated, false, all);
    assert_eq!(
        (ids_of(&kept), kept.total),
        (vec![b], 1),
        "d was shown too rarely to count as never useful"
    );

    store.record_shown(&[d]).expect("shown once more");

    let kept = page(store, never, Sort::Updated, false, all);
    assert_eq!(
        (ids_of(&kept), kept.total),
        (vec![b, d], 2),
        "shown often enough and never found useful"
    );

    let kept = page(store, None, Sort::Updated, false, all);
    assert_eq!((ids_of(&kept), kept.total), (vec![a, b, c, d], 4));

    // By how often: a 2, c 1, b and d 0 (ties by id in the direction of the sort).
    let sorted = |store: &mut dyn KnowledgeStore, sort: Sort, descending: bool| {
        ids_of(&page(store, None, sort, descending, all))
    };

    assert_eq!(sorted(store, Sort::Useful, false), [b, d, c, a]);
    assert_eq!(sorted(store, Sort::Useful, true), [a, c, d, b]);

    // By when: c at 900, a at 800, b and d never.
    assert_eq!(sorted(store, Sort::LastUseful, false), [b, d, a, c]);
    assert_eq!(sorted(store, Sort::LastUseful, true), [c, a, d, b]);

    // The filter and the sort go together, and the page and the total follow the filter.
    let kept = page(store, ever, Sort::Useful, true, all);
    assert_eq!((ids_of(&kept), kept.total), (vec![a, c], 2));

    let kept = page(store, never, Sort::Useful, true, (1, 1));
    assert_eq!((ids_of(&kept), kept.total), (vec![b], 2));

    let kept = page(store, None, Sort::Useful, true, (1, 2));
    assert_eq!((ids_of(&kept), kept.total), (vec![c, d], 4));
}

/// A document is stored with its authors, an update keeps who added it and replaces who updated
/// it (also with nobody), and `get`, `list` and the lookups all show them.
pub fn authors_contract(store: &mut dyn KnowledgeStore) {
    let ada = author("Ada", Some("claude-code"));
    let bob = author("Bob", None);

    let (mut doc, chunks, links) = Seed::new("Guide")
        .chunk("Intro", "alpha words")
        .by("Ada", Some("claude-code"))
        .parts();
    let id = store.put_document(&doc, &chunks, &links).expect("put the guide");

    let stored = read(store, id).doc;
    assert_eq!(
        (stored.added_by.clone(), stored.updated_by.clone()),
        (ada.clone(), ada.clone())
    );

    // The input's added_by is ignored for a document that exists, its updated_by is stored.
    doc.id = id;
    doc.added_by = bob.clone();
    doc.updated_by = bob.clone();
    assert_eq!(store.put_document(&doc, &chunks, &links).expect("update"), id);

    let stored = read(store, id).doc;
    assert_eq!(
        (stored.added_by.clone(), stored.updated_by.clone()),
        (ada.clone(), bob.clone())
    );

    // An update by nobody known clears who updated, and still keeps who added.
    doc.updated_by = None;
    store.put_document(&doc, &chunks, &links).expect("update by nobody");

    let stored = read(store, id).doc;
    assert_eq!(
        (stored.added_by.clone(), stored.updated_by.clone()),
        (ada.clone(), None)
    );

    // Relinking and renaming change neither.
    assert!(store.rename(id, "Guide v2", None, 0).expect("rename"));
    let stored = read(store, id).doc;
    assert_eq!((stored.added_by, stored.updated_by), (ada.clone(), None));

    // The lookups an import makes, and the list, show the same.
    let found = store
        .find_by_source_key(&doc.source_key, crate::store::Visibility::Live)
        .expect("find")
        .expect("found");
    assert_eq!(found.added_by, ada);

    let listed = rows(store);
    assert_eq!(listed[0].doc.added_by, ada);

    // A document stored by nobody known has no author.
    let anonymous = put(store, &Seed::new("Anonymous").chunk("Intro", "beta words"));
    let stored = read(store, anonymous).doc;
    assert_eq!((stored.added_by, stored.updated_by), (None, None));
}

/// The query of every document, sorted, over one page.
fn query(sort: Sort, descending: bool, (offset, limit): (usize, usize)) -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        author: None,
        want_authors: true,
        status: StatusFilter::All,
        sort,
        descending,
        offset,
        limit,
    }
}

fn named(name: &str) -> Option<AuthorFilter> {
    Some(AuthorFilter::Named(name.to_owned()))
}

/// The people of a page's facet with their counts, and the unknown count.
fn facet_of(page: &DocumentPage) -> (Vec<(String, usize)>, usize) {
    let people = page
        .authors
        .people()
        .into_iter()
        .map(|(name, count)| (name.to_owned(), count))
        .collect();

    (people, page.authors.unknown())
}

fn person(name: &str, count: usize) -> (String, usize) {
    (name.to_owned(), count)
}

/// `--author` keeps the documents added by exactly that name (case-sensitive, the harness ignored)
/// and `--author-unknown` those with no known author, the sort by who added a document puts the
/// unknown last in both directions and breaks ties by id, and the facet counts the people under
/// every filter but the author one.
pub fn author_filter_contract(store: &mut dyn KnowledgeStore) {
    let alpha = put(
        store,
        &Seed::new("Alpha")
            .chunk("1", "one")
            .by("Ada", Some("claude-code"))
            .updated(1),
    );
    let beta = put(store, &Seed::new("Beta").chunk("1", "two").by("Ada", None).updated(2));
    let gamma = put(
        store,
        &Seed::new("Gamma")
            .chunk("1", "three")
            .by("Bob", None)
            .format("pdf")
            .updated(3),
    );
    let delta = put(store, &Seed::new("Delta").chunk("1", "four").by("ada", None).updated(4));
    let epsilon = put(store, &Seed::new("Epsilon").chunk("1", "five").updated(5));
    let zeta = put(store, &Seed::new("Zeta").chunk("1", "six").by("", None).updated(6));
    let eta = put(
        store,
        &Seed::new("Eta").chunk("1", "seven").by("Émile", None).updated(7),
    );
    let theta = put(store, &Seed::new("Theta").chunk("1", "eight").updated(8));

    let listed = |store: &mut dyn KnowledgeStore, query: ListQuery| -> (Vec<i64>, usize) {
        let page = store.list(&query).expect("list");

        (ids_of(&page), page.total)
    };
    let all = (0, 50);

    // The name decides, not the harness it was written through, and not the case.
    let found = listed(
        store,
        ListQuery {
            author: named("Ada"),
            ..query(Sort::Updated, false, all)
        },
    );
    assert_eq!(found, (vec![alpha, beta], 2));

    let found = listed(
        store,
        ListQuery {
            author: named("ada"),
            ..query(Sort::Updated, false, all)
        },
    );
    assert_eq!(found, (vec![delta], 1));

    for nobody in ["ADA", "Ad", "Ada via claude-code", "Nobody"] {
        let found = listed(
            store,
            ListQuery {
                author: named(nobody),
                ..query(Sort::Updated, false, all)
            },
        );
        assert_eq!(found, (vec![], 0), "{nobody}");
    }

    // No name and an empty one are no known author.
    let unknown = Some(AuthorFilter::Unknown);
    let found = listed(
        store,
        ListQuery {
            author: unknown.clone(),
            ..query(Sort::Updated, false, all)
        },
    );
    assert_eq!(found, (vec![epsilon, zeta, theta], 3));

    let found = listed(store, query(Sort::Updated, false, all));
    assert_eq!(found, (vec![alpha, beta, gamma, delta, epsilon, zeta, eta, theta], 8));

    // Ascending by the name folded to lower case (the letters of ASCII only, so a name that starts
    // with another letter sorts after them), ties and the unknown by id, the unknown last.
    let ascending = [alpha, beta, delta, gamma, eta, epsilon, zeta, theta];
    let descending = [eta, gamma, delta, beta, alpha, theta, zeta, epsilon];

    assert_eq!(
        ids_of(&store.list(&query(Sort::AddedBy, false, all)).expect("list")),
        ascending
    );
    assert_eq!(
        ids_of(&store.list(&query(Sort::AddedBy, true, all)).expect("list")),
        descending
    );

    // Pages cut that order, also across the unknown ones.
    let found = listed(store, query(Sort::AddedBy, false, (2, 4)));
    assert_eq!(found, (vec![delta, gamma, eta, epsilon], 8));

    let found = listed(store, query(Sort::AddedBy, true, (4, 3)));
    assert_eq!(found, (vec![alpha, theta, zeta], 8));

    // The sort and the filter go together.
    let found = listed(
        store,
        ListQuery {
            author: named("Ada"),
            ..query(Sort::AddedBy, true, all)
        },
    );
    assert_eq!(found, (vec![beta, alpha], 2));

    let found = listed(
        store,
        ListQuery {
            author: unknown,
            ..query(Sort::AddedBy, true, all)
        },
    );
    assert_eq!(found, (vec![theta, zeta, epsilon], 3));

    // The facet: most first, then by name (ASCII letters folded), and the unknown ones apart. The
    // harness is no part of a person.
    let everyone = vec![person("Ada", 2), person("ada", 1), person("Bob", 1), person("Émile", 1)];

    let page = store.list(&query(Sort::Updated, false, all)).expect("list");
    assert_eq!(facet_of(&page), (everyone.clone(), 3));

    // The author filter does not change it, the total follows the filter.
    for author in [named("Ada"), named("Nobody"), Some(AuthorFilter::Unknown)] {
        let page = store
            .list(&ListQuery {
                author,
                ..query(Sort::Updated, false, all)
            })
            .expect("list");
        assert_eq!(facet_of(&page), (everyone.clone(), 3));
    }

    // Every other filter changes it, and the page does not: a page past the end has the facet too.
    let page = store
        .list(&ListQuery {
            format: Some("pdf".to_owned()),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!(facet_of(&page), (vec![person("Bob", 1)], 0));

    let page = store
        .list(&ListQuery {
            text: "Alpha".to_owned(),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!(facet_of(&page), (vec![person("Ada", 1)], 0));

    let page = store.list(&query(Sort::Updated, false, (100, 10))).expect("list");
    assert_eq!(
        (page.items.len(), page.total, facet_of(&page)),
        (0, 8, (everyone.clone(), 3))
    );

    store.record_shown(&[beta, epsilon]).expect("shown");
    store.record_useful(&[beta, epsilon], 10).expect("useful");

    let page = store
        .list(&ListQuery {
            useful: Some(UsefulFilter::Ever),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!(facet_of(&page), (vec![person("Ada", 1)], 1));

    let page = store
        .list(&ListQuery {
            useful: Some(UsefulFilter::Ever),
            author: named("Bob"),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!((page.total, facet_of(&page)), (0, (vec![person("Ada", 1)], 1)));
}

/// A name somebody wrote into the database that is not one (empty, only blanks, with spaces around
/// it, with an invisible character, too long) is no person: the facet counts it as unknown and never
/// shows it.
pub fn author_invalid_names_contract(store: &mut dyn KnowledgeStore) {
    // Names of 80 characters are fine, of 81 and of a hundred thousand are not (and the long ones
    // start alike, so they could be one group, counted all the same). A name that is 80 characters
    // and then blanks and more is not one either: the cut that reads 81 characters of it must not
    // make it pass as its trimmed form.
    let fine = "y".repeat(80);
    let emoji = "\u{1f600}".repeat(80);
    let padded_long = format!("{} {}", "b".repeat(80), "y".repeat(1000));
    let padded_short = format!("{} c", "b".repeat(80));
    let stored = [
        "Ada",
        "",
        "   ",
        " Bob ",
        "Eve\u{202e}",
        "Mal\u{200b}",
        &"x".repeat(81),
        &"x".repeat(100_000),
        &format!("{}z", "x".repeat(100_000)),
        &fine,
        &padded_long,
        &padded_short,
        &emoji,
        &"\u{1f600}".repeat(81),
    ];

    for (n, name) in stored.iter().enumerate() {
        put(
            store,
            &Seed::new(&format!("Doc {n}"))
                .chunk("1", "text")
                .by(name, None)
                .updated(n as i64),
        );
    }

    put(store, &Seed::new("Doc none").chunk("1", "text"));

    let page = store.list(&query(Sort::Updated, false, (0, 50))).expect("list");
    assert_eq!(
        facet_of(&page),
        (vec![person("Ada", 1), person(&fine, 1), person(&emoji, 1)], 12)
    );

    let printed = page.authors.json().to_string();
    assert!(!printed.contains('\u{202e}') && !printed.contains('\u{200b}') && !printed.contains("xxxx"));
    assert!(!printed.contains("bbbb"), "a padded name is not shown");

    // A page of one row, in either order, answers the same and shows no oversized name.
    for descending in [false, true] {
        let one = store.list(&query(Sort::AddedBy, descending, (0, 1))).expect("list");

        assert_eq!((one.items.len(), one.total), (1, 15));
        assert_eq!(
            facet_of(&one),
            (vec![person("Ada", 1), person(&fine, 1), person(&emoji, 1)], 12)
        );
        assert!(!one.authors.json().to_string().contains("xxxx"));
    }

    // The sort still answers: every row is there once.
    let page = store.list(&query(Sort::AddedBy, false, (0, 50))).expect("list");
    assert_eq!(page.items.len(), 15);
}

/// The authors facet is read only for a query that asks for it (`want_authors`): without it the
/// facet is empty and the page, its order and its total are the same.
pub fn author_facet_asked_contract(store: &mut dyn KnowledgeStore) {
    put(store, &Seed::new("Alpha").chunk("1", "one").by("Ada", None).updated(1));
    put(store, &Seed::new("Beta").chunk("1", "two").by("Bob", None).updated(2));
    put(store, &Seed::new("Gamma").chunk("1", "three").updated(3));

    let asked = store.list(&query(Sort::AddedBy, false, (0, 50))).expect("list");
    assert_eq!(facet_of(&asked), (vec![person("Ada", 1), person("Bob", 1)], 1));

    let not_asked = store
        .list(&ListQuery {
            want_authors: false,
            ..query(Sort::AddedBy, false, (0, 50))
        })
        .expect("list");
    assert_eq!(facet_of(&not_asked), (vec![], 0));
    assert_eq!((ids_of(&not_asked), not_asked.total), (ids_of(&asked), asked.total));

    // Also with the author filter, whose page and total are the same.
    let not_asked = store
        .list(&ListQuery {
            want_authors: false,
            author: named("Ada"),
            ..query(Sort::Updated, false, (0, 50))
        })
        .expect("list");
    assert_eq!((facet_of(&not_asked), not_asked.total), ((vec![], 0), 1));
}

/// A thousand documents of hundreds of people page in both directions of the sort by who added them
/// without overlap or gap, the unknown ones last, and the facet adds up to the documents the other
/// filters keep, whichever name is selected.
pub fn author_listing_scale_contract(store: &mut dyn KnowledgeStore) {
    const DOCS: usize = 1200;
    const PEOPLE: usize = 250;

    // Every tenth document has no author (a quarter of those have an empty name). The rest go round
    // the people, so the ones whose number ends in 9 never get one.
    let mut added: Vec<(i64, Option<String>)> = Vec::new();

    for n in 0..DOCS {
        let name = (n % 10 != 9).then(|| format!("Person {:03}", n % PEOPLE));
        let seed = Seed::new(&format!("Doc {n:04}")).chunk("1", "text").updated(n as i64);
        let seed = match (&name, n % 40) {
            (Some(name), _) => seed.by(name, None),
            (None, 9) => seed.by("", None),
            (None, _) => seed,
        };

        added.push((put(store, &seed), name));
    }

    // The order the contract asks for.
    let key = |name: &Option<String>| name.as_ref().map(|name| name.to_ascii_lowercase());
    let mut expected = added.clone();
    expected.sort_by(|a, b| match (key(&a.1), key(&b.1)) {
        (Some(x), Some(y)) => x.cmp(&y).then(a.0.cmp(&b.0)),
        (None, None) => a.0.cmp(&b.0),
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
    });
    let ascending: Vec<i64> = expected.iter().map(|(id, _)| *id).collect();

    let mut descending = expected.clone();
    descending.sort_by(|a, b| match (key(&a.1), key(&b.1)) {
        (Some(x), Some(y)) => y.cmp(&x).then(b.0.cmp(&a.0)),
        (None, None) => b.0.cmp(&a.0),
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
    });
    let descending: Vec<i64> = descending.iter().map(|(id, _)| *id).collect();

    for (way, wanted) in [(false, ascending), (true, descending)] {
        let mut paged: Vec<i64> = Vec::new();

        for offset in (0..DOCS).step_by(200) {
            let page = store.list(&query(Sort::AddedBy, way, (offset, 200))).expect("list");

            assert_eq!(page.total, DOCS);
            paged.extend(ids_of(&page));
        }

        assert_eq!(paged, wanted, "descending: {way}");
    }

    // The facet adds up, with or without a name selected, and holds the people it read.
    let unknown = added.iter().filter(|(_, name)| name.is_none()).count();
    let people = added
        .iter()
        .filter_map(|(_, name)| name.as_ref())
        .collect::<std::collections::BTreeSet<_>>();

    for author in [None, named("Person 007"), named("Nobody"), Some(AuthorFilter::Unknown)] {
        let page = store
            .list(&ListQuery {
                author: author.clone(),
                ..query(Sort::AddedBy, false, (0, 1))
            })
            .expect("list");
        let (listed, unknown_count) = facet_of(&page);

        assert_eq!(unknown_count, unknown, "{author:?}");
        assert_eq!(listed.len(), people.len(), "{author:?}");
        assert_eq!(
            listed.iter().map(|(_, count)| count).sum::<usize>() + unknown_count,
            DOCS
        );
        assert!(listed.windows(2).all(|pair| pair[0].1 >= pair[1].1));
    }

    let page = store
        .list(&ListQuery {
            author: named("Person 007"),
            ..query(Sort::AddedBy, false, (0, 50))
        })
        .expect("list");
    let own = added
        .iter()
        .filter(|(_, name)| name.as_deref() == Some("Person 007"))
        .count();
    assert_eq!((page.total, page.items.len()), (own, own));
}

fn usage(store: &mut dyn KnowledgeStore, id: i64) -> Option<DocUsage> {
    read(store, id).usage
}

fn counted(shown: i64, useful: i64, last_useful_at: Option<i64>) -> Option<DocUsage> {
    Some(DocUsage {
        shown,
        useful,
        last_useful_at,
    })
}

/// The counters start at 0, grow by one per call and document (an id given twice counts once),
/// skip an id that does not exist, show on `get` and `list`, and survive an update.
pub fn counters_contract(store: &mut dyn KnowledgeStore) {
    let first = put(store, &Seed::new("First").chunk("Intro", "alpha words").updated(1_000));
    let second = put(store, &Seed::new("Second").chunk("Intro", "beta words").updated(2_000));

    assert_eq!(usage(store, first), counted(0, 0, None));

    store.record_shown(&[first, second]).expect("shown");
    store.record_shown(&[first, first]).expect("shown twice in one call");
    store.record_useful(&[first], 500).expect("useful");
    store.record_useful(&[first], 700).expect("useful again");
    store.record_shown(&[]).expect("nothing");
    store.record_useful(&[], 1).expect("nothing");
    store.record_shown(&[987_654]).expect("an unknown id is skipped");

    assert_eq!(usage(store, first), counted(2, 2, Some(700)));
    assert_eq!(usage(store, second), counted(1, 0, None));

    let listed = rows(store);
    assert_eq!(listed[0].usage, counted(2, 2, Some(700)));
    assert_eq!(listed[1].usage, counted(1, 0, None));

    // Putting the document again never resets what was counted.
    let (mut doc, chunks, links) = Seed::new("First").chunk("Intro", "alpha words").updated(3_000).parts();
    doc.id = first;
    store.put_document(&doc, &chunks, &links).expect("update");

    assert_eq!(usage(store, first), counted(2, 2, Some(700)));
}

/// A title matches a document of the same scope only, ignoring case and whitespace.
pub fn titles_contract(store: &mut dyn KnowledgeStore) {
    const ONE: &str = "git:example.com/team/one";
    const TWO: &str = "git:example.com/team/two";

    let global = put(store, &Seed::new("Setup Guide").chunk("Intro", "alpha"));
    let in_one = put(
        store,
        &Seed::new("Setup  GUIDE")
            .source("/one")
            .chunk("Intro", "beta")
            .link(ONE, &[]),
    );
    let in_two = put(
        store,
        &Seed::new("setup guide")
            .source("/two")
            .chunk("Intro", "gamma")
            .link(TWO, &[]),
    );
    let in_both = put(
        store,
        &Seed::new("Setup guide")
            .source("/both")
            .chunk("Intro", "delta")
            .link(ONE, &[])
            .link(TWO, &[]),
    );
    put(
        store,
        &Seed::new("Setup Guide 2")
            .source("/other")
            .chunk("Intro", "eps")
            .link(ONE, &[]),
    );
    let polish = put(store, &Seed::new("Łódź Notes").source("/polish").chunk("Intro", "zeta"));

    let ids = |store: &mut dyn KnowledgeStore, title: &str, scope: &TitleScope| -> Vec<i64> {
        store
            .find_by_title(title, scope, crate::store::Visibility::Live)
            .expect("find by title")
            .iter()
            .map(|doc: &Document| doc.id)
            .collect()
    };

    assert_eq!(ids(store, "Setup Guide", &TitleScope::Global), [global]);
    assert_eq!(
        ids(store, "Setup Guide", &TitleScope::Repo(ONE.to_owned())),
        [in_one, in_both]
    );
    assert_eq!(
        ids(store, "Setup Guide", &TitleScope::Repo(TWO.to_owned())),
        [in_two, in_both]
    );
    assert!(
        ids(
            store,
            "Setup Guide",
            &TitleScope::Repo("git:example.com/team/none".to_owned())
        )
        .is_empty()
    );

    // Case and whitespace do not matter, other punctuation does.
    assert_eq!(ids(store, "  sETUP \t guide  ", &TitleScope::Global), [global]);
    assert!(ids(store, "Setup-Guide", &TitleScope::Global).is_empty());
    assert!(ids(store, "Setup", &TitleScope::Global).is_empty());
    assert_eq!(ids(store, "łódź  NOTES", &TitleScope::Global), [polish]);

    // The document comes back whole.
    let found = store
        .find_by_title(
            "setup guide",
            &TitleScope::Repo(TWO.to_owned()),
            crate::store::Visibility::Live,
        )
        .expect("find");
    assert_eq!(found[0].source_key, "/two");
}

/// The documents that share chunk hashes are counted by distinct hash, lowest id first.
pub fn shared_chunks_contract(store: &mut dyn KnowledgeStore) {
    let a = put(
        store,
        &Seed::new("A")
            .chunk("1", "alpha")
            .chunk("2", "beta")
            .chunk("3", "gamma"),
    );
    let b = put(
        store,
        &Seed::new("B").source("/b").chunk("1", "alpha").chunk("2", "delta"),
    );
    let c = put(
        store,
        &Seed::new("C").source("/c").chunk("1", "alpha").chunk("2", "alpha"),
    );

    let shared = |store: &mut dyn KnowledgeStore, shas: &[&str]| -> Vec<(i64, usize)> {
        let shas: Vec<String> = shas.iter().map(|sha| (*sha).to_owned()).collect();

        store
            .docs_sharing_chunks(&shas, crate::store::Visibility::Live)
            .expect("docs sharing chunks")
    };

    assert_eq!(shared(store, &["sha-alpha", "sha-beta"]), [(a, 2), (b, 1), (c, 1)]);
    assert_eq!(shared(store, &["sha-gamma"]), [(a, 1)]);
    assert_eq!(
        shared(store, &["sha-alpha", "sha-alpha"]),
        [(a, 1), (b, 1), (c, 1)],
        "a hash given twice"
    );
    assert!(shared(store, &["sha-nothing"]).is_empty());
    assert!(shared(store, &[]).is_empty());

    let too_many: Vec<String> = (0..=SHARED_SHAS_MAX).map(|n| format!("sha-{n}")).collect();
    assert!(matches!(
        store.docs_sharing_chunks(&too_many, crate::store::Visibility::Live),
        Err(StoreError::Invalid(_))
    ));
}

/// A database without the added columns (a read-only role on a database no writer extended) still
/// reads: no authors, no usage, and the counters do nothing without failing. `id` is a stored
/// document that was given authors.
pub fn absent_columns_contract(store: &mut dyn KnowledgeStore, id: i64) {
    let detail = read(store, id);
    assert_eq!(
        (detail.doc.added_by, detail.doc.updated_by, detail.usage),
        (None, None, None)
    );

    let listed = rows(store);
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|row| row.usage.is_none() && row.doc.added_by.is_none())
    );

    let found = store
        .find_by_source_key(&detail.doc.source_key, crate::store::Visibility::Live)
        .expect("find")
        .expect("found");
    assert_eq!(found.added_by, None);

    // Nobody can have been found useful and nothing was counted as shown: `ever` and `never` keep
    // none, no filter keeps every document, and the sorts by usefulness are the default order,
    // not an error.
    let all = (0, 50);
    let by_updated = ids_of(&page(store, None, Sort::Updated, false, all));
    let by_updated_desc = ids_of(&page(store, None, Sort::Updated, true, all));

    for filter in [UsefulFilter::Ever, UsefulFilter::Never] {
        let kept = page(store, Some(filter), Sort::Updated, false, all);
        assert_eq!((kept.items.len(), kept.total), (0, 0), "{filter:?}");
    }

    let kept = page(store, None, Sort::Updated, false, all);
    assert_eq!((ids_of(&kept), kept.total), (by_updated.clone(), by_updated.len()));

    for sort in [Sort::Useful, Sort::LastUseful] {
        assert_eq!(ids_of(&page(store, None, sort, false, all)), by_updated);
        assert_eq!(ids_of(&page(store, None, sort, true, all)), by_updated_desc);
    }

    store.record_shown(&[id]).expect("shown does nothing");
    store.record_useful(&[id], 5).expect("useful does nothing");
    assert_eq!(read(store, id).usage, None);

    // Nobody has a known author: a name keeps none, unknown keeps all, the sort by who added a
    // document is the default order, and the facet holds nobody and every document as unknown.
    let kept = store
        .list(&ListQuery {
            author: named("Ada"),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!((kept.items.len(), kept.total), (0, 0));
    assert_eq!(facet_of(&kept), (vec![], by_updated.len()));

    let kept = store
        .list(&ListQuery {
            author: Some(AuthorFilter::Unknown),
            ..query(Sort::Updated, false, all)
        })
        .expect("list");
    assert_eq!((ids_of(&kept), kept.total), (by_updated.clone(), by_updated.len()));
    assert_eq!(facet_of(&kept), (vec![], by_updated.len()));

    assert_eq!(
        ids_of(&store.list(&query(Sort::AddedBy, false, all)).expect("list")),
        by_updated
    );
    assert_eq!(
        ids_of(&store.list(&query(Sort::AddedBy, true, all)).expect("list")),
        by_updated_desc
    );
}

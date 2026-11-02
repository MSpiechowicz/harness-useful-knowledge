//! What every backend answers about authors, usefulness counters, titles and shared chunks (the
//! same checks run on each, on an empty database).

use super::Seed;
use crate::limits::SHARED_SHAS_MAX;
use crate::store::{Author, Detail, DocUsage, Document, KnowledgeStore, ListQuery, RepoFilter, Sort, StoreError};
use crate::store::{DocumentPage, DocumentRow, TitleScope, UsefulFilter};

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
            sort: Sort::Updated,
            descending: false,
            offset: 0,
            limit: 50,
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
            sort,
            descending,
            offset,
            limit,
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
    assert!(store.rename(id, "Guide v2").expect("rename"));
    let stored = read(store, id).doc;
    assert_eq!((stored.added_by, stored.updated_by), (ada.clone(), None));

    // The lookups an import makes, and the list, show the same.
    let found = store.find_by_source_key(&doc.source_key).expect("find").expect("found");
    assert_eq!(found.added_by, ada);

    let listed = rows(store);
    assert_eq!(listed[0].doc.added_by, ada);

    // A document stored by nobody known has no author.
    let anonymous = put(store, &Seed::new("Anonymous").chunk("Intro", "beta words"));
    let stored = read(store, anonymous).doc;
    assert_eq!((stored.added_by, stored.updated_by), (None, None));
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
            .find_by_title(title, scope)
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
        .find_by_title("setup guide", &TitleScope::Repo(TWO.to_owned()))
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

        store.docs_sharing_chunks(&shas).expect("docs sharing chunks")
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
        store.docs_sharing_chunks(&too_many),
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
        .find_by_source_key(&detail.doc.source_key)
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
}

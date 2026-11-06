use pretty_assertions::assert_eq;

use super::*;
use crate::store::AuthorFilter;

/// Documents with fixed titles and times in both places, for paging.
fn paging_fixture() -> RoutedStore {
    let mut remote = store();
    let mut local = store();

    for (n, title) in ["delta", "alpha", "echo", "Bravo", "charlie", "foxtrot", "golf"]
        .iter()
        .enumerate()
    {
        // Every second document shares its time with another, to make ties.
        let updated = 100 + (n as i64 / 2);
        let seed = Seed::new(&format!("{title} remote"))
            .updated(updated)
            .bytes(10 * (n as u64 % 3));
        insert(&mut remote, &seed.chunk("a", "text"));

        let seed = Seed::new(&format!("{title} local"))
            .updated(updated)
            .bytes(10 * (n as u64 % 3));
        insert(&mut local, &seed.chunk("a", "text"));
    }

    routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal))
}

#[test]
fn pages_of_the_merged_list_are_the_pages_of_one_sorted_list_for_every_sort() {
    for sort in [
        Sort::Title,
        Sort::Updated,
        Sort::Size,
        Sort::Chunks,
        Sort::Format,
        Sort::Paths,
        Sort::Repos,
    ] {
        for descending in [false, true] {
            let mut routed = paging_fixture();

            let everything = routed.list(&query(sort, descending, 0, 100)).expect("all");
            assert_eq!(everything.total, 14);
            assert_eq!(everything.items.len(), 14);

            let mut paged = Vec::new();

            for page in 0..5 {
                let got = routed.list(&query(sort, descending, page * 3, 3)).expect("page");
                assert_eq!(got.total, 14);
                paged.extend(got.items);
            }

            let ids = |rows: &[DocumentRow]| rows.iter().map(|row| row.doc.id).collect::<Vec<_>>();
            assert_eq!(ids(&paged), ids(&everything.items), "{sort:?} descending {descending}");

            // No document twice, none lost.
            let mut unique = ids(&paged);
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), 14);
        }
    }
}

#[test]
fn the_merge_reads_the_titles_in_order_across_places_and_breaks_ties_by_id() {
    let mut routed = paging_fixture();

    let by_title = titles(&routed.list(&query(Sort::Title, false, 0, 6)).expect("list"));
    assert_eq!(
        by_title,
        [
            "alpha local",
            "alpha remote",
            "Bravo local",
            "Bravo remote",
            "charlie local",
            "charlie remote"
        ]
    );

    // Equal times: the remote document (the lower id) first ascending, last descending.
    let oldest = routed.list(&query(Sort::Updated, false, 0, 2)).expect("list");
    assert_eq!(oldest.items[0].doc.updated_at, oldest.items[1].doc.updated_at);
    assert!(oldest.items[0].doc.id < oldest.items[1].doc.id);

    let newest = routed.list(&query(Sort::Updated, true, 0, 2)).expect("list");
    assert!(newest.items[0].doc.id > newest.items[1].doc.id);
}

#[test]
fn a_page_deeper_than_a_store_answers_at_once_is_still_right() {
    let mut remote = store();
    let mut local = store();

    for n in 0..230 {
        let seed = Seed::new(&format!("doc {n:03} remote")).chunk("a", "text");
        insert(&mut remote, &seed);

        let seed = Seed::new(&format!("doc {n:03} local")).chunk("a", "text");
        insert(&mut local, &seed);
    }

    let mut routed = routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    let page = routed.list(&query(Sort::Title, false, 400, 50)).expect("list");

    assert_eq!(page.total, 460);
    assert_eq!(page.items.len(), 50);
    assert_eq!(page.items[0].doc.title, "doc 200 local");
    assert_eq!(page.items[49].doc.title, "doc 224 remote");
}

#[test]
fn the_merged_titles_of_two_places_sort_the_way_each_store_sorts_them_for_polish_titles() {
    use crate::store::testing::{TITLES, title_order};

    let mut remote = store();
    let mut local = store();
    let mut expected: Vec<(i64, String)> = Vec::new();

    for (n, title) in TITLES.iter().enumerate() {
        let seed = Seed::new(title).chunk("a", "text");

        if n % 2 == 0 {
            expected.push((ids::tag(Place::Remote, insert(&mut remote, &seed)), (*title).to_owned()));
        } else {
            expected.push((ids::tag(Place::Local, insert(&mut local, &seed)), (*title).to_owned()));
        }
    }

    let mut routed = routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    let order = title_order(expected);

    assert_eq!(
        titles(&routed.list(&query(Sort::Title, false, 0, 50)).expect("list")),
        order
    );

    // The same in pages, which is where an order that differs from a store's shows.
    let mut paged = Vec::new();

    for page in 0..4 {
        paged.extend(titles(
            &routed.list(&query(Sort::Title, false, page * 3, 3)).expect("page"),
        ));
    }

    assert_eq!(paged, order);

    let mut reversed = order;
    reversed.reverse();
    assert_eq!(
        titles(&routed.list(&query(Sort::Title, true, 0, 50)).expect("list")),
        reversed
    );
}

#[test]
fn the_merged_list_sorts_by_paths_across_both_places() {
    let mut remote = store();
    let mut local = store();

    insert(
        &mut remote,
        &Seed::new("two").link("git:h/o/a", &["a/**", "b/**"]).chunk("a", "text"),
    );
    insert(
        &mut local,
        &Seed::new("three")
            .link("git:h/o/a", &["a/**"])
            .link("git:h/o/b", &["b/**", "c/**"])
            .chunk("a", "text"),
    );
    insert(&mut remote, &Seed::new("none").chunk("a", "text"));
    insert(
        &mut local,
        &Seed::new("one").link("git:h/o/a", &["a/**"]).chunk("a", "text"),
    );

    let mut routed = routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    let titles = |routed: &mut RoutedStore, descending| -> Vec<String> {
        let rows = routed
            .list(&query(Sort::Paths, descending, 0, 100))
            .expect("list")
            .items;

        rows.into_iter().map(|row| row.doc.title).collect()
    };

    assert_eq!(titles(&mut routed, false), ["none", "one", "two", "three"]);
    assert_eq!(titles(&mut routed, true), ["three", "two", "one", "none"]);
}

/// Both places hold documents by the same people (also by names that differ in case only) and by
/// nobody known. The titles say the place and the author.
fn authored_fixture() -> RoutedStore {
    let mut remote = store();
    let mut local = store();

    let remote_docs = [
        ("r-ada-1", Some("Ada")),
        ("r-bob", Some("bob")),
        ("r-none-1", None),
        ("r-ada-2", Some("ada")),
        ("r-cy", Some("Cy")),
    ];
    let local_docs = [
        ("l-none", None),
        ("l-ada", Some("Ada")),
        ("l-bob", Some("Bob")),
        ("l-ada-2", Some("Ada")),
        ("l-none-2", None),
    ];

    for (store, docs) in [(&mut remote, &remote_docs[..]), (&mut local, &local_docs[..])] {
        for (n, (title, author)) in docs.iter().enumerate() {
            let seed = Seed::new(title).chunk("a", "text").updated(100 + n as i64);

            insert(
                store,
                &match author {
                    Some(name) => seed.by(name, None),
                    None => seed,
                },
            );
        }
    }

    routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal))
}

#[test]
fn the_merged_list_sorts_by_who_added_the_documents_with_the_unknown_last_both_ways() {
    for descending in [false, true] {
        let mut routed = authored_fixture();
        let everything = routed.list(&query(Sort::AddedBy, descending, 0, 100)).expect("all");
        assert_eq!(everything.total, 10);

        // One sorted list in the order of every store: the folded name, the id breaking ties.
        let names: Vec<Option<String>> = everything
            .items
            .iter()
            .map(|row| row.doc.added_by.as_ref().map(|author| author.name.to_ascii_lowercase()))
            .collect();
        let known: Vec<&String> = names.iter().flatten().collect();
        let mut sorted = known.clone();
        sorted.sort();

        if descending {
            sorted.reverse();
        }

        assert_eq!(known, sorted, "descending {descending}");
        assert_eq!(known.len(), 7);
        assert!(
            names[7..].iter().all(Option::is_none),
            "the unknown last, descending {descending}"
        );

        // The pages are the pages of that list.
        let mut paged = Vec::new();

        for page in 0..4 {
            let got = routed
                .list(&query(Sort::AddedBy, descending, page * 3, 3))
                .expect("page");
            assert_eq!(got.total, 10);
            paged.extend(got.items.iter().map(|row| row.doc.id));
        }

        let ids: Vec<i64> = everything.items.iter().map(|row| row.doc.id).collect();
        assert_eq!(paged, ids, "descending {descending}");
    }
}

#[test]
fn the_merged_list_filters_by_author_and_adds_up_the_facets_of_both_places() {
    let mut routed = authored_fixture();
    let titles_of = |routed: &mut RoutedStore, author: Option<AuthorFilter>, only: Option<Place>| {
        let page = routed
            .list_in(
                only,
                &ListQuery {
                    author,
                    want_authors: true,
                    ..query(Sort::Updated, false, 0, 50)
                },
            )
            .expect("list");

        (titles(&page), page.total, page.authors)
    };

    let (found, total, facet) = titles_of(&mut routed, Some(AuthorFilter::Named("Ada".to_owned())), None);
    assert_eq!(
        (found, total),
        (vec!["r-ada-1".to_owned(), "l-ada".to_owned(), "l-ada-2".to_owned()], 3)
    );

    // The same facet whatever the author filter, the two places added up.
    let everyone = vec![("Ada", 3), ("ada", 1), ("Bob", 1), ("bob", 1), ("Cy", 1)];
    assert_eq!((facet.people(), facet.unknown()), (everyone.clone(), 3));

    let (found, total, facet) = titles_of(&mut routed, Some(AuthorFilter::Unknown), None);
    assert_eq!((found.len(), total), (3, 3));
    assert_eq!((facet.people(), facet.unknown()), (everyone, 3));

    // One place alone has its own total and facet.
    let (found, total, facet) = titles_of(&mut routed, None, Some(Place::Remote));
    assert_eq!((found.len(), total), (5, 5));
    assert_eq!(
        (facet.people(), facet.unknown()),
        (vec![("Ada", 1), ("ada", 1), ("bob", 1), ("Cy", 1)], 1)
    );
}

#[test]
fn a_merged_list_reads_the_facet_of_each_place_once_however_many_pages_it_fetches() {
    let mut remote = store();
    let mut local = store();

    // 300 documents in each place: a page of 200 and the rest, by Ada, Bob and nobody known.
    for n in 0..300 {
        for (place, prefix) in [(&mut remote, "r"), (&mut local, "l")] {
            let seed = Seed::new(&format!("{prefix}{n:03}")).chunk("a", "text").updated(n);

            insert(
                place,
                &match n % 3 {
                    0 => seed.by("Ada", None),
                    1 => seed.by("Bob", None),
                    _ => seed,
                },
            );
        }
    }

    let mut routed = routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));

    // Far enough in that every place is asked for more than one page.
    let asked = ListQuery {
        want_authors: true,
        ..query(Sort::Updated, false, 450, 50)
    };
    let page = routed.list(&asked).expect("list");

    assert_eq!((page.items.len(), page.total), (50, 600));
    assert_eq!(
        (page.authors.people(), page.authors.unknown()),
        (vec![("Ada", 200), ("Bob", 200)], 200)
    );

    let not_asked = routed
        .list(&ListQuery {
            want_authors: false,
            ..asked
        })
        .expect("list");
    assert_eq!(
        (
            not_asked.total,
            not_asked.authors.people().len(),
            not_asked.authors.unknown()
        ),
        (600, 0, 0)
    );
}

use pretty_assertions::assert_eq;

use super::*;

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

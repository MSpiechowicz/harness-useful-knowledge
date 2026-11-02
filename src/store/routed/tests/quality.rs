//! Counters, titles and shared chunks across the two places.

use pretty_assertions::assert_eq;

use super::flaky::Machine;
use super::*;
use crate::store::{DocUsage, TitleScope, UsefulFilter};

fn routed(repository: Where, global: Where) -> RoutedStore {
    let mut remote = store();
    insert(&mut remote, &Seed::new("Shared title").chunk("Intro", "alpha words"));
    insert(
        &mut remote,
        &Seed::new("Remote linked")
            .source("/rl")
            .chunk("Intro", "beta words")
            .link(REPO, &[]),
    );

    let mut local = store();
    insert(
        &mut local,
        &Seed::new("shared  TITLE").source("/ls").chunk("Intro", "alpha words"),
    );
    insert(
        &mut local,
        &Seed::new("Local linked")
            .source("/ll")
            .chunk("Intro", "gamma words")
            .link(REPO, &[]),
    );

    routed_over(local, remote, Routing::new(repository, global))
}

fn id_of(routed: &mut RoutedStore, title: &str) -> i64 {
    let page = routed.list(&query(Sort::Title, false, 0, 50)).expect("list");

    page.items
        .iter()
        .find(|row| row.doc.title == title)
        .unwrap_or_else(|| panic!("{title} is listed"))
        .doc
        .id
}

fn usage(routed: &mut RoutedStore, id: i64) -> Option<DocUsage> {
    routed
        .get(id, &Detail::default())
        .expect("get")
        .expect("document")
        .usage
}

#[test]
fn a_counter_lands_in_the_place_the_id_carries() {
    let mut routed = routed(Where::RemoteLocal, Where::RemoteLocal);
    let remote = id_of(&mut routed, "Shared title");
    let local = id_of(&mut routed, "shared TITLE");
    let other = id_of(&mut routed, "Remote linked");

    routed.record_shown(&[remote, local, local]).expect("shown");
    routed.record_useful(&[local], 900).expect("useful");

    let counted = |shown, useful, last| {
        Some(DocUsage {
            shown,
            useful,
            last_useful_at: last,
        })
    };
    assert_eq!(usage(&mut routed, remote), counted(1, 0, None));
    assert_eq!(usage(&mut routed, local), counted(1, 1, Some(900)));
    assert_eq!(usage(&mut routed, other), counted(0, 0, None));
}

#[test]
fn a_title_is_found_in_every_place_that_shows_its_kind_and_the_ids_carry_the_place() {
    let mut both = routed(Where::RemoteLocal, Where::RemoteLocal);
    let mut places: Vec<Place> = both
        .find_by_title("Shared Title", &TitleScope::Global)
        .expect("find")
        .iter()
        .map(|doc| ids::place_of(doc.id))
        .collect();
    places.sort_by_key(|place| *place == Place::Local);
    assert_eq!(places, [Place::Remote, Place::Local]);

    // Global documents are kept on this computer alone: the remote's are not shown.
    let mut local_only = routed(Where::RemoteLocal, Where::Local);
    let found = local_only
        .find_by_title("shared title", &TitleScope::Global)
        .expect("find");
    assert_eq!(
        found.iter().map(|doc| ids::place_of(doc.id)).collect::<Vec<_>>(),
        [Place::Local]
    );

    let linked = both
        .find_by_title("local linked", &TitleScope::Repo(REPO.to_owned()))
        .expect("find");
    assert_eq!(linked.len(), 1);
    assert_eq!(ids::place_of(linked[0].id), Place::Local);
}

#[test]
fn documents_sharing_chunks_come_from_every_place_with_tagged_ids() {
    let mut routed = routed(Where::RemoteLocal, Where::RemoteLocal);
    let found = routed
        .docs_sharing_chunks(&["sha-alpha words".to_owned()])
        .expect("sharing");

    let mut places: Vec<Place> = found.iter().map(|(id, _)| ids::place_of(*id)).collect();
    places.sort_by_key(|place| *place == Place::Local);

    assert_eq!(places, [Place::Remote, Place::Local]);
    assert!(found.iter().all(|(_, shared)| *shared == 1));
}

#[test]
fn the_remote_answers_whether_it_is_writable_and_a_remote_that_is_down_does_not() {
    let up = Machine::new(true);
    assert_eq!(up.process().remote_writable(), Some(true));

    let down = Machine::new(false);
    let mut routed = down.process();
    assert_eq!(routed.remote_writable(), None);

    // A counter for the remote is skipped while it cannot be reached, not waited for.
    routed.record_shown(&[ids::tag(Place::Remote, 1)]).expect("skipped");
}

fn listed(
    routed: &mut RoutedStore,
    useful: Option<UsefulFilter>,
    sort: Sort,
    descending: bool,
) -> (Vec<String>, usize) {
    let page = routed
        .list(&ListQuery {
            useful,
            ..query(sort, descending, 0, 50)
        })
        .expect("list");

    (titles(&page), page.total)
}

#[test]
fn the_useful_filter_and_sorts_merge_the_counters_of_both_places() {
    let mut routed = routed(Where::RemoteLocal, Where::RemoteLocal);
    let remote = id_of(&mut routed, "Shared title");
    let local = id_of(&mut routed, "Local linked");

    routed.record_useful(&[remote], 100).expect("useful");
    routed.record_useful(&[local], 50).expect("useful");
    routed.record_useful(&[local], 60).expect("useful again");

    // Shown often enough and never found useful: the first. The second was shown too rarely to say.
    let (often, rarely) = (id_of(&mut routed, "Remote linked"), id_of(&mut routed, "shared TITLE"));

    for _ in 1..UsefulFilter::SHOWN_MIN {
        routed.record_shown(&[often, rarely]).expect("shown");
    }

    routed.record_shown(&[often]).expect("shown once more");

    let (found, total) = listed(&mut routed, Some(UsefulFilter::Ever), Sort::Title, false);
    assert_eq!(
        (found, total),
        (vec!["Local linked".to_owned(), "Shared title".to_owned()], 2)
    );

    let (found, total) = listed(&mut routed, Some(UsefulFilter::Never), Sort::Title, false);
    assert_eq!((found, total), (vec!["Remote linked".to_owned()], 1));

    // The count of one place decides against the other's, and the page is cut after the merge.
    let (found, total) = listed(&mut routed, None, Sort::Useful, true);
    assert_eq!(
        (found[..2].to_vec(), total),
        (vec!["Local linked".to_owned(), "Shared title".to_owned()], 4)
    );

    let (found, _) = listed(&mut routed, None, Sort::Useful, false);
    assert_eq!(found[2..], ["Shared title".to_owned(), "Local linked".to_owned()]);

    let (found, _) = listed(&mut routed, None, Sort::LastUseful, true);
    assert_eq!(found[..2], ["Shared title".to_owned(), "Local linked".to_owned()]);

    let page = routed
        .list(&ListQuery {
            useful: Some(UsefulFilter::Ever),
            ..query(Sort::Useful, true, 1, 1)
        })
        .expect("list");
    assert_eq!((titles(&page), page.total), (vec!["Shared title".to_owned()], 2));
}

#[test]
fn a_busy_remote_drops_its_counts_only_and_stays_in_use() {
    let machine = Machine::new(true);
    let mut routed = machine.process();
    let remote = id_of(&mut routed, "Remote global");
    let local = id_of(&mut routed, "Local global");

    machine.net.set_busy(true);
    assert!(matches!(routed.record_shown(&[remote, local]), Err(StoreError::Busy)));
    assert!(matches!(
        routed.record_useful(&[remote, local], 7),
        Err(StoreError::Busy)
    ));
    assert!(!routed.offline(), "a busy remote is not a lost one");
    assert_eq!(
        usage(&mut routed, local).map(|usage| (usage.shown, usage.useful)),
        Some((1, 1)),
        "this computer counted"
    );

    machine.net.set_busy(false);
    routed.record_shown(&[remote]).expect("the same handle counts again");
    assert_eq!(usage(&mut routed, remote).map(|usage| usage.shown), Some(1));
}

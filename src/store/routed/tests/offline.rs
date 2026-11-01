//! A remote database that goes away and comes back: the routed store answers from this computer
//! meanwhile, does not wait for the remote again until its cooldown ends, and never writes to this
//! computer what was meant for the remote.

use pretty_assertions::assert_eq;

use super::flaky::{COOLDOWN, Flaky, Machine, Net};
use super::*;
use crate::backend::OpenError;
use crate::store::testing::store;
use crate::store::{Detail, Link, Reach, StoreError};
use std::time::Duration;

fn all_titles(routed: &mut RoutedStore) -> Vec<String> {
    sorted_titles(routed)
}

const LOCAL_ONLY: [&str; 2] = ["Local global", "Local linked"];
const EVERYTHING: [&str; 4] = ["Local global", "Local linked", "Remote global", "Remote linked"];

#[test]
fn an_unreachable_remote_leaves_every_read_to_this_computer_and_costs_one_attempt_per_cooldown() {
    let machine = Machine::new(false);
    let mut routed = machine.process();

    assert_eq!(machine.net.dials(), 1, "dialled once, when the store was opened");
    assert!(routed.offline());

    assert_eq!(all_titles(&mut routed), LOCAL_ONLY);
    assert_eq!(routed.list(&query(Sort::Title, false, 0, 10)).expect("list").total, 2);
    assert_eq!(search_titles(&mut routed, "gateway").len(), 2);
    assert_eq!(routed.repos().expect("repos").global, 1);
    assert_eq!(routed.stats().expect("stats").documents, 2);
    assert!(routed.offline());
    assert_eq!(machine.net.dials(), 1, "the cooldown skips the remote");

    std::thread::sleep(COOLDOWN + Duration::from_millis(100));
    assert_eq!(all_titles(&mut routed), LOCAL_ONLY);
    assert_eq!(machine.net.dials(), 2, "tried again once the cooldown ended");

    assert_eq!(all_titles(&mut routed), LOCAL_ONLY);
    assert_eq!(machine.net.dials(), 2, "and the new failure starts a new cooldown");
}

#[test]
fn a_remote_that_comes_back_is_used_again_and_its_cooldown_is_cleared() {
    let machine = Machine::new(false);
    let mut routed = machine.process();
    assert_eq!(all_titles(&mut routed), LOCAL_ONLY);

    machine.net.set(true);
    assert_eq!(all_titles(&mut routed), LOCAL_ONLY, "still cooling down");
    assert!(routed.offline());

    std::thread::sleep(COOLDOWN + Duration::from_millis(100));
    assert_eq!(all_titles(&mut routed), EVERYTHING);
    assert!(!routed.offline());
    assert_eq!(search_titles(&mut routed, "gateway").len(), 4);

    let again = machine.process();
    assert!(!again.offline(), "no cooldown is left for the next process");
    assert_eq!(
        machine.net.dials(),
        3,
        "the next process opened the remote straight away"
    );
}

#[test]
fn a_connection_lost_during_a_call_leaves_the_remote_out_of_the_answer() {
    let machine = Machine::new(true);
    let mut routed = machine.process();

    assert_eq!(all_titles(&mut routed), EVERYTHING);
    assert!(!routed.offline());

    machine.net.set(false);
    assert_eq!(
        all_titles(&mut routed),
        LOCAL_ONLY,
        "the list falls back in the same call"
    );
    assert!(routed.offline());

    machine.net.set(true);
    std::thread::sleep(COOLDOWN + Duration::from_millis(100));
    assert_eq!(all_titles(&mut routed), EVERYTHING);

    // A search runs over the places that are left when one is lost during it.
    machine.net.set(false);
    assert_eq!(search_titles(&mut routed, "gateway").len(), 2);
    assert!(routed.offline());
    assert_eq!(all_titles(&mut routed), LOCAL_ONLY);
}

#[test]
fn another_process_skips_a_remote_the_first_found_unreachable_until_the_cooldown_ends() {
    let machine = Machine::new(false);
    let first = machine.process();
    assert!(first.offline());
    assert_eq!(machine.net.dials(), 1);

    machine.net.set(true);
    let mut second = machine.process();
    assert!(second.offline(), "the file says it is down");
    assert_eq!(machine.net.dials(), 1, "so the second process did not dial");
    assert_eq!(all_titles(&mut second), LOCAL_ONLY);

    std::thread::sleep(COOLDOWN + Duration::from_millis(100));
    assert_eq!(all_titles(&mut second), EVERYTHING);
    assert_eq!(machine.net.dials(), 2);
}

#[test]
fn a_document_of_the_remote_is_refused_clearly_while_one_of_this_computer_is_still_read() {
    let machine = Machine::new(false);
    let mut routed = machine.process();
    let local = routed
        .list(&query(Sort::Title, false, 0, 10))
        .expect("list")
        .items
        .iter()
        .find(|row| row.doc.title == "Local global")
        .expect("local document")
        .doc
        .id;

    let remote = 1;
    let refusal = |result: Result<(), StoreError>| match result {
        Err(StoreError::Unreachable(message)) => message,
        other => panic!("not refused: {other:?}"),
    };

    assert_eq!(
        refusal(routed.get(remote, &Detail::default()).map(drop)),
        "remote knowledge is unreachable right now"
    );
    assert_eq!(
        refusal(routed.matching_chunks(remote, "gateway", 5).map(drop)),
        "remote knowledge is unreachable right now"
    );
    assert!(routed.get(local, &Detail::default()).expect("local get").is_some());
    assert!(
        routed
            .matching_chunks(local, "gateway", 5)
            .expect("local matches")
            .total
            > 0
    );
}

#[test]
fn a_write_meant_for_the_remote_is_refused_and_never_lands_on_this_computer() {
    let machine = Machine::new(false);
    let mut routed = machine.process();
    let (doc, chunks, _) = Seed::new("Fresh").chunk("a", "fresh text").parts();
    let link = Link {
        repo_key: REPO.to_owned(),
        globs: Vec::new(),
    };

    let write_refused = |result: Result<(), StoreError>| match result {
        Err(StoreError::Unreachable(message)) => assert!(message.starts_with(REMOTE_DOWN), "{message}"),
        other => panic!("not refused: {other:?}"),
    };

    // A new document goes remote by default in remote+local.
    write_refused(routed.put_document(&doc, &chunks, &[]).map(drop));
    write_refused(
        routed
            .put_document(&doc, &chunks, std::slice::from_ref(&link))
            .map(drop),
    );
    write_refused(routed.rename(1, "Renamed").map(drop));
    write_refused(routed.set_links(1, &[]).map(drop));
    write_refused(routed.delete(&[1]).map(drop));
    write_refused(routed.set_meta("model_id", "x"));

    assert_eq!(all_titles(&mut routed), LOCAL_ONLY, "nothing was saved here instead");

    // Writes to this computer go on.
    let local = ids::tag(Place::Local, 1);
    assert!(routed.rename(local, "Mine").expect("local rename"));
}

#[test]
fn a_remote_that_is_alone_is_not_replaced_and_its_failure_is_the_calls() {
    let net = Net::new(false);
    let remote = Flaky {
        inner: store(),
        net: net.clone(),
    };
    let routing = Routing::new(Where::Remote, Where::Remote);
    let mut routed = RoutedStore::new(None, Some(Box::new(remote)), routing);

    let error = routed.list(&query(Sort::Title, false, 0, 10)).expect_err("fails");
    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
    assert!(!routed.offline(), "a store that was given open does not fall back");
}

#[test]
fn a_failure_that_is_not_about_reaching_the_remote_is_an_error_not_an_offline_answer() {
    let remote = Remote::lazy(
        || {
            Err(OpenError::Missing {
                code: "no-pgvector",
                message: "no pgvector".to_owned(),
            })
        },
        Reach::default(),
    );
    let routing = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
    let mut routed = RoutedStore::new(Some(Box::new(store())), None, routing).with_remote(remote);

    assert!(matches!(
        routed.settle(),
        Err(OpenError::Missing {
            code: "no-pgvector",
            ..
        })
    ));

    let error = routed.list(&query(Sort::Title, false, 0, 10)).expect_err("fails");
    assert!(
        matches!(
            error,
            StoreError::Missing {
                code: "no-pgvector",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!routed.offline());
}

#[test]
fn a_list_narrowed_to_a_place_pages_and_counts_that_place_and_asks_the_remote_only_when_it_is_asked() {
    let machine = Machine::new(true);
    let mut routed = machine.process();
    let page = |routed: &mut RoutedStore, place: Option<Place>, offset: usize, limit: usize| {
        routed
            .list_in(place, &query(Sort::Title, false, offset, limit))
            .expect("list")
    };

    let remote = page(&mut routed, Some(Place::Remote), 0, 10);
    assert_eq!(titles(&remote), ["Remote global", "Remote linked"]);
    assert_eq!(remote.total, 2);

    let second = page(&mut routed, Some(Place::Remote), 1, 1);
    assert_eq!((titles(&second), second.total), (vec!["Remote linked".to_owned()], 2));

    let local = page(&mut routed, Some(Place::Local), 0, 10);
    assert_eq!(titles(&local), LOCAL_ONLY);
    assert_eq!(local.total, 2);

    let all = page(&mut routed, None, 0, 10);
    assert_eq!(all.total, 4);

    // The remote goes away: the local set still lists, the remote one is an error.
    machine.net.set(false);
    let local = page(&mut routed, Some(Place::Local), 0, 10);
    assert_eq!(titles(&local), LOCAL_ONLY);

    let error = routed
        .list_in(Some(Place::Remote), &query(Sort::Title, false, 0, 10))
        .expect_err("refused");
    assert!(
        matches!(&error, StoreError::Unreachable(message) if message == REMOTE_DOWN),
        "{error:?}"
    );
}

#[test]
fn a_remote_lost_during_a_list_of_the_remote_alone_is_an_error_not_an_empty_list() {
    let machine = Machine::new(true);
    let mut routed = machine.process();

    machine.net.set(false);
    let error = routed
        .list_in(Some(Place::Remote), &query(Sort::Title, false, 0, 10))
        .expect_err("not an empty list");

    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
}

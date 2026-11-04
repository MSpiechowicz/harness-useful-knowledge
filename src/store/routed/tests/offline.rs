//! A remote database that goes away and comes back: the routed store answers from this computer
//! meanwhile, does not wait for the remote again until its cooldown ends, and never writes to this
//! computer what was meant for the remote.

use pretty_assertions::assert_eq;

use super::flaky::{COOLDOWN, Flaky, Machine, Net};
use super::*;
use crate::backend::OpenError;
use crate::config::Secrets;
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
    write_refused(routed.rename(1, "Renamed", None, 0).map(drop));
    write_refused(routed.set_links(1, &[]).map(drop));
    write_refused(routed.delete(&[1]).map(drop));
    write_refused(routed.set_meta("model_id", "x"));

    assert_eq!(all_titles(&mut routed), LOCAL_ONLY, "nothing was saved here instead");

    // Writes to this computer go on.
    let local = ids::tag(Place::Local, 1);
    assert!(routed.rename(local, "Mine", None, 0).expect("local rename"));
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
        |_patience| {
            Err(OpenError::Missing {
                code: "no-pgvector",
                message: "no pgvector".to_owned(),
            })
        },
        Reach::default(),
        flaky::PROBE,
        Secrets::default(),
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

#[test]
fn a_probe_that_only_ran_out_of_time_does_not_turn_away_a_reader_that_waits_longer() {
    let machine = Machine::new(false);
    machine.net.set_lag(Duration::from_millis(80));

    // A hook that waits 50 ms and does not get its answer in time.
    let mut hook = machine.process_probing(Duration::from_millis(50));
    assert!(hook.offline());
    assert_eq!(machine.net.dials(), 1);

    // Another hook would not wait longer either, so it does not dial.
    let again = machine.process_probing(Duration::from_millis(50));
    assert!(again.offline());
    assert_eq!(machine.net.dials(), 1);

    // A reader that waits seconds is not turned away by that: the remote has come up meanwhile.
    machine.net.set(true);
    let mut reader = machine.process_probing(Duration::from_secs(3));
    assert!(!reader.offline(), "the remote was tried and answered");
    assert_eq!(machine.net.dials(), 2);
    assert_eq!(all_titles(&mut reader), EVERYTHING);

    // The hook that wrote the record is itself still within its cooldown, until the remote answers.
    assert_eq!(all_titles(&mut hook), LOCAL_ONLY);
}

#[test]
fn a_refusal_is_a_fact_for_every_reader_whatever_it_waits() {
    let machine = Machine::new(false);
    let _first = machine.process_probing(Duration::from_millis(50));
    assert_eq!(machine.net.dials(), 1);

    let patient = machine.process_probing(Duration::from_secs(3));
    assert!(patient.offline());
    assert_eq!(
        machine.net.dials(),
        1,
        "a refused connection is refused for a slow reader too"
    );
}

#[test]
fn a_write_tries_the_remote_even_while_its_cooldown_runs_and_goes_on_once_it_answers() {
    let machine = Machine::new(false);
    let mut routed = machine.process();
    assert!(routed.offline());
    assert_eq!(machine.net.dials(), 1);

    // Still down: the write tries again, the cooldown notwithstanding, and is refused.
    let refused = routed.rename(1, "Renamed remote", None, 0).expect_err("down");
    assert!(
        matches!(&refused, StoreError::Unreachable(message) if message == REMOTE_DOWN_WRITE),
        "{refused:?}"
    );
    assert_eq!(machine.net.dials(), 2, "the write dialled although the cooldown ran");

    // Back up, but a read would still be cooling down for a minute: the write is not refused.
    machine.net.set(true);
    assert_eq!(all_titles(&mut routed), LOCAL_ONLY, "reads honour the cooldown");

    assert!(routed.rename(1, "Renamed remote", None, 0).expect("renamed"));
    assert!(!routed.offline(), "the answer ends the fallback");
    assert!(all_titles(&mut routed).contains(&"Renamed remote".to_owned()));
    assert_eq!(machine.net.dials(), 3);
}

#[test]
fn deleting_linking_and_the_metadata_also_wait_for_no_cooldown() {
    let machine = Machine::new(false);
    let mut routed = machine.process();
    machine.net.set(true);
    assert!(routed.offline(), "the cooldown runs");

    routed.set_meta("k", "v").expect("meta is written to the remote");
    assert!(
        routed
            .set_links(
                1,
                &[Link {
                    repo_key: REPO.to_owned(),
                    globs: Vec::new()
                }]
            )
            .expect("links")
    );
    assert_eq!(routed.delete(&[2]).expect("deleted"), 1);
    assert!(!routed.offline());
}

#[test]
fn the_reason_a_connection_was_lost_is_scrubbed_before_it_is_shared() {
    let dir = tempfile::tempdir().expect("dir");
    let reach = Reach::beside_with(&dir.path().join("knowledge.sqlite3"), "remote:test", COOLDOWN);
    let secrets = Secrets::of("postgres://u:hunter2-secret@db/knowledge", None);
    let mut remote = Remote::lazy(|_| Ok(None), reach.clone(), flaky::PROBE, secrets);

    let lost = StoreError::Unreachable("lost postgres://u:hunter2-secret@db/knowledge".to_owned());
    assert!(remote.lost(&lost));

    let kept = reach.cooling().expect("cooling");
    let file = std::fs::read_to_string(dir.path().join("remote-unreachable.json")).expect("file");

    for text in [kept, file] {
        assert!(!text.contains("hunter2-secret"), "{text}");
    }
}

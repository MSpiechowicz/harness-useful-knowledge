//! An approval that replaces a document goes to the place that document is in, whatever the
//! document's number is in the other place.

use super::*;
use crate::config::Where;

const BOTH: Routing = Routing {
    repository: Where::RemoteLocal,
    global: Where::RemoteLocal,
};

/// A computer whose own file holds one document (id 1, "Mine") and whose remote database is never
/// reached.
fn computer() -> World {
    let world = World::new();
    let mut local = SqliteStore::open(&world.dir.path().join("knowledge").join(FILE_NAME)).expect("local file");
    insert(&mut local, &Seed::new("Mine").chunk("Intro", "gateway notes of mine"));

    world
}

fn approve(world: &World, routing: Routing, held: &str, extra: &[&str]) -> Reply {
    let mut args = vec!["approve", "--json", held];
    args.extend_from_slice(extra);
    let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

    reply(&world.env, &unreachable_remote(), routing, &args)
}

fn mine(world: &World) -> String {
    let detail = world
        .store()
        .get(1, &crate::store::Detail::default())
        .expect("get")
        .expect("document 1");

    detail.doc.title
}

#[test]
fn a_place_that_disagrees_with_the_document_the_proposal_replaces_is_refused_and_nothing_is_touched() {
    let world = computer();
    let waiting = |held: &str| world.held().get(held).expect("get").is_some();

    // Document 1 in a place of two is the remote database's. Saving to this computer would overwrite
    // this computer's document 1 instead, so it is refused.
    let remote_one = world.propose("Replacement", TEXT, Vec::new(), Some(1));
    let Reply::Refused(Code::Refused, message) = approve(&world, BOTH, &remote_one, &["--place", "local"]) else {
        panic!("not refused");
    };
    assert!(message.contains("in the remote database"), "{message}");
    assert_eq!(mine(&world), "Mine", "this computer's document 1 is untouched");
    assert!(waiting(&remote_one), "the proposal waits for another try");

    let local_one = world.propose(
        "Another",
        "Another zebrafish text.",
        Vec::new(),
        Some(crate::store::routed::ids::tag(Place::Local, 1)),
    );
    let Reply::Refused(Code::Refused, message) = approve(&world, BOTH, &local_one, &["--place", "remote"]) else {
        panic!("not refused");
    };
    assert!(message.contains("on this computer"), "{message}");
    assert_eq!(mine(&world), "Mine");
    assert!(waiting(&local_one));
}

#[test]
fn a_replacement_of_a_document_on_this_computer_updates_it_there_without_being_told_the_place() {
    let world = computer();
    let held = world.propose(
        "Gateway runbook",
        TEXT,
        Vec::new(),
        Some(crate::store::routed::ids::tag(Place::Local, 1)),
    );

    let Reply::Done(done) = approve(&world, BOTH, &held, &[]) else {
        panic!("approved");
    };

    assert_eq!(done["approved"]["status"], "updated");
    assert_eq!(done["approved"]["docId"], json!("local:1"));
    assert_eq!(mine(&world), "Gateway runbook");
    assert_eq!(world.store().stats().expect("stats").documents, 1);
}

#[test]
fn an_id_that_names_a_place_is_refused_while_only_one_place_is_in_use() {
    let world = computer();
    let held = world.propose(
        "Replacement",
        TEXT,
        Vec::new(),
        Some(crate::store::routed::ids::tag(Place::Local, 1)),
    );

    let Reply::Refused(Code::Refused, message) = approve(&world, Routing::local(), &held, &[]) else {
        panic!("not refused");
    };

    assert!(message.contains("both in use"), "{message}");
    assert_eq!(mine(&world), "Mine");
}

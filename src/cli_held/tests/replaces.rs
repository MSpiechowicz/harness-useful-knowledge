//! An approval that replaces a document goes to the place that document is in, whatever the
//! document's number is in the other place.

use super::*;
use crate::config::Where;
use crate::held::Replaced;
use crate::store::KnowledgeStore;

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
fn an_id_naming_the_only_place_in_use_replaces_there_and_one_naming_the_other_place_is_refused() {
    let world = computer();
    let local_one = crate::store::routed::ids::tag(Place::Local, 1);

    let held = world.propose("Replacement", TEXT, Vec::new(), Some(local_one));
    let Reply::Done(done) = approve(&world, Routing::local(), &held, &[]) else {
        panic!("approved");
    };
    assert_eq!(done["approved"]["status"], "updated");
    assert_eq!(mine(&world), "Replacement");

    // The remote database alone: this computer's document is not a place in use any more.
    let held = world.propose("Another", "Another zebrafish text.", Vec::new(), Some(local_one));
    let remote_only = Routing::new(Where::Remote, Where::Remote);
    let Reply::Refused(Code::Refused, message) = approve(&world, remote_only, &held, &[]) else {
        panic!("not refused");
    };
    assert!(message.contains("not in use"), "{message}");
    assert_eq!(mine(&world), "Replacement", "this computer's document is untouched");
    assert!(world.held().get(&held).expect("get").is_some(), "the proposal waits");
}

impl World {
    /// The document `id` of this computer's file as `knowledge_add` records it when it holds a
    /// proposal, `None` when there is no such document.
    pub(super) fn replaced(&self, id: i64) -> Option<Replaced> {
        let mut store = backend::open(&self.env, &Backend::Sqlite, Routing::local(), Access::Read).ok()??;
        let doc = store.get(id, &crate::store::Detail::default()).ok()??.doc;

        Some(Replaced {
            content_sha256: doc.content_sha256,
            source_key: doc.source_key,
        })
    }
}

#[test]
fn a_proposal_does_not_replace_a_document_that_took_the_number_of_the_one_it_was_about() {
    let world = World::new();
    let file = world.dir.path().join("knowledge").join(FILE_NAME);
    let mut local = SqliteStore::open(&file).expect("local file");
    let first = insert(&mut local, &Seed::new("Mine").chunk("Intro", "gateway notes of mine"));
    assert_eq!(first, 1);

    let held = world.propose(
        "Replacement",
        TEXT,
        Vec::new(),
        Some(crate::store::routed::ids::tag(Place::Local, first)),
    );

    // The document is deleted and another one is imported under the same number (a database
    // that reuses row numbers does this).
    local.delete(&[first]).expect("delete");
    let other = insert(
        &mut local,
        &Seed::new("Someone else's").chunk("Intro", "zebrafish notes"),
    );
    drop(local);

    let connection = rusqlite::Connection::open(&file).expect("raw connection");
    connection
        .execute_batch(&format!(
            "PRAGMA foreign_keys = OFF;
             UPDATE knowledge_chunks SET doc_id = {first} WHERE doc_id = {other};
             UPDATE knowledge_documents SET id = {first} WHERE id = {other};"
        ))
        .expect("the new document takes the old number");
    drop(connection);

    let Reply::Refused(Code::Refused, message) = approve_local(&world, &held) else {
        panic!("not refused");
    };
    assert!(message.contains("changed or was deleted"), "{message}");
    assert_eq!(mine(&world), "Someone else's", "nothing was overwritten");
    assert!(world.held().get(&held).expect("get").is_some(), "the proposal waits");
}

#[test]
fn a_proposal_for_a_document_that_was_deleted_is_refused() {
    let world = World::new();
    let mut local = SqliteStore::open(&world.dir.path().join("knowledge").join(FILE_NAME)).expect("local file");
    let first = insert(&mut local, &Seed::new("Mine").chunk("Intro", "gateway notes of mine"));
    let held = world.propose(
        "Replacement",
        TEXT,
        Vec::new(),
        Some(crate::store::routed::ids::tag(Place::Local, first)),
    );

    local.delete(&[first]).expect("delete");
    drop(local);

    let Reply::Refused(Code::Refused, message) = approve_local(&world, &held) else {
        panic!("not refused");
    };
    assert!(message.contains("changed or was deleted"), "{message}");
    assert_eq!(
        world.store().stats().expect("stats").documents,
        0,
        "no document was created"
    );
}

fn approve_local(world: &World, held: &str) -> Reply {
    world.run(&["approve", "--json", held])
}

#[test]
fn a_proposal_shows_the_document_it_replaces_as_clients_read_ids() {
    use crate::store::routed::ids;

    let held = |replaces| {
        HeldItem::new(
            Source {
                client: "tests".to_owned(),
                session: None,
                project_path: None,
                repo_key: None,
            },
            Proposal {
                title: "T".to_owned(),
                format: "markdown".to_owned(),
                name: "t.md".to_owned(),
                source_path: None,
                content: "text".to_owned(),
                links: Vec::new(),
                replaces: Some(replaces),
                replaced: None,
            },
        )
    };

    assert_eq!(
        crate::cli_held::public(&held(ids::tag(crate::config::Place::Remote, 12)))["replaces"],
        serde_json::json!(12)
    );
    assert_eq!(
        crate::cli_held::public(&held(ids::tag(crate::config::Place::Local, 12)))["replaces"],
        serde_json::json!("local:12")
    );
}

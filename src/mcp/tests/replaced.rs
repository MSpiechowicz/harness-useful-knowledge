//! A proposal that replaces a document keeps that document's identity, so the approval can tell it
//! from another document that later takes the same number.

use super::*;

#[test]
fn a_proposal_records_the_text_hash_and_key_of_the_document_it_replaces() {
    let mut world = World::new();
    let project = project_arg(&world);
    let seed = Seed::new("Mine")
        .source("path:/docs/mine.md")
        .chunk("Intro", "gateway notes");
    let id = insert(&mut world.database(), &seed);
    let stored = world
        .database()
        .get(id, &crate::store::Detail::default())
        .expect("get")
        .expect("the document")
        .doc;

    world.ok(
        "knowledge_add",
        json!({ "title": "Mine, again", "content": "New text.", "replaces": id, "project_path": project }),
    );

    let item = held_item(&world);
    assert_eq!(item["replaced"]["contentSha256"], json!(stored.content_sha256));
    assert_eq!(item["replaced"]["sourceKey"], json!("path:/docs/mine.md"));
}

#[test]
fn a_proposal_for_a_document_that_does_not_exist_is_not_held() {
    let mut world = World::new();
    let project = project_arg(&world);
    insert(
        &mut world.database(),
        &Seed::new("Mine").chunk("Intro", "gateway notes"),
    );

    for given in [json!(2), json!("local:2")] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "Gone", "content": "Text.", "replaces": given, "project_path": project }),
        );

        assert!(refused.contains("does not exist"), "{given}: {refused}");
    }

    assert!(world.held_files().is_empty(), "nothing is held");
}

fn held_item(world: &World) -> Value {
    let files = world.held_files();
    assert_eq!(files.len(), 1, "{files:?}");

    serde_json::from_slice(&std::fs::read(&files[0]).expect("read")).expect("JSON")
}

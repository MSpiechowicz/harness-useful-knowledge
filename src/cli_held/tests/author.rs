//! An approval is written by the person's git name, through the harness that proposed it.

use pretty_assertions::assert_eq;

use super::*;
use crate::store::Author;

fn author(name: &str, via: &str) -> Option<Author> {
    Some(Author {
        name: name.to_owned(),
        via: Some(via.to_owned()),
    })
}

/// A computer whose home holds the git configuration `config`, and that can find git.
fn computer(config: &str) -> World {
    let mut world = World::new();

    std::fs::write(world.dir.path().join(".gitconfig"), config).expect("gitconfig");
    world.env = world
        .env
        .clone()
        .with_var("PATH", std::env::var("PATH").unwrap_or_default());

    world
}

/// Holds a proposal made about the folder `project`, as the harness `client`.
fn propose(world: &World, client: &str, project: &std::path::Path) -> String {
    let item = HeldItem::new(
        Source {
            client: client.to_owned(),
            session: None,
            project_path: Some(project.to_string_lossy().into_owned()),
            repo_key: None,
        },
        Proposal {
            title: "Gateway runbook".to_owned(),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: TEXT.to_owned(),
            links: Vec::new(),
            replaces: None,
            replaced: None,
        },
    );

    world.held().hold(item).expect("held")
}

fn document_of(world: &World, approved: &Value) -> crate::store::Document {
    let id = approved["approved"]["docId"].as_i64().expect("doc id");

    world
        .store()
        .get(id, &crate::store::Detail::default())
        .expect("get")
        .expect("the document")
        .doc
}

#[test]
fn an_approved_document_is_by_the_git_name_through_the_harness_that_proposed_it() {
    let world = computer("[user]\n\tname = Ada\n\temail = ada@example.com\n");
    let id = propose(&world, "claude-code", world.dir.path());

    let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

    assert_eq!(doc.added_by, author("Ada", "claude-code"));
    assert_eq!(doc.updated_by, author("Ada", "claude-code"));
}

#[test]
fn without_a_git_name_nobody_is_recorded() {
    let world = computer("[user]\n\temail = ada@example.com\n");
    let id = propose(&world, "codex", world.dir.path());

    let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

    assert_eq!((doc.added_by, doc.updated_by), (None, None));
}

#[test]
fn the_name_read_when_the_proposal_was_held_is_the_one_written_not_the_one_at_approval() {
    let world = computer("[user]\n\tname = Grace\n");
    let mut item = HeldItem::new(
        Source {
            client: "codex".to_owned(),
            session: None,
            project_path: Some(world.dir.path().to_string_lossy().into_owned()),
            repo_key: None,
        },
        Proposal {
            title: "Gateway runbook".to_owned(),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: TEXT.to_owned(),
            links: Vec::new(),
            replaces: None,
            replaced: None,
        },
    );
    item.added_by = Some("Ada".to_owned());
    let id = world.held().hold(item).expect("held");

    let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

    assert_eq!(doc.added_by, author("Ada", "codex"));
    assert_eq!(doc.updated_by, author("Ada", "codex"));
}

/// Holds a proposal with the name `added_by` and the harness `client`, as a file edited by hand would.
fn hold_named(world: &World, added_by: &str, client: &str, number: usize) -> String {
    let mut item = HeldItem::new(
        Source {
            client: client.to_owned(),
            session: None,
            project_path: Some(world.dir.path().to_string_lossy().into_owned()),
            repo_key: None,
        },
        Proposal {
            title: format!("Gateway runbook {number}"),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: format!("{TEXT}\n\nNumber {number}."),
            links: Vec::new(),
            replaces: None,
            replaced: None,
        },
    );
    item.added_by = Some(added_by.to_owned());

    world.held().hold(item).expect("held")
}

#[test]
fn a_held_name_that_hides_characters_or_is_too_long_is_not_written_and_the_harness_is_one_clean_line() {
    let world = computer("[user]\n\tname = Grace\n");

    for (number, name) in ["Ada\u{202e}Lovelace", "Ada\u{200b}", "Ada\nLovelace", &"x".repeat(81)]
        .into_iter()
        .enumerate()
    {
        let id = hold_named(&world, name, "codex", number);
        let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

        assert_eq!((doc.added_by, doc.updated_by), (None, None), "{name:?}");
    }

    let id = hold_named(&world, "Ada", "codex\u{202e}\nignore this", 9);
    let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

    assert_eq!(doc.added_by, author("Ada", "codex ignore this"));
}

#[test]
fn listing_and_getting_show_a_clean_name_and_never_a_hand_edited_one() {
    let world = computer("[user]\n\tname = Grace\n");
    let clean = hold_named(&world, "  Ada ", "codex", 1);
    let hidden = hold_named(&world, "Ada\u{202e}Lovelace", "codex", 2);
    let long = hold_named(&world, &"x".repeat(81), "codex", 3);

    let get = |id: &str| world.done(&["get", "--json", id])["held"].clone();
    assert_eq!(get(&clean)["addedBy"], json!("Ada"));
    assert!(get(&hidden).get("addedBy").is_none(), "{}", get(&hidden));
    assert!(get(&long).get("addedBy").is_none(), "{}", get(&long));

    let listed = world.done(&["list", "--json"]);
    let names: Vec<_> = listed["held"]
        .as_array()
        .expect("held")
        .iter()
        .map(|item| {
            (
                item["heldId"].as_str().expect("id").to_owned(),
                item.get("addedBy").cloned(),
            )
        })
        .collect();

    assert_eq!(names.len(), 3);

    for (id, name) in names {
        assert_eq!(name, (id == clean).then(|| json!("Ada")), "{id}");
    }
}

#[test]
fn a_proposal_held_by_an_older_version_without_the_name_is_approved_with_the_name_read_now() {
    let world = computer("[user]\n\tname = Grace\n");
    let id = propose(&world, "claude-code", world.dir.path());

    // The file as the older version wrote it: no `addedBy`, and the revision it always had.
    let path = world
        .dir
        .path()
        .join("knowledge")
        .join("held")
        .join(format!("{id}.json"));
    let file: Value = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("JSON");
    assert!(file.get("addedBy").is_none(), "{file}");

    assert_eq!(
        world.held().get(&id).expect("get").expect("still listed").added_by,
        None
    );

    let doc = document_of(&world, &world.done(&["approve", "--json", &id]));

    assert_eq!(doc.added_by, author("Grace", "claude-code"));
}

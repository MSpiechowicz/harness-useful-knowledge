//! A proposal the user asked to change is answered by naming it with `revises`.

use super::*;
use crate::cli_held;
use crate::config::Routing;

fn held_item(world: &World, id: &str) -> Value {
    let path = world
        .held_files()
        .into_iter()
        .find(|path| path.file_stem().is_some_and(|stem| stem.to_string_lossy() == id));

    serde_json::from_slice(&std::fs::read(path.expect("the held file")).expect("read")).expect("JSON")
}

fn sent_id(text: &str) -> String {
    text.lines()
        .next()
        .and_then(|line| line.strip_prefix("held:"))
        .expect("held:<id>")
        .to_owned()
}

#[test]
fn revises_replaces_the_proposal_the_user_asked_to_change_under_its_id() {
    let mut world = World::with(Fake(Ok(axis(0))), Some("chat-7"));
    let project = project_arg(&world);

    let first = sent_id(&world.ok(
        "knowledge_add",
        json!({ "title": "Gateway runbook", "content": "# Runbook\n\nRestart it.", "project_path": project }),
    ));

    let changed = cli_held::reply(
        &world.server.env,
        &Backend::Sqlite,
        Routing::local(),
        &[
            Some("request-changes"),
            Some("--json"),
            Some(&first),
            Some("--comment"),
            Some("Name the steps"),
        ],
    );
    assert!(matches!(changed, cli_held::Reply::Done(_)), "{changed:?}");

    // A new title, so only the name ties it to the proposal.
    let text = world.ok(
        "knowledge_add",
        json!({
            "title": "Gateway restart runbook",
            "content": "# Runbook\n\n1. Stop it.\n2. Start it.",
            "project_path": project,
            "revises": format!("held:{first}"),
        }),
    );
    assert_eq!(sent_id(&text), first, "{text}");
    assert_eq!(
        text.lines().nth(1),
        Some("Sent to review in Harness Desktop → Knowledge.")
    );
    assert_eq!(world.held_files().len(), 1);

    let item = held_item(&world, &first);
    assert_eq!(item["title"], "Gateway restart runbook");
    assert!(item.get("changesRequested").is_none(), "the request is answered");
}

#[test]
fn revises_must_name_a_proposal_and_one_that_asked_for_nothing_is_not_replaced() {
    let mut world = World::with(Fake(Ok(axis(0))), Some("chat-7"));
    let project = project_arg(&world);

    for bad in ["", "held:", "12", "../etc/passwd"] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "A", "content": "text a", "project_path": project, "revises": bad }),
        );
        assert!(refused.contains("revises"), "{bad:?}: {refused}");
    }

    let first = sent_id(&world.ok(
        "knowledge_add",
        json!({ "title": "First", "content": "text one", "project_path": project }),
    ));
    let second = sent_id(&world.ok(
        "knowledge_add",
        json!({ "title": "Second", "content": "text two", "project_path": project, "revises": first }),
    ));

    assert_ne!(first, second, "no change was asked for, so it is a proposal of its own");
    assert_eq!(world.held_files().len(), 2);

    // The agent is told it is a new proposal, and nothing of the one it named.
    let told = world.ok(
        "knowledge_add",
        json!({ "title": "Third", "content": "text three", "project_path": project, "revises": second }),
    );
    let said = told.lines().nth(1).expect("a second line");
    assert!(said.starts_with("Held as a new proposal"), "{told}");
    assert!(!said.contains("Second") && !said.contains(&second), "{told}");

    // A well-formed id nobody holds is the same.
    let told = world.ok(
        "knowledge_add",
        json!({
            "title": "Fourth",
            "content": "text four",
            "project_path": project,
            "revises": "held:00000000-0000-4000-8000-000000000000",
        }),
    );
    assert!(
        told.lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("Held as a new proposal")),
        "{told}"
    );
}

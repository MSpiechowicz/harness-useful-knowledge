//! A proposal records the git name of the person it is for when it is held.

use super::*;

#[test]
fn a_proposal_holds_the_git_name_of_the_person_it_is_written_for() {
    let mut world = World::new();
    std::fs::write(
        world.home().join(".gitconfig"),
        "[user]\n\tname = Ada\n\temail = ada@example.com\n",
    )
    .expect("gitconfig");
    world.server.env = world
        .server
        .env
        .clone()
        .with_var("PATH", std::env::var("PATH").unwrap_or_default());

    world.ok(
        "knowledge_add",
        json!({ "title": "Runbook", "content": "# Runbook\n\nRestart it.", "project_path": project_arg(&world) }),
    );

    let files = world.held_files();
    let item: Value = serde_json::from_slice(&std::fs::read(&files[0]).expect("read")).expect("JSON");

    assert_eq!(item["addedBy"], "Ada");
    assert!(!item.to_string().contains("ada@example.com"), "never the email");
}

#[test]
fn without_a_git_name_the_proposal_has_no_name() {
    let mut world = World::new();

    world.ok(
        "knowledge_add",
        json!({ "title": "Runbook", "content": "# Runbook\n\nRestart it.", "project_path": project_arg(&world) }),
    );

    let files = world.held_files();
    let item: Value = serde_json::from_slice(&std::fs::read(&files[0]).expect("read")).expect("JSON");

    assert!(item.get("addedBy").is_none(), "{item}");
}

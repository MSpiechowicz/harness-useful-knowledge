use super::*;

fn held_item(world: &World) -> Value {
    let files = world.held_files();
    assert_eq!(files.len(), 1, "{files:?}");

    serde_json::from_slice(&std::fs::read(&files[0]).expect("read")).expect("JSON")
}

#[test]
fn a_proposal_goes_to_the_review_queue_and_never_to_the_database() {
    let mut world = World::with(Fake(Ok(axis(0))), Some("chat-7"));
    world.server.handle(
        json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "clientInfo": { "name": "codex" } } }),
    );

    let text = world.ok(
        "knowledge_add",
        json!({ "title": "Gateway runbook", "content": "# Runbook\n\nRestart it.", "project_path": project_arg(&world) }),
    );

    let (line, sent) = text.split_once('\n').expect("two lines");
    let id = line.strip_prefix("held:").expect("held:<id>");
    assert_eq!(sent, "Sent to review in Harness Desktop → Knowledge.");
    assert!(crate::held::is_held_id(id), "{id}");

    let item = held_item(&world);
    assert_eq!(item["heldId"], id);
    assert_eq!(item["title"], "Gateway runbook");
    assert_eq!(item["content"], "# Runbook\n\nRestart it.");
    assert_eq!(item["format"], "markdown");
    assert_eq!(item["client"], "codex");
    assert_eq!(item["session"], "chat-7");
    assert_eq!(item["projectPath"], project_arg(&world));
    assert!(
        item["repoKey"].as_str().is_some_and(|key| key.starts_with("path:")),
        "{item}"
    );
    assert_eq!(
        item["links"][0]["repo"], item["repoKey"],
        "linked to the project's repository by default"
    );

    assert!(
        !world.knowledge().join(FILE_NAME).exists(),
        "the database was not even created"
    );
}

#[test]
fn an_existing_database_gets_no_row_from_a_proposal() {
    let mut world = World::new();
    let mut database = world.database();
    insert(&mut database, &Seed::new("Existing").chunk("A", "text"));
    let before = database.stats().expect("stats");

    world.ok(
        "knowledge_add",
        json!({ "title": "New", "content": "Some text.", "project_path": project_arg(&world) }),
    );

    let after = database.stats().expect("stats");
    assert_eq!(
        (before.documents, before.chunks, before.generation),
        (after.documents, after.chunks, after.generation)
    );
}

#[test]
fn links_are_the_repository_by_default_a_global_document_when_empty_and_checked() {
    let mut world = World::new();
    let project = project_arg(&world);
    let held = |world: &World| {
        let mut found: Vec<Value> = world
            .held_files()
            .iter()
            .map(|path| serde_json::from_slice(&std::fs::read(path).expect("read")).expect("JSON"))
            .collect();
        found.sort_by(|a, b| a["title"].as_str().cmp(&b["title"].as_str()));
        found
    };

    world.ok(
        "knowledge_add",
        json!({ "title": "A", "content": "text a", "project_path": project, "links": [] }),
    );
    world.ok(
        "knowledge_add",
        json!({
            "title": "B", "content": "text b", "project_path": project,
            "links": [{ "globs": ["src/api/**"] }, { "repo": "git:github.com/o/other" }],
        }),
    );

    let items = held(&world);
    assert_eq!(items[0]["links"], json!([]));
    assert_eq!(items[1]["links"][0]["globs"], json!(["src/api/**"]));
    assert_eq!(items[1]["links"][1]["repo"], "git:github.com/o/other");

    let refused = world.err(
        "knowledge_add",
        json!({ "title": "C", "content": "text c", "project_path": project, "links": [{ "repo": "not a key" }] }),
    );
    assert!(refused.contains("repository"), "{refused}");
    assert_eq!(world.held_files().len(), 2);
}

#[test]
fn a_path_inside_the_project_is_read_now_and_kept_as_it_was() {
    let mut world = World::new();
    let file = world.project().join("docs").join("notes.md");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("folder");
    std::fs::write(&file, "# Notes\n\nfirst version").expect("write");

    world.ok(
        "knowledge_add",
        json!({ "title": "Notes", "path": "docs/notes.md", "project_path": project_arg(&world) }),
    );
    std::fs::write(&file, "# Notes\n\nsecond version").expect("write");

    let item = held_item(&world);
    assert_eq!(item["content"], "# Notes\n\nfirst version");
    assert_eq!(item["sourcePath"], "docs/notes.md");
    assert_eq!(item["name"], "notes.md");
    assert_eq!(item["format"], "markdown");
}

#[test]
fn a_path_outside_the_project_is_refused_however_it_is_spelled() {
    let mut world = World::new();
    std::fs::write(world.home().join("secret.txt"), "outside the project").expect("write");
    let project = project_arg(&world);
    let outside = world.home().join("secret.txt").to_string_lossy().into_owned();

    for path in [
        "../secret.txt",
        "docs/../../secret.txt",
        outside.as_str(),
        "/etc/passwd",
        "",
    ] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "Secrets", "path": path, "project_path": project }),
        );
        assert!(
            refused.contains("inside the project") || refused.contains("does not exist"),
            "{path}: {refused}"
        );
    }

    assert_eq!(world.held_files().len(), 0);
}

#[test]
fn hidden_files_and_folders_are_never_proposed() {
    let mut world = World::new();
    std::fs::create_dir_all(world.project().join(".ssh")).expect("folder");
    std::fs::write(world.project().join(".ssh").join("notes.md"), "# Notes\n\ntext").expect("write");
    std::fs::write(world.project().join(".env"), "PORT=8080\n").expect("write");
    let project = project_arg(&world);

    for path in [".ssh/notes.md", ".env", "./.env"] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "x", "path": path, "project_path": project }),
        );
        assert!(refused.contains("hidden"), "{path}: {refused}");
    }

    assert_eq!(world.held_files().len(), 0);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_never_followed_not_the_file_and_not_a_folder_on_the_way() {
    let mut world = World::new();
    let outside = world.home().join("outside");
    std::fs::create_dir(&outside).expect("folder");
    std::fs::write(outside.join("secret.md"), "# Secret\n\nnot for the queue").expect("write");

    std::os::unix::fs::symlink(outside.join("secret.md"), world.project().join("link.md")).expect("link file");
    std::os::unix::fs::symlink(&outside, world.project().join("linked-folder")).expect("link folder");
    std::fs::write(world.project().join("inside.md"), "# Inside").expect("write");
    std::os::unix::fs::symlink(world.project().join("inside.md"), world.project().join("alias.md")).expect("alias");

    let project = project_arg(&world);

    for path in ["link.md", "linked-folder/secret.md", "alias.md"] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "x", "path": path, "project_path": project }),
        );
        assert!(refused.contains("symbolic link"), "{path}: {refused}");
    }

    assert_eq!(world.held_files().len(), 0);
}

#[test]
fn a_file_that_is_too_large_binary_a_pdf_or_holds_a_credential_is_refused() {
    let mut world = World::new();
    let project = project_arg(&world);

    std::fs::write(world.project().join("big.md"), "x".repeat(CONTENT_MAX + 1)).expect("write");
    std::fs::write(world.project().join("binary.md"), [0x89, b'P', b'N', b'G', 0, 1, 2]).expect("write");
    std::fs::write(world.project().join("doc.pdf"), "%PDF-1.4 not really").expect("write");

    let refused = |world: &mut World, arguments: Value| world.err("knowledge_add", arguments);

    assert!(
        refused(
            &mut world,
            json!({ "title": "x", "path": "big.md", "project_path": project })
        )
        .contains("larger than 256 KiB")
    );
    assert!(
        refused(
            &mut world,
            json!({ "title": "x", "path": "binary.md", "project_path": project })
        )
        .contains("UTF-8")
    );
    assert!(
        refused(
            &mut world,
            json!({ "title": "x", "path": "doc.pdf", "project_path": project })
        )
        .contains("PDF")
    );
    assert!(
        refused(
            &mut world,
            json!({ "title": "x", "content": "y".repeat(CONTENT_MAX + 1), "project_path": project })
        )
        .contains("larger than 256 KiB")
    );

    // Built at run time: a literal token would trip the secret scanners of the repository host.
    let token = format!("{}{}", "ghp_", "a1B2".repeat(9));
    assert!(
        refused(
            &mut world,
            json!({ "title": "x", "content": format!("use {token} to log in"), "project_path": project })
        )
        .contains("credential")
    );

    assert_eq!(world.held_files().len(), 0);
}

#[test]
fn the_arguments_are_checked() {
    let mut world = World::new();
    let project = project_arg(&world);

    for (arguments, expected) in [
        (
            json!({ "title": "x", "project_path": project }),
            "either content or path",
        ),
        (
            json!({ "title": "x", "content": "a", "path": "b.md", "project_path": project }),
            "either content or path",
        ),
        (
            json!({ "title": "  ", "content": "a", "project_path": project }),
            "title",
        ),
        (
            json!({ "title": "x", "content": "   ", "project_path": project }),
            "no text",
        ),
        (
            json!({ "title": "x", "content": "a", "format": "docx", "project_path": project }),
            "format must be",
        ),
        (
            json!({ "title": "x", "content": "a", "replaces": 0, "project_path": project }),
            "replaces",
        ),
        (
            json!({ "title": "x", "content": "a", "project_path": "relative/dir" }),
            "absolute",
        ),
        (
            json!({ "title": "x", "content": "a", "project_path": project, "extra": 1 }),
            "invalid arguments",
        ),
    ] {
        let refused = world.err("knowledge_add", arguments.clone());
        assert!(refused.contains(expected), "{arguments}: {refused}");
    }

    assert_eq!(world.held_files().len(), 0);
}

#[test]
fn proposing_the_same_document_again_is_the_same_proposal() {
    let mut world = World::new();
    let arguments = json!({ "title": "Same", "content": "same text", "project_path": project_arg(&world) });

    let first = world.ok("knowledge_add", arguments.clone());
    let again = world.ok("knowledge_add", arguments);

    assert_eq!(first, again);
    assert_eq!(world.held_files().len(), 1);
}

#[test]
fn only_a_project_folder_is_read_from_never_the_home_folder_or_a_folder_without_project_files() {
    let mut world = World::new();
    let keys = world.home().join("keys");
    std::fs::create_dir(&keys).expect("folder");
    std::fs::write(keys.join("id_rsa"), "not a credential, just text").expect("write");
    std::fs::write(world.home().join("diary.txt"), "private words").expect("write");

    for (folder, path) in [
        (world.home(), "diary.txt"),
        (keys.clone(), "id_rsa"),
        (std::path::PathBuf::from("/etc"), "hostname"),
    ] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "x", "path": path, "project_path": folder.to_string_lossy() }),
        );

        assert!(refused.contains("root of a project"), "{folder:?}: {refused}");
    }

    assert!(world.held_files().is_empty());

    // A folder with a project file is one.
    std::fs::write(keys.join("Makefile"), "all:").expect("marker");
    world.ok(
        "knowledge_add",
        json!({ "title": "x", "path": "id_rsa", "project_path": keys.to_string_lossy() }),
    );
}

#[test]
fn the_clients_name_is_one_visible_line() {
    let mut world = World::new();
    world.server.handle(json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "clientInfo": { "name": "co\u{202e}de\nx\u{7}" } }
    }));

    world.ok(
        "knowledge_add",
        json!({ "title": "Notes", "content": "some text", "project_path": project_arg(&world) }),
    );

    assert_eq!(held_item(&world)["client"], "code x");
}

#[test]
fn replaces_is_a_document_id_as_the_tools_show_it_and_is_kept_with_its_place() {
    let mut world = World::new();
    let project = project_arg(&world);
    let local_seven = crate::store::routed::ids::tag(crate::config::Place::Local, 7);

    // Only this computer is in use, so each of them names its document 7.
    for (index, (given, kept)) in [
        (json!(7), local_seven),
        (json!("7"), local_seven),
        (json!("local:7"), local_seven),
    ]
    .into_iter()
    .enumerate()
    {
        world.ok(
            "knowledge_add",
            json!({
                "title": format!("Replacement {index}"),
                "content": format!("Text number {index}."),
                "replaces": given,
                "project_path": project,
            }),
        );

        let item: Value = world
            .held_files()
            .iter()
            .map(|path| serde_json::from_slice::<Value>(&std::fs::read(path).expect("read")).expect("JSON"))
            .find(|item| item["title"] == format!("Replacement {index}"))
            .expect("the proposal is held");

        assert_eq!(item["replaces"], json!(kept), "{given}");
    }

    for bad in [
        json!("local:"),
        json!("x"),
        json!(0),
        json!(-3),
        json!(true),
        json!("local:0"),
    ] {
        let refused = world.err(
            "knowledge_add",
            json!({ "title": "x", "content": "a", "replaces": bad, "project_path": project }),
        );
        assert!(
            refused.contains("replaces") || refused.contains("invalid arguments"),
            "{bad}: {refused}"
        );
    }

    assert_eq!(world.held_files().len(), 3);
}

#[test]
fn a_proposal_replaces_the_document_of_the_place_it_named_when_the_routing_changes_before_it_is_approved() {
    use crate::cli_held::{Reply, reply};
    use crate::config::{Routing, Where};
    use crate::location::DIR_VAR;

    let mut world = World::new();
    insert(
        &mut world.database(),
        &Seed::new("Mine").chunk("Intro", "gateway notes of mine"),
    );

    // Only this computer is in use: document 1 is its own.
    let mut propose = |title: &str, text: &str| {
        let held = world.ok(
            "knowledge_add",
            json!({ "title": title, "content": text, "replaces": 1, "project_path": project_arg(&world) }),
        );

        held.lines()
            .next()
            .and_then(|line| line.strip_prefix("held:"))
            .expect("held:<id>")
            .to_owned()
    };
    let first = propose("Gateway runbook", "# Runbook\n\nRestart it.");
    let second = propose("Another runbook", "# Other\n\nSomething else.");

    let env = Env::for_tests(world.home()).with_var(DIR_VAR, world.knowledge().to_string_lossy());
    let remote =
        crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend");
    let approve =
        |held: &str, routing: Routing| reply(&env, &remote, routing, &[Some("approve"), Some("--json"), Some(held)]);
    let title = || {
        let mut database = world.database();

        database
            .get(1, &crate::store::Detail::default())
            .expect("get")
            .expect("document 1")
            .doc
            .title
    };

    // The remote database alone: this computer's document is not in use, so the approval is
    // refused and nothing is touched (the remote's own document 1 included, which is never opened).
    let Reply::Refused(_, message) = approve(&second, Routing::new(Where::Remote, Where::Remote)) else {
        panic!("not refused");
    };
    assert!(message.contains("not in use"), "{message}");
    assert_eq!(title(), "Mine");

    // The remote database plus this computer: it is still this computer's document 1.
    let Reply::Done(done) = approve(&first, Routing::new(Where::RemoteLocal, Where::RemoteLocal)) else {
        panic!("approved");
    };
    assert_eq!(done["approved"]["docId"], json!("local:1"));
    assert_eq!(title(), "Gateway runbook");
}

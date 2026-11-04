//! Asking for changes to a proposal, a rejection with a reason, and an approval while team approval
//! is on in the remote database.

use super::*;
use crate::config::Where;
use crate::store::{Detail, KnowledgeStore, Status};

fn changes_requested(world: &World, id: &str, extra: &[&str]) -> Reply {
    let mut args = vec!["request-changes", "--json", id];
    args.extend_from_slice(extra);

    world.run(&args)
}

#[test]
fn a_request_for_changes_marks_the_proposal_and_it_keeps_waiting() {
    let world = World::new();
    let id = world.propose("Gateway runbook", TEXT, vec![link("git:github.com/o/r")], None);
    let shown = world.done(&["get", "--json", &id])["held"].clone();

    assert_eq!(shown["status"], "waiting");
    assert!(shown.get("changesRequested").is_none());
    assert!(shown.get("comment").is_none(), "no request, no comment");

    let revision = shown["revision"].as_str().expect("revision").to_owned();
    let done = match changes_requested(
        &world,
        &id,
        &["--expect", &revision, "--comment", "  Name the\nsteps  "],
    ) {
        Reply::Done(json) => json,
        other => panic!("{other:?}"),
    };
    let held = &done["held"];

    assert_eq!(held["status"], "changesRequested");
    assert_eq!(held["changesRequested"]["comment"], "Name the steps");
    assert_eq!(held["comment"], "Name the steps", "also at the top level");
    assert!(
        held["changesRequested"]["at"]
            .as_str()
            .is_some_and(|at| at.contains('T'))
    );
    assert!(held["changesRequested"].get("by").is_some());
    assert_ne!(
        held["revision"], shown["revision"],
        "a decision on the old view is refused"
    );

    // It is still in the queue, as listed, and nothing reached the database.
    let listed = world.done(&["list", "--json"]);
    assert_eq!(listed["held"][0]["status"], "changesRequested");
    assert_eq!(listed["held"][0]["changesRequested"]["comment"], "Name the steps");
    assert_eq!(listed["held"][0]["comment"], "Name the steps");
    assert!(!world.database_exists());

    let (code, _) = world.refused(&["approve", "--json", &id, "--expect", &revision]);
    assert_eq!(code, Code::Changed);

    // Asking again replaces the comment.
    let current = held["revision"].as_str().expect("revision").to_owned();
    changes_requested(&world, &id, &["--expect", &current, "--comment", "Also the limits"]);
    let again = world.done(&["get", "--json", &id])["held"].clone();
    assert_eq!(again["changesRequested"]["comment"], "Also the limits");
    assert_eq!(again["comment"], "Also the limits");
}

#[test]
fn a_request_for_changes_needs_a_comment_and_a_proposal() {
    let world = World::new();
    let id = world.propose("Gateway runbook", TEXT, Vec::new(), None);

    assert!(matches!(changes_requested(&world, &id, &[]), Reply::Usage));
    assert_eq!(
        changes_requested(&world, &id, &["--comment", "   "]),
        Reply::Refused(Code::CommentRequired, "requesting changes needs a comment".to_owned())
    );
    assert!(matches!(
        changes_requested(&world, &id, &["--comment", &"x".repeat(501)]),
        Reply::Refused(Code::Refused, _)
    ));
    assert!(matches!(
        changes_requested(&world, &crate::held::random_id(), &["--comment", "no"]),
        Reply::Refused(Code::NotFound, _)
    ));
    assert_eq!(world.done(&["get", "--json", &id])["held"]["status"], "waiting");
}

#[test]
fn a_rejection_keeps_the_reason_in_its_reply() {
    let world = World::new();
    let id = world.propose("Gateway runbook", TEXT, Vec::new(), None);
    let other = world.propose("Other", "# Other\n\nWords.", Vec::new(), None);

    assert_eq!(
        world.done(&["reject", "--json", &id, "--comment", "Out of date"]),
        json!({ "rejected": id, "comment": "Out of date" })
    );
    assert_eq!(world.done(&["reject", "--json", &other]), json!({ "rejected": other }));
    assert_eq!(world.done(&["list", "--json"]), json!({ "held": [] }));
}

fn session_item(world: &World, session: &str, title: &str, content: &str) -> HeldItem {
    let mut item = HeldItem::new(
        Source {
            client: "codex".to_owned(),
            session: Some(session.to_owned()),
            project_path: Some("/work/repo".to_owned()),
            repo_key: None,
        },
        Proposal {
            title: title.to_owned(),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: content.to_owned(),
            links: Vec::new(),
            replaces: None,
            replaced: None,
        },
    );
    item.added_by = world.env.home.to_str().map(|_| "Ada".to_owned());

    item
}

#[test]
fn a_later_proposal_of_the_same_session_answers_the_request_and_keeps_the_id() {
    let world = World::new();
    let held = world.held();
    let id = held
        .hold(session_item(&world, "s1", "Runbook", "# Runbook\n\nFirst words."))
        .expect("held");

    world.done(&["request-changes", "--json", &id, "--comment", "Add the steps"]);

    // Another session with the same title is a proposal of its own.
    let other = held
        .hold(session_item(&world, "s2", "Runbook", "# Runbook\n\nOther words."))
        .expect("held");
    assert_ne!(other, id);

    // The same session with the same title (case and spaces ignored) replaces it.
    let again = held
        .hold(session_item(
            &world,
            "s1",
            " runbook ",
            "# Runbook\n\nFirst words and the steps.",
        ))
        .expect("held");
    assert_eq!(again, id);

    let item = world.done(&["get", "--json", &id])["held"].clone();
    assert_eq!(item["status"], "waiting");
    assert!(item.get("changesRequested").is_none());
    assert_eq!(item["content"], "# Runbook\n\nFirst words and the steps.");
    assert_eq!(
        world.done(&["list", "--json"])["held"].as_array().map(Vec::len),
        Some(2)
    );

    // A proposal that was not asked to change is not replaced by a title, however alike.
    let plain = held
        .hold(session_item(&world, "s1", "Runbook", "# Runbook\n\nThird words."))
        .expect("held");
    assert_ne!(plain, id);
}

#[test]
fn a_proposal_may_name_the_one_it_answers_and_only_from_its_session() {
    let world = World::new();
    let held = world.held();
    let id = held
        .hold(session_item(&world, "s1", "Runbook", "# Runbook\n\nFirst words."))
        .expect("held");

    world.done(&["request-changes", "--json", &id, "--comment", "Rename it"]);

    let stranger = held
        .hold_named(session_item(&world, "s2", "Renamed", "# Renamed\n\nWords."), Some(&id))
        .expect("held");
    assert_ne!(stranger, id, "another session cannot take it over");

    let answer = held
        .hold_named(
            session_item(&world, "s1", "Renamed", "# Renamed\n\nFirst words."),
            Some(&id),
        )
        .expect("held");
    assert_eq!(answer, id);
    assert_eq!(world.done(&["get", "--json", &id])["held"]["title"], "Renamed");

    // Once answered it no longer has a request, so naming it again holds a new proposal.
    let more = held
        .hold_named(
            session_item(&world, "s1", "Renamed", "# Renamed\n\nMore words."),
            Some(&id),
        )
        .expect("held");
    assert_ne!(more, id);
}

/// The remote database alone is the libSQL server under test, with team approval switched on.
fn team() -> Option<(World, Backend, Routing, Box<dyn KnowledgeStore>)> {
    let url = std::env::var("HARNESS_KNOWLEDGE_TEST_LIBSQL_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())?;
    let backend = Backend::Libsql {
        url: url.clone(),
        token: None,
    };
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("harness-knowledge-libsql-tests.lock"))
        .expect("lock file");
    lock.lock().expect("lock");
    // The lock is held for the rest of the process: the case is short and the others take turns.
    std::mem::forget(lock);

    let world = World::new();
    let mut store = crate::store::libsql::open_boxed(
        &url,
        None,
        Access::Create,
        std::time::Duration::from_secs(10),
        &backend.secrets(),
    )
    .expect("open")
    .expect("a store");
    store.set_meta(crate::review::APPROVAL_KEY, "on").expect("approval on");

    Some((world, backend, Routing::new(Where::Remote, Where::Remote), store))
}

#[test]
fn team_an_approval_under_team_approval_is_a_draft_or_a_pending_version() {
    let Some((world, backend, routing, mut store)) = team() else {
        eprintln!("skipped: set HARNESS_KNOWLEDGE_TEST_LIBSQL_URL to run this case on a libSQL server");
        return;
    };
    let approve = |id: &str| match reply(
        &world.env,
        &backend,
        routing,
        &[Some("approve"), Some("--json"), Some(id)],
    ) {
        Reply::Done(json) => json["approved"].clone(),
        other => panic!("{other:?}"),
    };
    let unique = format!("Team runbook {}", crate::import::document::now());

    // A new proposal is a draft: no agent reads it.
    let new = world.propose(&unique, &format!("# {unique}\n\nZebrafish words."), Vec::new(), None);
    let approved = approve(&new);
    assert_eq!(
        (approved["status"].as_str(), approved["draft"].as_bool()),
        (Some("added"), Some(true))
    );

    let draft = approved["docId"].as_i64().expect("id");
    assert!(
        store.get(draft, &Detail::default()).expect("get").is_none(),
        "agents do not read it"
    );
    assert_eq!(
        store
            .get(
                draft,
                &Detail {
                    drafts: true,
                    ..Detail::default()
                }
            )
            .expect("get")
            .expect("row")
            .doc
            .status,
        Status::Draft
    );

    // A live document it replaces gets a pending version and keeps its text.
    store.set_meta(crate::review::APPROVAL_KEY, "off").expect("off");
    let live = world.propose(
        &format!("{unique} live"),
        &format!("# {unique} live\n\nOld words."),
        Vec::new(),
        None,
    );
    let live = approve(&live)["docId"].as_i64().expect("id");
    assert_eq!(
        store
            .get(live, &Detail::default())
            .expect("get")
            .expect("live")
            .doc
            .status,
        Status::Live
    );

    store.set_meta(crate::review::APPROVAL_KEY, "on").expect("on");
    let was = store.get(live, &Detail::default()).expect("get").expect("live").doc;
    let replacing = HeldItem::new(
        Source {
            client: "codex".to_owned(),
            session: None,
            project_path: None,
            repo_key: None,
        },
        Proposal {
            title: format!("{unique} live"),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: format!("# {unique} live\n\nNew words."),
            links: Vec::new(),
            replaces: Some(crate::store::routed::ids::tag(Place::Remote, live)),
            replaced: Some(crate::held::Replaced {
                content_sha256: was.content_sha256,
                source_key: was.source_key,
            }),
        },
    );
    let held = world.held().hold(replacing).expect("held");
    let approved = approve(&held);

    assert_eq!(approved["status"], "pending");
    assert_ne!(approved["docId"].as_i64(), Some(live));
    assert_eq!(
        store.pending_of(live).expect("pending").map(|version| version.id),
        approved["docId"].as_i64()
    );

    // Clean up: the setting and what this case made.
    store.delete(&[draft, live]).expect("delete");
    store.set_meta(crate::review::APPROVAL_KEY, "off").expect("off");
}

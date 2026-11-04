//! Agents never see a document that is not live: not a draft, a waiting one, one with changes
//! requested, or the pending version of a live document. Every tool is asked about a database
//! that holds all four next to a live document, all of them matching the same words and vectors,
//! and answers as if they were not there.

use super::*;
use crate::store::{Detail, DocUsage, Status};

/// The ids of the fixture: the live document and the four that are not live.
struct Fixture {
    live: i64,
    others: [i64; 4],
}

impl Fixture {
    fn all(&self) -> Vec<i64> {
        std::iter::once(self.live).chain(self.others).collect()
    }
}

/// Words every document of the fixture has, and the one that only the hidden ones carry.
const HIDDEN: &str = "zebrasecret";

fn doc(title: &str) -> Seed {
    Seed::new(title).embedded(
        "Install",
        &format!("Install the gateway with the installer. {title}"),
        axis(0),
    )
}

fn fixture(world: &mut World) -> Fixture {
    let mut database = world.database();
    let live = insert(&mut database, &doc("Gateway live"));
    let others = [
        insert(
            &mut database,
            &doc(&format!("Gateway draft {HIDDEN}")).status(Status::Draft),
        ),
        insert(
            &mut database,
            &doc(&format!("Gateway waiting {HIDDEN}")).status(Status::Waiting),
        ),
        insert(
            &mut database,
            &doc(&format!("Gateway changes {HIDDEN}")).status(Status::ChangesRequested),
        ),
        insert(
            &mut database,
            &doc(&format!("Gateway pending {HIDDEN}")).pending_of(live),
        ),
    ];
    drop(database);
    world.server.store = None;

    Fixture { live, others }
}

fn usage(world: &World, doc: i64) -> DocUsage {
    let mut store = SqliteStore::open_read_only(&world.knowledge().join(FILE_NAME))
        .expect("opens")
        .expect("the database exists");

    store
        .get(
            doc,
            &Detail {
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("reads")
        .expect("the document exists")
        .usage
        .expect("the database has counters")
}

/// The chunk id of the one chunk of `doc`, read as the person does.
fn chunk_of(world: &World, doc: i64) -> i64 {
    let mut store = SqliteStore::open_read_only(&world.knowledge().join(FILE_NAME))
        .expect("opens")
        .expect("the database exists");

    store
        .get(
            doc,
            &Detail {
                sections: true,
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("reads")
        .and_then(|detail| detail.sections)
        .and_then(|sections| sections.first().map(|section| section.chunk_id))
        .expect("a chunk")
}

fn pointed(reply: &str) -> Vec<i64> {
    reply
        .lines()
        .filter_map(|line| line.split("<knowledge-doc id=\"").nth(1))
        .filter_map(|rest| rest.split(':').next()?.parse().ok())
        .collect()
}

fn says_nothing_of_the_hidden(reply: &str, fixture: &Fixture) {
    assert!(!reply.to_lowercase().contains(HIDDEN), "{reply}");
    assert!(
        !reply.contains("pending") && !reply.contains("draft") && !reply.contains("waiting"),
        "{reply}"
    );

    for id in fixture.others {
        assert!(
            !reply.contains(&format!("id=\"{id}:")) && !reply.contains(&format!("{id} |")),
            "{reply}"
        );
    }
}

#[test]
fn a_search_finds_the_live_document_only_in_every_scope_and_mode() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);

    for scope in ["repo", "all"] {
        for mode in ["pointers", "chunks"] {
            let reply = world.ok(
                "knowledge_search",
                json!({
                    "query": "install gateway installer", "project_path": project, "scope": scope,
                    "mode": mode, "limit": 20,
                }),
            );

            assert_eq!(pointed(&reply), [fixture.live], "{scope} {mode}: {reply}");
            says_nothing_of_the_hidden(&reply, &fixture);
        }
    }

    // The word only the hidden documents carry finds none of them (the live one comes by its vector).
    let reply = world.ok(
        "knowledge_search",
        json!({ "query": HIDDEN, "project_path": project, "scope": "all" }),
    );
    assert_eq!(pointed(&reply), [fixture.live], "{reply}");
    says_nothing_of_the_hidden(&reply, &fixture);
}

#[test]
fn a_get_of_a_document_that_is_not_live_answers_like_a_document_that_does_not_exist() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);
    let unknown = 99;

    let asked = |world: &mut World, id: Value, extra: Value| {
        let mut arguments = json!({ "id": id, "project_path": project });

        for (key, value) in extra.as_object().expect("an object") {
            arguments[key] = value.clone();
        }

        world.call("knowledge_get", arguments)
    };

    for id in fixture.others {
        let chunk = chunk_of(&world, id);

        for (shown, plain, extra) in [
            (json!(id), json!(unknown), json!({})),
            (json!(id), json!(unknown), json!({ "scope": "all" })),
            (json!(id), json!(unknown), json!({ "max_bytes": 100_000 })),
            (
                json!(format!("{id}:{chunk}")),
                json!(format!("{unknown}:{chunk}")),
                json!({}),
            ),
            (
                json!(format!("{id}:{chunk}")),
                json!(format!("{unknown}:{chunk}")),
                json!({ "neighbors": 3 }),
            ),
        ] {
            let (reply, is_error) = asked(&mut world, shown.clone(), extra.clone());
            let (missing, missing_error) = asked(&mut world, plain, extra);

            assert!(is_error && missing_error, "{id}: {reply} / {missing}");
            assert_eq!(
                reply.replace(&id.to_string(), "N"),
                missing.replace(&unknown.to_string(), "N"),
                "{shown}"
            );
            assert!(!reply.to_lowercase().contains(HIDDEN), "{reply}");
        }
    }

    // A chunk of a hidden document asked for under the live document's id is not found either.
    for id in fixture.others {
        let chunk = chunk_of(&world, id);
        let (reply, is_error) = asked(&mut world, json!(format!("{}:{chunk}", fixture.live)), json!({}));

        assert!(is_error, "{reply}");
        assert!(!reply.to_lowercase().contains(HIDDEN), "{reply}");
    }

    // The live document reads.
    let (reply, is_error) = asked(&mut world, json!(fixture.live), json!({}));
    assert!(!is_error, "{reply}");
    assert!(reply.contains("Gateway live"), "{reply}");
}

#[test]
fn a_list_has_the_live_document_only_and_counts_only_it() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);

    for extra in [json!({}), json!({ "query": "gateway" })] {
        let mut arguments = json!({ "project_path": project });

        for (key, value) in extra.as_object().expect("an object") {
            arguments[key] = value.clone();
        }

        let reply = world.ok("knowledge_list", arguments);

        assert!(reply.contains("total=\"1\""), "{reply}");
        assert!(reply.contains(&format!("{} | Gateway live", fixture.live)), "{reply}");
        says_nothing_of_the_hidden(&reply, &fixture);
    }
}

#[test]
fn feedback_refuses_a_document_that_is_not_live_exactly_like_an_unknown_one() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);

    let asked = fixture.all();
    let reply = world.ok("knowledge_feedback", json!({ "ids": asked, "project_path": project }));
    let [draft, waiting, changes, pending] = fixture.others;

    assert_eq!(
        reply,
        format!(
            "marked 1 document useful: {}\nnot found: {draft}, {waiting}, {changes}, {pending}",
            fixture.live
        )
    );

    // As a chunk id, a number a search could have shown, it is the same.
    let chunk = chunk_of(&world, draft);
    let reply = world.ok(
        "knowledge_feedback",
        json!({ "ids": [format!("{draft}:{chunk}")], "project_path": project }),
    );
    let unknown = world.ok(
        "knowledge_feedback",
        json!({ "ids": [format!("99:{chunk}")], "project_path": project }),
    );
    assert_eq!(reply.replace(&draft.to_string(), "N"), unknown.replace("99", "N"));

    for id in fixture.others {
        assert_eq!((usage(&world, id).useful, usage(&world, id).shown), (0, 0), "{id}");
    }

    assert_eq!(usage(&world, fixture.live).useful, 1);
}

#[test]
fn a_search_shows_and_counts_the_live_document_alone() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);

    world.ok(
        "knowledge_search",
        json!({ "query": "install gateway installer", "project_path": project, "limit": 20 }),
    );

    assert_eq!(usage(&world, fixture.live).shown, 1);

    for id in fixture.others {
        assert_eq!(usage(&world, id).shown, 0, "{id}");
    }

    // Even a counter queued for a document that is not live moves nothing (the store counts live
    // documents only).
    world.server.counting.queue_shown(&fixture.others);
    world.server.counting.queue_useful(&fixture.others, 1_000);
    world.server.after_reply();

    for id in fixture.others {
        let counted = usage(&world, id);
        assert_eq!((counted.shown, counted.useful), (0, 0), "{id}");
    }
}

#[test]
fn replacing_a_document_that_is_not_live_answers_like_replacing_one_that_does_not_exist() {
    let mut world = World::new();
    let fixture = fixture(&mut world);
    let project = project_arg(&world);
    let unknown = 99;

    let propose = |world: &mut World, id: i64| {
        world.call(
            "knowledge_add",
            json!({ "title": "Mine", "content": "New text.", "replaces": id, "project_path": project }),
        )
    };
    let (missing, missing_error) = propose(&mut world, unknown);

    assert!(missing_error, "{missing}");

    for id in fixture.others {
        let (reply, is_error) = propose(&mut world, id);

        assert!(is_error, "{reply}");
        assert_eq!(reply.replace(&id.to_string(), "N"), missing.replace("99", "N"));
    }

    assert!(world.held_files().is_empty(), "nothing is held");

    // The live document can be replaced.
    let (reply, is_error) = propose(&mut world, fixture.live);
    assert!(!is_error, "{reply}");
    assert_eq!(world.held_files().len(), 1);
}

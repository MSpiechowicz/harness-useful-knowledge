//! The usefulness counters: `knowledge_search` counts the documents it shows, `knowledge_feedback`
//! the ones that helped, both once per document and session, after the reply, and never for the
//! ranking.

use std::time::{Duration, Instant};

use super::*;
use crate::store::{Detail, DocUsage};

/// A computer with three documents that all match "install gateway", and their ids.
fn seeded() -> (World, [i64; 3]) {
    let mut world = World::new();
    let mut database = world.database();
    let first = insert(
        &mut database,
        &Seed::new("Gateway guide").embedded("Install", "Install the gateway with the installer.", angled(5.0)),
    );
    let second = insert(
        &mut database,
        &Seed::new("Gateway runbook").embedded("Install", "Install the gateway by tagging a release.", angled(40.0)),
    );
    let third = insert(
        &mut database,
        &Seed::new("Gateway notes").embedded("Install", "Install the gateway from the package.", angled(70.0)),
    );
    drop(database);
    world.server.store = None;

    (world, [first, second, third])
}

fn initialize(world: &mut World, client: &str) {
    world.server.handle(json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": client } },
    }));
}

fn search(world: &mut World, limit: u32) -> String {
    let project = project_arg(world);

    world.ok(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project, "limit": limit }),
    )
}

/// The documents a search reply points to, in order.
fn pointed(reply: &str) -> Vec<i64> {
    reply
        .lines()
        .filter_map(|line| line.split("<knowledge-doc id=\"").nth(1))
        .filter_map(|rest| rest.split(':').next()?.parse().ok())
        .collect()
}

fn usage(world: &World, doc: i64) -> DocUsage {
    let path = world.knowledge().join(FILE_NAME);
    let mut store = SqliteStore::open_read_only(&path)
        .expect("opens")
        .expect("the database exists");

    store
        .get(doc, &Detail::default())
        .expect("reads")
        .expect("the document exists")
        .usage
        .expect("the database has counters")
}

/// `(shown, useful)` of each of `docs`.
fn counts(world: &World, docs: &[i64]) -> Vec<(i64, i64)> {
    docs.iter()
        .map(|doc| {
            let usage = usage(world, *doc);

            (usage.shown, usage.useful)
        })
        .collect()
}

fn feedback(world: &mut World, ids: Value) -> String {
    let project = project_arg(world);

    world.ok("knowledge_feedback", json!({ "ids": ids, "project_path": project }))
}

#[test]
fn a_search_counts_the_documents_it_shows_once_per_session() {
    let (mut world, docs) = seeded();

    let top = pointed(&search(&mut world, 1));
    assert_eq!(top.len(), 1);

    let expected: Vec<(i64, i64)> = docs.iter().map(|doc| (i64::from(top.contains(doc)), 0)).collect();
    assert_eq!(counts(&world, &docs), expected, "only the document shown");

    search(&mut world, 1);
    assert_eq!(counts(&world, &docs), expected, "the same document counts once");

    let all = pointed(&search(&mut world, 10));
    assert_eq!(all.len(), 3);
    assert_eq!(
        counts(&world, &docs),
        vec![(1, 0); 3],
        "each of the others once, the first still once"
    );
}

#[test]
fn a_document_the_reply_cap_left_out_is_not_counted() {
    let mut world = World::new();
    let mut database = world.database();
    let big = "gateway install ".repeat(375);
    let docs: Vec<i64> = (0..10)
        .map(|number| {
            insert(
                &mut database,
                &Seed::new(&format!("Gateway guide {number}")).embedded("Install", &big, angled(5.0 + number as f32)),
            )
        })
        .collect();
    drop(database);

    let project = project_arg(&world);
    let reply = world.ok(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project, "limit": 10, "mode": "chunks" }),
    );
    let shown = pointed(&reply);

    assert!(reply.contains("more)"), "the cap left some out");
    assert!(!shown.is_empty() && shown.len() < docs.len());

    for (doc, (count, _)) in docs.iter().zip(counts(&world, &docs)) {
        assert_eq!(count, i64::from(shown.contains(doc)), "document {doc}");
    }
}

#[test]
fn the_desktop_searching_in_the_background_counts_nothing_and_an_agent_does() {
    let (mut world, docs) = seeded();

    initialize(&mut world, "harness-desktop");
    search(&mut world, 10);
    assert_eq!(counts(&world, &docs), vec![(0, 0); 3]);

    initialize(&mut world, "claude-code");
    search(&mut world, 10);
    assert_eq!(counts(&world, &docs), vec![(1, 0); 3]);
}

#[test]
fn counters_are_written_after_the_reply_and_not_before() {
    let (mut world, docs) = seeded();
    let project = project_arg(&world);

    let (_, is_error) = world.call_unflushed(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project }),
    );
    assert!(!is_error);
    assert_eq!(
        counts(&world, &docs),
        vec![(0, 0); 3],
        "the reply is out, nothing written yet"
    );

    world.server.after_reply();
    assert_eq!(counts(&world, &docs), vec![(1, 0); 3]);
}

#[test]
fn the_serve_loop_writes_the_counters_once_the_answer_is_written() {
    let (mut world, docs) = seeded();
    let project = project_arg(&world);
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "knowledge_search", "arguments": { "query": "install gateway", "project_path": project } },
    });
    let input = format!("{call}\n");
    let mut output: Vec<u8> = Vec::new();

    serve(input.as_bytes(), &mut output, &mut world.server).expect("serves");

    let answer: Value = serde_json::from_slice(&output).expect("one JSON answer");
    assert!(
        answer["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("<knowledge-doc")
    );
    assert_eq!(counts(&world, &docs), vec![(1, 0); 3]);
}

#[test]
fn feedback_counts_the_documents_that_helped_once_per_session() {
    let (mut world, docs) = seeded();
    let started = crate::held::now();

    // A chunk id as a search shows it counts for its document, a repeated document once.
    let reply = feedback(
        &mut world,
        json!([docs[0], format!("{}:1", docs[0]), docs[1].to_string()]),
    );
    assert_eq!(reply, format!("marked 2 documents useful: {}, {}", docs[0], docs[1]));

    let first = usage(&world, docs[0]);
    assert_eq!((first.useful, first.shown), (1, 0));
    assert!(first.last_useful_at.is_some_and(|at| at >= started));
    assert_eq!(usage(&world, docs[1]).useful, 1);
    assert_eq!(usage(&world, docs[2]).useful, 0);
    assert_eq!(usage(&world, docs[2]).last_useful_at, None);

    let again = feedback(&mut world, json!([docs[0]]));
    assert_eq!(again, format!("marked 1 document useful: {}", docs[0]));
    assert_eq!(usage(&world, docs[0]).useful, 1, "once per session");
}

#[test]
fn feedback_with_unknown_ids_or_none_is_not_an_error() {
    let (mut world, docs) = seeded();

    assert_eq!(feedback(&mut world, json!([])), "marked no documents useful");
    assert_eq!(
        feedback(&mut world, json!([999, docs[2]])),
        format!("marked 1 document useful: {}\nnot found: 999", docs[2])
    );
    assert_eq!(
        feedback(&mut world, json!([4242])),
        "marked no documents useful\nnot found: 4242"
    );
    assert_eq!(counts(&world, &docs), vec![(0, 0), (0, 0), (0, 1)]);

    let project = project_arg(&world);
    let reply = world.ok(
        "knowledge_feedback",
        json!({ "ids": [docs[0]], "reason": "answered the question", "project_path": project }),
    );
    assert!(reply.starts_with("marked 1 document useful"), "{reply}");
}

#[test]
fn feedback_refuses_what_is_not_an_id_a_long_reason_or_too_many_ids() {
    let (mut world, _) = seeded();
    let project = project_arg(&world);

    let wrong = world.err("knowledge_feedback", json!({ "ids": ["abc"], "project_path": project }));
    assert!(wrong.contains("id must be a document id"), "{wrong}");

    let many: Vec<u32> = (1..=11).collect();
    let too_many = world.err("knowledge_feedback", json!({ "ids": many, "project_path": project }));
    assert!(too_many.contains("at most 10"), "{too_many}");

    let reason = "x".repeat(201);
    let long = world.err(
        "knowledge_feedback",
        json!({ "ids": [], "reason": reason, "project_path": project }),
    );
    assert!(long.contains("reason must be at most 200 characters"), "{long}");

    let missing = world.err("knowledge_feedback", json!({ "project_path": project }));
    assert!(missing.contains("ids"), "{missing}");
}

#[test]
fn a_document_of_another_repository_counts_only_with_scope_all() {
    let mut world = World::new();
    let mut database = world.database();
    let other = insert(
        &mut database,
        &Seed::new("Other repo notes")
            .embedded("Deploy", "Deploy the gateway.", angled(9.0))
            .link("git:github.com/o/other", &[]),
    );
    drop(database);
    let project = project_arg(&world);

    let hidden = world.ok("knowledge_feedback", json!({ "ids": [other], "project_path": project }));
    assert_eq!(hidden, format!("marked no documents useful\nnot found: {other}"));
    assert_eq!(usage(&world, other).useful, 0);

    let allowed = world.ok(
        "knowledge_feedback",
        json!({ "ids": [other], "project_path": project, "scope": "all" }),
    );
    assert_eq!(allowed, format!("marked 1 document useful: {other}"));
    assert_eq!(usage(&world, other).useful, 1);
}

#[test]
fn usefulness_never_changes_the_order_of_a_search() {
    let (mut world, docs) = seeded();

    let before = search(&mut world, 10);
    feedback(&mut world, json!([docs[2], docs[2]]));
    feedback(&mut world, json!([docs[1]]));
    let after = search(&mut world, 10);

    assert_eq!(usage(&world, docs[2]).useful, 1);
    assert_eq!(after, before);
}

#[test]
fn a_locked_database_drops_the_count_and_the_search_is_answered_promptly() {
    let (mut world, docs) = seeded();
    let lock = rusqlite::Connection::open(world.knowledge().join(FILE_NAME)).expect("second connection");
    lock.execute_batch("BEGIN IMMEDIATE").expect("takes the write lock");

    let started = Instant::now();
    let reply = search(&mut world, 10);
    let fed = feedback(&mut world, json!([docs[0]]));

    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(pointed(&reply).len(), 3);
    assert!(fed.starts_with("marked 1 document useful"), "{fed}");

    lock.execute_batch("ROLLBACK").expect("releases it");
    drop(lock);

    assert_eq!(counts(&world, &docs), vec![(0, 0); 3], "dropped, not waited for");
}

#[cfg(unix)]
#[test]
fn a_database_that_cannot_be_written_still_answers_every_tool() {
    use std::os::unix::fs::PermissionsExt;

    let (mut world, docs) = seeded();
    let path = world.knowledge().join(FILE_NAME);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("read-only file");

    if std::fs::OpenOptions::new().append(true).open(&path).is_ok() {
        return; // the file is writable anyway (running as root): nothing to prove
    }

    assert_eq!(pointed(&search(&mut world, 10)).len(), 3);
    assert!(feedback(&mut world, json!([docs[0]])).starts_with("marked 1 document useful"));
    assert_eq!(counts(&world, &docs), vec![(0, 0); 3]);
}

#[test]
fn counting_never_creates_a_database_that_does_not_exist() {
    let mut world = World::new();
    let project = project_arg(&world);

    assert_eq!(
        world.ok(
            "knowledge_search",
            json!({ "query": "install gateway", "project_path": project })
        ),
        "no documents yet"
    );
    assert_eq!(
        feedback(&mut world, json!([1])),
        "marked no documents useful\nnot found: 1"
    );

    assert!(!world.knowledge().join(FILE_NAME).exists());
}

#[test]
fn a_database_created_later_is_counted_from_then_on() {
    let mut world = World::new();
    search(&mut world, 5);
    assert!(!world.knowledge().join(FILE_NAME).exists());

    let doc = insert(
        &mut world.database(),
        &Seed::new("Gateway guide").embedded("Install", "Install the gateway.", angled(5.0)),
    );
    search(&mut world, 5);

    assert_eq!(usage(&world, doc).shown, 1);
}

#[test]
fn documents_of_this_computer_are_counted_in_its_file_while_the_remote_is_down() {
    let mut world = super::offline::offline_world();
    let project = project_arg(&world);

    let reply = world.ok(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project }),
    );
    assert!(reply.contains("id=\"local:1:"), "{reply}");
    assert_eq!(usage(&world, 1).shown, 1);

    let fed = world.ok(
        "knowledge_feedback",
        json!({ "ids": ["local:1:1"], "project_path": project }),
    );
    assert_eq!(fed, "marked 1 document useful: local:1");
    assert_eq!(usage(&world, 1).useful, 1);
}

//! What an approval may overwrite: an agent's proposal never takes the key its own text names.

use serde_json::json;

use super::*;
use crate::store::{Detail, Origin};

const PAGE: &str = include_str!("../../extract/fixtures/confluence/Deploy-the-gateway_100003.html");

fn propose_page(world: &World, replaces: Option<i64>) -> String {
    let item = HeldItem::new(
        Source {
            client: "codex".to_owned(),
            session: None,
            project_path: None,
            repo_key: None,
        },
        Proposal {
            title: "Gateway notes".to_owned(),
            format: "html".to_owned(),
            name: "Deploy-the-gateway_100003.html".to_owned(),
            source_path: None,
            content: PAGE.to_owned(),
            links: Vec::new(),
            replaces,
        },
    );

    world.held().hold(item).expect("held")
}

fn existing_page(world: &World) -> i64 {
    let mut database = SqliteStore::open(&world.dir.path().join("knowledge").join(FILE_NAME)).expect("database");

    insert(
        &mut database,
        &Seed::new("Deploy the gateway")
            .source("confluence:100003")
            .chunk("Deploy", "the real runbook"),
    )
}

#[test]
fn a_proposal_naming_a_confluence_page_id_does_not_overwrite_that_page() {
    let world = World::new();
    let page = existing_page(&world);
    let id = propose_page(&world, None);

    let approved = world.done(&["approve", "--json", &id]);

    assert_eq!(approved["approved"]["status"], "added");
    assert_ne!(approved["approved"]["docId"], json!(page));

    let mut store = world.store();
    assert_eq!(store.stats().expect("stats").documents, 2);

    let original = store.get(page, &Detail::default()).expect("get").expect("kept").doc;
    assert_eq!(original.title, "Deploy the gateway");
    assert_eq!(original.origin, Origin::User);

    let added = approved["approved"]["docId"].as_i64().expect("doc id");
    let added = store.get(added, &Detail::default()).expect("get").expect("stored").doc;
    assert!(
        added.source_key.starts_with(&format!("agent/{id}/")),
        "{}",
        added.source_key
    );
    assert_eq!(added.origin, Origin::AgentApproved);
}

#[test]
fn replacing_a_document_makes_it_an_agent_approved_one() {
    let world = World::new();
    let page = existing_page(&world);
    let id = propose_page(&world, Some(page));

    let approved = world.done(&["approve", "--json", &id]);

    assert_eq!(approved["approved"]["status"], "updated");
    assert_eq!(approved["approved"]["docId"], json!(page));

    let mut store = world.store();
    assert_eq!(store.stats().expect("stats").documents, 1);

    let doc = store.get(page, &Detail::default()).expect("get").expect("stored").doc;
    assert_eq!(doc.origin, Origin::AgentApproved);
    assert!(doc.source_key.starts_with("agent/"), "{}", doc.source_key);
}

#[test]
fn a_database_that_lacks_a_capability_ends_an_approval_with_exit_three() {
    let reply = open_failure(OpenError::Missing {
        code: "no-pgvector",
        message: "no pgvector".to_owned(),
    });

    assert_eq!(
        reply,
        Reply::Missing {
            code: "no-pgvector",
            message: "no pgvector".to_owned()
        }
    );
    assert_eq!(
        open_failure(OpenError::Failed("down".to_owned())),
        Reply::Failed("down".to_owned())
    );
}

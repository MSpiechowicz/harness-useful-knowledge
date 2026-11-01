//! This computer's file and a PostgreSQL database read and written as one (see
//! [`crate::store::routed`]), through the commands.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::support::{database, insert};
use crate::cli_docs::{Context, docs_reply};
use crate::config::{Routing, Where, backend_for};
use crate::location::{DIR_VAR, FILE_NAME};
use crate::search::{self, Query, Tuning};
use crate::store::testing::Seed;
use crate::store::testing::insert as insert_local;
use crate::store::{KnowledgeStore, RoutedStore, Scope, SqliteStore};
use crate::support::Env;

fn docs(context: &Context, args: &[&str]) -> Value {
    let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

    docs_reply(context, &args, &mut std::io::empty())
        .expect("the command answers")
        .json
}

#[test]
fn team_remote_and_local_documents_are_listed_searched_and_changed_together() {
    let Some(db) = database() else { return };
    let root = tempfile::tempdir().expect("temp dir");
    let folder = root.path().join("knowledge");
    let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());
    let backend = backend_for(Some(db.url().to_owned()), None).expect("backend");
    let context = Context::new(&env, &backend, Routing::new(Where::RemoteLocal, Where::RemoteLocal));

    // No local file yet: a read answers from the remote database and creates nothing.
    let mut remote = db.open();
    insert(
        &mut remote,
        &Seed::new("Remote runbook").chunk("Deploy", "restart the zebrafish gateway"),
    );
    let listed = docs(&context, &["list", "--json"]);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["place"], "remote");
    assert_eq!(listed["items"][0]["id"], 1, "a remote id stays a number");
    assert!(!folder.exists(), "a read never creates the local database");

    let mut local = SqliteStore::open(&folder.join(FILE_NAME)).expect("create the local file");
    insert_local(
        &mut local,
        &Seed::new("My notes")
            .chunk("Deploy", "my own zebrafish gateway tricks")
            .chunk("Versions", "Upgrade to version 1.2")
            .chunk("Other", "version two, section 10"),
    );
    drop(local);

    // Both show, each with its place, the local id written with its prefix.
    let listed = docs(&context, &["list", "--json", "--sort", "title"]);
    assert_eq!(listed["total"], 2);
    let titles: Vec<&Value> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| &item["title"])
        .collect();
    assert_eq!(titles, [&json!("My notes"), &json!("Remote runbook")]);
    assert_eq!(listed["items"][0]["id"], "local:1");
    assert_eq!(listed["items"][0]["place"], "local");

    // Paging across the places.
    let second = docs(
        &context,
        &["list", "--json", "--sort", "title", "--per-page", "1", "--page", "2"],
    );
    assert_eq!(second["items"][0]["title"], "Remote runbook");
    assert_eq!(second["total"], 2);

    // A search runs in each place and answers from both.
    let mut routed = crate::backend::open(&env, &backend, context.routing, crate::backend::Access::Read)
        .expect("open")
        .expect("exists");
    let scope = Scope::everything();
    let query = Query {
        text: "zebrafish gateway",
        scope: &scope,
        paths: &[],
        limit: 10,
    };
    let found = search::run(routed.as_mut(), None, &query, Tuning::SEARCH).expect("search");
    let mut found_titles: Vec<&str> = found.hits.iter().map(|scored| scored.hit.title.as_str()).collect();
    found_titles.sort_unstable();
    assert_eq!(found_titles, ["My notes", "Remote runbook"]);

    // A document is read, matched, renamed and deleted by the id it was shown with.
    let got = docs(&context, &["get", "--json", "local:1", "--sections"]);
    assert_eq!(got["document"]["title"], "My notes");
    assert_eq!(got["sections"][0]["chunkId"], "local:1");

    let matches = docs(&context, &["matches", "--json", "local:1", "--query", "version 1"]);
    assert_eq!(matches["total"], 1);
    assert_eq!(matches["matches"][0]["section"], "Versions");
    assert_eq!(matches["matches"][0]["chunkId"], "local:2");

    assert_eq!(
        docs(&context, &["rename", "--json", "1", "--title", "Remote handbook"])["id"],
        1
    );
    assert_eq!(
        docs(&context, &["rename", "--json", "local:1", "--title", "Mine"])["id"],
        "local:1"
    );
    assert_eq!(
        docs(&context, &["list", "--json", "--query", "handbook"])["items"][0]["place"],
        "remote"
    );

    // `stats` and `check` report both places, the top level being the remote database.
    let stats = crate::cli_docs::stats_reply(&context, &[Some("--json")])
        .expect("stats")
        .json;
    assert_eq!(stats["documents"], 1);
    assert_eq!(stats["places"]["remote"]["documents"], 1);
    assert_eq!(stats["places"]["local"]["documents"], 1);
    assert_eq!(stats["places"]["local"]["chunks"], 3);

    let check = crate::cli_docs::check_reply(&context, &[Some("--json")]).expect("check");
    assert_eq!(check.code, 0);
    assert_eq!(check.json["backend"], "postgres");
    assert_eq!(check.json["places"]["local"]["backend"], "sqlite");
    assert_eq!(check.json["places"]["remote"]["ok"], true);

    assert_eq!(docs(&context, &["delete", "--json", "local:1", "1"])["deleted"], 2);
    assert_eq!(docs(&context, &["list", "--json"])["total"], 0);

    // The routed store of the same routing is what the commands used.
    let mut again: RoutedStore = RoutedStore::new(None, Some(Box::new(db.open())), context.routing);
    assert_eq!(again.stats().expect("stats").documents, 0);
}

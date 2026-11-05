//! `docs list --useful` and the usefulness sorts, the counters and authors in the JSON of a
//! document, and `remoteWritable` in `stats`.

use pretty_assertions::assert_eq;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::*;
use crate::config::Where;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::SqliteStore;
use crate::store::testing::{Seed, insert};

struct Lab {
    root: tempfile::TempDir,
    env: Env,
}

impl Lab {
    /// Three global documents: `a` found useful twice (last at 800), `b` shown three times and never
    /// found useful, `c` found useful once (at 900).
    fn new() -> (Self, [i64; 3]) {
        let root = tempfile::tempdir().expect("temp dir");
        let folder = root.path().join("knowledge");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());

        let mut store = SqliteStore::open(&folder.join(FILE_NAME)).expect("create");
        let a = insert(
            &mut store,
            &Seed::new("A").source("/a").chunk("1", "alpha").updated(1_000),
        );
        let b = insert(
            &mut store,
            &Seed::new("B").source("/b").chunk("1", "beta").updated(2_000),
        );
        let c = insert(
            &mut store,
            &Seed::new("C").source("/c").chunk("1", "gamma").updated(3_000),
        );

        store.record_shown(&[a, b, c]).expect("shown");
        store.record_shown(&[b]).expect("shown again");
        store.record_shown(&[b]).expect("shown a third time");
        store.record_useful(&[a], 700).expect("useful");
        store.record_useful(&[a], 800).expect("useful again");
        store.record_useful(&[c], 900).expect("useful");

        (Self { root, env }, [a, b, c])
    }

    fn run(&self, args: &[&str]) -> Reply {
        let context = Context::new(&self.env, &Backend::Sqlite, Routing::local());
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        match args.first() {
            Some(Some("stats")) => overview::stats(&context, &args[1..]),
            _ => docs_reply(&context, &args, &mut std::io::empty()),
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        self.run(args)
            .unwrap_or_else(|failure| panic!("{args:?}: {failure:?}"))
            .json
    }

    fn titles(&self, args: &[&str]) -> Vec<String> {
        let listed = self.json(args);

        listed["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["title"].as_str().expect("title").to_owned())
            .collect()
    }

    /// Removes the added columns, leaving the file as an older server's.
    fn strip(&self) {
        let conn = Connection::open(self.root.path().join("knowledge").join(FILE_NAME)).expect("open");

        for name in crate::store::quality::USAGE_NAMES {
            conn.execute_batch(&format!("ALTER TABLE knowledge_documents DROP COLUMN {name}"))
                .expect("drop column");
        }
    }
}

#[test]
fn useful_keeps_the_documents_found_useful_or_never_and_any_keeps_all() {
    let (lab, _) = Lab::new();
    let list = |useful: &str| lab.titles(&["list", "--json", "--useful", useful]);

    assert_eq!(list("ever"), ["A", "C"]);
    assert_eq!(list("never"), ["B"]);
    assert_eq!(list("any"), ["A", "B", "C"]);

    assert_eq!(lab.titles(&["list", "--json"]), ["A", "B", "C"]);

    assert_eq!(lab.json(&["list", "--json", "--useful", "ever"])["total"], 2);
    assert!(matches!(
        lab.run(&["list", "--json", "--useful", "sometimes"]),
        Err(Failure::Usage)
    ));
}

#[test]
fn the_list_takes_the_sort_by_paths() {
    let (lab, _) = Lab::new();

    assert_eq!(lab.titles(&["list", "--json", "--sort", "paths"]), ["A", "B", "C"]);
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "paths", "--desc"]),
        ["C", "B", "A"]
    );
}

#[test]
fn the_sorts_by_usefulness_take_desc_and_break_ties_by_id() {
    let (lab, _) = Lab::new();
    let sorted = |sort: &str, desc: bool| {
        let mut args = vec!["list", "--json", "--sort", sort];

        if desc {
            args.push("--desc");
        }

        lab.titles(&args)
    };

    assert_eq!(sorted("useful", false), ["B", "C", "A"]);
    assert_eq!(sorted("useful", true), ["A", "C", "B"]);
    assert_eq!(sorted("lastUseful", false), ["B", "A", "C"]);
    assert_eq!(sorted("lastUseful", true), ["C", "A", "B"]);

    assert_eq!(
        lab.titles(&[
            "list",
            "--json",
            "--useful",
            "ever",
            "--sort",
            "useful",
            "--desc",
            "--per-page",
            "1",
            "--page",
            "2"
        ]),
        ["C"]
    );
    assert!(matches!(
        lab.run(&["list", "--json", "--sort", "Useful"]),
        Err(Failure::Usage)
    ));
}

#[test]
fn a_document_shows_its_counters_and_the_keys_are_absent_without_counters() {
    let (lab, [a, b, _]) = Lab::new();

    let listed = lab.json(&["list", "--json"]);
    let first = &listed["items"][0];
    assert_eq!(
        (
            first["shown"].clone(),
            first["useful"].clone(),
            first["lastUsefulAt"].clone()
        ),
        (json!(1), json!(2), json!("1970-01-01T00:13:20Z"))
    );

    let never_useful = &listed["items"][1];
    assert_eq!(
        (
            never_useful["shown"].clone(),
            never_useful["useful"].clone(),
            never_useful["lastUsefulAt"].clone()
        ),
        (json!(3), json!(0), Value::Null)
    );

    let got = lab.json(&["get", "--json", &b.to_string()]);
    assert_eq!(got["document"]["shown"], 3);
    assert_eq!(got["document"]["lastUsefulAt"], Value::Null);
    assert_eq!(lab.json(&["get", "--json", &a.to_string()])["document"]["useful"], 2);

    lab.strip();

    let listed = lab.json(&["list", "--json"]);
    let first = listed["items"][0].as_object().expect("item");
    assert!(first.contains_key("addedBy") && first.contains_key("links"));
    assert!(
        ["shown", "useful", "lastUsefulAt"]
            .iter()
            .all(|key| !first.contains_key(*key))
    );

    let got = lab.json(&["get", "--json", &a.to_string()]);
    let document = got["document"].as_object().expect("document");
    assert!(
        ["shown", "useful", "lastUsefulAt"]
            .iter()
            .all(|key| !document.contains_key(*key))
    );
}

#[test]
fn never_needs_the_document_shown_three_times() {
    let (lab, _) = Lab::new();
    let list = |useful: &str| lab.titles(&["list", "--json", "--useful", useful]);

    let mut store = SqliteStore::open(&lab.root.path().join("knowledge").join(FILE_NAME)).expect("open");
    let d = insert(
        &mut store,
        &Seed::new("D").source("/d").chunk("1", "delta").updated(4_000),
    );

    assert_eq!(list("never"), ["B"], "never shown");

    store.record_shown(&[d]).expect("shown");
    store.record_shown(&[d]).expect("shown again");
    assert_eq!(list("never"), ["B"], "shown twice is too rarely to say");

    store.record_shown(&[d]).expect("shown a third time");
    assert_eq!(list("never"), ["B", "D"]);
    assert_eq!(list("ever"), ["A", "C"]);
}

#[test]
fn a_database_without_counters_lists_nothing_for_ever_or_never_and_everything_otherwise() {
    let (lab, _) = Lab::new();
    lab.strip();

    for filter in ["ever", "never"] {
        let listed = lab.json(&["list", "--json", "--useful", filter]);
        assert_eq!(
            (listed["items"].clone(), listed["total"].clone()),
            (json!([]), json!(0))
        );
    }

    assert_eq!(lab.titles(&["list", "--json", "--useful", "any"]), ["A", "B", "C"]);

    // The sorts by usefulness are the default order.
    assert_eq!(lab.titles(&["list", "--json", "--sort", "useful"]), ["A", "B", "C"]);
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "lastUseful", "--desc"]),
        ["C", "B", "A"]
    );
}

#[test]
fn remote_writable_is_null_without_a_remote_database_and_while_it_is_down() {
    let (lab, _) = Lab::new();
    let stats = lab.json(&["stats", "--json"]);

    assert_eq!(stats["remoteWritable"], Value::Null);
    assert!(stats.as_object().expect("stats").contains_key("remoteWritable"));

    // A remote nobody listens on: its place is offline, and nothing is known of its privileges.
    let backend =
        crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend");
    let both = Routing {
        repository: Where::RemoteLocal,
        global: Where::RemoteLocal,
    };
    let context = Context::new(&lab.env, &backend, both);
    let stats = overview::stats(&context, &[Some("--json")]).expect("stats").json;

    assert_eq!(stats["places"]["remote"], json!({ "offline": true }));
    assert_eq!(stats["remoteWritable"], Value::Null);
}

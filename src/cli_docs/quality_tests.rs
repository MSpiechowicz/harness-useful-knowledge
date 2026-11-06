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

    /// A lab whose documents are the `seeds`, stored in order, and their ids.
    fn with(seeds: &[Seed]) -> (Self, Vec<i64>) {
        let root = tempfile::tempdir().expect("temp dir");
        let folder = root.path().join("knowledge");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());

        let mut store = SqliteStore::open(&folder.join(FILE_NAME)).expect("create");
        let ids = seeds.iter().map(|seed| insert(&mut store, seed)).collect();

        (Self { root, env }, ids)
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
        self.drop_columns(&crate::store::quality::USAGE_NAMES);
    }

    /// Removes the author columns only.
    fn strip_authors(&self) {
        self.drop_columns(&crate::store::quality::AUTHOR_NAMES);
    }

    fn drop_columns(&self, names: &[&str]) {
        let conn = Connection::open(self.root.path().join("knowledge").join(FILE_NAME)).expect("open");

        for name in names {
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

/// Four documents: Ada added two (one through a harness), Bob one, and nobody known the last.
fn authored() -> Lab {
    let (lab, _) = Lab::with(&[
        Seed::new("A")
            .source("/a")
            .chunk("1", "alpha")
            .by("Ada", Some("codex"))
            .updated(1_000),
        Seed::new("B")
            .source("/b")
            .chunk("1", "beta")
            .by("bob", None)
            .updated(2_000),
        Seed::new("C").source("/c").chunk("1", "gamma").updated(3_000),
        Seed::new("D")
            .source("/d")
            .chunk("1", "delta")
            .by("Ada", None)
            .format("pdf")
            .updated(4_000),
    ]);

    lab
}

#[test]
fn author_keeps_the_documents_a_person_added_and_author_unknown_those_nobody_did() {
    let lab = authored();

    assert_eq!(lab.titles(&["list", "--json", "--author", "Ada"]), ["A", "D"]);
    assert_eq!(lab.titles(&["list", "--json", "--author=bob"]), ["B"]);
    assert_eq!(
        lab.titles(&["list", "--json", "--author", "  Ada  "]),
        ["A", "D"],
        "trimmed"
    );
    assert_eq!(lab.titles(&["list", "--json", "--author-unknown"]), ["C"]);

    for other in ["ADA", "Bob", "Nobody"] {
        let listed = lab.json(&["list", "--json", "--author", other]);
        assert_eq!(
            (listed["items"].clone(), listed["total"].clone()),
            (json!([]), json!(0)),
            "{other}"
        );
    }

    // It goes with the other filters, and the total follows it.
    let listed = lab.json(&["list", "--json", "--author", "Ada", "--format", "pdf"]);
    assert_eq!(
        (listed["total"].clone(), listed["items"].as_array().map(Vec::len)),
        (json!(1), Some(1))
    );
}

#[test]
fn the_list_carries_who_added_the_documents_under_every_filter_but_the_author_one() {
    let lab = authored();
    let everyone = json!({ "people": [{ "name": "Ada", "count": 2 }, { "name": "bob", "count": 1 }], "unknown": 1 });

    assert_eq!(lab.json(&["list", "--json"])["authors"], everyone);
    assert_eq!(lab.json(&["list", "--json", "--author", "Ada"])["authors"], everyone);
    assert_eq!(lab.json(&["list", "--json", "--author-unknown"])["authors"], everyone);
    assert_eq!(
        lab.json(&["list", "--json", "--page", "9"])["authors"],
        everyone,
        "past the end"
    );

    assert_eq!(
        lab.json(&["list", "--json", "--format", "pdf"])["authors"],
        json!({ "people": [{ "name": "Ada", "count": 1 }], "unknown": 0 })
    );
    assert_eq!(
        lab.json(&["list", "--json", "--query", "alpha", "--author", "bob"])["authors"],
        json!({ "people": [{ "name": "Ada", "count": 1 }, { "name": "bob", "count": 0 }], "unknown": 0 }),
        "the selected person is listed, with the documents the filters keep"
    );

    // A name nobody added under the other filters is in the facet with a count of 0.
    assert_eq!(
        lab.json(&["list", "--json", "--format", "pdf", "--author", "Zed"])["authors"],
        json!({ "people": [{ "name": "Ada", "count": 1 }, { "name": "Zed", "count": 0 }], "unknown": 0 })
    );
}

#[test]
fn added_by_sorts_by_the_name_with_the_unknown_last_both_ways() {
    let lab = authored();

    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "addedBy"]),
        ["A", "D", "B", "C"]
    );
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "addedBy", "--desc"]),
        ["B", "D", "A", "C"]
    );
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "addedBy", "--page", "2", "--per-page", "3"]),
        ["C"]
    );
}

#[test]
fn at_most_two_hundred_people_are_listed_and_the_selected_one_is_among_them() {
    let seeds: Vec<Seed> = (0..250)
        .map(|n| {
            // The last people added the fewest documents, so they are the ones cut.
            let seed = Seed::new(&format!("Doc {n:03}"))
                .source(&format!("/{n}"))
                .chunk("1", "text");

            seed.by(&format!("Person {n:03}"), None)
        })
        .chain((0..3).map(|n| {
            Seed::new(&format!("Extra {n}"))
                .source(&format!("/extra/{n}"))
                .chunk("1", "text")
                .by("Person 000", None)
        }))
        .collect();
    let (lab, _) = Lab::with(&seeds);

    let names = |listed: &Value| -> Vec<String> {
        listed["authors"]["people"]
            .as_array()
            .expect("people")
            .iter()
            .map(|person| person["name"].as_str().expect("name").to_owned())
            .collect()
    };

    let listed = lab.json(&["list", "--json"]);
    let people = names(&listed);
    assert_eq!(people.len(), 200);
    assert_eq!(people[0], "Person 000");
    assert!(!people.contains(&"Person 249".to_owned()));
    assert_eq!(listed["authors"]["people"][0]["count"], json!(4));

    let listed = lab.json(&["list", "--json", "--author", "Person 249"]);
    let people = names(&listed);
    assert_eq!(
        (people.len(), people.last().map(String::as_str)),
        (200, Some("Person 249"))
    );
    assert_eq!(listed["total"], json!(1));
    assert!(
        listed["authors"]["people"]
            .as_array()
            .expect("people")
            .contains(&json!({ "name": "Person 249", "count": 1 }))
    );
}

#[test]
fn a_database_without_the_author_columns_has_no_person() {
    let lab = authored();
    lab.strip_authors();

    let nobody = json!({ "people": [], "unknown": 4 });
    // The person asked for is listed all the same, with the documents they were found to have (none).
    let asked = json!({ "people": [{ "name": "Ada", "count": 0 }], "unknown": 4 });

    let listed = lab.json(&["list", "--json", "--author", "Ada"]);
    assert_eq!(
        (
            listed["items"].clone(),
            listed["total"].clone(),
            listed["authors"].clone()
        ),
        (json!([]), json!(0), asked)
    );

    assert_eq!(
        lab.titles(&["list", "--json", "--author-unknown"]),
        ["A", "B", "C", "D"]
    );
    assert_eq!(lab.json(&["list", "--json"])["authors"], nobody);
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "addedBy"]),
        ["A", "B", "C", "D"]
    );
    assert_eq!(
        lab.titles(&["list", "--json", "--sort", "addedBy", "--desc"]),
        ["D", "C", "B", "A"]
    );
}

#[test]
fn the_author_options_refuse_what_is_not_a_name_or_asks_for_nothing() {
    let (lab, _) = Lab::with(&[]);

    for args in [
        &["list", "--json", "--author", ""][..],
        &["list", "--json", "--author", "   "],
        &["list", "--json", "--author", "Ada\nLovelace"],
        &["list", "--json", "--author", "Ada\u{202e}"],
        &["list", "--json", "--author", &"x".repeat(81)],
        &["list", "--json", "--author"],
        &["list", "--json", "--author", "Ada", "--author", "Bob"],
        &["list", "--json", "--author", "Ada", "--author-unknown"],
        &["list", "--json", "--author-unknown", "--author-unknown"],
        &["list", "--json", "--author-unknown=yes"],
        &["list", "--json", "--sort", "addedby"],
    ] {
        assert_eq!(lab.run(args).map(|done| done.json), Err(Failure::Usage), "{args:?}");
    }
}

#[test]
fn a_name_of_a_hundred_thousand_characters_is_never_printed_and_counts_as_unknown() {
    let huge = "x".repeat(100_000);
    let (lab, _) = Lab::with(&[
        Seed::new("A")
            .source("/a")
            .chunk("1", "alpha")
            .by("Ada", None)
            .updated(1_000),
        Seed::new("B")
            .source("/b")
            .chunk("1", "beta")
            .by(&huge, None)
            .updated(2_000),
        Seed::new("C")
            .source("/c")
            .chunk("1", "gamma")
            .by(&huge, None)
            .updated(3_000),
    ]);

    let listed = lab.json(&["list", "--json", "--per-page", "1", "--sort", "addedBy"]);

    assert_eq!(listed["total"], json!(3));
    assert_eq!(
        listed["authors"],
        json!({ "people": [{ "name": "Ada", "count": 1 }], "unknown": 2 })
    );
    assert!(listed.to_string().len() < 5_000, "no oversized name in the answer");
}

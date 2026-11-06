use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::SqliteStore;
use crate::store::testing::{Seed, insert};

/// A knowledge folder that does not exist yet, and the environment that points at it.
struct Fixture {
    _root: tempfile::TempDir,
    folder: std::path::PathBuf,
    env: Env,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let folder = root.path().join("knowledge");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());

        Self {
            _root: root,
            folder,
            env,
        }
    }

    /// The database with a global guide and a billing document linked to a repository.
    fn populated() -> (Self, i64, i64) {
        let fixture = Self::new();
        let mut store = SqliteStore::open(&fixture.folder.join(FILE_NAME)).expect("create");

        let guide = insert(
            &mut store,
            &Seed::new("Gateway guide")
                .updated(1_700_000_000)
                .bytes(500)
                .chunk("Intro", "routing of requests")
                .chunk("Intro > Limits", "rate limits per key"),
        );
        let billing = insert(
            &mut store,
            &Seed::new("Billing handbook")
                .updated(1_700_000_100)
                .bytes(900)
                .format("pdf")
                .chunk("Invoices", "invoices are monthly")
                .link("git:github.com/o/api", &["src/billing/**"]),
        );

        (fixture, guide, billing)
    }

    fn run(&self, args: &[&str], stdin: &str) -> Reply {
        self.run_on(&Backend::Sqlite, args, stdin)
    }

    fn run_on(&self, backend: &Backend, args: &[&str], stdin: &str) -> Reply {
        let context = Context::new(&self.env, backend, Routing::local());
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        let mut input = stdin.as_bytes();
        match args.first() {
            Some(Some("repos")) => overview::repos(&context, &args[1..]),
            Some(Some("stats")) => overview::stats(&context, &args[1..]),
            Some(Some("project-key")) => overview::project_key(&context, &args[1..]),
            Some(Some("check")) => check::check(&context, &args[1..]),
            _ => docs_reply(&context, &args, &mut input),
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        self.run(args, "")
            .unwrap_or_else(|failure| panic!("{args:?}: {failure:?}"))
            .json
    }

    fn folder_exists(&self) -> bool {
        self.folder.exists()
    }
}

fn failed(reply: Reply) -> String {
    match reply {
        Err(Failure::Failed(message)) => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn titles(listing: &Value) -> Vec<&str> {
    listing["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["title"].as_str().expect("title"))
        .collect()
}

#[test]
fn on_a_fresh_folder_every_read_is_empty_and_creates_nothing() {
    let fixture = Fixture::new();

    assert_eq!(
        fixture.json(&["list", "--json"]),
        json!({ "items": [], "total": 0, "authors": { "people": [], "unknown": 0 } })
    );
    assert_eq!(
        fixture.json(&[
            "list",
            "--json",
            "--repo",
            "git:h/o/r",
            "--format",
            "pdf",
            "--query",
            "x",
            "--sort",
            "size",
            "--desc",
            "--page",
            "3"
        ]),
        json!({ "items": [], "total": 0, "authors": { "people": [], "unknown": 0 } })
    );
    assert_eq!(fixture.json(&["repos", "--json"]), json!({ "global": 0, "repos": [] }));

    let stats = fixture.json(&["stats", "--json"]);
    assert_eq!(
        (
            stats["documents"].clone(),
            stats["chunks"].clone(),
            stats["model"].clone()
        ),
        (json!(0), json!(0), Value::Null)
    );

    let check = fixture.json(&["check", "--json"]);
    assert_eq!(check["ok"], json!(true));

    assert_eq!(failed(fixture.run(&["get", "--json", "1"], "")), "document 1 not found");
    assert_eq!(fixture.json(&["delete", "--json", "1", "2"]), json!({ "deleted": 0 }));
    assert_eq!(
        failed(fixture.run(&["rename", "--json", "1", "--title", "New"], "")),
        "document 1 not found"
    );
    assert_eq!(
        failed(fixture.run(&["links", "set", "--json", "1"], "[]")),
        "document 1 not found"
    );

    assert!(
        !fixture.folder_exists(),
        "no command created the database or its folder"
    );
}

#[test]
fn a_listing_shows_a_page_of_documents_with_their_links() {
    let (fixture, guide, billing) = Fixture::populated();

    let listing = fixture.json(&["list", "--json", "--sort", "title"]);
    assert_eq!(listing["total"], json!(2));
    assert_eq!(titles(&listing), ["Billing handbook", "Gateway guide"]);

    let first = &listing["items"][0];
    assert_eq!(first["id"], json!(billing));
    assert_eq!(first["format"], json!("pdf"));
    assert_eq!(first["bytes"], json!(900));
    assert_eq!(first["chunkCount"], json!(1));
    assert_eq!(first["origin"], json!("user"));
    assert_eq!(first["updatedAt"], json!("2023-11-14T22:15:00Z"));
    assert_eq!(
        first["links"],
        json!([{ "repo": "git:github.com/o/api", "globs": ["src/billing/**"] }])
    );
    assert_eq!(listing["items"][1]["id"], json!(guide));
    assert_eq!(listing["items"][1]["links"], json!([]));

    assert_eq!(
        titles(&fixture.json(&["list", "--json"])),
        ["Gateway guide", "Billing handbook"],
        "oldest import first by default"
    );
    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--desc"])),
        ["Billing handbook", "Gateway guide"]
    );
    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--sort", "size", "--desc"])),
        ["Billing handbook", "Gateway guide"]
    );
}

#[test]
fn a_listing_filters_and_pages() {
    let (fixture, _, _) = Fixture::populated();

    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--global"])),
        ["Gateway guide"]
    );
    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--repo", "git:github.com/o/api"])),
        ["Billing handbook"]
    );
    assert_eq!(titles(&fixture.json(&["list", "--json", "--all"])).len(), 2);
    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--format", "pdf"])),
        ["Billing handbook"]
    );
    assert_eq!(
        titles(&fixture.json(&["list", "--json", "--query", "limits"])),
        ["Gateway guide"]
    );

    let second = fixture.json(&["list", "--json", "--sort", "title", "--per-page", "1", "--page", "2"]);
    assert_eq!(
        (titles(&second), second["total"].clone()),
        (vec!["Gateway guide"], json!(2))
    );

    let past = fixture.json(&["list", "--json", "--per-page", "1", "--page", "9"]);
    assert_eq!((titles(&past).len(), past["total"].clone()), (0, json!(2)));
}

#[test]
fn bad_arguments_are_usage_errors() {
    let fixture = Fixture::new();

    for args in [
        &["list"][..],
        &["list", "--json", "extra"],
        &["list", "--json", "--global", "--all"],
        &["list", "--json", "--repo", "git:h/o/r", "--global"],
        &["list", "--json", "--repo", "github.com/o/r"],
        &["list", "--json", "--repo", "*"],
        &["list", "--json", "--sort", "size-ish"],
        &["list", "--json", "--format", "PDF"],
        &["list", "--json", "--page", "0"],
        &["list", "--json", "--page", "x"],
        &["list", "--json", "--per-page", "0"],
        &["list", "--json", "--per-page", "201"],
        &["list", "--json", "--page", "99999999999999999999"],
        &["list", "--json", "--query", &"a".repeat(201)],
        &["get", "--json"],
        &["get", "--json", "0"],
        &["get", "--json", "1", "2"],
        &["get", "--json", "1", "--chunk", "x"],
        &["get", "1"],
        &["rename", "--json", "1"],
        &["rename", "--json", "--title", "T"],
        &["delete", "--json"],
        &["delete", "--json", "1", "-2"],
        &["links", "--json", "1"],
        &["links", "set", "--json"],
        &["unknown"],
        &["repos"],
        &["stats", "--json", "x"],
        &["check"],
        &["project-key", "--json"],
    ] {
        assert_eq!(fixture.run(args, ""), Err(Failure::Usage), "{args:?}");
    }

    assert!(!fixture.folder_exists());
}

#[test]
fn a_document_comes_back_with_its_outline_and_a_chunk_of_it() {
    let (fixture, guide, billing) = Fixture::populated();
    let id = guide.to_string();

    let plain = fixture.json(&["get", "--json", &id]);
    assert_eq!(plain["document"]["title"], json!("Gateway guide"));
    assert!(plain.get("sections").is_none() && plain.get("chunk").is_none());

    let outline = fixture.json(&["get", "--json", &id, "--sections"]);
    let sections = outline["sections"].as_array().expect("sections");
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[1]["headingPath"], json!("Intro > Limits"));
    assert_eq!(sections[1]["ordinal"], json!(1));

    let chunk_id = sections[1]["chunkId"].as_i64().expect("chunk id").to_string();
    let chunk = fixture.json(&["get", "--json", &id, "--chunk", &chunk_id]);
    assert_eq!(chunk["chunk"]["text"], json!("rate limits per key"));
    assert!(chunk["chunk"].get("embedding").is_none());

    assert!(
        failed(fixture.run(&["get", "--json", &billing.to_string(), "--chunk", &chunk_id], ""))
            .contains("is not in document")
    );
    assert_eq!(
        failed(fixture.run(&["get", "--json", "999"], "")),
        "document 999 not found"
    );
}

#[test]
fn links_are_replaced_from_stdin_and_validated() {
    let (fixture, guide, _) = Fixture::populated();
    let id = guide.to_string();
    let set = |stdin: &str| fixture.run(&["links", "set", "--json", &id], stdin);

    let reply = set(r#"[{"repo": "git:github.com/o/api", "globs": ["src/**", " src/** "]}, {"repo": "path:/w"}]"#)
        .expect("set");
    assert_eq!(
        reply.json,
        json!({ "id": guide, "links": [
            { "repo": "git:github.com/o/api", "globs": ["src/**"] },
            { "repo": "path:/w", "globs": [] },
        ] })
    );

    let shown = fixture.json(&["get", "--json", &id]);
    assert_eq!(shown["document"]["links"].as_array().expect("links").len(), 2);

    let many: Vec<Value> = (0..33).map(|n| json!({ "repo": format!("git:h/o/r{n}") })).collect();
    let too_many_globs: Vec<String> = (0..33).map(|n| format!("d{n}/**")).collect();

    for bad in [
        "not json".to_owned(),
        "{}".to_owned(),
        r#"[{"repo": "git:h/o/r", "extra": 1}]"#.to_owned(),
        r#"[{"repo": "github.com/o/r"}]"#.to_owned(),
        r#"[{"repo": "git:h/o/r", "globs": ["[bad"]}]"#.to_owned(),
        serde_json::to_string(&many).expect("json"),
        serde_json::to_string(&json!([{ "repo": "git:h/o/r", "globs": too_many_globs }])).expect("json"),
        serde_json::to_string(&json!([{ "repo": "git:h/o/r", "globs": ["g".repeat(257)] }])).expect("json"),
        " ".repeat(usize::try_from(STDIN_MAX).expect("size") + 1),
    ] {
        assert!(
            matches!(set(&bad), Err(Failure::Failed(_))),
            "{}",
            &bad[..bad.len().min(60)]
        );
    }

    let unchanged = fixture.json(&["get", "--json", &id]);
    assert_eq!(
        unchanged["document"]["links"].as_array().expect("links").len(),
        2,
        "a refused input changes nothing"
    );

    set("[]").expect("clear");
    assert_eq!(fixture.json(&["get", "--json", &id])["document"]["links"], json!([]));
    assert_eq!(
        failed(fixture.run(&["links", "set", "--json", "999"], "[]")),
        "document 999 not found"
    );
}

#[test]
fn a_rename_cleans_the_title_and_keeps_to_300_characters() {
    let (fixture, guide, _) = Fixture::populated();
    let id = guide.to_string();

    let reply = fixture.json(&["rename", "--json", &id, "--title", "  New\n title "]);
    assert_eq!(reply, json!({ "id": guide, "title": "New title" }));
    assert_eq!(
        fixture.json(&["get", "--json", &id])["document"]["title"],
        json!("New title")
    );

    assert!(failed(fixture.run(&["rename", "--json", &id, "--title", &"t".repeat(301)], "")).contains("1-300"));
    assert!(failed(fixture.run(&["rename", "--json", &id, "--title", "  "], "")).contains("1-300"));
    assert!(
        fixture
            .run(&["rename", "--json", &id, "--title", &"t".repeat(300)], "")
            .is_ok()
    );
    assert_eq!(
        failed(fixture.run(&["rename", "--json", "999", "--title", "x"], "")),
        "document 999 not found"
    );
}

#[test]
fn deleting_removes_documents_and_counts_those_that_existed() {
    let (fixture, guide, billing) = Fixture::populated();
    let (guide, billing) = (guide.to_string(), billing.to_string());

    assert_eq!(
        fixture.json(&["delete", "--json", &guide, &guide, "999"]),
        json!({ "deleted": 1 })
    );
    assert_eq!(titles(&fixture.json(&["list", "--json"])), ["Billing handbook"]);
    assert_eq!(
        fixture.json(&["repos", "--json"]),
        json!({ "global": 0, "repos": [{ "key": "git:github.com/o/api", "docs": 1 }] })
    );

    assert_eq!(fixture.json(&["delete", "--json", &billing]), json!({ "deleted": 1 }));
    assert_eq!(
        fixture.json(&["list", "--json"]),
        json!({ "items": [], "total": 0, "authors": { "people": [], "unknown": 0 } })
    );
}

#[test]
fn stats_count_the_waiting_proposals_without_a_database() {
    let fixture = Fixture::new();
    assert_eq!(fixture.json(&["stats", "--json"])["held"], json!(0));

    let held = crate::held::HeldStore::open(&fixture.env).expect("held store");
    let folder = fixture.folder.join("held");
    std::fs::create_dir_all(&folder).expect("folder");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    std::fs::write(folder.join("0123abcd-0123-0123-0123-0123456789ab.json"), "{}").expect("file");
    std::fs::write(folder.join("notes.json"), "{}").expect("file");

    assert_eq!(held.count(), 1);
    assert_eq!(fixture.json(&["stats", "--json"])["held"], json!(1));
    assert!(!fixture.folder.join(FILE_NAME).exists(), "no database was created");
}

#[test]
fn stats_and_check_describe_the_database() {
    let (fixture, _, _) = Fixture::populated();

    let stats = fixture.json(&["stats", "--json"]);
    assert_eq!(
        (
            stats["documents"].clone(),
            stats["chunks"].clone(),
            stats["bytes"].clone(),
            stats["globalDocuments"].clone(),
            stats["repositories"].clone()
        ),
        (json!(2), json!(3), json!(1400), json!(1), json!(1))
    );
    assert_eq!(stats["schema"], json!(2));
    assert_eq!(stats["generation"], json!(2));

    let check = fixture.run(&["check", "--json"], "").expect("check");
    assert_eq!(check.code, 0);
    let local = json!({
        "ok": true,
        "backend": "sqlite",
        "schema": 2,
        "capabilities": { "keyword": true, "vector": true, "vectorIndex": "ready" },
        "model": { "dims": 384 },
    });
    let mut expected = local.clone();
    expected["places"] = json!({ "local": local });
    assert_eq!(
        check.json, expected,
        "one place in use: the top level is it, `places` has it alone"
    );

    let mut store = SqliteStore::open(&fixture.folder.join(FILE_NAME)).expect("open");
    store.set_meta("model_id", "multilingual-e5-small").expect("meta");
    assert_eq!(
        fixture.json(&["check", "--json"])["model"],
        json!({ "dims": 384, "id": "multilingual-e5-small" })
    );
}

#[test]
fn a_missing_capability_of_the_database_exits_3_in_every_command_and_a_failed_open_exits_1() {
    for code in [
        "no-fts5",
        "no-mongot",
        "no-pgvector",
        "pgvector-no-privilege",
        "pgvector-too-old",
        "libsql-no-vector",
        "index-building",
    ] {
        let message = format!("the database lacks {code}");
        let failure = Failure::from(OpenError::Missing {
            code,
            message: message.clone(),
        });

        assert_eq!(failure, Failure::Missing { code, message });
        assert_eq!(finish(Err(failure)), ExitCode::from(3), "{code}");
    }

    let failed = Failure::from(OpenError::Failed("cannot open".to_owned()));
    assert_eq!(failed, Failure::Failed("cannot open".to_owned()));
    assert_eq!(finish(Err(failed)), ExitCode::FAILURE);
}

#[test]
fn project_key_names_the_repository_of_an_absolute_folder() {
    let fixture = Fixture::new();
    let project = tempfile::tempdir().expect("project");
    let path = project.path().to_string_lossy().into_owned();

    let reply = fixture.json(&["project-key", "--json", "--path", &path]);
    let key = reply["key"].as_str().expect("key");
    assert!(key.starts_with("path:"), "{reply}");
    assert_eq!(reply["remote"], Value::Null);

    assert!(
        failed(fixture.run(&["project-key", "--json", "--path", "relative/dir"], ""))
            .contains("--path must be an absolute")
    );
    assert!(failed(fixture.run(&["project-key", "--json", "--path", "/no/such/folder/here"], "")).contains("--path"));
}

use serde_json::json;

use super::*;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::SqliteStore;
use crate::store::testing::{Seed, angled, axis, insert};

struct Fake(Result<Vec<f32>, String>);

impl QueryEmbedder for Fake {
    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        self.0.clone()
    }
}

struct World {
    dir: tempfile::TempDir,
    env: Env,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dunce::canonicalize(dir.path()).expect("home");
        std::fs::create_dir(home.join("proj")).expect("project");
        let env = Env::for_tests(&home).with_var(DIR_VAR, home.join("knowledge").to_string_lossy());

        Self { dir, env }
    }

    fn project(&self) -> String {
        dunce::canonicalize(self.dir.path())
            .expect("home")
            .join("proj")
            .to_string_lossy()
            .into_owned()
    }

    fn database(&self) -> SqliteStore {
        SqliteStore::open(&self.dir.path().join("knowledge").join(FILE_NAME)).expect("database")
    }

    fn search(&self, args: &[&str], embedder: &mut Fake) -> Result<Answer, Failure> {
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        answer(&self.env, &Backend::Sqlite, Routing::local(), &args, embedder)
    }
}

fn items(answer: &Answer) -> &Vec<Value> {
    answer.json.as_array().expect("an array")
}

#[test]
fn without_a_database_the_answer_is_an_empty_array_and_nothing_is_created() {
    let world = World::new();

    let found = world
        .search(&["--json", "--query", "gateway"], &mut Fake(Ok(axis(0))))
        .expect("answers");

    assert_eq!(found.json, json!([]));
    assert_eq!(found.notice, None);
    assert!(!world.dir.path().join("knowledge").exists());
}

#[test]
fn a_search_prints_pointers_with_their_ids_and_a_wrapped_block() {
    let world = World::new();
    let mut database = world.database();
    insert(
        &mut database,
        &Seed::new("Gateway guide").embedded(
            "Install > Linux",
            "Install the gateway with the installer.",
            angled(4.0),
        ),
    );
    drop(database);

    let found = world
        .search(&["--json", "--query", "install gateway"], &mut Fake(Ok(axis(0))))
        .expect("answers");
    let item = &items(&found)[0];

    assert_eq!(item["title"], "Gateway guide");
    assert_eq!(item["section"], "Install > Linux");
    assert_eq!(item["snippet"], "Install the gateway with the installer.");
    assert_eq!(item["id"], format!("{}:{}", item["docId"], item["chunkId"]));
    assert!(item["score"].as_f64().is_some_and(|score| score > 400.0));
    assert!(
        item["block"]
            .as_str()
            .expect("block")
            .starts_with("<knowledge-doc id=\"")
    );
    assert!(item.get("text").is_none());
    assert_eq!(found.notice, None);
}

#[test]
fn the_chunks_mode_returns_the_text_and_the_limit_applies() {
    let world = World::new();
    let mut database = world.database();

    for number in 0..4 {
        insert(
            &mut database,
            &Seed::new(&format!("Guide {number}")).embedded(
                "A",
                &format!("The gateway routes requests {number}."),
                angled(3.0 + number as f32),
            ),
        );
    }

    drop(database);

    let found = world
        .search(
            &["--json", "--query", "gateway", "--mode", "chunks", "--limit", "2"],
            &mut Fake(Ok(axis(0))),
        )
        .expect("answers");

    assert_eq!(items(&found).len(), 2);
    assert!(
        items(&found)[0]["text"]
            .as_str()
            .expect("text")
            .starts_with("The gateway routes requests")
    );
    assert!(items(&found)[0].get("snippet").is_none());
}

#[test]
fn the_repository_scope_follows_the_project_and_all_sees_every_repository() {
    let world = World::new();
    let key = format!("path:{}", world.project());
    let mut database = world.database();
    let seed = |title: &str| Seed::new(title).embedded("A", "The gateway routes requests.", axis(0));

    insert(&mut database, &seed("Global"));
    insert(&mut database, &seed("Mine").link(&key, &[]));
    insert(&mut database, &seed("Theirs").link("git:github.com/o/other", &[]));
    drop(database);

    let titles = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--json", "--query", "gateway routes", "--project"];
        let project = world.project();
        args.push(&project);
        args.extend_from_slice(extra);

        let found = world.search(&args, &mut Fake(Ok(axis(0)))).expect("answers");
        let mut titles: Vec<String> = items(&found)
            .iter()
            .map(|item| item["title"].as_str().expect("title").to_owned())
            .collect();
        titles.sort();
        titles
    };

    assert_eq!(titles(&[]), ["Global", "Mine"]);
    assert_eq!(titles(&["--scope", "all"]), ["Global", "Mine", "Theirs"]);
}

#[test]
fn paths_repeat_and_a_matching_glob_comes_first() {
    let world = World::new();
    let key = format!("path:{}", world.project());
    let mut database = world.database();
    let seed = |title: &str| Seed::new(title).embedded("A", "The gateway routes requests.", axis(0));

    insert(&mut database, &seed("Plain").link(&key, &[]));
    insert(&mut database, &seed("Api").link(&key, &["src/api/**"]));
    drop(database);

    let project = world.project();
    let found = world
        .search(
            &[
                "--json",
                "--query",
                "gateway routes",
                "--project",
                &project,
                "--path",
                "README.md",
                "--path=src/api/server.rs",
            ],
            &mut Fake(Ok(axis(0))),
        )
        .expect("answers");

    assert_eq!(items(&found)[0]["title"], "Api");
}

#[test]
fn an_embedder_that_is_down_is_reported_for_stderr_and_the_words_answer() {
    let world = World::new();
    let mut database = world.database();
    insert(
        &mut database,
        &Seed::new("Gateway guide").chunk("A", "The gateway routes requests."),
    );
    drop(database);

    let found = world
        .search(
            &["--json", "--query", "gateway routes"],
            &mut Fake(Err("the embedder is not reachable (x)".to_owned())),
        )
        .expect("answers");

    assert_eq!(items(&found).len(), 1);
    assert!(
        found
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("keyword results only")),
        "{:?}",
        found.notice
    );
}

#[test]
fn bad_arguments_are_usage_errors_and_a_bad_project_fails() {
    let world = World::new();
    let mut embedder = Fake(Ok(axis(0)));

    for args in [
        &["--query", "gateway"][..],
        &["--json"],
        &["--json", "--query", "  "],
        &["--json", "--query", "a", "--limit", "0"],
        &["--json", "--query", "a", "--limit", "21"],
        &["--json", "--query", "a", "--limit", "x"],
        &["--json", "--query", "a", "--scope", "every"],
        &["--json", "--query", "a", "--mode", "full"],
        &["--json", "--query", "a", "--path"],
        &["--json", "--query", "a", "extra"],
        &["--json", "--query", "a", "--query", "b"],
    ] {
        assert_eq!(
            world.search(args, &mut embedder).expect_err("usage"),
            Failure::Usage,
            "{args:?}"
        );
    }

    let failure = world
        .search(
            &["--json", "--query", "a", "--project", "/no/such/folder"],
            &mut embedder,
        )
        .expect_err("fails");
    assert!(matches!(failure, Failure::Failed(message) if message.contains("--project")));
}

#[test]
fn a_store_that_lacks_a_capability_ends_the_search_with_exit_three() {
    let missing = StoreError::Missing {
        code: "no-pgvector",
        message: "the pgvector extension is not installed".to_owned(),
    };

    assert_eq!(
        Failure::from(missing),
        Failure::Missing {
            code: "no-pgvector",
            message: "the pgvector extension is not installed".to_owned()
        }
    );
}

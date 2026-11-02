//! Where an import saves when a remote database is set: the manifest's `place` and the routing.
//! The remote database in these tests is never reached, so a write that is refused or goes to this
//! computer must not open it.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::import::import_with;
use super::*;
use crate::config::Where;
use crate::extract::pdf::PdfPages;
use crate::import;
use crate::location::{DIR_VAR, FILE_NAME};

struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &std::path::Path) -> Result<Vec<String>, String> {
        Err("no PDF reader in this test".to_owned())
    }
}

struct Setup {
    root: tempfile::TempDir,
    env: Env,
    backend: Backend,
}

impl Setup {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("knowledge").to_string_lossy());
        let backend =
            crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend");

        Self { root, env, backend }
    }

    fn database_exists(&self) -> bool {
        self.root.path().join("knowledge").join(FILE_NAME).exists()
    }

    fn manifest(&self, place: Option<Place>, links: &[&str]) -> import::Manifest {
        let file = self.root.path().join("guide.md");
        std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");

        import::Manifest {
            paths: vec![file.to_string_lossy().into_owned()],
            links: links
                .iter()
                .map(|repo| Link {
                    repo_key: (*repo).to_owned(),
                    globs: Vec::new(),
                })
                .collect(),
            place,
            ..import::Manifest::default()
        }
    }

    fn import(&self, routing: Routing, manifest: &import::Manifest) -> Result<Vec<Value>, Failure> {
        let context = Context::new(&self.env, &self.backend, routing);
        let mut out: Vec<u8> = Vec::new();

        import_with(&context, manifest, &NoPdf, None, &|| false, &mut out)?;

        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON line"))
            .collect())
    }
}

#[test]
fn an_import_to_this_computer_writes_the_local_file_and_names_the_document_with_its_place() {
    let setup = Setup::new();
    let routing = Routing::new(Where::RemoteLocal, Where::RemoteLocal);

    let events = setup
        .import(routing, &setup.manifest(Some(Place::Local), &[]))
        .expect("imports");

    assert_eq!(events[1]["status"], "added");
    assert_eq!(events[1]["docId"], json!("local:1"));
    assert!(setup.database_exists());
}

#[test]
fn a_kind_the_routing_keeps_local_is_imported_there_without_asking_and_ids_stay_plain_with_one_place() {
    let setup = Setup::new();

    // Documents linked to a repository stay on this computer, the global ones are remote.
    let routing = Routing::new(Where::Local, Where::Remote);
    let manifest = setup.manifest(None, &["git:github.com/o/r"]);

    let events = setup.import(routing, &manifest).expect("imports");
    assert_eq!(events[1]["docId"], json!("local:1"), "both places are in use");

    // Everything local: one place, plain ids.
    let setup = Setup::new();
    let events = setup
        .import(Routing::new(Where::Local, Where::Local), &setup.manifest(None, &[]))
        .expect("imports");
    assert_eq!(events[1]["docId"], json!(1));
}

#[test]
fn a_place_the_routing_does_not_keep_the_kind_in_is_refused_before_anything_is_opened() {
    let setup = Setup::new();

    let linked = setup.manifest(Some(Place::Local), &["git:github.com/o/r"]);
    let refused = setup.import(Routing::new(Where::Remote, Where::RemoteLocal), &linked);
    assert!(
        matches!(&refused, Err(Failure::Failed(message)) if message.contains("repository_documents") && message.contains("in the remote database")),
        "{refused:?}"
    );

    let global = setup.manifest(Some(Place::Remote), &[]);
    let refused = setup.import(Routing::new(Where::RemoteLocal, Where::Local), &global);
    assert!(
        matches!(&refused, Err(Failure::Failed(message)) if message.contains("global_documents")),
        "{refused:?}"
    );

    assert!(!setup.database_exists());
}

#[test]
fn without_a_remote_database_only_this_computer_is_a_place() {
    let setup = Setup::new();
    let context = Context::new(&setup.env, &Backend::Sqlite, Routing::local());
    let mut out: Vec<u8> = Vec::new();

    let remote = setup.manifest(Some(Place::Remote), &[]);
    let refused = import_with(&context, &remote, &NoPdf, None, &|| false, &mut out);
    assert!(matches!(refused, Err(Failure::Failed(message)) if message.contains("no remote database")));

    let default = setup.manifest(None, &[]);
    import_with(&context, &default, &NoPdf, None, &|| false, &mut out).expect("imports locally");
    assert!(setup.database_exists());
}

#[test]
fn a_replacement_goes_to_the_place_of_the_document_it_replaces() {
    let setup = Setup::new();
    let routing = Routing::new(Where::RemoteLocal, Where::RemoteLocal);

    setup
        .import(routing, &setup.manifest(Some(Place::Local), &[]))
        .expect("first");

    let mut again = setup.manifest(None, &[]);
    again.replaces = Some(crate::store::routed::ids::tag(Place::Local, 1));

    let events = setup.import(routing, &again).expect("replaces");
    assert_eq!(events[1]["docId"], json!("local:1"));
    assert!(
        matches!(events[1]["status"].as_str(), Some("updated" | "unchanged")),
        "{events:?}"
    );

    let context = Context::new(&setup.env, &setup.backend, Routing::new(Where::Local, Where::Local));
    let listed = docs_reply(&context, &[Some("list"), Some("--json")], &mut std::io::empty()).expect("list");
    assert_eq!(listed.json["total"], 1);
}

/// A schema of the PostgreSQL server `HARNESS_KNOWLEDGE_TEST_POSTGRES_URL` names (with pgvector),
/// dropped on drop. `None` (after saying so) without the variable.
struct Schema {
    admin_url: String,
    name: String,
}

impl Schema {
    fn create() -> Option<Self> {
        let admin_url = std::env::var("HARNESS_KNOWLEDGE_TEST_POSTGRES_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());

        let Some(admin_url) = admin_url else {
            eprintln!("skipped: set HARNESS_KNOWLEDGE_TEST_POSTGRES_URL to run the two-place replacement test");
            return None;
        };

        let name = format!("hk_replacing_{}", std::process::id());
        let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).expect("the test server answers");

        admin
            .batch_execute("CREATE EXTENSION IF NOT EXISTS vector SCHEMA public")
            .expect("pgvector");
        admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{name}\" CASCADE; CREATE SCHEMA \"{name}\""
            ))
            .expect("schema");

        Some(Self { admin_url, name })
    }

    fn url(&self) -> String {
        let separator = if self.admin_url.contains('?') { '&' } else { '?' };

        format!("{}{separator}options=-csearch_path%3D{}", self.admin_url, self.name)
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        if let Ok(mut admin) = postgres::Client::connect(&self.admin_url, postgres::NoTls) {
            let _ = admin.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.name));
        }
    }
}

#[test]
fn a_replacement_that_names_a_place_that_is_not_in_use_is_refused_before_anything_is_opened() {
    let setup = Setup::new();
    let mut manifest = setup.manifest(None, &[]);
    let file = manifest.paths[0].clone();

    manifest
        .replacing
        .insert(file, crate::store::routed::ids::tag(Place::Local, 1));

    // This computer is not a place when everything is remote.
    let refused = setup.import(Routing::new(Where::Remote, Where::Remote), &manifest);
    assert!(
        matches!(&refused, Err(Failure::Failed(message)) if message.contains("not in use") && message.contains("this computer")),
        "{refused:?}"
    );

    assert!(!setup.database_exists());
}

#[test]
fn files_replacing_documents_of_both_places_are_saved_each_where_its_document_is() {
    let Some(schema) = Schema::create() else { return };
    let root = tempfile::tempdir().expect("temp dir");
    let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("knowledge").to_string_lossy());
    let backend = crate::config::backend_for(Some(schema.url()), None).expect("backend");
    let context = Context::new(&env, &backend, Routing::new(Where::RemoteLocal, Where::RemoteLocal));

    let file = |name: &str, text: &str| {
        let path = root.path().join(name);
        std::fs::write(&path, text).expect("write");
        path.to_string_lossy().into_owned()
    };
    let run = |manifest: &import::Manifest| -> Vec<Value> {
        let mut out: Vec<u8> = Vec::new();
        import_with(&context, manifest, &NoPdf, None, &|| false, &mut out).expect("imports");

        String::from_utf8_lossy(&out)
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON line"))
            .collect()
    };
    let files = |paths: &[&String], place: Option<Place>| import::Manifest {
        paths: paths.iter().map(|path| (*path).clone()).collect(),
        place,
        ..import::Manifest::default()
    };

    // One document in each place.
    let remote_old = file("remote-old.md", "# Remote old\n\nThe first remote text.");
    let local_old = file("local-old.md", "# Local old\n\nThe first local text.");
    run(&files(&[&remote_old], Some(Place::Remote)));
    run(&files(&[&local_old], Some(Place::Local)));

    // A batch replaces both and adds a third, in the default place.
    let remote_new = file("remote-new.md", "# Remote new\n\nThe second remote text.");
    let local_new = file("local-new.md", "# Local new\n\nThe second local text.");
    let fresh = file("fresh.md", "# Fresh\n\nA new document.");
    let mut batch = files(&[&local_new, &fresh, &remote_new], None);
    batch.replacing.insert(remote_new.clone(), 1);
    batch
        .replacing
        .insert(local_new.clone(), crate::store::routed::ids::tag(Place::Local, 1));

    let events = run(&batch);
    let file_events: Vec<&Value> = events.iter().filter(|event| event["type"] == "file").collect();
    let by_name = |name: &str| {
        file_events
            .iter()
            .find(|event| event["path"].as_str().is_some_and(|path| path.ends_with(name)))
            .copied()
            .unwrap_or_else(|| panic!("no event for {name}: {events:?}"))
    };

    assert_eq!(events[0], json!({ "type": "plan", "total": 3 }));
    assert_eq!(events.last().expect("done")["counts"]["updated"], 2);
    assert_eq!(events.last().expect("done")["counts"]["added"], 1);
    assert_eq!(by_name("remote-new.md")["status"], "updated");
    assert_eq!(by_name("remote-new.md")["docId"], json!(1));
    assert_eq!(by_name("local-new.md")["status"], "updated");
    assert_eq!(by_name("local-new.md")["docId"], json!("local:1"));
    assert_eq!(by_name("fresh.md")["status"], "added");

    let listed = docs_reply(
        &context,
        &[Some("list"), Some("--json"), Some("--sort"), Some("title")],
        &mut std::io::empty(),
    )
    .expect("list");
    let titles: Vec<(&str, &str)> = listed.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| {
            (
                item["title"].as_str().expect("title"),
                item["place"].as_str().expect("place"),
            )
        })
        .collect();

    assert_eq!(
        titles,
        [("Fresh", "remote"), ("Local new", "local"), ("Remote new", "remote")],
        "two replaced where they were, one new"
    );

    // `docs similar` looks in both places and names the place of each match.
    let similar = docs_reply(
        &context,
        &[
            Some("similar"),
            Some("--json"),
            Some(local_new.as_str()),
            Some(remote_new.as_str()),
        ],
        &mut std::io::empty(),
    )
    .expect("similar");
    let found = |index: usize| similar.json["similar"][index]["match"].clone();

    assert_eq!(
        (found(0)["id"].clone(), found(0)["place"].clone()),
        (json!("local:1"), json!("local"))
    );
    assert_eq!(
        (found(1)["id"].clone(), found(1)["place"].clone()),
        (json!(1), json!("remote"))
    );
    assert_eq!(found(0)["reason"], "sourceKey");
}

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

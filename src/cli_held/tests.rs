use serde_json::{Value, json};

use super::*;
use crate::backend::Access;
use crate::held::{Proposal, Source};
use crate::location::{DIR_VAR, FILE_NAME};
use crate::search::{self, Query, Tuning};
use crate::store::testing::{Seed, insert};
use crate::store::{Link, Scope, SqliteStore};

/// A computer: a home folder and a knowledge folder.
struct World {
    dir: tempfile::TempDir,
    env: Env,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests(dir.path()).with_var(DIR_VAR, dir.path().join("knowledge").to_string_lossy());

        Self { dir, env }
    }

    fn held(&self) -> HeldStore {
        HeldStore::open(&self.env).expect("held store")
    }

    /// Holds a proposal and returns its id.
    fn propose(&self, title: &str, content: &str, links: Vec<Link>, replaces: Option<i64>) -> String {
        let item = HeldItem::new(
            Source {
                client: "codex".to_owned(),
                session: None,
                project_path: Some("/work/repo".to_owned()),
                repo_key: Some("git:github.com/o/r".to_owned()),
            },
            Proposal {
                title: title.to_owned(),
                format: "markdown".to_owned(),
                name: "proposal.md".to_owned(),
                source_path: None,
                content: content.to_owned(),
                links,
                replaces,
            },
        );

        self.held().hold(item).expect("held")
    }

    fn run(&self, args: &[&str]) -> Reply {
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        reply(&self.env, &Backend::Sqlite, Routing::local(), &args)
    }

    fn done(&self, args: &[&str]) -> Value {
        match self.run(args) {
            Reply::Done(json) => json,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn refused(&self, args: &[&str]) -> (Code, String) {
        match self.run(args) {
            Reply::Refused(code, message) => (code, message),
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn database_exists(&self) -> bool {
        self.dir.path().join("knowledge").join(FILE_NAME).exists()
    }

    fn store(&self) -> Box<dyn crate::store::KnowledgeStore> {
        backend::open(&self.env, &Backend::Sqlite, Routing::local(), Access::Read)
            .expect("open")
            .expect("the database exists")
    }
}

fn link(repo: &str) -> Link {
    Link {
        repo_key: repo.to_owned(),
        globs: vec!["src/**".to_owned()],
    }
}

const TEXT: &str = "# Gateway runbook\n\nRestart the zebrafish gateway with the zebrafish command.";

#[test]
fn list_and_get_show_the_queue_newest_first_with_the_text_only_in_get() {
    let world = World::new();
    assert_eq!(world.done(&["list", "--json"]), json!({ "held": [] }));
    assert!(!world.database_exists());

    let id = world.propose("Gateway runbook", TEXT, vec![link("git:github.com/o/r")], None);

    let listed = world.done(&["list", "--json"]);
    let item = &listed["held"][0];
    assert_eq!(item["heldId"], id);
    assert_eq!(item["title"], "Gateway runbook");
    assert_eq!(item["client"], "codex");
    assert_eq!(item["links"][0]["globs"], json!(["src/**"]));
    assert!(item.get("content").is_none());
    assert!(
        item["preview"]
            .as_str()
            .expect("preview")
            .starts_with("# Gateway runbook")
    );

    let got = world.done(&["get", "--json", &id]);
    assert_eq!(got["held"]["content"], TEXT);
    assert_eq!(got["held"]["revision"], item["revision"]);

    assert_eq!(
        world
            .refused(&["get", "--json", "00000000-0000-4000-8000-000000000000"])
            .0,
        Code::NotFound
    );
    assert_eq!(world.refused(&["get", "--json", "../etc/passwd"]).0, Code::NotFound);
    assert!(!world.database_exists(), "reading the queue creates no database");
}

#[test]
fn an_approved_proposal_is_a_document_found_by_search_and_leaves_the_queue() {
    let world = World::new();
    let id = world.propose("Gateway runbook", TEXT, vec![link("git:github.com/o/r")], None);

    let approved = world.done(&["approve", "--json", &id]);
    let result = &approved["approved"];

    assert_eq!(result["heldId"], id);
    assert_eq!(result["status"], "added");
    assert_eq!(result["title"], "Gateway runbook");
    assert_eq!(
        result["embedding"], "pending",
        "no embedder runs here, the chunks wait for a reindex"
    );
    assert!(result["docId"].as_i64().is_some_and(|doc| doc > 0));
    assert_eq!(world.done(&["list", "--json"]), json!({ "held": [] }));

    // The document is there with the agent's title, origin and links, and a search finds it.
    let mut store = world.store();
    let doc_id = result["docId"].as_i64().expect("doc id");
    let detail = store
        .get(doc_id, &crate::store::Detail::default())
        .expect("get")
        .expect("the document");

    assert_eq!(detail.doc.title, "Gateway runbook");
    assert_eq!(detail.doc.origin, Origin::AgentApproved);
    assert_eq!(detail.links, vec![link("git:github.com/o/r")]);

    let scope = Scope::everything();
    let query = Query {
        text: "zebrafish gateway",
        scope: &scope,
        paths: &[],
        limit: 5,
    };
    let found = search::run(store.as_mut(), None, &query, Tuning::SEARCH).expect("search");

    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].hit.title, "Gateway runbook");

    // A second approval finds nothing to approve.
    assert_eq!(world.refused(&["approve", "--json", &id]).0, Code::NotFound);
}

#[test]
fn the_title_the_agent_gave_wins_over_the_one_in_the_text() {
    let world = World::new();
    let id = world.propose(
        "Agent title",
        "# Heading in the text\n\nSome body text here.",
        Vec::new(),
        None,
    );

    let doc = world.done(&["approve", "--json", &id])["approved"]["docId"]
        .as_i64()
        .expect("doc");
    let detail = world
        .store()
        .get(doc, &crate::store::Detail::default())
        .expect("get")
        .expect("doc");

    assert_eq!(detail.doc.title, "Agent title");
    assert!(detail.links.is_empty(), "a global document");
}

#[test]
fn approving_text_that_is_already_imported_is_unchanged_and_adds_the_links() {
    let world = World::new();
    let first = world.propose("Gateway runbook", TEXT, Vec::new(), None);
    let doc = world.done(&["approve", "--json", &first])["approved"]["docId"]
        .as_i64()
        .expect("doc");

    let again = world.propose("Gateway runbook", TEXT, vec![link("git:github.com/o/r")], None);
    let approved = world.done(&["approve", "--json", &again]);

    assert_eq!(approved["approved"]["status"], "unchanged");
    assert_eq!(approved["approved"]["docId"], doc);

    let detail = world
        .store()
        .get(doc, &crate::store::Detail::default())
        .expect("get")
        .expect("doc");
    assert_eq!(detail.links.len(), 1);
}

#[test]
fn a_replacing_proposal_updates_the_document_in_place() {
    let world = World::new();
    let first = world.propose("Gateway runbook", TEXT, Vec::new(), None);
    let doc = world.done(&["approve", "--json", &first])["approved"]["docId"]
        .as_i64()
        .expect("doc");

    let replacing = world.propose(
        "Gateway runbook",
        "# Gateway runbook\n\nA new text about the gateway.",
        Vec::new(),
        Some(doc),
    );
    let approved = world.done(&["approve", "--json", &replacing]);

    assert_eq!(approved["approved"]["status"], "updated");
    assert_eq!(approved["approved"]["docId"], doc);
    assert_eq!(world.store().stats().expect("stats").documents, 1);
}

#[test]
fn an_approval_that_fails_puts_the_proposal_back() {
    let world = World::new();
    let id = world.propose(
        "Replaces nothing",
        "# Text\n\nSome text for nobody.",
        Vec::new(),
        Some(999),
    );

    let (code, message) = world.refused(&["approve", "--json", &id]);

    assert_eq!(code, Code::Refused);
    assert!(message.contains("999"), "{message}");
    assert_eq!(
        world.done(&["list", "--json"])["held"][0]["heldId"],
        id,
        "it waits for another try"
    );
    assert_eq!(world.store().stats().expect("stats").documents, 0);
}

#[test]
fn a_rejected_proposal_is_deleted_and_the_database_untouched() {
    let world = World::new();
    let id = world.propose("Unwanted", "# Unwanted\n\nSome text.", Vec::new(), None);

    assert_eq!(world.done(&["reject", "--json", &id]), json!({ "rejected": id }));
    assert_eq!(world.done(&["list", "--json"]), json!({ "held": [] }));
    assert_eq!(world.refused(&["reject", "--json", &id]).0, Code::NotFound);
    assert!(!world.database_exists());
}

#[test]
fn a_decision_on_a_proposal_that_changed_is_refused_and_it_keeps_waiting() {
    let world = World::new();
    let id = world.propose("Guide", "# Guide\n\nSome text here.", Vec::new(), None);
    let revision = world.done(&["get", "--json", &id])["held"]["revision"]
        .as_str()
        .expect("revision")
        .to_owned();

    for decision in ["approve", "reject"] {
        let (code, _) = world.refused(&[decision, "--json", &id, "--expect", "not-the-revision"]);
        assert_eq!(code, Code::Changed);
        assert_eq!(
            world.done(&["list", "--json"])["held"].as_array().map(Vec::len),
            Some(1)
        );
    }

    assert_eq!(
        world.done(&["reject", "--json", &id, "--expect", &revision]),
        json!({ "rejected": id })
    );
}

#[test]
fn two_approvals_of_one_proposal_import_it_once() {
    let world = World::new();
    let id = world.propose("Gateway runbook", TEXT, Vec::new(), None);

    let outcomes: Vec<Reply> = std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| world.run(&["approve", "--json", &id])))
            .collect();

        attempts
            .into_iter()
            .map(|attempt| attempt.join().expect("thread"))
            .collect()
    });

    let approved = outcomes.iter().filter(|reply| matches!(reply, Reply::Done(_))).count();

    // A claim that loses finds nothing to approve, and every other outcome is a refusal.
    assert_eq!(approved, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|reply| matches!(reply, Reply::Done(_) | Reply::Refused(Code::NotFound, _))),
        "{outcomes:?}"
    );
    assert_eq!(world.store().stats().expect("stats").documents, 1);
}

#[test]
fn approving_into_an_existing_database_keeps_its_documents() {
    let world = World::new();
    let mut database = SqliteStore::open(&world.dir.path().join("knowledge").join(FILE_NAME)).expect("database");
    insert(&mut database, &Seed::new("Existing").chunk("A", "text"));
    drop(database);

    let id = world.propose("Gateway runbook", TEXT, Vec::new(), None);
    world.done(&["approve", "--json", &id]);

    assert_eq!(world.store().stats().expect("stats").documents, 2);
}

#[test]
fn bad_arguments_are_usage_errors() {
    let world = World::new();

    for args in [
        &[][..],
        &["list"],
        &["get", "--json"],
        &["approve", "--json"],
        &["approve", "--json", "a", "b"],
        &["reject", "id"],
        &["frobnicate", "--json"],
        &["list", "--json", "extra"],
    ] {
        assert_eq!(world.run(args), Reply::Usage, "{args:?}");
    }
}

mod keys;
mod replaces;

/// A team database that is never reached: an approval into this computer's file does not open it.
fn unreachable_remote() -> Backend {
    crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend")
}

#[test]
fn an_approval_goes_where_place_says_and_the_routing_allows() {
    use crate::config::Where;

    let world = World::new();
    let remote = unreachable_remote();
    let both = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
    let approve = |held: &str, routing: Routing, extra: &[&str]| {
        let mut args = vec!["approve", "--json", held];
        args.extend_from_slice(extra);
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        reply(&world.env, &remote, routing, &args)
    };

    // To this computer, with the id written as such because both places are in use.
    let first = world.propose("Local runbook", TEXT, Vec::new(), None);
    let Reply::Done(done) = approve(&first, both, &["--place", "local"]) else {
        panic!("approved");
    };
    assert_eq!(done["approved"]["status"], "added");
    assert_eq!(done["approved"]["docId"], json!("local:1"));
    assert!(world.database_exists());
    assert_eq!(world.store().stats().expect("stats").documents, 1);

    // A place the routing does not keep that kind in is refused, and the proposal waits.
    let second = world.propose(
        "Linked runbook",
        "Another zebrafish text.",
        vec![link("git:github.com/o/r")],
        None,
    );
    let local_only = Routing::new(Where::Local, Where::RemoteLocal);
    let Reply::Refused(Code::Refused, message) = approve(&second, local_only, &["--place", "remote"]) else {
        panic!("refused");
    };
    assert!(
        message.contains("repository_documents") && message.contains("on this computer"),
        "{message}"
    );
    assert_eq!(world.held().count(), 1, "the proposal is back in the queue");

    // Without --place a kind kept local goes local, and a bad place is a usage error.
    let Reply::Done(done) = approve(&second, local_only, &[]) else {
        panic!("approved");
    };
    assert_eq!(done["approved"]["docId"], json!("local:2"));
    assert_eq!(approve("whatever", both, &["--place", "cloud"]), Reply::Usage);

    let reject = world.propose("Third", "More text here.", Vec::new(), None);
    let args = [
        Some("reject"),
        Some("--json"),
        Some(reject.as_str()),
        Some("--place"),
        Some("local"),
    ];
    assert_eq!(
        reply(&world.env, &remote, both, &args),
        Reply::Usage,
        "only an approval takes --place"
    );
}

#[test]
fn without_a_remote_database_only_this_computer_can_be_chosen() {
    let world = World::new();
    let held = world.propose("Runbook", TEXT, Vec::new(), None);

    let (code, message) = {
        let args = [
            Some("approve"),
            Some("--json"),
            Some(held.as_str()),
            Some("--place"),
            Some("remote"),
        ];
        match reply(&world.env, &Backend::Sqlite, Routing::local(), &args) {
            Reply::Refused(code, message) => (code, message),
            other => panic!("{other:?}"),
        }
    };

    assert_eq!(code, Code::Refused);
    assert!(message.contains("no remote database"), "{message}");

    let done = world.done(&["approve", "--json", &held, "--place", "local"]);
    assert_eq!(done["approved"]["docId"], 1, "plain ids with one place in use");
}

#[test]
fn an_approval_to_a_remote_that_cannot_be_reached_is_refused_and_recorded_like_an_import() {
    use crate::config::Where;

    let world = World::new();
    let remote = unreachable_remote();
    let both = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
    let held = world.propose("Runbook", TEXT, Vec::new(), None);

    let args = [Some("approve"), Some("--json"), Some(held.as_str())];
    let Reply::Unreachable(message) = reply(&world.env, &remote, both, &args) else {
        panic!("refused as unreachable");
    };

    assert_eq!(message, REMOTE_DOWN_WRITE);
    assert_eq!(
        world.held().count(),
        1,
        "the proposal waits, nothing is saved to this computer"
    );
    assert!(!world.database_exists());
    assert!(
        world
            .dir
            .path()
            .join("knowledge")
            .join("remote-unreachable.json")
            .exists(),
        "the failure goes to the cooldown that every other command shares"
    );
}

#[test]
fn a_proposal_shows_the_document_it_replaces_as_clients_read_ids() {
    use crate::store::routed::ids;

    let held = |replaces| {
        HeldItem::new(
            Source {
                client: "tests".to_owned(),
                session: None,
                project_path: None,
                repo_key: None,
            },
            Proposal {
                title: "T".to_owned(),
                format: "markdown".to_owned(),
                name: "t.md".to_owned(),
                source_path: None,
                content: "text".to_owned(),
                links: Vec::new(),
                replaces: Some(replaces),
            },
        )
    };

    assert_eq!(
        super::public(&held(ids::tag(crate::config::Place::Remote, 12)))["replaces"],
        serde_json::json!(12)
    );
    assert_eq!(
        super::public(&held(ids::tag(crate::config::Place::Local, 12)))["replaces"],
        serde_json::json!("local:12")
    );
}

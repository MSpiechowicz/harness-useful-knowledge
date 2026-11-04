//! The person's reads see drafts with their state (`docs list`, `docs get`, `docs similar`), and a
//! pending version only through the live document it replaces.

use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;
use crate::extract::pdf::PdfPages;
use crate::import::manifest::Manifest;
use crate::import::similar;
use crate::import::walk;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::testing::{Seed, insert};
use crate::store::{ReviewAction, ReviewChange, SqliteStore, Status, TitleScope};
use crate::tokens::counter_for;

struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("no PDF reader in this test".to_owned())
    }
}

struct Lab {
    root: tempfile::TempDir,
    env: Env,
}

/// The ids of the fixture, in the order they are stored.
struct Rows {
    live: i64,
    draft: i64,
    waiting: i64,
    changes: i64,
    pending: i64,
    /// A live document with no pending version.
    plain: i64,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("knowledge").to_string_lossy());

        Self { root, env }
    }

    fn database(&self) -> SqliteStore {
        SqliteStore::open(&self.root.path().join("knowledge").join(FILE_NAME)).expect("database")
    }

    fn seeded() -> (Self, Rows) {
        let lab = Self::new();
        let mut store = lab.database();
        let doc = |title: &str| Seed::new(title).chunk("Intro", "Install the gateway with the installer.");
        let live = insert(&mut store, &doc("Gateway live"));
        let draft = insert(&mut store, &doc("Gateway draft").status(Status::Draft));
        let waiting = insert(&mut store, &doc("Gateway waiting").status(Status::Draft));
        let changes = insert(&mut store, &doc("Gateway changes").status(Status::Draft));
        let pending = insert(&mut store, &doc("Gateway live, newer").pending_of(live));
        let plain = insert(&mut store, &doc("Gateway plain"));

        // The history of the rows that were asked and sent back.
        let rev = |store: &mut SqliteStore, id| {
            store
                .get(
                    id,
                    &Detail {
                        drafts: true,
                        ..Detail::default()
                    },
                )
                .expect("get")
                .expect("row")
                .doc
                .review_rev
        };
        let at = rev(&mut store, waiting);
        store
            .set_review(waiting, at, &ReviewChange::new(ReviewAction::Ask, Some("Ada"), 5_000))
            .expect("ask");
        let at = rev(&mut store, changes);
        store
            .set_review(changes, at, &ReviewChange::new(ReviewAction::Ask, Some("Ada"), 6_000))
            .expect("ask");
        let at = rev(&mut store, changes);
        store
            .set_review(
                changes,
                at,
                &ReviewChange::new(ReviewAction::RequestChanges, Some("Grace"), 7_000).with_comment("Add the table"),
            )
            .expect("request changes");

        (
            lab,
            Rows {
                live,
                draft,
                waiting,
                changes,
                pending,
                plain,
            },
        )
    }

    fn context(&self) -> Context<'_> {
        Context::new(&self.env, &Backend::Sqlite, Routing::local())
    }

    fn run(&self, args: &[&str]) -> Reply {
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        docs_reply(&self.context(), &args, &mut std::io::empty())
    }

    fn json(&self, args: &[&str]) -> Value {
        self.run(args)
            .unwrap_or_else(|failure| panic!("{args:?}: {failure:?}"))
            .json
    }

    /// `(id, status)` of the rows `docs list --status` keeps, by id.
    fn listed(&self, status: Option<&str>) -> Vec<(i64, String)> {
        let mut args = vec!["list", "--json", "--per-page", "100"];

        if let Some(status) = status {
            args.extend(["--status", status]);
        }

        let mut rows: Vec<(i64, String)> = self.json(&args)["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| {
                (
                    item["id"].as_i64().expect("id"),
                    item["status"].as_str().expect("status").to_owned(),
                )
            })
            .collect();
        rows.sort();

        rows
    }
}

fn state(id: i64, status: &str) -> (i64, String) {
    (id, status.to_owned())
}

#[test]
fn a_list_shows_drafts_with_their_status_and_never_a_pending_version_as_a_row() {
    let (lab, rows) = Lab::seeded();
    let everything = vec![
        state(rows.live, "live"),
        state(rows.draft, "draft"),
        state(rows.waiting, "waiting"),
        state(rows.changes, "changesRequested"),
        state(rows.plain, "live"),
    ];

    assert_eq!(lab.listed(None), everything);
    assert_eq!(lab.listed(Some("all")), everything);
}

#[test]
fn status_keeps_the_rows_in_that_state_and_the_live_ones_whose_pending_version_is() {
    let (lab, rows) = Lab::seeded();

    assert_eq!(
        lab.listed(Some("live")),
        [state(rows.live, "live"), state(rows.plain, "live")]
    );
    assert_eq!(
        lab.listed(Some("draft")),
        [state(rows.live, "live"), state(rows.draft, "draft")],
        "the live document whose pending version is a draft"
    );
    assert_eq!(lab.listed(Some("waiting")), [state(rows.waiting, "waiting")]);
    assert_eq!(
        lab.listed(Some("changesRequested")),
        [state(rows.changes, "changesRequested")]
    );
    assert_eq!(lab.listed(Some("pending")), [state(rows.live, "live")]);

    for wrong in ["", "drafts", "Live", "approved"] {
        assert_eq!(
            lab.run(&["list", "--json", "--status", wrong]).map(|_| ()),
            Err(Failure::Usage),
            "{wrong:?}"
        );
    }
}

#[test]
fn a_row_says_its_revision_and_a_live_row_its_pending_version() {
    let (lab, rows) = Lab::seeded();
    let listing = lab.json(&["list", "--json", "--per-page", "100"]);
    let item = |id: i64| {
        listing["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == id)
            .expect("listed")
            .clone()
    };

    let live = item(rows.live);
    assert_eq!(live["revision"], 0);
    assert_eq!(live["pending"]["id"], rows.pending);
    assert_eq!(live["pending"]["status"], "draft");
    assert_eq!(live["pending"]["revision"], 0);
    assert!(live["pending"].get("updatedBy").is_some() && live["pending"].get("updatedAt").is_some());
    assert_eq!(item(rows.plain)["pending"], Value::Null);
    assert!(item(rows.draft).get("pending").is_none(), "only a live row has one");
    assert!(item(rows.waiting)["revision"].as_i64().expect("revision") > 0);
    assert!(
        listing["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|item| item.get("pendingOf").is_none())
    );
}

#[test]
fn a_get_opens_a_draft_and_a_pending_version_and_says_what_a_reviewer_asked() {
    let (lab, rows) = Lab::seeded();

    let draft = &lab.json(&["get", "--json", &rows.draft.to_string()])["document"];
    assert_eq!(
        (draft["status"].clone(), draft["lastChange"].clone()),
        (json!("draft"), Value::Null)
    );
    assert_eq!(draft["lastRequest"], Value::Null);
    assert!(draft.get("pending").is_none() && draft.get("pendingOf").is_none());

    let changes = &lab.json(&["get", "--json", &rows.changes.to_string()])["document"];
    assert_eq!(changes["status"], "changesRequested");
    assert_eq!(
        changes["lastChange"],
        json!({ "by": "Grace", "at": "1970-01-01T01:56:40Z", "comment": "Add the table" })
    );
    assert_eq!(
        changes["lastRequest"],
        json!({ "by": "Ada", "at": "1970-01-01T01:40:00Z", "comment": null })
    );

    let pending = &lab.json(&["get", "--json", &rows.pending.to_string()])["document"];
    assert_eq!(
        (pending["status"].clone(), pending["pendingOf"].clone()),
        (json!("draft"), json!(rows.live))
    );
    assert!(pending.get("pending").is_none());

    let live = &lab.json(&["get", "--json", &rows.live.to_string(), "--sections"]);
    assert_eq!(live["document"]["status"], "live");
    assert_eq!(live["document"]["pending"]["id"], rows.pending);
    assert!(live["document"].get("lastChange").is_none());
    assert_eq!(live["sections"].as_array().expect("sections").len(), 1);
}

#[test]
fn the_matches_inside_a_draft_and_a_pending_version_can_be_read() {
    let (lab, rows) = Lab::seeded();

    for id in [rows.draft, rows.pending] {
        let found = lab.json(&["matches", "--json", &id.to_string(), "--query", "installer"]);

        assert_eq!(found["total"], 1, "{id}: {found}");
    }

    assert!(matches!(
        lab.run(&["matches", "--json", "99", "--query", "installer"]),
        Err(Failure::Failed(_))
    ));
}

#[test]
fn a_get_of_a_missing_row_still_fails() {
    let (lab, _) = Lab::seeded();

    assert!(matches!(lab.run(&["get", "--json", "99"]), Err(Failure::Failed(_))));
}

/// Imports `text` as a draft the way a draft import leaves it, by making the row a draft.
fn guide(title: &str, middle: &str) -> String {
    let filler = |tag: &str| {
        (0..8)
            .map(|n| format!("The {tag} part explains detail number {n} of how requests flow."))
            .collect::<Vec<_>>()
            .join(" ")
    };

    format!(
        "# {title}\n\n## Intro\n\n{}\n\n## Middle\n\n{middle}\n\n## Limits\n\n{}\n",
        filler("intro"),
        filler("limits")
    )
}

fn import_file(lab: &Lab, name: &str, text: &str) -> std::path::PathBuf {
    let path = lab.root.path().join("files").join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("folders");
    std::fs::write(&path, text).expect("write");

    let manifest = Manifest {
        paths: vec![path.to_string_lossy().into_owned()],
        ..Manifest::default()
    };
    super::import::import_with(&lab.context(), &manifest, &NoPdf, None, &|| false, &mut Vec::new()).expect("imports");

    path
}

fn similar_match(lab: &Lab, file: &Path) -> Value {
    let done = lab
        .run(&["similar", "--json", &file.to_string_lossy()])
        .expect("similar answers");

    done.json["similar"][0]["match"].clone()
}

#[test]
fn similar_finds_a_draft_for_the_person_with_its_status() {
    let lab = Lab::new();
    let file = import_file(&lab, "guide.md", &guide("Guide", "How it works."));
    let live = similar_match(&lab, &file);
    assert_eq!((live["id"].clone(), live["status"].clone()), (json!(1), json!("live")));

    for status in ["draft", "waiting", "changesRequested"] {
        rusqlite::Connection::open(lab.root.path().join("knowledge").join(FILE_NAME))
            .expect("second connection")
            .execute("UPDATE knowledge_documents SET status = ?1", [status])
            .expect("set the status");

        let found = similar_match(&lab, &file);
        assert_eq!(
            (found["id"].clone(), found["status"].clone()),
            (json!(1), json!(status))
        );
        assert_eq!(found["reason"], "sourceKey");
    }
}

#[test]
fn similar_never_finds_a_pending_version() {
    let lab = Lab::new();
    import_file(&lab, "guide.md", &guide("Guide", "How it works."));
    let newer = lab.root.path().join("files").join("newer.md");
    std::fs::write(&newer, guide("Guide", "How it works, edited.")).expect("write");

    // The pending version of the guide holds exactly the newer text.
    let candidates = walk::plan(&Manifest {
        paths: vec![newer.to_string_lossy().into_owned()],
        ..Manifest::default()
    })
    .expect("plan");
    let counter = counter_for(&lab.env);
    let probe = similar::probe(&candidates[0], &NoPdf, counter.as_ref(), TitleScope::Global).expect("probe");
    let mut store = lab.database();
    let (mut doc, _, _) = Seed::new("Guide").pending_of(1).parts();
    doc.content_sha256 = probe.content_sha.clone();
    doc.source_key = probe.source_key.clone();
    let chunks: Vec<_> = probe
        .chunk_shas
        .iter()
        .enumerate()
        .map(|(ordinal, sha)| {
            let (_, mut chunks, _) = Seed::new("Guide").chunk("Section", &format!("text {ordinal}")).parts();
            let mut chunk = chunks.remove(0);
            chunk.ordinal = ordinal;
            chunk.chunk_sha = sha.clone();
            chunk
        })
        .collect();
    let pending = store.put_document(&doc, &chunks, &[]).expect("pending version");
    drop(store);

    let found = similar_match(&lab, &newer);

    assert_ne!(found["id"], json!(pending), "{found}");
    assert_eq!(found["id"], 1, "the live document it replaces: {found}");
    assert_eq!(found["status"], "live");
    assert_eq!(found["reason"], "sharedChunks");
}

//! Imports that write drafts: a new document as a draft, a live one replaced by a pending version,
//! a draft that stays one, the original file, and asking for approval in the same step.

use pretty_assertions::assert_eq;

use super::*;
use crate::store::{Author, Document, EventKind, Status, Visibility};

/// How an import is set: drafts asked for, team approval on, asking, and who writes.
#[derive(Default, Clone)]
struct Mode {
    draft: bool,
    approval: bool,
    ask: bool,
    note: Option<&'static str>,
    name: Option<&'static str>,
}

fn approval() -> Mode {
    Mode {
        approval: true,
        name: Some("Ada"),
        ..Mode::default()
    }
}

fn import(store: &mut SqliteStore, file: &Path, replaces: Option<i64>, mode: &Mode) -> Value {
    let mut output: Vec<u8> = Vec::new();
    let manifest = manifest(&[file], &[]);
    let mut importer = Importer::new(store, &ApproxCounter, &InProcess, &never);
    importer.replaces = replaces;
    importer.draft = mode.draft;
    importer.approval = mode.approval;
    importer.ask = mode.ask;
    importer.note = mode.note.map(str::to_owned);
    importer.author = mode.name.map(|name| Author {
        name: name.to_owned(),
        via: None,
    });

    run(&mut importer, &manifest, &mut Events::new(&mut output)).expect("the import runs");

    let events: Vec<Value> = String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect();

    file_event(&events, file.file_name().and_then(|name| name.to_str()).expect("name")).clone()
}

fn row(store: &mut SqliteStore, id: i64) -> Document {
    store
        .get(
            id,
            &Detail {
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("get")
        .expect("the row exists")
        .doc
}

fn live_ids(store: &mut SqliteStore) -> Vec<i64> {
    all_documents(store)
        .iter()
        .filter(|row| row.doc.status == Status::Live)
        .map(|row| row.doc.id)
        .collect()
}

fn history(store: &mut SqliteStore, id: i64) -> Vec<EventKind> {
    store
        .history(id)
        .expect("history")
        .expect("a history")
        .events
        .iter()
        .map(|event| event.kind)
        .collect()
}

#[test]
fn an_import_keeps_the_file_as_its_original_for_download() {
    let dir = tempfile::tempdir().expect("temp dir");
    let text = guide("The first version.");
    let file = write(dir.path(), "guide.md", &text);
    let mut db = store();

    let event = import(&mut db, &file, None, &Mode::default());
    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("added"), Some(false))
    );

    let kept = db.file(1).expect("file").expect("the original is kept");
    assert_eq!(kept.name, "guide.md");
    assert_eq!(kept.bytes, text.as_bytes());

    // A replaced document gets the new original, not the old one.
    let newer = guide("A second version.");
    write(dir.path(), "guide.md", &newer);
    let event = import(&mut db, &file, None, &Mode::default());
    assert_eq!(event["status"], "updated");
    assert_eq!(db.file(1).expect("file").expect("kept").bytes, newer.as_bytes());

    // The same text again changes nothing, the file included.
    assert_eq!(import(&mut db, &file, None, &Mode::default())["status"], "unchanged");
}

#[test]
fn a_draft_is_stored_for_the_person_and_found_again_by_the_next_import() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();
    let mode = Mode {
        draft: true,
        ..Mode::default()
    };

    let event = import(&mut db, &file, None, &mode);
    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("added"), Some(true))
    );

    let id = event["docId"].as_i64().expect("id");
    assert_eq!(row(&mut db, id).status, Status::Draft);
    assert_eq!(history(&mut db, id), [EventKind::Drafted]);
    let key = row(&mut db, id).source_key;
    assert!(
        db.find_by_source_key(&key, Visibility::Live).expect("find").is_none(),
        "agents do not find it"
    );
    assert!(live_ids(&mut db).is_empty());

    // The same file again is the same draft, and a changed one edits it in place.
    assert_eq!(import(&mut db, &file, None, &mode)["status"], "unchanged");

    write(dir.path(), "guide.md", &guide("A second version."));
    let event = import(&mut db, &file, None, &mode);
    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("updated"), Some(true))
    );
    assert_eq!(event["docId"].as_i64(), Some(id));
    assert_eq!(history(&mut db, id), [EventKind::Edited, EventKind::Drafted]);
    assert_eq!(all_documents(&mut db).len(), 1);
}

#[test]
fn with_team_approval_on_every_new_document_is_a_draft() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("Text."));
    let mut db = store();

    let event = import(&mut db, &file, None, &approval());

    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("added"), Some(true))
    );
    assert_eq!(row(&mut db, event["docId"].as_i64().expect("id")).status, Status::Draft);
    assert!(live_ids(&mut db).is_empty());
}

#[test]
fn a_live_document_replaced_under_approval_gets_one_pending_version_and_keeps_its_text() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");
    let live_text = row(&mut db, live).content_sha256;

    write(dir.path(), "guide.md", &guide("A second version."));
    let event = import(&mut db, &file, Some(live), &approval());

    assert_eq!(event["status"], "pending");
    assert_eq!(event["draft"], true);

    let pending = event["docId"].as_i64().expect("the pending row");
    assert_ne!(pending, live);
    assert_eq!(row(&mut db, pending).pending_of, Some(live));
    assert_eq!(row(&mut db, pending).status, Status::Draft);
    assert_eq!(
        row(&mut db, live).content_sha256,
        live_text,
        "the live text is untouched"
    );
    assert_eq!(history(&mut db, pending), [EventKind::UpdateDrafted]);
    assert_eq!(
        all_documents(&mut db).len(),
        1,
        "a pending version is no row of its own"
    );
    assert_eq!(
        db.file(live).expect("file").map(|file| file.bytes),
        Some(guide("The first version.").into_bytes())
    );
    assert_eq!(
        db.file(pending).expect("file").map(|file| file.bytes),
        Some(guide("A second version.").into_bytes()),
        "a pending row has its own file"
    );

    // Replacing again updates that version, there is never a second one.
    write(dir.path(), "guide.md", &guide("A third version."));
    let event = import(&mut db, &file, Some(live), &approval());
    assert_eq!(
        (event["status"].as_str(), event["docId"].as_i64()),
        (Some("pending"), Some(pending))
    );
    assert_eq!(history(&mut db, pending), [EventKind::Edited, EventKind::UpdateDrafted]);
    assert_eq!(db.stats().expect("stats").pending, 1);

    // The same text again is unchanged and names the pending row.
    let event = import(&mut db, &file, Some(live), &approval());
    assert_eq!(
        (event["status"].as_str(), event["docId"].as_i64()),
        (Some("unchanged"), Some(pending))
    );

    // The pending version found by the source key too, without naming the document.
    let event = import(&mut db, &file, None, &approval());
    assert_eq!(
        (event["status"].as_str(), event["docId"].as_i64()),
        (Some("unchanged"), Some(pending))
    );
}

#[test]
fn a_file_with_the_live_text_is_unchanged_even_when_a_pending_version_is_asked_for() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("Same text."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");
    let event = import(&mut db, &file, Some(live), &approval());

    assert_eq!(
        (event["status"].as_str(), event["docId"].as_i64()),
        (Some("unchanged"), Some(live))
    );
    assert_eq!(db.stats().expect("stats").pending, 0);
}

#[test]
fn the_draft_flag_without_approval_writes_a_pending_version_and_without_it_a_replace_is_direct() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    write(dir.path(), "guide.md", &guide("A second version."));
    let direct = import(&mut db, &file, Some(live), &Mode::default());
    assert_eq!(
        (direct["status"].as_str(), direct["draft"].as_bool()),
        (Some("updated"), Some(false))
    );
    assert_eq!(row(&mut db, live).status, Status::Live);

    write(dir.path(), "guide.md", &guide("A third version."));
    let drafted = import(
        &mut db,
        &file,
        Some(live),
        &Mode {
            draft: true,
            ..Mode::default()
        },
    );
    assert_eq!(drafted["status"], "pending");
    assert_eq!(
        db.pending_of(live).expect("pending of").map(|version| version.id),
        drafted["docId"].as_i64()
    );
}

#[test]
fn replacing_a_row_that_is_not_live_keeps_it_a_draft_whatever_the_settings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();
    let draft = Mode {
        draft: true,
        name: Some("Ada"),
        ..Mode::default()
    };

    let id = import(&mut db, &file, None, &draft)["docId"].as_i64().expect("id");
    let revision = row(&mut db, id).review_rev;
    db.set_review(
        id,
        revision,
        &crate::store::ReviewChange::new(crate::store::ReviewAction::Ask, Some("Ada"), 5),
    )
    .expect("ask");
    assert_eq!(row(&mut db, id).status, Status::Waiting);

    // With approval off and no draft flag the replaced draft still is not live, and what was
    // asked is a draft again.
    write(dir.path(), "guide.md", &guide("A second version."));
    let event = import(&mut db, &file, Some(id), &Mode::default());

    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("updated"), Some(true))
    );
    assert_eq!(row(&mut db, id).status, Status::Draft);
    assert_eq!(
        history(&mut db, id),
        [EventKind::Edited, EventKind::Asked, EventKind::Drafted]
    );

    // A row with requested changes stays as it is: the author asks again themselves.
    let revision = row(&mut db, id).review_rev;
    let by = |action| crate::store::ReviewChange::new(action, Some("Ada"), 6);
    db.set_review(id, revision, &by(crate::store::ReviewAction::Ask))
        .expect("ask");
    let revision = row(&mut db, id).review_rev;
    db.set_review(id, revision, &by(crate::store::ReviewAction::RequestChanges))
        .expect("request");

    write(dir.path(), "guide.md", &guide("A third version."));
    import(&mut db, &file, Some(id), &Mode::default());
    assert_eq!(row(&mut db, id).status, Status::ChangesRequested);
}

#[test]
fn replacing_and_asking_is_one_step_with_a_note_in_the_history() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    write(dir.path(), "guide.md", &guide("A second version."));
    let asking = Mode {
        ask: true,
        note: Some("Rewrote the middle"),
        ..approval()
    };
    let event = import(&mut db, &file, Some(live), &asking);

    assert_eq!(
        (event["status"].as_str(), event["asked"].as_bool()),
        (Some("pending"), Some(true))
    );

    let pending = event["docId"].as_i64().expect("id");
    assert_eq!(row(&mut db, pending).status, Status::Waiting);

    let events = db.history(pending).expect("history").expect("history").events;
    let asked = events
        .iter()
        .find(|event| event.kind == EventKind::Asked)
        .expect("asked");
    assert_eq!(asked.by.as_deref(), Some("Ada"));
    assert_eq!(asked.comment.as_deref(), Some("Rewrote the middle"));

    // A waiting version that is replaced is a draft again, and asking again is the same one step.
    write(dir.path(), "guide.md", &guide("A third version."));
    let event = import(&mut db, &file, Some(live), &asking);
    assert_eq!(
        (event["docId"].as_i64(), event["asked"].as_bool()),
        (Some(pending), Some(true))
    );
    assert_eq!(row(&mut db, pending).status, Status::Waiting);
    assert_eq!(
        history(&mut db, pending),
        [
            EventKind::Asked,
            EventKind::Edited,
            EventKind::Asked,
            EventKind::UpdateDrafted
        ]
    );
}

#[test]
fn asking_without_approval_or_without_a_name_stores_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let off = import(
        &mut db,
        &file,
        None,
        &Mode {
            ask: true,
            name: Some("Ada"),
            ..Mode::default()
        },
    );
    assert_eq!(off["status"], "failed");
    assert!(
        off["reason"].as_str().is_some_and(|reason| reason.contains("approval")),
        "{off}"
    );

    let nameless = import(
        &mut db,
        &file,
        None,
        &Mode {
            ask: true,
            name: None,
            ..approval()
        },
    );
    assert_eq!(nameless["status"], "failed");
    assert!(
        nameless["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("user.name")),
        "{nameless}"
    );

    assert!(all_documents(&mut db).is_empty(), "nothing was written");
}

#[test]
fn the_links_of_an_import_go_to_the_live_document_and_a_pending_version_has_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    write(dir.path(), "guide.md", &guide("A second version."));

    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(&mut db, &ApproxCounter, &InProcess, &never);
    importer.links = vec![link("git:h/o/r", &["src/**"])];
    importer.replaces = Some(live);
    importer.approval = true;
    run(&mut importer, &manifest(&[&file], &[]), &mut Events::new(&mut output)).expect("runs");

    let detail = |store: &mut SqliteStore, id| {
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
    };
    let pending = db.pending_of(live).expect("pending of").expect("a pending version").id;

    assert_eq!(detail(&mut db, live).links, vec![link("git:h/o/r", &["src/**"])]);
    assert!(detail(&mut db, pending).links.is_empty());
}

#[test]
fn the_done_event_counts_pending_versions_only_when_there_are_some() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let events = run_import(&mut db, &manifest(&[&file], &[]), None, &never);
    assert_eq!(done(&events)["counts"].get("pending"), None);

    write(dir.path(), "guide.md", &guide("A second version."));

    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(&mut db, &ApproxCounter, &InProcess, &never);
    importer.approval = true;
    run(&mut importer, &manifest(&[&file], &[]), &mut Events::new(&mut output)).expect("runs");

    let last: Value =
        serde_json::from_str(String::from_utf8(output).expect("utf-8").lines().last().expect("line")).expect("json");
    assert_eq!(last["counts"]["pending"], 1);
    assert_eq!(last["counts"]["updated"], 0);
}

#[test]
fn an_approved_proposal_that_replaces_a_live_document_becomes_its_pending_version_under_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();
    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    // What `held approve` sets: an agent's text, untrusted keys, replacing a named live document.
    write(dir.path(), "proposal.md", &guide("The agent's version."));
    let proposal = dir.path().join("proposal.md");
    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(&mut db, &ApproxCounter, &InProcess, &never);
    importer.replaces = Some(live);
    importer.origin = crate::store::Origin::AgentApproved;
    importer.trust_extracted_key = false;
    importer.approval = true;
    run(
        &mut importer,
        &manifest(&[&proposal], &[]),
        &mut Events::new(&mut output),
    )
    .expect("runs");

    let event: Value =
        serde_json::from_str(String::from_utf8(output).expect("utf-8").lines().nth(1).expect("event")).expect("json");
    assert_eq!(
        (event["status"].as_str(), event["draft"].as_bool()),
        (Some("pending"), Some(true))
    );
    assert_eq!(row(&mut db, live).status, Status::Live);
    assert_eq!(
        row(&mut db, event["docId"].as_i64().expect("id")).origin,
        crate::store::Origin::AgentApproved
    );
}

#[test]
fn a_reindex_by_id_reads_drafts_and_pending_versions_and_leaves_their_state() {
    use crate::import::reindex::{self, Which};

    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();
    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    write(dir.path(), "guide.md", &guide("A second version."));
    let pending = import(&mut db, &file, Some(live), &approval())["docId"]
        .as_i64()
        .expect("id");

    let draft_file = write(dir.path(), "other.md", &guide("Another document."));
    let draft = import(&mut db, &draft_file, None, &approval())["docId"]
        .as_i64()
        .expect("id");

    let mut embedder = Fake::new();
    let done = reindex::run(&mut db, &mut embedder, &Which::Ids(vec![pending, draft]), &|| false).expect("reindex");

    assert!(done.failed.is_empty(), "{:?}", done.failed);
    assert_eq!(done.documents, 2);
    assert_eq!(
        (row(&mut db, pending).status, row(&mut db, draft).status),
        (Status::Draft, Status::Draft)
    );
    assert_eq!(row(&mut db, live).status, Status::Live);

    let missing = reindex::run(&mut db, &mut embedder, &Which::Ids(vec![9999]), &|| false).expect("reindex");
    assert_eq!(missing.failed.len(), 1);
}

#[test]
fn asking_for_a_file_whose_text_a_draft_has_already_asks_that_draft() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let drafted = import(&mut db, &file, None, &approval());
    let draft = drafted["docId"].as_i64().expect("id");
    assert_eq!(row(&mut db, draft).status, Status::Draft);

    let asking = Mode {
        ask: true,
        note: Some("Please review"),
        ..approval()
    };
    let event = import(&mut db, &file, None, &asking);

    assert_eq!(
        (
            event["status"].as_str(),
            event["docId"].as_i64(),
            event["asked"].as_bool()
        ),
        (Some("unchanged"), Some(draft), Some(true))
    );
    assert_eq!(row(&mut db, draft).status, Status::Waiting);

    let events = db.history(draft).expect("history").expect("history").events;
    let asked = events
        .iter()
        .find(|event| event.kind == EventKind::Asked)
        .expect("asked");
    assert_eq!(
        (asked.by.as_deref(), asked.comment.as_deref()),
        (Some("Ada"), Some("Please review"))
    );

    // Asked already: there is nothing more to ask.
    let again = import(&mut db, &file, None, &asking);
    assert_eq!(again["status"], "unchanged");
    assert_ne!(again["asked"].as_bool(), Some(true), "{again}");
    assert_eq!(
        history(&mut db, draft)
            .iter()
            .filter(|kind| **kind == EventKind::Asked)
            .count(),
        1
    );

    // With approval off the ask is refused as before, and nothing changes.
    let off = import(
        &mut db,
        &file,
        None,
        &Mode {
            ask: true,
            name: Some("Ada"),
            ..Mode::default()
        },
    );
    assert_eq!(off["status"], "failed");
}

#[test]
fn asking_for_a_file_whose_text_a_pending_version_has_already_asks_the_pending_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    let live = import(&mut db, &file, None, &Mode::default())["docId"]
        .as_i64()
        .expect("id");

    write(dir.path(), "guide.md", &guide("A second version."));
    let pending = import(&mut db, &file, Some(live), &approval())["docId"]
        .as_i64()
        .expect("pending id");
    assert_eq!(row(&mut db, pending).status, Status::Draft);

    let asking = Mode {
        ask: true,
        ..approval()
    };
    let event = import(&mut db, &file, Some(live), &asking);

    assert_eq!(
        (
            event["status"].as_str(),
            event["docId"].as_i64(),
            event["asked"].as_bool()
        ),
        (Some("unchanged"), Some(pending), Some(true))
    );
    assert_eq!(row(&mut db, pending).status, Status::Waiting);

    // A live document that the file matches has nothing to ask.
    write(dir.path(), "guide.md", &guide("The first version."));
    let live_again = import(&mut db, &file, Some(live), &asking);
    assert_eq!(live_again["status"], "unchanged");
    assert_ne!(live_again["asked"].as_bool(), Some(true), "{live_again}");
}

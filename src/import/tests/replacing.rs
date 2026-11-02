//! Authors on imported documents, and a manifest whose files each replace a document of their own.

use pretty_assertions::assert_eq;

use super::*;
use crate::store::testing::{Seed, insert};
use crate::store::{Author, Document};

fn author(name: &str, via: Option<&str>) -> Option<Author> {
    Some(Author {
        name: name.to_owned(),
        via: via.map(str::to_owned),
    })
}

/// Imports `manifest` as `by`, the way `docs import` does for one place.
fn run_as(store: &mut SqliteStore, manifest: &Manifest, by: Option<Author>) -> Vec<Value> {
    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(store, &ApproxCounter, &InProcess, &never);
    importer.links = manifest.links.clone();
    importer.author = by;

    run(&mut importer, manifest, &mut Events::new(&mut output)).expect("the import runs");

    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect()
}

fn stored(store: &mut SqliteStore, id: i64) -> Document {
    store
        .get(id, &Detail::default())
        .expect("get")
        .expect("the document exists")
        .doc
}

#[test]
fn a_new_document_is_added_by_the_author_and_an_update_keeps_who_added_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("The first version."));
    let mut db = store();

    run_as(&mut db, &manifest(&[&file], &[]), author("Ada", Some("claude-code")));
    let first = stored(&mut db, 1);
    assert_eq!(first.added_by, author("Ada", Some("claude-code")));
    assert_eq!(first.updated_by, author("Ada", Some("claude-code")));

    // The same text again changes nothing, whoever imports it.
    let events = run_as(&mut db, &manifest(&[&file], &[]), author("Bob", None));
    assert_eq!(file_event(&events, "guide.md")["status"], "unchanged");
    assert_eq!(stored(&mut db, 1), first);

    // A new version of it is Bob's update of Ada's document.
    write(dir.path(), "guide.md", &guide("A second version."));
    let events = run_as(&mut db, &manifest(&[&file], &[]), author("Bob", None));
    assert_eq!(file_event(&events, "guide.md")["status"], "updated");

    let second = stored(&mut db, 1);
    assert_eq!(second.added_by, author("Ada", Some("claude-code")));
    assert_eq!(second.updated_by, author("Bob", None));
}

#[test]
fn without_an_author_nobody_is_recorded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = write(dir.path(), "guide.md", &guide("Text."));
    let mut db = store();

    run_as(&mut db, &manifest(&[&file], &[]), None);

    let doc = stored(&mut db, 1);
    assert_eq!((doc.added_by, doc.updated_by), (None, None));
}

#[test]
fn each_file_of_a_batch_replaces_the_document_the_manifest_names_for_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut db = store();

    let old_a = insert(
        &mut db,
        &Seed::new("Old A")
            .source("docs/old-a.md")
            .chunk("Intro", "alpha")
            .link("git:h/o/a", &["src/**"])
            .by("Grace", None),
    );
    let old_b = insert(
        &mut db,
        &Seed::new("Old B")
            .source("docs/old-b.md")
            .chunk("Intro", "beta")
            .link("git:h/o/b", &[])
            .by("Grace", Some("codex")),
    );
    let before_a = stored(&mut db, old_a);

    let new_a = write(dir.path(), "first.md", &guide("Replaces A."));
    let new_b = write(
        &dir.path().join("deeper"),
        "second.md",
        "# Second\n\nReplaces B with this text.",
    );
    let fresh = write(dir.path(), "fresh.md", "# Fresh\n\nNot replacing anything.");

    let mut batch = manifest(&[&new_b, &fresh, &new_a], &[]);
    batch.replacing.insert(new_a.to_string_lossy().into_owned(), old_a);
    batch.replacing.insert(new_b.to_string_lossy().into_owned(), old_b);

    let events = run_as(&mut db, &batch, author("Ada", None));

    assert_eq!(
        statuses(&events),
        pairs(&[("second.md", "updated"), ("fresh.md", "added"), ("first.md", "updated")])
    );
    assert_eq!(file_event(&events, "first.md")["docId"], old_a);
    assert_eq!(file_event(&events, "second.md")["docId"], old_b);

    let rows = all_documents(&mut db);
    assert_eq!(rows.len(), 3, "two replaced, one new");

    let a = stored(&mut db, old_a);
    assert_eq!(a.title, "Guide");
    assert_eq!(a.created_at, before_a.created_at, "a replacement is the same document");
    assert_eq!(a.added_by, author("Grace", None));
    assert_eq!(a.updated_by, author("Ada", None));

    let b = stored(&mut db, old_b);
    assert_eq!(b.title, "Second");
    assert_eq!(b.added_by, author("Grace", Some("codex")));
    assert_eq!(b.updated_by, author("Ada", None));

    let links_of = |id: i64, db: &mut SqliteStore| db.get(id, &Detail::default()).expect("get").expect("doc").links;
    assert_eq!(links_of(old_a, &mut db), [link("git:h/o/a", &["src/**"])]);
    assert_eq!(links_of(old_b, &mut db), [link("git:h/o/b", &[])]);

    let new_id = file_event(&events, "fresh.md")["docId"].as_i64().expect("id");
    assert!(![old_a, old_b].contains(&new_id));

    let added = stored(&mut db, new_id);
    assert_eq!(
        (added.added_by, added.updated_by),
        (author("Ada", None), author("Ada", None))
    );
}

#[test]
fn a_file_replacing_a_missing_document_fails_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut db = store();
    let kept = insert(
        &mut db,
        &Seed::new("Kept").source("docs/kept.md").chunk("Intro", "kept text"),
    );
    let gone = write(
        dir.path(),
        "gone.md",
        "# Gone\n\nReplaces a document that is not there.",
    );
    let ok = write(dir.path(), "ok.md", "# Ok\n\nReplaces the kept one.");

    let mut batch = manifest(&[&gone, &ok], &[]);
    batch.replacing.insert(gone.to_string_lossy().into_owned(), 99);
    batch.replacing.insert(ok.to_string_lossy().into_owned(), kept);

    let events = run_as(&mut db, &batch, None);

    assert_eq!(file_event(&events, "gone.md")["status"], "failed");
    assert_eq!(file_event(&events, "gone.md")["reason"], "document 99 not found");
    assert_eq!(file_event(&events, "ok.md")["status"], "updated");
    assert_eq!(file_event(&events, "ok.md")["docId"], kept);
}

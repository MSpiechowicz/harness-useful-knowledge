//! Source keys: where a document of the same name from elsewhere goes, and what a changed title does.

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn same_named_folders_of_different_repositories_keep_their_own_documents() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = write(&dir.path().join("a").join("docs"), "guide.md", &guide("Middle of a."));
    let b = write(&dir.path().join("b").join("docs"), "guide.md", &guide("Middle of b."));
    let mut db = store();

    let mut first = manifest(&[], &[a.parent().expect("folder")]);
    first.links = vec![link("git:github.com/o/a", &[])];
    let mut second = manifest(&[], &[b.parent().expect("folder")]);
    second.links = vec![link("git:github.com/o/b", &[])];

    run_import(&mut db, &first, None, &never);
    let events = run_import(&mut db, &second, None, &never);

    assert_eq!(file_event(&events, "guide.md")["status"], "added");
    assert_eq!(all_documents(&mut db).len(), 2);
    assert!(
        db.find_by_source_key("git:github.com/o/a/docs/guide.md")
            .expect("find")
            .is_some()
    );
}

#[test]
fn a_page_replaced_from_an_unrelated_repository_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let page = include_str!("../../extract/fixtures/confluence/Deploy-the-gateway_100003.html");
    let first = write(&dir.path().join("one"), "Deploy-the-gateway_100003.html", page);
    let again = write(
        &dir.path().join("two"),
        "Deploy-the-gateway_100003.html",
        &page.replace("gwctl rollback", "gwctl undo"),
    );
    let mut db = store();

    let mut linked_a = manifest(&[&first], &[]);
    linked_a.links = vec![link("git:github.com/o/a", &[])];
    run_import(&mut db, &linked_a, None, &never);

    // The same repository again is a newer version, with nothing to say.
    let events = run_import(&mut db, &linked_a_with(&again, "git:github.com/o/a"), None, &never);
    assert_eq!(file_event(&events, "100003.html")["status"], "updated");
    assert!(file_event(&events, "100003.html").get("reason").is_none());

    let newest = write(
        &dir.path().join("three"),
        "Deploy-the-gateway_100003.html",
        &page.replace("gwctl rollback", "gwctl revert"),
    );
    let events = run_import(&mut db, &linked_a_with(&newest, "git:github.com/o/b"), None, &never);
    let event = file_event(&events, "100003.html");

    assert_eq!(event["status"], "updated");
    assert_eq!(event["reason"], "replaced a document with the same name");
}

fn linked_a_with(path: &Path, repo: &str) -> Manifest {
    let mut manifest = manifest(&[path], &[]);
    manifest.links = vec![link(repo, &[])];

    manifest
}

#[test]
fn a_new_title_updates_the_document_and_embeds_it_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "guide.md", &guide("Middle."));
    let mut db = store();
    let mut embedder = Fake::new();
    let titled = |db: &mut SqliteStore, embedder: &mut Fake, title: &str| {
        let mut output: Vec<u8> = Vec::new();
        let mut importer = Importer::new(db, &ApproxCounter, &InProcess, &never);
        importer.embedder = Some(embedder as &mut dyn Embedder);
        importer.title = Some(title.to_owned());
        run(&mut importer, &manifest(&[&path], &[]), &mut Events::new(&mut output)).expect("import");

        String::from_utf8(output).expect("utf-8")
    };

    titled(&mut db, &mut embedder, "Old title");
    let embedded = embedder.passages.len();
    let output = titled(&mut db, &mut embedder, "Old title");
    assert!(output.contains("\"status\":\"unchanged\""), "{output}");
    assert_eq!(embedder.passages.len(), embedded);

    let output = titled(&mut db, &mut embedder, "New title");

    assert!(output.contains("\"status\":\"updated\""), "{output}");
    assert!(embedder.passages.len() > embedded, "the passages carry the title");
    assert_eq!(all_documents(&mut db).len(), 1);
    assert_eq!(all_documents(&mut db)[0].doc.title, "New title");
}

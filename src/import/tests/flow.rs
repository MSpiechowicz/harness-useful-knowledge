use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pretty_assertions::assert_eq;

use super::*;
use crate::store::{Detail, KnowledgeStore};

#[test]
fn a_cancel_between_documents_leaves_only_whole_documents() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files: Vec<PathBuf> = (0..5)
        .map(|n| {
            write(
                dir.path(),
                &format!("doc{n}.md"),
                &guide(&format!("Middle number {n}.")),
            )
        })
        .collect();
    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let mut db = store();

    // The cancel arrives after two documents were checked.
    let asked = Cell::new(0);
    let cancel = || {
        asked.set(asked.get() + 1);
        asked.get() > 2
    };
    let events = run_import(&mut db, &manifest(&refs, &[]), None, &cancel);

    assert_eq!(statuses(&events).len(), 2);
    assert_eq!(done(&events)["cancelled"], true);
    assert_eq!(done(&events)["counts"]["added"], 2);

    let documents = all_documents(&mut db);
    assert_eq!(documents.len(), 2);

    for row in documents {
        let outline = Detail {
            sections: true,
            chunk: None,
            drafts: false,
        };
        let sections = db
            .get(row.doc.id, &outline)
            .expect("get")
            .expect("stored")
            .sections
            .expect("outline");

        assert_eq!(
            sections.len(),
            row.doc.chunk_count,
            "every chunk of {} is there",
            row.doc.title
        );
        assert_eq!(sections.len(), 3);
    }
}

#[test]
fn a_cancel_while_embedding_a_document_writes_none_of_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let small = write(dir.path(), "a-small.md", &guide("Middle."));

    // Twenty sections that cannot merge: two embedding batches.
    let sections: Vec<String> = (0..20)
        .map(|n| {
            let text = (0..8)
                .map(|m| format!("Section {n} says thing {m} about how the gateway routes requests."))
                .collect::<Vec<_>>()
                .join(" ");

            format!("## Part {n}\n\n{text}")
        })
        .collect();
    let big = write(dir.path(), "b-big.md", &format!("# Big\n\n{}", sections.join("\n\n")));

    // The first batch of the big document is embedded, and then the cancel arrives.
    let stop = Rc::new(Cell::new(false));
    let calls = Rc::new(Cell::new(0));
    let (stop_in_embedder, calls_in_embedder) = (Rc::clone(&stop), Rc::clone(&calls));
    let mut embedder = Fake::new();
    embedder.on_call = Some(Box::new(move || {
        calls_in_embedder.set(calls_in_embedder.get() + 1);

        if calls_in_embedder.get() == 2 {
            stop_in_embedder.set(true);
        }
    }));

    let mut db = store();
    let stopped = Rc::clone(&stop);
    let events = run_import(
        &mut db,
        &manifest(&[&small, &big], &[]),
        Some(&mut embedder),
        &move || stopped.get(),
    );

    assert_eq!(done(&events)["cancelled"], true);
    assert_eq!(statuses(&events), pairs(&[("a-small.md", "added")]));
    assert_eq!(
        all_documents(&mut db).len(),
        1,
        "the half-embedded document was not written"
    );
    assert_eq!(db.stats().expect("stats").chunks, 3);
}

#[test]
fn replacing_a_document_keeps_its_id_and_links() {
    let dir = tempfile::tempdir().expect("temp dir");
    let old = write(dir.path(), "old-name.md", &guide("The first version."));
    let mut db = store();

    let mut linked = manifest(&[&old], &[]);
    linked.links = vec![link("git:h/o/a", &[])];
    let events = run_import(&mut db, &linked, None, &never);
    let id = file_event(&events, "old-name.md")["docId"].as_i64().expect("id");

    let new = write(
        &dir.path().join("elsewhere"),
        "new-name.md",
        &guide("A different second version."),
    );
    let mut replacing = manifest(&[&new], &[]);
    replacing.replaces = Some(id);
    let events = run_import(&mut db, &replacing, None, &never);

    assert_eq!(file_event(&events, "new-name.md")["status"], "updated");
    assert_eq!(file_event(&events, "new-name.md")["docId"], id);

    let rows = all_documents(&mut db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].doc.source_name, "new-name.md");
    assert_eq!(rows[0].links, [link("git:h/o/a", &[])]);

    // The same file again changes nothing.
    let events = run_import(&mut db, &replacing, None, &never);
    assert_eq!(file_event(&events, "new-name.md")["status"], "unchanged");

    // A document that is not there, and content that already is another document.
    let third = write(dir.path(), "third.md", &guide("Third."));
    let mut gone = manifest(&[&third], &[]);
    gone.replaces = Some(77);
    let events = run_import(&mut db, &gone, None, &never);
    assert_eq!(file_event(&events, "third.md")["status"], "failed");
    assert_eq!(file_event(&events, "third.md")["reason"], "document 77 not found");

    let other_id = file_event(
        &run_import(&mut db, &manifest(&[&third], &[]), None, &never),
        "third.md",
    )["docId"]
        .as_i64()
        .expect("id");
    let mut clash = manifest(&[&third], &[]);
    clash.replaces = Some(id);
    let events = run_import(&mut db, &clash, None, &never);
    assert_eq!(
        file_event(&events, "third.md")["reason"],
        format!("the same content is already document {other_id}").as_str()
    );
}

#[test]
fn a_replacement_of_several_files_is_refused_before_anything_is_printed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = write(dir.path(), "a.md", "# A\n\ntext");
    let b = write(dir.path(), "b.md", "# B\n\ntext");
    let mut db = store();
    let mut many = manifest(&[&a, &b], &[]);
    many.replaces = Some(1);

    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(&mut db, &ApproxCounter, &InProcess, &never);
    importer.replaces = many.replaces;

    assert_eq!(
        run(&mut importer, &many, &mut Events::new(&mut output)).expect_err("refused"),
        "replaces takes exactly one file"
    );
    assert!(output.is_empty());
}

#[test]
fn limits_skip_or_fail_a_file_and_never_the_import() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let mib: u64 = 1024 * 1024;

    // A sparse 33 MiB text file is over the file limit, a 65 MiB PDF over the PDF limit.
    std::fs::File::create(root.join("a-big.txt"))
        .expect("create")
        .set_len(33 * mib)
        .expect("size");
    std::fs::File::create(root.join("b-big.pdf"))
        .expect("create")
        .set_len(65 * mib)
        .expect("size");

    // 9 MiB of words is within the file limit but over the text limit.
    write(root, "c-text.txt", &"lorem ipsum ".repeat(9 * mib as usize / 12));

    // A title longer than 200 characters (a heading's limit) is cut.
    write(
        root,
        "d-title.md",
        &format!("# {}\n\nbody text of the document", "T".repeat(500)),
    );

    // More than 4000 chunks.
    let paragraphs: Vec<String> = (0..4200)
        .map(|n| format!("Paragraph {n}. {}", "filler ".repeat(150)))
        .collect();
    write(root, "e-chunks.txt", &paragraphs.join("\n\n"));

    let mut db = store();
    let events = run_import(&mut db, &manifest(&[], &[root]), None, &never);
    let reason = |name: &str| {
        file_event(&events, name)["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };

    assert_eq!(file_event(&events, "a-big.txt")["status"], "skipped");
    assert_eq!(reason("a-big.txt"), "larger than 32 MiB");
    assert_eq!(file_event(&events, "b-big.pdf")["status"], "skipped");
    assert_eq!(reason("b-big.pdf"), "larger than 64 MiB");
    assert_eq!(file_event(&events, "c-text.txt")["status"], "failed");
    assert_eq!(reason("c-text.txt"), "the text is over 8 MiB");
    assert_eq!(file_event(&events, "d-title.md")["status"], "added");
    assert_eq!(file_event(&events, "e-chunks.txt")["status"], "failed");
    assert!(reason("e-chunks.txt").contains("4000"), "{}", reason("e-chunks.txt"));

    let id = file_event(&events, "d-title.md")["docId"].as_i64().expect("id");
    let title = db.get(id, &Detail::default()).expect("get").expect("stored").doc.title;
    assert_eq!(title.chars().count(), 200);
    assert_eq!(all_documents(&mut db).len(), 1);
}

#[test]
fn problems_with_manifest_entries_are_reported_per_entry() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let good = write(root, "good.md", &guide("Middle."));
    let folder = root.join("folder");
    std::fs::create_dir(&folder).expect("folder");

    let mut db = store();
    let entries = manifest(
        &[&root.join("missing.md"), &folder, &good],
        &[&root.join("no-such-folder"), &good],
    );
    let events = run_import(&mut db, &entries, None, &never);

    assert_eq!(
        statuses(&events),
        pairs(&[
            ("missing.md", "failed"),
            ("folder", "failed"),
            ("good.md", "added"),
            ("no-such-folder", "failed"),
            ("good.md", "failed"),
        ])
    );
}

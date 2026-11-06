use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use serde_json::Value;

use super::embed::{EmbedError, Embedder};
use super::*;
use crate::extract::pdf::PdfPages;
use crate::extract::testing::pdf;
use crate::extract::worker;
use crate::store::testing::{MODEL, fake_vector, store};
use crate::store::{Detail, DocumentRow, KnowledgeStore, Link, ListQuery, RepoFilter, Sort, SqliteStore};
use crate::tokens::{APPROX_COUNTER, ApproxCounter};

/// Reads PDFs in this process: the worker's own reader, without the child.
struct InProcess;

impl PdfPages for InProcess {
    fn pages(&self, path: &Path) -> Result<Vec<String>, String> {
        worker::extract_pages(&path.to_string_lossy())
    }
}

/// An embedder that records what it was asked, and can fail or run a hook on each call.
struct Fake {
    passages: Vec<String>,
    /// How many passages each call carried.
    calls: Vec<usize>,
    model: String,
    fail: bool,
    on_call: Option<Box<dyn Fn()>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            passages: Vec::new(),
            calls: Vec::new(),
            model: MODEL.to_owned(),
            fail: false,
            on_call: None,
        }
    }
}

impl Embedder for Fake {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if let Some(on_call) = &self.on_call {
            on_call();
        }

        if self.fail {
            return Err(EmbedError("the embedder is down".to_owned()));
        }

        self.passages.extend(passages.iter().cloned());
        self.calls.push(passages.len());

        Ok(passages
            .iter()
            .map(|passage| fake_vector(passage.bytes().map(u64::from).sum()))
            .collect())
    }
}

fn manifest(paths: &[&Path], folders: &[&Path]) -> Manifest {
    Manifest {
        paths: paths.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
        folders: folders.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
        ..Manifest::default()
    }
}

/// Runs an import and returns the events it printed.
fn run_import(
    store: &mut SqliteStore,
    manifest: &Manifest,
    embedder: Option<&mut Fake>,
    cancel: &dyn Fn() -> bool,
) -> Vec<Value> {
    let mut output: Vec<u8> = Vec::new();
    let mut importer = Importer::new(store, &ApproxCounter, &InProcess, cancel);
    importer.embedder = embedder.map(|embedder| embedder as &mut dyn Embedder);
    importer.links = manifest.links.clone();
    importer.replaces = manifest.replaces;

    run(&mut importer, manifest, &mut Events::new(&mut output)).expect("the import runs");

    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect()
}

fn never() -> bool {
    false
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, status)| ((*name).to_owned(), (*status).to_owned()))
        .collect()
}

/// `(file name, status)` of the file events, in order.
fn statuses(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|event| event["type"] == "file")
        .map(|event| {
            let name = Path::new(event["path"].as_str().expect("path"))
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();

            (name, event["status"].as_str().expect("status").to_owned())
        })
        .collect()
}

fn file_event<'a>(events: &'a [Value], name: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == "file" && event["path"].as_str().is_some_and(|path| path.ends_with(name)))
        .unwrap_or_else(|| panic!("no event for {name}: {events:?}"))
}

fn done(events: &[Value]) -> &Value {
    events
        .last()
        .filter(|event| event["type"] == "done")
        .expect("ends with done")
}

/// A Markdown guide of three sections: the first and last are too long to merge with a sibling.
fn guide(middle: &str) -> String {
    let filler = |tag: &str| {
        (0..8)
            .map(|n| format!("The {tag} part explains detail number {n} of how requests flow."))
            .collect::<Vec<_>>()
            .join(" ")
    };

    format!(
        "# Guide\n\n## Intro\n\n{}\n\n## Middle\n\n{middle}\n\n## Limits\n\n{}\n",
        filler("intro"),
        filler("limits")
    )
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);

    std::fs::create_dir_all(dir).expect("folders");
    std::fs::write(&path, text).expect("write");

    path
}

fn all_documents(store: &mut SqliteStore) -> Vec<DocumentRow> {
    store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            author: None,
            want_authors: false,
            sort: Sort::Title,
            descending: false,
            offset: 0,
            limit: 200,
            status: crate::store::StatusFilter::All,
        })
        .expect("list")
        .items
}

fn link(repo: &str, globs: &[&str]) -> Link {
    Link {
        repo_key: repo.to_owned(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
    }
}

#[test]
fn every_format_imports_with_a_status_per_file_and_the_events_frame_them() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();

    write(root, "guide.md", &guide("The middle explains the flow of a request."));
    write(root, "notes.txt", &"word ".repeat(200));
    write(
        root,
        "page.html",
        "<html><head><title>Page</title></head><body><h1>Page</h1><p>Hello there.</p></body></html>",
    );
    write(root, "api.json", include_str!("../../extract/fixtures/petstore.json"));
    write(
        root,
        "legacy.yaml",
        include_str!("../../extract/fixtures/petstore.yaml"),
    );
    write(root, "guide.adoc", include_str!("../../extract/fixtures/guide.adoc"));
    write(root, "guide.rst", include_str!("../../extract/fixtures/guide.rst"));
    write(root, "ci.yaml", include_str!("../../extract/fixtures/not-openapi.yaml"));
    std::fs::write(
        root.join("handbook.pdf"),
        pdf(&[&format!("Handbook\n{}", "pdf words ".repeat(60))]),
    )
    .expect("pdf");
    std::fs::write(root.join("image.bin"), [0u8, 1, 2]).expect("binary");

    let mut db = store();
    let events = run_import(&mut db, &manifest(&[&root.join("image.bin")], &[root]), None, &never);

    assert_eq!(events[0]["type"], "plan");
    assert_eq!(
        events[0]["total"], 10,
        "the chosen file and the nine documents of the folder"
    );

    assert_eq!(
        statuses(&events),
        pairs(&[
            ("image.bin", "skipped"),
            ("api.json", "added"),
            ("ci.yaml", "skipped"),
            ("guide.adoc", "added"),
            ("guide.md", "added"),
            ("guide.rst", "added"),
            ("handbook.pdf", "added"),
            ("legacy.yaml", "added"),
            ("notes.txt", "added"),
            ("page.html", "added"),
        ])
    );
    assert!(
        file_event(&events, "ci.yaml")["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("OpenAPI"))
    );
    assert!(
        file_event(&events, "image.bin")["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("binary"))
    );

    let formats: Vec<String> = all_documents(&mut db).into_iter().map(|row| row.doc.format).collect();
    for format in ["markdown", "text", "html", "openapi", "asciidoc", "rst", "pdf"] {
        assert!(formats.iter().any(|stored| stored == format), "{format} in {formats:?}");
    }

    // One progress event per file, then the summary.
    let progress: Vec<&Value> = events.iter().filter(|event| event["type"] == "progress").collect();
    assert_eq!(progress.len(), 10);
    assert_eq!(progress[9]["done"], 10);
    assert_eq!(progress[9]["total"], 10);

    let summary = done(&events);
    assert_eq!(summary["counts"]["added"], 8);
    assert_eq!(summary["counts"]["skipped"], 2);
    assert_eq!(summary["cancelled"], false);
    assert_eq!(summary["counter"], APPROX_COUNTER);
    assert!(summary["embeddingPending"].as_u64().expect("pending") > 0);
    assert_eq!(file_event(&events, "guide.md")["embedding"], "pending");
}

#[test]
fn a_scanned_and_a_malformed_pdf_fail_and_the_other_files_go_on() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();

    std::fs::write(root.join("a-scanned.pdf"), pdf(&["", "", ""])).expect("scanned");
    std::fs::write(root.join("b-malformed.pdf"), b"%PDF-1.4\nnot a pdf at all").expect("malformed");
    write(root, "c-good.md", &guide("Middle."));

    let mut db = store();
    let events = run_import(&mut db, &manifest(&[], &[root]), None, &never);

    assert_eq!(
        statuses(&events),
        pairs(&[
            ("a-scanned.pdf", "failed"),
            ("b-malformed.pdf", "failed"),
            ("c-good.md", "added")
        ])
    );
    assert_eq!(
        file_event(&events, "a-scanned.pdf")["reason"],
        "the PDF has no text layer (scanned)"
    );
    assert!(
        file_event(&events, "b-malformed.pdf")["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("PDF"))
    );
    assert_eq!(done(&events)["counts"]["failed"], 2);
}

#[test]
fn the_dedupe_matrix_unchanged_updated_with_reuse_and_added() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let path = write(root, "guide.md", &guide("The middle explains the flow."));
    let source_key = format!("global/{}/guide.md", root.file_name().expect("name").to_string_lossy());
    let mut db = store();
    let mut embedder = Fake::new();

    // Added: three sections, three chunks, three embeddings.
    let events = run_import(&mut db, &manifest(&[&path], &[]), Some(&mut embedder), &never);
    let first = file_event(&events, "guide.md");

    assert_eq!(first["status"], "added");
    assert_eq!(first["chunks"], 3);
    assert_eq!(first["embedding"], "done");
    assert_eq!(embedder.passages.len(), 3);
    assert_eq!(done(&events)["embeddingPending"], 0);

    let id = first["docId"].as_i64().expect("doc id");
    let created = db
        .find_by_source_key(&source_key, crate::store::Visibility::Live)
        .expect("find")
        .expect("stored")
        .created_at;

    // Unchanged: the same content embeds nothing.
    let events = run_import(&mut db, &manifest(&[&path], &[]), Some(&mut embedder), &never);
    assert_eq!(file_event(&events, "guide.md")["status"], "unchanged");
    assert_eq!(file_event(&events, "guide.md")["docId"], id);
    assert_eq!(embedder.passages.len(), 3, "no re-embedding");

    // The same text under another path and name is the same content.
    let copy = write(
        &root.join("copy"),
        "other-name.md",
        &guide("The middle explains the flow."),
    );
    let events = run_import(&mut db, &manifest(&[&copy], &[]), Some(&mut embedder), &never);
    assert_eq!(file_event(&events, "other-name.md")["status"], "unchanged");
    assert_eq!(file_event(&events, "other-name.md")["docId"], id);
    assert_eq!(all_documents(&mut db).len(), 1);

    // Updated: the middle section changed. The document is the same and only its chunk is embedded.
    write(
        root,
        "guide.md",
        &guide("The middle was rewritten and now says something new."),
    );
    let events = run_import(&mut db, &manifest(&[&path], &[]), Some(&mut embedder), &never);
    let updated = file_event(&events, "guide.md");

    assert_eq!(updated["status"], "updated");
    assert_eq!(updated["docId"], id);
    assert_eq!(updated["chunks"], 3);
    assert_eq!(embedder.passages.len(), 4, "one new passage, two vectors reused");
    assert!(embedder.passages[3].contains("rewritten"));

    let stored = db.get(id, &Detail::default()).expect("get").expect("stored").doc;
    assert_eq!(stored.created_at, created, "the creation time is kept");
    assert_eq!(db.stats().expect("stats").embedded_chunks, 3);
    assert_eq!(all_documents(&mut db).len(), 1);

    // Added: different content at a different path.
    let other = write(root, "other.md", &guide("An unrelated middle."));
    let events = run_import(&mut db, &manifest(&[&other], &[]), Some(&mut embedder), &never);
    assert_eq!(file_event(&events, "other.md")["status"], "added");
    assert_ne!(file_event(&events, "other.md")["docId"], id);
    assert_eq!(all_documents(&mut db).len(), 2);
}

#[test]
fn an_unchanged_document_takes_the_links_of_the_new_import() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "guide.md", &guide("Middle."));
    let mut db = store();

    let mut first = manifest(&[&path], &[]);
    first.links = vec![link("git:h/o/a", &["src/**"])];
    run_import(&mut db, &first, None, &never);

    let mut second = manifest(&[&path], &[]);
    second.links = vec![link("git:h/o/a", &["docs/**"]), link("git:h/o/b", &[])];
    let events = run_import(&mut db, &second, None, &never);
    assert_eq!(file_event(&events, "guide.md")["status"], "unchanged");

    let row = all_documents(&mut db).remove(0);
    assert_eq!(
        row.links,
        [link("git:h/o/a", &["src/**", "docs/**"]), link("git:h/o/b", &[])]
    );
}

#[test]
fn a_confluence_page_is_updated_by_its_page_id_wherever_it_is_imported_from() {
    let dir = tempfile::tempdir().expect("temp dir");
    let page = include_str!("../../extract/fixtures/confluence/Deploy-the-gateway_100003.html");
    let first = write(&dir.path().join("export-1"), "Deploy-the-gateway_100003.html", page);
    let mut db = store();

    let events = run_import(&mut db, &manifest(&[&first], &[]), None, &never);
    let id = file_event(&events, "100003.html")["docId"].as_i64().expect("id");

    let again = write(
        &dir.path().join("export-2"),
        "Deploy-the-gateway_100003.html",
        &page.replace("gwctl rollback", "gwctl undo"),
    );
    let events = run_import(&mut db, &manifest(&[&again], &[]), None, &never);

    assert_eq!(file_event(&events, "100003.html")["status"], "updated");
    assert_eq!(file_event(&events, "100003.html")["docId"], id);

    let stored = db.get(id, &Detail::default()).expect("get").expect("stored").doc;
    assert_eq!(stored.source_key, "confluence:100003");
    assert_eq!(stored.title, "Deploy the gateway");
    assert_eq!(all_documents(&mut db).len(), 1);
}

#[test]
fn a_failing_embedder_leaves_the_chunks_pending_and_is_not_asked_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = write(dir.path(), "a.md", &guide("Middle of a."));
    let b = write(dir.path(), "b.md", &guide("Middle of b."));
    let mut db = store();
    let mut embedder = Fake::new();
    embedder.fail = true;

    let events = run_import(&mut db, &manifest(&[&a, &b], &[]), Some(&mut embedder), &never);

    assert_eq!(statuses(&events), pairs(&[("a.md", "added"), ("b.md", "added")]));
    assert_eq!(file_event(&events, "a.md")["embedding"], "pending");
    assert_eq!(done(&events)["embeddingPending"], 6);
    assert_eq!(db.stats().expect("stats").chunks, 6);
}

#[test]
fn a_subtitle_file_in_a_folder_is_imported_as_a_transcript_with_a_section_per_stretch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("talks");
    write(&root, "review.vtt", include_str!("../../extract/fixtures/talk.vtt"));
    write(&root, "call.srt", include_str!("../../extract/fixtures/talk.srt"));
    let mut db = store();

    let events = run_import(&mut db, &manifest(&[], &[&root]), None, &never);

    assert_eq!(
        statuses(&events),
        pairs(&[("call.srt", "added"), ("review.vtt", "added")])
    );

    let rows = all_documents(&mut db);

    assert!(rows.iter().all(|row| row.doc.format == "transcript"), "{rows:?}");
    assert_eq!(
        db.get(rows[0].doc.id, &Detail::default())
            .expect("get")
            .expect("stored")
            .doc
            .title,
        "call"
    );
}

mod drafts;
mod flow;
mod keys;
mod reindex;
mod replacing;

//! `docs similar`: which stored document a file is a version of, and why.

use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::import::import_with;
use super::*;
use crate::extract::pdf::PdfPages;
use crate::import;
use crate::limits::FILES_PER_IMPORT_MAX;
use crate::location::{DIR_VAR, FILE_NAME};

struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("no PDF reader in this test".to_owned())
    }
}

/// A Markdown guide of three sections, the first and last too long to merge with the middle one.
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

struct Lab {
    root: tempfile::TempDir,
    env: Env,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("knowledge").to_string_lossy());

        Self { root, env }
    }

    fn context(&self) -> Context<'_> {
        Context::new(&self.env, &Backend::Sqlite, Routing::local())
    }

    fn database_exists(&self) -> bool {
        self.root.path().join("knowledge").join(FILE_NAME).exists()
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.root.path().join("files").join(relative);

        std::fs::create_dir_all(path.parent().expect("parent")).expect("folders");
        std::fs::write(&path, text).expect("write");

        path
    }

    /// Imports `file`, linked to `repo` when given.
    fn import(&self, file: &Path, repo: Option<&str>) {
        let manifest = import::Manifest {
            paths: vec![file.to_string_lossy().into_owned()],
            links: repo
                .map(|repo| Link {
                    repo_key: repo.to_owned(),
                    globs: Vec::new(),
                })
                .into_iter()
                .collect(),
            ..import::Manifest::default()
        };

        import_with(&self.context(), &manifest, &NoPdf, None, &|| false, &mut Vec::new()).expect("imports");
    }

    fn run(&self, flags: &[&str], paths: &[&Path]) -> Reply {
        let mut args: Vec<Option<&str>> = vec![Some("similar"), Some("--json")];
        args.extend(flags.iter().map(|flag| Some(*flag)));

        let paths: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        args.extend(paths.iter().map(|path| Some(path.as_str())));

        docs_reply(&self.context(), &args, &mut std::io::empty())
    }

    /// The `similar` entries for `paths`.
    fn similar(&self, flags: &[&str], paths: &[&Path]) -> Vec<Value> {
        let done = self.run(flags, paths).expect("similar answers");

        assert_eq!(done.code, 0);
        done.json["similar"].as_array().expect("a list").clone()
    }

    /// The one match of the one file, or `Null`.
    fn matched(&self, flags: &[&str], file: &Path) -> Value {
        self.similar(flags, &[file])[0]["match"].clone()
    }
}

#[test]
fn the_same_file_is_found_by_its_source_key_and_says_what_it_shares() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&file, None);

    let entries = lab.similar(&[], &[&file]);
    let found = &entries[0]["match"];

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], json!(file.to_string_lossy()));
    assert_eq!(found["id"], 1);
    assert_eq!(found["title"], "Guide");
    assert_eq!(found["place"], "local");
    assert_eq!(found["reason"], "sourceKey");
    assert_eq!(found["chunks"], 3);
    assert_eq!(found["sharedChunks"], 3);
    assert!(
        found.get("addedBy").is_some() && found.get("updatedBy").is_some(),
        "{found}"
    );
    assert!(
        found["updatedAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{found}"
    );
}

#[test]
fn the_same_text_under_another_name_is_the_same_content() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&file, None);

    let copy = lab.write("copy/renamed-guide.md", &guide("Guide", "How it works."));
    let found = lab.matched(&[], &copy);

    assert_eq!(found["reason"], "sameContent");
    assert_eq!(found["id"], 1);
    assert_eq!(found["sharedChunks"], 3);
}

#[test]
fn an_edited_copy_sharing_more_than_half_of_the_chunks_matches_the_best_document() {
    let lab = Lab::new();
    let original = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&original, None);
    // A document that shares less with the copy than the first one does.
    let sibling = lab.write("docs/other.md", &guide("Guide", "Another middle."));
    lab.import(&sibling, None);

    let edited = lab.write("copy/guide-v2.md", &guide("Guide", "How it works, edited."));
    let found = lab.matched(&[], &edited);

    assert_eq!(found["reason"], "sharedChunks");
    assert_eq!(found["sharedChunks"], 2);
    assert_eq!(found["chunks"], 3);
    assert_eq!(found["id"], 1, "equal shares go to the lowest id");
}

#[test]
fn a_file_sharing_one_chunk_is_not_similar_and_neither_is_one_sharing_half() {
    let lab = Lab::new();
    let original = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&original, None);

    // Same title, but a repository scope: the title does not reach the global document, so only
    // the chunks could. Intro is the same, the other two sections differ.
    let one = guide("Guide", "Another middle.").replace("limits part explains", "limits section describes");
    let one = lab.write("copy/one.md", &one);
    let repo = ["--repo", "git:h/o/r"];

    assert_eq!(lab.matched(&repo, &one), Value::Null);

    // Two of four chunks are half, not more than half.
    let extra = (0..8)
        .map(|n| format!("The extra part explains detail number {n} of something else."))
        .collect::<Vec<_>>()
        .join(" ");
    let two = lab.write(
        "copy/two.md",
        &format!("{}\n## Extra\n\n{extra}\n", guide("Guide", "Another middle.")),
    );
    assert_eq!(lab.matched(&repo, &two), Value::Null);

    let elsewhere = Lab::new();
    elsewhere.import(&two, None);
    assert_eq!(
        elsewhere.matched(&[], &two)["chunks"],
        4,
        "the file has four chunks, two of them the stored document's"
    );

    // The only chunk of a short file is in the document, which is all of it and still one chunk.
    let intro_only = guide("Guide", "x");
    let intro_only = intro_only.split("## Middle").next().expect("intro").to_owned();
    let tiny = lab.write("copy/tiny.md", &intro_only);
    assert_eq!(lab.matched(&repo, &tiny), Value::Null);
}

#[test]
fn a_title_matches_only_inside_its_scope() {
    let lab = Lab::new();
    let linked = lab.write("docs/guide.md", &guide("Setup Guide", "Linked to a repository."));
    lab.import(&linked, Some("git:h/o/a"));

    let global = lab.write("docs/global.md", &guide("Global Notes", "Linked to nothing."));
    lab.import(&global, None);

    // Nothing in common with the stored text but the title, in other letters and spacing.
    let same_title = |title: &str| lab.write("t/x.md", &format!("# {title}\n\nA text of its own, quite different."));

    let in_repo = lab.matched(&["--repo", "git:h/o/a"], &same_title("  SETUP   guide "));
    assert_eq!(in_repo["reason"], "title");
    assert_eq!(in_repo["id"], 1);
    assert_eq!(in_repo["sharedChunks"], 0);

    assert_eq!(
        lab.matched(&["--repo", "git:h/o/b"], &same_title("Setup Guide")),
        Value::Null
    );
    assert_eq!(lab.matched(&["--global"], &same_title("Setup Guide")), Value::Null);
    assert_eq!(
        lab.matched(&[], &same_title("Setup Guide")),
        Value::Null,
        "global is the default"
    );

    let in_global = lab.matched(&["--global"], &same_title("global notes"));
    assert_eq!(
        (in_global["reason"].as_str(), in_global["id"].as_i64()),
        (Some("title"), Some(2))
    );
    assert_eq!(
        lab.matched(&["--repo", "git:h/o/a"], &same_title("Global Notes")),
        Value::Null
    );
}

#[test]
fn the_repository_scope_makes_the_same_source_key_an_import_with_that_link_makes() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&file, Some("git:h/o/a"));

    assert_eq!(lab.matched(&["--repo", "git:h/o/a"], &file)["reason"], "sourceKey");
    // Another scope is another key: the chunks and the content are what is left (the content is
    // the same, so that is the reason).
    assert_eq!(lab.matched(&[], &file)["reason"], "sameContent");
}

#[test]
fn folders_expand_as_an_import_walks_them_and_files_that_fail_say_why() {
    let lab = Lab::new();
    let first = lab.write("docs/a.md", &guide("Alpha", "First."));
    lab.import(&first, None);
    lab.write("docs/sub/b.md", "# Beta\n\nSecond.");
    lab.write("docs/.hidden/c.md", "# Hidden\n\nNever walked.");
    lab.write("docs/code.rs", "fn main() {}");
    let empty = lab.write("loose/empty.md", "");
    let missing = lab.root.path().join("files/nowhere.md");

    let folder = lab.root.path().join("files/docs");
    let entries = lab.similar(&[], &[&folder, &empty, &missing]);
    let paths: Vec<_> = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("path"))
        .collect();

    assert_eq!(
        paths,
        [
            first.to_string_lossy().as_ref(),
            dunce::canonicalize(lab.root.path().join("files/docs/sub/b.md"))
                .expect("canonical")
                .to_string_lossy()
                .as_ref(),
            empty.to_string_lossy().as_ref(),
            missing.to_string_lossy().as_ref(),
        ]
    );
    assert_eq!(entries[0]["match"]["reason"], "sourceKey");
    assert_eq!(entries[1]["match"], Value::Null);
    assert!(entries[1].get("error").is_none());
    assert_eq!(entries[2]["match"], Value::Null);
    assert_eq!(entries[2]["error"], "the file has no text");
    assert_eq!(entries[3]["match"], Value::Null);
    assert!(
        entries[3]["error"]
            .as_str()
            .is_some_and(|error| error.contains("does not exist"))
    );
}

#[test]
fn without_a_database_nothing_matches_and_nothing_is_created() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", &guide("Guide", "How it works."));

    assert_eq!(lab.matched(&[], &file), Value::Null);
    assert!(!lab.database_exists());
}

#[test]
fn bad_arguments_are_usage_errors() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", "# Guide\n\ntext");
    let run = |args: &[Option<&str>]| docs_reply(&lab.context(), args, &mut std::io::empty());
    let path = file.to_string_lossy().into_owned();

    for bad in [
        vec![Some("similar"), Some("--json")],
        vec![Some("similar"), Some(path.as_str())],
        vec![
            Some("similar"),
            Some("--json"),
            Some("--global"),
            Some("--repo"),
            Some("git:h/o/r"),
            Some(path.as_str()),
        ],
        vec![
            Some("similar"),
            Some("--json"),
            Some("--repo"),
            Some("not-a-key"),
            Some(path.as_str()),
        ],
        vec![Some("similar"), Some("--json"), Some("--all"), Some(path.as_str())],
    ] {
        assert_eq!(run(&bad).expect_err("usage"), Failure::Usage, "{bad:?}");
    }

    let many: Vec<String> = (0..=FILES_PER_IMPORT_MAX).map(|n| format!("/f{n}.md")).collect();
    let mut args = vec![Some("similar"), Some("--json")];
    args.extend(many.iter().map(|path| Some(path.as_str())));

    assert_eq!(run(&args).expect_err("too many"), Failure::Usage);
}

#[test]
fn with_one_place_in_use_ids_stay_plain_and_the_place_is_that_one() {
    let lab = Lab::new();
    let file = lab.write("docs/guide.md", &guide("Guide", "How it works."));
    lab.import(&file, None);

    let found = lab.matched(&[], &file);

    assert_eq!(found["id"], 1);
    assert_eq!(found["place"], "local");
}

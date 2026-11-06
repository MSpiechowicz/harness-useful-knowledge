//! X6: the floors, the margin and the boosts, measured on the family's own documentation with the
//! real model. It skips unless `HARNESS_KNOWLEDGE_TEST_MODEL_DIR` names a folder with
//! `model_quantized.onnx` (or `onnx/model_quantized.onnx`) and `tokenizer.json`, and
//! `HARNESS_KNOWLEDGE_TEST_CORPUS_ROOT` names the folder that holds the clones
//! `harness-useful-desktop`, `harness-useful-memory` and `harness-useful-skills`.
//!
//! The corpus is the Markdown of `desktop/docs`, `memory/{README.md,docs}` and
//! `skills/{README.md,docs}`, copied under distinct folder names and imported by the real importer
//! with the real tokenizer and model. Embedding it takes minutes, so `HARNESS_KNOWLEDGE_TEST_CORPUS_CACHE`
//! may name a folder to keep the finished database in. Run it in release mode, with
//! `--nocapture` to see the tables:
//! `cargo test --release search::tests::calibration -- --nocapture`.

mod boosts;
mod floors;
mod recall;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::embed::Silent;
use crate::embed::engine::TractLoader;
use crate::embed::manifest::{MODEL, ONNX_FILE, TOKENIZER_FILE};
use crate::embed::pool::{Holder, Pool};
use crate::extract::pdf::PdfPages;
use crate::import::events::Events;
use crate::import::{self, EmbedError, Embedder, Importer, Manifest};
use crate::search::QueryEmbedder;
use crate::store::fuse::Scored;
use crate::store::{Detail, KnowledgeStore, SqliteStore};
use crate::tokens::ModelCounter;

struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("no PDFs in the corpus".to_owned())
    }
}

/// The model's worker pool, as the importer's embedder and the search's query embedder.
pub(super) struct Real {
    pool: Pool,
}

impl Embedder for Real {
    fn model_id(&self) -> &str {
        MODEL.id
    }

    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.pool
            .passages(passages, Duration::from_secs(600))
            .map_err(EmbedError)
    }
}

impl QueryEmbedder for Real {
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.pool.query(text)
    }
}

/// The model folder of the environment, with the two files where the holder reads them.
fn model_folder() -> Option<(tempfile::TempDir, PathBuf)> {
    let Ok(source) = std::env::var("HARNESS_KNOWLEDGE_TEST_MODEL_DIR") else {
        eprintln!("HARNESS_KNOWLEDGE_TEST_MODEL_DIR is not set, skipping");
        return None;
    };

    let source = PathBuf::from(source);
    let onnx = [source.join(ONNX_FILE), source.join("onnx").join(ONNX_FILE)]
        .into_iter()
        .find(|path| path.is_file())
        .expect("model_quantized.onnx under HARNESS_KNOWLEDGE_TEST_MODEL_DIR");
    let folder = tempfile::tempdir().expect("temp dir");

    for (from, name) in [(onnx, ONNX_FILE), (source.join(TOKENIZER_FILE), TOKENIZER_FILE)] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&from, folder.path().join(name)).expect("link the model file");
        #[cfg(not(unix))]
        std::fs::copy(&from, folder.path().join(name)).expect("copy the model file");
    }

    let tokenizer = folder.path().join(TOKENIZER_FILE);

    Some((folder, tokenizer))
}

/// `from/<relative>` copied to `to/<relative>` for every Markdown file under `from`.
fn copy_markdown(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());

        if path.is_dir() {
            copy_markdown(&path, &target);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            std::fs::create_dir_all(to).expect("create the corpus folder");
            std::fs::copy(&path, &target).expect("copy a document");
        }
    }
}

/// The corpus tree: `desktop/docs/**`, `memory/{README.md,docs/**}`, `skills/{README.md,docs/**}`.
fn corpus_tree(root: &Path, into: &Path) -> bool {
    let mut found = false;

    for (repo, name) in [
        ("harness-useful-desktop", "desktop"),
        ("harness-useful-memory", "memory"),
        ("harness-useful-skills", "skills"),
    ] {
        let repo = root.join(repo);
        found |= repo.join("docs").is_dir();

        copy_markdown(&repo.join("docs"), &into.join(name).join("docs"));

        if name != "desktop" && repo.join("README.md").is_file() {
            std::fs::create_dir_all(into.join(name)).expect("create the corpus folder");
            std::fs::copy(repo.join("README.md"), into.join(name).join("README.md")).expect("copy a README");
        }
    }

    found
}

/// The database of the corpus, built or reused from the cache folder.
fn corpus_database(tokenizer: &Path, real: &mut Real) -> Option<(tempfile::TempDir, SqliteStore)> {
    let Ok(root) = std::env::var("HARNESS_KNOWLEDGE_TEST_CORPUS_ROOT") else {
        eprintln!("HARNESS_KNOWLEDGE_TEST_CORPUS_ROOT is not set, skipping");
        return None;
    };

    let scratch = tempfile::tempdir().expect("temp dir");
    let cache = std::env::var_os("HARNESS_KNOWLEDGE_TEST_CORPUS_CACHE").map(PathBuf::from);
    let database = cache.as_ref().map_or_else(
        || scratch.path().join("knowledge.sqlite3"),
        |folder| folder.join("knowledge.sqlite3"),
    );

    if database.is_file() {
        eprintln!("corpus: reusing {}", database.display());
        return Some((scratch, SqliteStore::open(&database).expect("open the cached corpus")));
    }

    let tree = scratch.path().join("corpus");

    if !corpus_tree(Path::new(&root), &tree) {
        eprintln!("the corpus repositories are not under {root}, skipping");
        return None;
    }

    if let Some(folder) = &cache {
        std::fs::create_dir_all(folder).expect("create the cache folder");
    }

    let counter = ModelCounter::load(tokenizer).expect("the tokenizer");
    let mut store = SqliteStore::open(&database).expect("create the corpus database");
    let never = || false;
    let mut sink: Vec<u8> = Vec::new();

    let mut importer = Importer::new(&mut store, &counter, &NoPdf, &never);
    importer.embedder = Some(real);

    let manifest = Manifest {
        folders: vec![tree.to_string_lossy().into_owned()],
        ..Manifest::default()
    };
    let started = std::time::Instant::now();
    let report = import::run(&mut importer, &manifest, &mut Events::new(&mut sink)).expect("the import runs");
    drop(importer);

    eprintln!(
        "corpus: {:?} imported in {:.0}s ({} pending)",
        report.counts,
        started.elapsed().as_secs_f64(),
        report.pending
    );
    assert_eq!(report.counts.failed, 0, "{}", String::from_utf8_lossy(&sink));

    Some((scratch, store))
}

fn real_model(folder: &Path) -> Real {
    let holder = Arc::new(Holder::new(
        Arc::new(TractLoader),
        folder.to_path_buf(),
        MODEL.spec(),
        Arc::new(Silent),
    ));

    Real {
        pool: Pool::new(4, holder),
    }
}

/// The corpus database with the model that made its vectors.
pub(super) struct Lab {
    pub store: SqliteStore,
    pub real: Real,
    names: HashMap<i64, String>,
    _scratch: tempfile::TempDir,
    _model: tempfile::TempDir,
}

impl Lab {
    /// `None` (with a note) when the environment names no model or no corpus.
    fn open() -> Option<Self> {
        let (model, tokenizer) = model_folder()?;
        let mut real = real_model(model.path());
        let (scratch, store) = corpus_database(&tokenizer, &mut real)?;

        Some(Self {
            store,
            real,
            names: HashMap::new(),
            _scratch: scratch,
            _model: model,
        })
    }

    /// The source key of document `id` (`desktop/docs/hooks-format.md`).
    pub fn name(&mut self, id: i64) -> String {
        if let Some(name) = self.names.get(&id) {
            return name.clone();
        }

        let name = self
            .store
            .get(id, &Detail::default())
            .expect("get")
            .expect("document")
            .doc
            .source_key;
        self.names.insert(id, name.clone());

        name
    }

    /// The documents of `fused` in order of first appearance, as source keys.
    pub fn documents(&mut self, fused: &[Scored]) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();

        for scored in fused {
            let name = self.name(scored.hit.doc_id);

            if !found.contains(&name) {
                found.push(name);
            }
        }

        found
    }

    /// Every document id with its source key.
    pub fn all_documents(&mut self) -> Vec<(i64, String)> {
        let page = self
            .store
            .list(&crate::store::ListQuery {
                filter: crate::store::RepoFilter::All,
                format: None,
                text: String::new(),
                useful: None,
                author: None,
                want_authors: false,
                sort: crate::store::Sort::Title,
                descending: false,
                offset: 0,
                limit: 200,
                status: crate::store::StatusFilter::All,
            })
            .expect("list");

        page.items
            .into_iter()
            .map(|row| (row.doc.id, row.doc.source_key))
            .collect()
    }
}

#[test]
fn calibration_of_the_floors_the_margin_and_the_boosts_on_the_family_documentation() {
    let Some(mut lab) = Lab::open() else {
        return;
    };

    let stats = lab.store.stats().expect("stats");
    eprintln!(
        "corpus: {} documents, {} chunks, {} embedded",
        stats.documents, stats.chunks, stats.embedded_chunks
    );
    assert_eq!(stats.chunks, stats.embedded_chunks);

    // The links of an earlier run that did not end are not part of the baseline.
    for (id, _) in lab.all_documents() {
        lab.store.set_links(id, &[]).expect("clear the links");
    }

    recall::run(&mut lab);
    floors::run(&mut lab);
    boosts::run(&mut lab);
}

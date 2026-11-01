//! Test-only helpers: documents with chunks and fake embeddings, inserted without an importer
//! (the real import is built on `put_document` too).

use super::{Chunk, DIMS, Document, KnowledgeStore, Link, Origin, SqliteStore};

/// The model the fake embeddings claim to come from.
pub const MODEL: &str = "test-model";

/// A document to insert: `Seed::new("Title").chunk("Heading", "text").link("git:h/o/r", &["src/**"])`.
#[derive(Debug, Clone)]
pub struct Seed {
    title: String,
    source_key: String,
    format: String,
    bytes: u64,
    updated_at: i64,
    chunks: Vec<(String, String, Option<Vec<f32>>)>,
    links: Vec<Link>,
}

impl Seed {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            source_key: format!("/docs/{}", title.to_lowercase().replace(' ', "-")),
            format: "markdown".to_owned(),
            bytes: 1000,
            updated_at: 1_000,
            chunks: Vec::new(),
            links: Vec::new(),
        }
    }

    pub fn source(mut self, key: &str) -> Self {
        self.source_key = key.to_owned();
        self
    }

    pub fn format(mut self, format: &str) -> Self {
        self.format = format.to_owned();
        self
    }

    pub fn bytes(mut self, bytes: u64) -> Self {
        self.bytes = bytes;
        self
    }

    pub fn updated(mut self, at: i64) -> Self {
        self.updated_at = at;
        self
    }

    /// A chunk without an embedding.
    pub fn chunk(mut self, heading: &str, text: &str) -> Self {
        self.chunks.push((heading.to_owned(), text.to_owned(), None));
        self
    }

    /// A chunk embedded as `vector`.
    pub fn embedded(mut self, heading: &str, text: &str, vector: Vec<f32>) -> Self {
        self.chunks.push((heading.to_owned(), text.to_owned(), Some(vector)));
        self
    }

    pub fn link(mut self, repo: &str, globs: &[&str]) -> Self {
        self.links.push(Link {
            repo_key: repo.to_owned(),
            globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
        });
        self
    }

    /// The document as stored (id 0), its chunks and its links.
    pub fn parts(&self) -> (Document, Vec<Chunk>, Vec<Link>) {
        let doc = Document {
            id: 0,
            title: self.title.clone(),
            source_name: "source.md".to_owned(),
            source_key: self.source_key.clone(),
            format: self.format.clone(),
            content_sha256: format!("content-{}", self.source_key),
            file_sha256: format!("file-{}", self.source_key),
            bytes: self.bytes,
            chunk_count: 0,
            created_at: self.updated_at,
            updated_at: self.updated_at,
            origin: Origin::User,
            warnings: Vec::new(),
        };

        let chunks = self
            .chunks
            .iter()
            .enumerate()
            .map(|(ordinal, (heading, text, vector))| Chunk {
                id: 0,
                doc_id: 0,
                ordinal,
                heading_path: heading.clone(),
                text: text.clone(),
                chunk_sha: format!("sha-{text}"),
                tokens: text.split_whitespace().count(),
                char_start: ordinal * 100,
                char_end: ordinal * 100 + text.len(),
                embedding: vector.clone(),
                model_id: vector.as_ref().map(|_| MODEL.to_owned()),
            })
            .collect();

        (doc, chunks, self.links.clone())
    }
}

/// Inserts `seed` and returns the document id.
pub fn insert(store: &mut SqliteStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

/// A fresh in-memory store.
pub fn store() -> SqliteStore {
    SqliteStore::open_in_memory().expect("in-memory store")
}

/// The unit vector along axis `index`.
pub fn axis(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; DIMS];
    vector[index] = 1.0;
    vector
}

/// A unit vector between axes 0 and 1: its cosine with `axis(0)` is `cos(degrees)`.
pub fn angled(degrees: f32) -> Vec<f32> {
    let mut vector = vec![0.0; DIMS];
    vector[0] = degrees.to_radians().cos();
    vector[1] = degrees.to_radians().sin();
    vector
}

/// A deterministic, L2-normalised pseudo-random vector for `seed`.
pub fn fake_vector(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut vector: Vec<f32> = (0..DIMS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 2001) as f32 / 1000.0 - 1.0
        })
        .collect();

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    vector.iter_mut().for_each(|value| *value /= norm);
    vector
}

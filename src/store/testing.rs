//! Test-only helpers: documents with chunks and fake embeddings, inserted without an importer
//! (the real import is built on `put_document` too).

use super::{Chunk, DIMS, Detail, Document, KnowledgeStore, Link, Origin, SqliteStore, StoreError};

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

/// The ids of the chunks of document `doc`, in order.
fn chunk_ids(store: &mut dyn KnowledgeStore, doc: i64) -> Vec<i64> {
    let detail = Detail {
        sections: true,
        chunk: None,
    };
    let found = store.get(doc, &detail).expect("get").expect("the document exists");

    found
        .sections
        .expect("sections")
        .iter()
        .map(|section| section.chunk_id)
        .collect()
}

/// What every backend answers about the chunks that need a vector (the same checks run on each,
/// on an empty database):
///
/// - [`KnowledgeStore::pending_chunks`] lists the chunks without a vector of the model, the text
///   to embed as an import builds it, lowest ids first, in pages after a given id, and every chunk
///   for another model (a model switch).
/// - [`KnowledgeStore::set_chunk_embeddings`] stores vectors without changing a chunk id or text,
///   bumps the generation, skips a chunk that is gone and refuses a vector that is not [`DIMS`]
///   finite numbers.
pub fn pending_chunks_contract(store: &mut dyn KnowledgeStore) {
    const OTHER: &str = "other-model";

    let (doc, chunks, links) = Seed::new("Guide")
        .chunk("Intro", "alpha words")
        .embedded("Setup > Linux", "beta words", axis(2))
        .chunk("", "gamma words")
        .parts();
    let guide = store.put_document(&doc, &chunks, &links).expect("put the guide");

    let (doc, chunks, links) = Seed::new("Other").chunk("Notes", "delta words").parts();
    let other = store.put_document(&doc, &chunks, &links).expect("put the other");

    let guide_ids = chunk_ids(store, guide);
    let other_ids = chunk_ids(store, other);
    let (alpha, beta, gamma, delta) = (guide_ids[0], guide_ids[1], guide_ids[2], other_ids[0]);

    let pending = store.pending_chunks(MODEL, 0, 10).expect("pending");
    let ids: Vec<(i64, i64)> = pending.iter().map(|chunk| (chunk.chunk_id, chunk.doc_id)).collect();
    let texts: Vec<&str> = pending.iter().map(|chunk| chunk.embed_text.as_str()).collect();

    assert_eq!(ids, [(alpha, guide), (gamma, guide), (delta, other)]);
    assert_eq!(
        texts,
        [
            "Guide › Intro\nalpha words",
            "Guide\ngamma words",
            "Other › Notes\ndelta words"
        ]
    );

    // A page after a chunk id starts behind it, and the last page is empty.
    let first = store.pending_chunks(MODEL, 0, 2).expect("first page");
    assert_eq!(
        first.iter().map(|chunk| chunk.chunk_id).collect::<Vec<_>>(),
        [alpha, gamma]
    );

    let second = store.pending_chunks(MODEL, gamma, 2).expect("second page");
    assert_eq!(second.iter().map(|chunk| chunk.chunk_id).collect::<Vec<_>>(), [delta]);
    assert!(store.pending_chunks(MODEL, delta, 2).expect("last page").is_empty());

    // Another model makes every chunk pending, the embedded one too.
    let all: Vec<i64> = store
        .pending_chunks(OTHER, 0, 10)
        .expect("pending for another model")
        .iter()
        .map(|chunk| chunk.chunk_id)
        .collect();
    assert_eq!(all, [alpha, beta, gamma, delta]);

    let before = store.generation().expect("generation");
    let embedded_before = store.stats().expect("stats").embedded_chunks;
    let changed = store
        .set_chunk_embeddings(MODEL, &[(alpha, axis(0)), (gamma, axis(1))])
        .expect("set embeddings");

    assert_eq!(changed, 2);
    assert!(store.generation().expect("generation") > before, "the write is counted");
    assert_eq!(store.stats().expect("stats").embedded_chunks, embedded_before + 2);
    assert_eq!(chunk_ids(store, guide), guide_ids, "no chunk id changed");

    let still: Vec<i64> = store
        .pending_chunks(MODEL, 0, 10)
        .expect("pending")
        .iter()
        .map(|chunk| chunk.chunk_id)
        .collect();
    assert_eq!(still, [delta]);

    let kept = store
        .get(
            guide,
            &Detail {
                sections: false,
                chunk: Some(alpha),
            },
        )
        .expect("get")
        .and_then(|found| found.chunk)
        .expect("the chunk");
    assert_eq!(
        (kept.heading_path.as_str(), kept.text.as_str()),
        ("Intro", "alpha words")
    );

    let stored = store
        .reusable_embeddings(MODEL, &["sha-alpha words".to_owned()])
        .expect("embeddings");
    let vector = &stored["sha-alpha words"];
    assert!(
        vector.iter().zip(axis(0)).all(|(a, b)| (a - b).abs() < 1e-6),
        "the vector is stored exactly"
    );

    // A chunk that is gone is skipped, a bad vector or model is refused and changes nothing.
    assert_eq!(
        store
            .set_chunk_embeddings(MODEL, &[(987_654, axis(0))])
            .expect("skipped"),
        0
    );

    for bad in [vec![0.5; DIMS - 1], vec![f32::NAN; DIMS], Vec::new()] {
        assert!(matches!(
            store.set_chunk_embeddings(MODEL, &[(delta, bad)]),
            Err(StoreError::Invalid(_))
        ));
    }

    assert!(matches!(
        store.set_chunk_embeddings("", &[(delta, axis(0))]),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(store.pending_chunks(MODEL, 0, 10).expect("pending").len(), 1);

    // Embedding again with another model replaces the vector and the model.
    store
        .set_chunk_embeddings(OTHER, &[(alpha, axis(3))])
        .expect("re-embed");
    let for_model: Vec<i64> = store
        .pending_chunks(MODEL, 0, 10)
        .expect("pending")
        .iter()
        .map(|c| c.chunk_id)
        .collect();
    let for_other: Vec<i64> = store
        .pending_chunks(OTHER, 0, 10)
        .expect("pending")
        .iter()
        .map(|c| c.chunk_id)
        .collect();
    assert_eq!(for_model, [alpha, delta]);
    assert_eq!(for_other, [beta, gamma, delta]);
}

/// The titles [`title_order_contract`] sorts: Polish ones, upper and lower case, among ASCII ones.
pub const TITLES: [&str; 12] = [
    "Zebra",
    "alfa",
    "Łódź",
    "łabędź",
    "Żaba",
    "żółw",
    "Ąbc",
    "ąbc",
    "Bravo",
    "źdźbło",
    "Zażółć",
    "bravo",
];

/// The order the list sorted by title has in every place: case-insensitive for ASCII letters only
/// (the one fold every database can do the same way), then by bytes, ties by id. A title that
/// starts with a Polish capital sorts before its lower case letter, and both after every ASCII one.
pub fn title_order(mut titles: Vec<(i64, String)>) -> Vec<String> {
    titles.sort_by(|(a_id, a), (b_id, b)| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then(a_id.cmp(b_id)));

    titles.into_iter().map(|(_, title)| title).collect()
}

/// A list sorted by title, in either direction and in pages, follows [`title_order`] for
/// [`TITLES`], the way the merge of two places does (`store` is empty).
pub fn title_order_contract(store: &mut dyn KnowledgeStore) {
    use super::{ListQuery, RepoFilter, Sort};

    let mut inserted: Vec<(i64, String)> = Vec::new();

    for title in TITLES {
        let (doc, chunks, links) = Seed::new(title).chunk("Intro", "text").parts();
        let id = store.put_document(&doc, &chunks, &links).expect("put document");

        inserted.push((id, title.to_owned()));
    }

    let expected = title_order(inserted);
    let ask = |store: &mut dyn KnowledgeStore, descending: bool, offset: usize, limit: usize| -> Vec<String> {
        store
            .list(&ListQuery {
                filter: RepoFilter::All,
                format: None,
                text: String::new(),
                sort: Sort::Title,
                descending,
                offset,
                limit,
            })
            .expect("list")
            .items
            .into_iter()
            .map(|row| row.doc.title)
            .collect()
    };

    assert_eq!(ask(store, false, 0, 50), expected, "ascending");

    let mut reversed = expected.clone();
    reversed.reverse();
    assert_eq!(ask(store, true, 0, 50), reversed, "descending");

    let mut paged = Vec::new();

    for page in 0..4 {
        paged.extend(ask(store, false, page * 3, 3));
    }

    assert_eq!(paged, expected, "in pages of three");
}

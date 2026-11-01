//! What a knowledge backend stores and answers.
//!
//! A [`Document`] is one imported file, cut into [`Chunk`]s (the unit that is embedded and
//! searched) and linked to zero or more repositories ([`Link`]). A document without a link is
//! global. Every backend implements [`KnowledgeStore`]:
//!
//! - [`sqlite`]: the local file (FTS5 keyword index, vectors as BLOBs searched by brute force).
//! - [`fuse`]: merges the keyword and the vector candidates of one query (reciprocal rank fusion).
//! - [`rank`]: the repository and glob boosts, the floor and the per-document collapse.
//! - [`glob`]: the path globs of a link.
//! - [`clean`]: the checks every backend runs before it opens a write transaction.
//!
//! `updated_at` is the time the content was last imported: only [`KnowledgeStore::put_document`]
//! sets it, so renaming or relinking a document leaves it as it was.

pub mod clean;
pub mod fuse;
pub mod glob;
pub mod rank;
pub mod sqlite;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub use sqlite::SqliteStore;

use crate::project::ProjectKey;

/// The size of an embedding (multilingual-e5-small).
pub const DIMS: usize = 384;

/// Who a document came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Imported by the person.
    User,
    /// Proposed by an agent and approved in the review queue.
    AgentApproved,
}

impl Origin {
    /// The name stored and shown.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::AgentApproved => "agentApproved",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Self::User),
            "agentApproved" => Some(Self::AgentApproved),
            _ => None,
        }
    }
}

/// One imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// 0 for a document that is not stored yet ([`KnowledgeStore::put_document`] gives the id).
    pub id: i64,
    pub title: String,
    /// The file name as imported (`api.md`).
    pub source_name: String,
    /// What identifies the file across imports (its canonical path), for finding it again.
    pub source_key: String,
    /// `markdown`, `text`, `html`, `openapi`, `asciidoc`, `rst` or `pdf`.
    pub format: String,
    /// The hash of the extracted text: the same text is the same content.
    pub content_sha256: String,
    /// The hash of the file bytes.
    pub file_sha256: String,
    pub bytes: u64,
    /// Set from the chunks stored; ignored on input.
    pub chunk_count: usize,
    /// Unix seconds. Kept as it was when a stored document is put again.
    pub created_at: i64,
    /// Unix seconds of the latest import of the content.
    pub updated_at: i64,
    pub origin: Origin,
    /// What the extraction noticed (a table dropped, a page without text).
    pub warnings: Vec<String>,
}

/// A document's link to one repository, and the paths in it the document is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    /// `git:host/owner/repo` or `path:<root>` (see [`crate::project`]).
    #[serde(rename = "repo")]
    pub repo_key: String,
    /// Globs of repository-relative paths (`src/api/**`).
    #[serde(default)]
    pub globs: Vec<String>,
}

/// A part of a document: one section, or a piece of a long one.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// 0 for a chunk that is not stored yet; ignored on input.
    pub id: i64,
    /// The document it belongs to; ignored on input.
    pub doc_id: i64,
    /// Its position in the document, from 0.
    pub ordinal: usize,
    /// The headings above it, `Guide > Install > Linux`.
    pub heading_path: String,
    pub text: String,
    /// The hash of the text that was embedded: a chunk with the same hash and model reuses the
    /// embedding of an earlier import ([`KnowledgeStore::reusable_embeddings`]).
    pub chunk_sha: String,
    pub tokens: usize,
    /// Where the chunk is in the extracted text, in characters.
    pub char_start: usize,
    pub char_end: usize,
    /// [`DIMS`] floats, L2-normalised. `None` until the chunk is embedded.
    pub embedding: Option<Vec<f32>>,
    /// The model that made the embedding; set exactly when `embedding` is.
    pub model_id: Option<String>,
}

/// A chunk without its text or its vector: the outline of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub chunk_id: i64,
    pub ordinal: usize,
    pub heading_path: String,
    pub tokens: usize,
    pub char_start: usize,
    pub char_end: usize,
}

/// Which documents a search may see.
///
/// From a repository: the global documents and the ones linked to it. With
/// `include_other_repos`, or from [`ProjectKey::every`], all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub repo: Option<ProjectKey>,
    pub include_other_repos: bool,
}

impl Scope {
    /// The documents visible from `repo` (none: only the global ones).
    pub fn from_repo(repo: Option<ProjectKey>) -> Self {
        Self {
            repo,
            include_other_repos: false,
        }
    }

    /// Every document.
    pub fn everything() -> Self {
        Self {
            repo: None,
            include_other_repos: true,
        }
    }

    /// True when documents of every repository are visible.
    pub fn sees_everything(&self) -> bool {
        self.include_other_repos || self.repo.as_ref().is_some_and(ProjectKey::is_every)
    }

    /// The repository whose documents are boosted; none for no repository or for everything.
    pub fn repo_key(&self) -> Option<&str> {
        self.repo
            .as_ref()
            .filter(|repo| !repo.is_every())
            .map(|repo| repo.key.as_str())
    }
}

/// A chunk a search found, with the raw score of the list that found it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    pub chunk_id: i64,
    pub doc_id: i64,
    /// The title of the document.
    pub title: String,
    pub ordinal: usize,
    pub heading_path: String,
    pub text: String,
    /// The keyword relevance (the negated BM25, higher is better) or the cosine similarity.
    pub score: f64,
    /// When the document was last imported.
    pub updated_at: i64,
    /// The globs of the document's link to the scope's repository: `None` when it is not linked.
    pub repo_globs: Option<Vec<String>>,
}

/// Which documents `docs list` shows by repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoFilter {
    All,
    /// Documents linked to no repository.
    Global,
    /// Documents linked to this repository key.
    Repo(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Title,
    Updated,
    Size,
}

/// One page of the document list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    pub filter: RepoFilter,
    /// Exactly this format.
    pub format: Option<String>,
    /// Keeps the documents whose title contains it, or whose text has all its words (the last as
    /// a prefix). Blank keeps all.
    pub text: String,
    pub sort: Sort,
    pub descending: bool,
    pub offset: usize,
    pub limit: usize,
}

/// A listed document with its links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRow {
    pub doc: Document,
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPage {
    pub items: Vec<DocumentRow>,
    /// The documents the filters keep, over all pages.
    pub total: usize,
}

/// What [`KnowledgeStore::get`] reads besides the document and its links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub sections: bool,
    /// The id of a chunk of the document to read whole.
    pub chunk: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentDetail {
    pub doc: Document,
    pub links: Vec<Link>,
    /// Every chunk's outline, in order, when asked.
    pub sections: Option<Vec<Section>>,
    /// The chunk asked for, with its text and no vector.
    pub chunk: Option<Chunk>,
}

/// How many documents each repository has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoCounts {
    /// The documents linked to no repository.
    pub global: usize,
    /// The repository keys that have documents, with their counts, most first.
    pub repos: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub documents: usize,
    pub chunks: usize,
    /// Chunks that have a vector of the current model.
    pub embedded_chunks: usize,
    /// The bytes of the imported files.
    pub bytes: u64,
    pub global_documents: usize,
    /// How many repositories have documents.
    pub repositories: usize,
    pub generation: i64,
    pub schema_version: i64,
    /// The model that embedded the chunks, once one has.
    pub model_id: Option<String>,
}

/// Store failures; every message is safe to show to the agent.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The input breaks a limit or a rule.
    #[error("{0}")]
    Invalid(String),
    /// The document or chunk does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The database was written by a newer server; nothing is read or written.
    #[error("the knowledge database is newer than this server")]
    NewerSchema,
    /// This SQLite build has no FTS5, so there is no keyword index.
    #[error("this SQLite build has no FTS5 full-text search")]
    NoFts5,
    /// The database failed (the message carries no stored text).
    #[error("knowledge database error: {0}")]
    Backend(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        if sqlite::is_missing_fts5(&error) {
            return Self::NoFts5;
        }

        let message = match &error {
            rusqlite::Error::SqliteFailure(code, _) => format!("{:?}", code.code),
            other => other.to_string(),
        };

        Self::Backend(message.chars().take(200).collect())
    }
}

/// A knowledge backend. Writes and reads take `&mut self`; one store serves one process.
pub trait KnowledgeStore: Send {
    /// The value of a meta key (`schema_version`, `model_id`, `dims`, `generation`, ...).
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError>;

    /// Sets a meta key and bumps the generation. `generation`, `schema_version` and `dims` are
    /// the store's own and refused.
    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError>;

    /// The write counter: it grows on every write, so a process holding results can tell they are
    /// stale.
    fn generation(&mut self) -> Result<i64, StoreError>;

    /// Stores `doc` with its `chunks` and `links` in one transaction and returns its id. A `doc`
    /// with id 0 is inserted, one with an id replaces that document (its chunks and links are
    /// replaced, `created_at` is kept; a missing id is [`StoreError::NotFound`]). The ids of the
    /// chunks are given by the store, and `chunk_count` is the length of `chunks`.
    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError>;

    /// A document with this content hash (the oldest), for finding an unchanged import.
    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError>;

    /// A document imported from this source (the oldest), for updating it.
    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError>;

    /// The embeddings stored for chunks with these hashes and this model, by hash, so an import
    /// does not embed text it already embedded.
    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError>;

    /// Replaces the links of a document; false when there is no such document.
    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError>;

    /// Retitles a document; false when there is no such document.
    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError>;

    /// Deletes documents with their chunks, links and index entries; how many existed.
    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError>;

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError>;

    /// A document with its links, and what `detail` asks for.
    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError>;

    /// The best `k` chunks for the words of `query`, the most relevant first.
    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;

    /// The `k` chunks closest to `vector` (L2-normalised, [`DIMS`] floats), the closest first.
    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;

    fn repos(&mut self) -> Result<RepoCounts, StoreError>;

    fn stats(&mut self) -> Result<Stats, StoreError>;
}

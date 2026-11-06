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
//! - [`routed`]: this computer's file and a remote database as one store, ids carrying their place.
//! - [`reach`]: whether the remote database could be reached lately, shared by the processes.
//! - [`clean`]: the checks every backend runs before it opens a write transaction.
//!
//! `updated_at` is the time the content was last imported: only [`KnowledgeStore::put_document`]
//! sets it, so renaming or relinking a document leaves it as it was.

pub mod author_list;
pub mod clean;
mod error;
pub mod fuse;
pub mod glob;
pub mod libsql;
pub mod mongo;
pub mod postgres;
pub mod quality;
pub mod rank;
pub mod reach;
pub mod review;
pub mod routed;
pub mod sqlite;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod upgrade;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub use author_list::{AuthorFacet, AuthorFilter};
pub use error::StoreError;
pub use quality::{Author, Columns, DocUsage, TitleScope, UsefulFilter};
pub use reach::Reach;
pub use review::{
    Authorship, Comment, EventKind, History, NewComment, PendingVersion, Piece, QuoteKind, QuoteLine, ReviewAction,
    ReviewChange, ReviewEvent, ReviewReply, Reviewed, SectionText, Side, Status, StatusFilter, StoredFile, Visibility,
};
pub use routed::{Kinds, Part, RoutedStore};
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
    /// Who first stored it. Kept as it was when a stored document is put again (the input is
    /// ignored then); `None` when no git name was known, or the database has no author columns.
    pub added_by: Option<Author>,
    /// Who stored the latest import of the content.
    pub updated_by: Option<Author>,
    /// Live for a document agents see (see [`review`]). Ignored when a stored document is put
    /// again: a row changes state through [`KnowledgeStore::set_review`] and
    /// [`KnowledgeStore::promote`] only, and a change of content sends a waiting row back to draft.
    pub status: Status,
    /// The live document this row is a pending version of; `None` for every other row. Set when a
    /// pending version is first stored, never changed after.
    pub pending_of: Option<i64>,
    /// Raised by every content change and transition; set by the store, ignored on input.
    pub review_rev: i64,
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
    /// Whether drafts are seen too (the person's own searches). Agents never see them: every
    /// constructor but [`Scope::with_drafts`] leaves this off. A pending version is never seen.
    pub drafts: bool,
}

impl Scope {
    /// The documents visible from `repo` (none: only the global ones).
    pub fn from_repo(repo: Option<ProjectKey>) -> Self {
        Self {
            repo,
            include_other_repos: false,
            drafts: false,
        }
    }

    /// Every document.
    pub fn everything() -> Self {
        Self {
            repo: None,
            include_other_repos: true,
            drafts: false,
        }
    }

    /// The same scope, also seeing drafts (live documents and the rows that are not a pending
    /// version).
    #[must_use]
    pub fn with_drafts(mut self) -> Self {
        self.drafts = true;
        self
    }

    /// The scope as a team database is asked it. A repository that is a folder of this computer
    /// (`path:`) never leaves it, and a team database holds no link to one, so such a scope reads
    /// there as no repository: the global documents. Any other scope is unchanged.
    pub fn for_team(&self) -> std::borrow::Cow<'_, Scope> {
        if self.repo.as_ref().is_some_and(|repo| clean::is_local_path(&repo.key)) {
            return std::borrow::Cow::Owned(Scope {
                repo: None,
                ..self.clone()
            });
        }

        std::borrow::Cow::Borrowed(self)
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
    /// The state of the document; always live for an agent's search.
    pub status: Status,
}

/// Which documents `docs list` shows by repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoFilter {
    All,
    /// Documents linked to no repository.
    Global,
    /// Documents linked to this repository key.
    Repo(String),
    /// Documents linked to at least one repository.
    Linked,
}

impl RepoFilter {
    /// Whether this asks for the documents of a folder of this computer (`path:`), which a team
    /// database never holds and is never asked for.
    pub fn is_local_path(&self) -> bool {
        matches!(self, Self::Repo(key) if clean::is_local_path(key))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Title,
    Updated,
    Size,
    Format,
    Chunks,
    /// By how many path globs its links carry together (a global or whole-repository one counts 0).
    Paths,
    /// By the first repository key a document links to; global documents come first ascending.
    Repos,
    /// By how often agents found it useful (a document nobody rated counts 0).
    Useful,
    /// By when an agent last found it useful (never counts 0).
    LastUseful,
    /// By the name of the person who added it, ASCII letters folded to lower case. A document with
    /// no known author comes last, ascending or descending.
    AddedBy,
}

impl Sort {
    /// The order a database without the counter columns uses for the sorts by usefulness.
    pub fn without_usage(self) -> Self {
        match self {
            Self::Useful | Self::LastUseful => Self::Updated,
            other => other,
        }
    }

    /// The order a database without the author columns uses for the sort by who added a document.
    pub fn without_authors(self) -> Self {
        match self {
            Self::AddedBy => Self::Updated,
            other => other,
        }
    }

    /// The sort a database with these `columns` can do: the one asked for, or the default order.
    pub fn for_columns(self, columns: Columns) -> Self {
        let sort = if columns.usage { self } else { self.without_usage() };

        if columns.authors { sort } else { sort.without_authors() }
    }
}

/// One page of the document list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    pub filter: RepoFilter,
    /// Exactly this format.
    pub format: Option<String>,
    /// Keeps the documents whose title contains it, or whose text has all its words next to each
    /// other in that order (the last as a prefix). Blank keeps all.
    pub text: String,
    /// Keeps the documents agents found useful, or never useful; `None` keeps all.
    pub useful: Option<UsefulFilter>,
    /// Keeps the documents added by this person, or by nobody known; `None` keeps all.
    pub author: Option<AuthorFilter>,
    /// Whether the page carries the authors facet ([`DocumentPage::authors`]). It costs a grouping
    /// query over every document the other filters keep, so only the person's own list asks for it:
    /// without it the facet is empty and no such query runs.
    pub want_authors: bool,
    /// Keeps the rows in these states (see [`StatusFilter`]). No default: an agent's list asks for
    /// [`StatusFilter::Live`].
    pub status: StatusFilter,
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
    /// `None` when the database has no counter columns.
    pub usage: Option<DocUsage>,
    /// The pending version of a live document.
    pub pending: Option<PendingVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPage {
    pub items: Vec<DocumentRow>,
    /// The documents the filters keep, over all pages.
    pub total: usize,
    /// Who added the documents the filters keep, the author filter apart (see [`AuthorFacet`]). Empty
    /// unless the query asked for it ([`ListQuery::want_authors`]).
    pub authors: AuthorFacet,
}

/// What [`KnowledgeStore::get`] reads besides the document and its links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    pub sections: bool,
    /// The id of a chunk of the document to read whole.
    pub chunk: Option<i64>,
    /// Whether a document that is not live is read too (the person's own reads, a pending version
    /// included). Off for agents: the document is then `None`.
    pub drafts: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentDetail {
    pub doc: Document,
    pub links: Vec<Link>,
    /// `None` when the database has no counter columns.
    pub usage: Option<DocUsage>,
    /// Every chunk's outline, in order, when asked.
    pub sections: Option<Vec<Section>>,
    /// The chunk asked for, with its text and no vector.
    pub chunk: Option<Chunk>,
    /// The pending version of a live document; only read when the detail asks for `drafts`.
    pub pending: Option<PendingVersion>,
}

/// A chunk of a document that matches a text, with the text to cut a snippet from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMatch {
    pub chunk_id: i64,
    pub ordinal: usize,
    pub heading_path: String,
    pub text: String,
}

/// The chunks of one document that match a text, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkMatches {
    /// Every chunk that matches, over the `limit`.
    pub total: usize,
    pub chunks: Vec<ChunkMatch>,
}

/// A chunk that has no vector of the current model, with the text to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChunk {
    pub chunk_id: i64,
    pub doc_id: i64,
    /// What the embedder is given, as an import builds it (see [`crate::chunk::passage`]).
    pub embed_text: String,
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
    /// Rows in state draft, pending versions included.
    pub drafts: usize,
    /// Rows in state waiting, pending versions included: what the team waits to approve.
    pub waiting: usize,
    /// Rows in state `changesRequested`, pending versions included.
    pub changes_requested: usize,
    /// Rows that are a pending version of a live document. `documents` does not count them.
    pub pending: usize,
}

/// A knowledge backend. Writes and reads take `&mut self`; one store serves one process.
pub trait KnowledgeStore: Send {
    /// The value of a meta key (`schema_version`, `model_id`, `dims`, `generation`, ...).
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError>;

    /// Sets a meta key and bumps the generation. `generation`, `schema_version` and `dims` are
    /// the store's own and refused.
    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError>;

    /// Sets several meta keys in one transaction (all or none) and bumps the generation. Keys as
    /// [`KnowledgeStore::set_meta`].
    fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError>;

    /// The write counter: it grows on every write, so a process holding results can tell they are
    /// stale.
    fn generation(&mut self) -> Result<i64, StoreError>;

    /// Stores `doc` with its `chunks` and `links` in one transaction and returns its id. A `doc`
    /// with id 0 is inserted, one with an id replaces that document (its chunks and links are
    /// replaced, `created_at` is kept; a missing id is [`StoreError::NotFound`]). The ids of the
    /// chunks are given by the store, and `chunk_count` is the length of `chunks`.
    ///
    /// An insert is stored in the state `doc.status` names (a pending version names the live
    /// document in `doc.pending_of`, which must be live and have none yet). A replace raises the
    /// revision, sends a waiting row back to draft and drops the stored original file. A row that is
    /// not live records `drafted`, `updateDrafted` or `edited` in its history.
    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError>;

    /// A document with this content hash (the oldest), for finding an unchanged import. A pending
    /// version is never found, and a draft only with [`Visibility::WithDrafts`].
    fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;

    /// A document imported from this source (the oldest), for updating it. Visibility as
    /// [`KnowledgeStore::find_by_content_sha`].
    fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;

    /// The embeddings stored for chunks with these hashes and this model, by hash, so an import
    /// does not embed text it already embedded.
    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError>;

    /// The chunks that have no embedding or one of another model than `model_id`, with an id
    /// greater than `after` (0 for the first page), the lowest ids first, at most `limit` (and
    /// [`crate::limits::EMBED_BATCH_MAX`]). Give the last id of a page as `after` for the next.
    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError>;

    /// Stores each `(chunk id, vector)` as the embedding of that chunk, made by `model_id`, and
    /// bumps the generation. Chunk ids, texts and the keyword index stay as they are. A chunk that
    /// does not exist (deleted meanwhile) is skipped. How many chunks were updated.
    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError>;

    /// Counts a time shown to an agent on each document of `ids`. Best effort: one attempt that
    /// never waits for a lock, and nothing without the counter columns. An unknown id is skipped.
    fn record_shown(&mut self, _ids: &[i64]) -> Result<(), StoreError> {
        Ok(())
    }

    /// Counts a time found useful, at `at` (unix seconds), on each document of `ids`. As
    /// [`KnowledgeStore::record_shown`].
    fn record_useful(&mut self, _ids: &[i64], _at: i64) -> Result<(), StoreError> {
        Ok(())
    }

    /// The documents of `scope` whose title equals `title` once both are normalised (see
    /// [`quality::normalise_title`]), by id. Scans the titles of the scope, at most
    /// [`quality::TITLE_SCAN_MAX`], so it needs no index.
    /// A pending version is never found, and a draft only with [`Visibility::WithDrafts`].
    fn find_by_title(
        &mut self,
        title: &str,
        scope: &TitleScope,
        visibility: Visibility,
    ) -> Result<Vec<Document>, StoreError>;

    /// For each document with a chunk whose hash is in `chunk_shas` (at most
    /// [`crate::limits::SHARED_SHAS_MAX`]), the number of distinct hashes of them it has, by id.
    /// Visibility as [`KnowledgeStore::find_by_content_sha`].
    fn docs_sharing_chunks(
        &mut self,
        chunk_shas: &[String],
        visibility: Visibility,
    ) -> Result<Vec<(i64, usize)>, StoreError>;

    /// Whether the database this store writes to lets this user change documents: `None` when
    /// that is not known (a failed probe, a handle opened to read, a server that cannot tell).
    fn remote_writable(&mut self) -> Option<bool> {
        None
    }

    /// The value of a meta key of the shared (remote) database: a routed store reads it from the
    /// remote alone and never falls back to this computer's file (`None` with no remote, an error
    /// while the remote cannot be reached). A store that is one place reads its own.
    fn remote_meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.meta(key)
    }

    /// Replaces the links of a document; false when there is no such document.
    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError>;

    /// Retitles a document, in one transaction; false when there is no such document. A row that
    /// is not live takes it as a change of its content: its revision grows, a waiting row is a
    /// draft again and an `edited` event (comment `renamed`, by `by`, at `at` unix seconds) is
    /// recorded. A live document is retitled directly, and so is its pending version (as that same
    /// change), so a promote keeps the new title and the reviewer sees the rename.
    fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError>;

    /// Deletes documents with their chunks, links and index entries; how many existed.
    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError>;

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError>;

    /// [`KnowledgeStore::list`] narrowed to the documents kept in `place` (all of them for
    /// `None`), paged and counted over that set. Only a routed store has two places to tell
    /// apart, any other lists as it is: the caller knows which place such a store is.
    fn list_in(&mut self, _place: Option<crate::config::Place>, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.list(query)
    }

    /// A document with its links, and what `detail` asks for.
    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError>;

    /// The best `k` chunks for the words of `query`, the most relevant first.
    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;

    /// The `k` chunks closest to `vector` (L2-normalised, [`DIMS`] floats), the closest first.
    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;

    /// The chunks of document `doc_id` that match `text` the way the list's text filter reads it
    /// (see [`ListQuery::text`]), by position, at most `limit`; none for a text with no word or a
    /// document that does not exist.
    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError>;

    /// The pending version of live document `id`; `None` when it has none (or `id` is not a live
    /// document).
    fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError>;

    /// Applies `change` to document `id` if its revision is still `expect`: in one transaction the
    /// row takes the new state, its revision grows by one, the event is recorded and (for
    /// [`ReviewAction::Ask`]) the sections are kept as asked. A row in the wrong state, or at
    /// another revision, is left alone and answered as such. An action that makes a pending version
    /// live goes through [`KnowledgeStore::promote`] (this fails with [`StoreError::Invalid`]).
    fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;

    /// Makes pending version `id` live, if its revision is still `expect` and `change` is an action
    /// that goes live ([`ReviewAction::Approve`] or [`ReviewAction::GoLive`], allowed from the
    /// row's state), in one transaction: the live chunks are replaced by the pending ones with their
    /// embeddings, the live row takes the title, source, format, hashes, bytes, warnings, origin,
    /// `updated_at` and `updated_by` of the pending row and keeps its id, links, `created_at`,
    /// `added_by` and counters. The events, comments and original file move to the live row and the
    /// pending row is deleted. The live row's revision becomes the larger of the two, plus one.
    fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;

    /// The history of document `id`, newest first; `None` when there is no such document.
    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError>;

    /// The writers and the latest asker of many documents at once, for a list that names the people
    /// of the rows waiting for approval (see [`review::Authorship`]): at most `per_doc` writers each
    /// (the first ones). A document whose history could not be read may be left out, one with no such
    /// events has none. Backends read them in a small constant number of queries.
    fn authorship_of(&mut self, ids: &[i64], per_doc: usize) -> Result<HashMap<i64, review::Authorship>, StoreError> {
        let mut found = HashMap::new();

        for &id in ids {
            let Ok(mut writers) = self.writers_since_live(id) else {
                continue;
            };
            let Ok(history) = self.history(id) else { continue };

            writers.truncate(per_doc);
            found.insert(
                id,
                review::Authorship {
                    writers,
                    asker: history.as_ref().and_then(review::latest_asker),
                },
            );
        }

        Ok(found)
    }

    /// The distinct names on the `drafted`, `edited` and `updateDrafted` events of document `id`
    /// since it was last made live (after its newest `approved` or `wentLive` event), oldest
    /// first. It looks at every event, not only the newest [`review::HISTORY_MAX`], so a writer
    /// cannot push their own events out of sight. Events without a name are left out, and so is a
    /// document that does not exist (no names).
    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError>;

    /// The sections of document `id` as they are stored now; `None` when there is no such document.
    fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError>;

    /// The sections of document `id` as they were when it was asked at `revision` (the revision
    /// after the transition); `None` when none were kept.
    fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError>;

    /// Adds the comments of one review to document `id`, written by `by` at `at` (unix seconds),
    /// and returns their ids. They share a review number and carry `revision`, the one that was
    /// reviewed (not the document's current one, which a change may have moved on), and they do not
    /// change the document. Fails with [`StoreError::NotFound`] for an unknown document.
    fn add_comments(
        &mut self,
        id: i64,
        by: Option<&str>,
        at: i64,
        revision: i64,
        comments: &[NewComment],
    ) -> Result<Vec<i64>, StoreError>;

    /// Adds a reply to the comment `parent` of document `id`: its id, or `None` when `parent` is
    /// not a comment of the document that can be answered (a reply cannot).
    fn reply_comment(
        &mut self,
        id: i64,
        parent: i64,
        by: Option<&str>,
        at: i64,
        body: &str,
    ) -> Result<Option<i64>, StoreError>;

    /// Marks the comment `comment` of document `id` resolved by `by`, or open again; false when it
    /// is not a comment of the document that can be resolved (a reply cannot).
    fn resolve_comment(
        &mut self,
        id: i64,
        comment: i64,
        resolved: bool,
        by: Option<&str>,
        at: i64,
    ) -> Result<bool, StoreError>;

    /// Deletes the comment `comment` of document `id` and, when it starts a thread, its replies,
    /// in one step. The ids deleted, the comment first; `None` when `comment` is not a comment of
    /// the document. A reply deletes only itself. The history of the document is not touched.
    fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError>;

    /// Every comment of document `id`, in the order they were written.
    fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError>;

    /// Keeps `bytes` as the original file `name` of document `id` (replacing the one it has);
    /// false when there is no such document. Fails over [`crate::limits::FILE_BYTES_MAX`]
    /// ([`crate::limits::PDF_BYTES_MAX`] for a PDF).
    fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError>;

    /// The original file of document `id`, when it has one.
    fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError>;

    /// The stores a routed store reads, with the kinds of document each may show: `None` for a
    /// store that is one place. A search runs its pipeline in each and fuses the results.
    fn routed_parts(&mut self) -> Option<Vec<Part<'_>>> {
        None
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError>;

    fn stats(&mut self) -> Result<Stats, StoreError>;

    /// True while a routed store answers from this computer alone because its remote database
    /// cannot be reached (or is cooling down after it could not be). Read it after the call it
    /// is about. A store that is one place is never offline.
    fn offline(&self) -> bool {
        false
    }

    /// Tells a routed store that a call it handed out through [`KnowledgeStore::routed_parts`]
    /// failed with `error`. True when that was the remote database being unreachable and the call
    /// can be tried again, now without it.
    fn remote_lost(&mut self, _error: &StoreError) -> bool {
        false
    }
}

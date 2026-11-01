//! Search: one query through the keyword index and the vectors, fused and ranked.
//!
//! 1. The query is embedded as a `query:` text by an [`embedder`]. When none is available the
//!    search is keyword-only and says so ([`Vectors::Unavailable`]).
//! 2. The store returns the keyword top [`CANDIDATES`] and the vector top [`CANDIDATES`] of the
//!    documents the [`Scope`] sees. Vector candidates under the cosine floor and keyword candidates
//!    under the relevance floor never reach the fusion: reciprocal rank fusion only knows ranks, so
//!    without them an off-topic query would still fuse its best chunks into an answer.
//! 3. [`fuse`](crate::store::fuse) merges the lists, [`rank`](crate::store::rank) applies the repository
//!    and glob boosts, the floor and the margin, and keeps two chunks per document.
//! 4. [`render`] shows what is left as pointers or chunks, wrapped as untrusted documentation.

pub mod embedder;
pub mod render;
#[cfg(test)]
mod tests;

pub use embedder::{AutoEmbedder, QueryEmbedder, SocketEmbedder};
pub use render::Pointer;

use crate::store::fuse::{Scored, fuse};
use crate::store::routed::{ids, merge_found};
use crate::store::{Kinds, KnowledgeStore, Part, Scope, StoreError, rank};

/// How many candidates each list asks the store for.
pub const CANDIDATES: usize = 50;

/// Which candidate lists a search reads. Only the calibration measures them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lists {
    Both,
    KeywordOnly,
    VectorOnly,
}

/// How strict a search is: the floors a candidate needs before the fusion, and the lists it reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub lists: Lists,
    /// The least cosine of a vector candidate.
    pub cosine: f64,
    /// The least relevance (the negated BM25) of a keyword candidate.
    pub keyword: f64,
}

impl Tuning {
    /// What `search` and the MCP tool use.
    pub const SEARCH: Self = Self {
        lists: Lists::Both,
        cosine: rank::COSINE_FLOOR,
        keyword: rank::KEYWORD_FLOOR,
    };

    /// What the recall hook uses while the embedder answers: it adds text to a prompt unasked, so
    /// only the vector list counts, above a stricter cosine floor.
    pub const RECALL_VECTORS: Self = Self {
        lists: Lists::VectorOnly,
        cosine: rank::RECALL_COSINE_FLOOR,
        keyword: f64::MAX,
    };

    /// What the recall hook uses when it does not: the keyword list alone, above a stricter floor.
    pub const RECALL_KEYWORDS: Self = Self {
        lists: Lists::KeywordOnly,
        cosine: f64::MAX,
        keyword: rank::RECALL_KEYWORD_FLOOR,
    };
}

/// What to search for.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub text: &'a str,
    pub scope: &'a Scope,
    /// Repository-relative paths the caller is working on, for the glob boost.
    pub paths: &'a [String],
    /// How many chunks to return at most.
    pub limit: usize,
}

/// What became of the vector side of a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vectors {
    /// The query was embedded and the vector list read.
    Used,
    /// The vector list was not read (the caller did not ask, or the query has no text).
    NotAsked,
    /// The query could not be embedded: the answer is keyword-only. The reason is safe to show.
    Unavailable(String),
}

/// The answer to a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Best first, each document at most twice, with the boosted fused scores.
    pub hits: Vec<Scored>,
    pub vectors: Vectors,
}

/// Answers `query` from `store`, with `embedder` for the vector side (none: keyword-only).
///
/// A routed store (see [`KnowledgeStore::routed_parts`]) is searched place by place, each with the
/// whole pipeline, and the lists are fused by rank ([`fuse_places`]).
///
/// A remote place that turns out unreachable during the search is left out and the search runs
/// again over the places that are left (see [`KnowledgeStore::remote_lost`]).
pub fn run(
    store: &mut dyn KnowledgeStore,
    mut embedder: Option<&mut dyn QueryEmbedder>,
    query: &Query,
    tuning: Tuning,
) -> Result<Found, StoreError> {
    loop {
        let Some(parts) = store.routed_parts() else {
            return run_in(store, embedder.take(), query, tuning, Kinds::ALL);
        };
        let asking = embedder
            .as_mut()
            .map(|embedder| &mut **embedder as &mut dyn QueryEmbedder);

        match run_parts(parts, asking, query, tuning) {
            Err(error) if store.remote_lost(&error) => {}
            found => return found,
        }
    }
}

/// The pipeline in one store, which shows the document `kinds` only.
fn run_in(
    store: &mut dyn KnowledgeStore,
    embedder: Option<&mut dyn QueryEmbedder>,
    query: &Query,
    tuning: Tuning,
    kinds: Kinds,
) -> Result<Found, StoreError> {
    let (fused, vectors) = candidates(store, embedder, query, tuning, kinds)?;

    Ok(Found {
        hits: rank::rank(fused, query.paths, query.limit),
        vectors,
    })
}

/// The pipeline in each part, the query embedded once.
fn run_parts(
    parts: Vec<Part<'_>>,
    embedder: Option<&mut dyn QueryEmbedder>,
    query: &Query,
    tuning: Tuning,
) -> Result<Found, StoreError> {
    let mut cached = embedder.map(Cached::new);
    let mut founds = Vec::new();

    for part in parts {
        if part.kinds.is_none() {
            continue;
        }

        let embedder = cached.as_mut().map(|cached| cached as &mut dyn QueryEmbedder);
        let mut found = run_in(part.store, embedder, query, tuning, part.kinds)?;

        // From here on the ids say which place the chunk is in.
        for scored in &mut found.hits {
            scored.hit.chunk_id = ids::tag(part.place, scored.hit.chunk_id);
            scored.hit.doc_id = ids::tag(part.place, scored.hit.doc_id);
        }

        founds.push(found);
    }

    Ok(merge_found(founds, query.limit))
}

/// An embedder that answers the same text twice from its first answer.
struct Cached<'a> {
    inner: &'a mut dyn QueryEmbedder,
    last: Option<(String, Result<Vec<f32>, String>)>,
}

impl<'a> Cached<'a> {
    fn new(inner: &'a mut dyn QueryEmbedder) -> Self {
        Self { inner, last: None }
    }
}

impl QueryEmbedder for Cached<'_> {
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        if let Some((asked, answer)) = &self.last
            && asked == text
        {
            return answer.clone();
        }

        let answer = self.inner.embed_query(text);
        self.last = Some((text.to_owned(), answer.clone()));
        answer
    }
}

/// The fused candidates of `query`, before the boosts, the floor and the margin of [`rank`].
fn candidates(
    store: &mut dyn KnowledgeStore,
    embedder: Option<&mut dyn QueryEmbedder>,
    query: &Query,
    tuning: Tuning,
    kinds: Kinds,
) -> Result<(Vec<Scored>, Vectors), StoreError> {
    let text = query.text.trim();

    let mut keyword = if tuning.lists == Lists::VectorOnly || text.is_empty() {
        Vec::new()
    } else {
        let found = store.keyword_top_k(text, query.scope, CANDIDATES)?;
        kinds.keep(store, found)?
    };

    keyword.retain(|hit| hit.score >= tuning.keyword);

    let (mut vector, vectors) = match (tuning.lists, embedder) {
        (Lists::KeywordOnly, _) | (_, None) => (Vec::new(), Vectors::NotAsked),
        _ if text.is_empty() => (Vec::new(), Vectors::NotAsked),
        (_, Some(embedder)) => match embedder.embed_query(text) {
            Ok(embedding) => {
                let found = store.vector_top_k(&embedding, query.scope, CANDIDATES)?;
                (kinds.keep(store, found)?, Vectors::Used)
            }
            Err(reason) => (Vec::new(), Vectors::Unavailable(reason)),
        },
    };

    vector.retain(|hit| hit.score >= tuning.cosine);

    Ok((fuse(keyword, vector, CANDIDATES * 2), vectors))
}

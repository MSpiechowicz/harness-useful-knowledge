//! Which kinds of document a place may show, and the search over several places.
//!
//! A place shows the kinds its routing includes it for: with `repository_documents = "remote"` and
//! `global_documents = "local"`, a global document that is in the remote database stays hidden, as
//! does a linked one in the local file. The search asks each place for its candidates, drops the
//! hidden ones, and each place runs the whole pipeline (floors, boosts, at most two chunks of a
//! document) before [`fuse_places`] merges the lists by rank, so the keyword scores of two
//! different databases are never compared.

use std::collections::HashMap;

use crate::config::{Kind, Place};
use crate::search::{Found, Vectors};
use crate::store::fuse::Scored;
use crate::store::{ChunkHit, Detail, KnowledgeStore, StoreError};

/// The kinds of document a place shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kinds {
    pub repository: bool,
    pub global: bool,
}

impl Kinds {
    pub const ALL: Self = Self {
        repository: true,
        global: true,
    };

    pub fn allows(self, kind: Kind) -> bool {
        match kind {
            Kind::Repository => self.repository,
            Kind::Global => self.global,
        }
    }

    pub fn is_all(self) -> bool {
        self.repository && self.global
    }

    pub fn is_none(self) -> bool {
        !self.repository && !self.global
    }

    /// The hits of documents of an allowed kind. A hit knows only its link to the repository of
    /// the search, so the documents it cannot tell are looked up, once each.
    pub fn keep(self, store: &mut dyn KnowledgeStore, hits: Vec<ChunkHit>) -> Result<Vec<ChunkHit>, StoreError> {
        if self.is_all() {
            return Ok(hits);
        }

        let mut kinds: HashMap<i64, bool> = HashMap::new();
        let mut kept = Vec::with_capacity(hits.len());

        for hit in hits {
            let allowed = match kinds.get(&hit.doc_id) {
                Some(allowed) => *allowed,
                None => {
                    let allowed = match &hit.repo_globs {
                        Some(_) => self.repository,
                        // The hit was visible to the search that found it, so its document is read
                        // whatever its state.
                        None => match store.get(
                            hit.doc_id,
                            &Detail {
                                drafts: true,
                                ..Detail::default()
                            },
                        )? {
                            Some(found) => self.allows(Kind::of(found.links.len())),
                            None => false,
                        },
                    };

                    kinds.insert(hit.doc_id, allowed);
                    allowed
                }
            };

            if allowed {
                kept.push(hit);
            }
        }

        Ok(kept)
    }
}

/// One store of a routed one: its place and what it may show.
pub struct Part<'a> {
    pub place: Place,
    pub store: &'a mut dyn KnowledgeStore,
    pub kinds: Kinds,
}

/// The ranked lists of the places merged by rank (reciprocal rank fusion): a chunk scores
/// `30500 / (60 + rank in its place)`, the score a chunk first in one list gets from
/// [`crate::store::fuse`]. Ties go to the chunk with the better own score. A single non-empty list
/// is returned as it is, so a search in one place answers exactly as without the other.
pub fn fuse_places(lists: Vec<Vec<Scored>>, limit: usize) -> Vec<Scored> {
    let mut lists: Vec<Vec<Scored>> = lists.into_iter().filter(|list| !list.is_empty()).collect();

    if lists.len() <= 1 {
        let mut only = lists.pop().unwrap_or_default();
        only.truncate(limit);
        return only;
    }

    let mut merged: Vec<(f64, Scored)> = Vec::new();

    for list in lists {
        for (index, scored) in list.into_iter().enumerate() {
            let share = 30_500.0 / (60.0 + (index + 1) as f64);

            merged.push((scored.score, Scored { score: share, ..scored }));
        }
    }

    merged.sort_by(|(own_a, a), (own_b, b)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| own_b.total_cmp(own_a))
            .then_with(|| b.hit.updated_at.cmp(&a.hit.updated_at))
            .then_with(|| a.hit.chunk_id.cmp(&b.hit.chunk_id))
    });
    merged.truncate(limit);

    merged.into_iter().map(|(_, scored)| scored).collect()
}

/// What the places said about the vectors: used when any place used them, else the first reason
/// one was unavailable, else not asked.
pub fn merge_vectors(all: Vec<Vectors>) -> Vectors {
    if all.contains(&Vectors::Used) {
        return Vectors::Used;
    }

    all.into_iter()
        .find(|vectors| matches!(vectors, Vectors::Unavailable(_)))
        .unwrap_or(Vectors::NotAsked)
}

/// The found chunks of several places as one answer.
pub fn merge_found(founds: Vec<Found>, limit: usize) -> Found {
    let mut vectors = Vec::new();
    let mut lists = Vec::new();

    for found in founds {
        vectors.push(found.vectors);
        lists.push(found.hits);
    }

    Found {
        hits: fuse_places(lists, limit),
        vectors: merge_vectors(vectors),
    }
}

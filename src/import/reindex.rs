//! `docs reindex`: gives the chunks that have no vector of the current model one.
//!
//! A document is read whole (its outline, then each chunk) and stored again with the vectors an
//! earlier chunk of the same hash and model already has, and new ones for the rest. The document's
//! text, links, dates and chunk hashes do not change (chunk ids do).

use super::embed::{self, Embedder};
use crate::chunk::passage;
use crate::limits::LIST_PAGE_MAX;
use crate::store::{Chunk, Detail, Document, KnowledgeStore, Link, ListQuery, RepoFilter, Sort, StoreError};

/// What a reindex did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reindexed {
    /// Documents stored again.
    pub documents: usize,
    /// Chunks that got a vector from the embedder.
    pub embedded: usize,
    /// Chunks that took an existing vector.
    pub reused: usize,
    /// Documents that could not be reindexed: id and reason.
    pub failed: Vec<(i64, String)>,
    pub cancelled: bool,
}

/// Which documents to reindex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    Ids(Vec<i64>),
    All,
}

/// Reindexes the documents `which` names, checking `cancel` between documents and batches.
pub fn run(
    store: &mut dyn KnowledgeStore,
    embedder: &mut dyn Embedder,
    which: &Which,
    cancel: &dyn Fn() -> bool,
) -> Result<Reindexed, StoreError> {
    let ids = match which {
        Which::Ids(ids) => ids.clone(),
        Which::All => all_ids(store)?,
    };

    let mut done = Reindexed::default();
    let model = embedder.model_id().to_owned();

    for id in ids {
        if cancel() {
            done.cancelled = true;
            break;
        }

        match one(store, embedder, id, cancel) {
            Ok(Some(step)) => {
                if step.cancelled {
                    done.cancelled = true;
                    break;
                }

                done.documents += 1;
                done.reused += step.reused;
                done.embedded += step.embedded;
            }
            Ok(None) => done.failed.push((id, format!("document {id} not found"))),
            Err(error) => done.failed.push((id, error.to_string())),
        }
    }

    // After every document went through with this model, it is the database's model.
    if *which == Which::All && done.documents > 0 && done.failed.is_empty() && !done.cancelled {
        store.set_meta("model_id", &model)?;
    } else if done.embedded + done.reused > 0 {
        embed::record_model(store, &model)?;
    }

    Ok(done)
}

fn all_ids(store: &mut dyn KnowledgeStore) -> Result<Vec<i64>, StoreError> {
    let mut ids = Vec::new();

    loop {
        let page = store.list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            sort: Sort::Updated,
            descending: false,
            offset: ids.len(),
            limit: LIST_PAGE_MAX,
        })?;

        if page.items.is_empty() {
            return Ok(ids);
        }

        ids.extend(page.items.iter().map(|row| row.doc.id));
    }
}

/// What reindexing one document did.
struct Step {
    reused: usize,
    embedded: usize,
    /// True when the cancel came first: nothing was written.
    cancelled: bool,
}

/// A document as stored: its chunks have text, hashes and positions but no vectors.
struct Stored {
    doc: Document,
    chunks: Vec<Chunk>,
    links: Vec<Link>,
}

/// Reindexes document `id`. `None` when there is no such document.
fn one(
    store: &mut dyn KnowledgeStore,
    embedder: &mut dyn Embedder,
    id: i64,
    cancel: &dyn Fn() -> bool,
) -> Result<Option<Step>, StoreError> {
    let Some(Stored { doc, mut chunks, links }) = read(store, id)? else {
        return Ok(None);
    };

    let passages: Vec<String> = chunks
        .iter()
        .map(|chunk| passage(&doc.title, &chunk.heading_path, &chunk.text))
        .collect();
    let filled = embed::fill(store, embedder, &mut chunks, &passages, cancel)?;

    if filled.cancelled {
        return Ok(Some(Step {
            reused: 0,
            embedded: 0,
            cancelled: true,
        }));
    }

    if let Some(reason) = filled.failed {
        return Err(StoreError::Backend(reason));
    }

    store.put_document(&doc, &chunks, &links)?;

    Ok(Some(Step {
        reused: filled.reused,
        embedded: filled.embedded,
        cancelled: false,
    }))
}

/// Reads document `id` with its chunks (without vectors) and links.
fn read(store: &mut dyn KnowledgeStore, id: i64) -> Result<Option<Stored>, StoreError> {
    let outline = Detail {
        sections: true,
        chunk: None,
    };
    let Some(found) = store.get(id, &outline)? else {
        return Ok(None);
    };

    let mut chunks = Vec::new();

    for section in found.sections.unwrap_or_default() {
        let one = Detail {
            sections: false,
            chunk: Some(section.chunk_id),
        };

        if let Some(chunk) = store.get(id, &one)?.and_then(|detail| detail.chunk) {
            chunks.push(Chunk {
                id: 0,
                doc_id: 0,
                embedding: None,
                model_id: None,
                ..chunk
            });
        }
    }

    Ok(Some(Stored {
        doc: found.doc,
        chunks,
        links: found.links,
    }))
}

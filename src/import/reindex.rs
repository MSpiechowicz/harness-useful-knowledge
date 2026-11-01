//! `docs reindex`: gives the chunks that have no vector of the current model one.
//!
//! The store hands out the chunks that need one a page at a time ([`KnowledgeStore::pending_chunks`],
//! [`BATCH`] chunks, the text exactly as an import embeds it), and the vectors go back with
//! [`KnowledgeStore::set_chunk_embeddings`]. Documents are not rewritten: chunk ids, texts, links,
//! dates and the keyword index stay as they are, and every batch is stored as soon as it is made, so
//! a cancel (checked before each batch) or a failure keeps the work done.
//!
//! After a switch to another model every chunk is pending, so the same loop re-embeds the whole
//! database.

use std::collections::BTreeSet;

use super::embed::{self, Embedder};
use crate::store::{Detail, KnowledgeStore, PendingChunk, StoreError};

/// How many chunks are embedded and stored at a time (what the embedder takes in one call).
pub const BATCH: usize = 64;

/// What a reindex did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reindexed {
    /// Documents that got a vector for at least one chunk.
    pub documents: usize,
    /// Chunks that got a vector from the embedder.
    pub embedded: usize,
    /// Documents that could not be reindexed: id and reason. A failed batch ends the run.
    pub failed: Vec<(i64, String)>,
    pub cancelled: bool,
}

/// Which documents to reindex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    Ids(Vec<i64>),
    All,
}

/// Reindexes the documents `which` names, checking `cancel` before each batch.
pub fn run(
    store: &mut dyn KnowledgeStore,
    embedder: &mut dyn Embedder,
    which: &Which,
    cancel: &dyn Fn() -> bool,
) -> Result<Reindexed, StoreError> {
    let mut done = Reindexed::default();
    let model = embedder.model_id().to_owned();

    let wanted: Option<BTreeSet<i64>> = match which {
        Which::All => None,
        Which::Ids(ids) => {
            for id in ids {
                if store.get(*id, &Detail::default())?.is_none() {
                    done.failed.push((*id, format!("document {id} not found")));
                }
            }

            Some(ids.iter().copied().collect())
        }
    };

    let mut touched: BTreeSet<i64> = BTreeSet::new();
    let mut after = 0;

    loop {
        let page = store.pending_chunks(&model, after, BATCH)?;

        let Some(last) = page.last() else { break };
        after = last.chunk_id;

        let batch: Vec<PendingChunk> = page
            .into_iter()
            .filter(|chunk| wanted.as_ref().is_none_or(|wanted| wanted.contains(&chunk.doc_id)))
            .collect();

        if batch.is_empty() {
            continue;
        }

        if cancel() {
            done.cancelled = true;
            break;
        }

        match embed_batch(store, embedder, &model, &batch) {
            Ok(()) => {
                done.embedded += batch.len();
                touched.extend(batch.iter().map(|chunk| chunk.doc_id));
            }
            Err(reason) => {
                let documents: BTreeSet<i64> = batch.iter().map(|chunk| chunk.doc_id).collect();
                done.failed.extend(documents.into_iter().map(|id| (id, reason.clone())));
                break;
            }
        }
    }

    done.documents = touched.len();

    // A full pass that went through with this model makes it the database's model.
    if *which == Which::All && done.embedded > 0 && done.failed.is_empty() && !done.cancelled {
        store.set_meta("model_id", &model)?;
    } else if done.embedded > 0 {
        embed::record_model(store, &model)?;
    }

    Ok(done)
}

/// Embeds `batch` and stores the vectors; the reason when that did not work.
fn embed_batch(
    store: &mut dyn KnowledgeStore,
    embedder: &mut dyn Embedder,
    model: &str,
    batch: &[PendingChunk],
) -> Result<(), String> {
    let texts: Vec<String> = batch.iter().map(|chunk| chunk.embed_text.clone()).collect();
    let vectors = embedder.embed_passages(&texts).map_err(|error| error.to_string())?;

    if vectors.len() != batch.len() || !vectors.iter().all(|vector| embed::sound(vector)) {
        return Err("the embedder returned unusable vectors".to_owned());
    }

    let embeddings: Vec<(i64, Vec<f32>)> = batch.iter().map(|chunk| chunk.chunk_id).zip(vectors).collect();

    store
        .set_chunk_embeddings(model, &embeddings)
        .map(drop)
        .map_err(|error| error.to_string())
}

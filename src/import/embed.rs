//! Embeddings during an import: what makes a vector, and how an import avoids making one twice.
//!
//! [`Embedder`] is what the embedder process offers (the model's passage embeddings, one vector of
//! [`crate::store::DIMS`] floats per text). An import with no embedder stores its chunks without
//! vectors, and `docs reindex` fills them in once one exists.

use std::collections::HashMap;

use crate::embed::adapter::EmbedderAdapter;
use crate::embed::data_folder;
use crate::store::{Chunk, DIMS, KnowledgeStore};
use crate::support::Env;

/// How many passages one call to the embedder takes.
pub const BATCH: usize = 16;

/// An embedder failure; the message carries no document text.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EmbedError(pub String);

/// Makes the vectors of passages.
pub trait Embedder {
    /// The model that makes the vectors; chunks store it next to them.
    fn model_id(&self) -> &str;

    /// The L2-normalised vector of each passage, in order (the texts are `<title> › <heading path>\n<text>`,
    /// the embedder adds the `passage: ` prefix).
    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// The embedder of this computer's data folder, when it runs and its model is ready. `None`
/// otherwise: imports then store chunks without vectors and `docs reindex` fills them in.
pub fn default_embedder(env: &Env) -> Option<Box<dyn Embedder>> {
    let root = data_folder(env)?;

    EmbedderAdapter::connect(&root).map(|adapter| Box::new(adapter) as Box<dyn Embedder>)
}

/// What [`fill`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filled {
    /// Chunks that took the vector of an earlier chunk with the same hash and model.
    pub reused: usize,
    /// Chunks the embedder made a vector for.
    pub embedded: usize,
    /// Why some chunks have no vector, when the embedder failed.
    pub failed: Option<String>,
    /// True when the import was cancelled before every chunk had its vector.
    pub cancelled: bool,
}

/// Gives each chunk its vector: the stored one of an earlier chunk with the same hash and model
/// when there is one, else a new one from `embedder`, [`BATCH`] passages at a time. `passages` are the
/// texts to embed, one per chunk. Checks `cancel` between batches.
pub fn fill(
    store: &mut dyn KnowledgeStore,
    embedder: &mut dyn Embedder,
    chunks: &mut [Chunk],
    passages: &[String],
    cancel: &dyn Fn() -> bool,
) -> Result<Filled, crate::store::StoreError> {
    let model = embedder.model_id().to_owned();
    let hashes: Vec<String> = chunks.iter().map(|chunk| chunk.chunk_sha.clone()).collect();
    let known: HashMap<String, Vec<f32>> = store.reusable_embeddings(&model, &hashes)?;
    let mut filled = Filled::default();
    let mut missing: Vec<usize> = Vec::new();

    for (at, chunk) in chunks.iter_mut().enumerate() {
        match known.get(&chunk.chunk_sha) {
            Some(vector) => {
                chunk.embedding = Some(vector.clone());
                chunk.model_id = Some(model.clone());
                filled.reused += 1;
            }
            None => missing.push(at),
        }
    }

    for batch in missing.chunks(BATCH) {
        if cancel() {
            filled.cancelled = true;
            return Ok(filled);
        }

        let texts: Vec<String> = batch.iter().map(|at| passages[*at].clone()).collect();

        match embedder.embed_passages(&texts) {
            Ok(vectors) if vectors.len() == batch.len() && vectors.iter().all(|vector| sound(vector)) => {
                for (at, vector) in batch.iter().zip(vectors) {
                    chunks[*at].embedding = Some(vector);
                    chunks[*at].model_id = Some(model.clone());
                    filled.embedded += 1;
                }
            }
            Ok(_) => {
                filled.failed = Some("the embedder returned unusable vectors".to_owned());
                break;
            }
            Err(error) => {
                filled.failed = Some(error.to_string());
                break;
            }
        }
    }

    Ok(filled)
}

/// Records `model` as the model of the database when no model is recorded yet, so a search compares
/// only vectors of the model that made them (see `Stats::model_id`).
pub fn record_model(store: &mut dyn KnowledgeStore, model: &str) -> Result<(), crate::store::StoreError> {
    if store.meta("model_id")?.is_none() {
        store.set_meta("model_id", model)?;
    }

    Ok(())
}

/// True for a vector the store takes: [`DIMS`] finite numbers.
fn sound(vector: &[f32]) -> bool {
    vector.len() == DIMS && vector.iter().all(|value| value.is_finite())
}

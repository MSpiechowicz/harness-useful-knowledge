//! The importer's view of the embedder: [`crate::import::embed::Embedder`] over the socket.
//!
//! The chunker hands the importer passages as `<title> › <heading path>\n<text>`, without the
//! `passage: ` prefix: the embedder adds it itself (a vector is only comparable when a query and a
//! passage both got theirs), so the texts pass through unchanged.

use std::path::Path;

use super::client::EmbedderClient;
use crate::import::embed::{EmbedError, Embedder};

/// An [`Embedder`] that asks the embedder process of a data folder.
pub struct EmbedderAdapter {
    client: EmbedderClient,
    model: String,
}

impl EmbedderAdapter {
    /// The adapter for the embedder of the data folder `root`, when it runs and its model is ready
    /// (the answer takes at most half a second). `None` otherwise: an import then stores its chunks
    /// without vectors and `docs reindex` fills them in later.
    pub fn connect(root: &Path) -> Option<Self> {
        let client = EmbedderClient::new(root, &std::env::temp_dir());
        let pong = client.ping().ok().filter(|pong| pong.ready)?;

        Some(Self {
            client,
            model: pong.model?,
        })
    }
}

impl Embedder for EmbedderAdapter {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.client
            .embed_passages(passages)
            .map_err(|error| EmbedError(error.to_string()))
    }
}

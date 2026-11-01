//! Who turns a search query into a vector: the embedder process of this computer, and when it does
//! not run, the model loaded in this process.
//!
//! A search never waits for a model that is not there: the embedder is asked first (it fails fast
//! when it does not run), the in-process engine is used only when the model files are installed and
//! the caller allows it, and otherwise the search is keyword-only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::embed::client::ClientError;
use crate::embed::engine::{Embed, Kind, Loader, TEXT_MAX, TractLoader};
use crate::embed::{EmbedderClient, MODEL, model};

/// Something that makes the vector of a search query.
pub trait QueryEmbedder {
    /// The L2-normalised vector of `text` read as a query, or why there is none. The reason is
    /// safe to show.
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String>;

    /// Asks the embedder to have its model loaded, so the first query after an idle unload does not
    /// wait for it. Best effort with a short timeout, and nothing for an embedder that has no model
    /// to load.
    fn warm(&mut self) {}
}

/// The embedder process alone (the recall hook, which never loads a model itself).
pub struct SocketEmbedder {
    client: EmbedderClient,
}

impl SocketEmbedder {
    /// The embedder of the data folder `root` (`<data>/harness-knowledge`).
    pub fn new(root: &Path) -> Self {
        Self {
            client: EmbedderClient::new(root, &std::env::temp_dir()),
        }
    }
}

impl QueryEmbedder for SocketEmbedder {
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.client.embed_query(text).map_err(|error| error.to_string())
    }

    fn warm(&mut self) {
        let _ = self.client.warm();
    }
}

/// The embedder process, then the installed model in this process.
pub struct AutoEmbedder {
    client: EmbedderClient,
    root: PathBuf,
    loader: Arc<dyn Loader>,
    /// The model loaded in this process, kept for the rest of the session once it was needed.
    engine: Option<Arc<dyn Embed>>,
}

impl AutoEmbedder {
    pub fn new(root: &Path) -> Self {
        Self::with_loader(root, Arc::new(TractLoader))
    }

    pub fn with_loader(root: &Path, loader: Arc<dyn Loader>) -> Self {
        Self {
            client: EmbedderClient::new(root, &std::env::temp_dir()),
            root: root.to_path_buf(),
            loader,
            engine: None,
        }
    }

    /// The model of this process: loaded on the first call, `Err` when the model is not installed.
    fn engine(&mut self) -> Result<Arc<dyn Embed>, String> {
        if let Some(engine) = &self.engine {
            return Ok(Arc::clone(engine));
        }

        let dir = model::model_dir(&self.root);

        if !model::is_present(&dir, &MODEL) {
            return Err("the embedding model is not installed".to_owned());
        }

        let spec = model::installed(&dir).ok_or_else(|| "the embedding model is not installed".to_owned())?;
        let engine = self.loader.load(&dir, &spec)?;
        self.engine = Some(Arc::clone(&engine));

        Ok(engine)
    }
}

impl QueryEmbedder for AutoEmbedder {
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        match self.client.embed_query(text) {
            Ok(vector) => return Ok(vector),
            // The embedder runs and does not want this text: the model would not either.
            Err(error @ ClientError::Refused(_)) => return Err(error.to_string()),
            Err(ClientError::Unreachable(_)) => {}
        }

        let engine = self.engine()?;
        let clipped: String = text.chars().take(TEXT_MAX).collect();

        engine.embed(Kind::Query, &clipped)
    }

    fn warm(&mut self) {
        let _ = self.client.warm();
    }
}

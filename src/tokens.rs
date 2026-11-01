//! Token counts for the chunker: how long a passage is to the embedding model.
//!
//! The real count comes from the model's own tokenizer ([`ModelCounter`], `tokenizer.json` in the
//! model folder next to the database, which the embedder downloads). Until that file exists the
//! chunker uses [`ApproxCounter`], a deterministic estimate, and an import records which of the two
//! it used ([`TokenCounter::name`]).

use std::path::{Path, PathBuf};

use tokenizers::Tokenizer;

use crate::location::database_path;
use crate::support::Env;

/// The name [`ModelCounter`] reports.
pub const MODEL_COUNTER: &str = "tokenizer";

/// The name [`ApproxCounter`] reports.
pub const APPROX_COUNTER: &str = "approximate";

/// Characters per token of [`ApproxCounter`]: a little under what English text gives with the
/// e5 vocabulary, so the estimate errs on the long side.
const CHARS_PER_TOKEN: f64 = 3.5;

/// Counts the tokens of a text.
pub trait TokenCounter: Send + Sync {
    /// The tokens of `text`, without the model's start and end markers.
    fn count(&self, text: &str) -> usize;

    /// [`MODEL_COUNTER`] or [`APPROX_COUNTER`], for the import to record.
    fn name(&self) -> &'static str;
}

/// A deterministic estimate: the characters divided by 3.5, rounded up.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproxCounter;

impl TokenCounter for ApproxCounter {
    fn count(&self, text: &str) -> usize {
        (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
    }

    fn name(&self) -> &'static str {
        APPROX_COUNTER
    }
}

/// The model's tokenizer.
pub struct ModelCounter {
    tokenizer: Tokenizer,
}

impl ModelCounter {
    /// Loads the tokenizer from `path`. Truncation and padding are switched off, so a text over
    /// the model's limit is counted whole.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut tokenizer =
            Tokenizer::from_file(path).map_err(|error| format!("cannot load the tokenizer: {error}"))?;

        tokenizer
            .with_truncation(None)
            .map_err(|error| format!("cannot load the tokenizer: {error}"))?;
        tokenizer.with_padding(None);

        Ok(Self { tokenizer })
    }
}

impl TokenCounter for ModelCounter {
    fn count(&self, text: &str) -> usize {
        match self.tokenizer.encode_fast(text, false) {
            Ok(encoding) => encoding.len(),
            // The count only sizes chunks: fall back to the estimate rather than fail an import.
            Err(_) => ApproxCounter.count(text),
        }
    }

    fn name(&self) -> &'static str {
        MODEL_COUNTER
    }
}

/// Where the embedder keeps the tokenizer: `model/tokenizer.json` next to the database.
pub fn tokenizer_path(env: &Env) -> Option<PathBuf> {
    let database = database_path(env)?;

    Some(database.parent()?.join("model").join("tokenizer.json"))
}

/// The tokenizer at `path` when it loads, else the estimate.
pub fn counter_at(path: Option<&Path>) -> Box<dyn TokenCounter> {
    match path.filter(|path| path.is_file()).map(ModelCounter::load) {
        Some(Ok(counter)) => Box::new(counter),
        _ => Box::new(ApproxCounter),
    }
}

/// The counter an import uses: the model's tokenizer when it is downloaded, else the estimate.
pub fn counter_for(env: &Env) -> Box<dyn TokenCounter> {
    counter_at(tokenizer_path(env).as_deref())
}

/// The real tokenizer file, when the run names one (`HARNESS_KNOWLEDGE_TEST_TOKENIZER`): tests that
/// need it skip without.
#[cfg(test)]
pub(crate) fn test_tokenizer() -> Option<ModelCounter> {
    let path = std::env::var_os("HARNESS_KNOWLEDGE_TEST_TOKENIZER")?;

    Some(ModelCounter::load(Path::new(&path)).expect("the test tokenizer loads"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_estimate_is_the_characters_over_three_and_a_half_rounded_up() {
        assert_eq!(ApproxCounter.count(""), 0);
        assert_eq!(ApproxCounter.count("a"), 1);
        assert_eq!(ApproxCounter.count(&"a".repeat(7)), 2);
        assert_eq!(ApproxCounter.count(&"a".repeat(8)), 3);
        assert_eq!(ApproxCounter.count(&"ł".repeat(8)), 3, "characters, not bytes");
        assert_eq!(ApproxCounter.name(), APPROX_COUNTER);
    }

    #[test]
    fn without_a_tokenizer_file_the_estimate_is_used() {
        let dir = tempfile::tempdir().expect("temp dir");

        assert_eq!(counter_at(None).name(), APPROX_COUNTER);
        assert_eq!(counter_at(Some(&dir.path().join("none.json"))).name(), APPROX_COUNTER);

        let broken = dir.path().join("tokenizer.json");
        std::fs::write(&broken, "{ not a tokenizer").expect("write");
        assert_eq!(counter_at(Some(&broken)).name(), APPROX_COUNTER);
    }

    #[test]
    fn the_tokenizer_sits_in_the_model_folder_next_to_the_database() {
        let env = Env::for_tests("/home/u").with_var("HARNESS_KNOWLEDGE_DIR", "/data/k");

        assert_eq!(
            tokenizer_path(&env),
            Some(PathBuf::from("/data/k/model/tokenizer.json"))
        );
    }

    #[test]
    fn the_real_tokenizer_counts_words_without_markers() {
        let Some(counter) = test_tokenizer() else {
            return;
        };

        assert_eq!(counter.name(), MODEL_COUNTER);
        assert_eq!(counter.count(""), 0);
        assert!(counter.count("hello world") >= 2);
        assert!(counter.count("passage: Title › Heading\nSome text.") > counter.count("hello world"));

        let long = "word ".repeat(2000);
        assert!(counter.count(&long) > 1500, "no truncation to 512");
    }
}

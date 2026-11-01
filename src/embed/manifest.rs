//! The model the embedder runs, compiled in: which files, from where, and how to check them.
//!
//! The files are release assets of the harness-useful-knowledge repository, never Hugging Face. The
//! SHA-256 and size of each are pinned here, so the bytes are the same wherever they are fetched
//! from ([`BASE_URL_VAR`] only moves the base of the URL, for tests and offline mirrors).

use serde::{Deserialize, Serialize};

/// Names another base URL for [`ModelFile::name`] (a folder URL, without the file name). Only
/// `https://`, or `http://` to `127.0.0.1`, `localhost` or `[::1]`, is accepted.
pub const BASE_URL_VAR: &str = "HARNESS_KNOWLEDGE_MODEL_BASE_URL";

/// The ONNX graph in the model folder.
pub const ONNX_FILE: &str = "model_quantized.onnx";

/// The tokenizer in the model folder.
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// Where the files of [`MODEL`] are published.
macro_rules! asset {
    ($name:literal) => {
        concat!(
            "https://github.com/MSpiechowicz/harness-useful-knowledge/releases/download/model-e5-small-int8-1/",
            $name
        )
    };
}

/// One file of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    /// Where it is published.
    pub url: &'static str,
    /// Lowercase hex.
    pub sha256: &'static str,
    pub size: u64,
}

impl ModelFile {
    /// The URL to fetch: [`ModelFile::url`], or `<base>/<name>` when a base is given.
    pub fn url_under(&self, base: Option<&str>) -> String {
        match base {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), self.name),
            None => self.url.to_owned(),
        }
    }
}

/// How the token vectors become one text vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pooling {
    /// The mean over the tokens the attention mask keeps.
    Mean,
}

/// A model: a versioned id, its files, and how its output is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub id: &'static str,
    pub files: &'static [ModelFile],
    pub dims: usize,
    /// The most tokens the model reads, start and end markers included.
    pub max_tokens: usize,
    pub pooling: Pooling,
    pub normalize: bool,
}

/// The model this build serves: int8 `multilingual-e5-small`.
pub const MODEL: Manifest = Manifest {
    id: "multilingual-e5-small-int8-1",
    files: &[
        ModelFile {
            name: ONNX_FILE,
            url: asset!("model_quantized.onnx"),
            sha256: "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
            size: 118_308_185,
        },
        ModelFile {
            name: TOKENIZER_FILE,
            url: asset!("tokenizer.json"),
            sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
            size: 17_082_730,
        },
    ],
    dims: 384,
    max_tokens: 512,
    pooling: Pooling::Mean,
    normalize: true,
};

impl Manifest {
    /// What the model folder keeps of the manifest (`installed.json`), so the loader reads the
    /// settings of the files that are there.
    pub fn spec(&self) -> Spec {
        Spec {
            id: self.id.to_owned(),
            dims: self.dims,
            max_tokens: self.max_tokens,
            pooling: self.pooling,
            normalize: self.normalize,
        }
    }

    /// The bytes all files add up to.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

/// A downloaded model, as `installed.json` in its folder records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub dims: usize,
    pub max_tokens: usize,
    pub pooling: Pooling,
    pub normalize: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_come_from_the_release_and_are_pinned() {
        for file in MODEL.files {
            assert!(file.url.starts_with(
                "https://github.com/MSpiechowicz/harness-useful-knowledge/releases/download/model-e5-small-int8-1/"
            ));
            assert!(file.url.ends_with(file.name));
            assert_eq!(file.sha256.len(), 64);
            assert!(file.size > 0);
        }

        assert_eq!(MODEL.id, "multilingual-e5-small-int8-1");
        assert_eq!((MODEL.dims, MODEL.max_tokens), (384, 512));
    }

    #[test]
    fn a_base_url_changes_only_the_place() {
        let file = &MODEL.files[1];

        assert_eq!(file.url_under(None), file.url);
        assert_eq!(
            file.url_under(Some("http://127.0.0.1:9/mirror/")),
            "http://127.0.0.1:9/mirror/tokenizer.json"
        );
    }
}

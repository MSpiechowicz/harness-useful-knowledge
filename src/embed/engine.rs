//! The model in memory: the tokenizer and the optimised graph, one text per inference call.
//!
//! The int8 model quantises its activations per call, so a vector depends on what else is in the
//! batch. One text per call keeps a stored passage vector and a later query vector of the same
//! text comparable. The tokenizer and the graph are read-only once loaded, so the worker threads
//! of the [`super::pool::Pool`] share one [`Embed`] and each call builds its own tract state.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokenizers::{Tokenizer, TruncationParams};
use tract_onnx::prelude::*;

use super::manifest::{ONNX_FILE, Spec, TOKENIZER_FILE};

/// How long a loaded model may sit unused.
pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// The longest text handed to the tokenizer, in characters: far more than the 512 tokens the model
/// reads (about 2300 characters of English), so the tail is never tokenized for nothing.
pub const TEXT_MAX: usize = 8192;

/// What a text is for. e5 models are trained with these prefixes, and the vectors only compare
/// well when a query has the first and a stored passage the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Passage,
}

impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Query => "query: ",
            Self::Passage => "passage: ",
        }
    }
}

/// Something that turns a text into a unit vector. Shared by the worker threads.
pub trait Embed: Send + Sync {
    /// The normalised vector of `text`, read as a `kind`.
    fn embed(&self, kind: Kind, text: &str) -> Result<Vec<f32>, String>;
}

/// Loads an engine from the model folder.
pub trait Loader: Send + Sync {
    fn load(&self, dir: &Path, spec: &Spec) -> Result<Arc<dyn Embed>, String>;
}

/// The tract engine over the files of the model folder.
pub struct TractLoader;

impl Loader for TractLoader {
    fn load(&self, dir: &Path, spec: &Spec) -> Result<Arc<dyn Embed>, String> {
        Ok(Arc::new(Tract::load(dir, spec)?))
    }
}

/// A text as the model reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Encoded {
    pub ids: Vec<i64>,
    pub mask: Vec<i64>,
}

/// The tokenizer with truncation at the model's limit.
pub(crate) struct Encoder {
    tokenizer: Tokenizer,
}

impl Encoder {
    pub fn new(mut tokenizer: Tokenizer, max_tokens: usize) -> Result<Self, String> {
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_tokens,
                ..Default::default()
            }))
            .map_err(|error| format!("cannot set up the tokenizer: {error}"))?;

        Ok(Self { tokenizer })
    }

    /// The tokens of `kind`'s prefix and `text`, cut to the limit (the end marker is kept).
    pub fn encode(&self, kind: Kind, text: &str) -> Result<Encoded, String> {
        let clipped: String = text.chars().take(TEXT_MAX).collect();

        let encoded = self
            .tokenizer
            .encode(format!("{}{clipped}", kind.prefix()), true)
            .map_err(|error| format!("cannot tokenize: {error}"))?;

        Ok(Encoded {
            ids: encoded.get_ids().iter().map(|&id| i64::from(id)).collect(),
            mask: encoded.get_attention_mask().iter().map(|&bit| i64::from(bit)).collect(),
        })
    }
}

/// The inputs of one call: ids and mask, and for a graph that has a third input, the segment ids
/// (all 0, as a single sentence has them).
pub(crate) fn assemble(inputs: usize, encoded: &Encoded) -> Result<TVec<TValue>, String> {
    let length = encoded.ids.len();

    let tensor = |values: Vec<i64>| -> Result<TValue, String> {
        tract_ndarray::Array2::from_shape_vec((1, length), values)
            .map(|array| array.into_tensor().into())
            .map_err(|error| error.to_string())
    };

    let mut values = tvec!(tensor(encoded.ids.clone())?, tensor(encoded.mask.clone())?);

    match inputs {
        2 => {}
        3 => values.push(tensor(vec![0; length])?),
        other => return Err(format!("the model takes {other} inputs, expected 2 or 3")),
    }

    Ok(values)
}

/// The mean of the token vectors the mask keeps, scaled to length 1.
pub(crate) fn pooled(hidden: &[f32], dims: usize, mask: &[i64]) -> Vec<f32> {
    let mut vector = vec![0_f32; dims];
    let mut kept = 0_f32;

    for (token, bit) in mask.iter().enumerate() {
        if *bit == 0 {
            continue;
        }

        kept += 1.0;

        for (sum, value) in vector.iter_mut().zip(&hidden[token * dims..(token + 1) * dims]) {
            *sum += value;
        }
    }

    let kept = kept.max(1.0);
    vector.iter_mut().for_each(|value| *value /= kept);

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt().max(1e-12);
    vector.iter_mut().for_each(|value| *value /= norm);

    vector
}

/// The tokenizer and the optimised graph of multilingual-e5-small.
pub(crate) struct Tract {
    encoder: Encoder,
    plan: Arc<TypedRunnableModel>,
    /// How many inputs the graph has, read from the graph itself.
    inputs: usize,
    dims: usize,
}

impl Tract {
    pub fn load(dir: &Path, spec: &Spec) -> Result<Self, String> {
        let tokenizer = Tokenizer::from_file(dir.join(TOKENIZER_FILE))
            .map_err(|error| format!("cannot read the tokenizer: {error}"))?;

        let model = tract_onnx::onnx()
            .model_for_path(dir.join(ONNX_FILE))
            .map_err(|error| format!("cannot load the model: {error}"))?;

        Self::build(tokenizer, model, spec)
    }

    pub fn build(tokenizer: Tokenizer, mut model: InferenceModel, spec: &Spec) -> Result<Self, String> {
        let failed = |error: TractError| format!("cannot load the model: {error}");

        let inputs = model.input_outlets().map_err(failed)?.len();

        if !(2..=3).contains(&inputs) {
            return Err(format!("the model takes {inputs} inputs, expected 2 or 3"));
        }

        let batch = model.symbols.sym("batch_size");
        let length = model.symbols.sym("sequence_length");

        for input in 0..inputs {
            let fact = InferenceFact::dt_shape(
                i64::datum_type(),
                tvec!(TDim::from(batch.clone()), TDim::from(length.clone())),
            );
            model.set_input_fact(input, fact).map_err(failed)?;
        }

        let plan = model
            .into_optimized()
            .and_then(|model| model.into_runnable())
            .map_err(failed)?;

        Ok(Self {
            encoder: Encoder::new(tokenizer, spec.max_tokens)?,
            plan,
            inputs,
            dims: spec.dims,
        })
    }

    #[cfg(test)]
    pub fn inputs(&self) -> usize {
        self.inputs
    }
}

impl Embed for Tract {
    fn embed(&self, kind: Kind, text: &str) -> Result<Vec<f32>, String> {
        let encoded = self.encoder.encode(kind, text)?;
        let length = encoded.ids.len();

        // `run` spawns a state of its own, so the threads sharing the plan never share a state.
        let outputs = self
            .plan
            .run(assemble(self.inputs, &encoded)?)
            .map_err(|error| format!("the model failed: {error}"))?;

        let hidden = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|error| error.to_string())?;

        if hidden.shape() != [1, length, self.dims] {
            return Err("the model output has an unexpected shape".to_owned());
        }

        let values: Vec<f32> = hidden.iter().copied().collect();

        Ok(pooled(&values, self.dims, &encoded.mask))
    }
}

//! The real model. Ignored by default: set `HARNESS_KNOWLEDGE_TEST_MODEL_DIR` to a folder that
//! holds `model_quantized.onnx` (or `onnx/model_quantized.onnx`) and `tokenizer.json`, then run
//! `cargo test -- --ignored real_model --nocapture`. The throughput test reads
//! `HARNESS_KNOWLEDGE_TEST_THREADS` and `HARNESS_KNOWLEDGE_TEST_PASSAGES`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tempfile::TempDir;

use crate::embed::Silent;
use crate::embed::engine::{Embed, Kind, Loader, Tract, TractLoader};
use crate::embed::manifest::{MODEL, ONNX_FILE, TOKENIZER_FILE};
use crate::embed::pool::{Holder, Pool};

/// A model folder with the two files linked or copied in, from the environment; `None` (with a
/// note) when the variable is not set.
fn model_folder() -> Option<TempDir> {
    let Ok(source) = std::env::var("HARNESS_KNOWLEDGE_TEST_MODEL_DIR") else {
        eprintln!("HARNESS_KNOWLEDGE_TEST_MODEL_DIR is not set, skipping");
        return None;
    };

    let source = PathBuf::from(source);
    let onnx = [source.join(ONNX_FILE), source.join("onnx").join(ONNX_FILE)]
        .into_iter()
        .find(|path| path.is_file())
        .expect("model_quantized.onnx under HARNESS_KNOWLEDGE_TEST_MODEL_DIR");

    let folder = TempDir::new().expect("temp dir");

    for (from, name) in [(onnx, ONNX_FILE), (source.join(TOKENIZER_FILE), TOKENIZER_FILE)] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&from, folder.path().join(name)).expect("link the model file");
        #[cfg(not(unix))]
        std::fs::copy(&from, folder.path().join(name)).expect("copy the model file");
    }

    Some(folder)
}

/// The fp32 reference of the Hugging Face model: texts with their prefix, the last cut to 512 tokens.
#[derive(Deserialize)]
struct Reference {
    texts: Vec<String>,
    vectors: Vec<Vec<f64>>,
}

fn reference() -> Reference {
    serde_json::from_str(include_str!("reference.json")).expect("reference vectors")
}

/// The kind a reference text starts with, and the text without it.
fn split_prefix(text: &str) -> (Kind, &str) {
    for kind in [Kind::Query, Kind::Passage] {
        if let Some(rest) = text.strip_prefix(kind.prefix()) {
            return (kind, rest);
        }
    }

    panic!("a reference text without a prefix: {text:?}");
}

fn cosine(a: &[f32], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * y).sum();
    let norm = |values: &mut dyn Iterator<Item = f64>| values.map(|value| value * value).sum::<f64>().sqrt();

    dot / (norm(&mut a.iter().map(|x| f64::from(*x))) * norm(&mut b.iter().copied()))
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
#[ignore = "needs the model files: set HARNESS_KNOWLEDGE_TEST_MODEL_DIR"]
fn real_model_vectors_match_the_huggingface_reference() {
    let Some(folder) = model_folder() else {
        return;
    };

    let engine = Tract::load(folder.path(), &MODEL.spec()).expect("load the model");
    assert_eq!(engine.inputs(), 3, "ids, mask and segment ids");

    let reference = reference();
    assert_eq!(reference.texts.len(), 21);

    let mut lowest = 1.0_f64;

    for (text, expected) in reference.texts.iter().zip(&reference.vectors) {
        let (kind, rest) = split_prefix(text);
        let vector = engine.embed(kind, rest).expect("embed");

        assert_eq!(vector.len(), 384);
        let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-3, "normalised: {length}");

        let similarity = cosine(&vector, expected);
        assert!(similarity >= 0.99, "{similarity} for {:?}", &text[..text.len().min(60)]);
        lowest = lowest.min(similarity);
    }

    eprintln!("lowest cosine to the Hugging Face reference over 21 texts: {lowest:.4}");
}

#[test]
#[ignore = "needs the model files: set HARNESS_KNOWLEDGE_TEST_MODEL_DIR"]
fn real_model_ranks_the_english_passage_for_a_polish_query_first() {
    let Some(folder) = model_folder() else {
        return;
    };

    let engine = TractLoader.load(folder.path(), &MODEL.spec()).expect("load the model");
    let passages = [
        "Run cargo clean before committing because target/debug grew to 276 GB.",
        "The login token expires after the laptop sleeps, so refresh it before the first request.",
        "The PostgreSQL connection pool with four threads fills up under load and queries time out.",
        "Keyboard shortcut Ctrl+[ toggles the left panel and Ctrl+] the right panel.",
    ];

    let vectors: Vec<Vec<f32>> = passages
        .iter()
        .map(|passage| engine.embed(Kind::Passage, passage).expect("embed"))
        .collect();
    let query = engine
        .embed(Kind::Query, "token logowania wygasa po uśpieniu laptopa")
        .expect("embed the query");

    let mut ranked: Vec<(usize, f32)> = vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| (index, dot(&query, vector)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    eprintln!("polish query ranking: {ranked:?}");
    assert_eq!(ranked[0].0, 1, "{ranked:?}");
    assert!(ranked[0].1 > ranked[1].1 + 0.05, "a clear margin: {ranked:?}");
}

/// A value of `/proc/self/status` in megabytes (0 where there is no such file).
fn status_mb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with(key))
                .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kilobytes| kilobytes / 1024)
}

/// Passages of about 350 tokens: sliding windows over the reference's long text.
fn passages(count: usize) -> Vec<String> {
    let long = reference().texts.pop().expect("the long text");
    let long = long.strip_prefix("passage: ").expect("prefix").to_owned();
    let words: Vec<&str> = long.split(' ').collect();

    (0..count)
        .map(|index| {
            let window: Vec<&str> = words[(index * 7) % 200..].iter().take(300).copied().collect();
            format!("{} {index}", window.join(" "))
        })
        .collect()
}

/// Run once per thread count (the memory numbers are per process):
/// `HARNESS_KNOWLEDGE_TEST_THREADS=4 cargo test --release -- --ignored real_model_throughput --nocapture`
#[test]
#[ignore = "needs the model files: set HARNESS_KNOWLEDGE_TEST_MODEL_DIR; prints throughput and memory"]
fn real_model_throughput_and_memory_by_worker_count() {
    let Some(folder) = model_folder() else {
        return;
    };

    let threads: usize = std::env::var("HARNESS_KNOWLEDGE_TEST_THREADS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(1);
    let count: usize = std::env::var("HARNESS_KNOWLEDGE_TEST_PASSAGES")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(48);

    let holder = Arc::new(Holder::new(
        Arc::new(TractLoader),
        folder.path().to_path_buf(),
        MODEL.spec(),
        Arc::new(Silent),
    ));
    let pool = Pool::new(threads, Arc::clone(&holder));

    let started = Instant::now();
    holder.ensure().expect("load the model");
    let loaded_in = started.elapsed();
    let loaded_mb = status_mb("VmRSS");

    let texts = passages(count);
    pool.passages(&texts[..threads.max(1)], Duration::from_secs(600))
        .expect("warm up");

    let started = Instant::now();
    let vectors = pool.passages(&texts, Duration::from_secs(1200)).expect("passages");
    let seconds = started.elapsed().as_secs_f64();

    eprintln!(
        "workers={threads} loaded in {:.1}s, RSS loaded={loaded_mb} MB, {count} passages in {seconds:.2}s = {:.2}/s, \
         peak RSS={} MB, RSS now={} MB",
        loaded_in.as_secs_f64(),
        count as f64 / seconds,
        status_mb("VmHWM"),
        status_mb("VmRSS"),
    );

    // Whichever thread ran a passage, its vector is the one a single call gives.
    let engine = holder.ensure().expect("engine");

    for index in [0, count / 2, count - 1] {
        let alone = engine.embed(Kind::Passage, &texts[index]).expect("embed");
        assert_eq!(vectors[index], alone, "passage {index}");
    }
}

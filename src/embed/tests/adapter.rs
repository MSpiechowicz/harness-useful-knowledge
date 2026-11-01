//! The importer's adapter over a running embedder.

use tempfile::TempDir;

use super::socket::running;
use super::support::fake_vector;
use crate::embed::adapter::EmbedderAdapter;
use crate::embed::engine::Kind;
use crate::import::embed::Embedder;

#[test]
fn the_adapter_passes_the_texts_through_unchanged() {
    let running = running(2);
    let mut adapter = EmbedderAdapter::connect(&running.world.root).expect("an embedder runs here");

    assert_eq!(adapter.model_id(), "fake@1");

    let passages = vec![
        "Guide › Setup\nInstall the tool".to_owned(),
        "passage: the text itself starts like a prefix".to_owned(),
    ];
    let vectors = adapter.embed_passages(&passages).expect("embed");

    assert_eq!(
        vectors[0],
        fake_vector(Kind::Passage, "Guide › Setup\nInstall the tool")
    );
    assert_eq!(
        vectors[1],
        fake_vector(Kind::Passage, "passage: the text itself starts like a prefix"),
        "the importer's texts carry no prefix, so nothing is taken off"
    );
}

#[test]
fn there_is_no_adapter_without_a_ready_embedder() {
    let dir = TempDir::new().expect("temp dir");

    assert!(EmbedderAdapter::connect(dir.path()).is_none());
}

#[test]
fn a_failing_embedder_is_an_error_without_the_text() {
    let running = running(1);
    let mut adapter = EmbedderAdapter::connect(&running.world.root).expect("an embedder runs here");

    let error = adapter
        .embed_passages(&["passage: explode with secret contents".to_owned()])
        .expect_err("the fake engine refuses it");

    assert!(error.0.contains("refuses"), "{}", error.0);
    assert!(!error.0.contains("secret contents"), "{}", error.0);
}

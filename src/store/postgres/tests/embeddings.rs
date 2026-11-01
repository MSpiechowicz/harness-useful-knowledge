//! Embeddings stored after the fact, the way a reindex does.

use super::support::{database, insert};
use crate::store::testing::{MODEL, Seed, axis, pending_chunks_contract};
use crate::store::{KnowledgeStore, Scope};

#[test]
fn team_pending_chunks_and_embeddings_follow_the_contract() {
    let Some(db) = database() else { return };

    pending_chunks_contract(&mut db.open());
}

#[test]
fn team_a_stored_vector_is_found_by_the_vector_search_and_the_keyword_index_is_left_alone() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    insert(&mut store, &Seed::new("Guide").chunk("Intro", "needle words"));

    let keyword = |store: &mut crate::store::postgres::PostgresStore| {
        store
            .keyword_top_k("needle", &Scope::everything(), 10)
            .expect("keyword")
            .iter()
            .map(|hit| hit.chunk_id)
            .collect::<Vec<_>>()
    };

    let before = keyword(&mut store);
    assert_eq!(before.len(), 1);
    assert!(
        store
            .vector_top_k(&axis(0), &Scope::everything(), 10)
            .expect("vector")
            .is_empty()
    );

    let pending = store.pending_chunks(MODEL, 0, 10).expect("pending");
    store
        .set_chunk_embeddings(MODEL, &[(pending[0].chunk_id, axis(0))])
        .expect("embed");

    let hits = store.vector_top_k(&axis(0), &Scope::everything(), 10).expect("vector");
    assert_eq!(hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(), before);
    assert_eq!(keyword(&mut store), before);
}
